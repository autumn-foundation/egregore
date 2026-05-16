#![allow(missing_docs)]

use std::path::PathBuf;

use aletheia_codegraph::scan_repository;
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

#[test]
fn deterministic_scan_produces_stable_jsonl() {
    let repo = fixture_repo();

    let first = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let second = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");

    assert_eq!(first, second);
    assert!(first.contains(r#""kind":"Repository""#));
    assert!(first.contains(r#""kind":"File""#));
    assert!(first.contains(r#""repo_relative_path":"src/lib.rs""#));

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
fn rust_fixture_covers_common_symbols() {
    let repo = fixture_repo();

    let jsonl = scan_repository(&repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);

    assert_node(&records, "Module", "nested");
    assert_import(&records, "std::fmt::Debug");
    assert_import(&records, "super::Debug");

    assert_symbol(&records, "function", "answer");
    assert_symbol(&records, "function", "nested::helper");
    assert_symbol(&records, "function", "nested::debug_name");
    assert_symbol(&records, "struct", "nested::Widget");
    assert_symbol(&records, "enum", "nested::Mode");
    assert_symbol(&records, "trait", "nested::Runner");
    assert_symbol(&records, "impl", "nested::impl Widget");
    assert_symbol(&records, "impl", "nested::impl Runner for Widget");
    assert_symbol(&records, "method", "nested::Widget::new");
    assert_symbol(&records, "method", "nested::Widget::value");
    assert_symbol(&records, "method", "nested::Runner for Widget::run");
    assert_symbol(&records, "const", "nested::LIMIT");
    assert_symbol(&records, "static", "nested::NAME");
    assert_symbol(&records, "type_alias", "nested::Alias");
    assert_symbol(&records, "test", "nested::widget_runs");

    assert_diagnostic(&records, "macro invocation local_macro!");

    assert_edge_label(&records, "CONTAINS");
    assert_edge_label(&records, "DEFINES");
    assert_edge_label(&records, "IMPORTS");
    assert_edge_label(&records, "IMPLEMENTS");
    assert_edge_label(&records, "CALLS");
    assert_edge_label(&records, "MENTIONS");
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn assert_node(records: &[Value], kind: &str, name: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == kind
            && record["name"] == name
            && record["repo_relative_path"] == "src/lib.rs"
            && record.get("span").is_some()
    });
    assert!(found, "missing {kind} node named {name}");
}

fn assert_import(records: &[Value], name: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Import"
            && record["name"] == name
            && record["repo_relative_path"] == "src/lib.rs"
            && record.get("span").is_some()
    });
    assert!(found, "missing import node named {name}");
}

fn assert_symbol(records: &[Value], symbol_kind: &str, name: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Symbol"
            && record["symbol_kind"] == symbol_kind
            && record["name"] == name
            && record["repo_relative_path"] == "src/lib.rs"
            && record.get("span").is_some()
    });
    assert!(found, "missing {symbol_kind} symbol named {name}");
}

fn assert_diagnostic(records: &[Value], summary_fragment: &str) {
    let found = records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Diagnostic"
            && record["repo_relative_path"] == "src/lib.rs"
            && record["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains(summary_fragment))
    });
    assert!(found, "missing diagnostic containing {summary_fragment}");
}

fn assert_edge_label(records: &[Value], label: &str) {
    let found = records
        .iter()
        .any(|record| record["record_type"] == "edge" && record["label"] == label);
    assert!(found, "missing {label} edge");
}
