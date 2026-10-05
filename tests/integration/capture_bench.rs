//! SPEC-PROOF-RED-GREEN tests for `eg capture-bench` / `eg query bench`
//! (issue #237).
//!
//! Capturing criterion benchmark runs as citable, deterministic
//! verification-domain `BenchmarkRun` records, with a baseline-delta verdict
//! and a query lane that returns the latest run for a benchmark id / symbol /
//! file with a citable handle. CAPTURE-ONLY: no benchmark is ever executed
//! here — the fixtures are pre-captured `target/criterion`-shaped estimate
//! trees.

#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use assert_cmd::Command;
use serde_json::Value;

use aletheia_egregore::{
    bench_capture::{
        BENCH_NOISE_THRESHOLD_PCT, BENCH_SYMBOL_AMBIGUOUS_CODE, BENCH_SYMBOL_UNRESOLVED_CODE,
        BenchCaptureError, BenchmarkRunRequest, BenchmarkVerdict, NORMALIZED_SUMMARY_FORMAT,
        bench_error_diagnostic, benchmark_verdict, build_bench_run_records, parse_benchmark_inputs,
        scan_criterion_dir,
    },
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind},
};

// ── Fixtures ────────────────────────────────────────────────────────────────

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/capture_bench")
}

fn with_baseline() -> PathBuf {
    fixtures().join("criterion-with-baseline")
}

fn no_baseline() -> PathBuf {
    fixtures().join("criterion-no-baseline")
}

fn malformed() -> PathBuf {
    fixtures().join("criterion-malformed")
}

const fn sample_request<'a>(suite: &'a str, executed_at: &'a str) -> BenchmarkRunRequest<'a> {
    BenchmarkRunRequest {
        session_id: "sess-42",
        commit: "abc123def456",
        suite,
        command: Some("cargo bench -p mini_crate"),
        executed_at,
    }
}

/// Minimal in-memory code graph: `parse_scan` resolves to exactly one symbol,
/// `render_row` is ambiguous (two symbols), `encode_frame` / `hash_key` are
/// unresolved. Mirrors the capture-tests fixture style.
fn code_graph() -> Vec<GraphRecord> {
    let file = GraphRecord::node(
        "codegraph:v6:file-mini".to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "file src/lib.rs".to_owned(),
    );
    let sym = |id: &str, name: &str| {
        let mut r = GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some(name.to_owned()),
            format!("fn {name}"),
        );
        if let GraphRecord::Node { symbol_kind, .. } = &mut r {
            *symbol_kind = Some("function".to_owned());
        }
        r
    };
    vec![
        file,
        sym("codegraph:v6:sym-parse_scan", "parse_scan"),
        sym("codegraph:v6:sym-render_row-a", "render_row"),
        sym("codegraph:v6:sym-render_row-b", "render_row"),
    ]
}

fn bench_runs(records: &[GraphRecord]) -> Vec<&GraphRecord> {
    records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::BenchmarkRun,
                    ..
                }
            )
        })
        .collect()
}

// ── Verdict logic ───────────────────────────────────────────────────────────

