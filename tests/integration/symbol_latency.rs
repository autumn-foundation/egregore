//! End-to-end tests for `eg audit symbol-latency` — the time-to-first-
//! citable-answer product gate (issue #57).
//!
//! Where issue #255 (`eg audit query-latency`) gates cold p50 for
//! `query symbol` alone and issue #120 (`eg audit query-budget`) covers the
//! structural path with an advisory absolute budget plus a hard scaling
//! gate, this gate is the product contract: every query class
//! (`symbol`, `file`, `symbol-at-commit`, `drift`) must return a
//! *correctness-checked, deterministic, citable* answer within a hard warm
//! p95 budget of 2s on the pinned reference corpus, across 5 consecutive
//! warm runs. Boring substitutes (`rg`, `git grep`, `git show`) are
//! timed for comparison with explicit comparability notes, and setup phases
//! (cold scan, history replay, ingest, embedding setup) are reported
//! separately, never gated.
//!
//! The committed tests use a cheap profile (`--warm-samples 2`,
//! `--history-commits 2`, `--at-commit-index 0`): they exercise gate
//! mechanics, answer checking, and report shape, not percentile precision.
//! The dedicated `symbol-latency` CI job runs the full manifest-default
//! profile (5 warm samples, 25 commits) — the authoritative enforcement.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

/// Serializes the gate-running tests: each full gate run builds a ~180MB
/// fixture tree under the 512MB `/tmp` tmpfs, so parallel runs exhaust it
/// and fail with "No space left on device".
static GATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/symbol_latency_corpus.json")
}

const CHEAP_PROFILE: &[&str] = &[
    "--warm-samples",
    "2",
    "--history-commits",
    "2",
    "--at-commit-index",
    "0",
];

/// Runs the latency gate and returns `(exit_code, parsed report, stderr)`.
///
/// The full benchmark (fixture build, warm samples for four query classes,
/// and substitutes) is expensive, so the committed tests use the cheap
/// profile above. The dedicated CI job runs the manifest-default gate.
fn run_gate(profile: &[&str], extra_args: &[&str]) -> (i32, Value, String) {
    let mut cmd = egregore();
    cmd.args(["audit", "symbol-latency", "--corpus"])
        .arg(manifest_path())
        .args(profile);
    for arg in extra_args {
        cmd.arg(arg);
    }
    let assert = cmd.assert();
    let output = assert.get_output();
    let code = output.status.code().unwrap_or(-1);
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("report should be valid JSON");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (code, report, stderr)
}

/// Asserts one query cell met the hard budget with a correctness-checked,
/// deterministic, citable answer.
fn assert_cell_ok(report: &Value, class: &str) {
    let cell = &report["queries"][class];
    assert!(
        cell["p50_ms"].as_f64().unwrap_or(0.0) > 0.0,
        "missing p50 for {class}: {report}"
    );
    assert!(
        cell["p95_ms"].as_f64().unwrap_or(0.0) >= cell["p50_ms"].as_f64().unwrap(),
        "p95 < p50 for {class}: {report}"
    );
    assert_eq!(
        cell["within_budget"], true,
        "{class} missed the hard budget"
    );
    assert_eq!(
        cell["deterministic"], true,
        "{class} answer not deterministic across warm runs"
    );
    let rows = cell["answer"]["rows"].as_u64().unwrap_or(0);
    assert!(rows > 0, "{class} returned no answer rows: {report}");
    let record_ids = cell["answer"]["record_ids"]
        .as_array()
        .expect("answer.record_ids");
    assert_eq!(
        record_ids.len() as u64,
        rows,
        "{class}: every row must cite a record ID: {report}"
    );
    for id in record_ids {
        assert!(
            !id.as_str().unwrap_or("").is_empty(),
            "{class}: empty record ID: {report}"
        );
    }
}

