//! SPEC-PROOF-RED-GREEN tests for `eg capture-coverage` (issue #230).
//!
//! Capturing a `cargo llvm-cov` run as a citable, deterministic
//! verification-domain `CoverageReport` record. CAPTURE-ONLY: the coverage
//! tool is never executed by the lane — the fixtures are pre-captured
//! artefacts, except for the one explicitly gated live-`llvm-cov` test that
//! runs a real instrumented build of the seeded `mini_cov` fixture.

#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use assert_cmd::Command as AssertCommand;
use serde_json::Value;

use aletheia_egregore::{
    adapters::records_from_jsonl,
    ir::{EdgeLabel, GraphRecord, NodeKind},
};

// ── Fixtures ────────────────────────────────────────────────────────────────

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/capture_coverage")
}

fn coverage_report_node(records: &[GraphRecord]) -> &GraphRecord {
    records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::CoverageReport,
                    ..
                }
            )
        })
        .expect("a CoverageReport node should be emitted")
}

/// Parses the normalized JSON payload out of the `CoverageReport`'s
/// `stdout_handle`.
fn normalized_payload(records: &[GraphRecord]) -> Value {
    let node = coverage_report_node(records);
    let inline = match node {
        GraphRecord::Node { stdout_handle, .. } => stdout_handle
            .as_ref()
            .expect("CoverageReport should carry a stdout_handle")
            .inline
            .clone()
            .expect("normalized summary should be inline (well under the ceiling)"),
        _ => unreachable!(),
    };
    serde_json::from_str(&inline).expect("normalized summary should be valid JSON")
}

fn file_entry<'a>(payload: &'a Value, path: &str) -> &'a Value {
    payload["files"]
        .as_array()
        .expect("files array")
        .iter()
        .find(|f| f["path"] == Value::String(path.to_owned()))
        .unwrap_or_else(|| panic!("file entry for {path}"))
}

fn base_cli_args<'a>(input: &'a str, out: &'a str) -> Vec<&'a str> {
    vec![
        "capture-coverage",
        "--input",
        input,
        "--out",
        out,
        "--session-id",
        "sess-42",
        "--commit",
        "abc123def456",
        "--command",
        "cargo llvm-cov --json --output-path cov.json",
        "--exit-code",
        "0",
        "--executed-at",
        "2026-07-19T12:00:00Z",
        "--repo-root",
        "/repo",
    ]
}

fn run_capture(args: &[&str]) -> assert_cmd::assert::Assert {
    AssertCommand::cargo_bin("egregore")
        .expect("binary")
        .args(args)
        .assert()
}

/// The seeded JSON artefact's own per-file line percentages, keyed by
/// repo-relative path — the ground truth the capture must reproduce.
fn artefact_file_percents(input: &Path) -> Vec<(String, f64)> {
    let text = fs::read_to_string(input).expect("artefact readable");
    let parsed: Value = serde_json::from_str(&text).expect("artefact is JSON");
    parsed["data"][0]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| {
            let abs = f["filename"].as_str().expect("filename");
            let rel = abs.strip_prefix("/repo/").unwrap_or(abs).to_owned();
            let pct = f["summary"]["lines"]["percent"].as_f64().expect("percent");
            (rel, pct)
        })
        .collect()
}

// ── 1. JSON capture writes a CoverageReport ─────────────────────────────────

