//! Integration tests for `eg scan-logs` — runtime log-signature extraction
//! (issues #319 / #320). One captured log becomes deterministic,
//! redaction-safe graph records: a `LogSource`, one `ErrorSignature` per
//! `template-v1` fingerprint, capped `LogEvent` exemplars, and hourly
//! `LogOccurrenceBucket` nodes. Raw log text never enters the graph.

#![allow(missing_docs)]

use std::{fs, path::Path};

use aletheia_egregore::log_graph;
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";
const REPO_ID: &str = "log-fixture-repo";
/// Fixed capture timestamp for deterministic protected manifests (issue #321).
const FIXED_CAPTURED_AT: &str = "2026-06-18T00:00:00Z";
/// The secret embedded in the plain-text fixture's `[ERROR]` bootstrap line.
const FIXTURE_SECRET: &str = "hunterSECRETtokenValueLong";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

/// Builds the adversarial plain-text log fixture:
/// - one ERROR repeated 1000× varying only timestamp / PID / UUID / pointer,
/// - a multi-line Rust panic + backtrace (one logical event),
/// - a timestamp-less severity line,
/// - a secret-shaped line,
/// - interleaved INFO/DEBUG noise,
/// - a distinct WARN shape.
fn plain_log() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for i in 0..1000 {
        // Same error shape, only volatile fields vary: minute rolls, PID, UUID,
        // and pointer differ every line but normalize identically.
        let minute = i % 60;
        let pid = 1000 + i;
        let _ = writeln!(
            s,
            "2026-01-02T03:{minute:02}:00Z [ERROR] request {i:08x}-e29b-41d4-a716-446655440000 failed at 0x{i:08x} pid={pid}",
        );
        if i % 100 == 0 {
            s.push_str("2026-01-02T03:00:01Z INFO steady state ok\n");
            s.push_str("2026-01-02T03:00:02Z DEBUG cache warm\n");
        }
    }
    // A distinct WARN shape.
    s.push_str("2026-01-02T03:10:00Z [WARN] disk usage at 91 percent\n");
    // A multi-line panic + backtrace: ONE logical event, no timestamp.
    s.push_str("thread 'worker-7' panicked at 'index out of bounds: the len is 3 but the index is 10', src/pool.rs:88:13\n");
    s.push_str("stack backtrace:\n");
    s.push_str("   0: core::panicking::panic_bounds_check\n");
    s.push_str("   1: app::pool::checkout\n");
    s.push_str("   at src/pool.rs:88\n");
    s.push_str("note: run with `RUST_BACKTRACE=full` for a verbose backtrace\n");
    // A secret-shaped severity line.
    s.push_str("2026-01-02T03:11:00Z [ERROR] auth bootstrap failed API_KEY=hunterSECRETtokenValueLong reason denied\n");
    s
}

/// Builds a jsonl-v1 fixture with two more distinct error shapes.
fn jsonl_log() -> String {
    let mut s = String::new();
    s.push_str(r#"{"timestamp":"2026-01-02T04:00:00Z","level":"error","message":"upstream 503 from 10.0.0.7:443 after 250ms"}"#);
    s.push('\n');
    s.push_str(
        r#"{"ts":"2026-01-02T04:00:01Z","severity":"warn","msg":"queue depth 42 over soft limit"}"#,
    );
    s.push('\n');
    s.push_str(r#"{"time":"2026-01-02T04:00:02Z","level":"info","message":"heartbeat ok"}"#);
    s.push('\n');
    s
}

fn write_fixtures(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let plain = dir.join("app.log");
    let jsonl = dir.join("app.jsonl");
    fs::write(&plain, plain_log()).expect("write plain fixture");
    fs::write(&jsonl, jsonl_log()).expect("write jsonl fixture");
    (plain, jsonl)
}

/// Runs `scan_log_records` directly with a fixed transaction time and returns
/// the stamped, canonical JSONL.
fn scan_to_jsonl(log_path: &Path, repo_root: &Path) -> String {
    let scan = log_graph::scan_log_records(log_path, repo_root, REPO_ID, FIXED_TIME, false)
        .expect("scan should succeed");
    let producer = log_graph::log_importer_producer(scan.source_format_version, FIXED_TIME);
    let mut graph = aletheia_egregore::Graph::new();
    for record in scan.records {
        graph.push(record);
    }
    graph
        .stamp_producer(&producer)
        .to_jsonl()
        .expect("serialize jsonl")
}

fn parse_records(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("valid json line"))
        .collect()
}

fn nodes_of_kind<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|r| r["record_type"] == "node" && r["kind"] == kind)
        .collect()
}

// ── AC: 1000× error → exactly one ErrorSignature, occurrence_count == raw ────

#[test]
fn thousandfold_error_collapses_to_one_signature() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let jsonl = scan_to_jsonl(&plain, temp.path());
    let records = parse_records(&jsonl);

    let signatures = nodes_of_kind(&records, "ErrorSignature");
    // The repeated ERROR, the WARN, the panic, and the secret ERROR = 4 shapes.
    assert_eq!(signatures.len(), 4, "expected four distinct signatures");

    let repeated = signatures
        .iter()
        .find(|s| {
            s["log"]["template_excerpt"]
                .as_str()
                .unwrap_or_default()
                .contains("request <UUID> failed at <HEX>")
        })
        .expect("the repeated ERROR signature must exist");
    assert_eq!(
        repeated["log"]["occurrence_count"], 1000,
        "occurrence_count must equal the raw repetition count (100% dedup)"
    );
    assert_eq!(repeated["log"]["severity"], "error");
}

// ── AC: every record carries the identity + provenance contract ──────────────

