//! Integration tests for `eg validate` (issue #103): pre-ingest referential
//! integrity validation of a graph JSONL.
//!
//! Covers the seeded defect fixture (dangling edge endpoint, orphan node,
//! edge to tombstoned record, tombstone stranding a live edge, edge target
//! kind violation) plus a clean baseline, exit codes, canonical diagnostic
//! ordering, byte-identical repeated runs, and redaction safety.

#![allow(missing_docs)]

use std::{fs, path::Path, path::PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

/// Minimal Rust crate exercising every record kind the validator inspects:
/// Repository, File, Module, Symbol, Import nodes plus CONTAINS / DEFINES /
/// IMPORTS / CALLS edges.
const LIB_RS: &str = r"use std::fmt;

pub mod api;

pub fn alpha() -> usize {
    42
}

pub fn beta() -> usize {
    alpha()
}
";

const API_RS: &str = r"pub fn gamma() {}
";

/// Scans the fixture crate and returns (`TempDir`, graph path). Caller must
/// keep the `TempDir` alive.
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    fs::write(temp.path().join("src/lib.rs"), LIB_RS).expect("lib.rs");
    fs::write(temp.path().join("src/api.rs"), API_RS).expect("api.rs");
    let jsonl = scan_repository_at_with_override(temp.path(), FIXED_TIME, Some("validate-fixture"))
        .expect("fixture should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, jsonl).expect("write graph");
    (temp, graph)
}

/// Parses the graph JSONL into JSON values.
fn graph_records(graph: &Path) -> Vec<Value> {
    fs::read_to_string(graph)
        .expect("read graph")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid record"))
        .collect()
}

/// Finds the record ID of the first node with the given kind (and optional name).
fn node_id(records: &[Value], kind: &str, name: Option<&str>) -> String {
    records
        .iter()
        .find(|r| {
            r["record_type"] == "node"
                && r["kind"] == kind
                && name.is_none_or(|n| r["name"].as_str().is_some_and(|v| v.contains(n)))
        })
        .unwrap_or_else(|| panic!("missing {kind} node"))["id"]
        .as_str()
        .expect("id")
        .to_owned()
}

/// Appends raw JSONL lines to the graph file.
fn append_lines(graph: &Path, lines: &[String]) {
    let mut content = fs::read_to_string(graph).expect("read graph");
    for line in lines {
        content.push_str(line);
        content.push('\n');
    }
    fs::write(graph, content).expect("write graph");
}

fn edge_line(id: &str, label: &str, source: &str, target: &str) -> String {
    serde_json::json!({
        "record_type": "edge",
        "id": id,
        "schema_version": 5,
        "label": label,
        "source": source,
        "target": target,
        "summary": "injected edge"
    })
    .to_string()
}

fn tombstone_line(id: &str, deleted_id: &str) -> String {
    serde_json::json!({
        "record_type": "tombstone",
        "id": id,
        "schema_version": 5,
        "deleted_id": deleted_id,
        "summary": "injected tombstone"
    })
    .to_string()
}

fn orphan_node_line(id: &str, summary: &str) -> String {
    serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": "Symbol",
        "schema_version": 5,
        "repo_relative_path": "src/lib.rs",
        "span": {"start_byte": 0, "end_byte": 10, "start_line": 1, "end_line": 1},
        "name": "ghost_symbol",
        "language": "rust",
        "symbol_kind": "function",
        "disambiguator": 0,
        "summary": summary
    })
    .to_string()
}

/// Runs `eg validate` and returns (exit code, stdout lines as JSON values).
fn run_validate(graph: &Path) -> (i32, Vec<Value>) {
    let output = egregore()
        .arg("validate")
        .arg(graph)
        .output()
        .expect("run validate");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let lines = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout must be JSONL"))
        .collect();
    (output.status.code().expect("exit code"), lines)
}

/// Splits validate stdout into (diagnostic lines, summary line).
const fn split_output(lines: &[Value]) -> (&[Value], &Value) {
    let (summary, diagnostics) = lines.split_last().expect("at least a summary line");
    (diagnostics, summary)
}

fn diagnostics_with_code<'a>(diagnostics: &'a [Value], code: &str) -> Vec<&'a Value> {
    diagnostics.iter().filter(|d| d["code"] == code).collect()
}

// ---------------------------------------------------------------------------
// Clean baseline: exit 0, zero defects, machine-readable summary
// ---------------------------------------------------------------------------

