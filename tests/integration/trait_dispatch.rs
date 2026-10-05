//! Trait-dispatch resolution end to end (issue #267).
//!
//! A runnable fixture exercises a static call, a `dyn Trait` call, and
//! generic-bound calls (`T: Trait` inline and via `where` clause) against a
//! known trait-method target, asserting both the resolved implementor set and
//! the `unresolved_dispatch` marker set. The scan is byte-stable across runs,
//! and the query lanes (`transitive-callers`, `transitive-callees`,
//! `change-impact`) surface the dispatch boundaries explicitly.

#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-07T00:00:00Z";
const REPO_ID: &str = "trait-dispatch-fixture";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

fn fixture_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Cargo.toml",
            "[package]\nname = \"dispatch_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", "pub mod shapes;\npub mod draw;\n"),
        (
            "src/shapes.rs",
            "pub trait Renderable {\n    fn render(&self);\n}\n\
             pub struct Circle;\nimpl Renderable for Circle {\n    fn render(&self) {}\n}\n\
             pub struct Square;\nimpl Renderable for Square {\n    fn render(&self) {}\n}\n\
             // An unrelated inherent method that merely shares the method name:\n\
             // dispatch must never link here.\n\
             pub struct Speaker;\nimpl Speaker {\n    pub fn render(&self) {}\n}\n\
             // A trait with no in-crate implementors: its dispatch sites must\n\
             // mint the typed unresolved_dispatch marker.\n\
             pub trait Orphan {\n    fn orphan_render(&self);\n}\n",
        ),
        (
            "src/draw.rs",
            "use crate::shapes::{Circle, Orphan, Renderable, Speaker};\n\
             pub fn draw_static(c: &Circle) {\n    c.render();\n}\n\
             pub fn draw_dyn(item: &dyn Renderable) {\n    item.render();\n}\n\
             pub fn draw_generic<T: Renderable>(t: &T) {\n    t.render();\n}\n\
             pub fn draw_where<T>(t: T)\nwhere\n    T: Renderable,\n{\n    t.render();\n}\n\
             pub fn draw_orphan(o: &dyn Orphan) {\n    o.orphan_render();\n}\n\
             pub fn draw_speaker(s: &Speaker) {\n    s.render();\n}\n",
        ),
    ]
}

fn scan_fixture(root: &Path) -> String {
    scan_repository_at_with_override(root, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize")
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

/// Finds the record ID of the first node with the given name/path/symbol-kind.
fn node_id(records: &[Value], name: &str, path: &str, symbol_kind: &str) -> String {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["name"] == name
                && record["repo_relative_path"] == path
                && record["symbol_kind"] == symbol_kind
        })
        .unwrap_or_else(|| panic!("missing {symbol_kind} node {name} in {path}"))["id"]
        .as_str()
        .expect("node should have an ID")
        .to_owned()
}

/// Finds the record ID of the first method node whose qualified name ends
/// with `owner::method` in the given path.
fn method_id(records: &[Value], path: &str, owner: &str, method: &str) -> String {
    let suffix = format!("{owner}::{method}");
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["symbol_kind"] == "method"
                && record["repo_relative_path"] == path
                && record["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(suffix.as_str()))
        })
        .unwrap_or_else(|| panic!("missing method {suffix} in {path}"))["id"]
        .as_str()
        .expect("node should have an ID")
        .to_owned()
}

