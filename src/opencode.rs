//! Keeps opencode's config in sync with the Sift registry.
//!
//! opencode does not auto-discover models from a custom OpenAI-compatible
//! provider's `/v1/models`; the models must be listed in `opencode.json(c)`.
//! This module writes the current registry's models into the `sift-llm`
//! provider block, preserving the rest of the user's opencode config.

use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

use crate::provider::ProviderRegistry;

const GATEWAY_BASE_URL: &str = "http://localhost:8787/v1";
// Must match the suffix the proxy adds in `/v1/models` and strips on completions.
const SUFFIX: &str = " (Secured by SiftLLM)";

fn opencode_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("opencode")
}

/// Default opencode config path (`opencode.jsonc`, or `opencode.json` if that
/// is what already exists).
fn default_config_path() -> PathBuf {
    let dir = opencode_dir();
    let json = dir.join("opencode.json");
    if json.exists() && !dir.join("opencode.jsonc").exists() {
        return json;
    }
    dir.join("opencode.jsonc")
}

/// True if opencode looks set up on this machine (its config dir exists).
pub fn is_configured() -> bool {
    opencode_dir().exists()
}

/// Writes the current registry's models into opencode's `sift-llm` provider,
/// preserving everything else. Returns (model count, path written).
pub fn sync_opencode(path: Option<&str>) -> Result<(usize, PathBuf), String> {
    let path = path.map(PathBuf::from).unwrap_or_else(default_config_path);

    let mut root: Value = if path.exists() {
        let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&content).map_err(|e| {
            format!(
                "could not parse {} as JSON ({}). If it has comments, remove them and retry.",
                path.display(),
                e
            )
        })?
    } else {
        json!({ "$schema": "https://opencode.ai/config.json" })
    };

    // Build the models map from the registry (id must match what the gateway
    // exposes so the proxy can strip the suffix and route).
    let registry = ProviderRegistry::load();
    let mut models = serde_json::Map::new();
    for (model, _provider) in registry.all_models() {
        let clean = model.strip_prefix("models/").unwrap_or(model);
        let id = format!("{}{}", clean, SUFFIX);
        models.insert(id, json!({ "name": format!("{} (Sift secured)", clean) }));
    }
    let count = models.len();

    let obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
    let provider = obj
        .entry("provider")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| "\"provider\" is not an object".to_string())?;
    let sift = provider
        .entry("sift-llm")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| "\"provider.sift-llm\" is not an object".to_string())?;

    // Create scaffolding only if missing; preserve any user customisation.
    sift.entry("npm")
        .or_insert_with(|| json!("@ai-sdk/openai-compatible"));
    sift.entry("name").or_insert_with(|| json!("Sift LLM"));
    let options = sift.entry("options").or_insert_with(|| json!({}));
    if let Some(opts) = options.as_object_mut() {
        opts.entry("baseURL").or_insert_with(|| json!(GATEWAY_BASE_URL));
    }
    // Only the models block is authoritative from Sift.
    sift.insert("models".to_string(), Value::Object(models));

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    fs::write(&path, out).map_err(|e| e.to_string())?;

    Ok((count, path))
}

/// Removes the `sift-llm` provider from opencode's config, leaving the rest of
/// the file intact. Returns whether the provider was present.
pub fn remove_from_opencode(path: Option<&str>) -> Result<bool, String> {
    let path = path.map(PathBuf::from).unwrap_or_else(default_config_path);
    if !path.exists() {
        return Ok(false);
    }
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut root: Value = serde_json::from_str(&content)
        .map_err(|e| format!("could not parse {} as JSON ({})", path.display(), e))?;

    let removed = root
        .get_mut("provider")
        .and_then(|p| p.as_object_mut())
        .map(|prov| prov.remove("sift-llm").is_some())
        .unwrap_or(false);

    if removed {
        let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        fs::write(&path, out).map_err(|e| e.to_string())?;
    }
    Ok(removed)
}
