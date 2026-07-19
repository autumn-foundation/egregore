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
    CallResolution, EdgeLabel, GraphRecord, NodeKind, SnapshotHead, SourceSnapshotPayload,
    SourceSpan, TemporalMetadata,
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
    edge(
        &mut graph,
        EdgeLabel::Calls,
        &alpha,
        &beta,
        "alpha calls beta",
    );
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
    egregore().args(["index"]).arg(graph).assert().success();
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
// History-store composition with #457 HEAD-anchoring
//
// Over a `scan-history` graph, `deps`/`who-imports` HEAD-anchor by default and
// `path` reads the union — all consuming global commit topology / every record
// version that a targeted index closure cannot soundly supply. The sidecar
// loader detects a history store (temporal records / Commit nodes recorded in
// the index header) and falls back to the cold whole-file scan, so the indexed
// answer stays byte-identical to the cold answer. These fixtures make
// `corpus_mode` non-trivial (`head_anchored`) so that path is exercised.
// ---------------------------------------------------------------------------

const HT1: &str = "2026-01-01T00:00:00+00:00";
const HT2: &str = "2026-02-01T00:00:00+00:00";

fn hist_temporal(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

/// A `scan-history` fixture with a `source_snapshot` pinning HEAD to commit c2.
///
/// At c1 `anchor` calls `dep_gone` and imports `only_old::mod`; at c2 that call
/// and import are gone and `anchor` calls `dep_head` / imports `serde::Serialize`
/// instead. Head-anchoring (the #457 default) must therefore drop `dep_gone`
/// and `only_old::mod` from the default corpus. Returns (tempdir, graph path).
#[allow(clippy::too_many_lines)]
fn seed_history_graph() -> (tempfile::TempDir, PathBuf) {
    let mut graph = Graph::new();
    let repo_id = stable_id(&["node", "Repository", "rh"]);
    graph.push(
        GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("rh".to_owned()),
            "Repository rh".to_owned(),
        )
        .with_source_snapshot(SourceSnapshotPayload {
            head: SnapshotHead::Commit {
                sha: "bbbb2222".to_owned(),
            },
            dirty: false,
            repository_id: repo_id.clone(),
            scanned_at: HT2.to_owned(),
        }),
    );

    // Commits attributed to the repo via CONTAINS.
    let commit_in = |g: &mut Graph, sha: &str, parents: &[&str], vt: &str| {
        let cid = stable_id(&["node", "commit", "rh", sha]);
        g.push(
            GraphRecord::node(
                cid.clone(),
                NodeKind::Commit,
                None,
                None,
                Some(sha.to_owned()),
                format!("Commit {sha}"),
            )
            .with_temporal(hist_temporal(sha, parents, vt)),
        );
        g.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            cid,
            None,
            format!("repo contains commit {sha}"),
        ));
    };
    commit_in(&mut graph, "aaaa1111", &[], HT1);
    commit_in(&mut graph, "bbbb2222", &["aaaa1111"], HT2);

    // File attributed to the repo so owner_of resolves each symbol.
    file(&mut graph, &repo_id, "src/h.rs");

    let hist_symbol = |g: &mut Graph, name: &str, commit: &str, vt: &str| -> String {
        let id = sym_id("src/h.rs", name);
        g.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Symbol,
                "src/h.rs".to_owned(),
                span(1, 10),
                name.to_owned(),
                "rust",
                format!("fn {name}"),
            )
            .with_temporal(hist_temporal(commit, &[], vt)),
        );
        g.push(
            GraphRecord::edge(
                EdgeLabel::Defines,
                file_id("src/h.rs"),
                id.clone(),
                None,
                format!("src/h.rs defines {name}"),
            )
            .with_temporal(hist_temporal(commit, &[], vt)),
        );
        id
    };

    let anchor_id = hist_symbol(&mut graph, "anchor", "aaaa1111", HT1);
    let dep_gone_id = hist_symbol(&mut graph, "dep_gone", "aaaa1111", HT1);
    hist_symbol(&mut graph, "anchor", "bbbb2222", HT2);
    let dep_head_id = hist_symbol(&mut graph, "dep_head", "bbbb2222", HT2);

    let hist_call = |g: &mut Graph, from: &str, to: &str, commit: &str, vt: &str| {
        g.push(
            GraphRecord::edge(
                EdgeLabel::Calls,
                from.to_owned(),
                to.to_owned(),
                Some("1.0".to_owned()),
                "historical call".to_owned(),
            )
            .with_resolution(CallResolution::Resolved)
            .with_temporal(hist_temporal(commit, &[], vt)),
        );
    };
    hist_call(&mut graph, &anchor_id, &dep_gone_id, "aaaa1111", HT1);
    hist_call(&mut graph, &anchor_id, &dep_head_id, "bbbb2222", HT2);

    // Imports: one present only at c1 (dropped at HEAD), one present at HEAD.
    let hist_import = |g: &mut Graph, name: &str, commit: &str, vt: &str, line: usize| {
        let id = import_id("src/h.rs", name);
        g.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Import,
                "src/h.rs".to_owned(),
                span(line, line),
                name.to_owned(),
                "rust",
                format!("Rust import {name}"),
            )
            .with_temporal(hist_temporal(commit, &[], vt)),
        );
        g.push(
            GraphRecord::edge(
                EdgeLabel::Imports,
                file_id("src/h.rs"),
                id,
                None,
                format!("src/h.rs imports {name}"),
            )
            .with_temporal(hist_temporal(commit, &[], vt)),
        );
    };
    hist_import(&mut graph, "only_old::mod", "aaaa1111", HT1, 2);
    hist_import(&mut graph, "serde::Serialize", "bbbb2222", HT2, 2);

    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history.jsonl");
    fs::write(&path, graph.to_jsonl().expect("jsonl")).expect("write");
    (temp, path)
}

