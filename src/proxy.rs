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

use crate::detect::RegexDetector;
use crate::policy::{PolicyEngine, AuditRecord, Action};
use crate::provider::ProviderRegistry;
use crate::audit::log_audit;

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
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("✓ Sift listening on http://localhost:{}", port);
    axum::serve(listener, app).await?;

    Ok(())
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
            serde_json::json!({
                "id": format!("{} (PII secured by Sift)", model),
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
    let model_name = payload["model"].as_str().unwrap_or("").to_string();
    
    // Find provider from registry based on model name
    let provider = state.provider_registry.providers.iter().find(|p| {
        p.models.contains(&model_name)
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

    let api_key = match provider.get_api_key() {
        Some(key) => key,
        None => {
            return (StatusCode::BAD_REQUEST, format!("API key not found in environment: {}", provider.key_env)).into_response();
        }
    };

    // 3. Scan & Redact payload recursively
    let audit_trail = redact_json_value(&mut payload, &state.policy_engine, &state.detector);

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
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", api_key));

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
        // Return stream response
        let stream = upstream_res.bytes_stream().map(|result| {
            result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .body(Body::from_stream(stream))
            .unwrap()
    } else {
        // Return full response body
        let body_content = match upstream_res.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("Failed to read upstream response: {}", e)).into_response();
            }
        };
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_content))
            .unwrap()
    }
}

fn redact_json_value(
    value: &mut Value,
    engine: &PolicyEngine,
    detector: &RegexDetector,
) -> Vec<AuditRecord> {
    let mut audit_trail = Vec::new();
    match value {
        Value::String(s) => {
            let (new_s, mut records) = engine.process_text(detector, s);
            *s = new_s;
            audit_trail.append(&mut records);
        }
        Value::Array(arr) => {
            for v in arr {
                audit_trail.append(&mut redact_json_value(v, engine, detector));
            }
        }
        Value::Object(obj) => {
            for (k, v) in obj.iter_mut() {
                if k == "model" || k == "role" || k == "id" {
                    continue;
                }
                audit_trail.append(&mut redact_json_value(v, engine, detector));
            }
        }
        _ => {}
    }
    audit_trail
}
