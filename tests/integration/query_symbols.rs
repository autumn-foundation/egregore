#![allow(missing_docs)]

//! Integration tests for `eg query symbols <PATTERN>` — partial-name symbol
//! matching against the structural store, with no embedding model required
//! (issue #102).

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    ir::{Graph, stable_id},
    query::symbol_name_matches,
};
use assert_cmd::Command;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// A graph with three symbols across two files, plus a `File` node and
/// summaries whose *text* contains the pattern — so a match on anything but a
/// `Symbol` node's name is detectable as a false positive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");

    let alpha_file_id = stable_id(&["node", "File", "src/alpha.rs"]);
    // The File node's name and summary both contain `handle_` — a structural
    // query over Symbol names must never return it.
    let handle_file_id = stable_id(&["node", "File", "src/handle_stuff.rs"]);
    let input_id = stable_id(&["node", "Symbol", "src/alpha.rs", "handle_input"]);
    let request_id = stable_id(&["node", "Symbol", "src/beta.rs", "handle_request"]);
    let unrelated_id = stable_id(&["node", "Symbol", "src/alpha.rs", "unrelated_fn"]);

    let alpha_file = GraphRecord::syntax_node(
        alpha_file_id.clone(),
        NodeKind::File,
        "src/alpha.rs".to_owned(),
        span(1, 50),
        "alpha.rs".to_owned(),
        "rust",
        // Summary text (comment-shaped) containing the pattern: must not match.
        "Source file src/alpha.rs; comment says call handle_everything here".to_owned(),
    );
    let handle_file = GraphRecord::syntax_node(
        handle_file_id,
        NodeKind::File,
        "src/handle_stuff.rs".to_owned(),
        span(1, 10),
        "handle_stuff.rs".to_owned(),
        "rust",
        "Source file src/handle_stuff.rs".to_owned(),
    );
    let sym_input = GraphRecord::symbol(
        input_id.clone(),
        "fn",
        "src/alpha.rs".to_owned(),
        span(10, 20),
        "handle_input".to_owned(),
        // String-literal-shaped summary containing another symbol's name.
        "Rust function handle_input logs \"handle_request failed\"".to_owned(),
    );
    let sym_request = GraphRecord::symbol(
        request_id,
        "fn",
        "src/beta.rs".to_owned(),
        span(30, 40),
        "handle_request".to_owned(),
        "Rust function handle_request".to_owned(),
    );
    let sym_unrelated = GraphRecord::symbol(
        unrelated_id,
        "fn",
        "src/alpha.rs".to_owned(),
        span(25, 28),
        "unrelated_fn".to_owned(),
        "Rust function unrelated_fn".to_owned(),
    );
    let edge = GraphRecord::edge(
        EdgeLabel::Defines,
        alpha_file_id,
        input_id,
        Some("1.0".to_owned()),
        "file defines symbol".to_owned(),
    );

    let mut graph = Graph::new();
    graph.push(alpha_file);
    graph.push(handle_file);
    graph.push(sym_input);
    graph.push(sym_request);
    graph.push(sym_unrelated);
    graph.push(edge);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