#[test]
fn verdict_boundaries() {
    assert!(
        (BENCH_NOISE_THRESHOLD_PCT - 1.0).abs() < f64::EPSILON,
        "noise threshold should be 1.0, got {BENCH_NOISE_THRESHOLD_PCT}"
    );
    let (v, d) = benchmark_verdict(1250.0, Some(1000.0));
    assert_eq!(v, BenchmarkVerdict::Regression);
    assert_eq!(d, Some(25.0));

    let (v, d) = benchmark_verdict(900.0, Some(1000.0));
    assert_eq!(v, BenchmarkVerdict::Improvement);
    assert_eq!(d, Some(-10.0));

    // |delta| at or under the noise floor is unchanged, never a silent default.
    let (v, d) = benchmark_verdict(1005.0, Some(1000.0));
    assert_eq!(v, BenchmarkVerdict::Unchanged);
    assert_eq!(d, Some(0.5));
    let (v, _) = benchmark_verdict(1010.0, Some(1000.0));
    assert_eq!(v, BenchmarkVerdict::Unchanged);

    // No baseline is an explicit verdict, not a silent `unchanged`.
    let (v, d) = benchmark_verdict(500.0, None);
    assert_eq!(v, BenchmarkVerdict::NoBaseline);
    assert_eq!(d, None);

    // A degenerate (non-positive) baseline cannot define a delta.
    let (v, d) = benchmark_verdict(1000.0, Some(0.0));
    assert_eq!(v, BenchmarkVerdict::Unchanged);
    assert_eq!(d, None);

    assert_eq!(BenchmarkVerdict::Regression.as_str(), "regression");
    assert_eq!(BenchmarkVerdict::Improvement.as_str(), "improvement");
    assert_eq!(BenchmarkVerdict::Unchanged.as_str(), "unchanged");
    assert_eq!(BenchmarkVerdict::NoBaseline.as_str(), "no_baseline");
}

// ── Scanning + parsing ──────────────────────────────────────────────────────

#[test]
fn scan_finds_benchmarks_sorted_with_basenames() {
    let inputs = scan_criterion_dir(&with_baseline()).expect("fixture should scan");
    let ids: Vec<&str> = inputs.iter().map(|i| i.benchmark_id.as_str()).collect();
    assert_eq!(ids, vec!["encode_frame", "parse_scan", "render_row"]);
    assert!(
        inputs.iter().all(|i| i.base_estimates_bytes.is_some()),
        "every with-baseline benchmark should carry a base"
    );
}

#[test]
fn scan_no_baseline_fixture() {
    let inputs = scan_criterion_dir(&no_baseline()).expect("fixture should scan");
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].benchmark_id, "hash_key");
    assert!(inputs[0].base_estimates_bytes.is_none());
}

#[test]
fn scan_empty_dir_is_empty_error() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let err = scan_criterion_dir(temp.path()).expect_err("empty dir must fail");
    assert_eq!(err, BenchCaptureError::Empty);
    assert_eq!(err.code(), "empty_bench_output");
}

#[test]
fn scan_missing_dir_is_empty_error() {
    let err =
        scan_criterion_dir(&fixtures().join("does-not-exist")).expect_err("missing dir must fail");
    assert_eq!(err, BenchCaptureError::Empty);
}

#[test]
fn parse_estimates_carries_measurement_and_verdict() {
    let inputs = scan_criterion_dir(&with_baseline()).expect("fixture should scan");
    let parse = parse_benchmark_inputs(&inputs).expect("fixture should parse");
    let benches = parse.benchmarks();
    assert_eq!(benches.len(), 3);

    let parse_scan = benches
        .iter()
        .find(|b| b.id == "parse_scan")
        .expect("parse_scan should be parsed");
    assert!(
        (parse_scan.mean_ns - 1250.0).abs() < f64::EPSILON,
        "mean_ns should be 1250.0, got {}",
        parse_scan.mean_ns
    );
    assert_eq!(parse_scan.median_ns, Some(1242.0));
    assert_eq!(parse_scan.ci_lower_ns, Some(1235.2));
    assert_eq!(parse_scan.ci_upper_ns, Some(1268.7));
    assert_eq!(parse_scan.verdict, BenchmarkVerdict::Regression);
    assert_eq!(parse_scan.delta_pct, Some(25.0));
    assert!(!parse_scan.new_artifact_hash.is_empty());
    assert!(parse_scan.base_estimates_path.is_some());

    let render_row = benches
        .iter()
        .find(|b| b.id == "render_row")
        .expect("render_row should be parsed");
    assert_eq!(render_row.verdict, BenchmarkVerdict::Improvement);
    assert_eq!(render_row.delta_pct, Some(-10.0));

    let encode_frame = benches
        .iter()
        .find(|b| b.id == "encode_frame")
        .expect("encode_frame should be parsed");
    assert_eq!(encode_frame.verdict, BenchmarkVerdict::Unchanged);
    assert_eq!(encode_frame.delta_pct, Some(0.5));
}