// The gate passes on a small fixture (cheap profile: 2 warm samples,
// 2 history commits, at-commit-index 0): every query class meets the hard
// with a correctness-checked, deterministic, citable answer; setup phases
// are reported separately; the substitutes are timed with comparability
// notes. This exercises gate mechanics, not the full 25-commit reference
// fixture — the full manifest-default run is the dedicated CI job's
// responsibility, and it currently fails symbol-at-commit (16s p95 on the
// 264k-record history; see docs/cli/symbol-latency.md).
#[test]
fn gate_passes_on_reference_corpus() {
    let _gate_guard = GATE_LOCK.lock().unwrap();
    let (code, report, stderr) = run_gate(CHEAP_PROFILE, &[]);
    assert_eq!(code, 0, "gate should pass: {report}\nstderr: {stderr}");
    assert_eq!(report["ok"], true);
    assert_eq!(report["corpus_name"], "symbol-latency-reference");
    assert_eq!(report["warm_samples"], 2);
    assert_eq!(report["budget_p95_ms"], 2000.0);

    // Fixture scale: >= 25k LOC, >= 500 symbols, 25 commits on the manifest
    // default (the cheap profile overrides the history depth, which is why
    // only the scan record count is asserted here).
    let record_count = report["environment"]["record_count"]
        .as_u64()
        .expect("environment.record_count");
    assert!(
        record_count >= 500,
        "fixture unexpectedly small: {record_count} records"
    );

    // Every query class reports p50/p95, met the hard budget, returned a
    // deterministic answer, and cited a record ID for every row.
    for class in ["symbol", "file", "symbol-at-commit", "drift"] {
        assert_cell_ok(&report, class);
    }

    // Setup phases are timed separately, never gated: the four phases are
    // present, each either measured or explicitly skipped with a reason.
    let mut phases: Vec<&str> = report["setup_phases"]
        .as_array()
        .expect("setup_phases")
        .iter()
        .map(|phase| {
            let name = phase["phase"].as_str().unwrap_or("");
            if phase["skipped"].as_bool().unwrap_or(false) {
                assert!(
                    !phase["skip_reason"].as_str().unwrap_or("").is_empty(),
                    "setup phase {name} skipped without a reason: {report}"
                );
            } else {
                assert!(
                    phase["duration_ms"].as_f64().unwrap_or(0.0) > 0.0,
                    "setup phase {name} missing duration: {report}"
                );
            }
            name
        })
        .collect();
    phases.sort_unstable();
    assert_eq!(
        phases,
        ["cold-scan", "embedding-setup", "history-replay", "ingest"],
        "unexpected setup phases: {report}"
    );

    // The boring substitutes are timed with explicit comparability notes.
    let substitutes = report["substitutes"].as_array().expect("substitutes");
    assert!(
        substitutes.len() >= 4,
        "expected at least 4 substitute rows: {report}"
    );
    for sub in substitutes {
        assert!(
            !sub["substitute"].as_str().unwrap_or("").is_empty(),
            "substitute missing name: {report}"
        );
        assert!(
            ["comparable", "not-comparable"].contains(&sub["comparability"].as_str().unwrap_or("")),
            "substitute missing comparability verdict: {report}"
        );
        assert!(
            !sub["comparability_note"].as_str().unwrap_or("").is_empty(),
            "substitute missing comparability note: {report}"
        );
    }
    // drift has no text-search equivalent: its substitute must say so.
    let drift_subs: Vec<&Value> = substitutes
        .iter()
        .filter(|sub| sub["query"].as_str() == Some("drift"))
        .collect();
    assert!(
        !drift_subs.is_empty(),
        "drift substitute row missing: {report}"
    );
    for sub in drift_subs {
        assert_eq!(sub["comparability"], "not-comparable");
    }
}

