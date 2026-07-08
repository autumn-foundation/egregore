//! Integration tests for the at-import redaction report (issue #266).
//!
//! Written RED-first: they drive a verifiable, secret-free summary of exactly
//! what an `import-traj` / `import-codex` pass redacted. The report must
//! account for every seeded secret with its `secret_class`, field path,
//! owning record ID, and marker `hash_prefix` — and never a raw secret value.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

use aletheia_egregore::{
    codex,
    redaction::parse_redaction_markers,
    redaction_report::{
        REDACTION_DISABLED, REDACTION_ENABLED, RedactionReport, build_redaction_report,
    },
    traj,
};

// ── Seeded secret fixtures ────────────────────────────────────────────────────
//
// Constants holding secret-shaped values the importers must redact. Raw values
// exist only in the temp fixture files written per test — never in any report.

const SECRET_ENV_COMMAND: &str = "export STRIPE_TOKEN=supersecretvalue123";
const SECRET_ENV_RAW: &str = "supersecretvalue123";
const SECRET_CLOUD_STDOUT: &str = "leaked AKIAIOSFODNN7EXAMPLE creds";
const SECRET_CLOUD_RAW: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET_BEARER_RAW: &str = "abcdefghijklmnopqrstuvwxyz123456";
const SECRET_SK_RAW: &str = "sk-prod-abcdefghijklmnopqrstuvwxyz1234567890XXXX";

const CLEAN_TRAJ_FIXTURE: &str = "tests/fixtures/agent_memory/swe_agent_basic/trajectory.traj";

// ── Fixture writers ───────────────────────────────────────────────────────────

/// Writes a `.traj` fixture seeding two secrets across two classes:
/// an `env_secret` in the bash command and a `cloud_credential` in stdout
/// (surfaced through the command-failure path because exit code is 1).
fn write_secret_traj(dir: &Path) -> PathBuf {
    let traj_json = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {
            "model_name": "test-model",
            "exit_reason": "submitted",
            "outcome": "success",
            "started_at": "2026-01-01T00:00:00Z"
        },
        "messages": [
            {
                "role": "assistant",
                "content": format!("Set the key.\n\n```bash\n{SECRET_ENV_COMMAND}\n```"),
                "extra": {
                    "timestamp": "2026-01-01T00:00:01Z",
                    "actions": [SECRET_ENV_COMMAND]
                }
            },
            {
                "role": "user",
                "content": "observation",
                "extra": {
                    "run_result": {
                        "exit_code": 1,
                        "stdout": SECRET_CLOUD_STDOUT,
                        "stderr": ""
                    }
                }
            }
        ]
    });
    let path = dir.join("secret.traj");
    fs::write(
        &path,
        serde_json::to_string(&traj_json).expect("serialize traj"),
    )
    .expect("write traj fixture");
    path
}

/// Writes a Codex session JSONL fixture seeding an `api_token` in the shell
/// command (Bearer header) and another `api_token` (sk- key) in stdout.
fn write_secret_codex(dir: &Path) -> PathBuf {
    let arguments = serde_json::json!({
        "cmd": ["bash", "-lc", format!("curl -H 'Authorization: Bearer {SECRET_BEARER_RAW}'")]
    })
    .to_string();
    let output = serde_json::json!({
        "exit_code": 0,
        "stdout": format!("token {SECRET_SK_RAW} ok"),
        "stderr": ""
    })
    .to_string();
    let lines = [
        serde_json::json!({
            "type": "session",
            "id": "sess-1",
            "model": "codex-test",
            "created_at": "2026-01-01T00:00:00Z"
        }),
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "running"}],
            "id": "m1",
            "status": "completed"
        }),
        serde_json::json!({
            "type": "function_call",
            "call_id": "c1",
            "name": "shell",
            "arguments": arguments
        }),
        serde_json::json!({
            "type": "function_call_output",
            "call_id": "c1",
            "output": output
        }),
    ];
    let body = lines
        .iter()
        .map(|l| serde_json::to_string(l).expect("serialize codex line"))
        .collect::<Vec<_>>()
        .join("\n");
    let path = dir.join("secret-session.jsonl");
    fs::write(&path, body).expect("write codex fixture");
    path
}

fn import_secret_traj_report(dir: &Path) -> RedactionReport {
    let path = write_secret_traj(dir);
    let opts = traj::ImportOptions::default();
    let graph = traj::import_traj(&path, &opts).expect("traj import must succeed");
    build_redaction_report(graph.records(), opts.policy_version)
}