#[test]
fn parse_no_baseline_fixture_yields_no_baseline_verdict() {
    let inputs = scan_criterion_dir(&no_baseline()).expect("fixture should scan");
    let parse = parse_benchmark_inputs(&inputs).expect("fixture should parse");
    let bench = &parse.benchmarks()[0];
    assert_eq!(bench.verdict, BenchmarkVerdict::NoBaseline);
    assert_eq!(bench.delta_pct, None);
}

#[test]
fn parse_malformed_estimates_is_unparseable() {
    let inputs = scan_criterion_dir(&malformed()).expect("malformed dir still scans");
    let err = parse_benchmark_inputs(&inputs).expect_err("bad JSON must fail");
    assert_eq!(err, BenchCaptureError::Unparseable);
    assert_eq!(err.code(), "unparseable_bench_output");
}

#[test]
fn parse_missing_mean_is_unparseable() {
    use aletheia_egregore::bench_capture::BenchmarkInput;
    let inputs = vec![BenchmarkInput {
        benchmark_id: "median_only".to_owned(),
        new_estimates_bytes: br#"{"median": {"point_estimate": 5.0}}"#.to_vec(),
        new_estimates_path: "median_only/new/estimates.json".to_owned(),
        base_estimates_bytes: None,
        base_estimates_path: None,
    }];
    let err = parse_benchmark_inputs(&inputs).expect_err("missing mean must fail");
    assert_eq!(err, BenchCaptureError::Unparseable);
}

// ── Record building ─────────────────────────────────────────────────────────

fn outcome_no_graph() -> aletheia_egregore::bench_capture::BenchRunOutcome {
    let inputs = scan_criterion_dir(&with_baseline()).expect("fixture should scan");
    let parse = parse_benchmark_inputs(&inputs).expect("fixture should parse");
    build_bench_run_records(
        &sample_request("benches", "2026-07-19T12:00:00Z"),
        &parse,
        None,
    )
    .expect("fixture should build")
}

#[test]
fn build_emits_one_benchmark_run_per_benchmark() {
    let outcome = outcome_no_graph();
    let runs = bench_runs(&outcome.records);
    assert_eq!(runs.len(), 3, "one BenchmarkRun per parsed benchmark");

    for run in runs {
        let GraphRecord::Node {
            id,
            verification_kind,
            domain,
            schema_version,
            summary,
            executed_at,
            source_artifact_path,
            source_artifact_hash,
            stdout_handle,
            ..
        } = run
        else {
            panic!("expected node records");
        };
        assert_eq!(verification_kind.as_deref(), Some("benchmark_run"));
        assert_eq!(domain.as_deref(), Some("verification"));
        assert_eq!(*schema_version, 1);
        assert!(!summary.is_empty());
        assert_eq!(executed_at.as_deref(), Some("2026-07-19T12:00:00Z"));
        assert!(
            source_artifact_path
                .as_deref()
                .is_some_and(|p| p.ends_with("new/estimates.json")),
            "source_artifact_path must point at the raw criterion artifact"
        );
        assert!(
            !source_artifact_hash
                .as_deref()
                .unwrap_or_default()
                .is_empty()
        );
        let handle = stdout_handle.as_ref().expect("stdout_handle required");
        let inline = handle.inline.as_ref().expect("summary must fit inline");
        let summary_json: Value =
            serde_json::from_str(inline).expect("normalized summary must be JSON");
        assert_eq!(
            summary_json.get("format").and_then(Value::as_str),
            Some(NORMALIZED_SUMMARY_FORMAT)
        );
        assert_eq!(
            summary_json
                .get("measurement_of_record")
                .and_then(Value::as_str),
            Some("mean")
        );
        assert_eq!(summary_json.get("unit").and_then(Value::as_str), Some("ns"));
        assert!(
            summary_json
                .get("mean_ns")
                .and_then(Value::as_f64)
                .is_some(),
            "central estimate must be present"
        );
        assert!(
            summary_json
                .get("verdict")
                .and_then(Value::as_str)
                .is_some(),
            "verdict must be present"
        );
        // Evidence-handle rule: at least one handle present (all three are).
        assert!(!handle.hash.is_empty());
        let _ = id;
    }

    assert_eq!(
        outcome.benchmark_ids,
        vec!["encode_frame", "parse_scan", "render_row"]
    );
    assert_eq!(outcome.record_ids.len(), 3);
    // Stable-id tuple: ["benchmark_run", session_id, commit_sha, suite_name,
    // benchmark_id] — the schema's 4-tuple extended with the benchmark id so
    // one record per benchmark is addressable (documented in the schema doc).
    let expected = aletheia_egregore::ir::verification_stable_id(&[
        "benchmark_run",
        "sess-42",
        "abc123def456",
        "benches",
        "parse_scan",
    ]);
    assert!(
        outcome.record_ids.contains(&expected),
        "record ids must use the extended benchmark_run stable-id tuple"
    );
}

