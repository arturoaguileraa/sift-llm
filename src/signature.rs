//! Provider opaque-data passthrough.
//!
//! Some providers attach opaque, provider-specific data to tool calls that must be
//! echoed back verbatim on the next turn. Gemini 3 "thinking" models are the driving
//! case: every function call carries `extra_content.google.thought_signature`, and the
//! follow-up request is rejected (`400 "Function call is missing a thought_signature"`)
//! if it is not sent back. Harnesses talking through a generic OpenAI-compatible client
//! (opencode's `@ai-sdk/openai-compatible`) drop that non-standard field.
//!
//! Sift sits in the middle and sees the response, so it captures the opaque
//! `extra_content` here (keyed by tool_call id) and re-injects it into the matching
//! tool_call on the following request. This keeps such models working through Sift's
//! OpenAI-compatible surface without implementing each provider's native protocol.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// Captured `extra_content` blobs, keyed by tool_call id. Shared across requests.
pub type SignatureStore = Mutex<HashMap<String, Value>>;

/// Records a tool_call's `extra_content` under its id, if both are present.
pub fn remember(store: &SignatureStore, id: &str, extra_content: &Value) {
    if !extra_content.is_null() {
        store
            .lock()
            .unwrap()
            .insert(id.to_string(), extra_content.clone());
    }
}

/// Captures opaque data from every tool_call in a buffered (non-streaming) response.
pub fn capture_response(json: &Value, store: &SignatureStore) {
    let Some(choices) = json["choices"].as_array() else {
        return;
    };
    for choice in choices {
        if let Some(tool_calls) = choice["message"]["tool_calls"].as_array() {
            for tc in tool_calls {
                if let Some(id) = tc["id"].as_str() {
                    if let Some(extra) = tc.get("extra_content") {
                        remember(store, id, extra);
                    }
                }
            }
        }
    }
}

/// Re-injects captured opaque data into request tool_calls that lack it, matching by
/// tool_call id. A tool_call that already carries `extra_content` is left untouched.
pub fn inject_request(payload: &mut Value, store: &SignatureStore) {
    let guard = store.lock().unwrap();
    if guard.is_empty() {
        return;
    }
    let Some(messages) = payload["messages"].as_array_mut() else {
        return;
    };
    for msg in messages {
        let Some(tool_calls) = msg["tool_calls"].as_array_mut() else {
            continue;
        };
        for tc in tool_calls {
            let already_present = tc.get("extra_content").is_some_and(|v| !v.is_null());
            if already_present {
                continue;
            }
            if let Some(id) = tc["id"].as_str().map(str::to_string) {
                if let Some(extra) = guard.get(&id) {
                    tc["extra_content"] = extra.clone();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_extra_content_by_tool_call_id() {
        let store: SignatureStore = Mutex::new(HashMap::new());

        // Capture from a response carrying Gemini's thought_signature.
        let response = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_abc",
                        "extra_content": { "google": { "thought_signature": "SIG==" } },
                        "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
                    }]
                }
            }]
        });
        capture_response(&response, &store);

        // Next request resends the tool_call WITHOUT extra_content (harness dropped it).
        let mut request = json!({
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
                }]
            }]
        });
        inject_request(&mut request, &store);

        assert_eq!(
            request["messages"][0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "SIG=="
        );
    }

    #[test]
    fn inject_leaves_unknown_ids_and_present_content_alone() {
        let store: SignatureStore = Mutex::new(HashMap::new());
        remember(
            &store,
            "known",
            &json!({ "google": { "thought_signature": "S" } }),
        );

        let mut request = json!({
            "messages": [{
                "role": "assistant",
                "tool_calls": [
                    { "id": "unknown", "function": { "name": "f", "arguments": "{}" } },
                    { "id": "known", "extra_content": { "already": "here" },
                      "function": { "name": "g", "arguments": "{}" } }
                ]
            }]
        });
        inject_request(&mut request, &store);

        let calls = &request["messages"][0]["tool_calls"];
        assert!(calls[0].get("extra_content").is_none()); // unknown id: untouched
        assert_eq!(calls[1]["extra_content"]["already"], "here"); // present: not overwritten
    }
}
