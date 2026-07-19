//! End-to-end tests for `eg query who-imports <module-path>` (issue #444):
//! a read-only importer lookup over extractor-minted Import nodes, with
//! segment-aware prefix matching and an explicit `--crate` unification boundary.
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

fn import_id(path: &str, name: &str) -> String {
    stable_id(&["node", "import", "repo", path, name])
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

/// Pushes an Import node whose `name` carries the raw import path text, plus
/// the owning-file IMPORTS edge — exactly what the extractor mints.
fn import(graph: &mut Graph, path: &str, name: &str, line: usize) -> String {
    let id = import_id(path, name);
    graph.push(GraphRecord::syntax_node(
        id.clone(),
        NodeKind::Import,
        path.to_owned(),
        span(line),
        name.to_owned(),
        "rust",
        format!("Rust import {name}"),
    ));
    graph.push(GraphRecord::edge(
        EdgeLabel::Imports,
        file_id(path),
        id.clone(),
        None,
        format!("{path} imports {name}"),
    ));
    id
}

fn tombstone(graph: &mut Graph, deleted_id: &str) {
    graph.push(GraphRecord::Tombstone {
        id: format!("codegraph:v6:tomb_{deleted_id}"),
        schema_version: 6,
        deleted_id: deleted_id.to_owned(),
        summary: "removed".to_owned(),
        producer: None,
    });
}

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    deep_id: String,
}

fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("who_imports.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-who-imports"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-who-imports".to_owned()),
        "Repository repo-who-imports".to_owned(),
    ));

    for p in [
        "src/a.rs",
        "src/b.rs",
        "src/c.rs",
        "src/d.rs",
        "src/e.rs",
        "src/gone.rs",
    ] {
        file(&mut graph, &repo_id, p);
    }

    // A deep import that a `foo::bar` prefix query must match.
    let deep_id = import(&mut graph, "src/a.rs", "foo::bar::Baz", 3);
    // A sibling that must NEVER match `foo::bar` (segment boundary).
    import(&mut graph, "src/b.rs", "foo::barbell::Widget", 4);
    // An alias import matched on its path, not its alias.
    import(&mut graph, "src/c.rs", "serde::Serialize as S", 5);
    // A group import reduced to its common module prefix `foo::bar`.
    import(&mut graph, "src/d.rs", "foo::bar::{Qux, Quux}", 6);
    // An internal crate-relative import and an absolute crate import, for the
    // `--crate` unification test.
    import(&mut graph, "src/e.rs", "crate::widget::Thing", 7);
    import(&mut graph, "src/e.rs", "mycrate::widget::Other", 8);

    // A tombstoned import (excluded on both transports).
    let gone_id = import(&mut graph, "src/gone.rs", "foo::bar::Removed", 9);
    tombstone(&mut graph, &gone_id);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    Fixture {
        _temp: temp,
        graph: path,
        deep_id,
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
// Matching + citation handles + liveness.
// ---------------------------------------------------------------------------

#[test]
fn prefix_query_returns_deep_and_group_but_not_sibling_or_tombstoned() {
    let f = seed();
    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::bar",
        "--graph",
        f.graph.to_str().unwrap(),
    ]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["query_path"], "foo::bar");
    assert_eq!(header["total_importers"].as_u64(), Some(2));
    assert!(
        header["disclaimer"]
            .as_str()
            .unwrap()
            .contains("segment-aware"),
        "disclaimer states the matching contract"
    );

    let matched: Vec<&str> = rows
        .iter()
        .map(|r| r["import_path"].as_str().unwrap())
        .collect();
    // deep and group match; sibling `foo::barbell` and tombstoned
    // `foo::bar::Removed` do not.
    assert!(matched.contains(&"foo::bar::Baz"));
    assert!(matched.contains(&"foo::bar::{Qux, Quux}"));
    assert!(!matched.iter().any(|p| p.contains("barbell")));
    assert!(!matched.contains(&"foo::bar::Removed"));

    // Every row carries the citation handles: record ID + repo-relative path + span.
    for row in &rows {
        assert!(row["record_id"].as_str().is_some(), "record_id");
        assert!(
            row["repo_relative_path"].as_str().is_some(),
            "repo_relative_path"
        );
        assert!(row["span"].is_object(), "span");
        assert_eq!(row["trust"], "source_fact");
    }

    // The deep import is present exactly once (no append-order duplication).
    let deep_count = rows
        .iter()
        .filter(|r| r["record_id"].as_str() == Some(f.deep_id.as_str()))
        .count();
    assert_eq!(deep_count, 1);
}