#[test]
fn every_record_carries_identity_and_producer_envelope() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let jsonl = scan_to_jsonl(&plain, temp.path());
    let records = parse_records(&jsonl);

    for record in &records {
        let id = record["id"].as_str().expect("record has id");
        assert!(
            id.starts_with("log:v2:"),
            "id must be log:v2:-prefixed: {id}"
        );
        assert_eq!(record["schema_version"], 2, "log schema version is 2");

        let producer = &record["producer"];
        assert_eq!(producer["producer_kind"], "log_importer");
        let comps = &producer["producer_components"];
        assert_eq!(comps["importer_schema_version"], "2");
        assert_eq!(comps["fingerprint_algorithm"], "template-v1");
        assert!(
            comps["source_format_version"].is_string(),
            "source_format_version component present"
        );

        if record["record_type"] == "node" {
            assert!(record["summary"].is_string(), "node carries a summary");
            assert_eq!(record["domain"], "log", "node domain is log");
        }
    }

    // LogSource carries the artifact hash and repository-relative path.
    let sources = nodes_of_kind(&records, "LogSource");
    assert_eq!(sources.len(), 1);
    let src = sources[0];
    assert!(
        src["log"]["source_artifact_hash"]
            .as_str()
            .is_some_and(|h| !h.is_empty()),
        "LogSource carries a source_artifact_hash"
    );
    assert_eq!(src["log"]["source_relative_path"], "app.log");
    assert_eq!(src["log"]["source_format_version"], "plain-v1");
}

// ── AC: determinism (5 scans byte-identical) and CRLF/LF identity ────────────

#[test]
fn five_scans_are_byte_identical() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let first = scan_to_jsonl(&plain, temp.path());
    for _ in 0..4 {
        assert_eq!(scan_to_jsonl(&plain, temp.path()), first);
    }
}

#[test]
fn crlf_and_lf_yield_identical_record_ids() {
    let lf_dir = tempfile::tempdir().expect("temp dir");
    let crlf_dir = tempfile::tempdir().expect("temp dir");
    let lf_log = lf_dir.path().join("app.log");
    let crlf_log = crlf_dir.path().join("app.log");
    let body = plain_log();
    fs::write(&lf_log, &body).expect("write lf");
    fs::write(&crlf_log, body.replace('\n', "\r\n")).expect("write crlf");

    let lf_ids: Vec<String> = record_ids(&scan_to_jsonl(&lf_log, lf_dir.path()));
    let crlf_ids: Vec<String> = record_ids(&scan_to_jsonl(&crlf_log, crlf_dir.path()));
    assert_eq!(
        lf_ids, crlf_ids,
        "CRLF and LF checkouts must produce identical record IDs"
    );
}

fn record_ids(jsonl: &str) -> Vec<String> {
    let mut ids: Vec<String> = parse_records(jsonl)
        .iter()
        .map(|r| r["id"].as_str().unwrap_or_default().to_owned())
        .collect();
    ids.sort();
    ids
}

// ── AC: secret redaction, no raw payload beyond bounded excerpts ─────────────

#[test]
fn secret_line_is_redacted_and_no_raw_secret_leaks() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let jsonl = scan_to_jsonl(&plain, temp.path());

    assert!(
        !jsonl.contains("hunterSECRETtokenValueLong"),
        "raw secret bytes must never appear in output"
    );
    assert!(
        jsonl.contains("<REDACTED:"),
        "the secret line must carry a redaction marker"
    );

    let records = parse_records(&jsonl);
    let redacted_sig = nodes_of_kind(&records, "ErrorSignature")
        .into_iter()
        .find(|s| {
            s["log"]["template_excerpt"]
                .as_str()
                .unwrap_or_default()
                .contains("<REDACTED:")
        })
        .expect("a redacted signature must exist");
    assert_eq!(
        redacted_sig["redaction_policy_version"], "v1",
        "redacted records carry redaction_policy_version"
    );

    // No stored excerpt exceeds the documented bound.
    for record in &records {
        for field in ["template_excerpt", "event_excerpt"] {
            if let Some(excerpt) = record["log"][field].as_str() {
                assert!(
                    excerpt.chars().count() <= log_graph::EXCERPT_MAX_CHARS,
                    "{field} exceeds the excerpt bound"
                );
            }
        }
    }
}

// ── AC: info/debug lines mint no signature but bump line_count ────────────────

#[test]
fn info_debug_lines_bump_line_count_but_mint_no_signature() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let jsonl = scan_to_jsonl(&plain, temp.path());
    let records = parse_records(&jsonl);

    let line_count = nodes_of_kind(&records, "LogSource")[0]["log"]["line_count"]
        .as_u64()
        .expect("line_count present");
    let raw_lines = plain_log().lines().count() as u64;
    assert_eq!(
        line_count, raw_lines,
        "line_count must include info/debug noise"
    );

    // No signature is derived from an INFO/DEBUG line.
    for sig in nodes_of_kind(&records, "ErrorSignature") {
        let excerpt = sig["log"]["template_excerpt"].as_str().unwrap_or_default();
        assert!(
            !excerpt.contains("steady state") && !excerpt.contains("cache warm"),
            "info/debug lines must not become signatures: {excerpt}"
        );
    }
}

// ── AC: multi-line panic is ONE event/signature ──────────────────────────────

#[test]
fn multiline_panic_is_one_event_and_signature() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let jsonl = scan_to_jsonl(&plain, temp.path());
    let records = parse_records(&jsonl);

    let panic_sigs: Vec<&Value> = nodes_of_kind(&records, "ErrorSignature")
        .into_iter()
        .filter(|s| {
            s["log"]["template_excerpt"]
                .as_str()
                .unwrap_or_default()
                .contains("panicked at")
        })
        .collect();
    assert_eq!(panic_sigs.len(), 1, "the panic is one signature");
    assert_eq!(
        panic_sigs[0]["log"]["occurrence_count"], 1,
        "the panic occurred once"
    );
    assert_eq!(panic_sigs[0]["log"]["severity"], "fatal");

    // The single panic exemplar carries the backtrace as one multi-line event.
    let excerpt = panic_sigs[0]["log"]["template_excerpt"]
        .as_str()
        .unwrap_or_default();
    assert!(
        excerpt.contains("core::panicking::panic_bounds_check"),
        "the backtrace attached to the same logical event: {excerpt}"
    );

    // The timestamp-less panic falls back to the transaction-time bucket hour.
    let panic_event = nodes_of_kind(&records, "LogEvent")
        .into_iter()
        .find(|e| e["log"]["severity"] == "fatal")
        .expect("a fatal exemplar exists");
    assert_eq!(
        panic_event["valid_time_source"], "inferred_from_transaction_time",
        "a timestamp-less line uses the transaction-time fallback"
    );
}

// ── AC: jsonl-v1 handled; unrecognized format exits 1, no partial output ─────

