//! Integration tests for `eg audit evidence-pack` (issue #338): control-scoped,
//! time-windowed evidence-pack assembly and offline re-verification.
//!
//! Covers the success-metric fixture (100% in-window recall, 0 out-of-window
//! leakage, three planted `merged_pr_without_approving_review` gaps, exit 1 at
//! the default review-coverage threshold), determinism, the exit-2 usage
//! surfaces, `verify` on clean vs tampered packs, and `--graph` vs `--data-dir`
//! equivalence.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

const FROM: &str = "2026-03-01T00:00:00Z";
const TO: &str = "2026-04-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/evidence_pack/seed.graph.jsonl")
}

fn assemble_over_graph() -> (i32, Value) {
    let output = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            FROM,
            "--to",
            TO,
            "--graph",
        ])
        .arg(fixture_path())
        .output()
        .expect("run assemble");
    let code = output.status.code().expect("exit code");
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    (code, value)
}

#[test]
fn assemble_fails_review_coverage_and_plants_three_gaps() {
    let (code, pack) = assemble_over_graph();
    assert_eq!(code, 1, "review coverage below 1.0 must exit 1");
    assert_eq!(pack["verdicts"]["ok"], false);
    assert_eq!(pack["verdicts"]["review_coverage"]["passed"], false);
    assert_eq!(pack["verdicts"]["required_classes"]["passed"], true);
    assert_eq!(pack["verdicts"]["citation"]["passed"], true);

    let mut gap_ids: Vec<&str> = pack["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["gap_class"] == "merged_pr_without_approving_review")
        .map(|g| g["record_ids"][0].as_str().unwrap())
        .collect();
    gap_ids.sort_unstable();
    assert_eq!(
        gap_ids,
        ["project:v1:pr04", "project:v1:pr05", "project:v1:pr06"]
    );
}

#[test]
fn recall_is_total_and_no_out_of_window_leakage() {
    let (_code, pack) = assemble_over_graph();
    let sections = pack["sections"].as_array().unwrap();
    let commits = sections.iter().find(|s| s["class"] == "commits").unwrap();
    assert_eq!(commits["record_count"], 14);
    // No out-of-window commit (cf*/ca*) leaked into the section.
    for r in commits["records"].as_array().unwrap() {
        let id = r["record"]["id"].as_str().unwrap();
        assert!(!id.contains(":cf") && !id.contains(":ca"), "leak: {id}");
    }
    // Every catalog class is present as a section.
    let classes: Vec<&str> = sections
        .iter()
        .map(|s| s["class"].as_str().unwrap())
        .collect();
    assert_eq!(
        classes,
        [
            "commits",
            "pull_requests",
            "reviews",
            "review_coverage",
            "structural_deltas",
            "public_api_deltas",
            "validation_runs",
            "verification_evidence"
        ]
    );
}

#[test]
fn disclaimer_is_verbatim_and_no_raw_payload() {
    let (_code, pack) = assemble_over_graph();
    assert_eq!(
        pack["manifest"]["disclaimer"],
        "rows are recorded observations of process execution as imported; never proof of control effectiveness, compliance, or completeness; absence of a record means no imported evidence, not no event; not an auditor opinion"
    );
    // Serialized pack must not carry raw emails or secret markers; the #116
    // redaction marker replaces the raw commit author address.
    let raw = serde_json::to_string(&pack).unwrap();
    assert!(!raw.contains("@example.com"));
    assert!(!raw.contains("BEGIN RSA PRIVATE KEY"));
    assert!(raw.contains("<REDACTED:email:"));
}

#[test]
fn assemble_output_is_byte_identical_across_runs() {
    let run = || {
        egregore()
            .args([
                "audit",
                "evidence-pack",
                "assemble",
                "--control",
                "CC8.1",
                "--from",
                FROM,
                "--to",
                TO,
                "--graph",
            ])
            .arg(fixture_path())
            .output()
            .expect("run")
            .stdout
    };
    let first = run();
    for _ in 0..4 {
        assert_eq!(first, run(), "assemble output must be byte-identical");
    }
}

#[test]
fn empty_window_is_vacuous_success() {
    let output = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-02T00:00:00Z",
            "--graph",
        ])
        .arg(fixture_path())
        .output()
        .expect("run");
    assert_eq!(
        output.status.code(),
        Some(0),
        "empty window is vacuous success"
    );
    let pack: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(pack["verdicts"]["ok"], true);
    for s in pack["sections"].as_array().unwrap() {
        assert_eq!(s["record_count"], 0);
    }
}