/// Parses the first NDJSON line of a lane's stdout as the summary/header object.
fn header_of(out: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(out);
    let first = text.lines().next().unwrap_or("{}");
    serde_json::from_str(first).unwrap_or(serde_json::Value::Null)
}

/// `deps` over a history store head-anchors by default (#457). The fixture's
/// `corpus_mode` must be `head_anchored`, and the indexed answer (loader falls
/// back to a cold scan for a history store) must be byte-identical to cold.
#[test]
fn differential_deps_history_head_anchored() {
    let (_t, graph) = seed_history_graph();
    // The fixture actually exercises the #457 head-anchor default.
    let _ = fs::remove_file(idx_path(&graph));
    let (cold_out, _) = run_lane(&graph, &["query", "deps", "anchor"]);
    let header = header_of(&cold_out);
    assert_eq!(
        header["corpus_mode"], "head_anchored",
        "history fixture must exercise the #457 head-anchor default; got {header}"
    );
    // Byte-identical cold vs indexed for the default and explicit corpus modes.
    assert_lane_identical(&graph, &["query", "deps", "anchor"]);
    assert_lane_identical(&graph, &["query", "deps", "anchor", "--all-history"]);
    assert_lane_identical(&graph, &["query", "deps", "anchor", "--at-head"]);
    assert_lane_identical(&graph, &["query", "deps", "dep_head"]);
}

/// `who-imports` over a history store head-anchors by default (#457). Indexed
/// (cold-scan fallback) must equal cold, including the dropped-at-HEAD import.
#[test]
fn differential_who_imports_history_head_anchored() {
    let (_t, graph) = seed_history_graph();
    let _ = fs::remove_file(idx_path(&graph));
    let (cold_out, _) = run_lane(&graph, &["query", "who-imports", "serde::Serialize"]);
    let header = header_of(&cold_out);
    assert_eq!(
        header["corpus_mode"], "head_anchored",
        "history who-imports must head-anchor by default; got {header}"
    );
    assert_lane_identical(&graph, &["query", "who-imports", "serde::Serialize"]);
    // `only_old::mod` exists only off-HEAD: head-anchored default drops it,
    // `--all-history` keeps it. Both must be byte-identical cold vs indexed.
    assert_lane_identical(&graph, &["query", "who-imports", "only_old::mod"]);
    assert_lane_identical(
        &graph,
        &["query", "who-imports", "only_old::mod", "--all-history"],
    );
    assert_lane_identical(&graph, &["query", "who-imports", "serde"]);
}

/// `path` is not migrated (always `Whole`) but must stay byte-identical over a
/// history store with or without an index.
#[test]
fn differential_path_history() {
    let (_t, graph) = seed_history_graph();
    assert_lane_identical(&graph, &["query", "path", "anchor", "dep_head"]);
    assert_lane_identical(
        &graph,
        &["query", "path", "anchor", "dep_gone", "--all-history"],
    );
}

/// Every migrated `--graph` lane must be byte-identical cold vs indexed over a
/// history store (the loader's cold-scan fallback for a history graph).
#[test]
fn differential_all_migrated_lanes_history() {
    let (_t, graph) = seed_history_graph();
    assert_lane_identical(&graph, &["query", "deps", "anchor"]);
    assert_lane_identical(&graph, &["query", "context", "anchor"]);
    assert_lane_identical(&graph, &["query", "symbol", "anchor"]);
    assert_lane_identical(&graph, &["query", "at", "src/h.rs:3"]);
    assert_lane_identical(&graph, &["query", "locate", "src/h.rs:3"]);
    assert_lane_identical(&graph, &["query", "file", "src/h.rs"]);
    assert_lane_identical(&graph, &["query", "who-imports", "serde::Serialize"]);
}