// ── Marker parsing (report building block) ────────────────────────────────────

#[test]
fn parse_markers_extracts_class_and_hash_prefix() {
    let value = "prefix <REDACTED:api_token:abc123def456> suffix";
    let markers = parse_redaction_markers(value);
    assert_eq!(markers.len(), 1, "one embedded marker must be found");
    let (class, hash) = &markers[0];
    assert_eq!(class.as_str(), "api_token");
    assert_eq!(hash, "abc123def456");
}

#[test]
fn parse_markers_ignores_unknown_class_names() {
    let value = "<REDACTED:bogus_class:abc123def456>";
    assert!(
        parse_redaction_markers(value).is_empty(),
        "a marker-shaped string with an unknown class must not be counted"
    );
}

#[test]
fn parse_markers_ignores_non_hex_hash() {
    let value = "<REDACTED:api_token:NOT-A-HASH!>";
    assert!(
        parse_redaction_markers(value).is_empty(),
        "a marker-shaped string with a non-hex hash must not be counted"
    );
}

#[test]
fn parse_markers_finds_multiple_occurrences() {
    let value = "<REDACTED:email:aaaa11112222> and <REDACTED:env_secret:bbbb33334444>";
    let markers = parse_redaction_markers(value);
    assert_eq!(markers.len(), 2, "both markers must be found");
    assert_eq!(markers[0].0.as_str(), "email");
    assert_eq!(markers[1].0.as_str(), "env_secret");
}

// ── Report accounts for 100% of seeded secrets ────────────────────────────────

#[test]
fn traj_report_accounts_for_seeded_secrets() {
    let temp = tempfile::tempdir().expect("temp dir");
    let report = import_secret_traj_report(temp.path());

    assert_eq!(report.redaction, REDACTION_ENABLED);
    assert_eq!(report.policy_version.as_deref(), Some("v1"));
    assert!(report.total > 0, "seeded secrets must produce entries");
    assert_eq!(report.total, report.entries.len() as u64);

    let classes: Vec<&str> = report.entries.iter().map(|e| e.secret_class).collect();
    assert!(
        classes.contains(&"env_secret"),
        "env_secret in the bash command must be accounted for; got classes {classes:?}"
    );
    assert!(
        classes.contains(&"cloud_credential"),
        "cloud_credential in stdout must be accounted for; got classes {classes:?}"
    );

    // Per-class counts must sum to the total.
    let sum: u64 = report.counts_by_class.values().sum();
    assert_eq!(sum, report.total, "per-class counts must sum to total");

    // Every entry carries an owning record ID, a field path, and a hash prefix
    // tying it to the stored marker.
    for entry in &report.entries {
        assert!(!entry.record_id.is_empty(), "entry must carry record id");
        assert!(!entry.field_path.is_empty(), "entry must carry field path");
        assert_eq!(
            entry.hash_prefix.len(),
            12,
            "hash prefix must be the 12-hex-char audit prefix"
        );
        assert!(
            entry
                .hash_prefix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "hash prefix must be lowercase hex; got {}",
            entry.hash_prefix
        );
    }
}

#[test]
fn traj_report_entries_tie_to_stored_markers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_secret_traj(temp.path());
    let opts = traj::ImportOptions::default();
    let graph = traj::import_traj(&path, &opts).expect("traj import must succeed");
    let report = build_redaction_report(graph.records(), opts.policy_version);
    let jsonl = graph.to_jsonl().expect("serialize graph");

    for entry in &report.entries {
        let marker = format!("<REDACTED:{}:{}>", entry.secret_class, entry.hash_prefix);
        assert!(
            jsonl.contains(&marker),
            "reported marker {marker} must exist in the emitted records"
        );
        assert!(
            jsonl.contains(&entry.record_id),
            "reported record id {} must exist in the emitted records",
            entry.record_id
        );
    }
}

