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
    let mut s = String::new();
    for i in 0..1000 {
        // Same error shape, only volatile fields vary: minute rolls, PID, UUID,
        // and pointer differ every line but normalize identically.
        let minute = i % 60;
        let pid = 1000 + i;
        s.push_str(&format!(
            "2026-01-02T03:{minute:02}:00Z [ERROR] request {:08x}-e29b-41d4-a716-446655440000 failed at 0x{:08x} pid={pid}\n",
            i, i,
        ));
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
    let scan = log_graph::scan_log_records(log_path, repo_root, REPO_ID, FIXED_TIME)
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
            id.starts_with("log:v1:"),
            "id must be log:v1:-prefixed: {id}"
        );
        assert_eq!(record["schema_version"], 1, "log schema version is 1");

        let producer = &record["producer"];
        assert_eq!(producer["producer_kind"], "log_importer");
        let comps = &producer["producer_components"];
        assert_eq!(comps["importer_schema_version"], "1");
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
    let scan_a =
        log_graph::scan_log_records(&plain, temp.path(), REPO_ID, FIXED_TIME).expect("scan a");
    let scan_b =
        log_graph::scan_log_records(&plain, temp.path(), REPO_ID, FIXED_TIME).expect("scan b");

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
    let scan = log_graph::scan_log_records(&plain, temp.path(), REPO_ID, FIXED_TIME).expect("scan");

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
