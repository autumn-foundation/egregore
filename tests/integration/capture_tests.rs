//! SPEC-PROOF-RED-GREEN tests for `eg capture-tests` (issue #165).
//!
//! Capturing a `cargo test` run as a citable, deterministic verification-domain
//! `TestRun` record. CAPTURE-ONLY: no test runner is ever executed here — the
//! fixtures are pre-captured libtest JSON event streams.

#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use assert_cmd::Command;
use serde_json::Value;

use aletheia_egregore::{
    ir::{EdgeLabel, GraphRecord, NodeKind},
    test_capture::{TestCaptureError, TestRunRequest, build_test_run_records, parse_libtest_json},
};

// ── Fixtures ────────────────────────────────────────────────────────────────

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/capture_tests")
}

fn read_fixture(name: &str) -> String {
    fs::read_to_string(fixtures().join(name)).expect("fixture should exist")
}

const fn sample_request<'a>(
    suite: &'a str,
    exit_code: i64,
    path: &'a str,
    hash: &'a str,
) -> TestRunRequest<'a> {
    TestRunRequest {
        session_id: "sess-42",
        commit: "abc123def456",
        suite,
        command: "cargo test -p mini_crate -- --format json",
        exit_code,
        executed_at: "2026-07-19T12:00:00Z",
        runner: Some("libtest"),
        runner_version: Some("1.94.1"),
        source_artifact_path: path,
        source_artifact_hash: hash,
    }
}

/// A minimal in-memory code graph: one Symbol `test_beta` in a file plus its
/// File node, so the failing-run resolution has an unambiguous target.
fn code_graph_with_test_beta() -> Vec<GraphRecord> {
    let file = GraphRecord::node(
        "codegraph:v6:file-mini".to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "file src/lib.rs".to_owned(),
    );
    let sym = GraphRecord::node(
        "codegraph:v6:sym-test-beta".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("test_beta".to_owned()),
        "symbol test_beta".to_owned(),
    );
    vec![file, sym]
}

fn test_run_node(records: &[GraphRecord]) -> &GraphRecord {
    records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::TestRun,
                    ..
                }
            )
        })
        .expect("a TestRun node should be emitted")
}

fn node_field_status(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node { status, .. } => status.clone(),
        _ => None,
    }
}

/// Parses the normalized JSON payload out of the `TestRun`'s `stdout_handle`.
fn normalized_payload(records: &[GraphRecord]) -> Value {
    let node = test_run_node(records);
    let inline = match node {
        GraphRecord::Node { stdout_handle, .. } => stdout_handle
            .as_ref()
            .expect("TestRun should carry a stdout_handle")
            .inline
            .clone()
            .expect("normalized summary should be inline (well under the ceiling)"),
        _ => unreachable!(),
    };
    serde_json::from_str(&inline).expect("normalized summary should be valid JSON")
}

fn outcome_for<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload["tests"]
        .as_array()
        .expect("tests array")
        .iter()
        .find(|t| t["name"] == Value::String(name.to_owned()))
        .and_then(|t| t["outcome"].as_str())
}

// ── 1. Passing run ──────────────────────────────────────────────────────────

#[test]
fn passing_run_yields_pass_testrun() {
    let parse = parse_libtest_json(&read_fixture("passing.json")).expect("parse ok");
    let req = sample_request("mini-unit", 0, "run.json", "deadbeefhash");
    let out = build_test_run_records(&req, &parse, None);

    let node = test_run_node(&out.records);
    // verification domain, test_run kind.
    match node {
        GraphRecord::Node {
            kind,
            domain,
            verification_kind,
            exit_code,
            source_artifact_hash,
            executed_at,
            ..
        } => {
            assert_eq!(*kind, NodeKind::TestRun);
            assert_eq!(domain.as_deref(), Some("verification"));
            assert_eq!(verification_kind.as_deref(), Some("test_run"));
            assert_eq!(*exit_code, Some(0));
            assert_eq!(source_artifact_hash.as_deref(), Some("deadbeefhash"));
            assert_eq!(executed_at.as_deref(), Some("2026-07-19T12:00:00Z"));
        }
        _ => panic!("expected node"),
    }
    assert_eq!(node_field_status(node).as_deref(), Some("pass"));

    let payload = normalized_payload(&out.records);
    assert_eq!(
        outcome_for(&payload, "mini_crate::test_alpha"),
        Some("pass")
    );
    assert_eq!(
        outcome_for(&payload, "mini_crate::test_gamma"),
        Some("pass")
    );
    assert_eq!(
        payload["command"],
        Value::String("cargo test -p mini_crate -- --format json".to_owned())
    );
}

