#![allow(missing_docs)]

//! TDD tests for issues #263 and #221: calibrated confidence thresholds and
//! the answer-level confidence verdict for `eg query semantic`.
//!
//! The fixture pins the calibrated thresholds and measured cases (a confident
//! in-corpus match, a weak near-miss, and a below-floor abstention); the
//! tests assert the code agrees with the fixture, so the thresholds cannot
//! drift silently. The verdict object is what both transports stamp on every
//! non-empty answer (`{"confidence": {...}}` in JSON, `confidence: <verdict>`
//! in text).

use std::path::PathBuf;

use aletheia_egregore::semantic_confidence::{
    ConfidenceBand, SEMANTIC_CONFIDENT_THRESHOLD, SEMANTIC_SELECTION_BASIS,
    SEMANTIC_WEAK_THRESHOLD, SemanticConfidence, SemanticConfidenceVerdict,
};
use serde::Deserialize;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/semantic_confidence_fixture.json")
}

#[derive(Deserialize)]
struct FixtureHit {
    record_id: String,
    score: f32,
}

#[derive(Deserialize)]
struct FixtureCase {
    // Note: the fixture also records the measured `query` text for
    // documentation; it is intentionally not deserialized — the verdict is a
    // pure function of the score distribution, not of the query string.
    ranked_hits: Vec<FixtureHit>,
    expected_verdict: String,
}

#[derive(Deserialize)]
struct Fixture {
    confident_threshold: f32,
    weak_threshold: f32,
    selection_basis: String,
    confident_case: FixtureCase,
    weak_case: FixtureCase,
    abstain_case: FixtureCase,
}

fn load_fixture() -> Fixture {
    let text = std::fs::read_to_string(fixture_path()).expect("read confidence fixture");
    serde_json::from_str(&text).expect("confidence fixture must be valid JSON")
}

fn best_score(case: &FixtureCase) -> f32 {
    case.ranked_hits
        .iter()
        .map(|h| h.score)
        .fold(f32::NEG_INFINITY, f32::max)
}

// ---------------------------------------------------------------------------
// Threshold pinning: the code constants must equal the documented fixture
// values.
// ---------------------------------------------------------------------------

#[test]
fn thresholds_match_documented_fixture_values() {
    let fixture = load_fixture();
    // Float equality via epsilon: the fixture pins the exact calibrated
    // values, and the constants must match them bit-for-bit in practice.
    assert!(
        (SEMANTIC_CONFIDENT_THRESHOLD - fixture.confident_threshold).abs() < f32::EPSILON,
        "SEMANTIC_CONFIDENT_THRESHOLD ({}) must equal the calibrated value pinned in corpus/semantic_confidence_fixture.json ({})",
        SEMANTIC_CONFIDENT_THRESHOLD,
        fixture.confident_threshold,
    );
    assert!(
        (SEMANTIC_WEAK_THRESHOLD - fixture.weak_threshold).abs() < f32::EPSILON,
        "SEMANTIC_WEAK_THRESHOLD ({}) must equal the documented value pinned in corpus/semantic_confidence_fixture.json ({})",
        SEMANTIC_WEAK_THRESHOLD,
        fixture.weak_threshold,
    );
    // Compile-time invariant (clippy::assertions_on_constants): both are
    // constants, so a runtime assert! would have a constant value.
    const {
        assert!(
            SEMANTIC_WEAK_THRESHOLD < SEMANTIC_CONFIDENT_THRESHOLD,
            "the weak floor must sit strictly below the confident bar"
        );
    }
}

#[test]
fn selection_basis_matches_documented_fixture_value() {
    let fixture = load_fixture();
    assert_eq!(
        SEMANTIC_SELECTION_BASIS, fixture.selection_basis,
        "selection basis tag must match the fixture"
    );
}

// ---------------------------------------------------------------------------
// Measured cases: the verdict derived from each case's best score must match
// the fixture's expectation.
// ---------------------------------------------------------------------------

#[test]
fn confident_case_yields_confident_verdict() {
    let fixture = load_fixture();
    let case = &fixture.confident_case;
    assert_eq!(case.expected_verdict, "confident");
    let best = best_score(case);
    assert_eq!(
        SemanticConfidence::of_best(best),
        SemanticConfidence::Confident,
        "confident case best score {best} must reach {SEMANTIC_CONFIDENT_THRESHOLD}"
    );
}

