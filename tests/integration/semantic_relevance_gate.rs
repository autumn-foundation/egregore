#![allow(missing_docs)]

//! Tests for issue #106: falsifiable + gate-able semantic-search relevance.
//!
//! These drive the pure, feature-free relevance harness (hit-rate@k, MRR,
//! recall, floor gate, deterministic report) over committed ranking fixtures
//! so the gate logic is exercised offline without an embedding model or a live
//! store.

use std::path::PathBuf;

use aletheia_egregore::semantic_eval::{
    DEFAULT_MIN_HIT_RATE_AT_5, DEFAULT_MIN_MRR, RELEVANCE_DETERMINISM_TOLERANCE, RelevanceFloors,
    RelevanceRankingFixture, build_relevance_report, capability_unavailable_report,
    compute_relevance_metrics, evaluate_relevance_fixture, relevance_exit_code,
    relevance_metrics_within_tolerance, render_relevance_report_json,
};

const FP_THRESHOLD: f32 = 0.5;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/semantic_relevance")
        .join(name)
}

fn load(name: &str) -> RelevanceRankingFixture {
    RelevanceRankingFixture::from_json_file(&fixture_path(name)).expect("load ranking fixture")
}

// ---------------------------------------------------------------------------
// GREEN target: the default floors are the issue's exact numbers.
// ---------------------------------------------------------------------------

#[test]
fn default_floors_match_issue_106() {
    assert!(
        (DEFAULT_MIN_HIT_RATE_AT_5 - 0.90).abs() < 1e-9,
        "issue #106 primary gate is hit-rate@5 >= 0.90"
    );
    assert!(
        (DEFAULT_MIN_MRR - 0.70).abs() < 1e-9,
        "issue #106 secondary gate is MRR >= 0.70"
    );
    let floors = RelevanceFloors::default();
    assert!((floors.min_hit_rate_at_5 - 0.90).abs() < 1e-9);
    assert!((floors.min_mrr - 0.70).abs() < 1e-9);
}

#[test]
fn determinism_tolerance_is_1e5() {
    assert!(
        (RELEVANCE_DETERMINISM_TOLERANCE - 1e-5).abs() < 1e-12,
        "issue #106 determinism tolerance is 1e-5"
    );
}

// ---------------------------------------------------------------------------
// Metric shape: hit-rate@k is monotonic in k, recall is the full-depth hit rate.
// ---------------------------------------------------------------------------

