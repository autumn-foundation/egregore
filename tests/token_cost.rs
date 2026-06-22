#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::doc_markdown
)]

//! End-to-end tests for `eg audit token-cost` — the query-answer token-cost
//! gate against the ripgrep baseline (issue #84).

use std::path::{Path, PathBuf};

use aletheia_egregore::scan_repository_at_with_override;
use aletheia_egregore::token_cost::count_tokens;
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/token_cost_corpus.json")
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/token_cost_corpus")
}

/// Runs the gate with default settings and returns (exit_code, parsed report).
fn run_default() -> (i32, Value) {
    let assert = egregore()
        .args(["audit", "token-cost", "--corpus"])
        .arg(manifest_path())
        .assert();
    let output = assert.get_output();
    let code = output.status.code().unwrap_or(-1);
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("report should be valid JSON");
    (code, report)
}

fn questions(report: &Value) -> &Vec<Value> {
    report["questions"].as_array().expect("questions array")
}

fn question_by_class<'a>(report: &'a Value, class: &str) -> &'a Value {
    questions(report)
        .iter()
        .find(|q| q["class"] == class)
        .unwrap_or_else(|| panic!("missing question class {class}"))
}

// AC1/AC2/AC4: the report measures, per question, the Egregore answer token
// cost and the grep-shaped baseline token cost, and expresses a ratio with raw
// counts — for exact-symbol, file-defines, and a semantic query.
#[test]
fn measures_all_three_question_classes_with_ratios() {
    let (code, report) = run_default();
    assert_eq!(code, 0, "default gate should pass on the pinned corpus");

    for class in ["exact_symbol", "file_defines", "semantic"] {
        let q = question_by_class(&report, class);
        let baseline = q["baseline_tokens"].as_u64().unwrap();
        let egregore = q["egregore_tokens"].as_u64().unwrap();
        assert!(baseline > 0, "{class} baseline tokens should be > 0");
        assert!(egregore > 0, "{class} egregore tokens should be > 0");
        let ratio = q["ratio"].as_f64().unwrap();
        // ratio == baseline / egregore
        assert!(
            (ratio - (baseline as f64 / egregore as f64)).abs() < 0.01,
            "{class} ratio must equal baseline/egregore"
        );
        assert!(
            ratio >= 3.0,
            "{class} should beat the 3x floor, got {ratio}"
        );
        // AC2/AC10: the exact baseline command is recorded.
        assert!(
            q["baseline_command"].as_str().unwrap().starts_with("rg "),
            "{class} must record an rg baseline command"
        );
    }
}

// AC3: a single pinned, documented token-count method, reported for both sides.
#[test]
fn pins_one_documented_token_count_method() {
    let (_, report) = run_default();
    assert_eq!(report["token_count_method"], "word-punct-v1");
    assert!(
        report["token_count_method_description"]
            .as_str()
            .unwrap()
            .contains("token")
    );
}

// AC4: aggregate ratio is reported alongside raw counts.
#[test]
fn reports_aggregate_ratio_and_raw_counts() {
    let (_, report) = run_default();
    let agg = &report["aggregate"];
    let baseline = agg["baseline_tokens"].as_u64().unwrap();
    let egregore = agg["egregore_tokens"].as_u64().unwrap();
    let ratio = agg["ratio"].as_f64().unwrap();
    assert!(baseline > 0 && egregore > 0);
    assert!((ratio - (baseline as f64 / egregore as f64)).abs() < 0.01);
    assert!(ratio >= 3.0, "aggregate ratio should beat 3x, got {ratio}");
}

// AC5: correctness held constant — each counted answer carries the expected
// record ID plus a file/span handle.
#[test]
fn counted_answers_carry_expected_handle() {
    let (_, report) = run_default();
    for q in questions(&report) {
        assert_eq!(
            q["correctness"], "cited",
            "question {} must be cited",
            q["id"]
        );
        assert_eq!(q["answer_has_expected_record_id"], true);
        assert_eq!(q["answer_has_file_span_or_commit_handle"], true);
    }
}

