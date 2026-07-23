use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use crate::detect::RegexDetector;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Shadow,
    Enforce,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Shadow
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Pass,
    Redact,
    Block,
}

impl Default for Action {
    fn default() -> Self {
        Action::Redact
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub mode: Mode,

    #[serde(default)]
    pub policies: HashMap<String, Action>,

    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let mut policies = HashMap::new();
        policies.insert("api_key".to_string(), Action::Block);
        policies.insert("private_key".to_string(), Action::Block);
        policies.insert("password".to_string(), Action::Block);
        policies.insert("credit_card".to_string(), Action::Block);
        policies.insert("connection_string".to_string(), Action::Redact);
        policies.insert("email".to_string(), Action::Redact);
        policies.insert("ip_address".to_string(), Action::Pass);

        PolicyConfig {
            mode: Mode::Shadow,
            policies,
            allowlist: vec!["example.com".to_string(), "127.0.0.1".to_string()],
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub category: String,
    pub original_text: String,
    pub action_taken: Action,
    pub mode: Mode,
}

pub struct PolicyEngine {
    pub config: PolicyConfig,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig) -> Self {
        Self { config }
    }

    pub fn load_or_default<P: AsRef<Path>>(path: P) -> Self {
        if let Ok(content) = fs::read_to_string(path) {
            if let Ok(config) = serde_yaml::from_str(&content) {
                return Self::new(config);
            }
        }
        Self::new(PolicyConfig::default())
    }

    pub fn is_allowed(&self, value: &str) -> bool {
        self.config.allowlist.iter().any(|item| value.contains(item))
    }

    pub fn get_action(&self, category: &str) -> Action {
        self.config
            .policies
            .get(category)
            .copied()
            .unwrap_or(Action::Redact)
    }

    pub fn process_text(&self, detector: &RegexDetector, text: &str) -> (String, Vec<AuditRecord>) {
        let matches = detector.scan(text);
        if matches.is_empty() {
            return (text.to_string(), Vec::new());
        }

        let mut audit_trail = Vec::new();
        let mut result = String::with_capacity(text.len());
        let mut last_end = 0;

        for m in &matches {
            if m.start < last_end {
                continue; // Skip overlapping matches
            }

            let action = if self.is_allowed(&m.matched_text) {
                Action::Pass
            } else {
                self.get_action(&m.category)
            };

            audit_trail.push(AuditRecord {
                category: m.category.clone(),
                original_text: m.matched_text.clone(),
                action_taken: action,
                mode: self.config.mode,
            });

            result.push_str(&text[last_end..m.start]);

            match (self.config.mode, action) {
                (Mode::Enforce, Action::Redact) => {
                    result.push_str(&m.redaction_tag);
                }
                (Mode::Enforce, Action::Block) => {
                    result.push_str(&format!("[BLOCKED_{}]", m.category.to_uppercase()));
                }
                (Mode::Shadow, _) | (_, Action::Pass) => {
                    result.push_str(&m.matched_text);
                }
            }

            last_end = m.end;
        }

        if last_end < text.len() {
            result.push_str(&text[last_end..]);
        }

        (result, audit_trail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shadow_mode_keeps_original_text() {
        let engine = PolicyEngine::new(PolicyConfig {
            mode: Mode::Shadow,
            ..Default::default()
        });
        let detector = RegexDetector::new();
        let input = "Email me at test@example.com";
        let (output, audit) = engine.process_text(&detector, input);
        
        // In shadow mode, output should match input (since example.com is allowed and mode is shadow)
        assert_eq!(output, input);
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn test_enforce_mode_redacts_email() {
        let engine = PolicyEngine::new(PolicyConfig {
            mode: Mode::Enforce,
            allowlist: vec![],
            ..Default::default()
        });
        let detector = RegexDetector::new();
        let input = "Email me at user@secretcorp.com";
        let (output, audit) = engine.process_text(&detector, input);

        assert_eq!(output, "Email me at [EMAIL_REDACTED]");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action_taken, Action::Redact);
    }
}