fn fixture_graph_with_tombstoned_symbol() -> (tempfile::TempDir, PathBuf) {
    use aletheia_egregore::ir::SCHEMA_VERSION;
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("tombstoned.jsonl");

    let live_id = stable_id(&["node", "Symbol", "src/lib.rs", "handle_live"]);
    let dead_id = stable_id(&["node", "Symbol", "src/lib.rs", "handle_deleted"]);
    let tombstone_id = stable_id(&["tombstone", &dead_id]);

    let live = GraphRecord::symbol(
        live_id,
        "fn",
        "src/lib.rs".to_owned(),
        span(5, 10),
        "handle_live".to_owned(),
        "Rust function handle_live".to_owned(),
    );
    let dead = GraphRecord::symbol(
        dead_id.clone(),
        "fn",
        "src/lib.rs".to_owned(),
        span(20, 30),
        "handle_deleted".to_owned(),
        "Rust function handle_deleted".to_owned(),
    );
    let tombstone = GraphRecord::Tombstone {
        id: tombstone_id,
        schema_version: SCHEMA_VERSION,
        deleted_id: dead_id,
        summary: "handle_deleted removed".to_owned(),
        producer: None,
    };

    let mut graph = Graph::new();
    graph.push(live);
    graph.push(dead);
    graph.push(tombstone);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

fn fixture_graph_with_temporal_symbol() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history.jsonl");

    let sym_id = stable_id(&["node", "Symbol", "src/lib.rs", "handle_history", "aaa"]);
    let sym = GraphRecord::node(
        sym_id,
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(10, 20)),
        Some("handle_history".to_owned()),
        "handle_history at aaaa1111".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111".to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let mut graph = Graph::new();
    graph.push(sym);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

fn stdout_lines(pattern_args: &[&str], graph: &std::path::Path) -> Vec<serde_json::Value> {
    let output = egregore()
        .args(["query", "symbols"])
        .args(pattern_args)
        .arg("--graph")
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON line"))
        .collect()
}

// ---------------------------------------------------------------------------
// Substring matching (the minimum bar)
// ---------------------------------------------------------------------------

#[test]
fn query_symbols_substring_returns_typed_rows_sorted_by_path_line_id() {
    let (_temp, graph) = fixture_graph();

    let rows = stdout_lines(&["handle_"], &graph);
    assert_eq!(rows.len(), 2, "exactly the two handle_* symbols: {rows:?}");

    // Sorted by (repo_relative_path, span.start_line, record_id):
    // src/alpha.rs before src/beta.rs.
    assert_eq!(rows[0]["name"], "handle_input");
    assert_eq!(rows[0]["repo_relative_path"], "src/alpha.rs");
    assert_eq!(rows[1]["name"], "handle_request");
    assert_eq!(rows[1]["repo_relative_path"], "src/beta.rs");

    // Typed Symbol records with the same shape as `query symbol`.
    for row in &rows {
        assert_eq!(row["kind"], "Symbol");
        assert!(
            row["record_id"]
                .as_str()
                .expect("id")
                .starts_with("codegraph:v")
        );
        assert!(row["schema_version"].is_number());
        assert!(row["span"]["start_line"].as_u64().is_some());
    }
}

#[test]
fn query_symbols_never_matches_file_nodes_or_summary_text() {
    let (_temp, graph) = fixture_graph();

    // `handle_` appears in a File node's name/path, in a comment-shaped
    // summary, and in a string-literal-shaped summary. None of those may
    // produce a row: only Symbol node *names* are searched.
    let rows = stdout_lines(&["handle_"], &graph);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, vec!["handle_input", "handle_request"]);
    assert!(rows.iter().all(|r| r["kind"] == "Symbol"));

    // `handle_everything` exists only inside summary text — no match.
    egregore()
        .args(["query", "symbols", "handle_everything", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
}

#[test]
fn query_symbols_exact_name_is_still_a_substring_match() {
    let (_temp, graph) = fixture_graph();

    let rows = stdout_lines(&["handle_input"], &graph);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "handle_input");
}

// ---------------------------------------------------------------------------
// Glob matching (`*`, anchored)
// ---------------------------------------------------------------------------

#[test]
fn query_symbols_prefix_glob_matches_names_starting_with_prefix() {
    let (_temp, graph) = fixture_graph();

    let rows = stdout_lines(&["handle_*"], &graph);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, vec!["handle_input", "handle_request"]);
}

#[test]
fn query_symbols_suffix_glob_matches_names_ending_with_suffix() {
    let (_temp, graph) = fixture_graph();

    let rows = stdout_lines(&["*_input"], &graph);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, vec!["handle_input"]);
}

#[test]
fn query_symbols_glob_is_anchored_to_the_whole_name() {
    let (_temp, graph) = fixture_graph();

    // Without `*` the pattern `input` substring-matches handle_input; as an
    // anchored glob body with no stars around it, `in*put` must match the
    // whole name — which no symbol satisfies.
    egregore()
        .args(["query", "symbols", "in*put", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));

    // Anchored glob spanning the full name does match.
    let rows = stdout_lines(&["handle_*put"], &graph);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, vec!["handle_input"]);
}

// ---------------------------------------------------------------------------
// Case sensitivity
// ---------------------------------------------------------------------------

#[test]
fn query_symbols_is_case_sensitive_by_default() {
    let (_temp, graph) = fixture_graph();

    egregore()
        .args(["query", "symbols", "HANDLE_", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));
}

