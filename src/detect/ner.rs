//! Semantic PII detection via a local GLiNER model (ONNX, run with `gline-rs`/`ort`).
//!
//! Regex catches structured secrets (emails, keys, IBANs); NER catches the
//! *contextual* PII regex cannot see: person names, organizations, locations. The two
//! compose in [`super::Detector`]. The model is loaded once and reused; inference is
//! serialized behind a `Mutex` (CPU-bound, single-user local proxy).

use std::path::Path;
use std::sync::Mutex;

use gliner::model::pipeline::span::SpanMode;
use gliner::model::{input::text::TextInput, params::Parameters, GLiNER};
use orp::params::RuntimeParameters;

use super::regex::DetectionMatch;

/// Entity labels we ask the model for, and the Sift policy category each maps to.
/// The label is what GLiNER matches on; the category is what `policies.yaml` keys on.
const LABELS: &[(&str, &str)] = &[
    ("person", "person_name"),
    ("organization", "organization"),
    ("location", "location"),
    ("address", "address"),
    ("phone number", "phone"),
    ("email", "email"),
    ("credit card number", "credit_card"),
    ("iban", "iban"),
    ("password", "password"),
];

pub struct NerDetector {
    model: Mutex<GLiNER<SpanMode>>,
    labels: Vec<String>,
    threshold: f32,
}

impl NerDetector {
    /// Loads the GLiNER model from `dir` (expects `model.onnx` and `tokenizer.json`).
    /// Returns an error if the files are missing or the model fails to load.
    pub fn load(dir: &Path, threshold: f32) -> Result<Self, String> {
        let tokenizer = dir.join("tokenizer.json");
        let onnx = dir.join("model.onnx");
        if !tokenizer.exists() || !onnx.exists() {
            return Err(format!(
                "model files not found in {} (run `sift model pull`)",
                dir.display()
            ));
        }
        let model = GLiNER::<SpanMode>::new(
            Parameters::default(),
            RuntimeParameters::default(),
            tokenizer.to_string_lossy().as_ref(),
            onnx.to_string_lossy().as_ref(),
        )
        .map_err(|e| format!("failed to load NER model: {e}"))?;

        Ok(Self {
            model: Mutex::new(model),
            labels: LABELS.iter().map(|(label, _)| label.to_string()).collect(),
            threshold,
        })
    }

    /// Runs NER over `text` and returns matches above the confidence threshold.
    /// Failures (tokenization/inference) degrade to "no matches" rather than erroring:
    /// the regex pass still runs, so detection never fully breaks on a bad input.
    pub fn scan(&self, text: &str) -> Vec<DetectionMatch> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let labels: Vec<&str> = self.labels.iter().map(String::as_str).collect();
        let input = match TextInput::from_str(&[text], &labels) {
            Ok(input) => input,
            Err(_) => return Vec::new(),
        };
        let output = {
            let model = self.model.lock().unwrap();
            match model.inference(input) {
                Ok(output) => output,
                Err(_) => return Vec::new(),
            }
        };

        let mut matches = Vec::new();
        for spans in output.spans {
            for span in spans {
                if span.probability() < self.threshold {
                    continue;
                }
                let category = category_for(span.class());
                let (start, end) = match locate(text, span.offsets(), span.text()) {
                    Some(range) => range,
                    None => continue, // could not place the span reliably; skip it
                };
                matches.push(DetectionMatch {
                    category: category.to_string(),
                    matched_text: span.text().to_string(),
                    start,
                    end,
                    redaction_tag: format!("[{}_REDACTED]", category.to_uppercase()),
                });
            }
        }
        matches
    }
}

/// Maps a GLiNER entity label to its Sift policy category (falls back to the label
/// with spaces normalized to underscores).
fn category_for(label: &str) -> String {
    LABELS
        .iter()
        .find(|(l, _)| *l == label)
        .map(|(_, cat)| cat.to_string())
        .unwrap_or_else(|| label.replace(' ', "_"))
}

/// Resolves a span to a byte range in `text`. GLiNER offsets are character-based, so we
/// convert to bytes; if the resulting slice doesn't match the expected text (offset
/// semantics differ, or multi-byte edge cases), fall back to locating the exact text.
fn locate(
    text: &str,
    (char_start, char_end): (usize, usize),
    expected: &str,
) -> Option<(usize, usize)> {
    let byte_start = text.char_indices().nth(char_start).map(|(b, _)| b);
    let byte_end = if char_end >= text.chars().count() {
        Some(text.len())
    } else {
        text.char_indices().nth(char_end).map(|(b, _)| b)
    };
    if let (Some(bs), Some(be)) = (byte_start, byte_end) {
        if be <= text.len() && bs <= be && text.get(bs..be) == Some(expected) {
            return Some((bs, be));
        }
    }
    // Fallback: locate the exact matched text.
    text.find(expected).map(|b| (b, b + expected.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_resolves_char_offsets_to_bytes_with_accents() {
        // "ñ" is 2 bytes, so the char offset of "X" (1) differs from its byte offset (2).
        // locate must return byte offsets whose slice equals the expected text.
        assert_eq!(locate("ñX", (1, 2), "X"), Some((2, 3)));

        // A realistic Spanish span: byte offsets must land exactly on the entity.
        let text = "Contacta a María Pérez en Madrid";
        let target = "María Pérez";
        let char_start = text[..text.find(target).unwrap()].chars().count();
        let char_end = char_start + target.chars().count();
        let (bs, be) = locate(text, (char_start, char_end), target).unwrap();
        assert_eq!(&text[bs..be], target);
    }

    #[test]
    fn locate_falls_back_to_exact_text_when_offsets_are_off() {
        // Offsets out of range → fall back to searching the exact text.
        assert_eq!(locate("abcXdef", (99, 100), "X"), Some((3, 4)));
    }

    #[test]
    fn category_for_maps_known_labels_and_normalizes_unknowns() {
        assert_eq!(category_for("person"), "person_name");
        assert_eq!(category_for("phone number"), "phone");
        assert_eq!(category_for("credit card number"), "credit_card");
        // Unknown label: normalized (spaces -> underscores) rather than dropped.
        assert_eq!(category_for("job title"), "job_title");
    }
}