#[test]
fn jsonl_format_is_detected_and_extracted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (_, jsonl_path) = write_fixtures(temp.path());
    let out = scan_to_jsonl(&jsonl_path, temp.path());
    let records = parse_records(&out);

    assert_eq!(
        nodes_of_kind(&records, "LogSource")[0]["log"]["source_format_version"],
        "jsonl-v1"
    );
    // The error and warn lines mint signatures; the info line does not.
    assert_eq!(
        nodes_of_kind(&records, "ErrorSignature").len(),
        2,
        "one error + one warn signature from jsonl"
    );
}

#[test]
fn unrecognized_binary_format_exits_1_with_no_output() {
    let temp = tempfile::tempdir().expect("temp dir");
    let bin = temp.path().join("blob.log");
    let out = temp.path().join("out.jsonl");
    fs::write(&bin, [0x00_u8, 0x01, 0x02, 0xff, 0xfe, b'l', b'o', b'g']).expect("write binary");

    egregore()
        .arg("scan-logs")
        .arg(&bin)
        .arg("--repo-path")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .assert()
        .code(1)
        .stdout(predicates::str::contains(
            "\"code\":\"unrecognized_format\"",
        ));

    assert!(!out.exists(), "no partial output on the error path");
}

// ── AC: validate passes; ingest dry-run + embedded; re-ingest 0 duplicates ───

#[test]
fn emitted_graph_validates_and_ingests_dry_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");

    egregore()
        .arg("scan-logs")
        .arg(&plain)
        .arg("--repo-path")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .assert()
        .success();

    egregore().arg("validate").arg(&out).assert().success();

    egregore()
        .arg("ingest")
        .arg(&out)
        .arg("--adapter")
        .arg("dry-run")
        .assert()
        .success()
        .stdout(predicates::str::contains("failed: 0"));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_ingest_round_trips_and_reingest_has_zero_duplicates() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let data_dir = temp.path().join("store");

    egregore()
        .arg("scan-logs")
        .arg(&plain)
        .arg("--repo-path")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .assert()
        .success();

    // First embedded ingest: all records written, none failed.
    egregore()
        .arg("ingest")
        .arg(&out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("failed: 0"));

    let total_after_first = inspect_total(&data_dir);
    assert!(total_after_first > 0, "store holds log records");

    // Re-ingest the identical file: no new physical records (0 duplicates).
    egregore()
        .arg("ingest")
        .arg(&out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("failed: 0"));

    let total_after_second = inspect_total(&data_dir);
    assert_eq!(
        total_after_first, total_after_second,
        "re-ingesting the same file must add zero duplicate records"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn inspect_total(data_dir: &Path) -> u64 {
    let output = egregore()
        .arg("inspect")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8 inspect output");
    let json: Value = serde_json::from_str(text.trim()).expect("inspect emits json");
    json["records"].as_u64().expect("records count present")
}

// ── Producer identity is never an ID input ───────────────────────────────────

#[test]
fn record_ids_are_stable_across_producer_perturbation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());

    // Two different producer-started-at values (simulating different binary runs)
    // must not change any record ID — the producer envelope is non-identity.
    let scan_a = log_graph::scan_log_records(&plain, temp.path(), REPO_ID, FIXED_TIME, false)
        .expect("scan a");
    let scan_b = log_graph::scan_log_records(&plain, temp.path(), REPO_ID, FIXED_TIME, false)
        .expect("scan b");

    let ids_a: Vec<&str> = scan_a
        .records
        .iter()
        .map(aletheia_egregore::GraphRecord::id)
        .collect();
    let ids_b: Vec<&str> = scan_b
        .records
        .iter()
        .map(aletheia_egregore::GraphRecord::id)
        .collect();
    assert_eq!(ids_a, ids_b);

    let prod_a =
        log_graph::log_importer_producer(scan_a.source_format_version, "2020-01-01T00:00:00Z");
    let prod_b =
        log_graph::log_importer_producer(scan_b.source_format_version, "2099-12-31T23:59:59Z");
    let mut ga = aletheia_egregore::Graph::new();
    for r in scan_a.records {
        ga.push(r);
    }
    let mut gb = aletheia_egregore::Graph::new();
    for r in scan_b.records {
        gb.push(r);
    }
    let ida = record_ids(&ga.stamp_producer(&prod_a).to_jsonl().unwrap());
    let idb = record_ids(&gb.stamp_producer(&prod_b).to_jsonl().unwrap());
    assert_eq!(
        ida, idb,
        "different producer_started_at must not change record IDs"
    );
}

// ── Exemplar cap: never a silent drop ────────────────────────────────────────

#[test]
fn exemplar_cap_emits_diagnostic_and_keeps_default_cap() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let scan =
        log_graph::scan_log_records(&plain, temp.path(), REPO_ID, FIXED_TIME, false).expect("scan");

    // The 1000× error has 1000 distinct timestamps → capped at the default.
    let cap_diag = scan
        .diagnostics
        .iter()
        .find(|d| d.code == "exemplar_cap_reached")
        .expect("an exemplar-cap diagnostic must be surfaced");
    assert_eq!(cap_diag.kept, log_graph::DEFAULT_EXEMPLAR_CAP as u64);
    assert!(
        cap_diag.dropped > 0,
        "dropped exemplars are counted, not silent"
    );

    // Exactly the cap number of LogEvent exemplars exist for that signature.
    let records = parse_records(&{
        let producer = log_graph::log_importer_producer(scan.source_format_version, FIXED_TIME);
        let mut g = aletheia_egregore::Graph::new();
        for r in scan.records {
            g.push(r);
        }
        g.stamp_producer(&producer).to_jsonl().unwrap()
    });
    let events_for_repeated = records
        .iter()
        .filter(|r| {
            r["record_type"] == "node"
                && r["kind"] == "LogEvent"
                && r["log"]["event_excerpt"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("request <UUID> failed at <HEX>")
        })
        .count();
    assert_eq!(
        events_for_repeated,
        log_graph::DEFAULT_EXEMPLAR_CAP,
        "the repeated signature keeps exactly the exemplar cap"
    );
}

// ── Protected raw-log capture (issue #321) ───────────────────────────────────

