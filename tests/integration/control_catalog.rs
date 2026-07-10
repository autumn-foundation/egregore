//! Integration tests for `eg audit control-catalog` (issue #337): the SOC2
//! control->evidence-class catalog loader, validator, and BLAKE3 hash-pin.
//!
//! Covers the embedded default catalog, `--catalog` override, byte-identical
//! repeated runs, and the exit-2 error surfaces (unknown evidence class,
//! unknown schema version, malformed JSON, missing file).

#![allow(missing_docs)]

use std::fs;

use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn write_temp(name: &str, contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join(name);
    fs::write(&path, contents).expect("write fixture");
    (temp, path)
}

#[test]
fn default_catalog_is_valid_and_reports_identity() {
    let output = egregore()
        .args(["audit", "control-catalog"])
        .output()
        .expect("run");
    assert_eq!(
        output.status.code(),
        Some(0),
        "default catalog should be valid"
    );

    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout is one JSON line");
    assert_eq!(value["ok"], true);
    assert_eq!(value["catalog_id"], "soc2-v1");
    assert_eq!(value["catalog_schema_version"]["domain"], "control_catalog");
    assert_eq!(value["catalog_schema_version"]["kind"], "ControlCatalog");
    assert_eq!(value["catalog_schema_version"]["version"], 1);
    assert_eq!(value["control_count"], 3);
    assert_eq!(value["controls"].as_array().unwrap().len(), 3);

    let hash = value["catalog_hash"].as_str().expect("hash string");
    assert!(
        hash.starts_with("control_catalog:v1:"),
        "unexpected hash handle: {hash}"
    );
}

#[test]
fn default_output_is_byte_identical_across_runs() {
    let first = egregore()
        .args(["audit", "control-catalog"])
        .output()
        .expect("run");
    let second = egregore()
        .args(["audit", "control-catalog"])
        .output()
        .expect("run");
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(
        first.stdout, second.stdout,
        "default JSON output must be byte-identical across runs"
    );
    // The single-line contract: exactly one trailing newline, no interior ones.
    let text = String::from_utf8(first.stdout).expect("utf8");
    assert_eq!(text.matches('\n').count(), 1, "must be a single JSON line");
}

#[test]
fn catalog_override_with_good_file_works() {
    let good = r#"{
        "catalog_id": "custom-v1",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
        "controls": [
            { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                { "class": "commits", "requirement": "required" }
            ] }
        ]
    }"#;
    let (_temp, path) = write_temp("good.json", good);
    let output = egregore()
        .args(["audit", "control-catalog", "--catalog"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["catalog_id"], "custom-v1");
    assert_eq!(value["control_count"], 1);
}

#[test]
fn unknown_evidence_class_exits_two_and_names_class() {
    let bad = r#"{
        "catalog_id": "x",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
        "controls": [
            { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                { "class": "not_a_real_class", "requirement": "required" }
            ] }
        ]
    }"#;
    let (_temp, path) = write_temp("bad_class.json", bad);
    let output = egregore()
        .args(["audit", "control-catalog", "--catalog"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr json");
    assert_eq!(err["code"], "unknown_evidence_class");
    assert_eq!(err["control_id"], "CC1.1");
    assert_eq!(err["class"], "not_a_real_class");
}

#[test]
fn unknown_schema_version_exits_two_with_tuple() {
    let bad = r#"{
        "catalog_id": "x",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 2 },
        "controls": []
    }"#;
    let (_temp, path) = write_temp("bad_version.json", bad);
    let output = egregore()
        .args(["audit", "control-catalog", "--catalog"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr json");
    assert_eq!(err["code"], "unknown_schema_version");
    assert_eq!(err["version"]["domain"], "control_catalog");
    assert_eq!(err["version"]["kind"], "ControlCatalog");
    assert_eq!(err["version"]["version"], 2);
}

#[test]
fn malformed_json_exits_two() {
    let (_temp, path) = write_temp("malformed.json", "{ this is not json");
    let output = egregore()
        .args(["audit", "control-catalog", "--catalog"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr json");
    assert_eq!(err["code"], "malformed_json");
}

#[test]
fn missing_file_exits_two() {
    let output = egregore()
        .args([
            "audit",
            "control-catalog",
            "--catalog",
            "/no/such/catalog/soc2.json",
        ])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr json");
    assert_eq!(err["code"], "catalog_read_error");
}
