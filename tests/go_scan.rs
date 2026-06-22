#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use aletheia_egregore::{
    incremental::scan_repository_incremental, scan_repository, scan_repository_at_with_override,
};
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go_basic")
}

fn shifted_fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go_basic_shifted")
}

const WIDGET_PATH: &str = "widget/widget.go";
const WIDGET_TEST_PATH: &str = "widget/widget_test.go";

#[test]
fn deterministic_go_scan_produces_stable_jsonl() {
    let repo = fixture_repo();
    let fixed_time = "2026-05-19T00:00:00Z";

    let first = scan_repository_at_with_override(&repo, fixed_time, Some("go-fixture-repo"))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let second = scan_repository_at_with_override(&repo, fixed_time, Some("go-fixture-repo"))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");

    assert_eq!(first, second);
    assert!(first.contains(r#""kind":"Repository""#));
    assert!(first.contains(r#""kind":"File""#));
    assert!(first.contains(r#""repo_relative_path":"widget/widget.go""#));
    assert!(first.contains(r#""language":"go""#));

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
fn go_fixture_covers_common_symbols() {
    let repo = fixture_repo();
    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // Imports, one node per import spec (including grouped blocks).
    assert_import(&records, r#""fmt""#);
    assert_import(&records, r#""strings""#);

    // Package-level values, qualified by the package directory.
    assert_symbol(&records, "const", "widget.LIMIT");
    assert_symbol(&records, "var", "widget.NAME");

    // Types: struct, interface, and a defined type.
    assert_symbol(&records, "struct", "widget.Base");
    assert_symbol(&records, "struct", "widget.Widget");
    assert_symbol(&records, "interface", "widget.Describable");
    assert_symbol(&records, "interface", "widget.Reader");
    assert_symbol(&records, "type", "widget.WidgetID");

    // Functions and methods (methods qualify by receiver type).
    assert_symbol(&records, "function", "widget.helper");
    assert_symbol(&records, "function", "widget.Answer");
    assert_symbol(&records, "method", "widget.Base.Describe");
    assert_symbol(&records, "method", "widget.Widget.Run");
    assert_symbol(&records, "method", "widget.Widget.Describe");

    // Test functions in the *_test.go file are tagged as test symbols.
    assert_symbol_in_path(&records, "test", "widget.TestAnswer", WIDGET_TEST_PATH);
    assert_symbol_in_path(&records, "test", "widget.TestHelper", WIDGET_TEST_PATH);

    // Every Go symbol carries the go language tag.
    assert!(
        records.iter().any(|record| record["kind"] == "Symbol"
            && record["language"] == "go"
            && record["name"] == "widget.Widget"),
        "Widget symbol should be tagged language go"
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
fn go_embedding_emits_implements_edges() {
    let repo = fixture_repo();
    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // Struct embedding: Widget embeds Base → Implements edge from Widget to Base.
    let widget_id = symbol_id(&records, "struct", "widget.Widget");
    let base_id = symbol_id(&records, "struct", "widget.Base");
    assert!(
        has_edge(&records, "IMPLEMENTS", &widget_id, &base_id),
        "Widget should embed Base"
    );

    // Interface embedding: Reader embeds Describable.
    let reader_id = symbol_id(&records, "interface", "widget.Reader");
    let describable_id = symbol_id(&records, "interface", "widget.Describable");
    assert!(
        has_edge(&records, "IMPLEMENTS", &reader_id, &describable_id),
        "Reader should embed Describable"
    );
}

#[test]
fn go_symbol_ids_survive_non_semantic_byte_shift() {
    let fixed_time = "2026-05-19T00:00:00Z";
    let repo_id = Some("go-symbol-stability");
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
fn go_module_path_is_directory_scoped() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    fs::create_dir_all(repo.join("pkg")).expect("pkg dir should be created");
    fs::write(
        repo.join("pkg/mod.go"),
        "package pkg\n\nfunc Bar() int { return 1 }\n",
    )
    .expect("mod.go should write");
    fs::write(
        repo.join("top.go"),
        "package main\n\nfunc Baz() int { return 2 }\n",
    )
    .expect("top.go should write");

    let jsonl = scan_repository(repo)
        .expect("repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // Package directory qualifies symbols; the filename is not part of the name.
    assert_symbol_in_path(&records, "function", "pkg.Bar", "pkg/mod.go");
    // A root-level file has no package prefix.
    assert_symbol_in_path(&records, "function", "Baz", "top.go");
}

#[test]
fn go_forward_referenced_embedding_emits_implements_edge() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    // Widget embeds Base, declared BEFORE Base, to exercise deferred resolution.
    fs::write(
        repo.join("forward.go"),
        "package main\n\ntype Widget struct {\n\tBase\n}\n\ntype Base struct{}\n",
    )
    .expect("forward.go should write");

    let jsonl = scan_repository(repo)
        .expect("repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    // Root-level file → no package prefix, so names are bare.
    let widget_id = symbol_id(&records, "struct", "Widget");
    let base_id = symbol_id(&records, "struct", "Base");
    assert!(
        has_edge(&records, "IMPLEMENTS", &widget_id, &base_id),
        "Widget should embed Base even when declared before it"
    );
}

#[test]
fn go_incremental_cache_reuses_unchanged_files() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("pkg")).expect("pkg dir should be created");
    fs::write(
        repo.join("pkg/mod.go"),
        "package pkg\n\nfunc Bar() int { return 1 }\n",
    )
    .expect("mod.go should write");
    let cache = temp.path().join("cache.json");

    let first = scan_repository_incremental(&repo, &cache).expect("first incremental scan");
    assert!(
        first.rebuilt_files.iter().any(|f| f == "pkg/mod.go"),
        "first scan rebuilds the module: {:?}",
        first.rebuilt_files
    );

    let second = scan_repository_incremental(&repo, &cache).expect("second incremental scan");
    assert!(
        second.reused_files.iter().any(|f| f == "pkg/mod.go"),
        "unchanged module should be reused: {:?}",
        second.reused_files
    );
}

#[test]
fn go_producer_envelope_records_go_grammar() {
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
        components.get("tree_sitter_go").is_some(),
        "Go graph should record the tree_sitter_go grammar: {components}"
    );
    assert!(
        components.get("tree_sitter_rust").is_none(),
        "Go-only graph should not record the Rust grammar: {components}"
    );
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

fn has_edge(records: &[Value], label: &str, source: &str, target: &str) -> bool {
    records.iter().any(|record| {
        record["record_type"] == "edge"
            && record["label"] == label
            && record["source"] == source
            && record["target"] == target
    })
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