#[test]
fn tombstoned_then_revived_import_is_live_over_graph() {
    // The shared latest-write-wins Liveness gate (issue #421) includes an Import
    // re-added after its own tombstone. Written in explicit append order (not
    // `Graph::to_jsonl`, which sorts) so the re-add follows the tombstone.
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
    file(&mut graph, &repo_id, "src/r.rs");
    let revived_id = import(&mut graph, "src/r.rs", "foo::bar::Back", 3);
    tombstone(&mut graph, &revived_id);
    import(&mut graph, "src/r.rs", "foo::bar::Back", 3);

    let jsonl = graph
        .records()
        .iter()
        .map(|r| serde_json::to_string(r).expect("serialize record"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{jsonl}\n")).expect("write fixture");

    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::bar",
        "--graph",
        path.to_str().unwrap(),
    ]);
    assert_eq!(header["total_importers"].as_u64(), Some(1));
    assert_eq!(rows[0]["record_id"].as_str(), Some(revived_id.as_str()));
}

#[test]
fn pub_use_re_export_site_is_found_end_to_end() {
    // The Rust extractor's `import_name` only trims a leading bare `use`, so a
    // `pub use crate::internal::Widget;` re-export keeps its visibility on the
    // Import node `name` (`pub use crate::internal::Widget`). The lane must
    // strip that keyword prefix before matching (#449, finding 1).
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("reexport.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-reexport"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-reexport".to_owned()),
        "Repository repo-reexport".to_owned(),
    ));
    file(&mut graph, &repo_id, "src/lib.rs");
    file(&mut graph, &repo_id, "src/vis.rs");
    // Public re-export whose extractor name retains the `pub use` prefix.
    import(
        &mut graph,
        "src/lib.rs",
        "pub use crate::internal::Widget",
        1,
    );
    // A `pub(crate) use` re-export keeps its restricted visibility too.
    import(
        &mut graph,
        "src/vis.rs",
        "pub(crate) use crate::internal::Helper",
        2,
    );

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "crate::internal",
        "--graph",
        path.to_str().unwrap(),
    ]);
    assert_eq!(header["total_importers"].as_u64(), Some(2));
    let matched: Vec<&str> = rows
        .iter()
        .map(|r| r["import_path"].as_str().unwrap())
        .collect();
    assert!(matched.contains(&"pub use crate::internal::Widget"));
    assert!(matched.contains(&"pub(crate) use crate::internal::Helper"));
}

#[test]
fn history_union_returns_import_removed_in_a_later_commit() {
    // A `scan-history` graph is the UNION of all commit snapshots: history
    // replay stamps per-commit Import records but never tombstones one removed
    // in a later commit, and this lane has no `--at`/`--as-of` and no HEAD-only
    // filter. So an import present only in an early commit is STILL returned by
    // an unpinned query — mirroring `deps`/`path` (#449, finding 2, documented
    // in docs/cli/who-imports.md).
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("history_union.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-history-union"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-history-union".to_owned()),
        "Repository repo-history-union".to_owned(),
    ));
    file(&mut graph, &repo_id, "src/old.rs");
    // Import present in an early commit and dropped later WITHOUT a tombstone —
    // exactly how history replay leaves a removed import in the union JSONL.
    let legacy_id = import(&mut graph, "src/old.rs", "foo::bar::Legacy", 1);

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");

    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::bar",
        "--graph",
        path.to_str().unwrap(),
    ]);
    assert_eq!(
        header["total_importers"].as_u64(),
        Some(1),
        "unpinned who-imports over a history union still returns a later-removed import"
    );
    assert_eq!(rows[0]["record_id"].as_str(), Some(legacy_id.as_str()));
}

#[test]
fn segment_boundary_never_bleeds_into_sibling() {
    let f = seed();
    // `foo::barbell` is a distinct module — querying it returns only the sibling.
    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::barbell",
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["total_importers"].as_u64(), Some(1));
    assert_eq!(rows[0]["import_path"], "foo::barbell::Widget");
}

