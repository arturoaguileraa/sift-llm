use crate::detect::RegexDetector;
use crate::vault::Vault;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Mode {
    #[default]
    Shadow,
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Action {
    /// Leave the value untouched (functional data you want the model to see).
    Pass,
    /// Irreversible replacement with a fixed tag (`[EMAIL_REDACTED]`). The value
    /// is stripped from the prompt and never comes back.
    Redact,
    /// Reversible replacement with a coherent token (`[EMAIL_1]`) recorded in the
    /// vault, then restored in the response. The primary action.
    #[default]
    Pseudonymize,
    /// Reject the whole request before it reaches the LLM (see the proxy's block
    /// handling: enforce mode returns a 400).
    Block,
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
        // Pseudonymize is the default posture: everything sensitive is replaced
        // with a reversible token and restored in the response, so the model
        // never sees real values but the harness still gets usable output.
        // `block` and `redact` remain available per-category for anyone who wants
        // a stricter (irreversible / reject) stance.
        let mut policies = HashMap::new();
        policies.insert("api_key".to_string(), Action::Pseudonymize);
        policies.insert("private_key".to_string(), Action::Pseudonymize);
        policies.insert("password".to_string(), Action::Pseudonymize);
        policies.insert("credit_card".to_string(), Action::Pseudonymize);
        policies.insert("connection_string".to_string(), Action::Pseudonymize);
        policies.insert("email".to_string(), Action::Pseudonymize);
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
    /// The token this value was pseudonymized to, if any. Lets the outbound log
    /// pair up with the inbound rehydration log.
    pub token: Option<String>,
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
        self.config
            .allowlist
            .iter()
            .any(|item| value.contains(item))
    }

    pub fn get_action(&self, category: &str) -> Action {
        self.config
            .policies
            .get(category)
            .copied()
            .unwrap_or(Action::Pseudonymize)
    }

    /// Scans `text`, applies the configured action per match, and returns the
    /// rewritten text plus an audit trail. Pseudonymized matches are minted into
    /// `vault` so the response path can restore them; the vault is shared across
    /// every call within a request, which is what keeps tokens (`[EMAIL_1]`)
    /// coherent across all messages in the payload.
    pub fn process_text(
        &self,
        detector: &RegexDetector,
        text: &str,
        vault: &mut Vault,
    ) -> (String, Vec<AuditRecord>) {
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

            result.push_str(&text[last_end..m.start]);

            let mut token = None;
            match (self.config.mode, action) {
                (Mode::Enforce, Action::Redact) => {
                    result.push_str(&m.redaction_tag);
                }
                (Mode::Enforce, Action::Pseudonymize) => {
                    let t = vault.tokenize(&m.matched_text, &m.category);
                    result.push_str(&t);
                    token = Some(t);
                }
                (Mode::Enforce, Action::Block) => {
                    result.push_str(&format!("[BLOCKED_{}]", m.category.to_uppercase()));
                }
                (Mode::Shadow, _) | (_, Action::Pass) => {
                    result.push_str(&m.matched_text);
                }
            }

            audit_trail.push(AuditRecord {
                category: m.category.clone(),
                original_text: m.matched_text.clone(),
                action_taken: action,
                mode: self.config.mode,
                token,
            });

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
        let mut vault = Vault::new();
        let input = "Email me at test@example.com";
        let (output, audit) = engine.process_text(&detector, input, &mut vault);

        // In shadow mode, output should match input (since example.com is allowed and mode is shadow)
        assert_eq!(output, input);
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn test_enforce_mode_pseudonymizes_email() {
        let engine = PolicyEngine::new(PolicyConfig {
            mode: Mode::Enforce,
            allowlist: vec![],
            ..Default::default()
        });
        let detector = RegexDetector::new();
        let mut vault = Vault::new();
        let input = "Email me at user@secretcorp.com";
        let (output, audit) = engine.process_text(&detector, input, &mut vault);

        // Default policy pseudonymizes email into a coherent, reversible token.
        assert_eq!(output, "Email me at [EMAIL_1]");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action_taken, Action::Pseudonymize);
        assert_eq!(vault.resolve("[EMAIL_1]"), Some("user@secretcorp.com"));
    }

    #[test]
    fn test_enforce_mode_redacts_when_configured() {
        let mut policies = std::collections::HashMap::new();
        policies.insert("email".to_string(), Action::Redact);
        let engine = PolicyEngine::new(PolicyConfig {
            mode: Mode::Enforce,
            policies,
            allowlist: vec![],
        });
        let detector = RegexDetector::new();
        let mut vault = Vault::new();
        let input = "Email me at user@secretcorp.com";
        let (output, audit) = engine.process_text(&detector, input, &mut vault);

        // Explicit `redact` still produces the irreversible fixed tag.
        assert_eq!(output, "Email me at [EMAIL_REDACTED]");
        assert_eq!(audit[0].action_taken, Action::Redact);
        assert!(vault.is_empty());
    }
}
