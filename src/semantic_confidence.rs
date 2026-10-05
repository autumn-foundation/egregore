//! Calibrated confidence verdicts for `eg query semantic` (issues #263, #221).
//!
//! Both transports (embedded `query_semantic` and the daemon `semantic_search`
//! verb) stamp every non-empty semantic answer with a top-level
//! [`SemanticConfidenceVerdict`]: a deterministic, byte-stable enum derived
//! from the ranked score distribution. Rows below the confident threshold are
//! still returned — flagged per-row as weak leads — so an empty result stays a
//! clean "no match" while a below-threshold non-empty answer is an explicit
//! `weak`/`abstain` verdict, never a silent drop.
//!
//! ## Thresholds
//!
//! The thresholds live here, in exactly one place:
//!
//! * [`SEMANTIC_CONFIDENT_THRESHOLD`] = 0.39 — best score at or above this is
//!   `confident`. Calibrated on the issue #221 measurement set (27 answerable
//!   + 20 unanswerable queries over a full-repo graph, 15,098 File/Symbol
//!   candidates): 24/27 answerable (88.9%) classify confident, meeting the
//!   >=85% bar.
//! * [`SEMANTIC_WEAK_THRESHOLD`] = 0.34 — best score in `[0.34, 0.39)` is
//!   `weak`; below is `abstain`. With these bars, 19/20 unanswerable (95.0%)
//!   classify weak/abstain, meeting the >=90% bar. See
//!   `corpus/semantic_confidence_fixture.json` for the derivation note.
//!
//! ## Determinism
//!
//! The verdict is a pure function of the ranked scores (a cosine similarity is
//! deterministic for fixed store + query vector + model), so identical
//! invocations produce byte-identical verdict objects on both transports. The
//! daemon stamps the verdict itself so MCP consumers see the same contract as
//! the CLI.

use serde::{Deserialize, Serialize};

/// Best-score bar for a `confident` verdict.
///
/// Calibrated on the issue #221 measurement set: 27 answerable + 20
/// unanswerable queries over a full-repo graph (15,098 File/Symbol candidates,
/// the exact `candidate_from_record` population). 24/27 answerable (88.9%)
/// score at or above this bar. See
/// `corpus/semantic_confidence_fixture.json` for the derivation.
pub const SEMANTIC_CONFIDENT_THRESHOLD: f32 = 0.39;

/// Best-score floor for a `weak` verdict; below this the answer is `abstain`.
///
/// Calibrated with the confident threshold: 19/20 unanswerable queries (95.0%)
/// fall below the confident bar (weak or abstain), meeting the >=90% bar.
pub const SEMANTIC_WEAK_THRESHOLD: f32 = 0.34;

/// Provenance label naming how the thresholds were set.
///
/// `corpus_calibrated_confidence_floor`: measured on the semantic relevance
/// corpus plus a held-out unanswerable set — see
/// `corpus/semantic_confidence_fixture.json`.
pub const SEMANTIC_SELECTION_BASIS: &str = "corpus_calibrated_confidence_floor";

/// Answer-level confidence verdict stamped on every semantic answer (issue #221).
///
/// Rendered in JSON as `confident` / `weak` / `abstain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticConfidence {
    /// Best score `>=` [`SEMANTIC_CONFIDENT_THRESHOLD`]: the answer is trusted.
    Confident,
    /// Best score in `[SEMANTIC_WEAK_THRESHOLD, SEMANTIC_CONFIDENT_THRESHOLD)`:
    /// a candidate lead the caller must treat as unverified.
    Weak,
    /// Best score `<` [`SEMANTIC_WEAK_THRESHOLD`]: no trustworthy signal; the
    /// returned rows are low-confidence leads only.
    Abstain,
}

impl SemanticConfidence {
    /// Machine-readable tag used in JSON and text output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confident => "confident",
            Self::Weak => "weak",
            Self::Abstain => "abstain",
        }
    }

    /// Deterministic verdict from the best score of the ranked candidate set.
    ///
    /// Pure function of score: identical store + query + model yields an
    /// identical verdict.
    #[must_use]
    pub const fn of_best(best_score: f32) -> Self {
        if best_score >= SEMANTIC_CONFIDENT_THRESHOLD {
            Self::Confident
        } else if best_score >= SEMANTIC_WEAK_THRESHOLD {
            Self::Weak
        } else {
            Self::Abstain
        }
    }
}