#[test]
fn codex_report_accounts_for_seeded_secrets() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_secret_codex(temp.path());
    let opts = codex::ImportOptions::default();
    let graph = codex::import_codex(&path, &opts).expect("codex import must succeed");
    let report = build_redaction_report(graph.records(), opts.policy_version);

    assert_eq!(report.redaction, REDACTION_ENABLED);
    assert!(report.total >= 2, "both seeded api tokens must be reported");
    assert!(
        report.entries.iter().all(|e| e.secret_class == "api_token"),
        "all seeded secrets are api_token class"
    );
    // The stdout secret lands in the CommandRun stdout handle.
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.field_path == "stdout_handle.inline"),
        "the stdout secret must be reported at its stored field path"
    );
    // The command secret is redacted before landing in the recorded command text.
    assert!(
        report.entries.iter().any(|e| e.field_path == "text"),
        "the command secret must be reported at its stored field path"
    );
}

// ── The report is secret-free ─────────────────────────────────────────────────

#[test]
fn traj_report_contains_zero_raw_secret_substrings() {
    let temp = tempfile::tempdir().expect("temp dir");
    let report = import_secret_traj_report(temp.path());
    let json = serde_json::to_string(&report).expect("serialize report");
    for raw in [SECRET_ENV_RAW, SECRET_CLOUD_RAW, SECRET_ENV_COMMAND] {
        assert!(
            !json.contains(raw),
            "report must never contain raw secret substring {raw}"
        );
    }
}

#[test]
fn codex_report_contains_zero_raw_secret_substrings() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_secret_codex(temp.path());
    let opts = codex::ImportOptions::default();
    let graph = codex::import_codex(&path, &opts).expect("codex import must succeed");
    let report = build_redaction_report(graph.records(), opts.policy_version);
    let json = serde_json::to_string(&report).expect("serialize report");
    for raw in [SECRET_BEARER_RAW, SECRET_SK_RAW] {
        assert!(
            !json.contains(raw),
            "report must never contain raw secret substring {raw}"
        );
    }
}

// ── Determinism: byte-identical across repeated runs ─────────────────────────

#[test]
fn traj_report_is_byte_identical_across_five_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let first =
        serde_json::to_string(&import_secret_traj_report(temp.path())).expect("serialize report");
    for run in 1..5 {
        let next = serde_json::to_string(&import_secret_traj_report(temp.path()))
            .expect("serialize report");
        assert_eq!(first, next, "report must be byte-identical on run {run}");
    }
}

// ── Zero-redaction pass is explicit, absent fields are never reported ─────────

#[test]
fn clean_import_emits_zero_total_enabled_report() {
    let opts = traj::ImportOptions::default();
    let graph =
        traj::import_traj(Path::new(CLEAN_TRAJ_FIXTURE), &opts).expect("clean import succeeds");
    let report = build_redaction_report(graph.records(), opts.policy_version);
    assert_eq!(report.redaction, REDACTION_ENABLED);
    assert_eq!(report.policy_version.as_deref(), Some("v1"));
    assert_eq!(report.total, 0, "clean pass must report total 0");
    assert!(
        report.entries.is_empty(),
        "clean pass must report no entries"
    );
    assert!(
        report.counts_by_class.is_empty(),
        "clean pass must report no per-class counts"
    );
}

#[test]
fn absent_fields_are_never_reported_as_redacted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let report = import_secret_traj_report(temp.path());
    // The traj importer never populates stderr handles: an absent field must
    // never appear in the report.
    assert!(
        report
            .entries
            .iter()
            .all(|e| e.field_path != "stderr_handle.inline"),
        "absent stderr_handle.inline must never be reported as redacted"
    );
}

// ── Passthrough import is marked disabled ─────────────────────────────────────

#[test]
fn passthrough_report_is_marked_disabled() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_secret_traj(temp.path());
    let opts = traj::ImportOptions::passthrough();
    let graph = traj::import_traj(&path, &opts).expect("passthrough import succeeds");
    let report = build_redaction_report(graph.records(), opts.policy_version);
    assert_eq!(
        report.redaction, REDACTION_DISABLED,
        "a no-redaction import must be marked disabled, not mistaken for a clean pass"
    );
    assert!(report.policy_version.is_none());
    assert_eq!(report.total, 0);
    assert!(report.entries.is_empty());
}

// ── Report carries the source handle ──────────────────────────────────────────

#[test]
fn report_carries_source_artifact_handle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_secret_traj(temp.path());
    let opts = traj::ImportOptions::default();
    let graph = traj::import_traj(&path, &opts).expect("traj import must succeed");
    let report = build_redaction_report(graph.records(), opts.policy_version);
    assert_eq!(
        report.source_artifact_path.as_deref(),
        Some(path.to_string_lossy().as_ref()),
        "report must name the imported source artifact"
    );
    let hash = report
        .source_artifact_hash
        .as_deref()
        .expect("report must carry the source artifact hash");
    assert_eq!(hash.len(), 64, "source hash must be a full BLAKE3 hex hash");
}