#[test]
fn build_is_deterministic_and_idempotent() {
    // Same input files produce byte-for-byte identical records: re-capturing
    // the same criterion output converges to a no-op (issue #130).
    let inputs = scan_criterion_dir(&with_baseline()).expect("fixture should scan");
    let parse = parse_benchmark_inputs(&inputs).expect("fixture should parse");
    let req = sample_request("benches", "2026-07-19T12:00:00Z");
    let first = build_bench_run_records(&req, &parse, None).expect("fixture should build");
    let second = build_bench_run_records(&req, &parse, None).expect("fixture should build");

    let mut g1 = Graph::new();
    for r in &first.records {
        g1.push(r.clone());
    }
    let mut g2 = Graph::new();
    for r in &second.records {
        g2.push(r.clone());
    }
    let j1 = g1.to_jsonl().expect("jsonl should serialize");
    let j2 = g2.to_jsonl().expect("jsonl should serialize");
    assert_eq!(j1, j2, "repeated capture must be byte-identical");
}

#[test]
fn symbol_resolution_mints_edges_or_diagnostics_never_silently() {
    let inputs = scan_criterion_dir(&with_baseline()).expect("fixture should scan");
    let parse = parse_benchmark_inputs(&inputs).expect("fixture should parse");
    let graph = code_graph();
    let outcome = build_bench_run_records(
        &sample_request("benches", "2026-07-19T12:00:00Z"),
        &parse,
        Some(&graph),
    )
    .expect("fixture should build");

    assert_eq!(outcome.resolved_count, 1);
    assert_eq!(outcome.unresolved_count, 1); // encode_frame
    assert_eq!(outcome.ambiguous_count, 1); // render_row
    // The envelope's `unresolved` section lists every benchmark with no
    // anchoring edge — unresolved and ambiguous alike, never silently
    // dropped; the per-reason counts above distinguish them.
    assert_eq!(
        outcome.unresolved,
        vec!["encode_frame".to_owned(), "render_row".to_owned()]
    );

    let parse_scan_id = outcome
        .record_ids
        .iter()
        .zip(outcome.benchmark_ids.iter())
        .find(|(_, b)| **b == "parse_scan")
        .map(|(id, _)| id.clone())
        .expect("parse_scan record id");

    let mentions = outcome
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::MentionsSymbol,
                    source,
                    target,
                    ..
                } if source == &parse_scan_id && target == "codegraph:v6:sym-parse_scan"
            )
        })
        .count();
    assert_eq!(mentions, 1, "exactly one MENTIONS_SYMBOL edge");

    let touched = outcome
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::TouchedFile,
                    source,
                    target,
                    ..
                } if source == &parse_scan_id && target == "codegraph:v6:file-mini"
            )
        })
        .count();
    assert_eq!(touched, 1, "exactly one TOUCHED_FILE edge");

    let diagnostics: Vec<&GraphRecord> = outcome
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Diagnostic,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        diagnostics.len(),
        2,
        "one diagnostic per unresolved/ambiguous link"
    );
    let codes: Vec<&str> = diagnostics
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { symbol_kind, .. } => symbol_kind.as_deref(),
            _ => None,
        })
        .collect();
    assert!(codes.contains(&BENCH_SYMBOL_UNRESOLVED_CODE));
    assert!(codes.contains(&BENCH_SYMBOL_AMBIGUOUS_CODE));
}