// ── 2. Failing run: FAILED_ON edge, never CLOSES_ACCEPTANCE_CRITERION ────────

#[test]
fn failing_run_emits_failed_on_edge_and_never_closes_ac() {
    let parse = parse_libtest_json(&read_fixture("failing.json")).expect("parse ok");
    let req = sample_request("mini-unit", 101, "run.json", "hash2");
    let code_graph = code_graph_with_test_beta();
    let out = build_test_run_records(&req, &parse, Some(&code_graph));

    let node = test_run_node(&out.records);
    assert_eq!(node_field_status(node).as_deref(), Some("fail"));

    // Exactly one FAILED_ON edge, for the single reported failure, targeting the
    // resolved test_beta symbol.
    let failed_on: Vec<&GraphRecord> = out
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::FailedOn,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        failed_on.len(),
        1,
        "exactly one FAILED_ON per reported failure"
    );
    match failed_on[0] {
        GraphRecord::Edge { target, .. } => {
            assert_eq!(target, "codegraph:v6:sym-test-beta");
        }
        _ => unreachable!(),
    }

    // A failing run NEVER closes an acceptance criterion at this layer.
    assert!(
        !out.records.iter().any(|r| matches!(
            r,
            GraphRecord::Edge {
                label: EdgeLabel::ClosesAcceptanceCriterion,
                ..
            }
        )),
        "capture-tests must never emit CLOSES_ACCEPTANCE_CRITERION"
    );

    // The raw failing-test stdout (with its planted secret) must never appear.
    let jsonl = serde_json::to_string(&out.records).expect("serialize");
    assert!(
        !jsonl.contains("SHOULD_NOT_APPEAR_IN_GRAPH"),
        "raw failing-test stdout must never enter the graph"
    );
}

// ── 3. Mixed outcomes distinguishable on read-back ──────────────────────────

#[test]
fn mixed_outcomes_distinguishable_on_readback() {
    let parse = parse_libtest_json(&read_fixture("mixed.json")).expect("parse ok");
    let req = sample_request("mini-unit", 101, "run.json", "hash3");
    let out = build_test_run_records(&req, &parse, None);

    assert_eq!(
        node_field_status(test_run_node(&out.records)).as_deref(),
        Some("fail")
    );
    let payload = normalized_payload(&out.records);
    assert_eq!(
        outcome_for(&payload, "mini_crate::test_alpha"),
        Some("pass")
    );
    assert_eq!(outcome_for(&payload, "mini_crate::test_beta"), Some("fail"));
    assert_eq!(
        outcome_for(&payload, "mini_crate::test_delta"),
        Some("ignored")
    );
}

// ── 4. Ignored tests are ignored, not pass ──────────────────────────────────

#[test]
fn ignored_tests_captured_as_ignored_not_pass() {
    let parse = parse_libtest_json(&read_fixture("ignored.json")).expect("parse ok");
    let req = sample_request("mini-unit", 0, "run.json", "hash4");
    let out = build_test_run_records(&req, &parse, None);

    // No failures + suite ok → pass.
    assert_eq!(
        node_field_status(test_run_node(&out.records)).as_deref(),
        Some("pass")
    );
    let payload = normalized_payload(&out.records);
    assert_eq!(
        outcome_for(&payload, "mini_crate::test_delta"),
        Some("ignored")
    );
    assert_ne!(
        outcome_for(&payload, "mini_crate::test_delta"),
        Some("pass")
    );
}

// ── 5 & 6. Malformed / empty input → parse error (no TestRun) ────────────────