// ── CLI: --redaction-report flag ──────────────────────────────────────────────

#[test]
fn import_traj_cli_writes_redaction_report_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let traj_path = write_secret_traj(temp.path());
    let out = temp.path().join("records.jsonl");
    let report_path = temp.path().join("report.json");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("import-traj")
        .arg(&traj_path)
        .arg("--out")
        .arg(&out)
        .arg("--redaction-report")
        .arg(&report_path)
        .assert()
        .success();

    assert!(out.exists(), "records JSONL must still be written");
    let body = fs::read_to_string(&report_path).expect("report file must be written");
    let report: serde_json::Value = serde_json::from_str(body.trim()).expect("report is JSON");
    assert_eq!(report["redaction"], "enabled");
    assert!(report["total"].as_u64().expect("total present") > 0);
    assert!(report["entries"].is_array());
    assert!(
        !body.contains(SECRET_ENV_RAW) && !body.contains(SECRET_CLOUD_RAW),
        "report file must never contain raw secrets"
    );
}

#[test]
fn import_traj_cli_report_is_byte_identical_across_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let traj_path = write_secret_traj(temp.path());
    let mut bodies = Vec::new();
    for i in 0..2 {
        let out = temp.path().join(format!("records-{i}.jsonl"));
        let report_path = temp.path().join(format!("report-{i}.json"));
        Command::cargo_bin("egregore")
            .expect("binary should run")
            .arg("import-traj")
            .arg(&traj_path)
            .arg("--out")
            .arg(&out)
            .arg("--redaction-report")
            .arg(&report_path)
            .assert()
            .success();
        bodies.push(fs::read(&report_path).expect("report must exist"));
    }
    assert_eq!(
        bodies[0], bodies[1],
        "re-running the same import must produce a byte-identical report"
    );
}

#[test]
fn import_traj_cli_report_dash_writes_stdout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let traj_path = write_secret_traj(temp.path());
    let out = temp.path().join("records.jsonl");

    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("import-traj")
        .arg(&traj_path)
        .arg("--out")
        .arg(&out)
        .arg("--redaction-report")
        .arg("-")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    let report: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be exactly the JSON report");
    assert_eq!(report["redaction"], "enabled");
}

#[test]
fn import_traj_cli_clean_pass_reports_zero_total() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("records.jsonl");
    let report_path = temp.path().join("report.json");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("import-traj")
        .arg(CLEAN_TRAJ_FIXTURE)
        .arg("--out")
        .arg(&out)
        .arg("--redaction-report")
        .arg(&report_path)
        .assert()
        .success();

    let body = fs::read_to_string(&report_path).expect("report must be written even when clean");
    let report: serde_json::Value = serde_json::from_str(body.trim()).expect("report is JSON");
    assert_eq!(report["redaction"], "enabled");
    assert_eq!(
        report["total"], 0,
        "silent clean pass must still be reported"
    );
}

#[test]
fn import_codex_cli_writes_redaction_report_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let codex_path = write_secret_codex(temp.path());
    let out = temp.path().join("records.jsonl");
    let report_path = temp.path().join("report.json");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("import-codex")
        .arg(&codex_path)
        .arg("--out")
        .arg(&out)
        .arg("--redaction-report")
        .arg(&report_path)
        .assert()
        .success();

    let body = fs::read_to_string(&report_path).expect("report file must be written");
    let report: serde_json::Value = serde_json::from_str(body.trim()).expect("report is JSON");
    assert_eq!(report["redaction"], "enabled");
    assert!(report["total"].as_u64().expect("total present") >= 2);
    assert!(
        !body.contains(SECRET_BEARER_RAW) && !body.contains(SECRET_SK_RAW),
        "report file must never contain raw secrets"
    );
}

#[test]
fn import_without_report_flag_is_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("records.jsonl");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("import-traj")
        .arg(CLEAN_TRAJ_FIXTURE)
        .arg("--out")
        .arg(&out)
        .assert()
        .success()
        .stdout(predicate::str::contains("imported"));
    assert!(out.exists());
}