#[test]
fn confident_case_rows_all_classify_strong() {
    let fixture = load_fixture();
    for hit in &fixture.confident_case.ranked_hits {
        assert_eq!(
            ConfidenceBand::of_score(hit.score),
            ConfidenceBand::Strong,
            "confident-case row {} score {} must classify strong",
            hit.record_id,
            hit.score
        );
    }
}

#[test]
fn weak_case_yields_weak_verdict() {
    let fixture = load_fixture();
    let case = &fixture.weak_case;
    assert_eq!(case.expected_verdict, "weak");
    let best = best_score(case);
    assert_eq!(
        SemanticConfidence::of_best(best),
        SemanticConfidence::Weak,
        "weak case best score {best} must sit in [{SEMANTIC_WEAK_THRESHOLD}, {SEMANTIC_CONFIDENT_THRESHOLD})"
    );
}

#[test]
fn abstain_case_yields_abstain_verdict() {
    let fixture = load_fixture();
    let case = &fixture.abstain_case;
    assert_eq!(case.expected_verdict, "abstain");
    let best = best_score(case);
    assert_eq!(
        SemanticConfidence::of_best(best),
        SemanticConfidence::Abstain,
        "abstain case best score {best} must sit below {SEMANTIC_WEAK_THRESHOLD}"
    );
}

// ---------------------------------------------------------------------------
// Verdict object: shape, required fields, byte-identical across repeats.
// ---------------------------------------------------------------------------

#[test]
fn verdict_object_carries_required_fields() {
    let fixture = load_fixture();
    let case = &fixture.abstain_case;
    let best = best_score(case);
    let verdict = SemanticConfidenceVerdict::new(
        SemanticConfidence::of_best(best),
        best,
        case.ranked_hits.len(),
    );
    assert_eq!(verdict.verdict, SemanticConfidence::Abstain);
    let json = serde_json::to_value(&verdict).expect("verdict must serialize");

    assert_eq!(json["verdict"], "abstain", "stable verdict tag");
    assert_eq!(json["best_score"], best);
    assert_eq!(json["total_candidates"], case.ranked_hits.len());
    assert_eq!(json["confident_threshold"], SEMANTIC_CONFIDENT_THRESHOLD);
    assert_eq!(json["weak_threshold"], SEMANTIC_WEAK_THRESHOLD);
    assert_eq!(json["selection_basis"], SEMANTIC_SELECTION_BASIS);
    // The query is deliberately NOT part of the verdict: the verdict is a
    // pure function of the score distribution, which is what makes it
    // deterministic and comparable across queries.
    assert!(
        json.get("query").is_none(),
        "verdict must not carry the query text"
    );
}

#[test]
fn verdict_object_is_byte_identical_across_repeats() {
    let fixture = load_fixture();
    let case = &fixture.weak_case;
    let best = best_score(case);
    let build = || {
        serde_json::to_string(&SemanticConfidenceVerdict::new(
            SemanticConfidence::of_best(best),
            best,
            case.ranked_hits.len(),
        ))
        .expect("serialize")
    };
    let first = build();
    for _ in 0..4 {
        assert_eq!(
            first,
            build(),
            "five repeated derivations must produce byte-identical verdict objects"
        );
    }
}

#[test]
fn verdict_text_line_is_stable() {
    let verdict = SemanticConfidenceVerdict::new(SemanticConfidence::Weak, 0.5213, 9);
    let text = verdict.as_text();
    assert!(
        text.starts_with("confidence: weak "),
        "text verdict must start with the stable tag, got {text}"
    );
    assert!(
        !text.contains('\n'),
        "text verdict must be a single line, got {text}"
    );
}

// ---------------------------------------------------------------------------
// Boundary and tag stability.
// ---------------------------------------------------------------------------

#[test]
fn verdict_boundaries_are_inclusive_at_the_bar() {
    assert_eq!(
        SemanticConfidence::of_best(SEMANTIC_CONFIDENT_THRESHOLD),
        SemanticConfidence::Confident,
        "a score exactly at the confident bar is confident"
    );
    assert_eq!(
        SemanticConfidence::of_best(SEMANTIC_WEAK_THRESHOLD),
        SemanticConfidence::Weak,
        "a score exactly at the weak floor is weak"
    );
    assert_eq!(
        ConfidenceBand::of_score(SEMANTIC_CONFIDENT_THRESHOLD),
        ConfidenceBand::Strong
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
