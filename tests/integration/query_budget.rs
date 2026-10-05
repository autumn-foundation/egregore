//! End-to-end tests for `eg audit query-budget` — the structural query
//! latency budget + scaling regression gate (issue #120).
//!
//! Where issue #255 (`eg audit query-latency`) gates cold p50 for
//! `query symbol` alone, this gate covers the agent-critical structural path
//! (`query symbol`, `query file`, `query drift`) with cold AND warm
//! invocations, records a ripgrep baseline for the equivalent symbol lookup,
//! and enforces the scaling assertion: a targeted single-symbol lookup must
//! grow sub-linearly with store size (10x the records costs <= 2x the
//! latency). The absolute p95 budget is advisory only — it is reported but
//! never fails the gate, so hardware variance cannot red CI.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/query_budget_corpus.json")
}

/// Runs the budget gate and returns `(exit_code, parsed report)`.
///
/// The full benchmark (10 corpus scans for the 10x store plus cold+warm
/// samples for three queries) is expensive, so the committed tests use cheap
/// profiles: the pass test uses `--samples 2 --warm-samples 1` (enough for a
/// real p50/p95), while the fail/advisory/schema tests use a single sample
/// per cell — they exercise gate logic and report shape, not percentile
/// precision. The dedicated CI job runs the full manifest-default profile.
fn run_gate_with_profile(profile: &[&str], extra_args: &[&str]) -> (i32, Value) {
    let mut cmd = egregore();
    cmd.args(["audit", "query-budget", "--corpus"])
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
    (code, report)
}

fn run_gate(extra_args: &[&str]) -> (i32, Value) {
    run_gate_with_profile(&["--samples", "2", "--warm-samples", "1"], extra_args)
}

fn run_gate_quick(extra_args: &[&str]) -> (i32, Value) {
    run_gate_with_profile(&["--samples", "1", "--warm-samples", "1"], extra_args)
}

// The scaling gate passes on the pinned reference corpus: the 10x store
// holds ~10x the records of the 1x store, every query x temp cell reports
// p50/p95, the ripgrep baseline is present or explicitly skipped, and the
// cold symbol scaling ratio sits under the 2.0 ceiling.
//
// TRACKED RED (issue #120): this test is `#[ignore]`d because the scaling
// assertion is violated on current trunk — measured 9.7-10.1x vs the 2.0x
// ceiling (2026-09-30, Fly Sprite). The query path loads the whole store per
// invocation, so time-to-first-answer scales linearly with record count;
// the optimization slice (lazy load / indexing / caching — explicitly out
// of scope for #120) must turn this green. Run with
// `cargo test -- --ignored` to observe the RED. Do NOT delete this test and
// do NOT raise the ceiling to fit the measurement: it is the regression
// guard's end-state. The dedicated `query-budget` CI job runs the live gate
// (not ignored) and is expected-red until the optimization slice lands.
#[test]
#[ignore = "tracked RED: scaling assertion violated on trunk (~10x vs 2.0x ceiling); pending the optimization slice"]
fn scaling_gate_passes_on_reference_corpus() {
    let (code, report) = run_gate(&[]);
    assert_eq!(code, 0, "gate should pass: {report}");
    assert_eq!(report["ok"], true);
    assert_eq!(report["corpus_name"], "query-budget-reference");

    let single_count = report["record_count_1x"].as_u64().expect("record_count_1x");
    let scaled_count = report["record_count_10x"]
        .as_u64()
        .expect("record_count_10x");
    assert!(
        single_count >= 8000,
        "1x store unexpectedly small: {single_count} records"
    );
    // Integer math: the 10x store should hold ~10x the records of the 1x
    // store (9.5x..=10.5x), without lossy u64->f64 casts.
    assert!(
        scaled_count * 2 >= single_count * 19 && scaled_count * 2 <= single_count * 21,
        "10x store should hold ~10x the records, got {scaled_count} vs {single_count}"
    );

    // Every query x temperature cell on both store sizes reports p50/p95.
    for query in ["symbol", "file", "drift"] {
        for temp in ["cold", "warm"] {
            for size in ["store_1x", "store_10x"] {
                let cell = &report["queries"][query][temp][size];
                assert!(
                    cell["p50_ms"].as_f64().unwrap_or(0.0) > 0.0,
                    "missing p50 for {query}/{temp}/{size}: {report}"
                );
                assert!(
                    cell["p95_ms"].as_f64().unwrap_or(0.0) >= cell["p50_ms"].as_f64().unwrap(),
                    "p95 < p50 for {query}/{temp}/{size}: {report}"
                );
            }
        }
    }

    // The ripgrep baseline for the equivalent symbol lookup is recorded, or
    // explicitly skipped with a reason — never silently absent.
    let rg = &report["ripgrep"];
    if rg["skipped"].as_bool().unwrap_or(false) {
        assert!(
            !rg["skip_reason"].as_str().unwrap_or("").is_empty(),
            "ripgrep skipped without a reason: {report}"
        );
    } else {
        assert!(
            rg["p50_ms"].as_f64().unwrap_or(0.0) > 0.0,
            "ripgrep baseline missing p50: {report}"
        );
    }

    // The scaling assertion itself: 10x the store costs <= 2x the latency.
    let scaling = &report["scaling"];
    assert_eq!(scaling["query"], "symbol");
    assert_eq!(scaling["temp"], "cold");
    let latency_ratio = scaling["ratio_p50"].as_f64().expect("ratio_p50");
    assert!(
        latency_ratio <= 2.0,
        "scaling violated: 10x store costs {latency_ratio}x the 1x latency"
    );
    assert_eq!(scaling["pass"], true);
}