#[test]
fn passing_fixture_metrics_are_well_formed() {
    let fixture = load("passing_ranking.json");
    let results = evaluate_relevance_fixture(&fixture, FP_THRESHOLD);
    let m = compute_relevance_metrics(&results);

    assert_eq!(m.labeled_count, 10, "10 labeled queries in the fixture");
    assert_eq!(m.ambiguous_count, 2, "2 ambiguous queries in the fixture");

    // hit-rate@k is monotonic non-decreasing in k.
    assert!(m.hit_rate_at_1 <= m.hit_rate_at_5);
    assert!(m.hit_rate_at_5 <= m.hit_rate_at_10);

    // Fixture design: 7 rank-1, 1 rank-2, 1 rank-4, 1 rank-8.
    assert!((m.hit_rate_at_1 - 0.7).abs() < 1e-9, "hit@1 = 7/10");
    assert!((m.hit_rate_at_5 - 0.9).abs() < 1e-9, "hit@5 = 9/10");
    assert!((m.hit_rate_at_10 - 1.0).abs() < 1e-9, "hit@10 = 10/10");
    assert!(
        (m.recall - 1.0).abs() < 1e-9,
        "recall = 10/10 (all found at some depth)"
    );
    // MRR = (7*1 + 1/2 + 1/4 + 1/8) / 10 = 0.7875
    assert!((m.mean_reciprocal_rank - 0.7875).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// GATE: passing fixture clears both floors; degraded fixture breaches both.
// ---------------------------------------------------------------------------

#[test]
fn passing_fixture_passes_the_gate() {
    let fixture = load("passing_ranking.json");
    let results = evaluate_relevance_fixture(&fixture, FP_THRESHOLD);
    let report = build_relevance_report(
        &results,
        RelevanceFloors::default(),
        fixture.source_snapshot,
    );

    assert!(
        report.ok,
        "passing fixture must clear the gate; report: {report:?}"
    );
    assert_eq!(report.capability, "available");
    assert!(report.breaches.is_empty(), "no floors breached");
    assert_eq!(relevance_exit_code(&report), 0, "pass exits 0");
}

#[test]
fn degraded_fixture_fails_the_gate_and_names_breached_metrics() {
    let fixture = load("degraded_ranking.json");
    let results = evaluate_relevance_fixture(&fixture, FP_THRESHOLD);
    let report = build_relevance_report(
        &results,
        RelevanceFloors::default(),
        fixture.source_snapshot,
    );

    assert!(!report.ok, "degraded rankings must fail the gate");
    assert_eq!(relevance_exit_code(&report), 1, "gate failure exits 1");

    // Both floors are breached; each breach must name the metric AND its observed value.
    let hit5 = report
        .breaches
        .iter()
        .find(|b| b.metric == "hit_rate_at_5")
        .expect("hit_rate_at_5 must be reported as breached");
    assert!(
        (hit5.observed - 0.2).abs() < 1e-9,
        "observed hit@5 = 2/10 must be reported; got {}",
        hit5.observed
    );
    assert!((hit5.floor - 0.90).abs() < 1e-9);

    let mrr = report
        .breaches
        .iter()
        .find(|b| b.metric == "mrr")
        .expect("mrr must be reported as breached");
    assert!(
        (mrr.observed - 0.2).abs() < 1e-9,
        "observed MRR = 0.2 must be reported; got {}",
        mrr.observed
    );
    assert!((mrr.floor - 0.70).abs() < 1e-9);

    // The report is still produced (printed on gate-fail): metrics present + misses listed.
    assert!(
        report.metrics.is_some(),
        "report metrics present on failure"
    );
    assert!(
        !report.misses.is_empty(),
        "missed queries must be reported for debugging"
    );
}

// ---------------------------------------------------------------------------
// Determinism: two renders of the same fixture are byte-identical.
// ---------------------------------------------------------------------------

#[test]
fn report_render_is_byte_identical_across_runs() {
    let render_once = || {
        let fixture = load("passing_ranking.json");
        let results = evaluate_relevance_fixture(&fixture, FP_THRESHOLD);
        let report = build_relevance_report(
            &results,
            RelevanceFloors::default(),
            fixture.source_snapshot,
        );
        render_relevance_report_json(&report)
    };
    let first = render_once();
    let second = render_once();
    assert_eq!(
        first, second,
        "report JSON must be byte-identical across runs"
    );
}

#[test]
fn metrics_within_tolerance_helper() {
    let fixture = load("passing_ranking.json");
    let a = compute_relevance_metrics(&evaluate_relevance_fixture(&fixture, FP_THRESHOLD));
    let b = compute_relevance_metrics(&evaluate_relevance_fixture(&fixture, FP_THRESHOLD));
    assert!(
        relevance_metrics_within_tolerance(&a, &b, RELEVANCE_DETERMINISM_TOLERANCE),
        "identical inputs must agree within tolerance"
    );

    let mut c = b;
    c.mean_reciprocal_rank += 1e-2; // beyond 1e-5
    assert!(
        !relevance_metrics_within_tolerance(&a, &c, RELEVANCE_DETERMINISM_TOLERANCE),
        "a 1e-2 divergence must fail the 1e-5 tolerance"
    );
}

// ---------------------------------------------------------------------------
// Honest degradation: embeddings-off never silently passes.
// ---------------------------------------------------------------------------

#[test]
fn capability_unavailable_never_passes_and_exits_2() {
    let report =
        capability_unavailable_report(RelevanceFloors::default(), "requires_embeddings_feature");
    assert!(
        !report.ok,
        "capability-unavailable must never report ok:true"
    );
    assert_eq!(report.capability, "requires_embeddings_feature");
    assert!(report.metrics.is_none(), "no metrics without a store");
    assert_eq!(
        relevance_exit_code(&report),
        2,
        "capability-unavailable exits 2 (usage/capability), never 0"
    );
}

// ---------------------------------------------------------------------------
// Corpus pin (AC): the committed corpus pins its source snapshot.
// ---------------------------------------------------------------------------

#[test]
fn committed_corpus_pins_a_source_snapshot() {
    use aletheia_egregore::semantic_eval::SemanticRelevanceCorpus;
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/semantic_relevance_corpus.json");
    let corpus = SemanticRelevanceCorpus::from_json_file(&path).expect("load corpus");
    let snapshot = corpus
        .source_snapshot
        .expect("corpus must pin a source snapshot (commit) for reproducibility");
    assert!(
        !snapshot.commit.is_empty(),
        "pinned snapshot commit must be non-empty"
    );
}

// ---------------------------------------------------------------------------
// Docs (AC): the corpus-format + add-a-query + floors doc exists.
// ---------------------------------------------------------------------------

#[test]
fn relevance_eval_documentation_exists() {
    let doc_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/cli/semantic-relevance-eval.md");
    assert!(
        doc_path.exists(),
        "docs/cli/semantic-relevance-eval.md must exist"
    );
    let text = std::fs::read_to_string(&doc_path).expect("read doc");
    for needle in [
        "hit-rate@5",
        "MRR",
        "recall",
        "1e-5",
        "source_snapshot",
        "add a query",
    ] {
        assert!(
            text.to_lowercase().contains(&needle.to_lowercase()),
            "doc must document '{needle}'"
        );
    }
}