/// `(source_id, target_id, resolution)` triples for every CALLS edge.
fn calls_edges(records: &[Value]) -> Vec<(String, String, Option<String>)> {
    records
        .iter()
        .filter(|record| record["record_type"] == "edge" && record["label"] == "CALLS")
        .map(|record| {
            (
                record["source"].as_str().unwrap_or("").to_owned(),
                record["target"].as_str().unwrap_or("").to_owned(),
                record["resolution"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn dispatch_marker(records: &[Value]) -> Value {
    let markers: Vec<&Value> = records
        .iter()
        .filter(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("unresolved_dispatch:"))
        })
        .collect();
    assert_eq!(
        markers.len(),
        1,
        "exactly one unresolved_dispatch marker expected, got {}",
        markers.len()
    );
    (*markers[0]).clone()
}

struct Fixture {
    _temp: tempfile::TempDir,
    graph_path: PathBuf,
    records: Vec<Value>,
}

fn fixture() -> Fixture {
    let temp = tempfile::TempDir::new().expect("temp dir should be created");
    write_fixture(temp.path(), &fixture_files());
    let jsonl = scan_fixture(temp.path());
    let records = parse_jsonl(&jsonl);
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, &jsonl).expect("graph file should be written");
    Fixture {
        _temp: temp,
        graph_path,
        records,
    }
}

fn run_query(fixture: &Fixture, args: &[&str]) -> (Value, Vec<Value>) {
    let stdout = egregore()
        .arg("query")
        .args(args)
        .arg("--graph")
        .arg(&fixture.graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(stdout).expect("utf8");
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header: Value =
        serde_json::from_str(lines.next().expect("header line")).expect("header is JSON");
    let rows: Vec<Value> = lines
        .map(|line| serde_json::from_str(line).expect("row is JSON"))
        .collect();
    (header, rows)
}

/// Runs `eg query change-impact`, which emits a single pretty JSON response
/// (not NDJSON) carrying a `diagnostics` array.
fn run_change_impact(fixture: &Fixture, anchor: &str) -> Value {
    let stdout = egregore()
        .args(["query", "change-impact", anchor])
        .arg("--graph")
        .arg(&fixture.graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(stdout).expect("utf8");
    serde_json::from_str(&text).expect("change-impact emits one JSON response")
}

// ---------------------------------------------------------------------------
// Scan-level assertions (AC1–AC6)
// ---------------------------------------------------------------------------

#[test]
fn scan_is_byte_stable_across_runs() {
    let temp = tempfile::TempDir::new().expect("temp dir should be created");
    write_fixture(temp.path(), &fixture_files());
    let first = scan_fixture(temp.path());
    let second = scan_fixture(temp.path());
    assert_eq!(
        first, second,
        "two scans of the fixture must be byte-identical"
    );
}

#[test]
fn static_call_stays_resolved_to_the_concrete_method() {
    let f = fixture();
    let caller = node_id(&f.records, "draw::draw_static", "src/draw.rs", "function");
    let target = method_id(&f.records, "src/shapes.rs", "Circle", "render");
    let edges = calls_edges(&f.records);
    assert!(
        edges
            .iter()
            .any(|(source, edge_target, resolution)| source == &caller
                && edge_target == &target
                && resolution.as_deref() == Some("resolved")),
        "static call must stay resolved to Circle::render"
    );
}

#[test]
fn dyn_dispatch_links_every_in_crate_implementor() {
    let f = fixture();
    let caller = node_id(&f.records, "draw::draw_dyn", "src/draw.rs", "function");
    let circle = method_id(&f.records, "src/shapes.rs", "Circle", "render");
    let square = method_id(&f.records, "src/shapes.rs", "Square", "render");
    let speaker = method_id(&f.records, "src/shapes.rs", "Speaker", "render");
    let edges = calls_edges(&f.records);
    for target in [&circle, &square] {
        assert!(
            edges
                .iter()
                .any(|(source, edge_target, resolution)| source == &caller
                    && edge_target == target
                    && resolution.as_deref() == Some("ambiguous")),
            "dyn dispatch must link {target} as ambiguous"
        );
    }
    assert!(
        !edges
            .iter()
            .any(|(source, edge_target, _)| source == &caller && edge_target == &speaker),
        "dyn dispatch must never link the unrelated same-named Speaker::render"
    );
}

#[test]
fn generic_bound_dispatch_links_every_in_crate_implementor() {
    let f = fixture();
    let circle = method_id(&f.records, "src/shapes.rs", "Circle", "render");
    let square = method_id(&f.records, "src/shapes.rs", "Square", "render");
    let edges = calls_edges(&f.records);
    for caller_name in ["draw::draw_generic", "draw::draw_where"] {
        let caller = node_id(&f.records, caller_name, "src/draw.rs", "function");
        for target in [&circle, &square] {
            assert!(
                edges.iter().any(|(source, edge_target, resolution)| {
                    source == &caller
                        && edge_target == target
                        && resolution.as_deref() == Some("ambiguous")
                }),
                "{caller_name} must link {target} as ambiguous"
            );
        }
    }
}

#[test]
fn unimplementable_dispatch_mints_a_typed_marker_with_span() {
    let f = fixture();
    let marker = dispatch_marker(&f.records);
    let name = marker["name"].as_str().expect("marker has a name");
    assert!(
        name.contains("Orphan") && name.contains("orphan_render"),
        "marker must carry the trait+method handle, got {name}"
    );
    // The call site is `o.orphan_render();` on line 18 of src/draw.rs.
    let span = &marker["span"];
    assert_eq!(
        span["start_line"].as_u64(),
        Some(18),
        "marker must carry the call-site span, got {span}"
    );
    let caller = node_id(&f.records, "draw::draw_orphan", "src/draw.rs", "function");
    let marker_id = marker["id"].as_str().expect("marker has an ID");
    let edges = calls_edges(&f.records);
    assert!(
        edges
            .iter()
            .any(|(source, target, resolution)| source == &caller
                && target == marker_id
                && resolution.as_deref() == Some("unresolved_dispatch")),
        "the dispatch edge must be typed unresolved_dispatch"
    );
}

// ---------------------------------------------------------------------------
// Query-lane assertions (AC4)
// ---------------------------------------------------------------------------

#[test]
fn transitive_callers_reaches_dispatch_callers_of_an_implementor() {
    let f = fixture();
    let circle = method_id(&f.records, "src/shapes.rs", "Circle", "render");
    let (_header, rows) = run_query(&f, &["transitive-callers", &circle]);
    // Caller rows carry no category: every row is a caller lead.
    let caller_ids: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["record_id"].as_str())
        .collect();
    for caller_name in [
        "draw::draw_static",
        "draw::draw_dyn",
        "draw::draw_generic",
        "draw::draw_where",
    ] {
        let id = node_id(&f.records, caller_name, "src/draw.rs", "function");
        assert!(
            caller_ids.contains(&id.as_str()),
            "{caller_name} must surface as a caller of Circle::render, got {caller_ids:?}"
        );
    }
    let orphan_caller = node_id(&f.records, "draw::draw_orphan", "src/draw.rs", "function");
    assert!(
        !caller_ids.contains(&orphan_caller.as_str()),
        "draw_orphan dispatches to no implementor and must not surface"
    );
}

#[test]
fn transitive_callees_enumerates_the_dispatch_boundary() {
    let f = fixture();
    let orphan_caller = node_id(&f.records, "draw::draw_orphan", "src/draw.rs", "function");
    let (_header, rows) = run_query(&f, &["transitive-callees", &orphan_caller]);
    let unresolved: Vec<&Value> = rows
        .iter()
        .filter(|row| row["category"].as_str() == Some("unresolved"))
        .collect();
    assert_eq!(
        unresolved.len(),
        1,
        "exactly one unresolved boundary expected, got {unresolved:?}"
    );
    assert_eq!(
        unresolved[0]["reason"].as_str(),
        Some("unresolved_dispatch"),
        "the boundary must carry the typed reason"
    );
    assert!(
        unresolved[0]["target_record_id"]
            .as_str()
            .is_some_and(|id| id == dispatch_marker(&f.records)["id"].as_str().unwrap_or("")),
        "the boundary must cite the marker node"
    );
}

#[test]
fn transitive_callees_reaches_dispatch_implementors() {
    let f = fixture();
    let caller = node_id(&f.records, "draw::draw_dyn", "src/draw.rs", "function");
    let (_header, rows) = run_query(&f, &["transitive-callees", &caller]);
    let reachable: Vec<&str> = rows
        .iter()
        .filter(|row| row["category"].as_str() == Some("reachable"))
        .filter_map(|row| row["record_id"].as_str())
        .collect();
    for (owner, method) in [("Circle", "render"), ("Square", "render")] {
        let id = method_id(&f.records, "src/shapes.rs", owner, method);
        assert!(
            reachable.contains(&id.as_str()),
            "{owner}::{method} must be reachable from draw_dyn, got {reachable:?}"
        );
    }
}

#[test]
fn change_impact_reports_the_dispatch_boundary_as_a_diagnostic() {
    let f = fixture();
    let orphan_caller = node_id(&f.records, "draw::draw_orphan", "src/draw.rs", "function");
    let response = run_change_impact(&f, &orphan_caller);
    let diagnostics = response["diagnostics"]
        .as_array()
        .expect("change-impact response carries diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|row| row["code"].as_str() == Some("unresolved_dispatch")
                && row["target_handle"].as_str().is_some_and(
                    |handle| handle.contains("unresolved_dispatch: Orphan::orphan_render")
                )),
        "change-impact must report the dispatch boundary as a typed diagnostic, got {diagnostics:?}"
    );
}
