//! Executable schema-version compatibility fixtures.

use std::fs;

use aletheia_egregore::{
    GraphRecord, NodeKind, SCHEMA_VERSION, SourceSpan,
    adapters::{AdapterError, records_from_jsonl, records_from_jsonl_report},
};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

fn codegraph_symbol_json(id: &str, schema_version: u32) -> serde_json::Value {
    json!({
        "record_type": "node",
        "id": id,
        "kind": "Symbol",
        "schema_version": schema_version,
        "repo_relative_path": "src/lib.rs",
        "span": {
            "start_byte": 0,
            "end_byte": 8,
            "start_line": 1,
            "end_line": 1
        },
        "name": "fixture::known",
        "language": "rust",
        "symbol_kind": "function",
        "summary": "known code graph symbol"
    })
}

#[test]
fn future_schema_version_is_typed_and_inspect_reports_mixed_counts() {
    let current = codegraph_symbol_json("codegraph:v4:known-symbol", SCHEMA_VERSION);
    let future_version = SCHEMA_VERSION + 1;
    let future = codegraph_symbol_json("codegraph:v5:future-symbol", future_version);
    let jsonl = format!("{current}\n{future}\n");

    let report = records_from_jsonl_report(&jsonl).expect("mixed JSONL should be readable");
    assert_eq!(
        report.records.len(),
        1,
        "recognized record must still parse"
    );
    assert!(matches!(
        &report.records[0],
        GraphRecord::Node {
            kind: NodeKind::Symbol,
            name: Some(name),
            ..
        } if name == "fixture::known"
    ));

    let unknown = report
        .unknown_schema_versions
        .first()
        .expect("future record should produce an unknown schema-version diagnostic");
    assert_eq!(unknown.code, "unknown_schema_version");
    assert_eq!(unknown.version.domain, "codegraph");
    assert_eq!(unknown.version.kind, "Symbol");
    assert_eq!(unknown.version.version, future_version);

    let parse_error =
        records_from_jsonl(&jsonl).expect_err("default reader must reject unknown versions");
    assert!(matches!(
        parse_error,
        AdapterError::UnknownSchemaVersion { ref version, .. }
            if version.domain == "codegraph"
                && version.kind == "Symbol"
                && version.version == future_version
    ));

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("mixed-schema.jsonl");
    fs::write(&graph_path, jsonl).expect("fixture should write");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg(&graph_path)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "schema_version codegraph Symbol v{SCHEMA_VERSION}: 1"
        )))
        .stdout(predicate::str::contains(format!(
            "unknown_schema_version codegraph Symbol v{future_version}: 1"
        )))
        .stderr(predicate::str::is_empty());
}

#[test]
fn additive_unknown_field_parses_and_inspects_without_warning() {
    let mut value = codegraph_symbol_json("codegraph:v4:additive-symbol", SCHEMA_VERSION);
    value["future_optional_field"] = json!("reader should ignore this additive field");
    let jsonl = format!("{value}\n");

    let records = records_from_jsonl(&jsonl).expect("additive optional field should be ignored");
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0],
        GraphRecord::Node {
            kind: NodeKind::Symbol,
            span: Some(SourceSpan { start_line: 1, .. }),
            ..
        }
    ));

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("additive-schema.jsonl");
    fs::write(&graph_path, jsonl).expect("fixture should write");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("inspect")
        .arg(&graph_path)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "schema_version codegraph Symbol v{SCHEMA_VERSION}: 1"
        )))
        .stdout(predicate::str::contains("unknown_schema_version").not())
        .stderr(predicate::str::is_empty());
}
