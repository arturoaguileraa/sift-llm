use std::collections::HashMap;
use once_cell::sync::Lazy;
use regex::Regex;

/// Matches a complete pseudonymization token like `[EMAIL_1]` or
/// `[CONNECTION_STRING_12]`. The trailing `_<digits>]` is deliberate: it means
/// the token regex never collides with irreversible redaction tags
/// (`[EMAIL_REDACTED]`) or block markers (`[BLOCKED_EMAIL]`), so rehydration
/// only ever touches values it actually minted.
pub static TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[[A-Z][A-Z_]*_\d+\]").unwrap());

/// Per-request, in-memory bidirectional map between real PII values and their
/// coherent pseudonymization tokens.
///
/// Lifecycle is deliberately short: the outbound path fills it (`tokenize`) while
/// redacting the request, and the inbound path drains it (`resolve` / `rehydrate`)
/// while rewriting the response. It lives only for one request/response cycle and
/// is never persisted.
///
/// Why a per-request vault is enough (and not a persistent session store): because
/// we rehydrate the response, the harness (opencode) only ever sees real values and
/// re-sends them on the next turn, so each request re-tokenizes from scratch. A
/// persistent, session-keyed vault only becomes useful later as a prompt-cache
/// coherence optimization, not for correctness. The `SessionStore` that would own
/// these vaults can be added without changing this type.
#[derive(Debug, Default)]
pub struct Vault {
    forward: HashMap<String, String>, // "juan@empresa.com" -> "[EMAIL_1]"
    reverse: HashMap<String, String>, // "[EMAIL_1]" -> "juan@empresa.com"
    counters: HashMap<String, u32>,   // category -> highest number minted so far
}

impl Vault {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the token for `value`, minting a fresh coherent one on first sight.
    /// A value seen twice in the same request reuses its token, so `[EMAIL_1]` is
    /// stable across every message in the payload and multi-turn context is not
    /// broken.
    pub fn tokenize(&mut self, value: &str, category: &str) -> String {
        if let Some(existing) = self.forward.get(value) {
            return existing.clone();
        }
        let counter = self.counters.entry(category.to_string()).or_insert(0);
        *counter += 1;
        let token = format!("[{}_{}]", category.to_uppercase(), counter);
        self.forward.insert(value.to_string(), token.clone());
        self.reverse.insert(token.clone(), value.to_string());
        token
    }

    /// Looks up the original value behind a token, if this vault minted it.
    pub fn resolve(&self, token: &str) -> Option<&str> {
        self.reverse.get(token).map(|s| s.as_str())
    }

    /// True when nothing was tokenized, letting callers skip rehydration entirely.
    pub fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }

    /// Replaces every complete token in `text` with its original value. Tokens
    /// this vault never minted are left untouched, so it is safe to run over any
    /// text. Used directly for buffered (non-streaming) responses and as the core
    /// primitive of the streaming rehydrator.
    pub fn rehydrate(&self, text: &str) -> String {
        if self.reverse.is_empty() || !text.contains('[') {
            return text.to_string();
        }
        TOKEN_RE
            .replace_all(text, |caps: &regex::Captures| {
                let token = &caps[0];
                self.resolve(token).unwrap_or(token).to_string()
            })
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_coherent_within_a_request() {
        let mut vault = Vault::new();
        let first = vault.tokenize("juan@empresa.com", "email");
        let again = vault.tokenize("juan@empresa.com", "email");
        let other = vault.tokenize("ana@empresa.com", "email");

        assert_eq!(first, "[EMAIL_1]");
        assert_eq!(again, "[EMAIL_1]"); // same value -> same token
        assert_eq!(other, "[EMAIL_2]"); // new value -> next number
    }

    #[test]
    fn counters_are_independent_per_category() {
        let mut vault = Vault::new();
        assert_eq!(vault.tokenize("juan@empresa.com", "email"), "[EMAIL_1]");
        assert_eq!(vault.tokenize("10.0.0.4", "ip_address"), "[IP_ADDRESS_1]");
        assert_eq!(vault.tokenize("ana@empresa.com", "email"), "[EMAIL_2]");
    }

    #[test]
    fn rehydrate_restores_known_tokens_and_leaves_unknown() {
        let mut vault = Vault::new();
        let token = vault.tokenize("juan@empresa.com", "email");
        let text = format!("Reply to {token} and cc [EMAIL_9] please");
        assert_eq!(
            vault.rehydrate(&text),
            "Reply to juan@empresa.com and cc [EMAIL_9] please"
        );
    }

    #[test]
    fn rehydrate_is_a_noop_without_tokens() {
        let vault = Vault::new();
        assert_eq!(vault.rehydrate("nothing to do here"), "nothing to do here");
    }
}
