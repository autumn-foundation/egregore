//! End-to-end tests for `eg query who-constructs <Type>` (issue #471): a
//! read-only, type-anchored lookup over the extractor-minted `CONSTRUCTS` edges
//! (PR #467 / issue #443). The inbound mirror of `deps` and the symmetric
//! partner to `who-imports`; each row carries the `e0063_risk` blast-radius flag.
#![allow(missing_docs, clippy::doc_markdown)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SnapshotHead, SourceSnapshotPayload, SourceSpan,
    TemporalMetadata,
    ir::{Graph, stable_id},
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn eg() -> Command {
    Command::cargo_bin("eg").expect("eg binary should run")
}

const fn span(start_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 40,
        start_line,
        end_line: start_line,
    }
}

fn file_id(path: &str) -> String {
    stable_id(&["node", "File", path])
}

fn sym_id(path: &str, name: &str) -> String {
    stable_id(&["node", "Symbol", path, name])
}

fn file(graph: &mut Graph, repo_id: &str, path: &str) {
    let fid = file_id(path);
    graph.push(GraphRecord::syntax_node(
        fid.clone(),
        NodeKind::File,
        path.to_owned(),
        span(1),
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

/// Pushes a Symbol node and its owning-file DEFINES edge, returning its id.
fn symbol(graph: &mut Graph, path: &str, name: &str, line: usize) -> String {
    let id = sym_id(path, name);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Symbol,
        path.to_owned(),
        span(line),
        name.to_owned(),
        "rust",
        format!("symbol {name}"),
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

/// Pushes a `CONSTRUCTS` edge (constructor → type) with an exhaustiveness marker.
fn constructs(graph: &mut Graph, constructor: &str, type_def: &str, exhaustive: bool) -> String {
    let edge = GraphRecord::edge(
        EdgeLabel::Constructs,
        constructor.to_owned(),
        type_def.to_owned(),
        None,
        "constructs".to_owned(),
    )
    .with_construct_exhaustive(exhaustive);
    let id = edge.id().to_owned();
    graph.push(edge);
    id
}

fn tombstone(graph: &mut Graph, deleted_id: &str) {
    graph.push(GraphRecord::Tombstone {
        id: format!("codegraph:v7:tomb_{deleted_id}"),
        schema_version: 7,
        deleted_id: deleted_id.to_owned(),
        summary: "removed".to_owned(),
        producer: None,
    });
}

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    deal_id: String,
    exhaustive_ctor: String,
    fru_ctor: String,
}

/// A single-snapshot (no temporal) fixture. One type `Deal` with two
/// constructors — an exhaustive literal and a `..base` FRU literal — plus an
/// unrelated type `Lonely` with no constructors, and a tombstoned constructor.
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("who_constructs.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-who-constructs"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-who-constructs".to_owned()),
        "Repository repo-who-constructs".to_owned(),
    ));

    for p in [
        "crates/core/src/model.rs",
        "crates/api/src/build.rs",
        "crates/cli/src/build.rs",
        "crates/core/src/gone.rs",
    ] {
        file(&mut graph, &repo_id, p);
    }

    // The anchor type, defined in the core crate.
    let deal_id = symbol(&mut graph, "crates/core/src/model.rs", "Deal", 1);
    // An unrelated type nobody constructs.
    symbol(&mut graph, "crates/core/src/model.rs", "Lonely", 20);

    // A cross-file, cross-crate constructor using the exhaustive literal form.
    let exhaustive_ctor = symbol(&mut graph, "crates/api/src/build.rs", "make_deal", 10);
    constructs(&mut graph, &exhaustive_ctor, &deal_id, true);
    // A different-crate constructor using the `..base` FRU form.
    let fru_ctor = symbol(&mut graph, "crates/cli/src/build.rs", "update_deal", 12);
    constructs(&mut graph, &fru_ctor, &deal_id, false);

    // A tombstoned constructor — excluded on both transports.
    let gone_ctor = symbol(&mut graph, "crates/core/src/gone.rs", "old_deal", 3);
    constructs(&mut graph, &gone_ctor, &deal_id, true);
    tombstone(&mut graph, &gone_ctor);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    Fixture {
        _temp: temp,
        graph: path,
        deal_id,
        exhaustive_ctor,
        fru_ctor,
    }
}

fn parse_ndjson(stdout: &[u8]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let out = String::from_utf8(stdout.to_vec()).expect("utf8");
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let header: serde_json::Value =
        serde_json::from_str(lines.next().expect("header line")).expect("header is JSON");
    let rows: Vec<serde_json::Value> = lines
        .map(|l| serde_json::from_str(l).expect("row is JSON"))
        .collect();
    (header, rows)
}

fn run_ok(args: &[&str]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let stdout = egregore()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    parse_ndjson(&stdout)
}

// ---------------------------------------------------------------------------
// Core: matching, e0063_risk, citation handles, cross-crate, determinism.
// ---------------------------------------------------------------------------

#[test]
fn lists_cross_crate_constructors_with_e0063_flags() {
    let f = seed();
    let (header, rows) = run_ok(&[
        "query",
        "who-constructs",
        "Deal",
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["handle"], "Deal");
    assert_eq!(header["direction"], "inbound");
    assert_eq!(header["edge_label"], "CONSTRUCTS");
    assert_eq!(header["target"]["name"], "Deal");
    assert_eq!(
        header["target"]["record_id"].as_str(),
        Some(f.deal_id.as_str())
    );
    // No temporal / no snapshot → single-snapshot corpus disclosure.
    assert_eq!(header["corpus_mode"], "single_snapshot");
    assert_eq!(header["corpus_mode_source"], "default");
    assert_eq!(header["total_constructors"].as_u64(), Some(2));

    // Two constructors from two different crates; the tombstoned one is gone.
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"make_deal"));
    assert!(names.contains(&"update_deal"));
    assert!(
        !names.contains(&"old_deal"),
        "tombstoned constructor excluded"
    );

    // Every row carries the citation handles + the correct e0063_risk flag.
    for row in &rows {
        assert!(row["record_id"].as_str().is_some(), "record_id");
        assert!(row["repo_relative_path"].as_str().is_some(), "path");
        assert!(row["span"].is_object(), "span");
        assert!(row["edge_record_id"].as_str().is_some(), "edge handle");
        assert_eq!(row["trust"], "source_fact");
    }
    let exhaustive = rows
        .iter()
        .find(|r| r["record_id"].as_str() == Some(f.exhaustive_ctor.as_str()))
        .expect("exhaustive row");
    assert_eq!(exhaustive["e0063_risk"], true);
    assert_eq!(exhaustive["is_exhaustive"], true);
    let fru = rows
        .iter()
        .find(|r| r["record_id"].as_str() == Some(f.fru_ctor.as_str()))
        .expect("fru row");
    assert_eq!(fru["e0063_risk"], false);
    assert_eq!(fru["is_exhaustive"], false);
}

