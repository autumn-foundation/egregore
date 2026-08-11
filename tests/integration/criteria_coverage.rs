//! Integration tests for `eg audit criteria-coverage` (issue #115): the
//! store-wide acceptance-criterion verification-coverage census and proof-gap
//! gate.
//!
//! Covers the labeled fixture's six buckets, the claimed-done-but-unproven set,
//! trust separation (recorded status and agent-minted evidence never prove), the
//! dangling bucket, the threshold gate's exit codes, the zero-denominator
//! vacuous pass, redaction safety, `--graph` vs `--data-dir` parity, strict
//! read-only behaviour, and byte-identical determinism across 5 runs.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

// ── fixture lines ───────────────────────────────────────────────────────────

fn task_line(id: &str, status: &str) -> String {
    format!(
        r#"{{"record_type":"node","id":"project:v1:{id}","kind":"Task","domain":"project","schema_version":1,"entity_id":"e-{id}","title":"SECRET-TASK-TITLE","status":"{status}","source_kind":"github_issue","valid_time":"2026-03-01T00:00:00Z","summary":"task {id}"}}"#
    )
}

fn criterion_line(
    id: &str,
    parent: &str,
    ordinal: u32,
    status: &str,
    link: Option<&str>,
) -> String {
    let link_field = link.map_or_else(String::new, |l| format!(r#","verification_link_id":"{l}""#));
    format!(
        r#"{{"record_type":"node","id":"project:v1:{id}","kind":"AcceptanceCriterion","domain":"project","schema_version":1,"entity_id":"e-{id}","parent_task_id":"project:v1:{parent}","ordinal":{ordinal},"status":"{status}","text":"SECRET-CRITERION-TEXT","source_handle":"tasks.jsonl:{id}:abc123"{link_field},"valid_time":"2026-03-01T00:00:00Z","summary":"criterion {id}"}}"#
    )
}

/// A verification record. Carries an evidence handle, as every real writer does
/// — the write path refuses a verification record without one.
fn verification_line(id: &str, kind: &str, status: Option<&str>, exit_code: Option<i64>) -> String {
    let status_field = status.map_or_else(String::new, |s| format!(r#","status":"{s}""#));
    let exit_field = exit_code.map_or_else(String::new, |c| format!(r#","exit_code":{c}"#));
    format!(
        r#"{{"record_type":"node","id":"{id}","kind":"{kind}","domain":"verification","schema_version":1,"text":"SECRET-COMMAND-OUTPUT","source_artifact_path":"tests/run.sh","source_artifact_hash":"blake3:fixture"{status_field}{exit_field},"summary":"run {id}"}}"#
    )
}

/// A verification record with NO evidence handle — what the daemon refuses to
/// persist but a hand-authored graph can still contain.
fn verification_line_without_handle(id: &str, status: &str) -> String {
    format!(
        r#"{{"record_type":"node","id":"{id}","kind":"TestRun","domain":"verification","schema_version":1,"status":"{status}","summary":"run {id}"}}"#
    )
}

/// A verification-SHAPED node minted under an `agent_memory:v1:` id — what
/// `eg command-evidence` writes from an agent-supplied exit code.
fn agent_minted_line(id: &str) -> String {
    format!(
        r#"{{"record_type":"node","id":"{id}","kind":"CommandEvidence","domain":"agent_memory","schema_version":1,"status":"pass","exit_code":0,"summary":"self-signed"}}"#
    )
}

fn project_edge_line(seq: u32, label: &str, source: &str, target: &str) -> String {
    format!(
        r#"{{"record_type":"edge","id":"project:v1:edge-{seq}","schema_version":1,"label":"{label}","source":"{source}","target":"{target}","summary":"{label}"}}"#
    )
}

fn tombstone_line(deleted_id: &str) -> String {
    format!(
        r#"{{"record_type":"tombstone","id":"project:v1:tomb-{deleted_id}","schema_version":1,"deleted_id":"{deleted_id}","summary":"removed"}}"#
    )
}

/// The labeled fixture: 10 live criteria across three tasks, one per bucket case.
///
/// | criterion | task | expected bucket |
/// |---|---|---|
/// | `ac-p1` | done | `proven` (edge → passing `TestRun`) |
/// | `ac-p2` | open | `proven` (field → passing `CommandRun` exit 0) |
/// | `ac-u1` | done | `unverified` |
/// | `ac-u2` | open | `unverified` (recorded `status: verified`, no evidence) |
/// | `ac-f1` | done | `failed_evidence` |
/// | `ac-d1` | open | `dangling_evidence` (absent target) |
/// | `ac-d2` | done | `dangling_evidence` (tombstoned target) |
/// | `ac-n1` | open | `non_verification_evidence` (agent-minted) |
/// | `ac-i1` | open | `inconclusive_evidence` |
/// | `ac-x1` | dropped | `unverified`, NOT claimed-done |
#[allow(clippy::too_many_lines)] // a literal fixture table, one entry per case
fn planted_lines() -> Vec<String> {
    vec![
        task_line("task-done", "closed_completed"),
        task_line("task-open", "open"),
        task_line("task-dropped", "closed_dropped"),
        verification_line("verification:v1:pass1", "TestRun", Some("pass"), None),
        verification_line("verification:v1:pass2", "CommandRun", None, Some(0)),
        verification_line("verification:v1:fail1", "TestRun", Some("fail"), None),
        verification_line("verification:v1:tombed", "TestRun", Some("pass"), None),
        verification_line(
            "verification:v1:incon",
            "Verification",
            Some("inconclusive"),
            None,
        ),
        agent_minted_line("agent_memory:v1:selfsigned"),
        tombstone_line("verification:v1:tombed"),
        criterion_line("ac-p1", "task-done", 0, "verified", None),
        project_edge_line(
            1,
            "CLOSES_ACCEPTANCE_CRITERION",
            "project:v1:ac-p1",
            "verification:v1:pass1",
        ),
        project_edge_line(
            2,
            "OWNED_BY_TASK",
            "project:v1:ac-p1",
            "project:v1:task-done",
        ),
        criterion_line(
            "ac-p2",
            "task-open",
            0,
            "verified",
            Some("verification:v1:pass2"),
        ),
        project_edge_line(
            3,
            "OWNED_BY_TASK",
            "project:v1:ac-p2",
            "project:v1:task-open",
        ),
        criterion_line("ac-u1", "task-done", 1, "unverified", None),
        project_edge_line(
            4,
            "OWNED_BY_TASK",
            "project:v1:ac-u1",
            "project:v1:task-done",
        ),
        criterion_line("ac-u2", "task-open", 1, "verified", None),
        project_edge_line(
            5,
            "OWNED_BY_TASK",
            "project:v1:ac-u2",
            "project:v1:task-open",
        ),
        criterion_line("ac-f1", "task-done", 2, "failed", None),
        project_edge_line(
            6,
            "CLOSES_ACCEPTANCE_CRITERION",
            "project:v1:ac-f1",
            "verification:v1:fail1",
        ),
        project_edge_line(
            7,
            "OWNED_BY_TASK",
            "project:v1:ac-f1",
            "project:v1:task-done",
        ),
        criterion_line("ac-d1", "task-open", 2, "verified", None),
        project_edge_line(
            8,
            "CLOSES_ACCEPTANCE_CRITERION",
            "project:v1:ac-d1",
            "verification:v1:ghost",
        ),
        project_edge_line(
            9,
            "OWNED_BY_TASK",
            "project:v1:ac-d1",
            "project:v1:task-open",
        ),
        criterion_line("ac-d2", "task-done", 3, "verified", None),
        project_edge_line(
            10,
            "CLOSES_ACCEPTANCE_CRITERION",
            "project:v1:ac-d2",
            "verification:v1:tombed",
        ),
        project_edge_line(
            11,
            "OWNED_BY_TASK",
            "project:v1:ac-d2",
            "project:v1:task-done",
        ),
        criterion_line("ac-n1", "task-open", 3, "verified", None),
        project_edge_line(
            12,
            "CLOSES_ACCEPTANCE_CRITERION",
            "project:v1:ac-n1",
            "agent_memory:v1:selfsigned",
        ),
        project_edge_line(
            13,
            "OWNED_BY_TASK",
            "project:v1:ac-n1",
            "project:v1:task-open",
        ),
        criterion_line("ac-i1", "task-open", 4, "unknown", None),
        project_edge_line(
            14,
            "CLOSES_ACCEPTANCE_CRITERION",
            "project:v1:ac-i1",
            "verification:v1:incon",
        ),
        project_edge_line(
            15,
            "OWNED_BY_TASK",
            "project:v1:ac-i1",
            "project:v1:task-open",
        ),
        criterion_line("ac-x1", "task-dropped", 0, "unverified", None),
        project_edge_line(
            16,
            "OWNED_BY_TASK",
            "project:v1:ac-x1",
            "project:v1:task-dropped",
        ),
    ]
}

fn write_graph(lines: &[String]) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("planted.graph.jsonl");
    fs::write(&path, lines.join("\n")).expect("write graph");
    (temp, path)
}

fn planted_graph() -> (tempfile::TempDir, PathBuf) {
    write_graph(&planted_lines())
}

fn run_graph(path: &Path, extra: &[&str]) -> (i32, Value, Vec<u8>) {
    let mut cmd = egregore();
    cmd.args(["audit", "criteria-coverage", "--graph"])
        .arg(path);
    for a in extra {
        cmd.arg(a);
    }
    let output = cmd.output().expect("run criteria-coverage");
    let code = output.status.code().expect("exit code");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (code, value, output.stdout)
}

fn row<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["criteria"]
        .as_array()
        .expect("criteria array")
        .iter()
        .find(|r| r["record_id"] == format!("project:v1:{id}"))
        .unwrap_or_else(|| panic!("row for {id} present"))
}

// ── AC1/AC2: the census ─────────────────────────────────────────────────────

#[test]
fn planted_fixture_classifies_every_criterion_into_its_labeled_bucket() {
    let (_g, path) = planted_graph();
    let (code, report, _) = run_graph(&path, &[]);
    assert_eq!(code, 1, "the planted proof gap must trip the gate");
    assert_eq!(report["total_criteria"], 10);
    for (id, expected) in [
        ("ac-p1", "proven"),
        ("ac-p2", "proven"),
        ("ac-u1", "unverified"),
        ("ac-u2", "unverified"),
        ("ac-f1", "failed_evidence"),
        ("ac-d1", "dangling_evidence"),
        ("ac-d2", "dangling_evidence"),
        ("ac-n1", "non_verification_evidence"),
        ("ac-i1", "inconclusive_evidence"),
        ("ac-x1", "unverified"),
    ] {
        assert_eq!(row(&report, id)["bucket"], expected, "bucket for {id}");
    }
}

#[test]
fn ratios_report_numerator_and_denominator_and_buckets_partition() {
    let (_g, path) = planted_graph();
    let (_code, report, _) = run_graph(&path, &[]);
    for (metric, numerator) in [
        ("proven", 2),
        ("unverified", 3),
        ("failed_evidence", 1),
        ("dangling_evidence", 2),
        ("non_verification_evidence", 1),
        ("inconclusive_evidence", 1),
        ("proof_gap", 8),
    ] {
        assert_eq!(report[metric]["numerator"], numerator, "{metric} numerator");
        assert_eq!(report[metric]["denominator"], 10, "{metric} denominator");
        assert!(report[metric]["ratio"].is_number(), "{metric} ratio");
    }
    let counts = report["bucket_counts"].as_object().expect("bucket_counts");
    let sum: u64 = counts.values().map(|v| v.as_u64().unwrap()).sum();
    assert_eq!(sum, 10, "buckets must partition the census");
    assert_eq!(counts.len(), 6, "every bucket present, even at zero");
}

#[test]
fn a_store_with_no_acceptance_criteria_is_a_vacuous_pass() {
    // A populated store that simply records no criteria: exit 0, null ratios,
    // and the stable diagnostic — never a divide-by-zero, never a fabricated 0.0.
    let (_g, path) = write_graph(&[task_line("task-open", "open")]);
    let (code, report, _) = run_graph(&path, &[]);
    assert_eq!(code, 0, "no criteria is a vacuous pass");
    assert_eq!(report["total_criteria"], 0);
    assert!(report["proven"]["ratio"].is_null(), "ratio must be null");
    assert_eq!(report["proven"]["denominator"], 0);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "no_acceptance_criteria")
    );
}

// ── AC3: claimed-done-but-unproven ──────────────────────────────────────────

#[test]
fn claimed_done_unproven_lists_the_full_set_with_task_and_verification_handles() {
    let (_g, path) = planted_graph();
    let (_code, report, _) = run_graph(&path, &[]);
    let set = report["claimed_done_unproven"].as_array().expect("array");
    let ids: Vec<&str> = set
        .iter()
        .map(|r| r["record_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["project:v1:ac-d2", "project:v1:ac-f1", "project:v1:ac-u1"],
        "100% of the claimed-done-but-unproven set, sorted"
    );
    assert_eq!(report["claimed_done_unproven_count"], 3);
    for r in set {
        assert_eq!(r["parent_task_id"], "project:v1:task-done");
    }
    // The closing verification record ID is carried where one exists.
    let failed = set
        .iter()
        .find(|r| r["record_id"] == "project:v1:ac-f1")
        .unwrap();
    assert_eq!(
        failed["closing_links"][0]["handle"],
        "verification:v1:fail1"
    );
    // A criterion under a DROPPED task claims no completion.
    assert!(!ids.contains(&"project:v1:ac-x1"));
    assert_eq!(report["done_task_statuses"][0], "closed_completed");
}

// ── AC4: trust separation ───────────────────────────────────────────────────

#[test]
fn recorded_status_and_agent_minted_evidence_never_prove_a_criterion() {
    let (_g, path) = planted_graph();
    let (_code, report, _) = run_graph(&path, &[]);

    // `ac-u2` records `status: verified` with NO closing evidence.
    assert_eq!(row(&report, "ac-u2")["bucket"], "unverified");
    assert_eq!(row(&report, "ac-u2")["criterion_status"], "verified");

    // An `agent_memory:v1:` CommandEvidence claiming pass/exit 0 proves nothing:
    // an agent-authored claim is never evidence for itself.
    assert_eq!(row(&report, "ac-n1")["bucket"], "non_verification_evidence");
    assert_eq!(
        row(&report, "ac-n1")["closing_links"][0]["resolution"],
        "not_verification_record"
    );
    assert!(row(&report, "ac-n1")["proving_verification_id"].is_null());

    // `Task.status: closed_completed` never confers proof either.
    for id in ["ac-u1", "ac-f1", "ac-d2"] {
        assert_ne!(row(&report, id)["bucket"], "proven", "{id}");
    }
}

// ── AC5: dangling closing edges ─────────────────────────────────────────────

#[test]
fn dangling_closing_edges_keep_their_handles_in_their_own_bucket() {
    let (_g, path) = planted_graph();
    let (_code, report, _) = run_graph(&path, &[]);
    for (id, handle) in [
        ("ac-d1", "verification:v1:ghost"),
        ("ac-d2", "verification:v1:tombed"),
    ] {
        let r = row(&report, id);
        assert_eq!(r["bucket"], "dangling_evidence");
        assert_eq!(r["closing_links"][0]["handle"], handle);
        assert_eq!(r["closing_links"][0]["resolution"], "unresolved");
    }
    // Never counted proven, never merged into unverified.
    assert_eq!(report["proven"]["numerator"], 2);
    assert_eq!(report["unverified"]["numerator"], 3);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "dangling_closing_evidence")
    );
}

// ── AC6: citable rows in both formats ───────────────────────────────────────

#[test]
fn every_row_carries_a_citable_record_id_and_source_handle() {
    let (_g, path) = planted_graph();
    let (_code, report, _) = run_graph(&path, &[]);
    for r in report["criteria"].as_array().unwrap() {
        let id = r["record_id"].as_str().expect("record_id");
        assert!(id.starts_with("project:v"), "{id}");
        assert!(r["parent_task_id"].is_string(), "{id} parent");
        assert!(
            r["source_handle"].as_str().is_some_and(|h| !h.is_empty()),
            "{id} source-system handle"
        );
    }
}

#[test]
fn text_format_mirrors_the_json_contract() {
    let (_g, path) = planted_graph();
    let (json_code, report, _) = run_graph(&path, &[]);
    let (text_code, _, text_bytes) = run_graph(&path, &["--format", "text"]);
    assert_eq!(json_code, text_code, "both formats share the gate verdict");
    let text = String::from_utf8(text_bytes).expect("utf-8");
    assert!(text.contains("ok: false"));
    assert!(text.contains("total_criteria: 10"));
    // Ratios render as numerator/denominator, never a bare percentage.
    assert!(text.contains("proven: 2/10"), "{text}");
    assert!(text.contains("proof_gap: 8/10"), "{text}");
    // Every claimed-done row appears with its task and closing handles.
    for id in ["ac-d2", "ac-f1", "ac-u1"] {
        assert!(text.contains(&format!("project:v1:{id}")), "{id} in text");
    }
    assert!(text.contains(report["disclaimer"].as_str().unwrap()));
}

// ── AC7: the threshold gate ─────────────────────────────────────────────────

#[test]
fn gate_trips_with_a_stable_diagnostic_naming_the_metric_and_value() {
    let (_g, path) = planted_graph();
    let (code, report, _) = run_graph(&path, &[]);
    assert_eq!(code, 1, "over the proof-gap line is never reported ready");
    assert_eq!(report["ok"], false);
    let breaches = report["breaches"].as_array().expect("breaches");
    let metrics: Vec<&str> = breaches
        .iter()
        .map(|b| b["metric"].as_str().unwrap())
        .collect();
    assert_eq!(metrics, vec!["claimed_done_unproven", "proven_ratio"]);
    let proven = breaches
        .iter()
        .find(|b| b["metric"] == "proven_ratio")
        .unwrap();
    assert_eq!(proven["observed"], "0.2000");
    assert_eq!(proven["bound"], "1.0000");
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"below_proven_ratio_threshold"), "{codes:?}");
    assert!(
        codes.contains(&"above_claimed_done_unproven_threshold"),
        "{codes:?}"
    );
}