#[test]
fn error_diagnostic_is_the_only_record_on_error_paths() {
    for (error, code) in [
        (BenchCaptureError::Empty, "empty_bench_output"),
        (BenchCaptureError::Unparseable, "unparseable_bench_output"),
    ] {
        let diagnostic =
            bench_error_diagnostic("sess-42", "abc123", "benches", "target/criterion", error);
        let GraphRecord::Node {
            kind, symbol_kind, ..
        } = &diagnostic
        else {
            panic!("expected a node");
        };
        assert_eq!(*kind, NodeKind::Diagnostic);
        assert_eq!(symbol_kind.as_deref(), Some(code));
    }
}

// ── CLI: capture-bench ──────────────────────────────────────────────────────

fn eg() -> Command {
    Command::cargo_bin("egregore").expect("egregore binary should build")
}

#[test]
fn cli_capture_bench_roundtrip_with_unresolved_section() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let out = temp.path().join("bench.jsonl");

    let assert = eg()
        .arg("capture-bench")
        .arg("--input")
        .arg(with_baseline())
        .arg("--out")
        .arg(&out)
        .arg("--session-id")
        .arg("sess-42")
        .arg("--commit")
        .arg("abc123def456")
        .arg("--suite")
        .arg("benches")
        .arg("--executed-at")
        .arg("2026-07-19T12:00:00Z")
        .assert()
        .success();
    let envelope: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("envelope must be JSON");
    assert_eq!(envelope.get("ok"), Some(&Value::Bool(true)));
    assert_eq!(envelope.get("records").and_then(Value::as_u64), Some(3));
    let benchmarks: Vec<&str> = envelope
        .get("benchmarks")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert_eq!(benchmarks, vec!["encode_frame", "parse_scan", "render_row"]);
    // The `unresolved` section reports benchmark→symbol links that could not
    // be anchored — never dropped silently (mirrors `eg query task`).
    let unresolved = envelope
        .get("unresolved")
        .and_then(Value::as_array)
        .expect("unresolved section must be present");
    assert_eq!(unresolved.len(), 3, "no --graph: nothing resolves");

    let jsonl = fs::read_to_string(&out).expect("output jsonl should exist");
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        let record: Value = serde_json::from_str(line).expect("line must be JSON");
        assert_eq!(
            record.get("verification_kind").and_then(Value::as_str),
            Some("benchmark_run")
        );
    }

    // Idempotency: re-capturing the same criterion output is a byte-for-byte
    // no-op on the output file (converges per issue #130).
    let out2 = temp.path().join("bench2.jsonl");
    eg().arg("capture-bench")
        .arg("--input")
        .arg(with_baseline())
        .arg("--out")
        .arg(&out2)
        .arg("--session-id")
        .arg("sess-42")
        .arg("--commit")
        .arg("abc123def456")
        .arg("--suite")
        .arg("benches")
        .arg("--executed-at")
        .arg("2026-07-19T12:00:00Z")
        .assert()
        .success();
    let first = fs::read(&out).expect("first capture");
    let second = fs::read(&out2).expect("second capture");
    assert_eq!(first, second, "repeated capture must be byte-identical");
}

