//! `eg export scip --graph <f>|--data-dir <d> --out <index.scip>` — the
//! definitions-only SCIP code-intelligence export (issue #233).
//!
//! CLI-level coverage: the command writes an encoded SCIP protobuf that parses
//! back in-process (no external `scip` CLI), the definition/skip report is
//! printed, output is byte-identical across runs, and the mutually-exclusive
//! input flags are enforced.

#![allow(missing_docs)]

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

use aletheia_egregore::{GraphRecord, NodeKind, ir::SourceSpan, scip::decode_index};

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 1,
        start_line,
        end_line,
        // No columns recorded: legacy provenance exercises the whole-line
        // fallback (issue #463).
        start_column: None,
        end_column: None,
    }
}

const fn span_columns(
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 1,
        start_line,
        end_line,
        start_column: Some(start_column),
        end_column: Some(end_column),
    }
}

/// A small code graph: one repository, one file, two real definitions, plus a
/// Diagnostic stub and a positionless edge that must never become occurrences.
fn line(record: &GraphRecord) -> String {
    serde_json::to_string(record).expect("serialize record")
}

fn fixture_jsonl() -> String {
    // Each GraphRecord::Node is a large struct, so serialize each as a temporary
    // (never held together in a stack array) into a small array of lines.
    let mut lines: Vec<String> = [
        line(&GraphRecord::node(
            "repo-1".to_string(),
            NodeKind::Repository,
            None,
            None,
            Some("widget".to_string()),
            "Repository widget".to_string(),
        )),
        line(&GraphRecord::node(
            "file-1".to_string(),
            NodeKind::File,
            Some("src/lib.rs".to_string()),
            None,
            Some("src/lib.rs".to_string()),
            "File src/lib.rs".to_string(),
        )),
        line(&GraphRecord::syntax_symbol(
            "s-widget".to_string(),
            "struct",
            "src/lib.rs".to_string(),
            span(3, 10),
            "Widget".to_string(),
            "rust",
            0,
            "Rust struct Widget".to_string(),
        )),
        line(&GraphRecord::syntax_symbol(
            "s-render".to_string(),
            "method",
            "src/lib.rs".to_string(),
            span(5, 8),
            "Widget::render".to_string(),
            "rust",
            0,
            "Rust method Widget::render".to_string(),
        )),
        line(&GraphRecord::node(
            "d-1".to_string(),
            NodeKind::Diagnostic,
            Some("src/lib.rs".to_string()),
            Some(span(30, 31)),
            Some("macro_bang".to_string()),
            "Diagnostic".to_string(),
        )),
        line(&GraphRecord::edge(
            aletheia_egregore::ir::EdgeLabel::Defines,
            "file-1".to_string(),
            "s-widget".to_string(),
            Some("1.0".to_string()),
            "File defines Widget".to_string(),
        )),
    ]
    .to_vec();
    lines.sort_unstable();
    format!("{}\n", lines.join("\n"))
}

#[test]
fn export_scip_from_graph_writes_parseable_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let graph = dir.path().join("graph.jsonl");
    let out = dir.path().join("index.scip");
    fs::write(&graph, fixture_jsonl()).expect("write graph");

    Command::cargo_bin("eg")
        .expect("eg binary")
        .arg("export")
        .arg("scip")
        .arg("--graph")
        .arg(&graph)
        .arg("--out")
        .arg(&out)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "exported 2 definitions across 1 documents",
        ))
        .stdout(predicate::str::contains("1 diagnostic-stubs"));

    // The written bytes parse back as a SCIP Index (in-process, no scip CLI).
    let bytes = fs::read(&out).expect("read scip index");
    assert!(!bytes.is_empty(), "SCIP index must not be empty");
    let index = decode_index(&bytes).expect("emitted index must parse back");
    assert_eq!(index.documents.len(), 1);
    let doc = &index.documents[0];
    assert_eq!(doc.relative_path, "src/lib.rs");
    assert_eq!(doc.symbols.len(), 2);
    assert_eq!(doc.occurrences.len(), 2);
    let meta = index.metadata.as_ref().expect("metadata");
    assert_eq!(meta.tool_info.name, "egregore");
}

/// Column-carrying spans (issue #463) export as precise half-open ranges, and
/// the document declares the UTF-8 position encoding the exporter uses.
#[test]
fn export_scip_emits_column_precise_ranges() {
    use aletheia_egregore::scip::build_index;

    let records = vec![
        GraphRecord::node(
            "file-1".to_string(),
            NodeKind::File,
            Some("src/lib.rs".to_string()),
            None,
            Some("src/lib.rs".to_string()),
            "File src/lib.rs".to_string(),
        ),
        GraphRecord::syntax_symbol(
            "s-indented".to_string(),
            "function",
            "src/lib.rs".to_string(),
            span_columns(3, 4, 3, 15),
            "indented".to_string(),
            "rust",
            0,
            "Rust function indented".to_string(),
        ),
        // Legacy span with no columns degrades to the whole-line range.
        GraphRecord::syntax_symbol(
            "s-legacy".to_string(),
            "function",
            "src/lib.rs".to_string(),
            span(5, 6),
            "legacy".to_string(),
            "rust",
            0,
            "Rust function legacy".to_string(),
        ),
    ];
    let export = build_index(&records, "widget", "0.0.0");
    let doc = &export.index.documents[0];
    assert_eq!(
        doc.position_encoding.enum_value_or_default(),
        scip::types::PositionEncoding::UTF8CodeUnitOffsetFromLineStart
    );
    let range_of = |display: &str| {
        doc.occurrences
            .iter()
            .find(|occ| {
                doc.symbols
                    .iter()
                    .any(|s| s.symbol == occ.symbol && s.display_name == display)
            })
            .expect("occurrence")
            .range
            .clone()
    };
    // Line 3 (1-based), columns 4..15 → 0-based half-open [2, 4, 2, 15].
    assert_eq!(range_of("indented"), vec![2, 4, 2, 15]);
    // No columns → whole-line fallback [4, 0, 6, 0].
    assert_eq!(range_of("legacy"), vec![4, 0, 6, 0]);
}

#[test]
fn export_scip_is_byte_identical_across_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let graph = dir.path().join("graph.jsonl");
    fs::write(&graph, fixture_jsonl()).expect("write graph");

    let run = |out: &std::path::Path| {
        Command::cargo_bin("eg")
            .expect("eg binary")
            .arg("export")
            .arg("scip")
            .arg("--graph")
            .arg(&graph)
            .arg("--out")
            .arg(out)
            .assert()
            .success();
        fs::read(out).expect("read out")
    };
    let a = run(&dir.path().join("a.scip"));
    let b = run(&dir.path().join("b.scip"));
    assert_eq!(a, b, "SCIP export must be byte-identical across runs");
}

#[test]
fn export_scip_rejects_both_input_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let graph = dir.path().join("graph.jsonl");
    let out = dir.path().join("index.scip");
    fs::write(&graph, fixture_jsonl()).expect("write graph");

    // clap enforces the mutual exclusion (conflicts_with) at parse time.
    Command::cargo_bin("eg")
        .expect("eg binary")
        .arg("export")
        .arg("scip")
        .arg("--graph")
        .arg(&graph)
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--out")
        .arg(&out)
        .assert()
        .failure();
}