#[test]
fn alias_import_matches_on_path_not_alias() {
    let f = seed();
    let (header, _) = run_ok(&[
        "query",
        "who-imports",
        "serde::Serialize",
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["total_importers"].as_u64(), Some(1));
}

// ---------------------------------------------------------------------------
// crate:: unification boundary.
// ---------------------------------------------------------------------------

#[test]
fn without_crate_flag_internal_and_external_forms_are_distinct() {
    let f = seed();
    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "mycrate::widget",
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(header["total_importers"].as_u64(), Some(1));
    assert_eq!(rows[0]["import_path"], "mycrate::widget::Other");
}

#[test]
fn crate_flag_unifies_internal_and_external_forms() {
    let f = seed();
    let (header, _) = run_ok(&[
        "query",
        "who-imports",
        "mycrate::widget",
        "--crate",
        "mycrate",
        "--graph",
        f.graph.to_str().unwrap(),
    ]);
    assert_eq!(
        header["total_importers"].as_u64(),
        Some(2),
        "--crate unifies crate::widget and mycrate::widget"
    );
    assert_eq!(header["crate_name"], "mycrate");
}

// ---------------------------------------------------------------------------
// Exit codes.
// ---------------------------------------------------------------------------

#[test]
fn well_formed_query_with_zero_importers_exits_2() {
    let f = seed();
    let assert = egregore()
        .args([
            "query",
            "who-imports",
            "nonexistent::module",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .code(2);
    let stdout = assert.get_output().stdout.clone();
    let out = String::from_utf8(stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(envelope["error"]["code"], "no_match");
}

#[test]
fn malformed_module_path_exits_1() {
    let f = seed();
    for bad in ["", "::foo", "foo::", "a::::b"] {
        let assert = egregore()
            .args([
                "query",
                "who-imports",
                bad,
                "--graph",
                f.graph.to_str().unwrap(),
            ])
            .assert()
            .code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            stderr.contains("malformed_module_path"),
            "expected malformed_module_path for {bad:?}, got {stderr:?}"
        );
    }
}

#[test]
fn eg_alias_runs_who_imports() {
    let f = seed();
    eg().args([
        "query",
        "who-imports",
        "foo::bar",
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
            "who-imports",
            "foo::bar",
            "--graph",
            f.graph.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let store_stdout = egregore()
        .args(["query", "who-imports", "foo::bar", "--data-dir"])
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
        .args(["query", "who-imports", "foo::bar", "--data-dir"])
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
//
// who-imports has NO --at/--as-of selector, so there is no commit-pinned
// corpus. Over a `scan-history` store carrying a `source_snapshot`, an unpinned
// query now DEFAULTS to HEAD-anchored (imports current at each repository's
// stamped HEAD); `--all-history` opts into the union, `--at-head` makes the
// default explicit.
// ---------------------------------------------------------------------------

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-02-01T00:00:00Z";

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

/// Pushes an Import node (with temporal provenance) plus its owning-file
/// IMPORTS edge, mirroring `scan-history` per-commit records.
fn hist_import(graph: &mut Graph, path: &str, name: &str, line: usize, commit: &str, vt: &str) {
    let id = import_id(path, name);
    graph.push(
        GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Import,
            path.to_owned(),
            span(line),
            name.to_owned(),
            "rust",
            format!("Rust import {name}"),
        )
        .with_temporal(temporal(commit, vt)),
    );
    graph.push(
        GraphRecord::edge(
            EdgeLabel::Imports,
            file_id(path),
            id,
            None,
            format!("{path} imports {name}"),
        )
        .with_temporal(temporal(commit, vt)),
    );
}

/// History fixture WITH a `source_snapshot` HEAD at c2 (`bbbb2222`).
///
/// `foo::bar::Kept` (src/keep.rs) is imported at HEAD (c2); `foo::bar::Legacy`
/// (src/old.rs) is imported only at c1 and removed at HEAD. Head-anchoring must
/// exclude the legacy import from the default/`--at-head` corpus but the
/// `--all-history` union keeps it. Returns `(temp, path)`.
fn seed_history_snapshot() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("who-history-snapshot.jsonl");
    let mut graph = Graph::new();

    let repo_id = stable_id(&["node", "Repository", "repo-wh427"]);
    graph.push(
        GraphRecord::node(
            repo_id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("repo-wh427".to_owned()),
            "Repository repo-wh427".to_owned(),
        )
        .with_source_snapshot(SourceSnapshotPayload {
            head: SnapshotHead::Commit {
                sha: "bbbb2222".to_owned(),
            },
            dirty: false,
            repository_id: repo_id.clone(),
            scanned_at: T2.to_owned(),
        }),
    );

    file(&mut graph, &repo_id, "src/keep.rs");
    file(&mut graph, &repo_id, "src/old.rs");

    hist_import(
        &mut graph,
        "src/keep.rs",
        "foo::bar::Kept",
        1,
        "bbbb2222",
        T2,
    );
    hist_import(
        &mut graph,
        "src/old.rs",
        "foo::bar::Legacy",
        1,
        "aaaa1111",
        T1,
    );

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

#[test]
fn all_history_flag_includes_import_removed_at_head() {
    let (_t, path) = seed_history_snapshot();
    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::bar",
        "--graph",
        path.to_str().unwrap(),
        "--all-history",
    ]);
    assert_eq!(header["corpus_mode"], "union");
    assert_eq!(header["corpus_mode_source"], "explicit_flag");
    assert_eq!(
        header["total_importers"].as_u64(),
        Some(2),
        "--all-history keeps the c1 import"
    );
    let matched: Vec<&str> = rows
        .iter()
        .map(|r| r["import_path"].as_str().unwrap())
        .collect();
    assert!(matched.contains(&"foo::bar::Kept"));
    assert!(matched.contains(&"foo::bar::Legacy"));
}

#[test]
fn default_head_anchors_and_excludes_import_removed_at_head() {
    let (_t, path) = seed_history_snapshot();
    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::bar",
        "--graph",
        path.to_str().unwrap(),
    ]);
    assert_eq!(
        header["corpus_mode"], "head_anchored",
        "a snapshot store defaults to head-anchored"
    );
    assert_eq!(header["corpus_mode_source"], "default");
    assert_eq!(
        header["total_importers"].as_u64(),
        Some(1),
        "the deleted-at-HEAD import is excluded under the default"
    );
    let matched: Vec<&str> = rows
        .iter()
        .map(|r| r["import_path"].as_str().unwrap())
        .collect();
    assert!(matched.contains(&"foo::bar::Kept"));
    assert!(
        !matched.contains(&"foo::bar::Legacy"),
        "legacy import removed at HEAD must not appear: {matched:?}"
    );
}

#[test]
fn at_head_flag_matches_default_and_is_explicit() {
    let (_t, path) = seed_history_snapshot();
    let (header, rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::bar",
        "--graph",
        path.to_str().unwrap(),
        "--at-head",
    ]);
    assert_eq!(header["corpus_mode"], "head_anchored");
    assert_eq!(
        header["corpus_mode_source"], "explicit_flag",
        "--at-head records an explicit selection"
    );
    assert_eq!(header["total_importers"].as_u64(), Some(1));
    let matched: Vec<&str> = rows
        .iter()
        .map(|r| r["import_path"].as_str().unwrap())
        .collect();
    assert!(!matched.contains(&"foo::bar::Legacy"), "{matched:?}");
}

