use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use colored::Colorize;
use futures_util::stream::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crate::audit::log_audit;
use crate::detect::Detector;
use crate::policy::{Action, AuditRecord, PolicyEngine};
use crate::provider::ProviderRegistry;
use crate::rehydrate::{log_reveals, rehydrate_json_value, SseRehydrator};
use crate::signature::{self, SignatureStore};
use crate::vault::Vault;

#[derive(Clone)]
pub struct AppState {
    pub policy_engine: Arc<PolicyEngine>,
    pub detector: Arc<Detector>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub client: Client,
    /// Provider opaque data (e.g. Gemini's thought_signature) captured from responses
    /// and re-injected on later requests. See the `signature` module.
    pub signatures: Arc<SignatureStore>,
}

pub async fn run_proxy(
    port: u16,
    policy_engine: PolicyEngine,
    detector: Detector,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState {
        policy_engine: Arc::new(policy_engine),
        detector: Arc::new(detector),
        provider_registry: Arc::new(ProviderRegistry::new()),
        client: Client::new(),
        signatures: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/models", get(handle_models))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            report_port_in_use(port);
            std::process::exit(1);
        }
        Err(e) => return Err(Box::new(e)),
    };
    println!("✓ Sift listening on http://localhost:{}", port);
    axum::serve(listener, app).await?;

    Ok(())
}

/// Prints the "port already in use" error with a hint to use `--port`.
pub fn report_port_in_use(port: u16) {
    eprintln!(
        "{} Port {} is already in use (another Sift gateway may be running).",
        "Error:".red().bold(),
        port
    );
    eprintln!(
        "  Use {} to run on a different port, e.g. {}.",
        "--port <PORT>".cyan(),
        format!("sift serve --port {}", port + 1).cyan()
    );
}