#[test]
fn unknown_control_exits_2_and_names_known_ids() {
    let output = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "ZZ9.9",
            "--from",
            FROM,
            "--to",
            TO,
            "--graph",
        ])
        .arg(fixture_path())
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(err["code"], "unknown_control");
    assert_eq!(
        err["known_controls"],
        serde_json::json!(["CC7.2", "CC7.3", "CC8.1"])
    );
}

#[test]
fn reversed_and_invalid_windows_exit_2() {
    let reversed = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            TO,
            "--to",
            FROM,
            "--graph",
        ])
        .arg(fixture_path())
        .output()
        .expect("run");
    assert_eq!(reversed.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&reversed.stderr).unwrap();
    assert_eq!(err["code"], "reversed_window");

    let invalid = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            "not-a-time",
            "--to",
            TO,
            "--graph",
        ])
        .arg(fixture_path())
        .output()
        .expect("run");
    assert_eq!(invalid.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(err["code"], "invalid_timestamp");
}

#[test]
fn both_and_neither_input_flags_exit_2() {
    let neither = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            FROM,
            "--to",
            TO,
        ])
        .output()
        .expect("run");
    assert_eq!(neither.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&neither.stderr).unwrap();
    assert_eq!(err["code"], "missing_input_flag");

    let both = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            FROM,
            "--to",
            TO,
            "--graph",
        ])
        .arg(fixture_path())
        .args(["--data-dir", "/tmp/eg-nonexistent-338"])
        .output()
        .expect("run");
    assert_eq!(both.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&both.stderr).unwrap();
    assert_eq!(err["code"], "conflicting_input_flags");
}

#[test]
fn verify_passes_clean_pack_and_fails_tampered() {
    let temp = tempfile::tempdir().unwrap();
    let pack_path = temp.path().join("pack.json");
    let (_code, pack) = assemble_over_graph();
    fs::write(&pack_path, serde_json::to_string(&pack).unwrap()).unwrap();

    let clean = egregore()
        .args(["audit", "evidence-pack", "verify"])
        .arg(&pack_path)
        .output()
        .expect("verify clean");
    assert_eq!(clean.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(report["ok"], true);

    // Flip a single byte in a stored hash.
    let mut tampered = pack;
    let sections = tampered["sections"].as_array_mut().unwrap();
    'outer: for s in sections {
        let records = s["records"].as_array_mut().unwrap();
        if let Some(first) = records.first_mut() {
            let h = first["hash"].as_str().unwrap().to_owned();
            let last = h.chars().last().unwrap();
            let flipped = format!(
                "{}{}",
                &h[..h.len() - 1],
                if last == 'a' { 'b' } else { 'a' }
            );
            first["hash"] = Value::String(flipped);
            break 'outer;
        }
    }
    let tamper_path = temp.path().join("tamper.json");
    fs::write(&tamper_path, serde_json::to_string(&tampered).unwrap()).unwrap();
    let bad = egregore()
        .args(["audit", "evidence-pack", "verify"])
        .arg(&tamper_path)
        .output()
        .expect("verify tampered");
    assert_eq!(bad.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&bad.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["integrity"]["passed"], false);
}

/// Codex round-11 Finding B: `verify` on a malformed pack whose parse fails on a
/// wrong-typed field carrying a secret must NOT echo the secret in the
/// `pack_parse_error` envelope. serde's `Error::to_string()` embeds the offending
/// VALUE for type errors, so the handler must emit a sanitized envelope — a stable
/// category plus 1-based line/column, never the raw serde message — mirroring the
/// catalog parser. Exit code stays 2.
#[test]
fn verify_malformed_pack_does_not_leak_secret_in_parse_error() {
    const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";
    let temp = tempfile::tempdir().unwrap();
    let (_code, pack) = assemble_over_graph();
    let mut json = pack;
    // `excluded_missing_valid_time` is a usize; a string with a secret triggers a
    // serde type (Data) error whose Display embeds the offending value.
    json["manifest"]["excluded_missing_valid_time"] = Value::String(SECRET.to_owned());
    let path = temp.path().join("malformed.json");
    fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

    let out = egregore()
        .args(["audit", "evidence-pack", "verify"])
        .arg(&path)
        .output()
        .expect("run verify");
    assert_eq!(out.status.code(), Some(2), "malformed pack is a load error");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(SECRET),
        "parse error must not leak the wrong-typed field value: {stderr}"
    );
    let err: Value = serde_json::from_slice(&out.stderr).expect("stderr is JSON");
    assert_eq!(err["code"], "pack_parse_error");
    assert!(err["line"].is_number(), "carries a line: {err}");
    assert!(err["column"].is_number(), "carries a column: {err}");
    assert!(
        err["category"].is_string(),
        "carries a stable category: {err}"
    );
    // The raw serde message (which leaks the value) must not be present.
    assert!(
        err.get("message").is_none(),
        "sanitized envelope must not carry the raw serde message: {err}"
    );
}