#[test]
fn resolves_by_canonical_record_id() {
    let f = seed();
    let (header, rows) = run_ok(&[
        "query",
        "who-constructs",
        &f.deal_id,
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["total_constructors"].as_u64(), Some(2));
    assert_eq!(rows.len(), 2);
}

#[test]
fn output_is_byte_identical_across_runs() {
    let f = seed();
    let args = [
        "query",
        "who-constructs",
        "Deal",
        "--graph",
        f.graph.to_str().unwrap(),
    ];
    let first = egregore()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second = egregore()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(first, second, "output must be byte-identical across runs");
}

// ---------------------------------------------------------------------------
// Liveness: tombstone + revive.
// ---------------------------------------------------------------------------

#[test]
fn tombstoned_then_revived_constructor_is_live_over_graph() {
    // Latest-write-wins (issue #421): a constructor re-added after its own
    // tombstone is live. Written in explicit append order so the re-add follows
    // the tombstone (Graph::to_jsonl would sort and lose the ordering).
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("revived.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-revived"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-revived".to_owned()),
        "Repository repo-revived".to_owned(),
    ));
    file(&mut graph, &repo_id, "src/model.rs");
    file(&mut graph, &repo_id, "src/build.rs");
    let deal = symbol(&mut graph, "src/model.rs", "Deal", 1);
    let ctor = symbol(&mut graph, "src/build.rs", "make", 3);
    constructs(&mut graph, &ctor, &deal, true);
    tombstone(&mut graph, &ctor);
    // Re-add the constructor node after its tombstone.
    symbol(&mut graph, "src/build.rs", "make", 3);

    let jsonl = graph
        .records()
        .iter()
        .map(|r| serde_json::to_string(r).expect("serialize record"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{jsonl}\n")).expect("write fixture");

    let (header, rows) = run_ok(&[
        "query",
        "who-constructs",
        "Deal",
        "--graph",
        path.to_str().unwrap(),
    ]);
    assert_eq!(header["total_constructors"].as_u64(), Some(1));
    assert_eq!(rows[0]["record_id"].as_str(), Some(ctor.as_str()));
}

// ---------------------------------------------------------------------------
// Exit codes.
// ---------------------------------------------------------------------------

#[test]
fn resolved_type_with_zero_constructors_exits_2() {
    let f = seed();
    let assert = egregore()
        .args([
            "query",
            "who-constructs",
            "Lonely",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(envelope["error"]["code"], "no_match");
}

#[test]
fn unknown_type_exits_2() {
    let f = seed();
    egregore()
        .args([
            "query",
            "who-constructs",
            "NoSuchType",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

#[test]
fn empty_handle_exits_1_malformed() {
    let f = seed();
    let assert = egregore()
        .args([
            "query",
            "who-constructs",
            "",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("malformed_type_handle"),
        "expected malformed_type_handle, got {stderr:?}"
    );
}

#[test]
fn ambiguous_same_named_type_exits_1_with_candidates() {
    // Two `Widget` type symbols in one repo → the name is ambiguous; all
    // candidate record IDs are reported (mirrors deps).
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("ambiguous.jsonl");
    let mut graph = Graph::new();
    let repo_id = stable_id(&["node", "Repository", "repo-ambiguous"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-ambiguous".to_owned()),
        "Repository repo-ambiguous".to_owned(),
    ));
    file(&mut graph, &repo_id, "src/a.rs");
    file(&mut graph, &repo_id, "src/b.rs");
    symbol(&mut graph, "src/a.rs", "Widget", 1);
    symbol(&mut graph, "src/b.rs", "Widget", 1);
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    let assert = egregore()
        .args([
            "query",
            "who-constructs",
            "Widget",
            "--graph",
            path.to_str().unwrap(),
        ])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("Ambiguous") || stderr.contains("candidates"),
        "expected ambiguity report, got {stderr:?}"
    );
}

#[test]
fn file_handle_is_unsupported_exit_1() {
    let f = seed();
    let assert = egregore()
        .args([
            "query",
            "who-constructs",
            "crates/core/src/model.rs",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("Unsupported") || stderr.contains("who-constructs accepts only"),
        "expected unsupported handle, got {stderr:?}"
    );
}

#[test]
fn eg_alias_runs_who_constructs() {
    let f = seed();
    eg().args([
        "query",
        "who-constructs",
        "Deal",
        "--graph",
        f.graph.to_str().unwrap(),
    ])
    .assert()
    .success();
}

// ---------------------------------------------------------------------------
// --data-dir parity + read-only store guarantee (embedded feature).
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
fn snapshot_tree(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).expect("read dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).expect("read file")));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_parity_with_graph() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&f.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let graph_stdout = egregore()
        .args([
            "query",
            "who-constructs",
            "Deal",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let store_stdout = egregore()
        .args(["query", "who-constructs", "Deal", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        graph_stdout, store_stdout,
        "graph and store views must be byte-identical"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_query_is_read_only() {
    let f = seed();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&f.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let before = snapshot_tree(&data_dir);
    egregore()
        .args(["query", "who-constructs", "Deal", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    let after = snapshot_tree(&data_dir);
    assert_eq!(
        before, after,
        "querying the embedded store must not create, modify, or delete any store file"
    );
}

// ---------------------------------------------------------------------------
// Issue #427 — corpus-mode default (HEAD-anchored) vs opt-in union.
// ---------------------------------------------------------------------------

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-02-01T00:00:00Z";
const C1: &str = "aaaa1111";
const C2: &str = "bbbb2222";

fn temporal(commit: &str, vt: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: vt.to_owned(),
        author_time: Some(vt.to_owned()),
        observed_at: vt.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

/// Pushes a Symbol (with temporal provenance) + its owning-file DEFINES edge.
fn hist_symbol(
    graph: &mut Graph,
    path: &str,
    name: &str,
    line: usize,
    commit: &str,
    vt: &str,
) -> String {
    let id = sym_id(path, name);
    graph.push(
        GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Symbol,
            path.to_owned(),
            span(line),
            name.to_owned(),
            "rust",
            format!("symbol {name}"),
        )
        .with_temporal(temporal(commit, vt)),
    );
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Defines,
            file_id(path),
            id.clone(),
            None,
            format!("{path} defines {name}"),
        )
        .with_temporal(temporal(commit, vt)),
    );
    id
}

fn hist_constructs(
    graph: &mut Graph,
    constructor: &str,
    type_def: &str,
    exhaustive: bool,
    commit: &str,
    vt: &str,
) {
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Constructs,
            constructor.to_owned(),
            type_def.to_owned(),
            None,
            "constructs".to_owned(),
        )
        .with_construct_exhaustive(exhaustive)
        .with_temporal(temporal(commit, vt)),
    );
}

/// History fixture WITH a `source_snapshot` HEAD at c2. `Deal` and `keep_ctor`
/// live at HEAD (c2); `legacy_ctor` constructs `Deal` only at c1 and is removed
/// at HEAD. Head-anchoring excludes the legacy site; `--all-history` keeps it.
fn seed_history_snapshot() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("who-constructs-history.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-wc427"]);
    graph.push(
        GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("repo-wc427".to_owned()),
            "Repository repo-wc427".to_owned(),
        )
        .with_source_snapshot(SourceSnapshotPayload {
            head: SnapshotHead::Commit { sha: C2.to_owned() },
            dirty: false,
            repository_id: repo_id.clone(),
            scanned_at: T2.to_owned(),
        }),
    );

    file(&mut graph, &repo_id, "src/model.rs");
    file(&mut graph, &repo_id, "src/keep.rs");
    file(&mut graph, &repo_id, "src/old.rs");

    // The anchor type, current at HEAD.
    let deal = hist_symbol(&mut graph, "src/model.rs", "Deal", 1, C2, T2);
    // A constructor current at HEAD.
    let keep_ctor = hist_symbol(&mut graph, "src/keep.rs", "keep_ctor", 2, C2, T2);
    hist_constructs(&mut graph, &keep_ctor, &deal, true, C2, T2);
    // A constructor present only at c1, removed at HEAD (no tombstone — exactly
    // how history replay leaves a removed record in the union JSONL).
    let legacy_ctor = hist_symbol(&mut graph, "src/old.rs", "legacy_ctor", 2, C1, T1);
    hist_constructs(&mut graph, &legacy_ctor, &deal, false, C1, T1);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

#[test]
fn default_head_anchors_and_excludes_site_removed_at_head() {
    let (_t, path) = seed_history_snapshot();
    let (header, rows) = run_ok(&[
        "query",
        "who-constructs",
        "Deal",
        "--graph",
        path.to_str().unwrap(),
    ]);
    assert_eq!(header["corpus_mode"], "head_anchored");
    assert_eq!(header["corpus_mode_source"], "default");
    assert_eq!(header["total_constructors"].as_u64(), Some(1));
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"keep_ctor"));
    assert!(
        !names.contains(&"legacy_ctor"),
        "site removed at HEAD must not appear: {names:?}"
    );
}

#[test]
fn all_history_flag_includes_site_removed_at_head() {
    let (_t, path) = seed_history_snapshot();
    let (header, rows) = run_ok(&[
        "query",
        "who-constructs",
        "Deal",
        "--graph",
        path.to_str().unwrap(),
        "--all-history",
    ]);
    assert_eq!(header["corpus_mode"], "union");
    assert_eq!(header["corpus_mode_source"], "explicit_flag");
    assert_eq!(header["total_constructors"].as_u64(), Some(2));
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"keep_ctor"));
    assert!(names.contains(&"legacy_ctor"));
}

#[test]
fn at_head_flag_is_explicit() {
    let (_t, path) = seed_history_snapshot();
    let (header, _rows) = run_ok(&[
        "query",
        "who-constructs",
        "Deal",
        "--graph",
        path.to_str().unwrap(),
        "--at-head",
    ]);
    assert_eq!(header["corpus_mode"], "head_anchored");
    assert_eq!(header["corpus_mode_source"], "explicit_flag");
    assert_eq!(header["total_constructors"].as_u64(), Some(1));
}

#[test]
fn at_head_with_all_history_is_unsupported() {
    let (_t, path) = seed_history_snapshot();
    let assert = egregore()
        .args([
            "query",
            "who-constructs",
            "Deal",
            "--graph",
            path.to_str().unwrap(),
            "--at-head",
            "--all-history",
        ])
        .assert()
        .failure()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let value: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("envelope line")).expect("json envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "unsupported_combination");
}
