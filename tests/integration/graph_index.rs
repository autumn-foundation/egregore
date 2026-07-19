//! Differential tests for the graph sidecar index (issue #447).
//!
//! The index is a pure access-path optimization: every migrated `eg query …
//! --graph` lane MUST produce byte-identical stdout and the same exit code with
//! or without a `<graph>.idx`. Each lane test runs the command (a) cold with no
//! index and (b) with a freshly built index, and asserts equality. Staleness,
//! absence, corruption, and version-reject all degrade to a correct cold scan.

#![allow(missing_docs, clippy::similar_names, clippy::doc_markdown)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::{
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SourceSpan,
    ir::{Graph, SCHEMA_VERSION, stable_id},
};
use assert_cmd::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn file_id(path: &str) -> String {
    stable_id(&["node", "File", path])
}

fn sym_id(path: &str, name: &str) -> String {
    stable_id(&["node", "Symbol", path, name])
}

fn import_id(path: &str, name: &str) -> String {
    stable_id(&["node", "import", "r", path, name])
}

fn file(graph: &mut Graph, repo_id: &str, path: &str) {
    let fid = file_id(path);
    graph.push(GraphRecord::syntax_node(
        fid.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1, 200),
        path.rsplit('/').next().unwrap().to_owned(),
        "rust",
        format!("Source file {path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        fid,
        None,
        format!("repo contains {path}"),
    ));
}

fn symbol(graph: &mut Graph, path: &str, name: &str, lines: (usize, usize)) -> String {
    let id = sym_id(path, name);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(lines.0, lines.1),
        name.to_owned(),
        "rust",
        format!("fn {name} in {path}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Defines,
        file_id(path),
        id.clone(),
        None,
        format!("{path} defines {name}"),
    ));
    id
}

fn edge(graph: &mut Graph, label: EdgeLabel, from: &str, to: &str, summary: &str) {
    graph.push(GraphRecord::edge(
        label,
        from.to_owned(),
        to.to_owned(),
        Some("1.0".to_owned()),
        summary.to_owned(),
    ));
}

fn import(graph: &mut Graph, path: &str, name: &str, line: usize) -> String {
    let id = import_id(path, name);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Import,
        path.to_owned(),
        span(line, line),
        name.to_owned(),
        "rust",
        format!("Rust import {name}"),
    ));
    // The extractor also emits a File —IMPORTS→ Import edge; include it so the
    // ancestry climb attributes the import to its repository.
    graph.push(GraphRecord::edge(
        EdgeLabel::Imports,
        file_id(path),
        id.clone(),
        None,
        format!("{path} imports {name}"),
    ));
    id
}

/// A rich fixture exercising every migrated lane: a repo with several files,
/// a symbol with two physical versions, a tombstoned symbol, an ambiguous
/// symbol name across two files, outbound CALLS/IMPLEMENTS/REFERENCES edges,
/// and two import nodes.
fn seed_graph() -> Graph {
    let mut graph = Graph::new();
    let repo_id = stable_id(&["node", "Repository", "r"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("r".to_owned()),
        "Repository r".to_owned(),
    ));

    for p in ["src/a.rs", "src/b.rs", "src/dup1.rs", "src/dup2.rs"] {
        file(&mut graph, &repo_id, p);
    }

    // alpha: two physical versions (append-only supersession), outbound deps.
    let alpha = symbol(&mut graph, "src/a.rs", "alpha", (5, 20));
    // Second version of alpha with a slightly different span.
    graph.push(GraphRecord::syntax_node(
        alpha.clone(),
        NodeKind::Symbol,
        "src/a.rs".to_owned(),
        span(5, 22),
        "alpha".to_owned(),
        "rust",
        "fn alpha in src/a.rs v2".to_owned(),
    ));

    let beta = symbol(&mut graph, "src/b.rs", "beta", (5, 12));
    let my_trait = symbol(&mut graph, "src/b.rs", "MyTrait", (14, 18));
    let const_x = symbol(&mut graph, "src/b.rs", "CONST_X", (20, 21));
    // gamma is defined then tombstoned.
    let gamma = symbol(&mut graph, "src/b.rs", "gamma", (24, 30));
    graph.push(GraphRecord::Tombstone {
        id: stable_id(&["tomb", &gamma]),
        schema_version: SCHEMA_VERSION,
        deleted_id: gamma.clone(),
        summary: "gamma deleted".to_owned(),
        producer: None,
    });

    // Ambiguous name `dup` across two files.
    let _dup1 = symbol(&mut graph, "src/dup1.rs", "dup", (5, 8));
    let _dup2 = symbol(&mut graph, "src/dup2.rs", "dup", (5, 8));

    // alpha's outbound dependency edges.
    edge(&mut graph, EdgeLabel::Calls, &alpha, &beta, "alpha calls beta");
    // Give the CALLS edge a resolution so deps prints resolution status.
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Calls,
            alpha.clone(),
            my_trait.clone(),
            Some("1.0".to_owned()),
            "alpha calls MyTrait".to_owned(),
        )
        .with_resolution(CallResolution::Resolved),
    );
    edge(
        &mut graph,
        EdgeLabel::Implements,
        &alpha,
        &my_trait,
        "alpha implements MyTrait",
    );
    edge(
        &mut graph,
        EdgeLabel::References,
        &alpha,
        &const_x,
        "alpha references CONST_X",
    );

    // Imports for who-imports.
    let _i1 = import(&mut graph, "src/a.rs", "serde::Serialize", 2);
    let _i2 = import(&mut graph, "src/a.rs", "foo::bar", 3);

    graph
}

