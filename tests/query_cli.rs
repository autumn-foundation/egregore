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
