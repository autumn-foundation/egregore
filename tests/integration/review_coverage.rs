//! Integration tests for `eg audit review-coverage` (issue #339): the standing,
//! citable review-coverage gate over pull requests merged in a valid-time window.
//!
//! Covers the four planted verdict cases, the citation set, the default-1.0 gate
//! (exit 1) vs a fully-covered store (exit 0), the empty-window vacuous pass, the
//! exit-2 usage surfaces, and byte-identical determinism.

#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::Value;

const FROM: &str = "2026-03-01T00:00:00Z";
const TO: &str = "2026-04-01T00:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn task_line(pr: &str, day: &str, native: &str) -> String {
    format!(
        r#"{{"record_type":"node","id":"project:v1:{pr}","kind":"Task","schema_version":1,"valid_time":"2026-03-{day}T12:00:00Z","entity_id":"pr-entity-{pr}","source_kind":"github_pr","author":"author-{pr}","head_sha":"head-{pr}","head_ref":"feature/{pr}","base_ref":"trunk","merge_commit_sha":"mc-{pr}","merged_at":"2026-03-{day}T12:00:00Z","system_native_id":"{native}","summary":"pr {pr}"}}"#
    )
}

/// A `Review` line. `author` and `review_commit_sha` are optional (empty = omit).
fn review_line(rv: &str, day: &str, author: &str, review_commit: &str) -> String {
    let author_field = if author.is_empty() {
        String::new()
    } else {
        format!(r#","author":"{author}""#)
    };
    let rcs_field = if review_commit.is_empty() {
        String::new()
    } else {
        format!(r#","review_commit_sha":"{review_commit}""#)
    };
    format!(
        r#"{{"record_type":"node","id":"project:v1:{rv}","kind":"Review","schema_version":1,"valid_time":"2026-03-{day}T08:00:00Z","entity_id":"review-entity-{rv}","review_kind":"pr_review","review_state":"approved"{author_field}{rcs_field},"summary":"review {rv}"}}"#
    )
}

fn edge_line(rv: &str, pr: &str) -> String {
    format!(
        r#"{{"record_type":"edge","id":"project:v1:edge-{rv}-{pr}","schema_version":1,"label":"REFERENCES_TASK","source":"project:v1:{rv}","target":"project:v1:{pr}","summary":"{rv} references {pr}"}}"#
    )
}

/// Builds a planted graph with all four verdict cases and writes it to a temp
/// file, returning the tempdir guard and the graph path.
fn planted_graph() -> (tempfile::TempDir, PathBuf) {
    let lines: Vec<String> = vec![
        // covered: non-author approval at final head.
        task_line("pr01", "05", "1"),
        review_line("r01", "05", "rev-1", "head-pr01"),
        edge_line("r01", "pr01"),
        // approval_stale_head: non-author approval anchored to a non-head commit.
        task_line("pr02", "06", "2"),
        review_line("r02", "06", "rev-2", "other-sha"),
        edge_line("r02", "pr02"),
        // self_approved_only: only approval is by the PR author.
        task_line("pr03", "07", "3"),
        review_line("r03", "07", "author-pr03", "head-pr03"),
        edge_line("r03", "pr03"),
        // uncovered: merged with zero reviews.
        task_line("pr04", "08", "4"),
    ];

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("planted.graph.jsonl");
    fs::write(&path, lines.join("\n")).unwrap();
    (temp, path)
}

fn run_graph(path: &PathBuf, extra: &[&str]) -> (i32, Value, Vec<u8>) {
    let mut cmd = egregore();
    cmd.args([
        "audit",
        "review-coverage",
        "--from",
        FROM,
        "--to",
        TO,
        "--graph",
    ])
    .arg(path);
    for a in extra {
        cmd.arg(a);
    }
    let output = cmd.output().expect("run review-coverage");
    let code = output.status.code().expect("exit code");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (code, value, output.stdout)
}

#[test]
fn planted_gaps_classify_and_gate_fails_at_default_threshold() {
    let (_g, path) = planted_graph();
    let (code, report, _) = run_graph(&path, &[]);
    assert_eq!(code, 1, "coverage below 1.0 must exit 1");
    assert_eq!(report["merged_pr_count"], 4);
    assert_eq!(report["covered_count"], 1);

    let verdict_of = |pr: &str| -> String {
        report["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["pr_task_id"] == format!("project:v1:{pr}"))
            .unwrap()["verdict"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(verdict_of("pr01"), "covered");
    assert_eq!(verdict_of("pr02"), "approval_stale_head");
    assert_eq!(verdict_of("pr03"), "self_approved_only");
    assert_eq!(verdict_of("pr04"), "uncovered");

    // The below-threshold diagnostic names the failing PR record IDs.
    let diag = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "below_review_coverage_threshold")
        .expect("below-threshold diagnostic");
    let failing: Vec<&str> = diag["record_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for pr in ["project:v1:pr02", "project:v1:pr03", "project:v1:pr04"] {
        assert!(failing.contains(&pr), "failing {pr}");
    }

    // Covered row carries its full citation set.
    let covered = report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["pr_task_id"] == "project:v1:pr01")
        .unwrap();
    assert_eq!(covered["system_native_id"], "1");
    assert_eq!(covered["merge_commit_sha"], "mc-pr01");
    assert_eq!(covered["approving_review_id"], "project:v1:r01");
    assert_eq!(covered["review_commit_sha"], "head-pr01");
    assert_eq!(covered["approver_login"], "rev-1");

    // The disclaimer states the exact scope.
    let disclaimer = report["disclaimer"].as_str().unwrap();
    assert!(disclaimer.contains("branch-protection"));
    assert!(disclaimer.contains("review quality"));
}

#[test]
fn fully_covered_store_passes_gate() {
    let mut lines: Vec<String> = Vec::new();
    for (pr, rv, day) in [("pr01", "r01", "05"), ("pr02", "r02", "06")] {
        lines.push(task_line(pr, day, pr));
        lines.push(review_line(rv, day, "rev-x", &format!("head-{pr}")));
        lines.push(edge_line(rv, pr));
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("covered.graph.jsonl");
    fs::write(&path, lines.join("\n")).unwrap();
    let (code, report, _) = run_graph(&path, &[]);
    assert_eq!(code, 0, "fully covered store must exit 0");
    assert_eq!(report["coverage"], 1.0);
}

#[test]
fn empty_window_is_a_vacuous_pass() {
    let (_g, path) = planted_graph();
    let output = egregore()
        .args([
            "audit",
            "review-coverage",
            "--from",
            "2025-01-01T00:00:00Z",
            "--to",
            "2025-02-01T00:00:00Z",
            "--graph",
        ])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["merged_pr_count"], 0);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "empty_window")
    );
}

#[test]
fn reversed_and_invalid_windows_exit_2() {
    let (_g, path) = planted_graph();
    let reversed = egregore()
        .args([
            "audit",
            "review-coverage",
            "--from",
            TO,
            "--to",
            FROM,
            "--graph",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(reversed.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&reversed.stderr).unwrap();
    assert_eq!(err["code"], "reversed_window");

    let invalid = egregore()
        .args([
            "audit",
            "review-coverage",
            "--from",
            "not-a-time",
            "--to",
            TO,
            "--graph",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(err["code"], "invalid_timestamp");
}

#[test]
fn both_or_neither_input_flags_exit_2() {
    let (_g, path) = planted_graph();
    let temp = tempfile::tempdir().unwrap();
    let both = egregore()
        .args([
            "audit",
            "review-coverage",
            "--from",
            FROM,
            "--to",
            TO,
            "--graph",
        ])
        .arg(&path)
        .arg("--data-dir")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(both.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&both.stderr).unwrap();
    assert_eq!(err["code"], "conflicting_input_flags");

    let neither = egregore()
        .args(["audit", "review-coverage", "--from", FROM, "--to", TO])
        .output()
        .unwrap();
    assert_eq!(neither.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&neither.stderr).unwrap();
    assert_eq!(err["code"], "missing_input_flag");
}

#[test]
fn out_of_range_min_coverage_exits_2() {
    let (_g, path) = planted_graph();
    let out = egregore()
        .args([
            "audit",
            "review-coverage",
            "--from",
            FROM,
            "--to",
            TO,
            "--min-coverage",
            "1.5",
            "--graph",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "invalid_min_coverage");
}

#[test]
fn output_is_byte_identical_across_runs() {
    let (_g, path) = planted_graph();
    let (_c1, _v1, out1) = run_graph(&path, &[]);
    for _ in 0..5 {
        let (_c, _v, out) = run_graph(&path, &[]);
        assert_eq!(out1, out, "review-coverage stdout must be byte-identical");
    }
}

#[test]
fn require_final_head_off_treats_stale_as_covered() {
    let (_g, path) = planted_graph();
    let (code, report, _) = run_graph(&path, &["--require-final-head", "false"]);
    // pr02 (stale) becomes covered; pr03 (self) and pr04 (uncovered) remain gaps.
    let verdict_of = |pr: &str| -> String {
        report["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["pr_task_id"] == format!("project:v1:{pr}"))
            .unwrap()["verdict"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(verdict_of("pr02"), "covered");
    assert_eq!(code, 1, "still below 1.0 due to self/uncovered");
}