// The gate must actually bite: an unmeetable --budget-p95-ms proves the
// hard budget fails the gate (exit 1, ok=false) with the stable
// `symbol_latency_budget_exceeded` diagnostic naming the query class and
// the observed p95 — not a rubber stamp.
#[test]
fn unmeetable_budget_fails_with_stable_diagnostic() {
    let _gate_guard = GATE_LOCK.lock().unwrap();
    let (code, report, stderr) = run_gate(CHEAP_PROFILE, &["--budget-p95-ms", "1"]);
    assert_eq!(
        code, 1,
        "gate should fail on an unmeetable budget: {report}"
    );
    assert_eq!(report["ok"], false);
    let diagnostic: Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be the JSON diagnostic");
    assert_eq!(diagnostic["code"], "symbol_latency_budget_exceeded");
    let query = diagnostic["query"].as_str().unwrap_or("");
    assert!(
        ["symbol", "file", "symbol-at-commit", "drift"].contains(&query),
        "diagnostic should name the query class: {diagnostic}"
    );
    assert!(
        diagnostic["observed_p95_ms"].as_f64().unwrap_or(0.0) > 1.0,
        "diagnostic should carry the observed p95: {diagnostic}"
    );
    assert_eq!(diagnostic["budget_p95_ms"], 1.0);
}

// The report carries everything needed to interpret the measurement
// without re-running it: corpus identity, machine context, per-class warm
// cells with checked answers, setup phases, substitutes, and the gate
// result. The schema is complete whether the gate passes or fails — a
// violated budget (exit 1) still prints the full report.
#[test]
fn report_schema_is_complete() {
    let _gate_guard = GATE_LOCK.lock().unwrap();
    let (code, report, _) = run_gate(CHEAP_PROFILE, &["--budget-p95-ms", "1"]);
    assert_eq!(code, 1, "expected the unmeetable budget to fail: {report}");
    for key in [
        "corpus_name",
        "corpus_version",
        "query_symbol",
        "query_file",
        "drift_limit",
        "at_commit_index",
        "at_commit_sha",
        "environment",
        "setup_phases",
        "queries",
        "substitutes",
        "warm_samples",
        "budget_p95_ms",
        "history_record_count",
        "ok",
    ] {
        assert!(!report[key].is_null(), "report missing required key: {key}");
    }
    // Environment context (AC7): OS, CPU class, Rust profile, Egregore
    // version, corpus name, record counts, store kind.
    for key in [
        "os",
        "cpu_class",
        "rust_profile",
        "egregore_version",
        "corpus_name",
        "record_count",
        "store_kind",
    ] {
        assert!(
            !report["environment"][key].is_null(),
            "report.environment missing required key: {key}"
        );
    }
    for class in ["symbol", "file", "symbol-at-commit", "drift"] {
        for key in [
            "query",
            "samples_ms",
            "p50_ms",
            "p95_ms",
            "min_ms",
            "max_ms",
            "answer",
            "deterministic",
            "within_budget",
        ] {
            assert!(
                !report["queries"][class][key].is_null(),
                "report.queries.{class} missing required key: {key}"
            );
        }
        for key in ["rows", "record_ids", "handles"] {
            assert!(
                !report["queries"][class]["answer"][key].is_null(),
                "report.queries.{class}.answer missing required key: {key}"
            );
        }
    }
}

// Usage errors fail fast with the shared JSON error envelope on stderr
// (exit 2), never a half-printed report.
#[test]
fn invalid_args_are_rejected() {
    for args in [
        vec!["--warm-samples", "0"],
        vec!["--history-commits", "1"],
        vec!["--budget-p95-ms", "0"],
        // at_commit_index must stay below the effective history depth.
        vec!["--at-commit-index", "25"],
        vec!["--history-commits", "3", "--at-commit-index", "3"],
    ] {
        let output = egregore()
            .args(["audit", "symbol-latency", "--corpus"])
            .arg(manifest_path())
            .args(&args)
            .output()
            .expect("command should run");
        assert_eq!(
            output.status.code(),
            Some(2),
            "args {args:?} should be a usage error"
        );
        let envelope: Value =
            serde_json::from_slice(&output.stderr).expect("error envelope should be valid JSON");
        assert!(
            !envelope["code"].as_str().unwrap_or("").is_empty(),
            "error envelope should carry a code for args {args:?}"
        );
    }
}
