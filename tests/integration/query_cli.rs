#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixture_graph_with_symbols() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "scan_repository"]);
    let _edge_id = stable_id(&["edge", "DEFINES", &file_id, &sym_id]);

    let file_node = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 50),
        "lib.rs".to_owned(),
        "rust",
        "Source file src/lib.rs".to_owned(),
    );
    let sym_node = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository".to_owned(),
    );
    let edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        sym_id,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(file_node);
    graph.push(sym_node);
    graph.push(edge);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn query_semantic_missing_data_dir_exits_1_without_creating_store() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("semantic-typo-store");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "semantic", "anything", "--data-dir"])
        .arg(&missing)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("embedded store not found")
                .and(predicate::str::contains("ingest")),
        );

    assert!(
        !missing.exists(),
        "semantic query must not create a missing --data-dir"
    );
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[test]
fn query_semantic_context_missing_data_dir_exits_1_without_creating_store() {
    // Mirrors the `query semantic` contract: a missing store is reported before
    // the embedding model is loaded, and the read-only workflow never creates a
    // store as a side effect of a typo'd path (AC9 read-only).
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("semantic-context-typo-store");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "semantic-context", "anything", "--data-dir"])
        .arg(&missing)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("embedded store not found")
                .and(predicate::str::contains("ingest")),
        );

    assert!(
        !missing.exists(),
        "semantic-context query must not create a missing --data-dir"
    );
}

fn fixture_graph_with_temporal_symbols() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history.jsonl");

    let sym_id_a = stable_id(&["node", "Symbol", "src/lib.rs", "scan_repository", "aaa"]);
    let sym_id_b = stable_id(&["node", "Symbol", "src/lib.rs", "scan_repository", "bbb"]);

    let sym_a = GraphRecord::symbol(
        sym_id_a,
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit aaa".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaaaaaaaaaaaaaa".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let sym_b = GraphRecord::symbol(
        sym_id_b,
        "fn",
        "src/lib.rs".to_owned(),
        span(12, 22),
        "scan_repository".to_owned(),
        "Rust function scan_repository at commit bbb".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "bbbbbbbbbbbbbbbb".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let mut graph = Graph::new();
    graph.push(sym_a);
    graph.push(sym_b);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

/// Fixture for Fix #1: two different symbols at commits sharing a prefix.
/// `scan_repository` at `aaaa0000...`, `other_fn` at `aaaa1111...`
fn fixture_graph_with_cross_symbol_commits() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("cross.jsonl");

    let sym_a = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "scan_repository", "aaaa0"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "scan_repository at aaaa0000".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaa000000000000".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let sym_b = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "other_fn", "aaaa1"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(30, 40),
        "other_fn".to_owned(),
        "other_fn at aaaa1111".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaa111111111111".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let mut graph = Graph::new();
    graph.push(sym_a);
    graph.push(sym_b);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

/// Fixture for Fix #3: nested symbol (file→module→symbol) with no direct file→symbol edge.
fn fixture_graph_with_nested_symbol() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("nested.jsonl");

    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let mod_id = stable_id(&["node", "Module", "src/lib.rs", "MyModule"]);
    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "nested_fn"]);

    let file_node = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 100),
        "lib.rs".to_owned(),
        "rust",
        "Source file".to_owned(),
    );
    let mod_node = GraphRecord::syntax_node(
        mod_id.clone(),
        NodeKind::Module,
        "src/lib.rs".to_owned(),
        span(5, 80),
        "MyModule".to_owned(),
        "rust",
        "Module MyModule".to_owned(),
    );
    let sym_node = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "nested_fn".to_owned(),
        "nested function".to_owned(),
    );
    // file→module DEFINES edge (but NOT file→symbol)
    let edge_file_mod = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        mod_id.clone(),
        Some("1.0".to_owned()),
        "file defines module".to_owned(),
    );
    // module→symbol DEFINES edge
    let edge_mod_sym = GraphRecord::edge(
        EdgeLabel::Defines,
        mod_id,
        sym_id,
        Some("1.0".to_owned()),
        "module defines symbol".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(file_node);
    graph.push(mod_node);
    graph.push(sym_node);
    graph.push(edge_file_mod);
    graph.push(edge_mod_sym);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[allow(clippy::too_many_lines)]
fn fixture_graph_with_drift() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("drift.jsonl");

    let target_id = stable_id(&["node", "Symbol", "src/lib.rs", "scan_repository"]);

    let drift_small_id = stable_id(&["node", "SemanticDrift", "small"]);
    let drift_large_id = stable_id(&["node", "SemanticDrift", "large"]);

    let drift_small = GraphRecord::node(
        drift_small_id.clone(),
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some("scan_repository".to_owned()),
        "small drift".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "bbbbbbbb".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
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
        prior_record_id: target_id.clone(),
        before_git_commit: "aaaaaaaa".to_owned(),
        after_git_commit: "bbbbbbbb".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.25,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });

    let drift_large = GraphRecord::node(
        drift_large_id.clone(),
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some("scan_repository".to_owned()),
        "large drift".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "cccccccc".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
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
        prior_record_id: target_id.clone(),
        before_git_commit: "bbbbbbbb".to_owned(),
        after_git_commit: "cccccccc".to_owned(),
        before_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-03T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.9,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });

    let target_sym = GraphRecord::symbol(
        target_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "scan_repository".to_owned(),
        "target symbol".to_owned(),
    );

    let edge_small = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_small_id,
        target_id.clone(),
        Some("1.0".to_owned()),
        "drifts from edge".to_owned(),
    );
    let edge_large = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_large_id,
        target_id,
        Some("1.0".to_owned()),
        "drifts from edge".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(drift_small);
    graph.push(drift_large);
    graph.push(target_sym);
    graph.push(edge_small);
    graph.push(edge_large);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