#[test]
fn verify_unreadable_pack_exits_2() {
    let missing = egregore()
        .args([
            "audit",
            "evidence-pack",
            "verify",
            "/tmp/eg-338-does-not-exist.json",
        ])
        .output()
        .expect("run");
    assert_eq!(missing.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(err["code"], "pack_read_error");
}

/// Codex finding 2 / AC6: an empty or whitespace-only `--graph` (zero records
/// loaded) is a LOAD error — exit 2 with a machine-readable code naming the
/// path — never the exit-1 "required class unavailable" path.
#[test]
fn empty_graph_input_exits_2_and_names_path() {
    let temp = tempfile::tempdir().unwrap();
    let empty = temp.path().join("empty.graph.jsonl");
    // Whitespace-only content: the JSONL reader skips blank lines, so zero
    // records load.
    fs::write(&empty, "\n   \n\n").unwrap();

    let output = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            FROM,
            "--to",
            TO,
            "--graph",
        ])
        .arg(&empty)
        .output()
        .expect("run");
    assert_eq!(
        output.status.code(),
        Some(2),
        "empty evidence input is a load error, not exit 1"
    );
    let err: Value = serde_json::from_slice(&output.stderr).expect("stderr is JSON");
    assert_eq!(err["code"], "empty_evidence_input");
    assert_eq!(
        err["path"].as_str().unwrap(),
        empty.display().to_string(),
        "the error must name the source path"
    );
}

/// Guard against regressing the vacuous-success case: a NON-empty store whose
/// records simply fall outside the window still exits 0 (`empty_window`), never
/// the new exit-2 empty-input path.
#[test]
fn out_of_window_nonempty_store_still_exits_0() {
    let output = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-02T00:00:00Z",
            "--graph",
        ])
        .arg(fixture_path())
        .output()
        .expect("run");
    assert_eq!(
        output.status.code(),
        Some(0),
        "non-empty store with no in-window records is a vacuous success"
    );
}

/// Codex round-12 P1 (Finding 1): `verify` must scan the RAW supplied artifact
/// BEFORE serde drops unknown fields. serde ignores unknown object keys by
/// default, so a secret placed in an unknown field (top-level or nested) is
/// dropped before the whole-artifact Safety scan runs, letting verify exit 0 on
/// an artifact that visibly contains a secret. Scanning the raw file text closes
/// the gap: Safety FAILS (exit 1) with a redaction-safe detail, and neither
/// stdout nor stderr echoes the secret value.
#[test]
fn verify_raw_scan_catches_secret_in_unknown_field() {
    const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";
    let temp = tempfile::tempdir().unwrap();
    let (_code, pack) = assemble_over_graph();
    let mut json = pack;
    // An unknown object key serde silently discards when deserializing into
    // `EvidencePack` — so `verify_pack`'s whole-artifact scan never sees it.
    json["leak"] = Value::String(SECRET.to_owned());
    let path = temp.path().join("unknown_field_leak.json");
    fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

    let out = egregore()
        .args(["audit", "evidence-pack", "verify"])
        .arg(&path)
        .output()
        .expect("run verify");

    assert_eq!(
        out.status.code(),
        Some(1),
        "a secret in the raw artifact fails Safety (exit 1)"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains(SECRET),
        "verify stdout must not echo the secret: {stdout}"
    );
    assert!(
        !stderr.contains(SECRET),
        "verify stderr must not echo the secret: {stderr}"
    );
    let report: Value = serde_json::from_slice(&out.stdout).expect("stdout is the JSON report");
    assert_eq!(report["ok"], false, "raw-scan secret fails the report");
    assert_eq!(
        report["safety"]["passed"], false,
        "raw-scan secret fails Safety"
    );
}

