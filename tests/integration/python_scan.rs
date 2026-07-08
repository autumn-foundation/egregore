#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{
    incremental::scan_repository_incremental, scan_repository, scan_repository_at_with_override,
    scan_repository_history_with_override,
};
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_basic")
}

fn shifted_fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_basic_shifted")
}

const WIDGET_PATH: &str = "pkg/widget.py";

#[test]
fn deterministic_python_scan_produces_stable_jsonl() {
    let repo = fixture_repo();
    let fixed_time = "2026-05-19T00:00:00Z";

    let first = scan_repository_at_with_override(&repo, fixed_time, Some("python-fixture-repo"))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let second = scan_repository_at_with_override(&repo, fixed_time, Some("python-fixture-repo"))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");

    assert_eq!(first, second);
    assert!(first.contains(r#""kind":"Repository""#));
    assert!(first.contains(r#""kind":"File""#));
    assert!(first.contains(r#""repo_relative_path":"pkg/widget.py""#));
    assert!(first.contains(r#""language":"python""#));

    let manifest_dir = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
    let portable_jsonl = first.replace('\\', "/");
    assert!(
        !portable_jsonl.contains(&manifest_dir),
        "absolute manifest dir leaked into JSONL: {portable_jsonl}"
    );

    let lines = first.lines().collect::<Vec<_>>();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted, "JSONL records should be canonically ordered");
}

#[test]
fn python_fixture_covers_common_symbols() {
    let repo = fixture_repo();
    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // Imports.
    assert_import(&records, "from __future__ import annotations");
    assert_import(&records, "import os");
    assert_import(&records, "from collections import OrderedDict");

    // Classes, functions, methods, tests, and variables, qualified by module path.
    assert_symbol(&records, "class", "pkg.widget.Base");
    assert_symbol(&records, "class", "pkg.widget.Widget");
    assert_symbol(&records, "class", "pkg.widget.TestWidget");
    assert_symbol(&records, "method", "pkg.widget.Base.describe");
    assert_symbol(&records, "method", "pkg.widget.Widget.__init__");
    assert_symbol(&records, "method", "pkg.widget.Widget.run");
    assert_symbol(&records, "function", "pkg.widget.helper");
    assert_symbol(&records, "function", "pkg.widget.answer");
    assert_symbol(&records, "test", "pkg.widget.test_widget_runs");
    assert_symbol(&records, "test", "pkg.widget.TestWidget.test_answer");
    assert_symbol(&records, "variable", "pkg.widget.LIMIT");
    assert_symbol(&records, "variable", "pkg.widget.NAME");

    // Every Python symbol carries the python language tag.
    assert!(
        records.iter().any(|record| record["kind"] == "Symbol"
            && record["language"] == "python"
            && record["name"] == "pkg.widget.Widget"),
        "Widget symbol should be tagged language python"
    );

    assert_edge_label(&records, "CONTAINS");
    assert_edge_label(&records, "DEFINES");
    assert_edge_label(&records, "IMPORTS");
    assert_edge_label(&records, "IMPLEMENTS");
    assert_edge_label(&records, "CALLS");
    assert_edge_label(&records, "REFERENCES");
    assert_edges_point_to_existing_nodes(&records);
}

#[test]
fn python_subclass_emits_implements_edge_to_base() {
    let repo = fixture_repo();
    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    let widget_id = symbol_id(&records, "class", "pkg.widget.Widget");
    let base_id = symbol_id(&records, "class", "pkg.widget.Base");

    let found = records.iter().any(|record| {
        record["record_type"] == "edge"
            && record["label"] == "IMPLEMENTS"
            && record["source"] == widget_id.as_str()
            && record["target"] == base_id.as_str()
    });
    assert!(found, "Widget should implement/extend Base");
}

#[test]
fn python_symbol_ids_survive_non_semantic_byte_shift() {
    let fixed_time = "2026-05-19T00:00:00Z";
    let repo_id = Some("python-symbol-stability");
    let base = scan_repository_at_with_override(fixture_repo(), fixed_time, repo_id)
        .expect("base fixture should scan")
        .to_jsonl()
        .expect("base graph should serialize");
    let shifted = scan_repository_at_with_override(shifted_fixture_repo(), fixed_time, repo_id)
        .expect("shifted fixture should scan")
        .to_jsonl()
        .expect("shifted graph should serialize");

    let base_records = parse_jsonl(&base);
    let shifted_records = parse_jsonl(&shifted);
    let base_symbols = symbol_records_by_id_without_span(&base_records);
    let shifted_symbols = symbol_records_by_id_without_span(&shifted_records);

    assert!(!base_symbols.is_empty(), "fixture should emit symbols");
    assert_eq!(
        base_symbols.keys().collect::<BTreeSet<_>>(),
        shifted_symbols.keys().collect::<BTreeSet<_>>(),
        "prepended comments must not change Symbol record IDs"
    );
    assert_eq!(
        base_symbols, shifted_symbols,
        "after removing span, shifted fixture Symbol records should be identical"
    );

    let base_spans = symbol_spans_by_id(&base_records);
    let shifted_spans = symbol_spans_by_id(&shifted_records);
    assert!(
        base_spans
            .iter()
            .any(|(id, span)| shifted_spans.get(id) != Some(span)),
        "shifted fixture should move at least one Symbol span"
    );
}

#[test]
fn python_producer_envelope_records_python_grammar() {
    let jsonl = scan_repository(fixture_repo())
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    let repository = records
        .iter()
        .find(|record| record["kind"] == "Repository")
        .expect("graph should carry a Repository node");
    let components = &repository["producer"]["producer_components"];
    assert!(
        components.get("tree_sitter_python").is_some(),
        "Python graph should record the tree_sitter_python grammar: {components}"
    );
    assert!(
        components.get("tree_sitter_rust").is_none(),
        "Python-only graph should not record the Rust grammar: {components}"
    );
}

#[test]
fn python_module_path_qualifies_symbols_from_repo_path() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    fs::create_dir_all(repo.join("pkg")).expect("pkg dir should be created");
    fs::write(repo.join("pkg/__init__.py"), "VALUE = 1\n").expect("__init__.py should be written");
    fs::write(repo.join("pkg/mod.py"), "def bar():\n    return 1\n").expect("mod.py should write");
    fs::write(repo.join("top.py"), "def baz():\n    return 2\n").expect("top.py should write");

    let jsonl = scan_repository(repo)
        .expect("repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_symbol_in_path(&records, "function", "pkg.mod.bar", "pkg/mod.py");
    assert_symbol_in_path(&records, "function", "top.baz", "top.py");
    // `__init__.py` resolves to the package itself, so a binding there is `pkg.VALUE`.
    assert_symbol_in_path(&records, "variable", "pkg.VALUE", "pkg/__init__.py");
}

#[test]
fn python_incremental_cache_reuses_unchanged_files() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("pkg")).expect("pkg dir should be created");
    fs::write(repo.join("pkg/mod.py"), "def bar():\n    return 1\n").expect("mod.py should write");
    let cache = temp.path().join("cache.json");

    let first = scan_repository_incremental(&repo, &cache).expect("first incremental scan");
    assert!(
        first.reused_files.is_empty(),
        "first scan reuses nothing: {:?}",
        first.reused_files
    );
    assert!(
        first.rebuilt_files.iter().any(|f| f == "pkg/mod.py"),
        "first scan rebuilds the module: {:?}",
        first.rebuilt_files
    );

    let second = scan_repository_incremental(&repo, &cache).expect("second incremental scan");
    assert!(
        second.reused_files.iter().any(|f| f == "pkg/mod.py"),
        "unchanged module should be reused: {:?}",
        second.reused_files
    );
}

#[test]
fn python_history_replay_emits_symbols_per_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let commits = seed_python_history_repo(&repo);

    let jsonl = scan_repository_history_with_override(&repo, Some("python-history"))
        .expect("history fixture should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);

    let symbols_by_commit = symbol_ids_by_commit(&records);
    for commit in &commits {
        let symbols = symbols_by_commit
            .get(commit)
            .unwrap_or_else(|| panic!("commit {commit} should emit symbols"));
        assert!(!symbols.is_empty(), "commit {commit} should emit symbols");
    }
    assert_edges_point_to_existing_nodes(&records);
}

// ---- helpers ----

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn symbol_records_by_id_without_span(records: &[Value]) -> BTreeMap<String, Value> {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Symbol")
        .map(|record| {
            let id = record["id"].as_str().expect("symbol id").to_owned();
            let mut without_span = record.clone();
            without_span
                .as_object_mut()
                .expect("symbol record is an object")
                .remove("span");
            (id, without_span)
        })
        .collect()
}

fn symbol_spans_by_id(records: &[Value]) -> BTreeMap<String, Value> {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Symbol")
        .map(|record| {
            (
                record["id"].as_str().expect("symbol id").to_owned(),
                record["span"].clone(),
            )
        })
        .collect()
}

fn symbol_ids_by_commit(records: &[Value]) -> BTreeMap<String, BTreeSet<String>> {
    let mut by_commit: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for record in records {
        if record["record_type"] != "node" || record["kind"] != "Symbol" {
            continue;
        }
        let Some(commit) = record["temporal"]["git_commit"].as_str() else {
            continue;
        };
        let id = record["id"].as_str().expect("symbol id").to_owned();
        by_commit.entry(commit.to_owned()).or_default().insert(id);
    }
    by_commit
}

fn symbol_id(records: &[Value], symbol_kind: &str, name: &str) -> String {
    records
        .iter()
        .find(|record| {
            record["kind"] == "Symbol"
                && record["symbol_kind"] == symbol_kind
                && record["name"] == name
        })
        .and_then(|record| record["id"].as_str())
        .unwrap_or_else(|| panic!("missing {symbol_kind} symbol {name}"))
        .to_owned()
}

fn assert_edges_point_to_existing_nodes(records: &[Value]) {
    let node_ids = records
        .iter()
        .filter(|record| record["record_type"] == "node")
        .map(|record| record["id"].as_str().expect("node id").to_owned())
        .collect::<BTreeSet<_>>();

    for edge in records
        .iter()
        .filter(|record| record["record_type"] == "edge")
    {
        let source = edge["source"].as_str().expect("edge source");
        let target = edge["target"].as_str().expect("edge target");
        assert!(
            node_ids.contains(source),
            "edge source {source} should exist"
        );
        assert!(
            node_ids.contains(target),
            "edge target {target} should exist"
        );
    }
}

fn assert_import(records: &[Value], name: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Import"
            && record["name"] == name
            && record["repo_relative_path"] == WIDGET_PATH
            && record.get("span").is_some()
    });
    assert!(found, "missing import node named {name}");
}

fn assert_symbol(records: &[Value], symbol_kind: &str, name: &str) {
    assert_symbol_in_path(records, symbol_kind, name, WIDGET_PATH);
}

fn assert_symbol_in_path(records: &[Value], symbol_kind: &str, name: &str, path: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Symbol"
            && record["symbol_kind"] == symbol_kind
            && record["name"] == name
            && record["repo_relative_path"] == path
            && record.get("span").is_some()
    });
    assert!(found, "missing {symbol_kind} symbol named {name} in {path}");
}

fn assert_edge_label(records: &[Value], label: &str) {
    let found = records
        .iter()
        .any(|record| record["record_type"] == "edge" && record["label"] == label);
    assert!(found, "missing {label} edge");
}

fn seed_python_history_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "test@example.invalid"]);
    git(repo, ["config", "user.name", "Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write_file(
        repo,
        "pkg/widget.py",
        "class Widget:\n    def run(self):\n        return 1\n\n\ndef answer():\n    return Widget().run()\n",
    );
    let first = commit_with_date(repo, "first", "2026-01-01T00:00:00Z");

    write_file(
        repo,
        "pkg/widget.py",
        "class Widget:\n    def run(self):\n        return 2\n\n\ndef answer():\n    return Widget().run()\n",
    );
    let second = commit_with_date(repo, "second", "2026-01-02T00:00:00Z");

    [first, second]
}

fn write_file(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("file parent")).expect("parent dir should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit_with_date(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "."]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        status.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git command {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git command {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}