#[test]
fn validate_clean_scan_graph_exits_0_with_zero_defects() {
    let (_temp, graph) = fixture_graph();
    let (code, lines) = run_validate(&graph);

    assert_eq!(code, 0, "clean graph must exit 0");
    let (diagnostics, summary) = split_output(&lines);
    assert!(
        diagnostics.is_empty(),
        "clean graph must emit zero diagnostics, got {diagnostics:?}"
    );
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["defects"], 0);
    assert!(summary["nodes"].as_u64().unwrap_or(0) > 0);
    assert!(summary["edges"].as_u64().unwrap_or(0) > 0);
    assert_eq!(summary["tombstones"], 0);
}

// ---------------------------------------------------------------------------
// Defect class 1: dangling edge endpoint
// ---------------------------------------------------------------------------

#[test]
fn validate_detects_dangling_edge_endpoint() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    let symbol = node_id(&records, "Symbol", Some("alpha"));
    append_lines(
        &graph,
        &[edge_line(
            "codegraph:v5:1111111111111111",
            "CALLS",
            &symbol,
            "codegraph:v5:feedfacefeedface",
        )],
    );

    let (code, lines) = run_validate(&graph);
    assert_eq!(code, 1, "defective graph must exit 1");
    let (diagnostics, summary) = split_output(&lines);
    let dangling = diagnostics_with_code(diagnostics, "dangling_edge_endpoint");
    assert_eq!(dangling.len(), 1, "exactly one dangling diagnostic");
    assert_eq!(dangling[0]["edge_id"], "codegraph:v5:1111111111111111");
    assert_eq!(dangling[0]["relation"], "CALLS");
    assert_eq!(dangling[0]["endpoint"], "target");
    assert_eq!(dangling[0]["missing_id"], "codegraph:v5:feedfacefeedface");
    assert_eq!(summary["ok"], false);
    assert_eq!(summary["defects"], 1);
}

#[test]
fn validate_emits_one_diagnostic_per_defect() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    let symbol = node_id(&records, "Symbol", Some("alpha"));
    // One edge with a dangling target, one with a dangling source: two defects.
    append_lines(
        &graph,
        &[
            edge_line(
                "codegraph:v5:1111111111111111",
                "CALLS",
                &symbol,
                "codegraph:v5:feedfacefeedface",
            ),
            edge_line(
                "codegraph:v5:2222222222222222",
                "CALLS",
                "codegraph:v5:deadbeefdeadbeef",
                &symbol,
            ),
        ],
    );

    let (code, lines) = run_validate(&graph);
    assert_eq!(code, 1);
    let (diagnostics, summary) = split_output(&lines);
    assert_eq!(
        diagnostics_with_code(diagnostics, "dangling_edge_endpoint").len(),
        2,
        "one machine-readable diagnostic per defect"
    );
    assert_eq!(summary["defects"], 2);
}

// ---------------------------------------------------------------------------
// Defect class 2: orphan node
// ---------------------------------------------------------------------------

#[test]
fn validate_detects_orphan_node() {
    let (_temp, graph) = fixture_graph();
    append_lines(
        &graph,
        &[orphan_node_line(
            "codegraph:v5:0f0f0f0f0f0f0f0f",
            "orphaned symbol",
        )],
    );

    let (code, lines) = run_validate(&graph);
    assert_eq!(code, 1);
    let (diagnostics, _summary) = split_output(&lines);
    let orphans = diagnostics_with_code(diagnostics, "orphan_node");
    assert_eq!(orphans.len(), 1, "exactly one orphan diagnostic");
    assert_eq!(orphans[0]["record_id"], "codegraph:v5:0f0f0f0f0f0f0f0f");
    assert_eq!(orphans[0]["kind"], "Symbol");
    assert_eq!(orphans[0]["repo_relative_path"], "src/lib.rs");
    assert!(
        orphans[0]["span"]["start_line"].as_u64().is_some(),
        "orphan diagnostic carries the span when present"
    );
}

// ---------------------------------------------------------------------------
// Defect class 3: edge to a tombstoned-and-unsuperseded record
// ---------------------------------------------------------------------------