/// Codex round-12 P1 (Finding 2): a whole-artifact SAFETY failure means the
/// assembled pack is unsafe to emit — its offending field still carries the raw
/// secret. The handler must NOT serialize/print the pack. Instead it emits a
/// redaction-safe `pack_safety_failed` error to stderr naming the failing field
/// and secret class, never the value, and exits 2 because it cannot emit a
/// redaction-safe artifact. stdout must stay empty of the pack.
#[test]
fn assemble_safety_failure_suppresses_pack_and_errors() {
    const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";
    let temp = tempfile::tempdir().unwrap();
    // A valid catalog whose CC8.1 title carries an AWS-key-shaped secret, as a
    // malicious `--catalog` would. `control.title` is copied into the manifest's
    // `control_title`, which the whole-artifact Safety scan covers.
    let catalog = serde_json::json!({
        "catalog_id": "soc2-v1",
        "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
        "controls": [
            {
                "control_id": "CC8.1",
                "title": format!("Change management {SECRET}"),
                "evidence_classes": [
                    { "class": "commits", "requirement": "required" },
                    { "class": "pull_requests", "requirement": "required" },
                    { "class": "reviews", "requirement": "required" },
                    { "class": "review_coverage", "requirement": "required" }
                ]
            }
        ]
    });
    let catalog_path = temp.path().join("evil-catalog.json");
    fs::write(&catalog_path, serde_json::to_string(&catalog).unwrap()).unwrap();

    let out = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            FROM,
            "--to",
            TO,
            "--catalog",
        ])
        .arg(&catalog_path)
        .arg("--graph")
        .arg(fixture_path())
        .output()
        .expect("run assemble");

    assert_eq!(
        out.status.code(),
        Some(2),
        "a safety failure suppresses the artifact (exit 2)"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains(SECRET),
        "stdout must not leak the secret: {stdout}"
    );
    assert!(
        !stderr.contains(SECRET),
        "stderr must not leak the secret: {stderr}"
    );
    assert!(
        stdout.trim().is_empty(),
        "no pack may be printed to stdout on a safety failure: {stdout}"
    );
    let err: Value = serde_json::from_slice(&out.stderr).expect("stderr is JSON");
    assert_eq!(err["code"], "pack_safety_failed");
    assert!(
        err["detail"].as_str().unwrap().contains("control_title"),
        "the error names the failing field: {err}"
    );
}

/// Regression (Codex round-12 P1 Finding 2): the safety-failure suppression must
/// apply ONLY to Safety failures. A pack that fails a NON-safety verdict (here
/// review coverage, with no secret anywhere) must STILL print the full report to
/// stdout at exit 1, exactly as before.
#[test]
fn assemble_nonsafety_verdict_failure_still_prints_report() {
    let (code, pack) = assemble_over_graph();
    assert_eq!(code, 1, "a review-coverage-only failure stays exit 1");
    assert_eq!(pack["verdicts"]["ok"], false);
    assert_eq!(
        pack["verdicts"]["safety"]["passed"], true,
        "no secret: Safety passes"
    );
    assert_eq!(pack["verdicts"]["review_coverage"]["passed"], false);
    assert!(
        !pack["sections"].as_array().unwrap().is_empty(),
        "the full report (with sections) was printed to stdout"
    );
}

/// `--graph` and `--data-dir` must produce a byte-identical pack. Only built
/// when the embedded store backend is compiled in.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn graph_and_data_dir_are_byte_identical() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("store");

    let ingest = egregore()
        .arg("ingest")
        .arg(fixture_path())
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("ingest");
    assert!(
        ingest.status.success(),
        "ingest failed: {}",
        String::from_utf8_lossy(&ingest.stderr)
    );

    let graph_out = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            FROM,
            "--to",
            TO,
            "--graph",
        ])
        .arg(fixture_path())
        .output()
        .expect("assemble graph")
        .stdout;

    let store_out = egregore()
        .args([
            "audit",
            "evidence-pack",
            "assemble",
            "--control",
            "CC8.1",
            "--from",
            FROM,
            "--to",
            TO,
            "--data-dir",
        ])
        .arg(&data_dir)
        .output()
        .expect("assemble data-dir")
        .stdout;

    assert_eq!(
        graph_out, store_out,
        "--graph and --data-dir packs must be byte-identical"
    );
}