// The gate must actually bite: an unmeetable --max-ratio proves the scaling
// assertion fails the gate (exit 1, ok=false) instead of rubber-stamping.
#[test]
fn scaling_gate_fails_when_ratio_unmeetable() {
    let (code, report) = run_gate_quick(&["--max-ratio", "0.000001"]);
    assert_eq!(code, 1, "gate should fail on an unmeetable ratio: {report}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["scaling"]["pass"], false);
}

// The absolute p95 budget is advisory: a 1ms budget is unmeetable, but the
// gate still passes — the exceedance is reported, never gating.
// (--max-ratio 1000 isolates this from the tracked scaling RED: the scaling
// assertion is violated on current trunk, so the default 2.0x ceiling would
// fail the gate for reasons unrelated to the budget.)
#[test]
fn p95_budget_is_advisory_not_gating() {
    let (code, report) = run_gate_quick(&["--budget-p95-ms", "1", "--max-ratio", "1000"]);
    assert_eq!(
        code, 0,
        "advisory p95 budget must not fail the gate: {report}"
    );
    assert_eq!(report["ok"], true);
    let advisory = report["advisory_p95"].as_array().expect("advisory_p95");
    assert!(
        !advisory.is_empty(),
        "unmeetable p95 budget should produce advisory entries: {report}"
    );
    for entry in advisory {
        assert_eq!(entry["within_budget"], false);
        assert!(!entry["query"].as_str().unwrap_or("").is_empty());
    }
}

// The report carries everything needed to interpret the measurement without
// re-running it: corpus identity, machine context, per-cell percentiles,
// the ripgrep baseline, the scaling assertion, and the advisory budgets.
// The schema is complete whether the gate passes or fails — a violated
// scaling assertion (exit 1, the tracked RED on current trunk) still prints
// the full report.
#[test]
fn report_schema_is_complete() {
    let (code, report) = run_gate_quick(&[]);
    assert!(
        code == 0 || code == 1,
        "unexpected exit code {code}: {report}"
    );
    for key in [
        "corpus_name",
        "corpus_version",
        "record_count_1x",
        "record_count_10x",
        "scale_factor",
        "reference_record_count",
        "reference_machine_class",
        "machine",
        "samples",
        "warm_samples",
        "queries",
        "ripgrep",
        "scaling",
        "budgets",
        "advisory_p95",
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
    for key in ["ratio_p50", "max_ratio", "pass", "query", "temp"] {
        assert!(
            !report["scaling"][key].is_null(),
            "report.scaling missing required key: {key}"
        );
    }
    for key in [
        "p50_ms",
        "p95_ms",
        "budget_p50_ms",
        "budget_p95_ms",
        "command",
        "skipped",
    ] {
        assert!(
            !report["ripgrep"][key].is_null(),
            "report.ripgrep missing required key: {key}"
        );
    }
}

// Usage errors fail fast with the shared JSON error envelope on stderr
// (exit 2), never a half-printed report.
#[test]
fn invalid_args_are_rejected() {
    for args in [
        vec!["--samples", "0"],
        vec!["--warm-samples", "0"],
        vec!["--max-ratio", "0"],
        // Equals form: bare `--max-ratio -1` is intercepted by clap as an
        // unexpected argument before the validator runs.
        vec!["--max-ratio=-1"],
        vec!["--scale-factor", "1"],
        vec!["--budget-p95-ms", "0"],
    ] {
        let output = egregore()
            .args(["audit", "query-budget", "--corpus"])
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
