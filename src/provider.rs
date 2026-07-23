use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub key_env: String,
    pub models: Vec<String>,
}

impl Provider {
    pub fn get_api_key(&self) -> Option<String> {
        env::var(&self.key_env).ok()
    }
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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn find_by_name(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn add(&mut self, provider: Provider) {
        self.providers.push(provider);
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
