use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::signature::{self, SignatureStore};
use crate::vault::Vault;

/// Walks a parsed JSON response and rehydrates every string in place. This covers
/// the assistant's message content and, importantly, the JSON-encoded `arguments`
/// of tool calls: a token leaked into `arguments` would otherwise be executed
/// literally by the harness (`send_email(to="[EMAIL_1]")`). Tokens the vault did
/// not mint are left untouched. Restored token names are appended to `revealed` so
/// the caller can log the inbound side.
pub fn rehydrate_json_value(value: &mut Value, vault: &Vault, revealed: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if s.contains('[') {
                *s = vault.rehydrate_tracked(s, revealed);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rehydrate_json_value(v, vault, revealed);
            }
        }
        Value::Object(obj) => {
            for v in obj.values_mut() {
                rehydrate_json_value(v, vault, revealed);
            }
        }
        _ => {}
    }
}

/// Rehydrator for streamed (SSE) chat-completion responses.
///
/// This is the "handle variables cut by the stream" machine from the design, and it
/// works at two levels because a token can be broken two different ways:
///
/// 1. **Transport framing.** A network chunk can split an SSE event (or a multi-byte
///    UTF-8 char) mid-way. We buffer raw bytes and only process complete events,
///    delimited by the blank line `\n\n`.
/// 2. **Delta framing.** The model emits a token like `[EMAIL_1]` across several
///    `delta.content` pieces (`[`, `EMAIL`, `_1`, `]`), each in its own SSE event —
///    so the token is *never contiguous in the raw bytes*. We reassemble it at the
///    content level: partial tokens are held in `pending` across events and released
///    (rehydrated) once complete.
///
/// It **owns** its vault because the streamed response body outlives the request
/// handler. It rehydrates both the assistant's `delta.content` and the streamed
/// `tool_call` argument deltas, each with its own held-back token buffer — `content`
/// in `pending`, and each tool call (keyed by its stream `index`) in `pending_args`.
/// A token straddling delta boundaries in either place is reassembled the same way.
pub struct SseRehydrator {
    vault: Vault,
    /// Raw bytes of an SSE event not yet terminated by `\n\n`.
    raw: Vec<u8>,
    /// `delta.content` held back mid-token, carried across events until it completes.
    pending: String,
    /// Per-tool-call `arguments` held back mid-token, keyed by the tool call's index.
    pending_args: HashMap<u64, String>,
    /// Shared store to capture provider opaque data (e.g. thought_signature) into.
    signatures: Arc<SignatureStore>,
    /// Maps a streamed tool call's `index` to its `id` (the id arrives in the first
    /// delta; opaque data may arrive in the same or a later delta).
    tool_ids: HashMap<u64, String>,
}

impl SseRehydrator {
    pub fn new(vault: Vault, signatures: Arc<SignatureStore>) -> Self {
        Self {
            vault,
            raw: Vec::new(),
            pending: String::new(),
            pending_args: HashMap::new(),
            signatures,
            tool_ids: HashMap::new(),
        }
    }