// AC1: the Egregore answer token cost equals the real `eg query` answer bytes
// for the structural (symbol/file) questions — measuring the returned answer.
#[test]
fn egregore_tokens_match_real_query_answer() {
    // Scan the corpus the same way the gate does and write a graph JSONL.
    let graph = scan_repository_at_with_override(
        corpus_dir(),
        "2026-01-01T00:00:00Z",
        Some("token-cost-fixture"),
    )
    .expect("scan corpus");
    let jsonl: String = graph
        .records()
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let tmp = tempfile::tempdir().unwrap();
    let graph_path = tmp.path().join("graph.jsonl");
    std::fs::write(&graph_path, jsonl).unwrap();

    let (_, report) = run_default();

    // exact symbol
    let out = egregore()
        .args(["query", "symbol", "config::parse_config", "--graph"])
        .arg(&graph_path)
        .args(["--format", "json"])
        .assert()
        .success();
    let answer = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let answer_tokens = count_tokens(answer.trim_end_matches('\n'));
    let q = question_by_class(&report, "exact_symbol");
    assert_eq!(
        q["egregore_tokens"].as_u64().unwrap() as usize,
        answer_tokens,
        "exact_symbol answer tokens must match the real eg query symbol output"
    );

    // file defines
    let out = egregore()
        .args(["query", "file", "src/config.rs", "--graph"])
        .arg(&graph_path)
        .args(["--format", "json"])
        .assert()
        .success();
    let answer = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let answer_tokens = count_tokens(answer.trim_end_matches('\n'));
    let q = question_by_class(&report, "file_defines");
    assert_eq!(
        q["egregore_tokens"].as_u64().unwrap() as usize,
        answer_tokens,
        "file_defines answer tokens must match the real eg query file output"
    );
}

// AC6: a class below threshold yields a distinct exit code and a stable
// diagnostic naming the class and the observed ratio — never a silent success.
#[test]
fn below_threshold_fails_with_named_diagnostic() {
    let assert = egregore()
        .args(["audit", "token-cost", "--corpus"])
        .arg(manifest_path())
        .args(["--min-ratio", "1000"])
        .assert();
    let output = assert.get_output();
    assert_eq!(
        output.status.code(),
        Some(1),
        "gate must fail below threshold"
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    let diags = report["diagnostics"].as_array().unwrap();
    assert!(!diags.is_empty(), "must emit diagnostics");
    let threshold_diag = diags
        .iter()
        .find(|d| d["code"] == "below_token_savings_threshold")
        .expect("a below-threshold diagnostic");
    assert!(threshold_diag["class"].is_string());
    assert!(threshold_diag["observed_ratio"].is_number());
}

// AC8: byte-identical token counts, ratios, and ordering across 5 runs.
#[test]
fn deterministic_across_five_runs() {
    let first = egregore()
        .args(["audit", "token-cost", "--corpus"])
        .arg(manifest_path())
        .assert()
        .get_output()
        .stdout
        .clone();
    for _ in 0..4 {
        let again = egregore()
            .args(["audit", "token-cost", "--corpus"])
            .arg(manifest_path())
            .assert()
            .get_output()
            .stdout
            .clone();
        assert_eq!(first, again, "token-cost report must be byte-identical");
    }
}

// AC10: enough non-sensitive context to interpret the result.
#[test]
fn reports_interpretation_context() {
    let (_, report) = run_default();
    let corpus = &report["corpus"];
    assert!(corpus["name"].is_string());
    assert!(corpus["source_file_count"].as_u64().unwrap() >= 1);
    assert!(corpus["record_count"].as_u64().unwrap() >= 1);
    assert_eq!(corpus["store_kind"], "jsonl");
    assert!(report["egregore_version"].is_string());
    for q in questions(&report) {
        assert!(
            q["baseline_command"]
                .as_str()
                .unwrap()
                .contains("token_cost_corpus")
        );
    }
}

// Exit code 2 on a missing/unparseable manifest.
#[test]
fn missing_manifest_is_usage_error() {
    let assert = egregore()
        .args([
            "audit",
            "token-cost",
            "--corpus",
            "/nonexistent/manifest.json",
        ])
        .assert();
    assert_eq!(assert.get_output().status.code(), Some(2));
}