#[test]
fn at_head_with_all_history_is_unsupported() {
    let (_t, path) = seed_history_snapshot();
    let assert = egregore()
        .args([
            "query",
            "who-imports",
            "foo::bar",
            "--graph",
            path.to_str().unwrap(),
            "--at-head",
            "--all-history",
        ])
        .assert()
        .failure()
        .code(1);
    let stdout = assert.get_output().stdout.clone();
    let out = String::from_utf8(stdout).expect("utf8");
    let value: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("envelope line")).expect("json envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "unsupported_combination");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_and_graph_agree_on_head_anchored_default() {
    let (_t, path) = seed_history_snapshot();
    let temp_db = tempfile::tempdir().expect("temp dir");
    let data_dir = temp_db.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let (graph_header, graph_rows) = run_ok(&[
        "query",
        "who-imports",
        "foo::bar",
        "--graph",
        path.to_str().unwrap(),
    ]);
    let store_stdout = egregore()
        .args(["query", "who-imports", "foo::bar", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let (store_header, store_rows) = parse_ndjson(&store_stdout);

    assert_eq!(graph_header["corpus_mode"], "head_anchored");
    assert_eq!(store_header["corpus_mode"], "head_anchored");
    let ids_of = |rows: &[serde_json::Value]| -> Vec<String> {
        let mut v: Vec<String> = rows
            .iter()
            .filter_map(|r| r["import_path"].as_str().map(str::to_owned))
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(
        ids_of(&graph_rows),
        ids_of(&store_rows),
        "graph and store must agree on HEAD state"
    );
    let matched = ids_of(&graph_rows);
    assert!(matched.contains(&"foo::bar::Kept".to_owned()));
    assert!(!matched.contains(&"foo::bar::Legacy".to_owned()));
}