#[test]
fn validate_detects_edge_to_tombstoned_record() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    let symbol = node_id(&records, "Symbol", Some("alpha"));
    // Tombstone an ID with no surviving node record, then reference it.
    append_lines(
        &graph,
        &[
            tombstone_line(
                "codegraph:v5:3333333333333333",
                "codegraph:v5:4444444444444444",
            ),
            edge_line(
                "codegraph:v5:5555555555555555",
                "CALLS",
                &symbol,
                "codegraph:v5:4444444444444444",
            ),
        ],
    );

    let (code, lines) = run_validate(&graph);
    assert_eq!(code, 1);
    let (diagnostics, _summary) = split_output(&lines);
    let tombstoned = diagnostics_with_code(diagnostics, "edge_to_tombstoned_record");
    assert_eq!(tombstoned.len(), 1);
    assert_eq!(tombstoned[0]["edge_id"], "codegraph:v5:5555555555555555");
    assert_eq!(tombstoned[0]["relation"], "CALLS");
    assert_eq!(tombstoned[0]["endpoint"], "target");
    assert_eq!(
        tombstoned[0]["tombstoned_id"],
        "codegraph:v5:4444444444444444"
    );
    assert_eq!(
        tombstoned[0]["tombstone_id"],
        "codegraph:v5:3333333333333333"
    );
    // The edge must never be reported as dangling too: the tombstone is the
    // stronger, more specific classification.
    assert!(
        diagnostics_with_code(diagnostics, "dangling_edge_endpoint").is_empty(),
        "tombstoned reference must not double-report as dangling"
    );
}

// ---------------------------------------------------------------------------
// Defect class 4: tombstone stranding a live edge
// ---------------------------------------------------------------------------

#[test]
fn validate_detects_tombstone_stranding_live_edge() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    // Tombstone a symbol whose node record (and DEFINES/CALLS edges) survive.
    let symbol = node_id(&records, "Symbol", Some("alpha"));
    append_lines(
        &graph,
        &[tombstone_line("codegraph:v5:6666666666666666", &symbol)],
    );

    let (code, lines) = run_validate(&graph);
    assert_eq!(code, 1);
    let (diagnostics, _summary) = split_output(&lines);
    let stranded = diagnostics_with_code(diagnostics, "tombstone_strands_live_edge");
    assert_eq!(stranded.len(), 1, "one diagnostic per stranding tombstone");
    assert_eq!(stranded[0]["tombstone_id"], "codegraph:v5:6666666666666666");
    assert_eq!(stranded[0]["deleted_id"], symbol);
    let stranded_edges = stranded[0]["stranded_edge_ids"]
        .as_array()
        .expect("stranded_edge_ids array");
    assert!(
        !stranded_edges.is_empty(),
        "the live edges still referencing the tombstoned node are listed"
    );
    // The node record is still present, so the edge-side reference resolves:
    // this fixture isolates the tombstone-side defect class.
    assert!(
        diagnostics_with_code(diagnostics, "edge_to_tombstoned_record").is_empty(),
        "a tombstoned ID with a surviving node record is superseded for the edge-side check"
    );
}

// ---------------------------------------------------------------------------
// Defect class 5: edge target kind violation
// ---------------------------------------------------------------------------

#[test]
fn validate_detects_edge_target_kind_violation() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    let file = node_id(&records, "File", Some("lib.rs"));
    let import = node_id(&records, "Import", None);
    // DEFINES must target a Symbol; an Import target is a kind violation.
    append_lines(
        &graph,
        &[edge_line(
            "codegraph:v5:7777777777777777",
            "DEFINES",
            &file,
            &import,
        )],
    );

    let (code, lines) = run_validate(&graph);
    assert_eq!(code, 1);
    let (diagnostics, _summary) = split_output(&lines);
    let violations = diagnostics_with_code(diagnostics, "edge_target_kind_violation");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["edge_id"], "codegraph:v5:7777777777777777");
    assert_eq!(violations[0]["relation"], "DEFINES");
    assert_eq!(violations[0]["target_id"], import);
    assert_eq!(violations[0]["target_kind"], "Import");
    assert_eq!(
        violations[0]["allowed_kinds"],
        serde_json::json!(["Symbol"]),
        "the allowed target kinds are named"
    );
}

// ---------------------------------------------------------------------------
// Determinism: canonical order, byte-identical across 5 runs
// ---------------------------------------------------------------------------

