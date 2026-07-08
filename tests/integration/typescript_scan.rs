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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typescript_basic")
}

fn shifted_fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typescript_basic_shifted")
}

const WIDGET_PATH: &str = "src/widget.ts";
const WIDGET_TEST_PATH: &str = "src/widget.test.ts";

#[test]
fn deterministic_typescript_scan_produces_stable_jsonl() {
    let repo = fixture_repo();
    let fixed_time = "2026-05-19T00:00:00Z";

    let first =
        scan_repository_at_with_override(&repo, fixed_time, Some("typescript-fixture-repo"))
            .expect("fixture repo should scan")
            .to_jsonl()
            .expect("graph should serialize");
    let second =
        scan_repository_at_with_override(&repo, fixed_time, Some("typescript-fixture-repo"))
            .expect("fixture repo should scan")
            .to_jsonl()
            .expect("graph should serialize");

    assert_eq!(first, second);
    assert!(first.contains(r#""kind":"Repository""#));
    assert!(first.contains(r#""kind":"File""#));
    assert!(first.contains(r#""repo_relative_path":"src/widget.ts""#));
    assert!(first.contains(r#""language":"typescript""#));

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
fn typescript_fixture_covers_common_symbols() {
    let repo = fixture_repo();
    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // Imports.
    assert_import(&records, r#"import { EventEmitter } from "events";"#);
    assert_import(&records, r#"import * as path from "path";"#);
    assert_import(&records, r#"import type { Readable } from "stream";"#);

    // Classes, functions, methods, variables, qualified by module path.
    assert_symbol(&records, "class", "src.widget.Base");
    assert_symbol(&records, "class", "src.widget.Widget");
    assert_symbol(&records, "method", "src.widget.Base.describe");
    assert_symbol(&records, "method", "src.widget.Widget.constructor");
    assert_symbol(&records, "method", "src.widget.Widget.run");
    assert_symbol(&records, "method", "src.widget.Widget.describe");
    assert_symbol(&records, "function", "src.widget.helper");
    assert_symbol(&records, "function", "src.widget.answer");
    assert_symbol(&records, "function", "src.widget.arrowHelper");
    assert_symbol(&records, "variable", "src.widget.LIMIT");
    assert_symbol(&records, "variable", "src.widget.NAME");

    // TypeScript-specific kinds.
    assert_symbol(&records, "interface", "src.widget.Describable");
    assert_symbol(&records, "type", "src.widget.WidgetId");
    assert_symbol(&records, "enum", "src.widget.WidgetKind");

    // Test symbols from the .test.ts file — stem is "widget.test" so module
    // path is src.widget.test.
    assert_symbol_in_path(
        &records,
        "test",
        "src.widget.test.testWidgetRuns",
        WIDGET_TEST_PATH,
    );
    assert_symbol_in_path(
        &records,
        "test",
        "src.widget.test.testAnswer",
        WIDGET_TEST_PATH,
    );
    assert_symbol_in_path(
        &records,
        "test",
        "src.widget.test.testHelper",
        WIDGET_TEST_PATH,
    );

    // Every TypeScript symbol carries the typescript language tag.
    assert!(
        records.iter().any(|record| record["kind"] == "Symbol"
            && record["language"] == "typescript"
            && record["name"] == "src.widget.Widget"),
        "Widget symbol should be tagged language typescript"
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
fn typescript_class_inheritance_emits_implements_edge() {
    let repo = fixture_repo();
    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // Widget extends Base → Implements edge from Widget to Base.
    let widget_id = symbol_id(&records, "class", "src.widget.Widget");
    let base_id = symbol_id(&records, "class", "src.widget.Base");
    let found = records.iter().any(|record| {
        record["record_type"] == "edge"
            && record["label"] == "IMPLEMENTS"
            && record["source"] == widget_id.as_str()
            && record["target"] == base_id.as_str()
    });
    assert!(found, "Widget should implement/extend Base");

    // Base implements Describable → Implements edge from Base to Describable.
    let describable_id = symbol_id(&records, "interface", "src.widget.Describable");
    let base_impl_found = records.iter().any(|record| {
        record["record_type"] == "edge"
            && record["label"] == "IMPLEMENTS"
            && record["source"] == base_id.as_str()
            && record["target"] == describable_id.as_str()
    });
    assert!(base_impl_found, "Base should implement Describable");
}

#[test]
fn typescript_symbol_ids_survive_non_semantic_byte_shift() {
    let fixed_time = "2026-05-19T00:00:00Z";
    let repo_id = Some("typescript-symbol-stability");
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
fn typescript_module_path_qualifies_symbols_from_repo_path() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    fs::create_dir_all(repo.join("pkg")).expect("pkg dir should be created");
    fs::write(repo.join("pkg/index.ts"), "export const VALUE = 1;\n")
        .expect("index.ts should be written");
    fs::write(
        repo.join("pkg/mod.ts"),
        "export function bar() { return 1; }\n",
    )
    .expect("mod.ts should write");
    fs::write(repo.join("top.ts"), "export function baz() { return 2; }\n")
        .expect("top.ts should write");

    let jsonl = scan_repository(repo)
        .expect("repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_symbol_in_path(&records, "function", "pkg.mod.bar", "pkg/mod.ts");
    assert_symbol_in_path(&records, "function", "top.baz", "top.ts");
    // index.ts resolves to the package itself: pkg.VALUE
    assert_symbol_in_path(&records, "variable", "pkg.VALUE", "pkg/index.ts");
}

#[test]
fn typescript_incremental_cache_reuses_unchanged_files() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir should be created");
    fs::write(
        repo.join("src/mod.ts"),
        "export function bar() { return 1; }\n",
    )
    .expect("mod.ts should write");
    let cache = temp.path().join("cache.json");

    let first = scan_repository_incremental(&repo, &cache).expect("first incremental scan");
    assert!(
        first.reused_files.is_empty(),
        "first scan reuses nothing: {:?}",
        first.reused_files
    );
    assert!(
        first.rebuilt_files.iter().any(|f| f == "src/mod.ts"),
        "first scan rebuilds the module: {:?}",
        first.rebuilt_files
    );

    let second = scan_repository_incremental(&repo, &cache).expect("second incremental scan");
    assert!(
        second.reused_files.iter().any(|f| f == "src/mod.ts"),
        "unchanged module should be reused: {:?}",
        second.reused_files
    );
}

#[test]
fn typescript_history_replay_emits_symbols_per_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let commits = seed_typescript_history_repo(&repo);

    let jsonl = scan_repository_history_with_override(&repo, Some("typescript-history"))
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

#[test]
fn typescript_producer_envelope_records_typescript_grammar() {
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
        components.get("tree_sitter_typescript").is_some(),
        "TypeScript graph should record the tree_sitter_typescript grammar: {components}"
    );
    assert!(
        components.get("tree_sitter_rust").is_none(),
        "TypeScript-only graph should not record the Rust grammar: {components}"
    );
}

#[test]
fn tsx_grammar_branch_emits_function_symbol() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    fs::write(
        repo.join("App.tsx"),
        "export function App(): JSX.Element { return <div>hello</div>; }\n",
    )
    .expect("App.tsx should be written");

    let jsonl = scan_repository(repo)
        .expect("repo with .tsx file should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_symbol_in_path(&records, "function", "App.App", "App.tsx");
}

#[test]
fn typescript_forward_referenced_base_emits_implements_edge() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    // Child is declared BEFORE Base — ensures deferred edge resolution is used.
    fs::write(
        repo.join("forward.ts"),
        "export class Child extends Base {}\nexport class Base {}\n",
    )
    .expect("forward.ts should be written");

    let jsonl = scan_repository(repo)
        .expect("repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // File is forward.ts → module prefix "forward", so classes qualify as forward.Child / forward.Base.
    let child_id = symbol_id(&records, "class", "forward.Child");
    let base_id = symbol_id(&records, "class", "forward.Base");
    let found = records.iter().any(|record| {
        record["record_type"] == "edge"
            && record["label"] == "IMPLEMENTS"
            && record["source"] == child_id.as_str()
            && record["target"] == base_id.as_str()
    });
    assert!(
        found,
        "Child should implement/extend Base even when declared before it"
    );
}

#[test]
fn typescript_namespace_members_qualify_with_namespace_prefix() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    fs::write(
        repo.join("ns.ts"),
        "export namespace Outer { export class Inner {} }\n",
    )
    .expect("ns.ts should be written");

    let jsonl = scan_repository(repo)
        .expect("repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // File is ns.ts → module prefix "ns", so symbols qualify as ns.Outer / ns.Outer.Inner.
    assert_symbol_in_path(&records, "namespace", "ns.Outer", "ns.ts");
    assert_symbol_in_path(&records, "class", "ns.Outer.Inner", "ns.ts");
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

fn seed_typescript_history_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "test@example.invalid"]);
    git(repo, ["config", "user.name", "Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write_file(
        repo,
        "src/widget.ts",
        "export class Widget { run() { return 1; } }\nexport function answer() { return new Widget().run(); }\n",
    );
    let first = commit_with_date(repo, "first", "2026-01-01T00:00:00Z");

    write_file(
        repo,
        "src/widget.ts",
        "export class Widget { run() { return 2; } }\nexport function answer() { return new Widget().run(); }\n",
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
