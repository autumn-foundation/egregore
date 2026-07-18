//! End-to-end tests for `eg query who-imports <module-path>` (issue #444):
//! a read-only importer lookup over extractor-minted Import nodes, with
//! segment-aware prefix matching and an explicit `--crate` unification boundary.
#![allow(missing_docs, clippy::doc_markdown)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SourceSpan,
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