#[test]
fn gate_passes_under_the_documented_bounds() {
    let (_g, path) = planted_graph();
    let (code, report, _) = run_graph(
        &path,
        &[
            "--min-proven-ratio",
            "0.1",
            "--max-claimed-done-unproven",
            "5",
        ],
    );
    assert_eq!(code, 0, "under the line the gate passes");
    assert_eq!(report["ok"], true);
    assert!(report["breaches"].as_array().is_none_or(Vec::is_empty));
}

#[test]
fn usage_errors_exit_two_with_machine_readable_codes() {
    let (_g, path) = planted_graph();

    // Both input flags.
    let out = egregore()
        .args(["audit", "criteria-coverage", "--graph"])
        .arg(&path)
        .args(["--data-dir", "/nonexistent"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("conflicting_input_flags"));

    // Neither input flag.
    let out = egregore()
        .args(["audit", "criteria-coverage"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing_input_flag"));

    // Out-of-range threshold: never silently disables the gate.
    let (code, _, _) = run_graph(&path, &["--min-proven-ratio", "1.5"]);
    assert_eq!(code, 2);
    let out = egregore()
        .args(["audit", "criteria-coverage", "--graph"])
        .arg(&path)
        .args(["--min-proven-ratio", "1.5"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid_min_proven_ratio"));

    // Out-of-range limit.
    let out = egregore()
        .args(["audit", "criteria-coverage", "--graph"])
        .arg(&path)
        .args(["--limit", "0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid_limit"));

    // Empty input file is a LOAD error, distinct from the vacuous pass.
    let (_e, empty) = write_graph(&[]);
    let out = egregore()
        .args(["audit", "criteria-coverage", "--graph"])
        .arg(&empty)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("empty_evidence_input"));
}

// ── AC8: redaction safety ───────────────────────────────────────────────────

#[test]
fn output_never_leaks_prose_bodies_in_either_format() {
    let (_g, path) = planted_graph();
    for extra in [vec![], vec!["--format", "text"]] {
        let (_code, _report, bytes) = run_graph(&path, &extra);
        let text = String::from_utf8(bytes).expect("utf-8");
        for secret in [
            "SECRET-CRITERION-TEXT",
            "SECRET-TASK-TITLE",
            "SECRET-COMMAND-OUTPUT",
        ] {
            assert!(!text.contains(secret), "report leaked {secret} ({extra:?})");
        }
    }
}

// ── AC9: determinism and read-only ──────────────────────────────────────────

#[test]
fn output_is_byte_identical_across_five_consecutive_runs() {
    let (_g, path) = planted_graph();
    let (_c, _v, first) = run_graph(&path, &[]);
    for _ in 0..5 {
        let (_c, _v, again) = run_graph(&path, &[]);
        assert_eq!(first, again, "output must be byte-identical across runs");
    }
}

#[test]
fn the_graph_input_is_never_mutated() {
    let (_g, path) = planted_graph();
    let before = fs::read(&path).expect("read before");
    let (_code, _report, _) = run_graph(&path, &[]);
    let after = fs::read(&path).expect("read after");
    assert_eq!(before, after, "criteria-coverage must never mutate --graph");
}

/// Sorted `(relative path, bytes)` fingerprint of every file under `root`.
#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let ft = entry.file_type().unwrap();
            let path = entry.path();
            if ft.is_dir() {
                walk(&path, base, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// The planted fixture minus the ONE line an embedded store cannot hold.
///
/// The ingest write path enforces referential integrity, so an edge whose target
/// node was never written is rejected outright — the `ac-d1` *absent-target*
/// dangling case is structurally unreachable through a store and is therefore a
/// `--graph`-only scenario. The *tombstoned-target* dangling case (`ac-d2`) IS
/// reachable in a store (write the record, retract it later) and keeps the
/// dangling bucket exercised on both transports. Dropping the edge leaves
/// `ac-d1` with no closing link at all, so it moves to `unverified`.
#[cfg(feature = "embedded-aletheiadb")]
fn embeddable_lines() -> Vec<String> {
    planted_lines()
        .into_iter()
        .filter(|line| !line.contains("verification:v1:ghost"))
        .collect()
}

/// Ingests the embeddable fixture into a fresh embedded store, and also writes
/// the same lines as a graph so the two transports compare like for like.
#[cfg(feature = "embedded-aletheiadb")]
fn ingest_planted() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("embeddable.graph.jsonl");
    fs::write(&graph, embeddable_lines().join("\n")).expect("write graph");
    let data_dir = temp.path().join("store");
    egregore()
        .arg("ingest")
        .arg(&graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    (temp, data_dir, graph)
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_read_is_strictly_read_only_and_creates_no_records() {
    let (_temp, data_dir, _graph) = ingest_planted();
    let before = dir_fingerprint(&data_dir);
    let out = egregore()
        .args(["audit", "criteria-coverage", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run");
    assert!(matches!(out.status.code(), Some(0 | 1)), "ran to a verdict");
    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "criteria-coverage must not create or mutate a single byte of the store"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn graph_and_data_dir_agree_on_the_gate_verdict_and_metrics() {
    let (_temp, data_dir, graph) = ingest_planted();
    let out = egregore()
        .args(["audit", "criteria-coverage", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run");
    let store: Value = serde_json::from_slice(&out.stdout).expect("json");

    // The SAME lines over `--graph`, so any difference is a transport
    // divergence rather than a fixture difference.
    let (graph_code, graph_report, _) = run_graph(&graph, &[]);

    // The dangling bucket must still be exercised on the store side, or this
    // parity check would silently degrade into comparing two easy cases.
    assert_eq!(
        store["dangling_evidence"]["numerator"], 1,
        "the tombstoned-target dangling case must survive ingest"
    );
    assert_eq!(out.status.code(), Some(graph_code), "same gate verdict");
    // Both sides are built from IDENTICAL lines, so the WHOLE report must match
    // — comparing only the scalars would hide an ingest round-trip that dropped
    // `source_handle`, `ordinal`, `criterion_status`, or a closing link.
    assert_eq!(
        store, graph_report,
        "--graph and --data-dir must produce the identical report"
    );
}

// ── review round 2: hostile input, gaps, and CLI-level plumbing ─────────────

/// A crafted store value must not be able to forge output lines or drive the
/// reader's terminal in EITHER format.
///
/// `docs/schema/verification.md` documents `status` as a free string with no
/// enum enforcement, and a dangling `verification_link_id` is reported with its
/// ORIGINAL text — the one value no writer validated.
#[test]
fn hostile_store_values_cannot_forge_output_or_drive_the_terminal() {
    let ansi = "\u{1b}[2J";
    let evil_status = format!("pass{ansi}\nbucket=proven\n{}", "A".repeat(5000));
    let evil_handle = "verification:v1:x\nclaimed_done_unproven: 0\nfake";
    let lines = vec![
        task_line("task-done", "closed_completed"),
        format!(
            r#"{{"record_type":"node","id":"verification:v1:evil","kind":"TestRun","domain":"verification","schema_version":1,"source_artifact_hash":"blake3:evil","status":{},"summary":"evil"}}"#,
            serde_json::to_string(&evil_status).unwrap()
        ),
        // Resolves to the hostile record, so its `status` really flows through.
        criterion_line(
            "ac-evil",
            "task-done",
            0,
            "verified",
            Some("verification:v1:evil"),
        ),
        // Carries the forged DANGLING handle — the one value no writer validated.
        format!(
            r#"{{"record_type":"node","id":"project:v1:ac-forged","kind":"AcceptanceCriterion","domain":"project","schema_version":1,"parent_task_id":"project:v1:task-done","ordinal":1,"status":"verified","verification_link_id":{},"summary":"forged"}}"#,
            serde_json::to_string(evil_handle).unwrap()
        ),
    ];
    let (_g, path) = write_graph(&lines);

    for extra in [vec![], vec!["--format", "text"]] {
        let (_code, report, bytes) = run_graph(&path, &extra);
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(
            !text.contains('\u{1b}'),
            "an ANSI escape reached the output ({extra:?})"
        );
        // The forged fragments must never appear as a line of their own.
        for forged in ["bucket=proven", "claimed_done_unproven: 0", "fake"] {
            assert!(
                !text.lines().any(|l| l.trim_start() == forged),
                "a crafted value forged the line {forged:?} ({extra:?})"
            );
        }
        if extra.is_empty() {
            // JSON: the whole report is one line, so bound the FIELD values.
            let evil = report["criteria"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["record_id"] == "project:v1:ac-evil")
                .expect("the evil row");
            let status = evil["closing_links"][0]["status"]
                .as_str()
                .expect("the hostile status flowed through a RESOLVED link");
            assert!(
                status.chars().count() <= 129,
                "the free-text status was not bounded: {} chars",
                status.chars().count()
            );
            assert!(!status.contains('\n') && !status.contains('\u{1b}'));
            // The dangling handle is sanitized but kept WHOLE — a truncated
            // handle would stop being a citation.
            let forged = report["criteria"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["record_id"] == "project:v1:ac-forged")
                .expect("the forged row");
            let handle = forged["closing_links"][0]["handle"].as_str().unwrap();
            assert!(!handle.contains('\n'), "newline survived in a handle");
            assert!(handle.ends_with("fake"), "handle must not be truncated");
        } else {
            // Text: no single rendered line may blow up.
            assert!(
                text.lines().all(|l| l.chars().count() < 2000),
                "an unbounded value reached a text line"
            );
        }
    }
}

/// A `verification:v1:`-prefixed record with a non-verification kind, and a
/// criterion closing itself, must both be refused at the CLI level.
#[test]
fn prefix_alone_does_not_certify_a_criterion() {
    let lines = vec![
        task_line("task-done", "closed_completed"),
        // A verification-PREFIXED Observation claiming to pass.
        r#"{"record_type":"node","id":"verification:v1:impostor","kind":"Observation","domain":"agent_memory","schema_version":1,"status":"pass","summary":"impostor"}"#.to_owned(),
        criterion_line(
            "ac-impostor",
            "task-done",
            0,
            "verified",
            Some("verification:v1:impostor"),
        ),
    ];
    let (_g, path) = write_graph(&lines);
    let (code, report, _) = run_graph(&path, &[]);
    let r = row(&report, "ac-impostor");
    assert_eq!(
        r["bucket"], "non_verification_evidence",
        "a verification: prefix on an Observation must not prove"
    );
    assert_eq!(
        r["closing_links"][0]["resolution"],
        "not_verification_record"
    );
    assert!(r["proving_verification_id"].is_null());
    assert_eq!(code, 1, "and the gate must fail");
}

/// Every `proven` row discloses the producer that WROTE its closing record,
/// because the verification-domain gate does not establish independent
/// observation.
#[test]
fn proven_rows_cite_the_proving_record_and_disclose_its_producer() {
    let (_g, path) = planted_graph();
    let (_code, report, _) = run_graph(&path, &[]);
    assert_eq!(
        row(&report, "ac-p1")["proving_verification_id"],
        "verification:v1:pass1"
    );
    assert_eq!(
        row(&report, "ac-p2")["proving_verification_id"],
        "verification:v1:pass2",
        "the field-sourced link cites its record too"
    );
    // `node_kind` is the trusted closed vocabulary, always present when resolved.
    assert_eq!(
        row(&report, "ac-p1")["closing_links"][0]["node_kind"],
        "TestRun"
    );
    // A non-proven row never names a proving record.
    for id in ["ac-u1", "ac-f1", "ac-d1", "ac-n1", "ac-i1"] {
        assert!(
            row(&report, id)["proving_verification_id"].is_null(),
            "{id} must not name a proving record"
        );
    }
}

/// The edge and the denormalized field naming the SAME verification collapse to
/// one link carrying both origins.
#[test]
fn edge_and_field_naming_one_record_merge_into_a_single_link() {
    let mut lines = vec![
        task_line("task-done", "closed_completed"),
        verification_line("verification:v1:pass1", "TestRun", Some("pass"), None),
        criterion_line(
            "ac-both",
            "task-done",
            0,
            "verified",
            Some("verification:v1:pass1"),
        ),
    ];
    lines.push(project_edge_line(
        1,
        "CLOSES_ACCEPTANCE_CRITERION",
        "project:v1:ac-both",
        "verification:v1:pass1",
    ));
    let (_g, path) = write_graph(&lines);
    let (code, report, _) = run_graph(&path, &[]);
    let closing = row(&report, "ac-both")["closing_links"]
        .as_array()
        .expect("closing links");
    assert_eq!(closing.len(), 1, "one handle, one link");
    assert_eq!(
        closing[0]["origins"],
        serde_json::json!(["closes_edge", "verification_link_id"]),
        "both representations are disclosed"
    );
    assert_eq!(code, 0, "a fully proven store passes");
}

/// `--limit` truncates both lists while the counts stay pre-truncation.
#[test]
fn limit_truncates_at_the_cli_while_counts_stay_whole() {
    let (_g, path) = planted_graph();
    let (_code, report, _) = run_graph(&path, &["--limit", "2"]);
    assert_eq!(report["total_criteria"], 10, "counts are pre-truncation");
    assert_eq!(report["criteria"].as_array().unwrap().len(), 2);
    assert_eq!(report["claimed_done_unproven_count"], 3);
    assert_eq!(report["claimed_done_unproven"].as_array().unwrap().len(), 2);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "results_truncated")
    );
    // Boundaries: the documented maximum is accepted, one past it is rejected.
    let (accepted, _, _) = run_graph(&path, &["--limit", "1000"]);
    assert_eq!(accepted, 1, "1000 is within range (gate still fails)");
    let (rejected, _, _) = run_graph(&path, &["--limit", "1001"]);
    assert_eq!(rejected, 2, "1001 exceeds the documented maximum");
}

/// The text form renders the zero-denominator case as `n/a`, never `0.0000`.
#[test]
fn text_format_renders_the_zero_denominator_case() {
    let (_g, path) = write_graph(&[task_line("task-open", "open")]);
    let (code, _v, bytes) = run_graph(&path, &["--format", "text"]);
    let text = String::from_utf8(bytes).expect("utf-8");
    assert_eq!(code, 0);
    assert!(text.contains("proven: 0/0 (n/a)"), "{text}");
    assert!(!text.contains("0.0000"), "no fabricated ratio: {text}");
    assert!(text.contains("no_acceptance_criteria"));
}

/// The text form carries the same citable evidence the JSON row carries.
#[test]
fn text_rows_carry_their_citable_evidence() {
    let (_g, path) = planted_graph();
    let (_code, _v, bytes) = run_graph(&path, &["--format", "text"]);
    let text = String::from_utf8(bytes).expect("utf-8");
    assert!(text.contains("closing=verification:v1:fail1"), "{text}");
    assert!(text.contains("resolution=failing"), "{text}");
    assert!(text.contains("proven_by=verification:v1:pass1"), "{text}");
    assert!(
        text.contains("source_handle=tasks.jsonl:ac-p1:abc123"),
        "{text}"
    );
    // Diagnostics name the records they derive from.
    assert!(text.contains("dangling_closing_evidence"), "{text}");
    assert!(
        text.lines().any(|l| l.trim() == "project:v1:ac-d1"),
        "diagnostic record IDs are rendered: {text}"
    );
}

/// Text output is deterministic too, not just JSON.
#[test]
fn text_output_is_byte_identical_across_five_runs() {
    let (_g, path) = planted_graph();
    let (_c, _v, first) = run_graph(&path, &["--format", "text"]);
    for _ in 0..5 {
        let (_c, _v, again) = run_graph(&path, &["--format", "text"]);
        assert_eq!(first, again, "text output must be byte-identical");
    }
}

/// A missing `--data-dir` is a load error, not a panic or a silent pass.
#[test]
fn a_missing_data_dir_exits_two() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("no-such-store");
    let out = egregore()
        .args(["audit", "criteria-coverage", "--data-dir"])
        .arg(&missing)
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "no report on a load error");
}

/// A criterion carrying an `EXTERNAL_HANDLE` edge, a path, and a span cites all
/// three — the "where present" half of the citation contract.
#[test]
fn optional_citation_handles_are_emitted_when_recorded() {
    let lines = vec![
        task_line("task-open", "open"),
        r#"{"record_type":"node","id":"project:v1:extlink","kind":"ExternalLink","domain":"project","schema_version":1,"url":"https://example.invalid/1","summary":"link"}"#.to_owned(),
        r#"{"record_type":"node","id":"project:v1:ac-cited","kind":"AcceptanceCriterion","domain":"project","schema_version":1,"parent_task_id":"project:v1:task-open","ordinal":0,"status":"unverified","repo_relative_path":"docs/spec.md","span":{"start_byte":0,"end_byte":10,"start_line":3,"end_line":4},"source_handle":"tasks.jsonl:ac-cited:h","summary":"cited"}"#.to_owned(),
        project_edge_line(
            1,
            "EXTERNAL_HANDLE",
            "project:v1:ac-cited",
            "project:v1:extlink",
        ),
    ];
    let (_g, path) = write_graph(&lines);
    let (_code, report, _) = run_graph(&path, &[]);
    let r = row(&report, "ac-cited");
    assert_eq!(r["repo_relative_path"], "docs/spec.md");
    assert_eq!(r["span"]["start_line"], 3);
    assert_eq!(r["source_handle"], "tasks.jsonl:ac-cited:h");
    assert_eq!(r["external_link_id"], "project:v1:extlink");
}

/// A verification record citing no evidence handle, and a verification kind the
/// closure edge may not target, must both be refused at the CLI level.
#[test]
fn evidence_provenance_and_legal_closure_kinds_are_required() {
    // (a) A bare passing TestRun with no evidence handle.
    let lines = vec![
        task_line("task-done", "closed_completed"),
        verification_line_without_handle("verification:v1:bare", "pass"),
        criterion_line(
            "ac-bare",
            "task-done",
            0,
            "verified",
            Some("verification:v1:bare"),
        ),
    ];
    let (_g, path) = write_graph(&lines);
    let (code, report, _) = run_graph(&path, &[]);
    assert_eq!(
        row(&report, "ac-bare")["bucket"],
        "non_verification_evidence"
    );
    assert_eq!(
        row(&report, "ac-bare")["closing_links"][0]["resolution"],
        "missing_evidence_handle"
    );
    assert_eq!(code, 1, "a record citing nothing cannot prove");

    // (b) A passing CoverageReport — a verification kind, but not one a
    // CLOSES_ACCEPTANCE_CRITERION edge may target.
    let lines = vec![
        task_line("task-done", "closed_completed"),
        verification_line("verification:v1:cov", "CoverageReport", Some("pass"), None),
        criterion_line(
            "ac-cov",
            "task-done",
            0,
            "verified",
            Some("verification:v1:cov"),
        ),
    ];
    let (_g, path) = write_graph(&lines);
    let (code, report, _) = run_graph(&path, &[]);
    assert_eq!(
        row(&report, "ac-cov")["bucket"],
        "non_verification_evidence"
    );
    assert_eq!(
        row(&report, "ac-cov")["closing_links"][0]["resolution"],
        "not_verification_record"
    );
    assert_eq!(code, 1);
}

/// A `Task` rewritten from `closed_completed` to `open` must not let record
/// order drop its unproven criteria out of the gap set.
#[test]
fn a_task_status_mutation_cannot_hide_a_criterion_from_the_gate() {
    let lines = vec![
        task_line("task-done", "closed_completed"),
        criterion_line("ac-hidden", "task-done", 0, "unverified", None),
        // A second write of the SAME task id, sorting/appearing later.
        task_line("task-done", "open"),
    ];
    let (_g, path) = write_graph(&lines);
    let (code, report, _) = run_graph(&path, &[]);
    assert!(
        report["claimed_done_unproven"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["record_id"] == "project:v1:ac-hidden"),
        "a done version anywhere must hold the criterion in the gap set"
    );
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "parent_task_status_ambiguous"),
        "the ambiguity must be disclosed"
    );
    assert_eq!(code, 1);
}