#[test]
fn query_symbols_case_insensitive_flag_widens_the_match() {
    let (_temp, graph) = fixture_graph();

    let rows = stdout_lines(&["HANDLE_", "--case-insensitive"], &graph);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, vec!["handle_input", "handle_request"]);
}

// ---------------------------------------------------------------------------
// Exit-code contract: no-match vs store-absent vs malformed pattern
// ---------------------------------------------------------------------------

#[test]
fn query_symbols_exits_2_on_no_match_with_empty_stdout() {
    let (_temp, graph) = fixture_graph();

    egregore()
        .args(["query", "symbols", "zzz_nothing", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no match"));
}

#[test]
fn query_symbols_exits_1_when_store_is_absent_not_2() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("does-not-exist.jsonl");

    // A missing store is an error (exit 1), never conflated with the
    // no-match signal (exit 2).
    egregore()
        .args(["query", "symbols", "handle_", "--graph"])
        .arg(&missing)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
}

#[test]
fn query_symbols_exits_1_on_empty_pattern() {
    let (_temp, graph) = fixture_graph();

    egregore()
        .args(["query", "symbols", "", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("empty"));
}

// ---------------------------------------------------------------------------
// Tombstone exclusion (parity with file_defines)
// ---------------------------------------------------------------------------

#[test]
fn query_symbols_excludes_tombstoned_symbols() {
    let (_temp, graph) = fixture_graph_with_tombstoned_symbol();

    let rows = stdout_lines(&["handle_"], &graph);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, vec!["handle_live"]);
}

// ---------------------------------------------------------------------------
// Temporal records carry git_commit
// ---------------------------------------------------------------------------

#[test]
fn query_symbols_temporal_records_carry_git_commit() {
    let (_temp, graph) = fixture_graph_with_temporal_symbol();

    let rows = stdout_lines(&["handle_hist"], &graph);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["git_commit"],
        "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111"
    );
}

// ---------------------------------------------------------------------------
// Determinism: byte-identical output across runs
// ---------------------------------------------------------------------------

#[test]
fn query_symbols_output_is_byte_identical_across_runs() {
    let (_temp, graph) = fixture_graph();

    let run = || {
        egregore()
            .args(["query", "symbols", "handle_", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    assert_eq!(
        run(),
        run(),
        "identical inputs must produce identical bytes"
    );
}

// ---------------------------------------------------------------------------
// --data-dir (structural embedded store, no --embed required)
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_symbols_data_dir_matches_without_embeddings() {
    let (_temp_graph, graph) = fixture_graph();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let output = egregore()
        .args(["query", "symbols", "handle_", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    let names: Vec<String> = stdout
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("valid JSON");
            v["name"].as_str().expect("name").to_owned()
        })
        .collect();
    assert_eq!(names, vec!["handle_input", "handle_request"]);
}

// ---------------------------------------------------------------------------
// Matcher unit behavior (library surface)
// ---------------------------------------------------------------------------

#[test]
fn symbol_name_matches_substring_and_anchored_glob_semantics() {
    // Literal substring when no `*` is present.
    assert!(symbol_name_matches("handle_", "handle_input", false));
    assert!(symbol_name_matches("dle_in", "handle_input", false));
    assert!(!symbol_name_matches("handle_x", "handle_input", false));

    // `*` makes the pattern an anchored glob over the whole name.
    assert!(symbol_name_matches("handle_*", "handle_input", false));
    assert!(symbol_name_matches("*_input", "handle_input", false));
    assert!(symbol_name_matches("h*t", "handle_input", false));
    assert!(symbol_name_matches("*", "anything", false));
    assert!(!symbol_name_matches("in*put", "handle_input", false));
    assert!(!symbol_name_matches("handle_*x", "handle_input", false));

    // A starless glob segment covering the whole name is exact.
    assert!(symbol_name_matches("a*a", "aba", false));
    assert!(!symbol_name_matches("a*a", "a", false));

    // Case sensitivity.
    assert!(!symbol_name_matches("HANDLE", "handle_input", false));
    assert!(symbol_name_matches("HANDLE", "handle_input", true));
    assert!(symbol_name_matches("Handle_*", "handle_input", true));
}
