#![allow(missing_docs)]

//! Issue #239: `eg verify-scan` — byte-for-byte stable re-scan as a runnable
//! check. These tests pin producer-relative inputs (transaction time and
//! repository identity) the same way the command does, so the only thing under
//! test is the scan's own determinism.

use std::path::PathBuf;

use aletheia_egregore::verify_scan::{
    RecordDifferenceKind, VerifyScanInputs, compare_scan_outputs, run_verify_scan,
    verify_scan_exit_code,
};
use aletheia_egregore::{scan_repository_at_with_exclusions, scan_repository_at_with_override};
use assert_cmd::Command;
use predicates::prelude::*;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

const REPO_ID: &str = "verify-scan-test-repo";
const FIXED_TIME: &str = "2026-05-19T00:00:00Z";

fn scan_fixture_jsonl() -> String {
    scan_repository_at_with_override(fixture_repo(), FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize")
}

#[test]
fn verify_scan_cli_reports_stable_on_deterministic_fixture() {
    let output = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("verify-scan")
        .arg(fixture_repo())
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value =
        serde_json::from_str(std::str::from_utf8(&output).expect("stdout should be UTF-8"))
            .expect("stdout should be a JSON report");
    assert_eq!(report["ok"], serde_json::Value::Bool(true));
    assert_eq!(report["stable"], serde_json::Value::Bool(true));
    let records = report["first"]["records"]
        .as_u64()
        .expect("report should carry a record count");
    assert!(records > 0, "fixture scan should produce records");
    assert_eq!(
        report["first"], report["second"],
        "both pinned scans should report identical counts"
    );
    assert_eq!(report["difference_count"], 0);
    assert_eq!(report["differences"], serde_json::Value::Array(Vec::new()));
}

#[test]
fn verify_scan_cli_text_format_reports_stable() {
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("verify-scan")
        .arg(fixture_repo())
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .stdout(predicate::str::contains("stable: true"));
}

#[test]
fn verify_scan_cli_is_stable_without_repo_id_override() {
    // Same checkout directory scanned twice: the auto-detected identity is
    // identical for both scans, so no override is needed for self-consistency.
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("verify-scan")
        .arg(fixture_repo())
        .assert()
        .success()
        .stdout(predicate::str::contains("\"stable\": true"));
}

#[test]
fn compare_reports_stable_for_identical_scans() {
    let jsonl = scan_fixture_jsonl();
    let comparison = compare_scan_outputs(&jsonl, &jsonl);
    assert!(comparison.stable, "identical output should compare stable");
    assert_eq!(comparison.difference_count, 0);
    assert!(comparison.differences.is_empty());
    assert!(comparison.first.records > 0);
    assert_eq!(comparison.first.records, comparison.second.records);
    assert!(
        comparison.first.nodes + comparison.first.edges <= comparison.first.records,
        "nodes + edges should not exceed the record count"
    );
}

#[test]
fn compare_detects_injected_field_mutation() {
    let first = scan_fixture_jsonl();
    let mut lines: Vec<String> = first.lines().map(str::to_owned).collect();
    let idx = lines
        .iter()
        .position(|line| line.contains(r#""kind":"Symbol""#) && line.contains(r#""name":"#))
        .expect("fixture should contain a named Symbol record");
    let mut record: serde_json::Value =
        serde_json::from_str(&lines[idx]).expect("record line should parse");
    let handle = record
        .get("id")
        .and_then(|id| id.as_str())
        .expect("record should carry a string id")
        .to_owned();
    let name = record
        .get("name")
        .and_then(|name| name.as_str())
        .expect("symbol should carry a name")
        .to_owned();
    record["name"] = serde_json::Value::String(format!("{name}_mutated"));
    lines[idx] = serde_json::to_string(&record).expect("record should serialize");
    lines.sort_unstable();
    let second = format!("{}\n", lines.join("\n"));

    let comparison = compare_scan_outputs(&first, &second);
    assert!(
        !comparison.stable,
        "a mutated field should compare unstable"
    );
    assert_eq!(comparison.difference_count, 1);
    assert_eq!(comparison.differences.len(), 1);
    let difference = &comparison.differences[0];
    assert_eq!(difference.handle, handle);
    assert_eq!(difference.kind, RecordDifferenceKind::FieldMismatch);
    assert_eq!(difference.field.as_deref(), Some("name"));
}

#[test]
fn compare_detects_dropped_record() {
    let first = scan_fixture_jsonl();
    let mut lines: Vec<String> = first.lines().map(str::to_owned).collect();
    let dropped: serde_json::Value =
        serde_json::from_str(&lines[3]).expect("record line should parse");
    let handle = dropped
        .get("id")
        .and_then(|id| id.as_str())
        .expect("record should carry a string id")
        .to_owned();
    lines.remove(3);
    let second = format!("{}\n", lines.join("\n"));

    let comparison = compare_scan_outputs(&first, &second);
    assert!(
        !comparison.stable,
        "a dropped record should compare unstable"
    );
    assert_eq!(comparison.difference_count, 1);
    assert!(
        comparison
            .differences
            .iter()
            .any(|difference| difference.handle == handle
                && difference.kind == RecordDifferenceKind::MissingInSecond),
        "dropped record handle should be reported as missing in the second scan"
    );
}

#[test]
fn compare_detects_added_record() {
    let first = scan_fixture_jsonl();
    let mut lines: Vec<String> = first.lines().map(str::to_owned).collect();
    let added: serde_json::Value =
        serde_json::from_str(&lines[3]).expect("record line should parse");
    let handle = added
        .get("id")
        .and_then(|id| id.as_str())
        .expect("record should carry a string id")
        .to_owned();
    // Duplicate the line with a fresh id so it parses as a genuinely new record.
    let mut injected = added;
    injected["id"] = serde_json::Value::String(format!("{handle}-injected"));
    lines.push(serde_json::to_string(&injected).expect("record should serialize"));
    lines.sort_unstable();
    let second = format!("{}\n", lines.join("\n"));

    let comparison = compare_scan_outputs(&first, &second);
    assert!(
        !comparison.stable,
        "an added record should compare unstable"
    );
    assert!(
        comparison
            .differences
            .iter()
            .any(|difference| difference.handle.starts_with(&handle)
                && difference.kind == RecordDifferenceKind::MissingInFirst),
        "added record handle should be reported as missing in the first scan"
    );
}

/// The command under test: `run_verify_scan` is the injectable driver behind
/// `eg verify-scan`. Injecting nondeterminism (the second scan mutates one
/// field) must make the command report exit 1, `stable: false`, the offending
/// handle, the first differing field, and the first differing line.
#[test]
fn verify_scan_command_reports_unstable_with_injected_nondeterminism() {
    let first_jsonl = scan_fixture_jsonl();
    let lines: Vec<&str> = first_jsonl.lines().collect();
    let idx = lines
        .iter()
        .position(|line| line.contains(r#""kind":"Symbol""#) && line.contains(r#""name":"#))
        .expect("fixture should contain a named Symbol record");
    let record: serde_json::Value =
        serde_json::from_str(lines[idx]).expect("record line should parse");
    let handle = record
        .get("id")
        .and_then(|id| id.as_str())
        .expect("record should carry a string id")
        .to_owned();
    let name = record
        .get("name")
        .and_then(|name| name.as_str())
        .expect("symbol should carry a name")
        .to_owned();
    let line_number = idx as u64 + 1;

    let mut mutated = record;
    mutated["name"] = serde_json::Value::String(format!("{name}_mutated"));
    let mut second_lines: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    second_lines[idx] = serde_json::to_string(&mutated).expect("record should serialize");
    let second_jsonl = format!("{}\n", second_lines.join("\n"));

    let mut calls = 0;
    let mut scan = |_inputs: &VerifyScanInputs| -> Result<String, String> {
        calls += 1;
        Ok(if calls == 1 {
            first_jsonl.clone()
        } else {
            second_jsonl.clone()
        })
    };
    let report = run_verify_scan(fixture_repo().as_path(), Some(REPO_ID), &mut scan)
        .expect("command driver should run");
    assert_eq!(calls, 2, "the command must scan exactly twice");

    // The command reports the unstable verdict and its exit code.
    assert!(!report.ok, "injected mutation should report ok: false");
    assert!(!report.comparison.stable);
    assert_eq!(
        verify_scan_exit_code(report.ok),
        1,
        "an unstable command reports exit 1"
    );

    // The JSON report names the offending handle, the first differing
    // field, and the first differing line.
    let rendered = report.render_json().expect("report should serialize");
    let parsed: serde_json::Value =
        serde_json::from_str(&rendered).expect("rendered report should parse");
    assert_eq!(parsed["ok"], serde_json::Value::Bool(false));
    assert_eq!(parsed["stable"], serde_json::Value::Bool(false));
    assert_eq!(
        parsed["differences"]
            .as_array()
            .expect("differences should be an array")
            .len(),
        1
    );
    let difference = &parsed["differences"][0];
    assert_eq!(
        difference["handle"],
        serde_json::Value::String(handle.clone())
    );
    assert_eq!(
        difference["field"],
        serde_json::Value::String("name".to_owned())
    );
    assert_eq!(difference["line"], serde_json::Value::from(line_number));
    assert_eq!(
        parsed["first_differing_line"],
        serde_json::Value::from(line_number)
    );
    assert!(parsed["first_differing_byte"].as_u64().is_some());

    // The text rendering carries the same verdict and location.
    let text = report.render_text();
    assert!(
        text.contains("stable: false"),
        "text should report unstable"
    );
    assert!(text.contains(&handle), "text should name the handle");
    assert!(
        text.contains(&format!("line {line_number}")),
        "text should name the differing line"
    );
}

/// Composition with issue #242 at the command level: the full `eg
/// verify-scan` path — pinned transaction time and repository identity plus
/// the real scan/redaction pipeline — over CRLF source files must report
/// stable.
#[test]
fn verify_scan_cli_is_stable_over_crlf_content() {
    let code = "pub struct Widget {\n    pub value: usize,\n}\n\nimpl Widget {\n    pub fn new(value: usize) -> Self {\n        Self { value }\n    }\n}\n";
    let temp = tempfile::tempdir().expect("temp dir should be created");
    std::fs::write(temp.path().join("widget.rs"), code.replace('\n', "\r\n"))
        .expect("CRLF file should be written");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("verify-scan")
        .arg(temp.path())
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"stable\": true"));
}

/// Composition with issue #242 (line-ending normalization): a CRLF checkout
/// and an LF checkout of the same content must produce byte-identical scan
/// output once transaction time and repository identity are pinned.
#[test]
fn crlf_and_lf_checkouts_produce_identical_scan_output() {
    let code = "pub struct Widget {\n    pub value: usize,\n}\n\nimpl Widget {\n    pub fn new(value: usize) -> Self {\n        Self { value }\n    }\n}\n";
    let temp_lf = tempfile::tempdir().expect("temp dir LF should be created");
    let temp_crlf = tempfile::tempdir().expect("temp dir CRLF should be created");
    std::fs::write(temp_lf.path().join("widget.rs"), code).expect("LF file should be written");
    std::fs::write(
        temp_crlf.path().join("widget.rs"),
        code.replace('\n', "\r\n"),
    )
    .expect("CRLF file should be written");

    let mut previous: Option<String> = None;
    for dir in [temp_lf.path(), temp_crlf.path()] {
        for _ in 0..2 {
            let jsonl = scan_repository_at_with_exclusions(
                dir,
                FIXED_TIME,
                Some(REPO_ID),
                &[] as &[String],
            )
            .expect("checkout should scan")
            .to_jsonl()
            .expect("graph should serialize");
            if let Some(previous_jsonl) = &previous {
                assert_eq!(
                    previous_jsonl, &jsonl,
                    "LF and CRLF checkouts must produce byte-for-byte identical scan output"
                );
                assert!(
                    compare_scan_outputs(previous_jsonl, &jsonl).stable,
                    "LF/CRLF comparison should report stable"
                );
            }
            previous = Some(jsonl);
        }
    }
}
