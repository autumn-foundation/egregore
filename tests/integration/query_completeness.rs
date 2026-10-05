//! Regression tests for issue #121: every `eg query` JSON answer carries a
//! stable completeness signal (`result_complete`, plus `total_matches` and
//! `applied_limit` when a cap or selection narrowed the answer).
//!
//! The tests below are the acceptance criteria made executable:
//! - AC2: `query drift --limit N` with M > N reports exactly N rows with
//!   `result_complete: false`, `total_matches: M`, `applied_limit: N`.
//! - AC3: M <= N reports `result_complete: true` and omits the totals.
//! - AC5: `query symbol --at` picking a single winner from several candidates
//!   reports `result_complete: false` with `total_matches` > 1.
//! - AC6: exhaustive answers (`query file`, uncapped `query symbol`) report
//!   `result_complete: true`.
//! - AC7: the exit-2 no-match path emits no JSON answer at all, so it cannot
//!   contradict the signal (a no-match is definitionally complete with zero
//!   total matches).
//!
//! AC4 (`query semantic`) cannot run end-to-end in this harness: the query
//! embedder needs a model that cannot be loaded offline. Its stamping logic
//! is covered by unit tests on the row constructor instead.

#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    ir::{Graph, stable_id},
};
use assert_cmd::Command;
use predicates::prelude::*;

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn drift_node(tag: &str, score: f64, commit: &str, valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "SemanticDrift", tag]);
    let target_id = stable_id(&["node", "Symbol", "src/lib.rs", "scan_repository"]);
    GraphRecord::node(
        id,
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some("scan_repository".to_owned()),
        format!("{tag} drift"),
    )
    .with_temporal(TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: vec![],
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: None,
    })
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "test-model-v1".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: target_id.clone(),
        prior_record_id: target_id,
        before_git_commit: "aaaaaaaa".to_owned(),
        after_git_commit: commit.to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: valid_time.to_owned(),
        metric_kind: MetricKind::CosineDistance,
        // Distinct scores so the --limit cut is deterministic.
        score,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    })
}

/// Three drift records (scores 0.25 / 0.50 / 0.90) so a `--limit` below 3
/// truncates the answer.
fn fixture_graph_with_three_drifts() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("drift.jsonl");

    let mut graph = Graph::new();
    graph.push(drift_node(
        "small",
        0.25,
        "bbbbbbbb",
        "2026-01-02T00:00:00Z",
    ));
    graph.push(drift_node(
        "medium",
        0.50,
        "cccccccc",
        "2026-01-03T00:00:00Z",
    ));
    graph.push(drift_node(
        "large",
        0.90,
        "dddddddd",
        "2026-01-04T00:00:00Z",
    ));
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

fn temporal_symbol(tag: &str, path: &str, commit: &str, valid_time: &str) -> GraphRecord {
    GraphRecord::symbol(
        stable_id(&["node", "Symbol", path, "scan_repository", tag]),
        "fn",
        path.to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        format!("Rust function scan_repository ({tag})"),
    )
    .with_temporal(TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: vec![],
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: None,
    })
}

/// Two same-named symbols at commit `bbbbbbbb…` (the `--at` selection pool)
/// plus one at `aaaaaaaa…` (the single-candidate control).
fn fixture_graph_with_duplicate_symbols_at_commit() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history.jsonl");

    let mut graph = Graph::new();
    graph.push(temporal_symbol(
        "aaa",
        "src/lib.rs",
        "aaaaaaaaaaaaaaaa",
        "2026-01-01T00:00:00Z",
    ));
    graph.push(temporal_symbol(
        "bbb-one",
        "src/lib.rs",
        "bbbbbbbbbbbbbbbb",
        "2026-01-02T00:00:00Z",
    ));
    graph.push(temporal_symbol(
        "bbb-two",
        "src/other.rs",
        "bbbbbbbbbbbbbbbb",
        "2026-01-02T00:00:00Z",
    ));
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

/// A file defining two symbols, wired through `DEFINES` edges.
fn fixture_graph_with_file_defines() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let file_node = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 50),
        "lib.rs".to_owned(),
        "rust",
        "Source file src/lib.rs".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(file_node);
    for (name, line) in [("scan_repository", 10), ("helper_fn", 30)] {
        let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", name]);
        graph.push(GraphRecord::symbol(
            sym_id.clone(),
            "fn",
            "src/lib.rs".to_owned(),
            span(line, line + 10),
            name.to_owned(),
            format!("Rust function {name}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            sym_id,
            Some("1.0".to_owned()),
            "file defines symbol".to_owned(),
        ));
    }
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

fn parse_rows(stdout: &[u8]) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8(stdout.to_vec()).expect("utf8");
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON row"))
        .collect()
}

// ---------------------------------------------------------------------------
// AC2: drift --limit truncates and says so
// ---------------------------------------------------------------------------