async fn handle_health() -> impl IntoResponse {
    let pid = std::process::id();
    let body = serde_json::json!({
        "status": "ok",
        "name": "sift-llm",
        "pid": pid
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap(),
    )
}

async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    let models = state.provider_registry.all_models();
    let data: Vec<Value> = models
        .into_iter()
        .map(|(model, provider)| {
            let clean_model = model.strip_prefix("models/").unwrap_or(model);
            serde_json::json!({
                "id": format!("{} (Secured by SiftLLM)", clean_model),
                "object": "model",
                "owned_by": provider,
            })
        })
        .collect();

    let body = serde_json::json!({
        "object": "list",
        "data": data,
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap(),
    )
}

async fn handle_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    // 1. Read Request Body
    let body_bytes = match axum::body::to_bytes(request.into_body(), 1024 * 1024 * 10).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to read request body: {}", e),
            )
                .into_response();
        }
    };

    let mut payload: Value = match serde_json::from_slice(&body_bytes) {
        Ok(json) => json,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid JSON payload: {}", e),
            )
                .into_response();
        }
    };

    // 2. Identify target provider and API key
    let mut model_name = payload["model"].as_str().unwrap_or("").to_string();

    // Strip Sift suffix if present
    if let Some(idx) = model_name.find(" (Secured by SiftLLM)") {
        model_name = model_name[..idx].to_string();
    } else if let Some(idx) = model_name.find(" (PII secured by Sift)") {
        model_name = model_name[..idx].to_string();
    }

    // Find provider from registry based on model name
    let provider = state
        .provider_registry
        .providers
        .iter()
        .find(|p| {
            p.models.contains(&model_name)
                || p.models.contains(&format!("models/{}", model_name))
                || model_name.starts_with(&p.name)
                || (p.name == "google"
                    && (model_name.starts_with("gemini") || model_name.starts_with("google")))
                || (p.name == "gemini"
                    && (model_name.starts_with("gemini") || model_name.starts_with("google")))
                || (p.name == "anthropic"
                    && (model_name.starts_with("claude") || model_name.starts_with("anthropic")))
                || (p.name == "openai"
                    && (model_name.starts_with("gpt")
                        || model_name.starts_with("o1")
                        || model_name.starts_with("o3")
                        || model_name.starts_with("text-")))
        })
        .or_else(|| {
            // Fallback to first provider (usually Anthropic or OpenAI)
            state.provider_registry.providers.first()
        });

    let provider = match provider {
        Some(p) => p,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "No upstream providers configured",
            )
                .into_response();
        }
    };

    // If the provider registry expects the "models/" prefix, restore it
    let final_model = if provider.models.contains(&format!("models/{}", model_name)) {
        format!("models/{}", model_name)
    } else {
        model_name.clone()
    };

    payload["model"] = serde_json::Value::String(final_model);

    // Local providers (e.g. Ollama) need no key. Only error when the provider
    // declares a key source (env var or inline) but we couldn't resolve it.
    let api_key = provider.get_api_key();
    let expects_key = !provider.key_env.is_empty() || provider.api_key.is_some();
    if expects_key && api_key.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            format!("API key not found in environment: {}", provider.key_env),
        )
            .into_response();
    }

    // 3. Scan & Redact payload recursively.
    // The vault is per-request: it is filled here while tokenizing the outbound
    // payload and drained below when rehydrating the response. Because we rehydrate,
    // the harness only ever sees real values, so a request-scoped vault is enough —
    // no session store needed for correctness.
    let mut vault = Vault::new();
    let audit_trail = redact_json_value(
        &mut payload,
        &state.policy_engine,
        &state.detector,
        &mut vault,
    );

    // Check if any block action was triggered in Enforce mode
    let contains_block = audit_trail
        .iter()
        .any(|rec| rec.action_taken == Action::Block && rec.mode == crate::policy::Mode::Enforce);

    if contains_block {
        let block_reason = audit_trail
            .iter()
            .filter(|rec| rec.action_taken == Action::Block)
            .map(|rec| format!("Blocked sensitive category: {}", rec.category))
            .collect::<Vec<String>>()
            .join(", ");
        return (
            StatusCode::BAD_REQUEST,
            format!("Request blocked by Sift: {}", block_reason),
        )
            .into_response();
    }

    // Log detections to console
    for record in &audit_trail {
        log_audit(record);
    }

    // Re-inject any provider opaque data (e.g. Gemini's thought_signature) the harness
    // dropped, so thinking models accept the tool-call continuation. Done after
    // redaction so the opaque blob is never scanned or altered.
    signature::inject_request(&mut payload, &state.signatures);

    // 4. Forward to upstream LLM API
    let upstream_url = format!("{}/chat/completions", provider.base_url);

    let is_stream = payload["stream"].as_bool().unwrap_or(false);

    let mut req_builder = state
        .client
        .post(&upstream_url)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = &api_key {
        req_builder = req_builder.header(header::AUTHORIZATION, format!("Bearer {}", key));
    }

    // Handle Anthropic specific headers if needed
    if provider.name == "anthropic" {
        req_builder = req_builder.header("anthropic-version", "2023-06-01");
    }

    // Forward other headers if helpful (like anthropic-beta)
    if let Some(beta) = headers.get("anthropic-beta") {
        req_builder = req_builder.header("anthropic-beta", beta);
    }

    let upstream_res = match req_builder.json(&payload).send().await {
        Ok(res) => res,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Upstream request failed: {}", e),
            )
                .into_response();
        }
    };

    let status =
        StatusCode::from_u16(upstream_res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if is_stream {
        // Always feed the stream through the SSE-aware rehydrator: even with no PII it
        // captures provider opaque data (thought_signature) — and when the vault is
        // empty it forwards frames unchanged. It reassembles tokens split across delta
        // events and transport chunks, and owns the vault because this stream outlives
        // the request handler.
        let upstream_stream = upstream_res.bytes_stream();
        let rehydrator = SseRehydrator::new(vault, state.signatures.clone());
        let out_stream = futures_util::stream::unfold(
            (upstream_stream, rehydrator, false),
            |(mut upstream, mut rehydrator, ended)| async move {
                if ended {
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => {
                        let out = rehydrator.push(&chunk);
                        Some((
                            Ok::<Vec<u8>, std::io::Error>(out),
                            (upstream, rehydrator, false),
                        ))
                    }
                    Some(Err(e)) => {
                        let err = std::io::Error::other(e);
                        Some((Err(err), (upstream, rehydrator, true)))
                    }
                    None => {
                        // Upstream finished: emit whatever tail was held back.
                        let tail = rehydrator.flush();
                        Some((Ok(tail), (upstream, rehydrator, true)))
                    }
                }
            },
        );
        let body = Body::from_stream(out_stream);

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .body(body)
            .unwrap()
    } else {
        // Read the full response, then rehydrate: restore original values behind
        // every token the vault minted for this request (message content and
        // tool-call arguments alike).
        let body_content = match upstream_res.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to read upstream response: {}", e),
                )
                    .into_response();
            }
        };

        // Always parse to capture provider opaque data (thought_signature), even when
        // there is no PII to rehydrate; only re-serialize when we actually rewrote
        // something, to avoid disturbing the upstream bytes needlessly.
        let response_body = match serde_json::from_slice::<Value>(&body_content) {
            Ok(json) => {
                signature::capture_response(&json, &state.signatures);
                if vault.is_empty() {
                    Body::from(body_content)
                } else {
                    let mut json = json;
                    let mut revealed = Vec::new();
                    rehydrate_json_value(&mut json, &vault, &mut revealed);
                    log_reveals(&vault, &revealed);
                    match serde_json::to_vec(&json) {
                        Ok(bytes) => Body::from(bytes),
                        // Serialization back should never fail, but fall back to the
                        // untouched upstream bytes rather than dropping the response.
                        Err(_) => Body::from(body_content),
                    }
                }
            }
            // Non-JSON (e.g. an upstream error page): pass through unchanged.
            Err(_) => Body::from(body_content),
        };

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(response_body)
            .unwrap()
    }
}

