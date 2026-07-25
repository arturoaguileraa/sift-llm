use axum::{
    body::Body,
    extract::{State, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use std::net::SocketAddr;
use futures_util::stream::StreamExt;
use reqwest::Client;
use serde_json::Value;
use colored::Colorize;

use crate::detect::RegexDetector;
use crate::policy::{PolicyEngine, AuditRecord, Action};
use crate::provider::ProviderRegistry;
use crate::audit::log_audit;
use crate::vault::Vault;
use crate::rehydrate::{log_reveals, rehydrate_json_value, SseRehydrator};

#[derive(Clone)]
pub struct AppState {
    pub policy_engine: Arc<PolicyEngine>,
    pub detector: Arc<RegexDetector>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub client: Client,
}

pub async fn run_proxy(port: u16, policy_engine: PolicyEngine) -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState {
        policy_engine: Arc::new(policy_engine),
        detector: Arc::new(RegexDetector::new()),
        provider_registry: Arc::new(ProviderRegistry::new()),
        client: Client::new(),
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

    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], serde_json::to_string(&body).unwrap())
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
            return (StatusCode::BAD_REQUEST, format!("Failed to read request body: {}", e)).into_response();
        }
    };

    let mut payload: Value = match serde_json::from_slice(&body_bytes) {
        Ok(json) => json,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON payload: {}", e)).into_response();
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
    let provider = state.provider_registry.providers.iter().find(|p| {
        p.models.contains(&model_name)
            || p.models.contains(&format!("models/{}", model_name))
            || model_name.starts_with(&p.name)
            || (p.name == "google" && (model_name.starts_with("gemini") || model_name.starts_with("google")))
            || (p.name == "gemini" && (model_name.starts_with("gemini") || model_name.starts_with("google")))
            || (p.name == "anthropic" && (model_name.starts_with("claude") || model_name.starts_with("anthropic")))
            || (p.name == "openai" && (model_name.starts_with("gpt") || model_name.starts_with("o1") || model_name.starts_with("o3") || model_name.starts_with("text-")))
    }).or_else(|| {
        // Fallback to first provider (usually Anthropic or OpenAI)
        state.provider_registry.providers.first()
    });

    let provider = match provider {
        Some(p) => p,
        None => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "No upstream providers configured").into_response();
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
    let audit_trail = redact_json_value(&mut payload, &state.policy_engine, &state.detector, &mut vault);

    // Check if any block action was triggered in Enforce mode
    let contains_block = audit_trail.iter().any(|rec| {
        rec.action_taken == Action::Block && rec.mode == crate::policy::Mode::Enforce
    });

    if contains_block {
        let block_reason = audit_trail
            .iter()
            .filter(|rec| rec.action_taken == Action::Block)
            .map(|rec| format!("Blocked sensitive category: {}", rec.category))
            .collect::<Vec<String>>()
            .join(", ");
        return (StatusCode::BAD_REQUEST, format!("Request blocked by Sift: {}", block_reason)).into_response();
    }

    // Log detections to console
    for record in &audit_trail {
        log_audit(record);
    }

    // 4. Forward to upstream LLM API
    let upstream_url = format!("{}/chat/completions", provider.base_url);
    
    let is_stream = payload["stream"].as_bool().unwrap_or(false);

    let mut req_builder = state.client.post(&upstream_url)
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
            return (StatusCode::BAD_GATEWAY, format!("Upstream request failed: {}", e)).into_response();
        }
    };

    let status = StatusCode::from_u16(upstream_res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if is_stream {
        let body = if vault.is_empty() {
            // Nothing was tokenized: pass the stream straight through.
            let stream = upstream_res.bytes_stream().map(|result| {
                result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            });
            Body::from_stream(stream)
        } else {
            // Rehydrate on the fly: feed each upstream chunk through the SSE-aware
            // rehydrator (which reassembles tokens split across delta events and across
            // transport chunks) and flush any held-back tail once upstream ends. The
            // rehydrator owns the vault because this stream outlives the request handler.
            let upstream_stream = upstream_res.bytes_stream();
            let rehydrator = SseRehydrator::new(vault);
            let out_stream = futures_util::stream::unfold(
                (upstream_stream, rehydrator, false),
                |(mut upstream, mut rehydrator, ended)| async move {
                    if ended {
                        return None;
                    }
                    match upstream.next().await {
                        Some(Ok(chunk)) => {
                            let out = rehydrator.push(&chunk);
                            Some((Ok::<Vec<u8>, std::io::Error>(out), (upstream, rehydrator, false)))
                        }
                        Some(Err(e)) => {
                            let err = std::io::Error::new(std::io::ErrorKind::Other, e);
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
            Body::from_stream(out_stream)
        };

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
                return (StatusCode::BAD_GATEWAY, format!("Failed to read upstream response: {}", e)).into_response();
            }
        };

        let response_body = if vault.is_empty() {
            Body::from(body_content)
        } else {
            match serde_json::from_slice::<Value>(&body_content) {
                Ok(mut json) => {
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
                // Non-JSON (e.g. an upstream error page): pass through unchanged.
                Err(_) => Body::from(body_content),
            }
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
    detector: &RegexDetector,
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
                if k == "model" || k == "role" || k == "id" {
                    continue;
                }
                audit_trail.append(&mut redact_json_value(v, engine, detector, vault));
            }
        }
        _ => {}
    }
    audit_trail
}