#[test]
fn drift_limit_truncates_with_completeness_signal() {
    let (_temp, graph) = fixture_graph_with_three_drifts();

    let output = egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .args(["--limit", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_rows(&output);
    assert_eq!(rows.len(), 1, "exactly one row under --limit 1");
    let row = &rows[0];
    // The top-ranked drift (score 0.90) must win the cut.
    assert!(
        row["score"].as_f64().expect("score") > 0.89,
        "the highest-scoring drift must be kept, got {row}"
    );
    assert_eq!(
        row["result_complete"], false,
        "a narrowed answer must report result_complete: false, got {row}"
    );
    assert_eq!(
        row["total_matches"], 3,
        "total_matches must count the pre-limit pool, got {row}"
    );
    assert_eq!(
        row["applied_limit"], 1,
        "applied_limit must echo the applied cap, got {row}"
    );
}

// ---------------------------------------------------------------------------
// AC3: drift pool within the limit is complete and omits the totals
// ---------------------------------------------------------------------------

#[test]
fn drift_complete_when_pool_within_limit() {
    let (_temp, graph) = fixture_graph_with_three_drifts();

    let output = egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .args(["--limit", "10"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_rows(&output);
    assert_eq!(rows.len(), 3, "all three drifts fit under --limit 10");
    for row in &rows {
        assert_eq!(
            row["result_complete"], true,
            "an unnarrowed answer must report result_complete: true, got {row}"
        );
        assert!(
            row.get("total_matches").is_none(),
            "total_matches must be omitted when nothing was narrowed, got {row}"
        );
        assert!(
            row.get("applied_limit").is_none(),
            "applied_limit must be omitted when nothing was narrowed, got {row}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC6: default drift limit over a small pool is complete
// ---------------------------------------------------------------------------

#[test]
fn drift_default_limit_is_complete_for_small_pool() {
    let (_temp, graph) = fixture_graph_with_three_drifts();

    let output = egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_rows(&output);
    assert_eq!(rows.len(), 3, "default limit (10) keeps all three drifts");
    for row in &rows {
        assert_eq!(row["result_complete"], true, "got {row}");
        assert!(row.get("total_matches").is_none(), "got {row}");
        assert!(row.get("applied_limit").is_none(), "got {row}");
    }
}

// ---------------------------------------------------------------------------
// AC5: symbol --at picks a single winner and reports the selection
// ---------------------------------------------------------------------------

#[test]
fn symbol_at_single_winner_reports_truncation() {
    let (_temp, graph) = fixture_graph_with_duplicate_symbols_at_commit();

    let output = egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .args(["--at", "bbbbbbbb"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_rows(&output);
    assert_eq!(rows.len(), 1, "--at keeps a single winner");
    let row = &rows[0];
    assert_eq!(row["git_commit"], "bbbbbbbbbbbbbbbb");
    assert_eq!(
        row["result_complete"], false,
        "a single-winner selection from 2 candidates must report \
         result_complete: false, got {row}"
    );
    assert_eq!(row["total_matches"], 2, "got {row}");
    assert_eq!(row["applied_limit"], 1, "got {row}");
}

#[test]
fn symbol_at_single_candidate_is_complete() {
    let (_temp, graph) = fixture_graph_with_duplicate_symbols_at_commit();

    let output = egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .args(["--at", "aaaaaaaa"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_rows(&output);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["git_commit"], "aaaaaaaaaaaaaaaa");
    assert_eq!(
        row["result_complete"], true,
        "a lone candidate is not a narrowed answer, got {row}"
    );
    assert!(row.get("total_matches").is_none(), "got {row}");
    assert!(row.get("applied_limit").is_none(), "got {row}");
}

// ---------------------------------------------------------------------------
// AC6: exhaustive answers — query file, uncapped query symbol
// ---------------------------------------------------------------------------

#[test]
fn file_defines_listing_is_exhaustive() {
    let (_temp, graph) = fixture_graph_with_file_defines();

    let output = egregore()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_rows(&output);
    assert_eq!(rows.len(), 2, "both defined symbols are listed");
    for row in &rows {
        assert_eq!(
            row["result_complete"], true,
            "the DEFINES listing is exhaustive, got {row}"
        );
        assert!(row.get("total_matches").is_none(), "got {row}");
        assert!(row.get("applied_limit").is_none(), "got {row}");
    }
}

#[test]
fn symbol_lookup_is_exhaustive() {
    let (_temp, graph) = fixture_graph_with_duplicate_symbols_at_commit();

    let output = egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_rows(&output);
    assert_eq!(rows.len(), 3, "all three same-named symbols are listed");
    for row in &rows {
        assert_eq!(
            row["result_complete"], true,
            "an uncapped name lookup is exhaustive, got {row}"
        );
        assert!(row.get("total_matches").is_none(), "got {row}");
        assert!(row.get("applied_limit").is_none(), "got {row}");
    }
}

// ---------------------------------------------------------------------------
// AC7: exit-2 no-match emits no JSON answer to contradict the signal
// ---------------------------------------------------------------------------

#[test]
fn no_match_exits_2_with_empty_stdout() {
    let (_temp, graph) = fixture_graph_with_file_defines();

    // Symbol lane: no such name.
    egregore()
        .args(["query", "symbol", "nonexistent_symbol", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));

    // Drift lane: a graph with no SemanticDrift nodes.
    egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));

    // File lane: no such path.
    egregore()
        .args(["query", "file", "src/missing.rs", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));
}
