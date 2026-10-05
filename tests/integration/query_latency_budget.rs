//! End-to-end tests for `eg audit query-latency` — the cold query-latency
//! budget gate for time-to-first-symbol-answer (issue #255).

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/query_latency_corpus.json")
}

/// Runs the latency benchmark gate and returns `(exit_code, parsed report)`.
///
/// `extra_args` selects the measurement profile: the full two-source gate is
/// expensive (an embedded ingest plus 22 cold processes), so only the
/// dedicated gate test runs it. The remaining tests use `--source graph
/// --samples 1` (a single cold process, no ingest) or exercise the
/// pre-measurement validation paths.
fn run_gate(extra_args: &[&str]) -> (i32, Value) {
    let mut cmd = egregore();
    cmd.args(["audit", "query-latency", "--corpus"])
        .arg(manifest_path());
    for arg in extra_args {
        cmd.arg(arg);
    }
    let assert = cmd.assert();
    let output = assert.get_output();
    let code = output.status.code().unwrap_or(-1);
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("report should be valid JSON");
    (code, report)
}

fn source(report: &Value, name: &str) -> Value {
    report["sources"][name].clone()
}

// The gate passes on the pinned reference corpus: both input sources are
// measured, p50 sits under the documented 2s budget, and the report records
// the corpus size so the measurement stays auditable. This is the one test
// that runs the full benchmark (embedded ingest + 11 cold samples per
// source); the dedicated CI step runs the same command.
#[test]
fn gate_passes_on_reference_corpus() {
    let (code, report) = run_gate(&[]);
    assert_eq!(code, 0, "gate should pass: {report}");
    assert_eq!(report["ok"], true);
    assert_eq!(report["corpus_name"], "query-latency-reference");

    // The corpus size is recorded in the benchmark so the measurement is
    // auditable; guard against the fixture silently collapsing.
    let record_count = report["record_count"].as_u64().expect("record_count");
    assert!(
        record_count >= 8000,
        "reference corpus unexpectedly small: {record_count}"
    );

    let graph = source(&report, "graph");
    assert_eq!(graph["skipped"], false);
    assert_eq!(graph["pass"], true);
    let samples = graph["samples_ms"].as_array().expect("graph samples");
    assert_eq!(samples.len(), 11, "default sample count");
    let p50 = graph["p50_ms"].as_f64().expect("graph p50");
    assert!(p50 > 0.0, "p50 should be a real measurement");
    assert!(
        p50 <= 2000.0,
        "graph p50 {p50}ms exceeds the 2s budget on the reference corpus"
    );
    assert!(graph["p95_ms"].as_f64().expect("graph p95") >= p50);

    // The embedded source is measured when the feature is enabled and
    // explicitly skipped (never silently dropped) when it is not.
    let data_dir = source(&report, "data_dir");
    #[cfg(feature = "embedded-aletheiadb")]
    assert!(
        !data_dir["skipped"].as_bool().unwrap_or(false),
        "data_dir skipped with the embedded feature enabled"
    );
    #[cfg(not(feature = "embedded-aletheiadb"))]
    assert!(
        data_dir["skipped"].as_bool().unwrap_or(false),
        "data_dir measured without the embedded feature"
    );
    if !data_dir["skipped"].as_bool().unwrap_or(false) {
        assert_eq!(data_dir["pass"], true);
        let p50 = data_dir["p50_ms"].as_f64().expect("data_dir p50");
        assert!(
            p50 <= 2000.0,
            "data_dir p50 {p50}ms exceeds the 2s budget on the reference corpus"
        );
    }
}

// The gate must actually fail when the budget is exceeded: a 1ms budget is
// unmeetable, so this proves the fail path (exit 1, ok=false) works and the
// gate is not a rubber stamp. Graph-only with one sample keeps it cheap.
#[test]
fn gate_fails_when_budget_exceeded() {
    let (code, report) = run_gate(&[
        "--source",
        "graph",
        "--samples",
        "1",
        "--budget-p50-ms",
        "1",
    ]);
    assert_eq!(code, 1, "gate should fail on a 1ms budget: {report}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["sources"]["graph"]["pass"], false);
}

// A budget override of zero (or negative) would silently disable the gate;
// it must be rejected as a usage error before any measurement runs. The
// rejection is a JSON error envelope on stderr (the `query_latency_exit`
// contract shared by all pre-measurement failures, matching the codebase's
// other audit usage errors), not a benchmark report on stdout.
#[test]
fn non_positive_budget_is_rejected() {
    let output = egregore()
        .args(["audit", "query-latency", "--corpus"])
        .arg(manifest_path())
        .args(["--budget-p50-ms", "0"])
        .output()
        .expect("command should run");
    assert_eq!(
        output.status.code(),
        Some(2),
        "zero budget should be a usage error"
    );
    let envelope: Value =
        serde_json::from_slice(&output.stderr).expect("error envelope should be valid JSON");
    assert_eq!(envelope["code"], "invalid_budget_p50_ms");
}

// The report carries everything needed to interpret the measurement without
// re-running it: corpus identity, machine context, per-source percentiles.
// Graph-only with one sample keeps it cheap; the schema is identical for
// every source.
#[test]
fn report_schema_is_complete() {
    let (code, report) = run_gate(&["--source", "graph", "--samples", "1"]);
    assert_eq!(code, 0);
    for key in [
        "corpus_name",
        "corpus_version",
        "query",
        "record_count",
        "reference_record_count",
        "reference_machine_class",
        "machine",
        "samples",
        "budget_p50_ms",
        "sources",
        "ok",
    ] {
        assert!(!report[key].is_null(), "report missing required key: {key}");
    }
    for key in ["os", "arch", "parallelism"] {
        assert!(
            !report["machine"][key].is_null(),
            "report.machine missing required key: {key}"
        );
    }
    for key in [
        "p50_ms",
        "p95_ms",
        "min_ms",
        "max_ms",
        "budget_p50_ms",
        "pass",
    ] {
        assert!(
            !report["sources"]["graph"][key].is_null(),
            "report.sources.graph missing required key: {key}"
        );
    }
}
