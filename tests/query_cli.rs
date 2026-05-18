#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    ir::{Graph, stable_id},
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
    });

    let mut graph = Graph::new();
    graph.push(sym_a);
    graph.push(sym_b);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

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
    })
    .with_semantic_drift(SemanticDriftMetadata {
        model_id: "test-model-v1".to_owned(),
        target_record_id: target_id.clone(),
        before_git_commit: "aaaaaaaa".to_owned(),
        after_git_commit: "bbbbbbbb".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        score: "0.250000".to_owned(),
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
    })
    .with_semantic_drift(SemanticDriftMetadata {
        model_id: "test-model-v1".to_owned(),
        target_record_id: target_id.clone(),
        before_git_commit: "bbbbbbbb".to_owned(),
        after_git_commit: "cccccccc".to_owned(),
        before_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-03T00:00:00Z".to_owned(),
        score: "0.900000".to_owned(),
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
    assert_eq!(first["score"], "0.900000", "largest drift should be first");
    assert_eq!(first["model_id"], "test-model-v1");
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
    assert_eq!(first["score"], "0.900000", "largest drift should be first");
    assert!(first["record_id"].is_string(), "record_id should be string");
    assert_eq!(first["model_id"], "test-model-v1");
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