/// Runs `eg scan-logs` with protected capture enabled and returns the completed
/// assertion (caller decides success/failure).
fn scan_logs_capture(
    plain: &Path,
    repo: &Path,
    out: &Path,
    store: &Path,
    producer: &str,
) -> assert_cmd::assert::Assert {
    egregore()
        .arg("scan-logs")
        .arg(plain)
        .arg("--repo-path")
        .arg(repo)
        .arg("--out")
        .arg(out)
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .arg("--protected-raw-artifacts")
        .arg("--protected-store")
        .arg(store)
        .arg("--producer")
        .arg(producer)
        .arg("--captured-at")
        .arg(FIXED_CAPTURED_AT)
        .assert()
}

/// Returns the single manifest record of an enabled-capture protected store.
fn only_manifest_record(store: &Path) -> Value {
    let manifest =
        fs::read_to_string(store.join("manifest.jsonl")).expect("manifest.jsonl must exist");
    let lines: Vec<&str> = manifest.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly one manifest record");
    serde_json::from_str(lines[0]).expect("manifest record is valid JSON")
}

fn blob_count(store: &Path) -> usize {
    fs::read_dir(store.join("blobs"))
        .expect("blobs dir must exist")
        .count()
}

// AC1: disabled by default — no store, no blob, no manifest.
#[test]
fn protected_capture_disabled_by_default_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");

    egregore()
        .arg("scan-logs")
        .arg(&plain)
        .arg("--repo-path")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .assert()
        .success();

    assert!(out.exists(), "graph JSONL is still written");
    assert!(
        !store.exists(),
        "protected store dir must not be created without the flag"
    );
    assert!(!store.join("blobs").exists(), "no blobs without the flag");
    assert!(
        !store.join("manifest.jsonl").exists(),
        "no manifest without the flag"
    );
}

// AC1: enabled — exactly one blob + one manifest entry, class log_payload.
#[test]
fn protected_capture_enabled_writes_one_blob_and_manifest_entry() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");

    let assert = scan_logs_capture(&plain, temp.path(), &out, &store, "op-1").success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    let summary: Value = serde_json::from_str(stdout.trim()).expect("capture summary JSON line");
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["protected_capture"]["source_class"], "log_payload");
    assert_eq!(summary["protected_capture"]["stored"], true);
    assert!(
        summary["protected_capture"]["byte_len"]
            .as_u64()
            .expect("byte_len present")
            > 0,
        "captured byte_len must be positive"
    );
    let handle = summary["protected_capture"]["handle"]
        .as_str()
        .expect("handle present");
    assert!(handle.starts_with("protected:v1:"));

    let record = only_manifest_record(&store);
    assert_eq!(record["source_class"], "log_payload");
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["handle"], handle);
    assert_eq!(blob_count(&store), 1, "exactly one blob");
}

// AC3: recapturing an unchanged log 5× yields zero duplicate manifest entries.
#[test]
fn protected_capture_five_times_is_zero_duplicate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");

    for _ in 0..5 {
        scan_logs_capture(&plain, temp.path(), &out, &store, "op-1").success();
    }
    let manifest =
        fs::read_to_string(store.join("manifest.jsonl")).expect("manifest.jsonl must exist");
    assert_eq!(
        manifest.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "5 recaptures → exactly one manifest entry"
    );
    assert_eq!(blob_count(&store), 1, "5 recaptures → exactly one blob");
}

// AC4: the graph JSONL never stores a protected handle.
#[test]
fn emitted_graph_never_contains_protected_handle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");

    scan_logs_capture(&plain, temp.path(), &out, &store, "op-1").success();

    let graph = fs::read_to_string(&out).expect("graph JSONL exists");
    assert!(
        !graph.contains("protected:v1:"),
        "graph JSONL must never carry a protected handle"
    );
}

// AC5: the stored blob is POST-REDACTION — carries the marker, not the secret.
#[test]
fn stored_blob_is_redacted_and_omits_the_secret() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");

    scan_logs_capture(&plain, temp.path(), &out, &store, "op-1").success();
    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();

    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");
    assert!(
        blob.contains("<REDACTED:"),
        "the secret-bearing line must be stored as a redaction marker"
    );
    assert!(
        !blob.contains(FIXTURE_SECRET),
        "the raw secret must never reach the protected blob"
    );
}