/// The index built over a history graph records the history signal, and its
/// presence never changes any migrated lane's answer (asserted above); this
/// pins the header-flag contract directly.
#[test]
fn history_graph_index_marks_temporal_history() {
    use aletheia_egregore::graph_index::GraphIndex;
    let (_t, graph) = seed_history_graph();
    let index = GraphIndex::build(&graph).expect("build index over history graph");
    assert!(
        index.body.has_temporal_history,
        "a scan-history graph must set has_temporal_history so the loader cold-scans"
    );

    // And a plain current-tree fixture must NOT set it (fast path stays on).
    let (_t2, plain) = write_graph(&seed_graph());
    let plain_index = GraphIndex::build(&plain).expect("build index over plain graph");
    assert!(
        !plain_index.body.has_temporal_history,
        "a plain scan graph keeps the #447 fast path"
    );
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

// ---------------------------------------------------------------------------
// Perf evidence (opt-in). Run with:
//   cargo test --test integration graph_index::perf_deps_cold_vs_indexed \
//     -- --ignored --nocapture
// Generates a large synthetic graph, times a deps/context query cold vs indexed,
// and writes the numbers to the scratchpad perf file.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "perf benchmark; opt-in via --ignored"]
fn perf_deps_cold_vs_indexed() {
    use std::time::Instant;

    // ~2,500 files × ~100 symbols = ~250k symbols; with files, edges, and a
    // repo the JSONL is well over 200k records.
    let files = 2_500usize;
    let per_file = 100usize;
    let mut graph = Graph::new();
    let repo_id = stable_id(&["node", "Repository", "big"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("big".to_owned()),
        "Repository big".to_owned(),
    ));
    let mut target = String::new();
    for f in 0..files {
        let path = format!("src/mod_{f}/file_{f}.rs");
        file(&mut graph, &repo_id, &path);
        let mut prev: Option<String> = None;
        for s in 0..per_file {
            let name = format!("sym_{f}_{s}");
            let id = symbol(&mut graph, &path, &name, (s * 3 + 1, s * 3 + 2));
            if let Some(p) = &prev {
                edge(&mut graph, EdgeLabel::Calls, p, &id, "calls");
            }
            prev = Some(id.clone());
            if f == files / 2 && s == per_file / 2 {
                target = name;
            }
        }
    }

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("perf447");
    let _ = fs::create_dir_all(&dir);
    let graph_path = dir.join("big.jsonl");
    let jsonl = graph.to_jsonl().expect("jsonl");
    let record_count = jsonl.lines().filter(|l| !l.trim().is_empty()).count();
    let byte_len = jsonl.len();
    fs::write(&graph_path, &jsonl).expect("write big graph");
    let _ = fs::remove_file(idx_path(&graph_path));

    let bin = env!("CARGO_BIN_EXE_egregore");
    let time_query = |label: &str, args: &[&str]| -> u128 {
        let start = Instant::now();
        let out = std::process::Command::new(bin)
            .args(args)
            .arg("--graph")
            .arg(&graph_path)
            .output()
            .expect("run");
        let ms = start.elapsed().as_millis();
        assert!(
            out.status.success(),
            "{label} query failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        ms
    };

    // Cold (no index).
    let cold_deps = time_query("cold deps", &["query", "deps", &target]);
    let cold_ctx = time_query("cold context", &["query", "context", &target]);

    // Build the index (timed).
    let build_start = Instant::now();
    std::process::Command::new(bin)
        .args(["index"])
        .arg(&graph_path)
        .output()
        .expect("index");
    let build_ms = build_start.elapsed().as_millis();
    let idx_len = fs::metadata(idx_path(&graph_path)).expect("idx").len();

    // Indexed.
    let idx_deps = time_query("idx deps", &["query", "deps", &target]);
    let idx_ctx = time_query("idx context", &["query", "context", &target]);

    let report = format!(
        "# Issue #447 — sidecar index perf evidence\n\n\
         Synthetic graph: {record_count} records, {byte_len} bytes \
         ({files} files × {per_file} symbols).\n\
         Index build: {build_ms} ms, index size {idx_len} bytes.\n\n\
         | query | cold (ms) | indexed (ms) |\n\
         |-------|-----------|--------------|\n\
         | deps <symbol> | {cold_deps} | {idx_deps} |\n\
         | context <symbol> | {cold_ctx} | {idx_ctx} |\n\n\
         (Whole-process wall time including startup; the delta is the \
         load/deserialize cost the index removes.)\n"
    );
    let perf_path = "/tmp/claude-0/-home-user-egregore/\
                     f35ef088-ad5e-51cc-bddb-474709a26a05/scratchpad/447-perf.md";
    let _ = fs::write(perf_path, &report);
    println!("{report}");
    let _ = fs::remove_file(&graph_path);
    let _ = fs::remove_file(idx_path(&graph_path));
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
