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
fn text_output_neutralizes_control_characters_in_catalog_values() {
    // A vendor-supplied `--catalog` is untrusted input: a control title
    // carrying an ANSI escape (ESC `[2J` clears the terminal) must not be
    // able to drive the operator's terminal through `--format text` (#337
    // review finding F2; mirrors the #104 `bounded_identity_field` doctrine).
    // The fixture carries the ESC as a JSON `\\u001b` escape, which the
    // parser materializes into a real control character.
    let catalog = r#"{
        "catalog_id": "evil",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
        "controls": [
            { "control_id": "CC1.1", "title": "before\u001b[2Jafter", "evidence_classes": [
                { "class": "commits", "requirement": "required" }
            ] }
        ]
    }"#;
    let (_temp, path) = write_temp("evil.json", catalog);
    let output = egregore()
        .args([
            "audit",
            "control-catalog",
            "--catalog",
            path.to_str().expect("utf8 path"),
            "--format",
            "text",
        ])
        .output()
        .expect("run");
    assert_eq!(
        output.status.code(),
        Some(0),
        "the catalog itself is schema-valid"
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        !stdout.contains('\u{1b}'),
        "an ESC byte must never reach the terminal"
    );
    assert!(stdout.contains("before"), "title text still rendered");
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
fn duplicate_evidence_class_exits_two_and_names_class() {
    let bad = r#"{
        "catalog_id": "x",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
        "controls": [
            { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                { "class": "commits", "requirement": "required" },
                { "class": "commits", "requirement": "optional" }
            ] }
        ]
    }"#;
    let (_temp, path) = write_temp("dup_class.json", bad);
    let output = egregore()
        .args(["audit", "control-catalog", "--catalog"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr json");
    assert_eq!(err["code"], "duplicate_evidence_class");
    assert_eq!(err["control_id"], "CC1.1");
    assert_eq!(err["class"], "commits");
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
fn malformed_catalog_error_is_redaction_safe() {
    // A wrong-type field (string where a u32 is expected) makes serde name the
    // offending value in its raw message. The stderr error envelope must expose
    // only a stable code plus a value-free location, never the catalog value —
    // the module's redaction-safe error contract (Codex P2, round 4).
    let bad = r#"{
        "catalog_id": "x",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": "LEAK_SENTINEL_9271" },
        "controls": []
    }"#;
    let (_temp, path) = write_temp("wrong_type_field.json", bad);
    let output = egregore()
        .args(["audit", "control-catalog", "--catalog"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("LEAK_SENTINEL_9271"),
        "stderr must not echo the catalog field value: {stderr}"
    );
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr json");
    assert_eq!(err["code"], "malformed_json");
    assert!(err.get("line").is_some(), "envelope must carry line");
    assert!(err.get("column").is_some(), "envelope must carry column");
    assert!(
        err.get("category").is_some(),
        "envelope must carry category"
    );
}

#[test]
fn unknown_field_exits_two_and_reports_malformed_json() {
    // An off-schema catalog with an extra top-level key must be rejected, not
    // silently normalized — otherwise its hash-pin would collide with the
    // shipped document (issue #337).
    let bad = r#"{
        "catalog_id": "x",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
        "controls": [],
        "extra_field": 1
    }"#;
    let (_temp, path) = write_temp("unknown_field.json", bad);
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
fn future_version_catalog_exits_two_and_reports_unknown_schema_version() {
    // A future catalog that bumps schema_version AND adds fields must report
    // `unknown_schema_version` (version gate runs before the strict v1 shape),
    // not `malformed_json` (Codex P2, round 3).
    let bad = r#"{
        "catalog_id": "x",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 2 },
        "controls": [],
        "extra_field": 1
    }"#;
    let (_temp, path) = write_temp("future_version_extra.json", bad);
    let output = egregore()
        .args(["audit", "control-catalog", "--catalog"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr json");
    assert_eq!(err["code"], "unknown_schema_version");
    assert_ne!(err["code"], "malformed_json");
    assert_eq!(err["version"]["domain"], "control_catalog");
    assert_eq!(err["version"]["kind"], "ControlCatalog");
    assert_eq!(err["version"]["version"], 2);
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