#[test]
fn cli_json_capture_writes_coverage_report() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("coverage.graph.jsonl");
    let input = fixtures().join("sample.json");

    run_capture(&base_cli_args(
        input.to_str().unwrap(),
        out.to_str().unwrap(),
    ))
    .success()
    .stdout(predicates::str::contains(r#""ok":true"#));

    let jsonl = fs::read_to_string(&out).expect("output written");
    let records = records_from_jsonl(&jsonl).expect("parse output jsonl");
    let node = coverage_report_node(&records);
    match node {
        GraphRecord::Node {
            kind,
            id,
            domain,
            verification_kind,
            status,
            exit_code,
            executed_at,
            source_artifact_path,
            source_artifact_hash,
            evidence_quality,
            ..
        } => {
            assert_eq!(*kind, NodeKind::CoverageReport);
            assert!(id.starts_with("verification:v1:"), "id was {id}");
            assert_eq!(domain.as_deref(), Some("verification"));
            assert_eq!(verification_kind.as_deref(), Some("coverage_report"));
            assert_eq!(status.as_deref(), Some("pass"));
            assert_eq!(*exit_code, Some(0));
            assert_eq!(executed_at.as_deref(), Some("2026-07-19T12:00:00Z"));
            assert_eq!(
                source_artifact_path.as_deref(),
                Some(input.to_str().unwrap())
            );
            assert!(
                !source_artifact_hash
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
            );
            assert_eq!(evidence_quality.as_deref(), Some("summarized"));
        }
        _ => panic!("expected node"),
    }

    let payload = normalized_payload(&records);
    assert_eq!(payload["format"], Value::String("llvm-cov-v1".to_owned()));
    assert_eq!(payload["tool"], Value::String("cargo-llvm-cov".to_owned()));
    assert_eq!(payload["files_measured"], Value::from(2));
    // Overall figures reproduce the artefact's totals exactly.
    assert!((payload["lines"]["percent"].as_f64().unwrap() - 50.0).abs() < f64::EPSILON);
    assert_eq!(payload["lines"]["covered"], Value::from(14));
    assert_eq!(payload["lines"]["total"], Value::from(28));
    // Region figures are reported as regions (honest), not relabeled branches.
    assert!((payload["regions"]["percent"].as_f64().unwrap() - 56.0).abs() < f64::EPSILON);
    assert!(payload.get("branches").is_none() || payload["branches"].is_null());
}

// ── 2. Per-file figures match the artefact for 100% of resolved files ───────

#[test]
fn cli_per_file_figures_match_artefact_for_all_resolved_files() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("code.graph.jsonl");
    let out = temp.path().join("coverage.graph.jsonl");
    let mini = fixtures().join("mini_cov");
    let input = fixtures().join("sample.json");

    // Scan the real fixture crate: src/lib.rs exists there, src/unused.rs does not.
    AssertCommand::cargo_bin("egregore")
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
    args.push("--graph");
    args.push(graph.to_str().unwrap());
    run_capture(&args).success();

    let jsonl = fs::read_to_string(&out).expect("output written");
    let records = records_from_jsonl(&jsonl).expect("parse output jsonl");
    let payload = normalized_payload(&records);

    // Every resolved file reproduces the artefact's line percent within rounding.
    let mut resolved = 0;
    for (path, artefact_pct) in artefact_file_percents(&input) {
        let entry = file_entry(&payload, &path);
        if !entry["resolved"].as_bool().unwrap() {
            continue;
        }
        resolved += 1;
        let captured_pct = entry["lines"]["percent"].as_f64().expect("percent");
        assert!(
            (captured_pct - artefact_pct).abs() <= 1e-4,
            "file {path}: captured {captured_pct} vs artefact {artefact_pct}"
        );
    }
    assert!(resolved >= 1, "at least src/lib.rs must resolve");

    // src/lib.rs anchors to its File; src/unused.rs is unresolved, never dropped.
    let touched = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::TouchedFile,
                    ..
                }
            )
        })
        .count();
    assert_eq!(touched, 1, "exactly one TOUCHED_FILE edge");
    assert!(
        payload["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == Value::String("src/unused.rs".to_owned())
                && f["resolved"] == Value::Bool(false)),
        "src/unused.rs must be present and marked unresolved"
    );
    assert!(
        records.iter().any(|r| match r {
            GraphRecord::Node { symbol_kind, .. } =>
                symbol_kind.as_deref() == Some("coverage_file_unresolved"),
            _ => false,
        }),
        "unresolved file gets a coverage_file_unresolved diagnostic"
    );
}

// ── 3. LCOV capture: branches + per-function anchoring ──────────────────────

#[test]
fn cli_lcov_capture_reports_branches_and_anchors_functions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("code.graph.jsonl");
    let out = temp.path().join("coverage.graph.jsonl");
    let mini = fixtures().join("mini_cov");
    let input = fixtures().join("sample.lcov");

    AssertCommand::cargo_bin("egregore")
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
    args.push("--format");
    args.push("llvm-cov-lcov");
    args.push("--graph");
    args.push(graph.to_str().unwrap());
    run_capture(&args).success();

    let jsonl = fs::read_to_string(&out).expect("output written");
    let records = records_from_jsonl(&jsonl).expect("parse output jsonl");
    let payload = normalized_payload(&records);

    assert_eq!(
        payload["artifact_format"],
        Value::String("llvm-cov-lcov".to_owned())
    );
    // Branch figures come from BRDA records: 1 of 2 taken in src/lib.rs.
    let lib = file_entry(&payload, "src/lib.rs");
    assert_eq!(lib["branches"]["covered"], Value::from(1));
    assert_eq!(lib["branches"]["total"], Value::from(2));
    assert!((lib["branches"]["percent"].as_f64().unwrap() - 50.0).abs() < f64::EPSILON);
    // Per-function execution flags are present (LCOV names functions).
    let fns = lib["function_details"]
        .as_array()
        .expect("function_details");
    assert_eq!(fns.len(), 3);
    let flag = |name: &str| {
        fns.iter()
            .find(|f| f["name"] == Value::String(name.to_owned()))
            .unwrap_or_else(|| panic!("function {name}"))["covered"]
            .as_bool()
            .unwrap()
    };
    assert!(flag("covered_add"));
    assert!(!flag("never_called"));
    assert!(flag("maybe_double"));

    // Each named function resolves to exactly one symbol → MENTIONS_SYMBOL.
    let mentions = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::MentionsSymbol,
                    ..
                }
            )
        })
        .count();
    assert_eq!(mentions, 3, "three named functions anchor to three symbols");
}