/// Top-level verdict object stamped on every non-empty semantic answer.
///
/// Serialized as the single `{"confidence": {...}}` line in JSON answers;
/// the daemon verb stamps the same object so MCP consumers inherit the
/// contract without CLI help.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticConfidenceVerdict {
    /// The answer-level verdict.
    pub verdict: SemanticConfidence,
    /// Highest score among the ranked candidates — the verdict's input.
    pub best_score: f32,
    /// Candidate count the verdict was computed over (pre-`--limit` truncation).
    pub total_candidates: usize,
    /// Bar for `confident` ([`SEMANTIC_CONFIDENT_THRESHOLD`]).
    pub confident_threshold: f32,
    /// Floor for `weak` ([`SEMANTIC_WEAK_THRESHOLD`]); below is `abstain`.
    pub weak_threshold: f32,
    /// How the thresholds were set ([`SEMANTIC_SELECTION_BASIS`]).
    pub selection_basis: String,
}

impl SemanticConfidenceVerdict {
    /// Builds a verdict from the best score and the pre-truncation candidate
    /// count. Thresholds and basis come from the module constants, so every
    /// producer stamps the same derivation.
    #[must_use]
    pub fn new(verdict: SemanticConfidence, best_score: f32, total_candidates: usize) -> Self {
        Self {
            verdict,
            best_score,
            total_candidates,
            confident_threshold: SEMANTIC_CONFIDENT_THRESHOLD,
            weak_threshold: SEMANTIC_WEAK_THRESHOLD,
            selection_basis: SEMANTIC_SELECTION_BASIS.to_owned(),
        }
    }

    /// One-line text rendering: `confidence: <verdict> (...)`.
    ///
    /// Carries the same fields as the JSON rendering (the JSON shape is the
    /// stability contract; text mirrors it for human readers).
    #[must_use]
    pub fn as_text(&self) -> String {
        format!(
            "confidence: {} (best score {:.4}; confident threshold {}; weak threshold {}; {} candidates; basis {})",
            self.verdict.as_str(),
            self.best_score,
            self.confident_threshold,
            self.weak_threshold,
            self.total_candidates,
            self.selection_basis,
        )
    }
}

/// Per-row confidence band (issue #263): `Strong` when the row's score is at
/// or above [`SEMANTIC_CONFIDENT_THRESHOLD`], else `Weak`. Rows are retrieval
/// leads, never proof — see `docs/cli/query.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBand {
    /// `score >= SEMANTIC_CONFIDENT_THRESHOLD`: the row cleared the bar.
    Strong,
    /// `score < SEMANTIC_CONFIDENT_THRESHOLD`: nearest-neighbor noise risk;
    /// the row is a lead to verify, not a result to cite.
    Weak,
}