// AC5 (regression, issue #321): a MULTI-LINE private-key block must be redacted
// in full. The original capture path redacted each normalized line
// independently, so only the `-----BEGIN … PRIVATE KEY-----` marker line (the
// one line that is individually secret-shaped) collapsed to a marker; the
// base64 key-material body lines and the `-----END … PRIVATE KEY-----` line
// were written to the protected blob unchanged, leaking the secret.
#[test]
fn stored_blob_redacts_multiline_private_key_block_in_full() {
    const KEY_BODY_MARKER: &str = "LEAKEDPRIVATEKEYBODY";
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("keyed.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    fixture.push_str("2026-01-02T03:00:01Z [ERROR] loaded deploy key material below\n");
    fixture.push_str("-----BEGIN OPENSSH PRIVATE KEY-----\n");
    fixture.push_str("b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtz\n");
    fixture.push_str("c2VjcmV0");
    fixture.push_str(KEY_BODY_MARKER);
    fixture.push_str("YmFzZTY0bGluZXNNdXN0QmVSZWRhY3RlZA==\n");
    fixture.push_str("ZWQyNTUxOQAAACDNqorgFVACa1nGGkM0iZBExampleTW9yZUJvZHkAAAA\n");
    fixture.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write keyed fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // The key block collapses to a redaction marker.
    assert!(
        blob.contains("<REDACTED:"),
        "the key block must be stored as a redaction marker"
    );
    // Body key-material bytes must never survive capture — the core bug.
    assert!(
        !blob.contains(KEY_BODY_MARKER),
        "base64 key-material body must be redacted, not just the BEGIN line"
    );
    // The BEGIN and END delimiter lines of the block are gone too.
    assert!(
        !blob.contains("-----END OPENSSH PRIVATE KEY-----"),
        "the END line of the key block must also be redacted"
    );
    assert!(
        !blob.contains("-----BEGIN OPENSSH PRIVATE KEY-----"),
        "the BEGIN line of the key block must also be redacted"
    );
    // Non-secret lines around the block are preserved (no over-redaction).
    assert!(
        blob.contains("service starting up nominally"),
        "normal lines before the key block are preserved"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal lines after the key block are preserved"
    );
}

// Regression (issue #321, Codex P1 "scan spans in byte order before marking
// lines"): when a lower-priority secret (an `API_KEY=` env secret) appears
// BEFORE a later higher-priority one (an SSH private-key block), the capture
// path's cursor walk called `detect_secret_span` over the whole remaining text,
// which returns the highest-priority CLASS match (the later key block) rather
// than the earliest BYTE offset, then advanced the cursor past that later span —
// skipping the earlier env-secret line so the API token leaked into the blob.
#[test]
fn stored_blob_redacts_earlier_lower_priority_secret_before_later_higher_priority() {
    const API_TOKEN_MARKER: &str = "hunterAPITOKENleakValueLong";
    const KEY_BODY_MARKER: &str = "LEAKEDKEYBODYMARKER";
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("mixed.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    // Lower-priority (EnvSecret) secret, EARLIER in the byte stream.
    fixture.push_str("2026-01-02T03:00:01Z [ERROR] auth bootstrap failed API_KEY=");
    fixture.push_str(API_TOKEN_MARKER);
    fixture.push_str(" reason denied\n");
    fixture.push_str("2026-01-02T03:00:02Z INFO between the two secrets\n");
    // Higher-priority (SshPrivateKey) secret block, LATER in the byte stream.
    fixture.push_str("-----BEGIN OPENSSH PRIVATE KEY-----\n");
    fixture.push_str("b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtz\n");
    fixture.push_str("c2VjcmV0");
    fixture.push_str(KEY_BODY_MARKER);
    fixture.push_str("YmFzZTY0bGluZXNNdXN0QmVSZWRhY3RlZA==\n");
    fixture.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write mixed fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // The EARLIER, lower-priority env-secret value must never survive capture —
    // the exact leak the byte-order bug produced.
    assert!(
        !blob.contains(API_TOKEN_MARKER),
        "the earlier lower-priority API token must be redacted, not skipped"
    );
    // The LATER, higher-priority key block body and END line must also be gone.
    assert!(
        !blob.contains(KEY_BODY_MARKER),
        "base64 key-material body must be redacted"
    );
    assert!(
        !blob.contains("-----END OPENSSH PRIVATE KEY-----"),
        "the END line of the key block must also be redacted"
    );
    // Both secret spans collapse to redaction markers.
    assert!(
        blob.contains("<REDACTED:"),
        "secret spans must be stored as redaction markers"
    );
    // Non-secret lines around and between the secrets are preserved verbatim.
    assert!(
        blob.contains("service starting up nominally"),
        "normal line before the first secret is preserved"
    );
    assert!(
        blob.contains("between the two secrets"),
        "normal line between the two secrets is preserved"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal line after the key block is preserved"
    );
}

// Regression (issue #321, Codex P1 "redact full env spans that overlap
// higher-priority tokens"): when a LOWER-priority env-secret value CONTAINS a
// HIGHER-priority API token AND a trailing suffix, the old earliest-start prefix
// probe redacted only the env-value bytes BEFORE the nested token, redacted the
// nested token next, then copied the suffix bytes AFTER the token verbatim into
// the blob — leaking the tail of the env-secret value. The iterative full-text
// redactor re-scans after replacing the inner token and re-detects the remaining
// env value, so prefix, nested token, AND suffix all collapse to one marker.
#[test]
fn stored_blob_redacts_full_env_span_overlapping_nested_higher_priority_token() {
    const NESTED_TOKEN: &str = "sk-abcdefghijklmnopqrstuvwx";
    const SUFFIX_MARKER: &str = "TAILLEAKMARKER";
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("overlap.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    // `PASSWORD=abcdefgh-<api token>!TAILLEAKMARKER` — one env secret whose value
    // wraps a higher-priority API token and a distinctive trailing suffix.
    fixture.push_str("2026-01-02T03:00:01Z [ERROR] auth failed PASSWORD=abcdefgh-");
    fixture.push_str(NESTED_TOKEN);
    fixture.push('!');
    fixture.push_str(SUFFIX_MARKER);
    fixture.push_str(" reason denied\n");
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write overlap fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // The nested higher-priority token must be gone.
    assert!(
        !blob.contains(NESTED_TOKEN),
        "the nested API token must be redacted"
    );
    // The SUFFIX after the nested token — still part of the env-secret value —
    // must NOT survive into the blob. This is the exact leak the old prefix probe
    // produced by truncating the overlapping lower-priority span.
    assert!(
        !blob.contains(SUFFIX_MARKER),
        "the env-secret suffix PAST the nested token must be redacted, not copied verbatim"
    );
    // The env-secret prefix bytes must be gone too.
    assert!(
        !blob.contains("abcdefgh-"),
        "the env-secret prefix bytes must be redacted"
    );
    // The overlapping span collapses to a redaction marker.
    assert!(
        blob.contains("<REDACTED:"),
        "the secret span must be stored as a redaction marker"
    );
    // Non-secret lines around the secret are preserved verbatim.
    assert!(
        blob.contains("service starting up nominally"),
        "normal line before the secret is preserved"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal line after the secret is preserved"
    );
}

// Regression (issue #321, Codex round-9 P1 "redact env tails when a value starts
// with a higher-priority token"): the round-7 overlap fix collapsed prefix +
// nested token + suffix only when the env value had a PREFIX before the nested
// higher-priority token. When the value STARTS WITH the token (no prefix), pass 1
// redacts just the leading token, leaving `PASSWORD=<REDACTED:secret>!TAIL…`; the
// env-secret matcher's prefix-based "already redacted" guard then skipped the
// assignment because the value BEGINS with `<REDACTED:`, so the suffix leaked into
// the blob. The exact-placeholder guard re-detects the whole value and collapses
// token + tail into one marker.
#[test]
fn stored_blob_redacts_env_tail_when_value_starts_with_higher_priority_token() {
    const NESTED_TOKEN: &str = "sk-abcdefghijklmnopqrstuvwx";
    const SUFFIX_MARKER: &str = "TAILLEAKMARKER";
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("starts-with-token.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    // `PASSWORD=<api token>!TAILLEAKMARKER` — the env value BEGINS with the
    // higher-priority token (no prefix) and hides a distinctive tail after it.
    fixture.push_str("2026-01-02T03:00:01Z [ERROR] auth failed PASSWORD=");
    fixture.push_str(NESTED_TOKEN);
    fixture.push('!');
    fixture.push_str(SUFFIX_MARKER);
    fixture.push_str(" reason denied\n");
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write starts-with-token fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // The leading higher-priority token must be gone.
    assert!(
        !blob.contains(NESTED_TOKEN),
        "the leading API token must be redacted"
    );
    // The SUFFIX after the leading token — still part of the env-secret value —
    // must NOT survive into the blob. This is the exact round-9 leak.
    assert!(
        !blob.contains(SUFFIX_MARKER),
        "the env-secret suffix PAST the leading token must be redacted, not copied verbatim"
    );
    // The overlapping span collapses to a redaction marker.
    assert!(
        blob.contains("<REDACTED:"),
        "the secret span must be stored as a redaction marker"
    );
    // Non-secret lines around the secret are preserved verbatim.
    assert!(
        blob.contains("service starting up nominally"),
        "normal line before the secret is preserved"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal line after the secret is preserved"
    );
}

// Regression (issue #321, Codex P1 "mark blank lines inside secret spans"): an
// encrypted RFC-1421-style PEM block carries `Proc-Type`/`DEK-Info` headers, a
// BLANK separator line, then the base64 body before `-----END … -----`. The old
// line-marking capture path split the detected span into two maximal runs of
// secret-bearing lines (the blank line unmarked, breaking the run): the first run
// held the `BEGIN` marker and redacted, but the later body/`END` run no longer
// contained `BEGIN`, so `redact_value` passed it through and the key material
// leaked into the protected blob. Byte-span redaction collapses the WHOLE span —
// headers, blank line, body, and `END` — to one marker regardless of internal
// blank lines.
#[test]
fn stored_blob_redacts_private_key_block_with_internal_blank_line() {
    const KEY_BODY_MARKER: &str = "LEAKEDPEMBODYAFTERBLANKLINE";
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("encrypted-pem.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    fixture.push_str("2026-01-02T03:00:01Z [ERROR] loaded encrypted deploy key below\n");
    fixture.push_str("-----BEGIN RSA PRIVATE KEY-----\n");
    fixture.push_str("Proc-Type: 4,ENCRYPTED\n");
    fixture.push_str("DEK-Info: AES-128-CBC,7F3A1C9E4B2D8A605E1F0C3B9D7A2E48\n");
    // The RFC-1421 blank separator line INSIDE the secret span — the exact byte
    // that broke the old line-run reassembly.
    fixture.push('\n');
    fixture.push_str("MIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Q\n");
    fixture.push_str("c2VjcmV0");
    fixture.push_str(KEY_BODY_MARKER);
    fixture.push_str("YmFzZTY0Qm9keU11c3RCZVJlZGFjdGVk\n");
    fixture.push_str("uXvpFi+ExampleTrailingBodyLineBeforeEndMarkerAAAA==\n");
    fixture.push_str("-----END RSA PRIVATE KEY-----\n");
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write encrypted-pem fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // The whole span collapses to a redaction marker.
    assert!(
        blob.contains("<REDACTED:"),
        "the key block must be stored as a redaction marker"
    );
    // The base64 body AFTER the internal blank line must never survive — the leak.
    assert!(
        !blob.contains(KEY_BODY_MARKER),
        "base64 body after the internal blank line must be redacted"
    );
    // The encrypted-key header lines must be gone.
    assert!(
        !blob.contains("Proc-Type: 4,ENCRYPTED"),
        "the Proc-Type header line must be redacted"
    );
    assert!(
        !blob.contains("DEK-Info: AES-128-CBC"),
        "the DEK-Info header line must be redacted"
    );
    // The BEGIN and END delimiter lines of the block are gone too.
    assert!(
        !blob.contains("-----BEGIN RSA PRIVATE KEY-----"),
        "the BEGIN line of the key block must also be redacted"
    );
    assert!(
        !blob.contains("-----END RSA PRIVATE KEY-----"),
        "the END line of the key block must also be redacted"
    );
    // Non-secret lines around the block are preserved (no over-redaction).
    assert!(
        blob.contains("service starting up nominally"),
        "normal lines before the key block are preserved"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal lines after the key block are preserved"
    );
}

// Regression (issue #321, Codex P1 "preserve v1 redaction coverage for quoted
// env secrets"): the byte-span capture path relies on `detect_secret_span`, whose
// EnvSecret span matcher treated the opening `"` of `KEY="value"` as a value
// delimiter and returned no span, so a common quoted `.env` secret was copied
// verbatim into the `log_payload` blob. `redact_value` redacts it; the span
// matcher must agree so capture stores post-redaction bytes.
#[test]
fn stored_blob_redacts_quoted_env_secret() {
    const QUOTED_SECRET: &str = "hunterSECRETtokenValueLong";
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("quoted.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    fixture.push_str("2026-01-02T03:00:01Z [ERROR] auth bootstrap failed API_KEY=\"");
    fixture.push_str(QUOTED_SECRET);
    fixture.push_str("\" reason denied\n");
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write quoted fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // The quoted env secret value must never survive capture — the exact leak the
    // quote-as-delimiter span bug produced.
    assert!(
        !blob.contains(QUOTED_SECRET),
        "the quoted env secret must be redacted, not copied verbatim"
    );
    // The secret span collapses to a redaction marker.
    assert!(
        blob.contains("<REDACTED:"),
        "the quoted secret must be stored as a redaction marker"
    );
    // Non-secret lines around the secret are preserved (no over-redaction).
    assert!(
        blob.contains("service starting up nominally"),
        "normal line before the secret is preserved"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal line after the secret is preserved"
    );
}

// Issue #321 (Codex round-8 P1): after a QUOTED env secret is redacted, the
// redaction loop re-detects the placeholder it just wrote as the quote-stripped
// env value. The old forward-progress guard `break`-ed out of the whole loop
// there, so a SECOND, independent env secret after the quoted one was never
// scanned and was copied into the protected blob unredacted. The scan-cursor fix
// must skip past the placeholder and keep redacting — BOTH secrets must be absent.
#[test]
fn stored_blob_redacts_second_env_secret_after_quoted_placeholder() {
    const FIRST_QUOTED: &str = "firstSecretValueLong";
    const SECOND_PLAIN: &str = "secondSecretValueLong";
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("two-secrets.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    fixture.push_str("2026-01-02T03:00:01Z [ERROR] auth bootstrap failed API_KEY=\"");
    fixture.push_str(FIRST_QUOTED);
    fixture.push_str("\" PASSWORD=");
    fixture.push_str(SECOND_PLAIN);
    fixture.push_str(" reason denied\n");
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write two-secret fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // Neither secret may survive: the first quoted one, nor the second plain one
    // that used to leak past the loop `break`.
    assert!(
        !blob.contains(FIRST_QUOTED),
        "the first (quoted) env secret must be redacted, not copied verbatim: {blob}"
    );
    assert!(
        !blob.contains(SECOND_PLAIN),
        "the second env secret after the quoted placeholder must ALSO be redacted: {blob}"
    );
    assert!(
        blob.contains("<REDACTED:"),
        "the secrets collapse to redaction markers"
    );
    // Non-secret content around the secrets is preserved (no over-redaction).
    assert!(
        blob.contains("service starting up nominally"),
        "normal line before the secrets is preserved"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal line after the secrets is preserved"
    );
}

// Issue #321 (Codex finding B): the protected blob must be derived from the SAME
// normalized buffer the scan read — not a second filesystem read that could
// observe appended/rotated bytes. The scan exposes its normalized buffer, and the
// capture redaction runs over that exact buffer, so the two never diverge. This
// test drives the refactored single-read API directly: `scan_log_records` returns
// the normalized source it hashed, and `redacted_source_bytes` takes that buffer
// (a `&str`), not a path.
#[test]
fn capture_redacts_the_same_normalized_buffer_the_scan_read() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("crlf.log");
    // CRLF endings + an env-secret line. The scan normalizes CRLF -> LF and hashes
    // that buffer; capture must redact THAT SAME buffer.
    std::fs::write(
        &log,
        b"2026-01-02T03:00:00Z INFO starting\r\n\
          2026-01-02T03:00:01Z [ERROR] auth failed API_KEY=hunterSECRETtokenValueLong denied\r\n",
    )
    .expect("write crlf fixture");

    // Capture requested → retain the normalized buffer (issue #321, Codex P2).
    let scan = log_graph::scan_log_records(&log, temp.path(), REPO_ID, FIXED_TIME, true)
        .expect("scan should succeed");

    // The scan exposes the exact normalized buffer it hashed: CRLF collapsed to LF.
    let normalized_source = scan
        .normalized_source
        .as_deref()
        .expect("retain=true must yield Some(normalized_source)");
    assert!(
        normalized_source.contains('\n') && !normalized_source.contains('\r'),
        "normalized_source is the CRLF->LF normalized buffer the scan hashed"
    );

    // Capture redaction runs over that same in-memory buffer (single read) and
    // matches the redaction of the identical buffer — no second filesystem read.
    let redacted = log_graph::redacted_source_bytes(normalized_source);
    let redacted_str = String::from_utf8(redacted).expect("utf8 redacted");
    assert!(
        !redacted_str.contains("hunterSECRETtokenValueLong"),
        "the secret in the scanned buffer must be redacted in the captured bytes"
    );
    assert!(
        redacted_str.contains("<REDACTED:"),
        "the captured bytes carry a redaction marker"
    );
    // Non-secret content from the scanned buffer is preserved verbatim.
    assert!(redacted_str.contains("INFO starting"));
}

// Regression (issue #321, Codex P2 "avoid cloning raw logs when capture is
// disabled"): the default scan path (no protected capture) must NOT retain a
// full-log clone of the normalized buffer past the scan — that is an avoidable
// large-log allocation for the common CI/runtime case that never needs it. The
// buffer is still read once and hashed transiently into `source_artifact_hash`;
// only RETENTION is conditional. When capture IS requested the same single-read
// buffer is retained and yields the correct redacted blob (no second read).
#[test]
fn default_scan_does_not_retain_normalized_source_but_capture_does() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("retain.log");
    std::fs::write(
        &log,
        b"2026-01-02T03:00:00Z INFO starting\n\
          2026-01-02T03:00:01Z [ERROR] auth failed API_KEY=hunterSECRETtokenValueLong denied\n",
    )
    .expect("write fixture");

    // Default path: buffer is dropped, not retained.
    let default_scan = log_graph::scan_log_records(&log, temp.path(), REPO_ID, FIXED_TIME, false)
        .expect("default scan should succeed");
    assert!(
        default_scan.normalized_source.is_none(),
        "the default (no-capture) scan must not retain the normalized buffer"
    );
    // The scan is otherwise unchanged — records are still produced.
    assert!(
        !default_scan.records.is_empty(),
        "the default scan still produces graph records"
    );

    // Capture path: buffer retained from the single read, redacted blob correct.
    let capture_scan = log_graph::scan_log_records(&log, temp.path(), REPO_ID, FIXED_TIME, true)
        .expect("capture scan should succeed");
    let normalized_source = capture_scan
        .normalized_source
        .as_deref()
        .expect("retain=true must yield Some(normalized_source)");
    let redacted =
        String::from_utf8(log_graph::redacted_source_bytes(normalized_source)).expect("utf8");
    assert!(
        !redacted.contains("hunterSECRETtokenValueLong"),
        "the retained single-read buffer redacts the secret"
    );
    assert!(
        redacted.contains("<REDACTED:"),
        "carries a redaction marker"
    );
}

// AC5/AC7: `protected get` verifies + returns bytes and `list` shows a
// log_payload entry with metadata only (no raw bytes).
#[test]
fn protected_get_and_list_expose_log_payload_metadata_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    let retrieved = temp.path().join("retrieved.log");

    scan_logs_capture(&plain, temp.path(), &out, &store, "op-1").success();
    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();

    egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .arg("--out")
        .arg(&retrieved)
        .assert()
        .success();
    assert!(retrieved.exists(), "retrieved file exists");
    assert!(
        !fs::read_to_string(&retrieved)
            .unwrap()
            .contains(FIXTURE_SECRET),
        "retrieved bytes carry no raw secret"
    );

    let list = egregore()
        .args(["protected", "list"])
        .arg("--store")
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_json: Value = serde_json::from_slice(&list).expect("list JSON");
    assert_eq!(list_json["count"], 1);
    let entry = &list_json["handles"][0];
    assert_eq!(entry["source_class"], "log_payload");
    assert!(entry["byte_len"].as_u64().unwrap() > 0);
    // Metadata only: no raw payload byte field is present in list output.
    let list_text = String::from_utf8(list).unwrap();
    assert!(
        !list_text.contains(FIXTURE_SECRET),
        "list output never includes raw bytes"
    );
}

// AC6: an unwritable store fails with a machine-readable diagnostic, a distinct
// non-zero exit code, and leaves no partial manifest.
#[test]
fn protected_capture_store_failure_is_machine_readable_and_atomic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    // A regular FILE where the store directory should be forces a store I/O
    // failure (the root is not a real directory).
    let store = temp.path().join("protected-file");
    fs::write(&store, b"sentinel").expect("write blocking file");

    scan_logs_capture(&plain, temp.path(), &out, &store, "op-1")
        .code(3)
        .stderr(predicates::str::contains("\"code\":\"store_io_error\""));

    // No partial write: the blocking file is untouched and no manifest exists.
    assert_eq!(
        fs::read(&store).unwrap(),
        b"sentinel",
        "the blocking file must be untouched (no partial write)"
    );
    assert!(
        !store.join("manifest.jsonl").exists(),
        "no partial manifest on failure"
    );
}

// AC6: capture enabled without --producer or --protected-store exits 1 with a
// machine-readable missing_field diagnostic.
#[test]
fn protected_capture_enabled_requires_producer_and_store() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");

    // Missing --producer (store present).
    egregore()
        .arg("scan-logs")
        .arg(&plain)
        .arg("--repo-path")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .arg("--protected-raw-artifacts")
        .arg("--protected-store")
        .arg(temp.path().join("protected"))
        .assert()
        .code(1)
        .stderr(predicates::str::contains("\"code\":\"missing_field\""));

    // Missing --protected-store (producer present).
    egregore()
        .arg("scan-logs")
        .arg(&plain)
        .arg("--repo-path")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg(REPO_ID)
        .arg("--protected-raw-artifacts")
        .arg("--producer")
        .arg("op-1")
        .assert()
        .code(1)
        .stderr(predicates::str::contains("\"code\":\"missing_field\""));
}