#[test]
fn malformed_input_is_unparseable() {
    let err = parse_libtest_json(&read_fixture("malformed.json")).unwrap_err();
    assert!(matches!(err, TestCaptureError::Unparseable));
}

#[test]
fn empty_input_is_empty_error() {
    let err = parse_libtest_json(&read_fixture("empty.json")).unwrap_err();
    assert!(matches!(err, TestCaptureError::Empty));
    // whitespace-only is also empty
    let err2 = parse_libtest_json("   \n  \n").unwrap_err();
    assert!(matches!(err2, TestCaptureError::Empty));
}

#[test]
fn prose_input_is_unparseable_never_a_testrun() {
    let err = parse_libtest_json("the quick brown fox\njumped over the lazy dog\n").unwrap_err();
    assert!(matches!(err, TestCaptureError::Unparseable));
}

// ── 9. Symbol anchoring requires --graph ────────────────────────────────────

#[test]
fn symbol_anchoring_without_graph_no_cross_domain_edges() {
    let parse = parse_libtest_json(&read_fixture("failing.json")).expect("parse ok");
    let req = sample_request("mini-unit", 101, "run.json", "hash5");
    let out = build_test_run_records(&req, &parse, None);

    // Only the self-contained TestRun node — no edges at all.
    assert!(
        !out.records
            .iter()
            .any(|r| matches!(r, GraphRecord::Edge { .. })),
        "no --graph: no cross-domain edges"
    );
    assert_eq!(
        out.records
            .iter()
            .filter(|r| matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::TestRun,
                    ..
                }
            ))
            .count(),
        1
    );
}

// ── 8. Trust separation: TestRun is verification, never agent-memory ────────

#[test]
fn trust_separation_verification_domain_only() {
    let parse = parse_libtest_json(&read_fixture("passing.json")).expect("parse ok");
    let req = sample_request("mini-unit", 0, "run.json", "hash6");
    let out = build_test_run_records(&req, &parse, None);

    let node = test_run_node(&out.records);
    match node {
        GraphRecord::Node {
            id, domain, kind, ..
        } => {
            assert_eq!(*kind, NodeKind::TestRun);
            assert_eq!(domain.as_deref(), Some("verification"));
            assert!(id.starts_with("verification:v1:"), "id was {id}");
            assert!(!id.starts_with("agent_memory:"));
        }
        _ => panic!("expected node"),
    }
    // No agent-memory records anywhere.
    assert!(
        !out.records
            .iter()
            .any(|r| r.id().starts_with("agent_memory:")),
        "capture-tests never mints agent-memory records"
    );
}

// ── CLI-level tests ─────────────────────────────────────────────────────────

fn base_cli_args<'a>(input: &'a str, out: &'a str) -> Vec<&'a str> {
    vec![
        "capture-tests",
        "--input",
        input,
        "--out",
        out,
        "--session-id",
        "sess-42",
        "--commit",
        "abc123def456",
        "--suite",
        "mini-unit",
        "--command",
        "cargo test -p mini_crate -- --format json",
        "--exit-code",
        "0",
        "--executed-at",
        "2026-07-19T12:00:00Z",
    ]
}

