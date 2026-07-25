use regex::Regex;
use once_cell::sync::Lazy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionMatch {
    pub category: String,
    pub matched_text: String,
    pub start: usize,
    pub end: usize,
    pub redaction_tag: String,
}

pub struct PatternDefinition {
    pub category: &'static str,
    pub regex: Lazy<Regex>,
    pub redaction_tag: &'static str,
}

static PATTERNS: Lazy<Vec<PatternDefinition>> = Lazy::new(|| {
    vec![
        // Secrets & Credentials
        PatternDefinition {
            category: "private_key",
            regex: Lazy::new(|| Regex::new(r"-----BEGIN\s+(?:RSA|EC|DSA|OPENSSH|PRIVATE)\s+KEY-----[\s\S]+?-----END\s+(?:RSA|EC|DSA|OPENSSH|PRIVATE)\s+KEY-----").unwrap()),
            redaction_tag: "[PRIVATE_KEY_REDACTED]",
        },
        PatternDefinition {
            category: "api_key",
            regex: Lazy::new(|| Regex::new(r"\bsk-ant-[a-zA-Z0-9\-_]{32,}\b").unwrap()),
            redaction_tag: "[ANTHROPIC_KEY_REDACTED]",
        },
        PatternDefinition {
            category: "api_key",
            regex: Lazy::new(|| Regex::new(r"\bsk-[a-zA-Z0-9]{32,}\b").unwrap()),
            redaction_tag: "[OPENAI_KEY_REDACTED]",
        },
        PatternDefinition {
            category: "api_key",
            regex: Lazy::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap()),
            redaction_tag: "[AWS_KEY_REDACTED]",
        },
        PatternDefinition {
            category: "api_key",
            regex: Lazy::new(|| Regex::new(r"\bghp_[a-zA-Z0-9]{36}\b|\bgithub_pat_[a-zA-Z0-9_]{82}\b").unwrap()),
            redaction_tag: "[GITHUB_TOKEN_REDACTED]",
        },
        PatternDefinition {
            category: "api_key",
            regex: Lazy::new(|| Regex::new(r"\bxox[baprs]-[0-9a-zA-Z]{10,}\b").unwrap()),
            redaction_tag: "[SLACK_TOKEN_REDACTED]",
        },
        PatternDefinition {
            category: "api_key",
            regex: Lazy::new(|| Regex::new(r"\bsk_live_[0-9a-zA-Z]{24}\b").unwrap()),
            redaction_tag: "[STRIPE_KEY_REDACTED]",
        },
        PatternDefinition {
            category: "connection_string",
            // The final char class excludes trailing sentence punctuation so a string
            // like "...prod." is not captured with the period — otherwise the same
            // connection string written twice (with and without a trailing period)
            // would be treated as two different secrets and break token coherence.
            regex: Lazy::new(|| Regex::new(r#"(?i)\b(?:postgres|postgresql|mysql|mongodb|redis)://[^\s"']*[^\s"'.,;:!?)\]}>]"#).unwrap()),
            redaction_tag: "[CONNECTION_STRING_REDACTED]",
        },
        PatternDefinition {
            category: "password",
            regex: Lazy::new(|| Regex::new(r#"(?i)"(?:password|passwd|pwd|secret)"\s*:\s*"([^"]+)""#).unwrap()),
            redaction_tag: "[PASSWORD_REDACTED]",
        },

        // Personal Identifiable Information (PII)
        PatternDefinition {
            category: "email",
            regex: Lazy::new(|| Regex::new(r"\b[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}\b").unwrap()),
            redaction_tag: "[EMAIL_REDACTED]",
        },
        PatternDefinition {
            category: "credit_card",
            regex: Lazy::new(|| Regex::new(r"\b(?:4[0-9]{12}(?:[0-9]{3})?|5[1-5][0-9]{14}|3[47][0-9]{13}|6(?:011|5[0-9]{2})[0-9]{12})\b").unwrap()),
            redaction_tag: "[CREDIT_CARD_REDACTED]",
        },
        PatternDefinition {
            category: "ip_address",
            regex: Lazy::new(|| Regex::new(r"\b(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b").unwrap()),
            redaction_tag: "[IP_REDACTED]",
        },
    ]
});

pub struct RegexDetector;

impl RegexDetector {
    pub fn new() -> Self {
        Self
    }

    pub fn scan(&self, text: &str) -> Vec<DetectionMatch> {
        let mut matches = Vec::new();
        for pattern in PATTERNS.iter() {
            for m in pattern.regex.find_iter(text) {
                matches.push(DetectionMatch {
                    category: pattern.category.to_string(),
                    matched_text: m.as_str().to_string(),
                    start: m.start(),
                    end: m.end(),
                    redaction_tag: pattern.redaction_tag.to_string(),
                });
            }
        }
        // Sort matches by start index
        matches.sort_by_key(|m| m.start);
        matches
    }

    /// Standalone redaction over all patterns. Exercised by the unit tests;
    /// the proxy path redacts via the policy engine instead.
    #[allow(dead_code)]
    pub fn redact(&self, text: &str) -> (String, Vec<DetectionMatch>) {
        let matches = self.scan(text);
        if matches.is_empty() {
            return (text.to_string(), matches);
        }

        let mut result = String::with_capacity(text.len());
        let mut last_end = 0;

        for m in &matches {
            if m.start >= last_end {
                result.push_str(&text[last_end..m.start]);
                result.push_str(&m.redaction_tag);
                last_end = m.end;
            }
        }

        if last_end < text.len() {
            result.push_str(&text[last_end..]);
        }

        (result, matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_email_redaction() {
        let detector = RegexDetector::new();
        let input = "Contact me at alice@example.com for info.";
        let (redacted, matches) = detector.redact(input);
        assert_eq!(redacted, "Contact me at [EMAIL_REDACTED] for info.");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].category, "email");
    }

    #[test]
    fn test_api_key_redaction() {
        let detector = RegexDetector::new();
        let input = "Here is my key: sk-ant-api03-12345678901234567890123456789012-AA";
        let (redacted, matches) = detector.redact(input);
        assert!(redacted.contains("[ANTHROPIC_KEY_REDACTED]"));
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn connection_string_excludes_trailing_punctuation() {
        let detector = RegexDetector::new();
        // Same connection string, one at end of a sentence (trailing period). Both must
        // match the identical text so downstream tokenization stays coherent. (scan()
        // also returns the embedded email match, so we filter to the connection_string.)
        let conn = |text: &str| -> String {
            detector
                .scan(text)
                .into_iter()
                .find(|m| m.category == "connection_string")
                .map(|m| m.matched_text)
                .expect("a connection_string match")
        };
        assert_eq!(
            conn("use postgres://user:pw@db.internal:5432/prod."),
            "postgres://user:pw@db.internal:5432/prod"
        );
        assert_eq!(
            conn("use postgres://user:pw@db.internal:5432/prod."),
            conn("use postgres://user:pw@db.internal:5432/prod here")
        );
    }
}
