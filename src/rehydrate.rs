use serde_json::Value;

use crate::vault::Vault;

/// Walks a parsed JSON response and rehydrates every string in place. This covers
/// the assistant's message content and, importantly, the JSON-encoded `arguments`
/// of tool calls: a token leaked into `arguments` would otherwise be executed
/// literally by the harness (`send_email(to="[EMAIL_1]")`). Tokens the vault did
/// not mint are left untouched.
pub fn rehydrate_json_value(value: &mut Value, vault: &Vault) {
    match value {
        Value::String(s) => {
            if s.contains('[') {
                *s = vault.rehydrate(s);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rehydrate_json_value(v, vault);
            }
        }
        Value::Object(obj) => {
            for v in obj.values_mut() {
                rehydrate_json_value(v, vault);
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
/// handler. Scope note: only assistant `delta.content` is rehydrated here; streamed
/// `tool_call` argument deltas are a documented follow-up (they need buffering the
/// whole arguments JSON per call before substitution). The non-streaming path already
/// rehydrates tool-call arguments fully.
pub struct SseRehydrator {
    vault: Vault,
    /// Raw bytes of an SSE event not yet terminated by `\n\n`.
    raw: Vec<u8>,
    /// Content held back mid-token, carried across events until the token completes.
    pending: String,
}

impl SseRehydrator {
    pub fn new(vault: Vault) -> Self {
        Self {
            vault,
            raw: Vec::new(),
            pending: String::new(),
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

    /// Rehydrates and emits whatever partial content is still buffered, as its own
    /// SSE content frame. Empty if nothing is pending.
    fn flush_pending(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let tail = self.vault.rehydrate(&std::mem::take(&mut self.pending));
        content_frame(&tail)
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

        // End-of-stream sentinel: release any held content *before* it.
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

        let content = json["choices"][0]["delta"]["content"].as_str();
        let Some(content) = content else {
            // No text delta (role marker, finish_reason, tool_calls-only): pass through.
            return event.to_vec();
        };

        // Reassemble across deltas: prepend whatever token fragment we held back.
        let combined = format!("{}{}", self.pending, content);
        let rehydrated = self.vault.rehydrate(&combined);

        // Hold back a still-open token (`[` with no `]` after it) for the next delta.
        let split = rehydrated
            .rfind('[')
            .filter(|&i| !rehydrated[i..].contains(']'))
            .unwrap_or(rehydrated.len());
        let emit = rehydrated[..split].to_string();
        self.pending = rehydrated[split..].to_string();

        json["choices"][0]["delta"]["content"] = Value::String(emit);
        frame_from_json(&json)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vault_with(value: &str, category: &str) -> (Vault, String) {
        let mut vault = Vault::new();
        let token = vault.tokenize(value, category);
        (vault, token)
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

        rehydrate_json_value(&mut body, &vault);

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

    #[test]
    fn sse_reassembles_a_token_split_across_delta_events() {
        // The realistic case: the model streams "[EMAIL_1]" as separate content
        // deltas, each in its own SSE event — never contiguous in the raw bytes.
        let (vault, _) = vault_with("juan@empresa.com", "email");
        let mut r = SseRehydrator::new(vault);

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
        let mut r = SseRehydrator::new(vault);

        let full = format!("{}{}data: [DONE]\n\n", delta("hi [EMAIL"), delta("_1] there"));
        let bytes = full.as_bytes();
        let mid = bytes.len() / 3;
        let out = run_sse(&mut r, &[&bytes[..mid], &bytes[mid..]]);
        assert_eq!(visible_text(&out), "hi juan@empresa.com there");
    }

    #[test]
    fn sse_passes_through_streams_without_tokens() {
        let vault = Vault::new(); // empty vault path is handled in proxy, but be safe
        let mut r = SseRehydrator::new(vault);
        let stream = format!("{}data: [DONE]\n\n", delta("just plain text"));
        let out = run_sse(&mut r, &[stream.as_bytes()]);
        assert_eq!(visible_text(&out), "just plain text");
        assert!(out.contains("data: [DONE]"));
    }
}