// AC7: neither stdout nor stderr of an enabled capture ever echoes the secret.
#[test]
fn protected_capture_output_never_echoes_the_secret() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (plain, _) = write_fixtures(temp.path());
    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");

    let assert = scan_logs_capture(&plain, temp.path(), &out, &store, "op-1").success();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains(FIXTURE_SECRET),
        "capture output must never echo the raw secret"
    );
    assert!(
        !combined.contains("<REDACTED:"),
        "capture output reports handles/hashes only, not payload text"
    );
}

// AC3/determinism: two enabled scans with a fixed captured_at produce a
// byte-identical manifest.
#[test]
fn protected_manifest_is_byte_identical_across_runs() {
    let mk = || {
        let temp = tempfile::tempdir().expect("temp dir");
        let (plain, _) = write_fixtures(temp.path());
        let out = temp.path().join("log.graph.jsonl");
        let store = temp.path().join("protected");
        scan_logs_capture(&plain, temp.path(), &out, &store, "op-1").success();
        let manifest = fs::read_to_string(store.join("manifest.jsonl")).expect("manifest exists");
        (temp, manifest)
    };
    let (_a, manifest_a) = mk();
    let (_b, manifest_b) = mk();
    assert_eq!(
        manifest_a, manifest_b,
        "fixed captured_at must yield a byte-identical manifest"
    );
}