#[test]
fn cli_capture_bench_empty_dir_exits_4() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let out = temp.path().join("bench.jsonl");
    eg().arg("capture-bench")
        .arg("--input")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--session-id")
        .arg("sess-42")
        .arg("--commit")
        .arg("abc123def456")
        .arg("--suite")
        .arg("benches")
        .arg("--executed-at")
        .arg("2026-07-19T12:00:00Z")
        .assert()
        .code(4);
}

#[test]
fn cli_capture_bench_malformed_exits_5() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let out = temp.path().join("bench.jsonl");
    eg().arg("capture-bench")
        .arg("--input")
        .arg(malformed())
        .arg("--out")
        .arg(&out)
        .arg("--session-id")
        .arg("sess-42")
        .arg("--commit")
        .arg("abc123def456")
        .arg("--suite")
        .arg("benches")
        .arg("--executed-at")
        .arg("2026-07-19T12:00:00Z")
        .assert()
        .code(5);
}

#[test]
fn cli_capture_bench_rejects_bad_executed_at() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let out = temp.path().join("bench.jsonl");
    eg().arg("capture-bench")
        .arg("--input")
        .arg(with_baseline())
        .arg("--out")
        .arg(&out)
        .arg("--session-id")
        .arg("sess-42")
        .arg("--commit")
        .arg("abc123def456")
        .arg("--suite")
        .arg("benches")
        .arg("--executed-at")
        .arg("not-a-timestamp")
        .assert()
        .code(1);
}

// ── CLI: query bench ────────────────────────────────────────────────────────

/// Capture the with-baseline fixture twice (two commits / instants) and union
/// with the code graph, returning the combined JSONL path.
fn captured_history(dir: &std::path::Path) -> PathBuf {
    let first = dir.join("run1.jsonl");
    let second = dir.join("run2.jsonl");
    for (out, commit, at) in [
        (&first, "aaa111", "2026-07-19T12:00:00Z"),
        (&second, "bbb222", "2026-07-20T12:00:00Z"),
    ] {
        eg().arg("capture-bench")
            .arg("--input")
            .arg(with_baseline())
            .arg("--out")
            .arg(out)
            .arg("--session-id")
            .arg("sess-42")
            .arg("--commit")
            .arg(commit)
            .arg("--suite")
            .arg("benches")
            .arg("--executed-at")
            .arg(at)
            .arg("--graph")
            .arg(code_graph_jsonl(dir))
            .assert()
            .success();
    }
    // Union the two runs plus the code graph into one store file for the
    // query lane (the query test asserts the anchored symbol metadata).
    let mut combined = String::new();
    combined.push_str(&fs::read_to_string(&first).expect("run1 jsonl"));
    combined.push_str(&fs::read_to_string(&second).expect("run2 jsonl"));
    combined.push_str(&fs::read_to_string(code_graph_jsonl(dir)).expect("code graph jsonl"));
    let path = dir.join("history.jsonl");
    fs::write(&path, combined).expect("history jsonl");
    path
}

fn code_graph_jsonl(dir: &std::path::Path) -> PathBuf {
    let mut graph = Graph::new();
    for r in code_graph() {
        graph.push(r);
    }
    let path = dir.join("code.jsonl");
    fs::write(&path, graph.to_jsonl().expect("code graph jsonl")).expect("code graph file");
    path
}