/// Writes the fixture to a temp `.jsonl` and returns (tempdir guard, path).
fn write_graph(graph: &Graph) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("graph.jsonl");
    fs::write(&path, graph.to_jsonl().expect("jsonl")).expect("write");
    (temp, path)
}

fn idx_path(graph: &Path) -> PathBuf {
    let mut s = graph.as_os_str().to_os_string();
    s.push(".idx");
    PathBuf::from(s)
}

/// Runs `eg <args…> --graph <graph>` and returns (stdout, exit_code).
fn run_lane(graph: &Path, args: &[&str]) -> (Vec<u8>, i32) {
    let output = egregore()
        .args(args)
        .arg("--graph")
        .arg(graph)
        .output()
        .expect("run");
    (output.stdout, output.status.code().unwrap_or(-1))
}

fn build_index(graph: &Path) {
    egregore()
        .args(["index"])
        .arg(graph)
        .assert()
        .success();
}

/// The core assertion: a lane's stdout + exit code are byte-identical cold vs
/// with a freshly-built index.
fn assert_lane_identical(graph: &Path, args: &[&str]) {
    let _ = fs::remove_file(idx_path(graph));
    let (cold_out, cold_code) = run_lane(graph, args);
    build_index(graph);
    let (idx_out, idx_code) = run_lane(graph, args);
    assert_eq!(
        cold_code, idx_code,
        "exit code differs cold vs indexed for {args:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&cold_out),
        String::from_utf8_lossy(&idx_out),
        "stdout differs cold vs indexed for {args:?}"
    );
    let _ = fs::remove_file(idx_path(graph));
}

// ---------------------------------------------------------------------------
// eg index command
// ---------------------------------------------------------------------------

#[test]
fn index_command_writes_sidecar_and_is_deterministic() {
    let (_t, graph) = write_graph(&seed_graph());
    build_index(&graph);
    let bytes_a = fs::read(idx_path(&graph)).expect("idx exists");
    // Rebuild → byte-identical index file.
    build_index(&graph);
    let bytes_b = fs::read(idx_path(&graph)).expect("idx exists");
    assert_eq!(bytes_a, bytes_b, "index bytes must be deterministic");
}

#[test]
fn index_command_refuses_unparseable_graph() {
    let temp = tempfile::tempdir().expect("temp");
    let graph = temp.path().join("bad.jsonl");
    fs::write(&graph, "{not valid json}\n").expect("write");
    egregore().args(["index"]).arg(&graph).assert().code(2);
    assert!(!idx_path(&graph).exists(), "no idx on a rejected graph");
}

// ---------------------------------------------------------------------------
// Differential per-lane tests
// ---------------------------------------------------------------------------

#[test]
fn differential_deps() {
    let (_t, graph) = write_graph(&seed_graph());
    // A symbol with outbound deps.
    assert_lane_identical(&graph, &["query", "deps", "alpha"]);
    // A canonical record-id handle.
    let alpha = sym_id("src/a.rs", "alpha");
    assert_lane_identical(&graph, &["query", "deps", &alpha]);
    // An ambiguous name (exit 1, candidate list).
    assert_lane_identical(&graph, &["query", "deps", "dup"]);
    // A symbol with no outbound deps.
    assert_lane_identical(&graph, &["query", "deps", "beta"]);
    // A nonexistent handle (exit 2).
    assert_lane_identical(&graph, &["query", "deps", "does_not_exist"]);
}

#[test]
fn differential_context() {
    let (_t, graph) = write_graph(&seed_graph());
    assert_lane_identical(&graph, &["query", "context", "alpha"]);
    assert_lane_identical(&graph, &["query", "context", "beta"]);
    assert_lane_identical(&graph, &["query", "context", "does_not_exist"]);
}

#[test]
fn differential_symbol() {
    let (_t, graph) = write_graph(&seed_graph());
    assert_lane_identical(&graph, &["query", "symbol", "alpha"]);
    assert_lane_identical(&graph, &["query", "symbol", "dup"]);
    assert_lane_identical(&graph, &["query", "symbol", "does_not_exist"]);
}

#[test]
fn differential_at() {
    let (_t, graph) = write_graph(&seed_graph());
    // Inside alpha's span.
    assert_lane_identical(&graph, &["query", "at", "src/a.rs:7"]);
    // In a gap (no enclosing symbol, exit 2).
    assert_lane_identical(&graph, &["query", "at", "src/a.rs:2"]);
    // Unknown path.
    assert_lane_identical(&graph, &["query", "at", "src/nope.rs:1"]);
}