impl ConfidenceBand {
    /// Machine-readable tag used in JSON and text output (`strong`/`weak`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Weak => "weak",
        }
    }

    /// Classifies `score` against [`SEMANTIC_CONFIDENT_THRESHOLD`].
    #[must_use]
    pub const fn of_score(score: f32) -> Self {
        if score >= SEMANTIC_CONFIDENT_THRESHOLD {
            Self::Strong
        } else {
            Self::Weak
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confident_band_starts_at_confident_threshold() {
        assert_eq!(
            ConfidenceBand::of_score(SEMANTIC_CONFIDENT_THRESHOLD),
            ConfidenceBand::Strong
        );
        assert_eq!(
            ConfidenceBand::of_score(SEMANTIC_CONFIDENT_THRESHOLD - 0.000_1),
            ConfidenceBand::Weak
        );
        assert_eq!(ConfidenceBand::of_score(1.0), ConfidenceBand::Strong);
        assert_eq!(ConfidenceBand::of_score(0.0), ConfidenceBand::Weak);
    }

    #[test]
    fn verdict_bands_match_documented_thresholds() {
        assert_eq!(
            SemanticConfidence::of_best(SEMANTIC_CONFIDENT_THRESHOLD),
            SemanticConfidence::Confident
        );
        assert_eq!(
            SemanticConfidence::of_best(SEMANTIC_CONFIDENT_THRESHOLD - 0.000_1),
            SemanticConfidence::Weak
        );
        assert_eq!(
            SemanticConfidence::of_best(SEMANTIC_WEAK_THRESHOLD),
            SemanticConfidence::Weak
        );
        assert_eq!(
            SemanticConfidence::of_best(SEMANTIC_WEAK_THRESHOLD - 0.000_1),
            SemanticConfidence::Abstain
        );
        assert_eq!(
            SemanticConfidence::of_best(1.0),
            SemanticConfidence::Confident
        );
        assert_eq!(
            SemanticConfidence::of_best(0.0),
            SemanticConfidence::Abstain
        );
    }

    #[test]
    fn verdict_tags_are_stable() {
        assert_eq!(SemanticConfidence::Confident.as_str(), "confident");
        assert_eq!(SemanticConfidence::Weak.as_str(), "weak");
        assert_eq!(SemanticConfidence::Abstain.as_str(), "abstain");
        assert_eq!(ConfidenceBand::Strong.as_str(), "strong");
        assert_eq!(ConfidenceBand::Weak.as_str(), "weak");
    }

    #[test]
    fn verdict_serializes_with_stable_tags_and_field_order() {
        let verdict = SemanticConfidenceVerdict::new(SemanticConfidence::Weak, 0.5213, 42);
        let json = serde_json::to_string(&verdict).expect("serialize verdict");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse verdict");
        assert_eq!(parsed["verdict"], "weak");
        // f32 thresholds are not exactly representable: compare with epsilon.
        assert!(
            (parsed["confident_threshold"].as_f64().unwrap_or(-1.0) - 0.39).abs() < 1e-6,
            "confident threshold must round-trip, got {parsed}"
        );
        assert!(
            (parsed["weak_threshold"].as_f64().unwrap_or(-1.0) - 0.34).abs() < 1e-6,
            "weak threshold must round-trip, got {parsed}"
        );
        assert_eq!(parsed["total_candidates"], 42);
        assert_eq!(parsed["selection_basis"], SEMANTIC_SELECTION_BASIS);
        // Field order is the declaration order — the byte-parity contract.
        // Assert on the raw string: without serde_json's `preserve_order`
        // feature, `Value`'s object map is a BTreeMap, so parsed key order
        // is alphabetical regardless of serialization order.
        let fields = [
            "verdict",
            "best_score",
            "total_candidates",
            "confident_threshold",
            "weak_threshold",
            "selection_basis",
        ];
        let mut cursor = 0;
        for field in fields {
            let needle = format!("\"{field}\":");
            let at = json[cursor..].find(needle.as_str()).unwrap_or_else(|| {
                panic!("field {field} must serialize in declaration order, got {json}")
            });
            cursor += at + needle.len();
        }
    }

    #[test]
    fn verdict_round_trips_through_json_value() {
        // The daemon CLI path deserializes the verb's verdict Value and
        // re-serializes it through this struct for canonical field order.
        let verdict = SemanticConfidenceVerdict::new(SemanticConfidence::Abstain, 0.4123, 7);
        let value = serde_json::to_value(&verdict).expect("to value");
        let back: SemanticConfidenceVerdict = serde_json::from_value(value).expect("from value");
        assert_eq!(back, verdict);
        assert_eq!(back.selection_basis, SEMANTIC_SELECTION_BASIS);
    }

    #[test]
    fn verdict_text_is_a_single_line_starting_with_confidence() {
        let verdict = SemanticConfidenceVerdict::new(SemanticConfidence::Confident, 0.8234, 10);
        let text = verdict.as_text();
        assert!(text.starts_with("confidence: confident "), "got {text}");
        assert!(!text.contains('\n'), "text verdict must be one line");
    }
}