// ---------------------------------------------------------------------------
// --data-dir integration tests (require embedded-aletheiadb feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_to_data_dir(graph: &std::path::Path, data_dir: &std::path::Path) {
    egregore()
        .arg("ingest")
        .arg(graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbol_data_dir_returns_jsonl_for_matching_symbol() {
    let (_temp_graph, graph) = fixture_graph_with_symbols();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    ingest_to_data_dir(&graph, &data_dir);

    let output = egregore()
        .args(["query", "symbol", "scan_repository", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["name"], "scan_repository");
    assert_eq!(parsed["kind"], "Symbol");
    assert!(parsed["record_id"].is_string());
    assert!(parsed["repo_relative_path"].is_string());
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbol_data_dir_exits_2_when_no_match() {
    let (_temp_graph, graph) = fixture_graph_with_symbols();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    ingest_to_data_dir(&graph, &data_dir);

    egregore()
        .args(["query", "symbol", "nonexistent_xyz", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_file_data_dir_returns_symbols_defined_in_file() {
    let (_temp_graph, graph) = fixture_graph_with_symbols();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    ingest_to_data_dir(&graph, &data_dir);

    let output = egregore()
        .args(["query", "file", "src/lib.rs", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["name"], "scan_repository");
    assert_eq!(parsed["kind"], "Symbol");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_drift_data_dir_returns_drift_ranked_by_score() {
    let (_temp_graph, graph) = fixture_graph_with_drift();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    ingest_to_data_dir(&graph, &data_dir);

    let output = egregore()
        .args(["query", "drift", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let first: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(first["score"], 0.9, "largest drift should be first");
    assert_eq!(first["embedding_model_name"], "test-model-v1");
    assert!(first["record_id"].is_string());
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbol_graph_and_data_dir_together_is_an_error() {
    let (_temp_graph, graph) = fixture_graph_with_symbols();
    let temp_db = tempfile::tempdir().expect("temp dir");

    egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .args(["--data-dir"])
        .arg(temp_db.path())
        .assert()
        .failure()
        .stdout(predicate::str::is_empty());
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbol_neither_graph_nor_data_dir_is_an_error() {
    egregore()
        .args(["query", "symbol", "scan_repository"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty());
}

// ---------------------------------------------------------------------------
// query symbol — happy path
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_prints_jsonl_for_matching_symbol() {
    let (_temp, graph) = fixture_graph_with_symbols();

    let output = egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    assert!(!stdout.trim().is_empty(), "should have output lines");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line"))
            .expect("valid JSON on first line");
    assert_eq!(parsed["name"], "scan_repository");
    assert_eq!(parsed["kind"], "Symbol");
    // Corpus disclosure (issue #427): `query symbol` has no summary envelope, so
    // each row discloses its corpus. A plain scan graph discloses single_snapshot.
    assert_eq!(parsed["corpus_mode"], "single_snapshot");
    assert_eq!(parsed["corpus_mode_source"], "default");
    assert!(
        parsed["corpus_disclaimer"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );
    assert!(
        parsed["record_id"].is_string(),
        "record_id should be string"
    );
    assert!(
        parsed["repo_relative_path"].is_string(),
        "repo_relative_path should be string"
    );
    assert!(parsed["span"].is_object(), "span should be object");
}

// ---------------------------------------------------------------------------
// query symbol — no match exits 2
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_exits_2_when_no_match() {
    let (_temp, graph) = fixture_graph_with_symbols();

    egregore()
        .args(["query", "symbol", "nonexistent_symbol", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));
}

// ---------------------------------------------------------------------------
// query symbol --at — happy path
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_at_commit_returns_single_record() {
    let (_temp, graph) = fixture_graph_with_temporal_symbols();

    let output = egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .args(["--at", "bbbbbbbb"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "should return exactly one record");
    let parsed: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
    assert_eq!(parsed["git_commit"], "bbbbbbbbbbbbbbbb");
    assert_eq!(parsed["name"], "scan_repository");
}

// ---------------------------------------------------------------------------
// query symbol --at — ambiguous prefix exits non-zero
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_at_ambiguous_prefix_exits_nonzero() {
    let (_temp, graph) = fixture_graph_with_temporal_symbols();

    egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .args(["--at", ""])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous commit prefix"));
}

// ---------------------------------------------------------------------------
// query file — happy path
// ---------------------------------------------------------------------------

#[test]
fn query_file_prints_symbols_defined_in_file() {
    let (_temp, graph) = fixture_graph_with_symbols();

    let output = egregore()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    assert!(!stdout.trim().is_empty(), "should have output lines");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["name"], "scan_repository");
    assert_eq!(parsed["kind"], "Symbol");
}

// ---------------------------------------------------------------------------
// query file — no match exits 2
// ---------------------------------------------------------------------------

#[test]
fn query_file_exits_2_when_no_file_node_found() {
    let (_temp, graph) = fixture_graph_with_symbols();

    egregore()
        .args(["query", "file", "src/nonexistent.rs", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));
}

// ---------------------------------------------------------------------------
// query drift — happy path
// ---------------------------------------------------------------------------

#[test]
fn query_drift_prints_jsonl_ranked_by_score_descending() {
    let (_temp, graph) = fixture_graph_with_drift();

    let output = egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "should have drift output");
    let first: serde_json::Value =
        serde_json::from_str(lines[0]).expect("valid JSON on first line");
    assert_eq!(first["score"], 0.9, "largest drift should be first");
    assert!(first["record_id"].is_string(), "record_id should be string");
    assert_eq!(first["embedding_model_name"], "test-model-v1");
    assert_eq!(first["before_commit"], "bbbbbbbb");
    assert_eq!(first["after_commit"], "cccccccc");
    // target info resolved from DriftsFrom edge
    assert_eq!(first["name"], "scan_repository");
    assert_eq!(first["repo_relative_path"], "src/lib.rs");
}

// ---------------------------------------------------------------------------
// query drift — --limit narrows output
// ---------------------------------------------------------------------------

#[test]
fn query_drift_limit_restricts_output_count() {
    let (_temp, graph) = fixture_graph_with_drift();

    let output = egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .args(["--limit", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(count, 1, "should return exactly 1 drift record");
}

// ---------------------------------------------------------------------------
// query drift — no drift nodes exits 2
// ---------------------------------------------------------------------------

#[test]
fn query_drift_exits_2_when_no_drift_nodes() {
    let (_temp, graph) = fixture_graph_with_symbols();

    egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));
}

// ---------------------------------------------------------------------------
// --format text
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_format_text_prints_human_readable_line() {
    let (_temp, graph) = fixture_graph_with_symbols();

    let output = egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .args(["--format", "text"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        stdout.contains("scan_repository"),
        "should contain symbol name"
    );
    assert!(stdout.contains("src/lib.rs"), "should contain file path");
}

#[test]
fn query_drift_format_text_prints_human_readable_line() {
    let (_temp, graph) = fixture_graph_with_drift();

    let output = egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .args(["--format", "text"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    assert!(
        stdout.contains("scan_repository"),
        "should contain symbol name"
    );
    assert!(stdout.contains("0.900000"), "should contain score");
}

// ---------------------------------------------------------------------------
// Invalid input / error handling
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_missing_graph_file_exits_nonzero_with_error_on_stderr() {
    egregore()
        .args([
            "query",
            "symbol",
            "scan_repository",
            "--graph",
            "/nonexistent/path/graph.jsonl",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_match("error|failed").unwrap());
}

#[test]
fn query_symbol_malformed_jsonl_exits_nonzero_with_error_on_stderr() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("bad.jsonl");
    fs::write(&path, "this is not json\n").expect("write bad file");

    egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&path)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_match("error|failed|parse").unwrap());
}

#[test]
fn query_file_malformed_jsonl_exits_nonzero_with_error_on_stderr() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("bad.jsonl");
    fs::write(&path, "{not valid json}\n").expect("write bad file");

    egregore()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&path)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_match("error|failed|parse").unwrap());
}

#[test]
fn query_drift_malformed_jsonl_exits_nonzero_with_error_on_stderr() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("bad.jsonl");
    fs::write(&path, "{bad json here}\n").expect("write bad file");

    egregore()
        .args(["query", "drift", "--graph"])
        .arg(&path)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_match("error|failed|parse").unwrap());
}

// ---------------------------------------------------------------------------
// eg alias works with query
// ---------------------------------------------------------------------------

#[test]
fn eg_alias_query_symbol_works() {
    let (_temp, graph) = fixture_graph_with_symbols();

    Command::cargo_bin("eg")
        .expect("eg binary")
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Fix #1: ambiguous prefix must be checked across ALL temporal records
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_at_ambiguous_when_prefix_matches_commits_from_other_symbols() {
    let (_temp, graph) = fixture_graph_with_cross_symbol_commits();

    // prefix "aaaa" matches both "aaaa000000000000" (scan_repository)
    // and "aaaa111111111111" (other_fn) → must be flagged as ambiguous
    egregore()
        .args(["query", "symbol", "scan_repository", "--graph"])
        .arg(&graph)
        .args(["--at", "aaaa"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous commit prefix"));
}

// ---------------------------------------------------------------------------
// Fix #3: query file must return nested symbols (not only direct DEFINES targets)
// ---------------------------------------------------------------------------

#[test]
fn query_file_returns_nested_symbols_not_directly_defined_by_file() {
    let (_temp, graph) = fixture_graph_with_nested_symbol();

    let output = egregore()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    assert!(!stdout.trim().is_empty(), "should have output lines");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["name"], "nested_fn");
    assert_eq!(parsed["kind"], "Symbol");
    assert_eq!(parsed["repo_relative_path"], "src/lib.rs");
}

// ---------------------------------------------------------------------------
// Fix A: query drift must resolve target via DRIFTS_FROM edge before fallback
// ---------------------------------------------------------------------------

/// Fixture where the drift node has a *stale* `target_record_id` that is NOT
/// present in the slice, but there IS a `DriftsFrom` edge pointing to the
/// real target symbol.  The resolver must follow the edge.
fn fixture_graph_with_drift_stale_target_id() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("drift_stale.jsonl");

    let real_target_id = stable_id(&["node", "Symbol", "src/lib.rs", "drifted_fn"]);
    let drift_id = stable_id(&["node", "SemanticDrift", "stale_target_drift"]);

    let drift_node = GraphRecord::node(
        drift_id.clone(),
        NodeKind::SemanticDrift,
        None, // no path on the drift node itself
        None,
        None,
        "drift with stale target_record_id".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "bbbbbbbb".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
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
        target_record_id: "stale-id-not-in-slice".to_owned(), // stale / missing
        prior_record_id: "stale-id-not-in-slice".to_owned(),
        before_git_commit: "aaaaaaaa".to_owned(),
        after_git_commit: "bbbbbbbb".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.75,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });

    let target_sym = GraphRecord::symbol(
        real_target_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(5, 15),
        "drifted_fn".to_owned(),
        "the real target symbol".to_owned(),
    );

    // DriftsFrom edge: drift → real target (this is the stable contract)
    let edge = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id,
        real_target_id,
        Some("1.0".to_owned()),
        "drifts from edge".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(drift_node);
    graph.push(target_sym);
    graph.push(edge);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn query_drift_resolves_target_via_drifts_from_edge_when_target_record_id_is_stale() {
    let (_temp, graph) = fixture_graph_with_drift_stale_target_id();

    let output = egregore()
        .args(["query", "drift", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(
        parsed["name"], "drifted_fn",
        "name must be resolved via DriftsFrom edge"
    );
    assert_eq!(
        parsed["repo_relative_path"], "src/lib.rs",
        "path must be resolved via DriftsFrom edge"
    );
}

// ---------------------------------------------------------------------------
// Fix B: missing / empty --data-dir must be an error, not a silent no-match
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbol_missing_data_dir_exits_1_with_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    // Use a sub-path that is never created — guaranteed not to exist
    let never_created = temp.path().join("never_created_sub");

    egregore()
        .args(["query", "symbol", "scan_repository", "--data-dir"])
        .arg(&never_created)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_match("error|not found|empty|ingest").unwrap());
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbol_empty_data_dir_exits_1_with_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let empty_dir = temp.path().join("never_populated");
    fs::create_dir_all(&empty_dir).expect("create dir");

    egregore()
        .args(["query", "symbol", "scan_repository", "--data-dir"])
        .arg(&empty_dir)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_match("error|not found|empty|ingest").unwrap());
}

// ---------------------------------------------------------------------------
// Fix E: tombstoned records must not appear in current-state queries
// ---------------------------------------------------------------------------

fn fixture_graph_with_tombstoned_symbol() -> (tempfile::TempDir, PathBuf) {
    use aletheia_egregore::ir::SCHEMA_VERSION;
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("tombstoned.jsonl");

    let file_id = stable_id(&["node", "File", "src/lib.rs"]);
    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "deleted_fn"]);
    let tombstone_id = stable_id(&["tombstone", &sym_id]);

    let file_node = GraphRecord::syntax_node(
        file_id,
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 50),
        "lib.rs".to_owned(),
        "rust",
        "Source file src/lib.rs".to_owned(),
    );
    let sym_node = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(5, 15),
        "deleted_fn".to_owned(),
        "A function that will be deleted".to_owned(),
    );
    let tombstone = GraphRecord::Tombstone {
        id: tombstone_id,
        schema_version: SCHEMA_VERSION,
        deleted_id: sym_id,
        summary: "deleted_fn removed".to_owned(),
        producer: None,
    };

    let mut graph = Graph::new();
    graph.push(file_node);
    graph.push(sym_node);
    graph.push(tombstone);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn query_symbol_exits_2_for_tombstoned_symbol() {
    let (_temp, graph) = fixture_graph_with_tombstoned_symbol();

    egregore()
        .args(["query", "symbol", "deleted_fn", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
}

#[test]
fn query_file_exits_2_for_file_with_only_tombstoned_symbols() {
    let (_temp, graph) = fixture_graph_with_tombstoned_symbol();

    egregore()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
}

fn fixture_graph_with_tombstoned_temporal_symbol() -> (tempfile::TempDir, PathBuf) {
    use aletheia_egregore::ir::SCHEMA_VERSION;
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("tombstoned_temporal.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "removed_fn", "aaaa"]);
    let tombstone_id = stable_id(&["tombstone", &sym_id]);

    let sym_node = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(5, 15),
        "removed_fn".to_owned(),
        "Function observed at commit aaaa".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaaaaaaaaaaaaaa".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });
    let tombstone = GraphRecord::Tombstone {
        id: tombstone_id,
        schema_version: SCHEMA_VERSION,
        deleted_id: sym_id,
        summary: "removed_fn deleted after aaaa".to_owned(),
        producer: None,
    };

    let mut graph = Graph::new();
    graph.push(sym_node);
    graph.push(tombstone);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn query_symbol_at_commit_still_works_for_tombstoned_temporal_symbol() {
    let (_temp, graph) = fixture_graph_with_tombstoned_temporal_symbol();

    // --at query must still find the historical observation even after tombstone
    let output = egregore()
        .args(["query", "symbol", "removed_fn", "--at", "aaaa", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("line")).expect("json");
    assert_eq!(parsed["name"], "removed_fn");
    assert_eq!(parsed["git_commit"], "aaaaaaaaaaaaaaaa");
}

fn fixture_graph_with_temporal_file_and_symbol() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("temporal_file.jsonl");

    let file_id = stable_id(&["node", "File", "src/lib.rs", "aaaa"]);
    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "temporal_fn", "aaaa"]);

    let file_node = GraphRecord::syntax_node(
        file_id,
        NodeKind::File,
        "src/lib.rs".to_owned(),
        span(1, 50),
        "lib.rs".to_owned(),
        "rust",
        "Source file at commit aaaa".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaaaaaaaaaaaaaa".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });
    let sym_node = GraphRecord::symbol(
        sym_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(10, 20),
        "temporal_fn".to_owned(),
        "Function at commit aaaa".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaaaaaaaaaaaaaa".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let mut graph = Graph::new();
    graph.push(file_node);
    graph.push(sym_node);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn query_file_finds_temporal_file_node_from_scan_history_graph() {
    let (_temp, graph) = fixture_graph_with_temporal_file_and_symbol();

    let output = egregore()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("line")).expect("json");
    assert_eq!(parsed["name"], "temporal_fn");
    assert_eq!(parsed["git_commit"], "aaaaaaaaaaaaaaaa");
}

// ---------------------------------------------------------------------------
// query context — fixture helpers
// ---------------------------------------------------------------------------

/// Creates a seeded fixture: one Symbol, one Observation linked via `evidence_link`,
/// one Task linked via `evidence_link`, and one Verification linked via `evidence_link`.
#[allow(clippy::too_many_lines)]
fn fixture_context_seeded() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("context_seeded.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "my_function"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 10,
            end_line: 20,
        },
        "my_function".to_owned(),
        "Rust fn my_function at src/lib.rs:10".to_owned(),
    );

    // Observation node linked to the symbol via evidence_links
    let obs_id = agent_memory_stable_id(&["obs", "ctx_obs1"]);
    let mut obs = GraphRecord::node(
        obs_id,
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation about my_function".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut text,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ref mut schema_version,
        ..
    } = obs
    {
        *text = Some("my_function needs error handling".to_owned());
        *agent_id = Some("agent:ctx_test".to_owned());
        *session_id = Some("session:ctx_test".to_owned());
        *observed_at = Some("2026-03-01T12:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Task node linked to the symbol
    let task_id = aletheia_egregore::ir::project_stable_id(&["task", "ctx_task1"]);
    let mut task = GraphRecord::node(
        task_id,
        NodeKind::Task,
        None,
        None,
        Some("Add error handling to my_function".to_owned()),
        "Task: add error handling".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut evidence_links,
        ref mut schema_version,
        ..
    } = task
    {
        *title = Some("Add error handling to my_function".to_owned());
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Verification record linked to the symbol
    let ver_id = verification_stable_id(&["verification", "ctx_ver1"]);
    let mut ver = GraphRecord::node(
        ver_id,
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification of my_function".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ref mut status,
        ref mut verification_kind,
        ..
    } = ver
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("passed".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = Graph::new();
    graph.push(sym);
    graph.push(obs);
    graph.push(task);
    graph.push(ver);
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    (temp, path)
}

/// Extends [`fixture_context_seeded`] with two `SemanticDrift` records
/// targeting `my_function` via `DriftsFrom` edges, for issue #108's
/// `drift_history` section. Returns `(temp_dir, graph_path, small_drift_id,
/// large_drift_id)`.
fn fixture_context_seeded_with_drift() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("context_seeded_drift.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "my_function"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 10,
            end_line: 20,
        },
        "my_function".to_owned(),
        "Rust fn my_function at src/lib.rs:10".to_owned(),
    );

    let drift_small_id = stable_id(&["node", "SemanticDrift", "ctx_small"]);
    let drift_large_id = stable_id(&["node", "SemanticDrift", "ctx_large"]);

    let drift_small = GraphRecord::node(
        drift_small_id.clone(),
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some("my_function".to_owned()),
        "small drift".to_owned(),
    )
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "test-model-v1".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: sym_id.clone(),
        prior_record_id: sym_id.clone(),
        before_git_commit: "aaaaaaaa".to_owned(),
        after_git_commit: "bbbbbbbb".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.25,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });

    let drift_large = GraphRecord::node(
        drift_large_id.clone(),
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some("my_function".to_owned()),
        "large drift".to_owned(),
    )
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "test-model-v1".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: sym_id.clone(),
        prior_record_id: sym_id.clone(),
        before_git_commit: "bbbbbbbb".to_owned(),
        after_git_commit: "cccccccc".to_owned(),
        before_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-03T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.9,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });

    let edge_small = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_small_id.clone(),
        sym_id.clone(),
        Some("1.0".to_owned()),
        "drifts from edge".to_owned(),
    );
    let edge_large = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_large_id.clone(),
        sym_id,
        Some("1.0".to_owned()),
        "drifts from edge".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(sym);
    graph.push(drift_small);
    graph.push(drift_large);
    graph.push(edge_small);
    graph.push(edge_large);
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    (temp, path, drift_small_id, drift_large_id)
}

// ---------------------------------------------------------------------------
// query context — happy path: structured JSON with all sections
// ---------------------------------------------------------------------------

/// AC1 + AC2: a single `eg query context` returns all linked context sections.
#[test]
fn query_context_returns_structured_json_with_all_sections() {
    let (_temp, graph) = fixture_context_seeded();

    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(parsed["ok"], true, "ok must be true on success");
    assert_eq!(parsed["symbol_name"], "my_function");

    // Corpus disclosure (issue #427): a snapshot-less graph discloses a single
    // snapshot (this lane reads the union over a scan-history store).
    assert_eq!(parsed["corpus_mode"], "single_snapshot");
    assert_eq!(parsed["corpus_mode_source"], "default");
    assert!(
        parsed["corpus_disclaimer"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );

    // source_facts section must contain the symbol node
    let facts = parsed["source_facts"]
        .as_array()
        .expect("source_facts array");
    assert!(!facts.is_empty(), "source_facts must not be empty");
    assert!(
        facts.iter().any(|f| f["kind"] == "Symbol"),
        "source_facts must contain the Symbol node"
    );

    // observations section must exist
    let obs_arr = parsed["observations"]
        .as_array()
        .expect("observations array");
    assert!(!obs_arr.is_empty(), "observations must not be empty");

    // project_state section must exist
    let proj = parsed["project_state"]
        .as_array()
        .expect("project_state array");
    assert!(!proj.is_empty(), "project_state must not be empty");

    // verification_evidence section must exist
    let ver = parsed["verification_evidence"]
        .as_array()
        .expect("verification_evidence array");
    assert!(!ver.is_empty(), "verification_evidence must not be empty");

    // AC4 (issue #108): drift_history is always present, empty (not absent,
    // not an error) when the fixture carries no SemanticDrift records.
    let drift_history = parsed["drift_history"]
        .as_array()
        .expect("drift_history array");
    assert!(
        drift_history.is_empty(),
        "drift_history must be empty when no drift records exist"
    );
}

// ---------------------------------------------------------------------------
// query context — AC2: every source fact carries record_id + path/span
// ---------------------------------------------------------------------------

#[test]
fn query_context_source_facts_carry_record_id_and_path() {
    let (_temp, graph) = fixture_context_seeded();

    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let facts = parsed["source_facts"].as_array().expect("source_facts");

    for fact in facts {
        assert!(
            fact["record_id"].is_string() && !fact["record_id"].as_str().unwrap().is_empty(),
            "every source_fact must have a non-empty record_id"
        );
        let has_path = fact["repo_relative_path"].is_string();
        let has_commit = fact
            .get("git_commit")
            .is_some_and(serde_json::Value::is_string);
        let has_valid_time = fact
            .get("valid_time")
            .is_some_and(serde_json::Value::is_string);
        assert!(
            has_path || has_commit || has_valid_time,
            "source_fact {} must carry repo_relative_path, git_commit, or valid_time",
            fact["record_id"]
        );
    }
}

// ---------------------------------------------------------------------------
// query context — AC3: every observation carries provenance fields
// ---------------------------------------------------------------------------

#[test]
fn query_context_observations_carry_provenance_fields() {
    let (_temp, graph) = fixture_context_seeded();

    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let obs_arr = parsed["observations"].as_array().expect("observations");

    for obs in obs_arr {
        assert!(
            obs["record_id"].is_string(),
            "every observation must have a record_id"
        );
        assert!(
            obs.get("provenance_handle")
                .is_some_and(serde_json::Value::is_string)
                || obs
                    .get("agent_id")
                    .is_some_and(serde_json::Value::is_string),
            "every observation must carry a provenance_handle or agent_id"
        );
        assert!(
            obs.get("observed_at")
                .is_some_and(serde_json::Value::is_string),
            "every observation must carry observed_at"
        );
        assert!(
            obs.get("confidence")
                .is_some_and(serde_json::Value::is_string),
            "every observation must carry confidence"
        );
    }
}

// ---------------------------------------------------------------------------
// query context — AC4: observation is never presented as source truth
// ---------------------------------------------------------------------------

#[test]
fn query_context_observation_not_in_source_facts() {
    let (_temp, graph) = fixture_context_seeded();

    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let facts = parsed["source_facts"].as_array().expect("source_facts");

    for fact in facts {
        assert_ne!(
            fact["kind"], "Observation",
            "Observation nodes must never appear in source_facts"
        );
    }
}

// ---------------------------------------------------------------------------
// query context — AC6: no-match is explicit and machine-readable
// ---------------------------------------------------------------------------

#[test]
fn query_context_no_match_returns_machine_readable_error_and_exits_2() {
    let (_temp, graph) = fixture_context_seeded();

    let output = egregore()
        .args(["query", "context", "nonexistent_fn_xyz", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("must be valid JSON");
    assert_eq!(parsed["ok"], false, "ok must be false for no-match");
    assert_eq!(
        parsed["error"]["code"], "no_match",
        "error.code must be 'no_match'"
    );
    assert_eq!(
        parsed["error"]["symbol_name"], "nonexistent_fn_xyz",
        "error.symbol_name must echo the queried symbol"
    );
}

// ---------------------------------------------------------------------------
// query context — AC7: stable output ordering
// ---------------------------------------------------------------------------

#[test]
fn query_context_stable_ordering_across_repeated_calls() {
    let (_temp, graph) = fixture_context_seeded();

    let run = || {
        let output = egregore()
            .args(["query", "context", "my_function", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(output).expect("utf8")
    };

    let first = run();
    let second = run();
    assert_eq!(
        first, second,
        "context query output must be identical for repeated calls on the same fixture"
    );
}

// ---------------------------------------------------------------------------
// query context — drift_history section (issue #108)
// ---------------------------------------------------------------------------

/// AC1 + AC2 + AC3: `drift_history` lists the symbol's own `SemanticDrift`
/// records, score descending, each carrying the full documented field set.
#[test]
fn query_context_includes_drift_history_ordered_by_score_desc() {
    let (_temp, graph, drift_small_id, drift_large_id) = fixture_context_seeded_with_drift();

    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let drift_history = parsed["drift_history"]
        .as_array()
        .expect("drift_history array");
    assert_eq!(drift_history.len(), 2, "both drift records must surface");

    // AC3: score descending, then record ID.
    assert_eq!(drift_history[0]["record_id"], drift_large_id);
    assert_eq!(drift_history[1]["record_id"], drift_small_id);
    assert!(
        drift_history[0]["score"].as_f64().unwrap() > drift_history[1]["score"].as_f64().unwrap()
    );

    // AC2: each entry carries the documented field set, including the
    // embedding_model identity block (parity with `eg query drift`).
    let large = &drift_history[0];
    assert_eq!(large["before_commit"], "bbbbbbbb");
    assert_eq!(large["after_commit"], "cccccccc");
    assert_eq!(large["before_valid_time"], "2026-01-02T00:00:00Z");
    assert_eq!(large["after_valid_time"], "2026-01-03T00:00:00Z");
    let embedding_model = &large["embedding_model"];
    assert_eq!(embedding_model["provider"], "test");
    assert_eq!(embedding_model["name"], "test-model-v1");
    assert_eq!(embedding_model["version"], "v1");
    assert_eq!(embedding_model["dim"], 384);
    assert_eq!(embedding_model["content_hash"], "fixture");
}

/// AC5: drift entries must never be mixed into `observations`.
#[test]
fn query_context_drift_history_never_appears_in_observations() {
    let (_temp, graph, drift_small_id, drift_large_id) = fixture_context_seeded_with_drift();

    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let observations = parsed["observations"]
        .as_array()
        .expect("observations array");
    assert!(
        !observations
            .iter()
            .any(|o| o["record_id"] == drift_small_id || o["record_id"] == drift_large_id),
        "drift records must never appear in observations (AC5): {observations:?}"
    );
}

/// AC3 / AC7 parity: `drift_history` ordering is byte-stable across repeated
/// calls, the same guarantee already held for the other five sections.
#[test]
fn query_context_drift_history_stable_ordering_across_repeated_calls() {
    let (_temp, graph, _small, _large) = fixture_context_seeded_with_drift();

    let run = || {
        let output = egregore()
            .args(["query", "context", "my_function", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(output).expect("utf8")
    };

    let first = run();
    let second = run();
    assert_eq!(
        first, second,
        "context query output (including drift_history) must be identical across repeated calls"
    );
}

// ---------------------------------------------------------------------------
// query context — task body_handle appears in project_state JSON
// ---------------------------------------------------------------------------

#[test]
fn query_context_task_body_handle_in_project_state() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("task_body.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "body_handle_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: 5,
            end_line: 15,
        },
        "body_handle_fn".to_owned(),
        "fn body_handle_fn".to_owned(),
    );

    let task_id = aletheia_egregore::ir::project_stable_id(&["task", "body_handle_task"]);
    let mut task = GraphRecord::node(
        task_id,
        NodeKind::Task,
        None,
        None,
        Some("Implement body_handle_fn".to_owned()),
        "Task: Implement body_handle_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut evidence_links,
        ref mut schema_version,
        ref mut body_handle,
        ..
    } = task
    {
        *title = Some("Implement body_handle_fn".to_owned());
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
        *body_handle = Some(Box::new(aletheia_egregore::ir::OutputHandle {
            inline: Some("Add error handling and retry logic.".to_owned()),
            hash: "aabbccdd".to_owned(),
            bytes: 35,
        }));
    }

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(sym);
    graph.push(task);
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    let output = egregore()
        .args(["query", "context", "body_handle_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value =
        serde_json::from_slice(&output).expect("stdout must be valid JSON");

    let project_state = parsed["project_state"]
        .as_array()
        .expect("project_state must be array");

    let task_item = project_state
        .iter()
        .find(|item| item["kind"].as_str() == Some("Task"))
        .expect("Task must be in project_state");

    assert!(
        task_item.get("body_handle").is_some(),
        "body_handle must be present in Task project_state item; got: {task_item}"
    );
    assert_eq!(
        task_item["body_handle"]["hash"].as_str(),
        Some("aabbccdd"),
        "body_handle.hash must match"
    );
    assert_eq!(
        task_item["body_handle"]["inline"].as_str(),
        Some("Add error handling and retry logic."),
        "body_handle.inline must match"
    );
}

// ---------------------------------------------------------------------------
// query context — topology edge temporal metadata appears in JSON output
// ---------------------------------------------------------------------------
//
// When a scan-history graph contains a DEFINES edge with temporal metadata,
// the JSON output for topology_edges must include git_commit and valid_time
// so consumers can cite which commit the file→symbol edge came from.

#[test]
fn query_context_topology_edge_carries_temporal_metadata() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("temporal_topo.jsonl");

    let file_id = stable_id(&["file", "temporal_topo_file"]);
    let sym_id = stable_id(&["node", "Symbol", "src/topo.rs", "temporal_topo_fn"]);

    let file = GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some("src/topo.rs".to_owned()),
        None,
        None,
        "src/topo.rs".to_owned(),
    );
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/topo.rs".to_owned(),
        span(1, 10),
        "temporal_topo_fn".to_owned(),
        "fn temporal_topo_fn".to_owned(),
    );
    // DEFINES edge with temporal metadata (as produced by scan-history)
    let defines_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        file_id,
        sym_id,
        None,
        "file defines symbol".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "deadbeefcafe1234".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-03-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-03-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(file);
    graph.push(sym);
    graph.push(defines_edge);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    let output = egregore()
        .args(["query", "context", "temporal_topo_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let topo = parsed["topology_edges"]
        .as_array()
        .expect("topology_edges array");
    assert!(
        !topo.is_empty(),
        "DEFINES edge must appear in topology_edges"
    );

    let defines = &topo[0];
    assert_eq!(defines["label"], "DEFINES");
    assert_eq!(
        defines.get("git_commit").and_then(|v| v.as_str()),
        Some("deadbeefcafe1234"),
        "topology_edge must carry git_commit from temporal metadata; got: {defines}"
    );
    assert_eq!(
        defines.get("valid_time").and_then(|v| v.as_str()),
        Some("2026-03-01T00:00:00Z"),
        "topology_edge must carry valid_time from temporal metadata; got: {defines}"
    );
}

// ---------------------------------------------------------------------------
// query context — missing graph file exits non-zero
// ---------------------------------------------------------------------------

#[test]
fn query_context_missing_graph_file_exits_nonzero() {
    egregore()
        .args([
            "query",
            "context",
            "my_function",
            "--graph",
            "/nonexistent/path/context.jsonl",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_match("error|failed").unwrap());
}

// ---------------------------------------------------------------------------
// query context — Failure node carries failure_kind and exit_code
// ---------------------------------------------------------------------------
//
// Finding (line 677): ContextObservation only serializes generic observation
// fields, dropping Failure-specific `failure_kind` (required by schema) and
// `exit_code`. Consumers cannot determine what kind of failure occurred from
// `eg query context` alone without reloading the raw record.

#[test]
fn query_context_failure_record_carries_failure_kind_and_exit_code() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("failure_fields.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "failing_ctx_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 1,
            end_line: 5,
        },
        "failing_ctx_fn".to_owned(),
        "fn failing_ctx_fn".to_owned(),
    );

    let fail_id = agent_memory_stable_id(&["fail", "cli_failure_test"]);
    let mut fail = GraphRecord::node(
        fail_id.clone(),
        NodeKind::Failure,
        None,
        None,
        None,
        "command failure for failing_ctx_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut failure_kind,
        ref mut exit_code,
        ref mut evidence_links,
        ..
    } = fail
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-04-01T10:00:00Z".to_owned());
        *confidence = Some("1.0".to_owned());
        *failure_kind = Some("command_failure".to_owned());
        *exit_code = Some(1);
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "FAILED_ON".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(sym);
    graph.push(fail);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let output = egregore()
        .args(["query", "context", "failing_ctx_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("valid JSON from query context");

    let obs = json["observations"]
        .as_array()
        .expect("observations array")
        .iter()
        .find(|o| o["record_id"].as_str() == Some(&fail_id))
        .expect("failure record must appear in observations");

    assert_eq!(
        obs["failure_kind"].as_str(),
        Some("command_failure"),
        "failure_kind must be forwarded in observation output"
    );
    assert_eq!(
        obs["exit_code"].as_i64(),
        Some(1),
        "exit_code must be forwarded in observation output"
    );
}

// ---------------------------------------------------------------------------
// query context — CommandRun carries exit_code, executed_at, evidence_quality
// ---------------------------------------------------------------------------
//
// Finding (line 712): ContextLinkedItem for verification evidence only exposes
// status/verification_kind plus handles. CommandRun records may carry exit_code,
// executed_at, and evidence_quality; without forwarding those, consumers cannot
// determine the execution outcome from `eg query context` alone.

#[test]
fn query_context_command_run_carries_execution_metadata() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("command_run_fields.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "cmd_run_ctx_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 1,
            end_line: 5,
        },
        "cmd_run_ctx_fn".to_owned(),
        "fn cmd_run_ctx_fn".to_owned(),
    );

    let run_id = verification_stable_id(&["run", "cli_cmd_run_test"]);
    let mut run = GraphRecord::node(
        run_id.clone(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "command run for cmd_run_ctx_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut status,
        ref mut exit_code,
        ref mut executed_at,
        ref mut evidence_quality,
        ref mut evidence_links,
        ..
    } = run
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("passed".to_owned());
        *exit_code = Some(0);
        *executed_at = Some("2026-04-01T11:00:00Z".to_owned());
        *evidence_quality = Some("verbatim".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(sym);
    graph.push(run);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let output = egregore()
        .args(["query", "context", "cmd_run_ctx_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("valid JSON from query context");

    let item = json["verification_evidence"]
        .as_array()
        .expect("verification_evidence array")
        .iter()
        .find(|v| v["record_id"].as_str() == Some(&run_id))
        .expect("CommandRun must appear in verification_evidence");

    assert_eq!(
        item["exit_code"].as_i64(),
        Some(0),
        "exit_code must be forwarded for CommandRun"
    );
    assert_eq!(
        item["executed_at"].as_str(),
        Some("2026-04-01T11:00:00Z"),
        "executed_at must be forwarded for CommandRun"
    );
    assert_eq!(
        item["evidence_quality"].as_str(),
        Some("verbatim"),
        "evidence_quality must be forwarded for CommandRun"
    );
}

// ---------------------------------------------------------------------------
// query context — PatchArtifact carries required patch metadata fields
// ---------------------------------------------------------------------------
//
// Finding (line 745): ContextLinkedItem for PatchArtifact drops required schema
// fields base_commit/unknown_base_reason, patch_bytes_size, and producer_session_id.
// Consumers cannot verify the patch provenance or size without reloading the record.

#[test]
fn query_context_patch_artifact_carries_patch_metadata() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("patch_artifact_fields.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "patch_ctx_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 1,
            end_line: 5,
        },
        "patch_ctx_fn".to_owned(),
        "fn patch_ctx_fn".to_owned(),
    );

    let patch_id = agent_memory_stable_id(&["patch", "cli_patch_test"]);
    let mut patch_record = GraphRecord::node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        None,
        None,
        None,
        "patch artifact for patch_ctx_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut base_commit,
        ref mut patch_bytes_size,
        ref mut producer_session_id,
        ref mut patch_status,
        ref mut evidence_links,
        ..
    } = patch_record
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *base_commit = Some("aabbccdd".to_owned());
        *patch_bytes_size = Some(1234);
        *producer_session_id = Some("session:patch_ctx_test".to_owned());
        *patch_status = Some("valid".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(sym);
    graph.push(patch_record);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let output = egregore()
        .args(["query", "context", "patch_ctx_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("valid JSON from query context");

    let item = json["artifacts"]
        .as_array()
        .expect("artifacts array")
        .iter()
        .find(|a| a["record_id"].as_str() == Some(&patch_id))
        .expect("PatchArtifact must appear in artifacts");

    assert_eq!(
        item["base_commit"].as_str(),
        Some("aabbccdd"),
        "base_commit must be forwarded for PatchArtifact"
    );
    assert_eq!(
        item["patch_bytes_size"].as_u64(),
        Some(1234),
        "patch_bytes_size must be forwarded for PatchArtifact"
    );
    assert_eq!(
        item["producer_session_id"].as_str(),
        Some("session:patch_ctx_test"),
        "producer_session_id must be forwarded for PatchArtifact"
    );
}

// ---------------------------------------------------------------------------
// query context — generic Artifact without title/name/text carries summary
// ---------------------------------------------------------------------------
//
// Finding (line 701): ContextLinkedItem has no summary field. For generic Artifact
// records where title/name/text/status are all None, the only human-readable field
// is the required GraphRecord::summary. Without it, consumers see only record_id
// and kind and must reload the raw graph to understand the context item.

#[test]
fn query_context_linked_item_carries_summary() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("artifact_summary.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "summary_ctx_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 1,
            end_line: 5,
        },
        "summary_ctx_fn".to_owned(),
        "fn summary_ctx_fn".to_owned(),
    );

    // Generic Artifact — no title, name, text, or status; only summary identifies it.
    let artifact_id = agent_memory_stable_id(&["artifact", "cli_summary_test"]);
    let mut artifact = GraphRecord::node(
        artifact_id.clone(),
        NodeKind::Artifact,
        None,
        None,
        None,
        "build output artifact for summary_ctx_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = artifact
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(sym);
    graph.push(artifact);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let output = egregore()
        .args(["query", "context", "summary_ctx_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("valid JSON from query context");

    let item = json["artifacts"]
        .as_array()
        .expect("artifacts array")
        .iter()
        .find(|a| a["record_id"].as_str() == Some(&artifact_id))
        .expect("Artifact must appear in artifacts");

    assert_eq!(
        item["summary"].as_str(),
        Some("build output artifact for summary_ctx_fn"),
        "summary must be forwarded for generic Artifact records without title/name/text/status"
    );
}

// ---------------------------------------------------------------------------
// query context — Decision observation carries summary field
// ---------------------------------------------------------------------------
//
// Finding (line 1710): Decision records are classified into observations but do
// not populate the Observation-only `text` field. Their human-readable content
// is in GraphRecord::summary. Without a summary field in ContextObservation,
// consumers cannot understand the Decision item without reloading the raw graph.

#[test]
fn query_context_decision_carries_summary() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("decision_summary.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "decision_ctx_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 1,
            end_line: 5,
        },
        "decision_ctx_fn".to_owned(),
        "fn decision_ctx_fn".to_owned(),
    );

    let dec_id = agent_memory_stable_id(&["decision", "cli_decision_test"]);
    let mut decision = GraphRecord::node(
        dec_id.clone(),
        NodeKind::Decision,
        None,
        None,
        None,
        "approved approach: use streaming parser for decision_ctx_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        // text intentionally NOT set — Decision uses summary, not text
        ..
    } = decision
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:test".to_owned());
        *session_id = Some("session:test".to_owned());
        *observed_at = Some("2026-04-01T09:00:00Z".to_owned());
        *confidence = Some("0.95".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "0.95".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(sym);
    graph.push(decision);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let output = egregore()
        .args(["query", "context", "decision_ctx_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("valid JSON from query context");

    let dec_item = json["observations"]
        .as_array()
        .expect("observations array")
        .iter()
        .find(|o| o["record_id"].as_str() == Some(&dec_id))
        .expect("Decision must appear in observations");

    assert_eq!(
        dec_item["summary"].as_str(),
        Some("approved approach: use streaming parser for decision_ctx_fn"),
        "Decision must carry summary field in observation output (Decision records \
         use summary, not text, for their human-readable content)"
    );
}

// ---------------------------------------------------------------------------
// query context — FileEdit carries hunk_count, linked_turn_id, linked_patch_id
// ---------------------------------------------------------------------------
//
// Finding (cli.rs:1786): ContextLinkedItem drops FileEdit-required fields
// `hunk_count` and `linked_turn_id` (and optional `linked_patch_id`).
// Consumers need these to reconstruct the edit event or associate it with
// its owning agent turn/patch from the context response alone.

#[test]
fn query_context_file_edit_carries_fileedit_metadata() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("file_edit_fields.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "file_edit_ctx_fn"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 1,
            end_line: 5,
        },
        "file_edit_ctx_fn".to_owned(),
        "fn file_edit_ctx_fn".to_owned(),
    );

    let file_edit_id = agent_memory_stable_id(&["fileedit", "file_edit_ctx_test"]);
    let mut file_edit = GraphRecord::node(
        file_edit_id.clone(),
        NodeKind::FileEdit,
        Some("src/lib.rs".to_owned()),
        None,
        None,
        "file edit for file_edit_ctx_fn".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut edit_kind,
        ref mut hunk_count,
        ref mut linked_turn_id,
        ref mut linked_patch_id,
        ref mut evidence_links,
        ..
    } = file_edit
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *edit_kind = Some("modify".to_owned());
        *hunk_count = Some(3);
        *linked_turn_id = Some("agent_memory:v1:turn:fe_ctx_turn".to_owned());
        *linked_patch_id = Some("agent_memory:v1:patch:fe_ctx_patch".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(sym_id),
            target_domain: "codegraph".to_owned(),
            relation: "TOUCHED_FILE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = aletheia_egregore::ir::Graph::new();
    graph.push(sym);
    graph.push(file_edit);
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");

    let output = egregore()
        .args(["query", "context", "file_edit_ctx_fn", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("valid JSON from query context");

    let item = json["artifacts"]
        .as_array()
        .expect("artifacts array")
        .iter()
        .find(|a| a["record_id"].as_str() == Some(&file_edit_id))
        .expect("FileEdit must appear in artifacts");

    assert_eq!(
        item["hunk_count"].as_u64(),
        Some(3),
        "hunk_count must be forwarded for FileEdit"
    );
    assert_eq!(
        item["linked_turn_id"].as_str(),
        Some("agent_memory:v1:turn:fe_ctx_turn"),
        "linked_turn_id must be forwarded for FileEdit"
    );
    assert_eq!(
        item["linked_patch_id"].as_str(),
        Some("agent_memory:v1:patch:fe_ctx_patch"),
        "linked_patch_id must be forwarded for FileEdit (optional but present)"
    );
}

// ---------------------------------------------------------------------------
// Issue 87: extraction completeness tests
// ---------------------------------------------------------------------------

fn fixture_graph_with_diagnostics() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let clean_file_id = stable_id(&["node", "File", "src/clean.rs"]);
    let clean_sym_id = stable_id(&["node", "Symbol", "src/clean.rs", "clean_fn"]);

    let dirty_file_id = stable_id(&["node", "File", "src/dirty.rs"]);
    let dirty_sym_id = stable_id(&["node", "Symbol", "src/dirty.rs", "dirty_fn"]);

    let clean_file = GraphRecord::syntax_node(
        clean_file_id.clone(),
        NodeKind::File,
        "src/clean.rs".to_owned(),
        span(1, 50),
        "clean.rs".to_owned(),
        "rust",
        "Source file src/clean.rs".to_owned(),
    );
    let clean_sym = GraphRecord::symbol(
        clean_sym_id.clone(),
        "fn",
        "src/clean.rs".to_owned(),
        span(10, 20),
        "clean_fn".to_owned(),
        "Rust function clean_fn".to_owned(),
    );
    let clean_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        clean_file_id,
        clean_sym_id,
        Some("1.0".to_owned()),
        "defines".to_owned(),
    );

    let dirty_file = GraphRecord::syntax_node(
        dirty_file_id.clone(),
        NodeKind::File,
        "src/dirty.rs".to_owned(),
        span(1, 50),
        "dirty.rs".to_owned(),
        "rust",
        "Source file src/dirty.rs".to_owned(),
    );
    let dirty_sym = GraphRecord::symbol(
        dirty_sym_id.clone(),
        "fn",
        "src/dirty.rs".to_owned(),
        span(10, 20),
        "dirty_fn".to_owned(),
        "Rust function dirty_fn".to_owned(),
    );
    let dirty_edge = GraphRecord::edge(
        EdgeLabel::Defines,
        dirty_file_id,
        dirty_sym_id,
        Some("1.0".to_owned()),
        "defines".to_owned(),
    );

    let diag_id = stable_id(&["node", "Diagnostic", "src/dirty.rs", "error_1"]);
    let diag = GraphRecord::node(
        diag_id,
        NodeKind::Diagnostic,
        Some("src/dirty.rs".to_owned()),
        Some(span(15, 16)),
        None,
        "unparsed macro".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(clean_file);
    graph.push(clean_sym);
    graph.push(clean_edge);
    graph.push(dirty_file);
    graph.push(dirty_sym);
    graph.push(dirty_edge);
    graph.push(diag);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

#[test]
fn query_file_completeness_flagging() {
    let (_temp, graph) = fixture_graph_with_diagnostics();

    // Query clean file - should be complete
    let output = egregore()
        .args(["query", "file", "src/clean.rs", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["extraction_completeness"], "complete");
    assert!(parsed["diagnostics"].is_null() || parsed.get("diagnostics").is_none());

    // Query dirty file - should be partial and enumerate diagnostics
    let output = egregore()
        .args(["query", "file", "src/dirty.rs", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["extraction_completeness"], "partial");

    let diags = parsed["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["repo_relative_path"], "src/dirty.rs");
    assert_eq!(diags[0]["span"]["start_line"].as_u64(), Some(15));
}

#[test]
fn query_symbol_completeness_flagging() {
    let (_temp, graph) = fixture_graph_with_diagnostics();

    // Clean symbol - complete
    let output = egregore()
        .args(["query", "symbol", "clean_fn", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["extraction_completeness"], "complete");
    assert!(parsed["diagnostics"].is_null() || parsed.get("diagnostics").is_none());

    // Dirty symbol - partial
    let output = egregore()
        .args(["query", "symbol", "dirty_fn", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("first line")).expect("valid JSON");
    assert_eq!(parsed["extraction_completeness"], "partial");
    // Symbol query results don't need to serialize the diagnostics themselves, just the status
}

// ---------------------------------------------------------------------------
// query context — supersession and contradiction resolution
// ---------------------------------------------------------------------------

fn fixture_context_with_supersession() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("supersession_seeded.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "my_function"]);
    let sym = GraphRecord::symbol(
        sym_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 80,
            start_line: 10,
            end_line: 20,
        },
        "my_function".to_owned(),
        "Rust fn my_function at src/lib.rs:10".to_owned(),
    );

    // Helper to make observation node
    let make_obs = |id_str: &str, superseded_by: Option<&str>| -> GraphRecord {
        let obs_id = agent_memory_stable_id(&["obs", id_str]);
        let mut obs = GraphRecord::node(
            obs_id,
            NodeKind::Observation,
            None,
            None,
            None,
            format!("Observation {id_str}"),
        );
        if let GraphRecord::Node {
            ref mut text,
            ref mut agent_id,
            ref mut session_id,
            ref mut observed_at,
            ref mut confidence,
            ref mut evidence_links,
            superseded_by: ref mut node_sub_by,
            ref mut schema_version,
            ..
        } = obs
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *text = Some(format!("Observation text {id_str}"));
            *agent_id = Some("agent:test".to_owned());
            *session_id = Some(format!("session:{id_str}"));
            *observed_at = Some("2026-03-01T12:00:00Z".to_owned());
            *confidence = Some("0.9".to_owned());
            *node_sub_by = superseded_by.map(|s| agent_memory_stable_id(&["obs", s]));
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some(sym_id.clone()),
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_SYMBOL".to_owned(),
                confidence: "0.9".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
        obs
    };

    // A -> B -> C
    // A: superseded by B
    // B: superseded by C
    // C: current (uncontested)
    let obs_a = make_obs("A", Some("B"));
    let obs_b = make_obs("B", Some("C"));
    let obs_c = make_obs("C", None);

    // X contradicts Y, Y contradicts X
    let obs_x = make_obs("X", None);
    let obs_y = make_obs("Y", None);

    // Create contradicts edge between X and Y
    let edge_contradicts = GraphRecord::edge(
        EdgeLabel::Contradicts,
        agent_memory_stable_id(&["obs", "X"]),
        agent_memory_stable_id(&["obs", "Y"]),
        None,
        "X contradicts Y".to_owned(),
    );

    // Uncontested record U
    let obs_u = make_obs("U", None);

    let mut graph = Graph::new();
    graph.push(sym);
    graph.push(obs_a);
    graph.push(obs_b);
    graph.push(obs_c);
    graph.push(obs_x);
    graph.push(obs_y);
    graph.push(edge_contradicts);
    graph.push(obs_u);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    (temp, path)
}

#[test]
fn test_query_context_supersession_exclude() {
    let (_temp, graph) = fixture_context_with_supersession();

    // Default mode: Exclude
    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let observations = parsed["observations"].as_array().expect("array");
    // Only U and C should remain (A, B are superseded, X, Y are contradicted)
    assert_eq!(observations.len(), 2);
    let record_ids: Vec<&str> = observations
        .iter()
        .map(|o| o["record_id"].as_str().unwrap())
        .collect();
    assert!(record_ids.contains(&agent_memory_stable_id(&["obs", "C"]).as_str()));
    assert!(record_ids.contains(&agent_memory_stable_id(&["obs", "U"]).as_str()));

    let excluded = parsed["excluded"].as_array().expect("array");
    assert_eq!(excluded.len(), 4); // A, B, X, Y
}

#[test]
fn test_query_context_supersession_include_but_flag() {
    let (_temp, graph) = fixture_context_with_supersession();

    let output = egregore()
        .args(["query", "context", "my_function", "--graph"])
        .arg(&graph)
        .arg("--supersession")
        .arg("include-but-flag")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let observations = parsed["observations"].as_array().expect("array");
    // All 6 observations should be included
    assert_eq!(observations.len(), 6);

    // Check statuses
    let get_obs = |id_str: &str| {
        let full_id = agent_memory_stable_id(&["obs", id_str]);
        observations
            .iter()
            .find(|o| o["record_id"].as_str().unwrap() == full_id)
            .unwrap()
            .clone()
    };

    let obs_a = get_obs("A");
    assert_eq!(obs_a["temporal_status"], "superseded");
    let sub_by = obs_a["superseded_by"].as_array().unwrap();
    assert_eq!(sub_by.len(), 1);
    assert_eq!(
        sub_by[0]["record_id"],
        agent_memory_stable_id(&["obs", "C"]).as_str()
    );

    let obs_x = get_obs("X");
    assert_eq!(obs_x["temporal_status"], "contradicted");
    let contra_by = obs_x["contradicted_by"].as_array().unwrap();
    assert_eq!(contra_by.len(), 1);
    assert_eq!(
        contra_by[0]["record_id"],
        agent_memory_stable_id(&["obs", "Y"]).as_str()
    );

    let obs_u = get_obs("U");
    assert_eq!(obs_u["temporal_status"], "current");
}