    /// Feeds a transport chunk and returns rewritten SSE bytes ready to forward.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.raw.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = find_subslice(&self.raw, b"\n\n") {
            let event: Vec<u8> = self.raw.drain(..pos + 2).collect();
            out.extend_from_slice(&self.process_event(&event));
        }
        out
    }

    /// Emits any held-back content once the upstream stream has ended.
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.raw.is_empty() {
            let event = std::mem::take(&mut self.raw);
            out.extend_from_slice(&self.process_event(&event));
        }
        out.extend_from_slice(&self.flush_pending());
        out
    }

    /// Slides one text piece through the token window: prepend the held-back fragment,
    /// rehydrate complete tokens (logging each restored one), then hold back a
    /// still-open token (`[` with no `]` after it) for the next piece. Returns the
    /// portion safe to emit now.
    fn slide(vault: &Vault, pending: &mut String, piece: &str) -> String {
        let combined = format!("{}{}", pending, piece);
        let mut revealed = Vec::new();
        let rehydrated = vault.rehydrate_tracked(&combined, &mut revealed);
        log_reveals(vault, &revealed);
        let split = rehydrated
            .rfind('[')
            .filter(|&i| !rehydrated[i..].contains(']'))
            .unwrap_or(rehydrated.len());
        let emit = rehydrated[..split].to_string();
        *pending = rehydrated[split..].to_string();
        emit
    }

    /// Emits whatever partial content and tool-call arguments are still buffered, each
    /// as its own trailing SSE frame. Empty if nothing is pending.
    fn flush_pending(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let mut revealed = Vec::new();
            let tail = self
                .vault
                .rehydrate_tracked(&std::mem::take(&mut self.pending), &mut revealed);
            log_reveals(&self.vault, &revealed);
            out.extend_from_slice(&content_frame(&tail));
        }
        // Deterministic order so the flushed frames are reproducible.
        let mut indices: Vec<u64> = self.pending_args.keys().copied().collect();
        indices.sort_unstable();
        for index in indices {
            let held = self.pending_args.remove(&index).unwrap_or_default();
            if !held.is_empty() {
                let mut revealed = Vec::new();
                let tail = self.vault.rehydrate_tracked(&held, &mut revealed);
                log_reveals(&self.vault, &revealed);
                out.extend_from_slice(&tool_args_frame(index, &tail));
            }
        }
        out
    }

    fn process_event(&mut self, event: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(event);

        // Collect the event's `data:` payload (joining multi-line data blocks).
        let mut payload: Option<String> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                payload = Some(match payload {
                    Some(mut d) => {
                        d.push('\n');
                        d.push_str(rest);
                        d
                    }
                    None => rest.to_string(),
                });
            }
        }

        let payload = match payload {
            // Not a data event (comment, keep-alive, blank): forward untouched.
            None => return event.to_vec(),
            Some(p) => p,
        };

        // End-of-stream sentinel: release any held fragments *before* it.
        if payload.trim() == "[DONE]" {
            let mut out = self.flush_pending();
            out.extend_from_slice(event);
            return out;
        }

        let mut json: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            // Unparseable payload: forward untouched rather than drop it.
            Err(_) => return event.to_vec(),
        };

        // Capture provider opaque data (e.g. Gemini's thought_signature) on every
        // response, independent of PII — it must round-trip even when nothing was
        // pseudonymized. This is what the buffered gating got wrong before.
        capture_delta_signatures(&json, &mut self.tool_ids, &self.signatures);

        // With an empty vault there is nothing to rehydrate; forward the frame
        // unchanged (no reframing of ordinary text) now that we have captured above.
        if self.vault.is_empty() {
            return event.to_vec();
        }

        let mut modified = false;

        // 1. Text content delta.
        if let Some(content) = json["choices"][0]["delta"]["content"].as_str().map(str::to_string) {
            let emit = Self::slide(&self.vault, &mut self.pending, &content);
            json["choices"][0]["delta"]["content"] = Value::String(emit);
            modified = true;
        }

        // 2. Tool-call argument deltas. Each element carries an `index` identifying
        //    which tool call it belongs to; its `arguments` is a fragment of a JSON
        //    string being streamed. We slide each through its own per-index buffer so a
        //    token split across fragments (`[EMA` | `IL_1]`) is reassembled. A literal
        //    `[` from a JSON array is never a token (tokens are `[UPPER_N]`), so it is
        //    at worst briefly held back, never corrupted.
        if let Some(tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array_mut() {
            for tc in tool_calls.iter_mut() {
                let index = tc["index"].as_u64().unwrap_or(0);
                if let Some(args) = tc["function"]["arguments"].as_str().map(str::to_string) {
                    let pending = self.pending_args.entry(index).or_default();
                    let emit = Self::slide(&self.vault, pending, &args);
                    tc["function"]["arguments"] = Value::String(emit);
                    modified = true;
                }
            }
        }

        if modified {
            frame_from_json(&json)
        } else {
            // No text/tool deltas (role marker, finish_reason only): pass through.
            event.to_vec()
        }
    }
}