fn redact_json_value(
    value: &mut Value,
    engine: &PolicyEngine,
    detector: &Detector,
    vault: &mut Vault,
) -> Vec<AuditRecord> {
    let mut audit_trail = Vec::new();
    match value {
        Value::String(s) => {
            let (new_s, mut records) = engine.process_text(detector, s, vault);
            *s = new_s;
            audit_trail.append(&mut records);
        }
        Value::Array(arr) => {
            for v in arr {
                audit_trail.append(&mut redact_json_value(v, engine, detector, vault));
            }
        }
        Value::Object(obj) => {
            for (k, v) in obj.iter_mut() {
                if skip_redaction(k) {
                    continue;
                }
                audit_trail.append(&mut redact_json_value(v, engine, detector, vault));
            }
        }
        _ => {}
    }
    audit_trail
}

/// Whether a JSON object key names a subtree we must NOT redact. Two reasons:
///  - identity/routing fields (`model`, `role`, `id`) aren't PII;
///  - `tools`/`functions`/`tool_choice`/`response_format` are *structural* schema
///    definitions. Tokenizing a word inside a tool's JSON schema (e.g. a property name
///    in `required`) corrupts the request — the provider rejects it with
///    "schema ... requires unspecified property '[PERSON_NAME_1]'". Only conversation
///    content (message text, tool-call arguments) should be pseudonymized.
fn skip_redaction(key: &str) -> bool {
    matches!(
        key,
        "model" | "role" | "id" | "tools" | "functions" | "tool_choice" | "response_format"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Mode, PolicyConfig};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn enforce_engine() -> PolicyEngine {
        PolicyEngine::new(PolicyConfig {
            mode: Mode::Enforce,
            ..Default::default()
        })
    }

    /// Exercises the whole request/response pipeline over a two-turn tool conversation:
    /// pseudonymize outbound, rehydrate inbound (content + tool args), capture the
    /// Gemini thought_signature, then on the next turn re-inject the signature and
    /// re-tokenize the PII coherently. This is the composition the unit tests cover
    /// piecewise.
    #[test]
    fn full_pipeline_two_turn_tool_conversation() {
        let engine = enforce_engine();
        let detector = Detector::new();
        let signatures: SignatureStore = Mutex::new(HashMap::new());

        // --- Turn 1 request: user message carrying PII ---
        let mut req1 = json!({
            "model": "gemini-x",
            "messages": [{ "role": "user", "content": "email juan@empresa.com about the weather" }]
        });
        let mut vault = Vault::new();
        redact_json_value(&mut req1, &engine, &detector, &mut vault);
        let sent = req1["messages"][0]["content"].as_str().unwrap();
        assert!(
            sent.contains("[EMAIL_1]"),
            "PII should be tokenized outbound"
        );
        assert!(
            !sent.contains("juan@empresa.com"),
            "real PII must not leave"
        );

        // --- Turn 1 response: model echoes the token in content and tool-call args,
        //     and attaches Gemini's opaque thought_signature ---
        let mut resp1 = json!({
            "choices": [{
                "message": {
                    "content": "I'll email [EMAIL_1].",
                    "tool_calls": [{
                        "id": "call_1",
                        "extra_content": { "google": { "thought_signature": "SIG==" } },
                        "function": { "name": "send_email", "arguments": "{\"to\": \"[EMAIL_1]\"}" }
                    }]
                }
            }]
        });
        signature::capture_response(&resp1, &signatures);
        let mut revealed = Vec::new();
        rehydrate_json_value(&mut resp1, &vault, &mut revealed);
        assert_eq!(
            resp1["choices"][0]["message"]["content"],
            "I'll email juan@empresa.com."
        );
        assert_eq!(
            resp1["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"to\": \"juan@empresa.com\"}"
        );

        // --- Turn 2 request: opencode resends the assistant tool_call WITHOUT the
        //     signature, plus the tool result. Sift must re-tokenize and re-inject. ---
        let mut req2 = json!({
            "model": "gemini-x",
            "messages": [
                { "role": "user", "content": "email juan@empresa.com about the weather" },
                { "role": "assistant", "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": { "name": "send_email", "arguments": "{\"to\": \"juan@empresa.com\"}" }
                }] },
                { "role": "tool", "tool_call_id": "call_1", "content": "sent to juan@empresa.com" }
            ]
        });
        let mut vault2 = Vault::new();
        redact_json_value(&mut req2, &engine, &detector, &mut vault2);
        signature::inject_request(&mut req2, &signatures);

        // Signature re-injected onto the assistant tool_call by id.
        assert_eq!(
            req2["messages"][1]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "SIG=="
        );
        // PII re-tokenized coherently in every message (same token throughout).
        let whole = serde_json::to_string(&req2).unwrap();
        assert!(
            !whole.contains("juan@empresa.com"),
            "no real PII in the forwarded request"
        );
        for path in [
            &req2["messages"][0]["content"],
            &req2["messages"][1]["tool_calls"][0]["function"]["arguments"],
            &req2["messages"][2]["content"],
        ] {
            assert!(path.as_str().unwrap().contains("[EMAIL_1]"));
        }
    }

    #[test]
    fn tool_definitions_are_not_redacted() {
        // Structural tool schemas must survive untouched — tokenizing a word inside them
        // corrupts the request (the provider rejects an "unspecified property"). Only the
        // conversation content should be pseudonymized.
        let engine = enforce_engine();
        let detector = Detector::new();
        let mut vault = Vault::new();
        let mut payload = json!({
            "model": "x",
            "messages": [{ "role": "user", "content": "mail juan@empresa.com" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "send_email",
                    "description": "emails admin@corp.com",
                    "parameters": {
                        "type": "object",
                        "properties": { "to": { "type": "string" } },
                        "required": ["to"]
                    }
                }
            }]
        });
        redact_json_value(&mut payload, &engine, &detector, &mut vault);

        // Message content is pseudonymized...
        assert!(payload["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("[EMAIL_1]"));
        // ...but the tool definition (even an email inside it) is left exactly as-is.
        assert_eq!(
            payload["tools"][0]["function"]["description"],
            "emails admin@corp.com"
        );
        assert_eq!(
            payload["tools"][0]["function"]["parameters"]["required"][0],
            "to"
        );
    }

    #[test]
    fn request_without_pii_is_left_untouched() {
        let engine = enforce_engine();
        let detector = Detector::new();
        let mut vault = Vault::new();
        let mut payload = json!({
            "model": "x",
            "messages": [{ "role": "user", "content": "hola que tal" }]
        });
        let original = payload.clone();
        let audit = redact_json_value(&mut payload, &engine, &detector, &mut vault);
        assert_eq!(payload, original, "no PII => payload unchanged");
        assert!(vault.is_empty());
        assert!(audit.is_empty());
    }

    #[test]
    fn same_value_gets_one_coherent_token_across_messages() {
        let engine = enforce_engine();
        let detector = Detector::new();
        let mut vault = Vault::new();
        let mut payload = json!({
            "model": "x",
            "messages": [
                { "role": "user", "content": "mail juan@empresa.com" },
                { "role": "assistant", "content": "noted juan@empresa.com" }
            ]
        });
        redact_json_value(&mut payload, &engine, &detector, &mut vault);
        for i in 0..2 {
            assert!(payload["messages"][i]["content"]
                .as_str()
                .unwrap()
                .contains("[EMAIL_1]"));
        }
        assert!(!serde_json::to_string(&payload)
            .unwrap()
            .contains("juan@empresa.com"));
    }

    #[test]
    fn allowlisted_value_passes_through_untokenized() {
        // The default allowlist includes example.com.
        let engine = enforce_engine();
        let detector = Detector::new();
        let mut vault = Vault::new();
        let mut payload = json!({
            "model": "x",
            "messages": [{ "role": "user", "content": "write to bob@example.com" }]
        });
        redact_json_value(&mut payload, &engine, &detector, &mut vault);
        assert_eq!(
            payload["messages"][0]["content"],
            "write to bob@example.com"
        );
        assert!(vault.is_empty());
    }

    #[test]
    fn block_policy_surfaces_a_block_record() {
        // A `block` category in enforce mode yields a Block audit record — which is what
        // the handler checks to reject the request with a 400 before it reaches the LLM.
        let mut policies = HashMap::new();
        policies.insert("api_key".to_string(), Action::Block);
        let engine = PolicyEngine::new(PolicyConfig {
            mode: Mode::Enforce,
            policies,
            allowlist: vec![],
        });
        let detector = Detector::new();
        let mut vault = Vault::new();
        let mut payload = json!({
            "model": "x",
            "messages": [{
                "role": "user",
                "content": "key sk-ant-api03-12345678901234567890123456789012-AA"
            }]
        });
        let audit = redact_json_value(&mut payload, &engine, &detector, &mut vault);
        assert!(audit.iter().any(|r| r.action_taken == Action::Block));
    }
}