#[test]
fn validate_diagnostics_are_canonically_ordered_and_byte_identical() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    let symbol = node_id(&records, "Symbol", Some("alpha"));
    let file = node_id(&records, "File", Some("lib.rs"));
    let import = node_id(&records, "Import", None);
    // Seed all defect classes at once.
    append_lines(
        &graph,
        &[
            edge_line(
                "codegraph:v5:1111111111111111",
                "CALLS",
                &symbol,
                "codegraph:v5:feedfacefeedface",
            ),
            orphan_node_line("codegraph:v5:0f0f0f0f0f0f0f0f", "orphaned symbol"),
            tombstone_line(
                "codegraph:v5:3333333333333333",
                "codegraph:v5:4444444444444444",
            ),
            edge_line(
                "codegraph:v5:5555555555555555",
                "CALLS",
                &symbol,
                "codegraph:v5:4444444444444444",
            ),
            tombstone_line("codegraph:v5:6666666666666666", &symbol),
            edge_line("codegraph:v5:7777777777777777", "DEFINES", &file, &import),
        ],
    );

    let run = || {
        let output = egregore()
            .arg("validate")
            .arg(&graph)
            .output()
            .expect("run validate");
        assert_eq!(output.status.code(), Some(1));
        output.stdout
    };

    let first = run();
    for _ in 0..4 {
        assert_eq!(
            run(),
            first,
            "repeating the same validation must be byte-identical"
        );
    }

    // Diagnostics are grouped by category code in a canonical sorted order.
    let stdout = String::from_utf8(first).expect("utf8");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL"))
        .collect();
    let (diagnostics, summary) = split_output(&lines);
    assert!(diagnostics.len() >= 4, "all defect classes detected");
    let codes: Vec<&str> = diagnostics
        .iter()
        .map(|d| d["code"].as_str().expect("code"))
        .collect();
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    assert_eq!(codes, sorted, "diagnostics must be canonically ordered");
    for class in [
        "dangling_edge_endpoint",
        "orphan_node",
        "edge_to_tombstoned_record",
        "tombstone_strands_live_edge",
        "edge_target_kind_violation",
    ] {
        assert!(
            codes.contains(&class),
            "defect class {class} must be detected, got {codes:?}"
        );
    }
    assert_eq!(summary["ok"], false);
    assert_eq!(summary["defects"].as_u64(), Some(diagnostics.len() as u64));
}

// ---------------------------------------------------------------------------
// Exit codes: load errors are distinct from gate failures
// ---------------------------------------------------------------------------

#[test]
fn validate_missing_graph_file_exits_2() {
    egregore()
        .args(["validate", "/nonexistent/graph.jsonl"])
        .assert()
        .code(2);
}

#[test]
fn validate_malformed_jsonl_exits_2() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("broken.jsonl");
    fs::write(&graph, "this is not json\n").expect("write graph");
    egregore().arg("validate").arg(&graph).assert().code(2);
}

// ---------------------------------------------------------------------------
// --format text
// ---------------------------------------------------------------------------

#[test]
fn validate_format_text_reports_defects_and_exit_code() {
    let (_temp, graph) = fixture_graph();
    let records = graph_records(&graph);
    let symbol = node_id(&records, "Symbol", Some("alpha"));
    append_lines(
        &graph,
        &[edge_line(
            "codegraph:v5:1111111111111111",
            "CALLS",
            &symbol,
            "codegraph:v5:feedfacefeedface",
        )],
    );

    let output = egregore()
        .args(["validate", "--format", "text"])
        .arg(&graph)
        .output()
        .expect("run validate");
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(
        stdout.contains("dangling_edge_endpoint"),
        "text output names the defect category, got {stdout:?}"
    );
    assert!(
        stdout.contains("1 defect"),
        "text output reports the defect count, got {stdout:?}"
    );
}

#[test]
fn validate_format_text_clean_graph_exits_0() {
    let (_temp, graph) = fixture_graph();
    let output = egregore()
        .args(["validate", "--format", "text"])
        .arg(&graph)
        .output()
        .expect("run validate");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(
        stdout.contains("0 defects"),
        "clean text output reports zero defects, got {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// Redaction safety: only IDs, categories, relations, paths, spans, counts
// ---------------------------------------------------------------------------

#[test]
fn validate_output_never_includes_record_payload_text() {
    let (_temp, graph) = fixture_graph();
    let sentinel = "SECRET_TRANSCRIPT_PAYLOAD_ZX81";
    append_lines(
        &graph,
        &[orphan_node_line("codegraph:v5:0f0f0f0f0f0f0f0f", sentinel)],
    );

    for format in ["json", "text"] {
        let output = egregore()
            .args(["validate", "--format", format])
            .arg(&graph)
            .output()
            .expect("run validate");
        assert_eq!(output.status.code(), Some(1));
        let stdout = String::from_utf8(output.stdout).expect("utf8");
        assert!(
            !stdout.contains(sentinel),
            "{format} output must never include record summary/payload text"
        );
    }
}