/// Captures provider opaque data from a streamed `delta.tool_calls` array, keyed by
/// tool_call id. The id arrives with the first delta of a call and is tracked in
/// `tool_ids` so opaque data arriving in the same or a later delta can be attributed.
fn capture_delta_signatures(
    json: &Value,
    tool_ids: &mut HashMap<u64, String>,
    store: &SignatureStore,
) {
    let Some(tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array() else {
        return;
    };
    for tc in tool_calls {
        let index = tc["index"].as_u64().unwrap_or(0);
        if let Some(id) = tc["id"].as_str() {
            tool_ids.insert(index, id.to_string());
        }
        if let Some(extra) = tc.get("extra_content").filter(|v| !v.is_null()) {
            let id = tc["id"]
                .as_str()
                .map(str::to_string)
                .or_else(|| tool_ids.get(&index).cloned());
            if let Some(id) = id {
                signature::remember(store, &id, extra);
            }
        }
    }
}

/// Logs each restored token once (per call), pairing it with its real value.
pub(crate) fn log_reveals(vault: &Vault, revealed: &[String]) {
    let mut seen = std::collections::HashSet::new();
    for token in revealed {
        if seen.insert(token) {
            if let Some(value) = vault.resolve(token) {
                crate::audit::log_reveal(token, value);
            }
        }
    }
}

/// Finds the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Serializes a JSON value as a single SSE `data:` frame.
fn frame_from_json(value: &Value) -> Vec<u8> {
    format!("data: {}\n\n", value).into_bytes()
}

/// Builds an SSE frame carrying `text` as a `delta.content` chunk.
fn content_frame(text: &str) -> Vec<u8> {
    let value = serde_json::json!({
        "choices": [{ "index": 0, "delta": { "content": text } }]
    });
    frame_from_json(&value)
}

/// Builds an SSE frame carrying `args` as a tool-call `arguments` chunk for the tool
/// call at `index`. Used to flush a held-back arguments tail at end of stream.
fn tool_args_frame(index: u64, args: &str) -> Vec<u8> {
    let value = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": { "tool_calls": [{ "index": index, "function": { "arguments": args } }] }
        }]
    });
    frame_from_json(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vault_with(value: &str, category: &str) -> (Vault, String) {
        let mut vault = Vault::new();
        let token = vault.tokenize(value, category);
        (vault, token)
    }

    /// A fresh, empty signature store for tests that don't care about capture.
    fn store() -> Arc<SignatureStore> {
        Arc::new(std::sync::Mutex::new(HashMap::new()))
    }

    #[test]
    fn rehydrates_content_and_tool_arguments() {
        let (vault, token) = vault_with("juan@empresa.com", "email");
        let mut body = json!({
            "choices": [{
                "message": {
                    "content": format!("I emailed {token}"),
                    "tool_calls": [{
                        "function": {
                            "name": "send_email",
                            "arguments": format!("{{\"to\":\"{token}\"}}")
                        }
                    }]
                }
            }]
        });

        let mut revealed = Vec::new();
        rehydrate_json_value(&mut body, &vault, &mut revealed);
        assert_eq!(revealed.len(), 2); // content + tool_call arguments

        let msg = &body["choices"][0]["message"];
        assert_eq!(msg["content"], "I emailed juan@empresa.com");
        assert_eq!(
            msg["tool_calls"][0]["function"]["arguments"],
            "{\"to\":\"juan@empresa.com\"}"
        );
    }

    /// Feeds transport chunks through the SSE rehydrator and returns the full
    /// rewritten stream as a String.
    fn run_sse(r: &mut SseRehydrator, chunks: &[&[u8]]) -> String {
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&r.push(c));
        }
        out.extend_from_slice(&r.flush());
        String::from_utf8(out).unwrap()
    }

    /// Extracts and concatenates every `delta.content` from a rewritten SSE stream,
    /// i.e. the text the end user would actually see.
    fn visible_text(sse: &str) -> String {
        let mut s = String::new();
        for line in sse.lines() {
            if let Some(payload) = line.strip_prefix("data: ") {
                if payload.trim() == "[DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(payload) {
                    if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
                        s.push_str(c);
                    }
                }
            }
        }
        s
    }

    fn delta(content: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({ "choices": [{ "index": 0, "delta": { "content": content } }] })
        )
    }

    /// An SSE event carrying a tool-call `arguments` fragment for tool call `index`.
    fn args_delta(index: u64, args: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [{ "index": index, "function": { "arguments": args } }] }
                }]
            })
        )
    }

    /// Concatenates every streamed tool-call `arguments` fragment for `index`.
    fn visible_args(sse: &str, index: u64) -> String {
        let mut s = String::new();
        for line in sse.lines() {
            if let Some(payload) = line.strip_prefix("data: ") {
                if payload.trim() == "[DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(payload) {
                    if let Some(calls) = v["choices"][0]["delta"]["tool_calls"].as_array() {
                        for tc in calls {
                            if tc["index"].as_u64() == Some(index) {
                                if let Some(a) = tc["function"]["arguments"].as_str() {
                                    s.push_str(a);
                                }
                            }
                        }
                    }
                }
            }
        }
        s
    }

    #[test]
    fn sse_reassembles_a_token_split_across_delta_events() {
        // The realistic case: the model streams "[EMAIL_1]" as separate content
        // deltas, each in its own SSE event — never contiguous in the raw bytes.
        let (vault, _) = vault_with("juan@empresa.com", "email");
        let mut r = SseRehydrator::new(vault, store());

        let stream = format!(
            "{}{}{}{}data: [DONE]\n\n",
            delta("Contacting "),
            delta("[EMA"),
            delta("IL_1"),
            delta("] now"),
        );
        let out = run_sse(&mut r, &[stream.as_bytes()]);
        assert_eq!(visible_text(&out), "Contacting juan@empresa.com now");
    }

    #[test]
    fn sse_reassembles_when_transport_splits_an_event() {
        // On top of delta framing, a network chunk cuts an SSE event in half.
        let (vault, _) = vault_with("juan@empresa.com", "email");
        let mut r = SseRehydrator::new(vault, store());

        let full = format!("{}{}data: [DONE]\n\n", delta("hi [EMAIL"), delta("_1] there"));
        let bytes = full.as_bytes();
        let mid = bytes.len() / 3;
        let out = run_sse(&mut r, &[&bytes[..mid], &bytes[mid..]]);
        assert_eq!(visible_text(&out), "hi juan@empresa.com there");
    }

    #[test]
    fn sse_passes_through_streams_without_tokens() {
        let vault = Vault::new(); // empty vault path is handled in proxy, but be safe
        let mut r = SseRehydrator::new(vault, store());
        let stream = format!("{}data: [DONE]\n\n", delta("just plain text"));
        let out = run_sse(&mut r, &[stream.as_bytes()]);
        assert_eq!(visible_text(&out), "just plain text");
        assert!(out.contains("data: [DONE]"));
    }

    #[test]
    fn sse_rehydrates_tool_call_arguments_split_across_deltas() {
        // The model streams send_email(to="[EMAIL_1]") as JSON-string fragments, with
        // the token split across two of them: `{"to": "[EMA` | `IL_1]"}`.
        let (vault, _) = vault_with("juan@empresa.com", "email");
        let mut r = SseRehydrator::new(vault, store());

        let stream = format!(
            "{}{}{}data: [DONE]\n\n",
            args_delta(0, "{\"to\": \"[EMA"),
            args_delta(0, "IL_1"),
            args_delta(0, "]\"}"),
        );
        let out = run_sse(&mut r, &[stream.as_bytes()]);

        // Reassembled arguments must carry the real value, and still be valid JSON.
        let args = visible_args(&out, 0);
        assert_eq!(args, "{\"to\": \"juan@empresa.com\"}");
        let parsed: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(parsed["to"], "juan@empresa.com");
    }

    #[test]
    fn sse_keeps_json_array_brackets_in_arguments_intact() {
        // A literal `[` from a JSON array in arguments must not be mistaken for a token
        // nor corrupted, even when the array spans two argument fragments.
        let (vault, _) = vault_with("juan@empresa.com", "email");
        let mut r = SseRehydrator::new(vault, store());

        let stream = format!(
            "{}{}data: [DONE]\n\n",
            args_delta(0, "{\"cc\": [\"[EMAIL_1"),
            args_delta(0, "]\"]}"),
        );
        let out = run_sse(&mut r, &[stream.as_bytes()]);

        let args = visible_args(&out, 0);
        assert_eq!(args, "{\"cc\": [\"juan@empresa.com\"]}");
        let parsed: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(parsed["cc"][0], "juan@empresa.com");
    }

    #[test]
    fn sse_captures_tool_call_thought_signature() {
        let (vault, _) = vault_with("juan@empresa.com", "email");
        let sigs = store();
        let mut r = SseRehydrator::new(vault, sigs.clone());

        // First delta of a tool call: carries id + Gemini's opaque extra_content.
        let first = format!(
            "data: {}\n\n",
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0,
                "id": "call_xyz",
                "extra_content": { "google": { "thought_signature": "SIG==" } },
                "function": { "name": "send_email", "arguments": "{\"to\": \"" }
            }] } }] })
        );
        let stream = format!("{}{}data: [DONE]\n\n", first, args_delta(0, "[EMAIL_1]\"}"));
        let _ = run_sse(&mut r, &[stream.as_bytes()]);

        let captured = sigs.lock().unwrap();
        assert_eq!(
            captured["call_xyz"]["google"]["thought_signature"],
            "SIG=="
        );
    }
}
