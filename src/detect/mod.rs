pub mod ner;
pub mod regex;

pub use ner::NerDetector;
pub use regex::{DetectionMatch, RegexDetector};

/// Composite detector: always runs regex, and optionally a semantic NER pass.
///
/// Regex owns the structured secrets (emails, keys, IBANs); NER adds the contextual
/// PII regex cannot see (names, orgs, locations). Matches from both are merged and
/// sorted by start offset; the policy engine drops overlaps as it walks them, so a
/// value caught by both is tokenized once.
pub struct Detector {
    regex: RegexDetector,
    ner: Option<NerDetector>,
}

impl Detector {
    /// Regex-only detector (NER disabled).
    pub fn new() -> Self {
        Self {
            regex: RegexDetector::new(),
            ner: None,
        }
    }

    /// Attaches a loaded NER detector to the regex pass.
    pub fn with_ner(mut self, ner: NerDetector) -> Self {
        self.ner = Some(ner);
        self
    }

    /// Runs every enabled detector and returns matches sorted by start offset.
    pub fn scan(&self, text: &str) -> Vec<DetectionMatch> {
        let mut matches = self.regex.scan(text);
        if let Some(ner) = &self.ner {
            matches.extend(ner.scan(text));
        }
        matches.sort_by_key(|m| m.start);
        matches
    }
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}