#[test]
fn cli_passing_run_writes_testrun_jsonl() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("testrun.graph.jsonl");
    let input = fixtures().join("passing.json");

    Command::cargo_bin("egregore")
        .expect("binary")
        .args(base_cli_args(
            input.to_str().unwrap(),
            out.to_str().unwrap(),
        ))
        .assert()
        .success();

    let jsonl = fs::read_to_string(&out).expect("output written");
    assert!(jsonl.contains(r#""kind":"TestRun""#));
    assert!(jsonl.contains(r#""domain":"verification""#));
    assert!(jsonl.contains("verification:v1:"));
}

#[test]
fn cli_empty_input_exits_4_with_diagnostic_no_testrun() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("testrun.graph.jsonl");
    let input = fixtures().join("empty.json");

    Command::cargo_bin("egregore")
        .expect("binary")
        .args(base_cli_args(
            input.to_str().unwrap(),
            out.to_str().unwrap(),
        ))
        .assert()
        .code(4);

    let jsonl = fs::read_to_string(&out).expect("diagnostic written");
    assert!(jsonl.contains(r#""kind":"Diagnostic""#));
    assert!(jsonl.contains("empty_test_output"));
    assert!(!jsonl.contains(r#""kind":"TestRun""#));
}

#[test]
fn cli_malformed_input_exits_5_with_diagnostic_no_testrun() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("testrun.graph.jsonl");
    let input = fixtures().join("malformed.json");

    Command::cargo_bin("egregore")
        .expect("binary")
        .args(base_cli_args(
            input.to_str().unwrap(),
            out.to_str().unwrap(),
        ))
        .assert()
        .code(5);

    let jsonl = fs::read_to_string(&out).expect("diagnostic written");
    assert!(jsonl.contains(r#""kind":"Diagnostic""#));
    assert!(jsonl.contains("unparseable_test_output"));
    assert!(!jsonl.contains(r#""kind":"TestRun""#));
}

#[test]
fn cli_bad_executed_at_exits_1() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("testrun.graph.jsonl");
    let input = fixtures().join("passing.json");
    let mut args = base_cli_args(input.to_str().unwrap(), out.to_str().unwrap());
    // Replace the executed-at value with a malformed one.
    let idx = args
        .iter()
        .position(|a| *a == "2026-07-19T12:00:00Z")
        .unwrap();
    args[idx] = "not-a-timestamp";

    Command::cargo_bin("egregore")
        .expect("binary")
        .args(args)
        .assert()
        .code(1)
        .stderr(predicates::str::contains("executed_at"));
}

#[test]
fn cli_unknown_format_exits_1() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("testrun.graph.jsonl");
    let input = fixtures().join("passing.json");
    let mut args = base_cli_args(input.to_str().unwrap(), out.to_str().unwrap());
    args.push("--format");
    args.push("junit-xml");

    Command::cargo_bin("egregore")
        .expect("binary")
        .args(args)
        .assert()
        .code(1);
}

#[test]
fn cli_determinism_five_runs_byte_identical() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixtures().join("passing.json");

    let mut outputs = Vec::new();
    for i in 0..5 {
        let out = temp.path().join(format!("run{i}.graph.jsonl"));
        Command::cargo_bin("egregore")
            .expect("binary")
            .args(base_cli_args(
                input.to_str().unwrap(),
                out.to_str().unwrap(),
            ))
            .assert()
            .success();
        outputs.push(fs::read(&out).expect("output"));
    }
    for i in 1..5 {
        assert_eq!(outputs[0], outputs[i], "run {i} diverged from run 0");
    }
}

#[test]
fn cli_failing_run_with_scanned_graph_emits_single_failed_on() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("code.graph.jsonl");
    let out = temp.path().join("testrun.graph.jsonl");
    let mini = fixtures().join("mini_crate");
    let input = fixtures().join("failing.json");

    // Scan the fixture crate to produce a real code graph.
    Command::cargo_bin("egregore")
        .expect("binary")
        .args([
            "scan",
            mini.to_str().unwrap(),
            "--out",
            graph.to_str().unwrap(),
        ])
        .assert()
        .success();

    let mut args = base_cli_args(input.to_str().unwrap(), out.to_str().unwrap());
    // failing run: exit code 101
    let idx = args.iter().position(|a| *a == "0").unwrap();
    args[idx] = "101";
    args.push("--graph");
    args.push(graph.to_str().unwrap());

    Command::cargo_bin("egregore")
        .expect("binary")
        .args(args)
        .assert()
        .success();

    let jsonl = fs::read_to_string(&out).expect("output");
    let records =
        aletheia_egregore::adapters::records_from_jsonl(&jsonl).expect("parse output jsonl");
    let failed_on = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::FailedOn,
                    ..
                }
            )
        })
        .count();
    assert_eq!(failed_on, 1, "exactly one FAILED_ON for the single failure");
    assert!(
        !records.iter().any(|r| matches!(
            r,
            GraphRecord::Edge {
                label: EdgeLabel::ClosesAcceptanceCriterion,
                ..
            }
        )),
        "a failing run never closes an acceptance criterion"
    );
}