// Regression (issue #321, Codex round-10 P1 "redact quoted env values past
// embedded whitespace"): a QUOTED env secret whose value contains INTERNAL spaces
// (a passphrase). The delimiter-only value boundary truncated the detected span at
// the first space, redacting only the first word; the next pass then read the
// `"<REDACTED:secret>` head as an already-redacted exact placeholder and copied the
// rest of the passphrase into the protected blob. The quote-aware value boundary
// runs the span to the matching closing quote so the whole passphrase — spaces
// included — collapses to one marker, and the per-line whole-value safety net caps
// the leak class regardless.
#[test]
fn stored_blob_redacts_quoted_env_secret_with_internal_whitespace() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("quoted-passphrase.log");
    let mut fixture = String::new();
    fixture.push_str("2026-01-02T03:00:00Z INFO service starting up nominally\n");
    // `PASSWORD="correct horse PASSPHRASELEAK staple"` — a quoted passphrase with
    // internal whitespace and a distinctive multi-word secret.
    fixture.push_str(
        "2026-01-02T03:00:01Z [ERROR] auth failed PASSWORD=\"correct horse PASSPHRASELEAK staple\"\n",
    );
    fixture.push_str("2026-01-02T03:00:05Z INFO service ready to accept traffic\n");
    fs::write(&log, &fixture).expect("write quoted-passphrase fixture");

    let out = temp.path().join("log.graph.jsonl");
    let store = temp.path().join("protected");
    scan_logs_capture(&log, temp.path(), &out, &store, "op-1").success();

    let handle = only_manifest_record(&store)["handle"]
        .as_str()
        .expect("handle")
        .to_owned();
    let got = egregore()
        .args(["protected", "get"])
        .arg(&handle)
        .arg("--store")
        .arg(&store)
        .arg("--operator")
        .arg("op-1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let blob = String::from_utf8(got).expect("utf8 blob");

    // The distinctive multi-word passphrase must never survive past the first space.
    assert!(
        !blob.contains("PASSPHRASELEAK"),
        "the quoted passphrase must be redacted past embedded whitespace: {blob}"
    );
    // No whitespace-separated word of the quoted value may survive either.
    assert!(
        !blob.contains("horse") && !blob.contains("staple"),
        "no word of the quoted passphrase may reach the blob: {blob}"
    );
    // The quoted value collapses to a redaction marker.
    assert!(
        blob.contains("<REDACTED:"),
        "the quoted secret must be stored as a redaction marker: {blob}"
    );
    // Non-secret lines around the secret are preserved verbatim.
    assert!(
        blob.contains("service starting up nominally"),
        "normal line before the secret is preserved: {blob}"
    );
    assert!(
        blob.contains("service ready to accept traffic"),
        "normal line after the secret is preserved: {blob}"
    );
}
