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

fn verification_line(id: &str, kind: &str, status: Option<&str>, exit_code: Option<i64>) -> String {
    let status_field = status.map_or_else(String::new, |s| format!(r#","status":"{s}""#));
    let exit_field = exit_code.map_or_else(String::new, |c| format!(r#","exit_code":{c}"#));
    format!(
        r#"{{"record_type":"node","id":"{id}","kind":"{kind}","domain":"verification","schema_version":1,"text":"SECRET-COMMAND-OUTPUT"{status_field}{exit_field},"summary":"run {id}"}}"#
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
        "non_verification_domain"
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
    for field in [
        "total_criteria",
        "proven",
        "unverified",
        "failed_evidence",
        "dangling_evidence",
        "non_verification_evidence",
        "inconclusive_evidence",
        "proof_gap",
        "bucket_counts",
        "claimed_done_unproven_count",
    ] {
        assert_eq!(
            store[field], graph_report[field],
            "--graph and --data-dir disagree on {field}"
        );
    }
}