// ── 4. Error paths: exit codes + diagnostics ────────────────────────────────

#[test]
fn cli_empty_input_exits_4_with_diagnostic_no_report() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("coverage.graph.jsonl");
    let input = fixtures().join("empty.json");

    run_capture(&base_cli_args(
        input.to_str().unwrap(),
        out.to_str().unwrap(),
    ))
    .code(4);

    let jsonl = fs::read_to_string(&out).expect("diagnostic written");
    assert!(jsonl.contains(r#""kind":"Diagnostic""#));
    assert!(jsonl.contains("empty_coverage_report"));
    assert!(!jsonl.contains(r#""kind":"CoverageReport""#));
}

#[test]
fn cli_malformed_input_exits_5_with_diagnostic_no_report() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("coverage.graph.jsonl");
    let input = fixtures().join("malformed.json");

    run_capture(&base_cli_args(
        input.to_str().unwrap(),
        out.to_str().unwrap(),
    ))
    .code(5);

    let jsonl = fs::read_to_string(&out).expect("diagnostic written");
    assert!(jsonl.contains(r#""kind":"Diagnostic""#));
    assert!(jsonl.contains("unparseable_coverage_report"));
    assert!(!jsonl.contains(r#""kind":"CoverageReport""#));
}

#[test]
fn cli_tool_missing_exits_5_with_distinct_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("coverage.graph.jsonl");
    let input = fixtures().join("tool_missing.txt");

    run_capture(&base_cli_args(
        input.to_str().unwrap(),
        out.to_str().unwrap(),
    ))
    .code(5)
    .stdout(predicates::str::contains("coverage_tool_missing"));

    let jsonl = fs::read_to_string(&out).expect("diagnostic written");
    assert!(jsonl.contains("coverage_tool_missing"));
    assert!(!jsonl.contains("unparseable_coverage_report"));
    assert!(!jsonl.contains(r#""kind":"CoverageReport""#));
}

#[test]
fn cli_bad_executed_at_exits_1() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("coverage.graph.jsonl");
    let input = fixtures().join("sample.json");
    let mut args = base_cli_args(input.to_str().unwrap(), out.to_str().unwrap());
    let idx = args
        .iter()
        .position(|a| *a == "2026-07-19T12:00:00Z")
        .unwrap();
    args[idx] = "not-a-timestamp";

    run_capture(&args)
        .code(1)
        .stderr(predicates::str::contains("executed_at"));
}

#[test]
fn cli_unknown_format_exits_1() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("coverage.graph.jsonl");
    let input = fixtures().join("sample.json");
    let mut args = base_cli_args(input.to_str().unwrap(), out.to_str().unwrap());
    args.push("--format");
    args.push("tarpaulin-json");

    run_capture(&args).code(1);
}

// ── 5. Determinism: 5 consecutive runs byte-identical ────────────────────────

#[test]
fn cli_determinism_five_runs_byte_identical() {
    let temp = tempfile::tempdir().expect("temp dir");
    let input = fixtures().join("sample.json");

    let mut outputs = Vec::new();
    for i in 0..5 {
        let out = temp.path().join(format!("run{i}.graph.jsonl"));
        run_capture(&base_cli_args(
            input.to_str().unwrap(),
            out.to_str().unwrap(),
        ))
        .success();
        outputs.push(fs::read(&out).expect("output"));
    }
    for i in 1..5 {
        assert_eq!(outputs[0], outputs[i], "run {i} diverged from run 0");
    }
}

// ── 6. Live run: real `cargo llvm-cov` on the seeded fixture ─────────────────

/// Copies the `mini_cov` fixture into a scratch dir (outside the repo, so no
/// workspace or toolchain pin interferes) for the live instrumented build.
fn stage_fixture(temp: &Path) -> PathBuf {
    let dest = temp.join("mini_cov");
    let src = dest.join("src");
    fs::create_dir_all(&src).expect("stage dirs");
    fs::copy(
        fixtures().join("mini_cov/Cargo.toml"),
        dest.join("Cargo.toml"),
    )
    .expect("copy manifest");
    fs::copy(fixtures().join("mini_cov/src/lib.rs"), src.join("lib.rs")).expect("copy lib.rs");
    dest
}

fn llvm_cov_available() -> bool {
    Command::new("cargo")
        .args(["llvm-cov", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn live_llvm_cov_run_figures_match_within_rounding() {
    if !llvm_cov_available() {
        eprintln!("SKIP: cargo-llvm-cov not installed; live coverage test skipped");
        return;
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let crate_dir = stage_fixture(temp.path());
    let cov_json = temp.path().join("cov.json");
    let graph = temp.path().join("code.graph.jsonl");
    let out = temp.path().join("coverage.graph.jsonl");

    // Scan BEFORE the instrumented build so target/ never enters the graph.
    AssertCommand::cargo_bin("egregore")
        .expect("binary")
        .args([
            "scan",
            crate_dir.to_str().unwrap(),
            "--out",
            graph.to_str().unwrap(),
        ])
        .assert()
        .success();

    // The real coverage run: caller-side, like the documented workflow.
    let cov_out = fs::File::create(&cov_json).expect("cov.json writable");
    let status = Command::new("cargo")
        .args(["llvm-cov", "--json"])
        .current_dir(&crate_dir)
        .stdout(cov_out)
        .status()
        .expect("cargo llvm-cov runs");
    let exit_code = status.code().unwrap_or(1);

    // Capture must complete in well under 2 s beyond the coverage run itself.
    let started = Instant::now();
    let exit_code_str = exit_code.to_string();
    let args = vec![
        "capture-coverage",
        "--input",
        cov_json.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--session-id",
        "sess-live",
        "--commit",
        "livecommit",
        "--command",
        "cargo llvm-cov --json",
        "--exit-code",
        &exit_code_str,
        "--executed-at",
        "2026-07-19T12:00:00Z",
        "--repo-root",
        crate_dir.to_str().unwrap(),
        "--graph",
        graph.to_str().unwrap(),
    ];
    run_capture(&args).success();
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_secs() < 2,
        "capture took {elapsed:?}, expected < 2 s beyond the coverage run"
    );

    let jsonl = fs::read_to_string(&out).expect("output written");
    // 0 raw bytes inline: the fixture's planted marker must never appear.
    assert!(
        !jsonl.contains("COVERAGE_SECRET_MARKER"),
        "raw source text must never enter the graph"
    );

    let records = records_from_jsonl(&jsonl).expect("parse output jsonl");
    let payload = normalized_payload(&records);

    // Ground truth: llvm-cov's own per-file summary, read back from the artefact.
    // 100% of the files llvm-cov measured under the repo root must resolve,
    // and every resolved file must reproduce the artefact's line percent.
    assert_all_resolved_files_match_artefact(&cov_json, &crate_dir, &payload);

    // The fixture's lib.rs anchors to its File record.
    assert!(
        records.iter().any(|r| matches!(
            r,
            GraphRecord::Edge {
                label: EdgeLabel::TouchedFile,
                ..
            }
        )),
        "TOUCHED_FILE edge expected for the fixture lib.rs"
    );

    // Partial coverage is honestly reported: never_called must be 0% covered.
    let lib_entry = file_entry(&payload, "src/lib.rs");
    let line_pct = lib_entry["lines"]["percent"].as_f64().unwrap();
    assert!(
        line_pct < 100.0 && line_pct > 0.0,
        "fixture is partially covered, got {line_pct}%"
    );
}

/// Asserts every file the coverage artefact measured under `crate_dir`
/// resolved to a graph File, and that each resolved file's normalized line
/// percent reproduces the artefact's own figure within rounding.
fn assert_all_resolved_files_match_artefact(cov_json: &Path, crate_dir: &Path, payload: &Value) {
    let artefact: Value =
        serde_json::from_str(&fs::read_to_string(cov_json).expect("artefact")).expect("json");
    let mut resolved = 0;
    let mut total = 0;
    for file in artefact["data"][0]["files"].as_array().expect("files") {
        let abs = file["filename"].as_str().expect("filename");
        let Some(rel) = abs.strip_prefix(crate_dir.to_str().unwrap()) else {
            continue;
        };
        let rel = rel.trim_start_matches('/').to_owned();
        total += 1;
        let entry = payload["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == Value::String(rel.clone()));
        let Some(entry) = entry else { continue };
        if !entry["resolved"].as_bool().unwrap() {
            continue;
        }
        resolved += 1;
        let artefact_pct = file["summary"]["lines"]["percent"]
            .as_f64()
            .expect("percent");
        let captured_pct = entry["lines"]["percent"].as_f64().expect("percent");
        assert!(
            (captured_pct - artefact_pct).abs() <= 1e-4,
            "file {rel}: captured {captured_pct} vs llvm-cov {artefact_pct}"
        );
    }
    assert!(total >= 1, "llvm-cov measured at least one file");
    assert_eq!(
        resolved, total,
        "100% of measured files under the repo root must resolve"
    );
}