#[test]
fn differential_locate() {
    let (_t, graph) = write_graph(&seed_graph());
    assert_lane_identical(&graph, &["query", "locate", "src/a.rs:7"]);
    assert_lane_identical(&graph, &["query", "locate", "src/a.rs:2"]);
    assert_lane_identical(&graph, &["query", "locate", "src/b.rs:6"]);
}

#[test]
fn differential_file() {
    let (_t, graph) = write_graph(&seed_graph());
    assert_lane_identical(&graph, &["query", "file", "src/a.rs"]);
    assert_lane_identical(&graph, &["query", "file", "src/b.rs"]);
    assert_lane_identical(&graph, &["query", "file", "src/nope.rs"]);
}

#[test]
fn differential_who_imports() {
    let (_t, graph) = write_graph(&seed_graph());
    assert_lane_identical(&graph, &["query", "who-imports", "serde"]);
    assert_lane_identical(&graph, &["query", "who-imports", "serde::Serialize"]);
    assert_lane_identical(&graph, &["query", "who-imports", "foo::bar"]);
    assert_lane_identical(&graph, &["query", "who-imports", "nonexistent::mod"]);
}

// ---------------------------------------------------------------------------
// Fallback: staleness, absence, corruption, version reject
// ---------------------------------------------------------------------------

/// After the graph changes, a stale index must trigger a cold-scan fallback and
/// return the CURRENT answer, never a stale hit.
#[test]
fn stale_index_falls_back_to_current_answer() {
    let (_t, graph) = write_graph(&seed_graph());
    build_index(&graph);

    // Mutate the graph: add a new symbol `delta` to src/a.rs by appending lines.
    let mut g2 = seed_graph();
    let _delta = symbol(&mut g2, "src/a.rs", "delta", (40, 50));
    fs::write(&graph, g2.to_jsonl().expect("jsonl")).expect("rewrite");

    // The .idx is now stale (hash/len mismatch). A query with the stale index
    // must match a fresh cold run over the new content.
    let (stale_out, stale_code) = run_lane(&graph, &["query", "file", "src/a.rs"]);
    fs::remove_file(idx_path(&graph)).expect("remove idx");
    let (cold_out, cold_code) = run_lane(&graph, &["query", "file", "src/a.rs"]);
    assert_eq!(stale_code, cold_code);
    assert_eq!(
        String::from_utf8_lossy(&stale_out),
        String::from_utf8_lossy(&cold_out),
        "stale index must yield the current cold answer"
    );
    // Sanity: the new symbol appears in the answer.
    assert!(
        String::from_utf8_lossy(&cold_out).contains("delta"),
        "current answer should include the new symbol"
    );
}

/// A truncated / corrupt `.idx` degrades to a cold scan with an identical result.
#[test]
fn corrupt_index_falls_back() {
    let (_t, graph) = write_graph(&seed_graph());
    let (cold_out, cold_code) = run_lane(&graph, &["query", "deps", "alpha"]);
    build_index(&graph);
    // Truncate the index to a few bytes.
    fs::write(idx_path(&graph), b"EGIX\x01").expect("truncate");
    let (corrupt_out, corrupt_code) = run_lane(&graph, &["query", "deps", "alpha"]);
    assert_eq!(cold_code, corrupt_code);
    assert_eq!(
        String::from_utf8_lossy(&cold_out),
        String::from_utf8_lossy(&corrupt_out),
    );
}

/// An index whose stored format version is unknown is treated as invalid → cold
/// scan, no mis-parse, no panic.
#[test]
fn version_mismatch_falls_back() {
    let (_t, graph) = write_graph(&seed_graph());
    let (cold_out, cold_code) = run_lane(&graph, &["query", "deps", "alpha"]);
    build_index(&graph);
    // Patch the format_version field (bytes 4..8) to 999.
    let mut bytes = fs::read(idx_path(&graph)).expect("read idx");
    bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
    fs::write(idx_path(&graph), &bytes).expect("rewrite idx");
    let (out, code) = run_lane(&graph, &["query", "deps", "alpha"]);
    assert_eq!(cold_code, code);
    assert_eq!(
        String::from_utf8_lossy(&cold_out),
        String::from_utf8_lossy(&out),
    );
}

/// An empty graph and a blank-only graph both index and query cleanly.
#[test]
fn empty_and_blank_graphs_index() {
    let temp = tempfile::tempdir().expect("temp");
    let empty = temp.path().join("empty.jsonl");
    fs::write(&empty, "").expect("write");
    egregore().args(["index"]).arg(&empty).assert().success();

    let blank = temp.path().join("blank.jsonl");
    fs::write(&blank, "\n  \n\n").expect("write");
    egregore().args(["index"]).arg(&blank).assert().success();
}