#[test]
fn cli_query_bench_returns_latest_with_citable_handle() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let history = captured_history(temp.path());

    let assert = eg()
        .arg("query")
        .arg("bench")
        .arg("parse_scan")
        .arg("--graph")
        .arg(&history)
        .assert()
        .success();
    let answer: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("answer must be JSON");
    assert_eq!(answer.get("ok"), Some(&Value::Bool(true)));
    assert_eq!(
        answer.get("benchmark_id").and_then(Value::as_str),
        Some("parse_scan")
    );
    // Citable handle: record_id + commit + artifact path.
    assert!(
        answer
            .get("record_id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "record_id must be present"
    );
    assert_eq!(answer.get("commit").and_then(Value::as_str), Some("bbb222"));
    assert!(
        answer
            .get("artifact_path")
            .and_then(Value::as_str)
            .is_some_and(|p| p.ends_with("new/estimates.json")),
        "artifact_path must point at the raw criterion artifact"
    );
    assert_eq!(
        answer.get("verdict").and_then(Value::as_str),
        Some("regression")
    );
    assert_eq!(
        answer.get("estimate_ns").and_then(Value::as_f64),
        Some(1250.0)
    );
    assert_eq!(answer.get("unit").and_then(Value::as_str), Some("ns"));
    // Symbol linkage resolved through the unioned code graph.
    let symbol = answer.get("symbol").expect("symbol linkage expected");
    assert_eq!(
        symbol.get("name").and_then(Value::as_str),
        Some("parse_scan")
    );
}

#[test]
fn cli_query_bench_temporal_selectors() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let history = captured_history(temp.path());

    // --as-of pins to the earlier run.
    let assert = eg()
        .arg("query")
        .arg("bench")
        .arg("parse_scan")
        .arg("--graph")
        .arg(&history)
        .arg("--as-of")
        .arg("2026-07-19T12:00:00Z")
        .assert()
        .success();
    let answer: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("answer must be JSON");
    assert_eq!(answer.get("commit").and_then(Value::as_str), Some("aaa111"));

    // --at pins by commit prefix.
    let assert = eg()
        .arg("query")
        .arg("bench")
        .arg("parse_scan")
        .arg("--graph")
        .arg(&history)
        .arg("--at")
        .arg("aaa")
        .assert()
        .success();
    let answer: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("answer must be JSON");
    assert_eq!(answer.get("commit").and_then(Value::as_str), Some("aaa111"));
}

#[test]
fn cli_query_bench_by_symbol_and_file_target() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let history = captured_history(temp.path());

    // Symbol-name target resolves through the MENTIONS_SYMBOL edge.
    let assert = eg()
        .arg("query")
        .arg("bench")
        .arg("parse_scan")
        .arg("--graph")
        .arg(&history)
        .assert()
        .success();
    let by_symbol: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("answer must be JSON");
    assert_eq!(by_symbol.get("ok"), Some(&Value::Bool(true)));

    // Benchmark-id target works even when no symbol resolves.
    let assert = eg()
        .arg("query")
        .arg("bench")
        .arg("encode_frame")
        .arg("--graph")
        .arg(&history)
        .assert()
        .success();
    let by_id: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("answer must be JSON");
    assert_eq!(by_id.get("ok"), Some(&Value::Bool(true)));
    assert_eq!(
        by_id.get("benchmark_id").and_then(Value::as_str),
        Some("encode_frame")
    );
    assert_eq!(
        by_id.get("verdict").and_then(Value::as_str),
        Some("unchanged")
    );
    assert!(by_id.get("symbol").is_none());
}

#[test]
fn cli_query_bench_no_match_is_exit_2_envelope() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let history = captured_history(temp.path());

    let assert = eg()
        .arg("query")
        .arg("bench")
        .arg("does_not_exist")
        .arg("--graph")
        .arg(&history)
        .assert()
        .code(2);
    let envelope: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("envelope must be JSON");
    assert_eq!(envelope.get("ok"), Some(&Value::Bool(false)));
    assert_eq!(
        envelope
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str),
        Some("no_match")
    );
}

#[test]
fn cli_query_bench_empty_store_is_exit_2_envelope() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let empty = temp.path().join("empty.jsonl");
    fs::write(&empty, "").expect("empty store file");

    eg().arg("query")
        .arg("bench")
        .arg("parse_scan")
        .arg("--graph")
        .arg(&empty)
        .assert()
        .code(2);
}
