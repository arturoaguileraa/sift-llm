use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub key_env: String,
    pub models: Vec<String>,
}

impl Provider {
    pub fn get_api_key(&self) -> Option<String> {
        if self.key_env.is_empty() {
            return None;
        }
        env::var(&self.key_env).ok()
    }
}

/// Well-known providers: name -> (base_url, api-key env var).
/// Lets `provider add anthropic` work without spelling out the URL.
pub fn preset(name: &str) -> Option<(String, String)> {
    let (url, key) = match name.to_lowercase().as_str() {
        "anthropic" => ("https://api.anthropic.com/v1", "ANTHROPIC_API_KEY"),
        "openai" => ("https://api.openai.com/v1", "OPENAI_API_KEY"),
        "groq" => ("https://api.groq.com/openai/v1", "GROQ_API_KEY"),
        "google" | "gemini" => (
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "GEMINI_API_KEY",
        ),
        "mistral" => ("https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
        "ollama" => ("http://localhost:11434/v1", ""),
        _ => return None,
    };
    Some((url.to_string(), key.to_string()))
}

/// Query an OpenAI-compatible `/models` endpoint and return the model ids.
pub async fn discover_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    if let Some(k) = api_key {
        req = req.header("Authorization", format!("Bearer {}", k));
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let json: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    let models = json["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRegistry {
    pub providers: Vec<Provider>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self {
            providers: vec![
                Provider {
                    name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com/v1".to_string(),
                    key_env: "ANTHROPIC_API_KEY".to_string(),
                    models: vec![
                        "claude-3-5-sonnet-20241022".to_string(),
                        "claude-3-opus-20240229".to_string(),
                        "claude-3-haiku-20240307".to_string(),
                    ],
                },
                Provider {
                    name: "openai".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    key_env: "OPENAI_API_KEY".to_string(),
                    models: vec![
                        "gpt-4o".to_string(),
                        "gpt-4o-mini".to_string(),
                        "o1-preview".to_string(),
                    ],
                },
                Provider {
                    name: "groq".to_string(),
                    base_url: "https://api.groq.com/openai/v1".to_string(),
                    key_env: "GROQ_API_KEY".to_string(),
                    models: vec![
                        "llama-3.3-70b-versatile".to_string(),
                        "llama-3.1-8b-instant".to_string(),
                    ],
                },
            ],
        }
    }
}

impl ProviderRegistry {
    /// Loads the persisted registry from disk, falling back to defaults.
    pub fn new() -> Self {
        Self::load()
    }

    /// Path to the persisted provider registry (`~/.config/sift/providers.json`).
    pub fn config_path() -> PathBuf {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("sift")
            .join("providers.json")
    }

    /// Reads the registry from disk. If the file is missing, returns the built-in
    /// defaults; if it is present but corrupt, warns and returns defaults.
    pub fn load() -> Self {
        match fs::read_to_string(Self::config_path()) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                eprintln!(
                    "Warning: could not parse {}: {}. Using defaults.",
                    Self::config_path().display(),
                    e
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Persists the registry to disk, creating the config directory if needed.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(path, json)
    }

    /// Inserts a provider, replacing any existing one with the same name.
    pub fn upsert(&mut self, provider: Provider) {
        if let Some(existing) = self.providers.iter_mut().find(|p| p.name == provider.name) {
            *existing = provider;
        } else {
            self.providers.push(provider);
        }
    }

    pub fn all_models(&self) -> Vec<(&str, &str)> {
        let mut list = Vec::new();
        for p in &self.providers {
            for m in &p.models {
                list.push((m.as_str(), p.name.as_str()));
            }
        }
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, url: &str) -> Provider {
        Provider {
            name: name.to_string(),
            base_url: url.to_string(),
            key_env: String::new(),
            models: vec![],
        }
    }

    #[test]
    fn test_upsert_replaces_existing() {
        let mut reg = ProviderRegistry { providers: vec![] };
        reg.upsert(provider("groq", "https://old"));
        reg.upsert(provider("groq", "https://new"));
        assert_eq!(reg.providers.len(), 1);
        assert_eq!(reg.providers[0].base_url, "https://new");
    }

    #[test]
    fn test_upsert_appends_new() {
        let mut reg = ProviderRegistry { providers: vec![] };
        reg.upsert(provider("groq", "https://a"));
        reg.upsert(provider("openai", "https://b"));
        assert_eq!(reg.providers.len(), 2);
    }

    #[test]
    fn test_preset_known_and_unknown() {
        let (url, key) = preset("anthropic").unwrap();
        assert_eq!(url, "https://api.anthropic.com/v1");
        assert_eq!(key, "ANTHROPIC_API_KEY");
        assert!(preset("does-not-exist").is_none());
    }
}
