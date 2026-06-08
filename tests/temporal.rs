#![allow(missing_docs)]

//! Tests for the bi-temporal selector grammar (issue #8).
//!
//! Assertions:
//! (a) every emitted code-graph record carries an RFC3339 `valid_time` and a
//!     non-empty `valid_time_source`.
//! (b) for the history fixture, `valid_time` is monotonic with `git_commit` order.
//! (c) the CLI rejects --as-of <instant> --at <commit> together.
//! (d) eg query symbol --tx-as-of returns a `not_implemented` error envelope.
//! (e) eg query symbol --as-of <instant> returns the state at the most recent
//!     commit at or before that instant.

use std::{
    fs,
    path::Path,
    path::PathBuf,
    process::{Command, Stdio},
};

use aletheia_egregore::{
    GraphRecord, TemporalMetadata,
    ir::{Graph, stable_id},
    scan_repository_at,
};
use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use serde_json::Value;

// ---------------------------------------------------------------------------
// (a) every emitted code-graph record carries valid_time and valid_time_source
// ---------------------------------------------------------------------------

#[test]
fn scan_records_carry_valid_time_and_valid_time_source() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn hello() {}\n").expect("lib.rs");

    let fixed_time = "2026-05-19T00:00:00Z";
    let graph = scan_repository_at(repo, fixed_time).expect("scan should succeed");
    let jsonl = graph.to_jsonl().expect("serialize");
    let records = parse_jsonl(&jsonl);

    let nodes: Vec<&Value> = records
        .iter()
        .filter(|r| r["record_type"] == "node")
        .collect();

    assert!(!nodes.is_empty(), "scan should emit node records");

    for node in &nodes {
        // valid_time must be present — either at top level (current-tree records)
        // or inside temporal (history-backed records).
        let vt = node.get("temporal").map_or_else(
            || node.get("valid_time").and_then(Value::as_str),
            |temporal| temporal.get("valid_time").and_then(Value::as_str),
        );
        assert!(
            vt.is_some(),
            "node missing valid_time: {}",
            serde_json::to_string_pretty(node).unwrap()
        );
        // Must be parseable as RFC3339.
        let vt_str = vt.unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(vt_str).is_ok(),
            "valid_time is not RFC3339: {vt_str}"
        );

        // valid_time_source must be non-empty — either at top level or in temporal.
        let vts = node.get("temporal").map_or_else(
            || node.get("valid_time_source").and_then(Value::as_str),
            |temporal| temporal.get("valid_time_source").and_then(Value::as_str),
        );
        assert!(
            vts.is_some_and(|s| !s.is_empty()),
            "node missing or empty valid_time_source: {}",
            serde_json::to_string_pretty(node).unwrap()
        );
    }
}

#[test]
fn scan_history_records_carry_git_commit_committer_date_source() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_single_commit_repo(&repo);

    let graph =
        aletheia_egregore::scan_repository_history(&repo).expect("history scan should succeed");
    let jsonl = graph.to_jsonl().expect("serialize");
    let records = parse_jsonl(&jsonl);

    let temporal_nodes: Vec<&Value> = records
        .iter()
        .filter(|r| r["record_type"] == "node" && r.get("temporal").is_some())
        .collect();

    assert!(
        !temporal_nodes.is_empty(),
        "history scan should emit temporal nodes"
    );

    for node in &temporal_nodes {
        let vts = node["temporal"]
            .get("valid_time_source")
            .and_then(Value::as_str);
        assert_eq!(
            vts,
            Some("git_commit_committer_date"),
            "history-backed node must have valid_time_source=git_commit_committer_date: {}",
            serde_json::to_string_pretty(node).unwrap()
        );
    }
}

// ---------------------------------------------------------------------------
// (b) valid_time is monotonic with git_commit order
// ---------------------------------------------------------------------------

#[test]
fn history_valid_time_is_monotonic_with_commit_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    let [sha1, sha2, sha3] = seed_three_commit_repo(&repo);

    let graph =
        aletheia_egregore::scan_repository_history(&repo).expect("history scan should succeed");
    let jsonl = graph.to_jsonl().expect("serialize");
    let records = parse_jsonl(&jsonl);

    let vt1 = commit_valid_time(&records, &sha1).expect("commit 1 valid_time");
    let vt2 = commit_valid_time(&records, &sha2).expect("commit 2 valid_time");
    let vt3 = commit_valid_time(&records, &sha3).expect("commit 3 valid_time");

    let dt1 = chrono::DateTime::parse_from_rfc3339(&vt1).expect("vt1 RFC3339");
    let dt2 = chrono::DateTime::parse_from_rfc3339(&vt2).expect("vt2 RFC3339");
    let dt3 = chrono::DateTime::parse_from_rfc3339(&vt3).expect("vt3 RFC3339");

    assert!(
        dt1 <= dt2,
        "commit 1 valid_time should be <= commit 2: {vt1} vs {vt2}"
    );
    assert!(
        dt2 <= dt3,
        "commit 2 valid_time should be <= commit 3: {vt2} vs {vt3}"
    );
}

// ---------------------------------------------------------------------------
// (c) CLI rejects --as-of and --at together
// ---------------------------------------------------------------------------

#[test]
fn cli_rejects_as_of_and_at_together() {
    let (_temp, graph) = fixture_temporal_graph();

    // clap's conflicts_with produces exit code 2; the message contains the flag names.
    CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "hello"])
        .args(["--graph"])
        .arg(&graph)
        .args(["--as-of", "2026-01-02T00:00:00Z"])
        .args(["--at", "aaaa"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("as-of").or(predicate::str::contains("at")));
}

// ---------------------------------------------------------------------------
// (d) --tx-as-of is implemented (issue #66) and works on history-replay data:
//     history-backed records resolve their transaction-time handle from
//     `temporal.observed_at` (the commit timeline that replay reconstructs), so
//     the documented `scan-history` + `--tx-as-of` workflow returns a prior view
//     instead of excluding every row.
// ---------------------------------------------------------------------------

#[test]
fn tx_as_of_uses_history_temporal_metadata() {
    let (_temp, graph) = fixture_temporal_graph();

    let output = CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "hello"])
        .args(["--graph"])
        .arg(&graph)
        .args(["--tx-as-of", "2026-01-02T00:00:00Z"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8 stdout");
    let envelope: Value =
        serde_json::from_str(stdout.trim()).expect("--tx-as-of output must be valid JSON");
    assert_eq!(envelope["ok"], true, "envelope.ok must be true");
    assert_ne!(
        envelope["error"]["code"], "not_implemented",
        "--tx-as-of must no longer be reserved"
    );

    let records = envelope["records"].as_array().expect("records array");
    let tx_times: Vec<&str> = records
        .iter()
        .filter_map(|r| r["transaction_time"].as_str())
        .collect();
    // The fixture's three commit snapshots are observed at 2026-01-01/02/03.
    // tx-as-of 2026-01-02 includes the first two and excludes the 2026-01-03 one.
    assert!(
        tx_times.contains(&"2026-01-01T00:00:00Z") && tx_times.contains(&"2026-01-02T00:00:00Z"),
        "history rows at or before the instant must be returned, got {tx_times:?}"
    );
    assert!(
        !tx_times.contains(&"2026-01-03T00:00:00Z"),
        "the later commit snapshot must be excluded, got {tx_times:?}"
    );
    let codes: Vec<&str> = envelope["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(
        !codes.contains(&"missing_transaction_metadata"),
        "history rows now resolve a transaction handle; must not be reported missing, got {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// (e) --as-of returns symbol at most recent commit at or before the instant
// ---------------------------------------------------------------------------

#[test]
fn as_of_returns_symbol_at_most_recent_commit_at_or_before_instant() {
    let (_temp, graph) = fixture_temporal_graph();

    // Fixture has hello at:
    //   commit aaa: valid_time 2026-01-01T00:00:00Z
    //   commit bbb: valid_time 2026-01-02T00:00:00Z
    //   commit ccc: valid_time 2026-01-03T00:00:00Z
    //
    // --as-of 2026-01-02T12:00:00Z should return the bbb record.

    let output = CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "hello"])
        .args(["--graph"])
        .arg(&graph)
        .args(["--as-of", "2026-01-02T12:00:00Z"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "should return exactly one record for --as-of"
    );

    let parsed: Value = serde_json::from_str(lines[0]).expect("valid JSON");
    assert_eq!(parsed["name"], "hello");
    assert_eq!(
        parsed["git_commit"], "bbbbbbbbbbbbbbbb",
        "should return state at most recent commit at or before 2026-01-02T12:00:00Z"
    );
}

#[test]
fn as_of_returns_no_match_when_instant_is_before_all_commits() {
    let (_temp, graph) = fixture_temporal_graph();

    CargoCommand::cargo_bin("egregore")
        .expect("egregore binary")
        .args(["query", "symbol", "hello"])
        .args(["--graph"])
        .arg(&graph)
        .args(["--as-of", "2025-01-01T00:00:00Z"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no match"));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn commit_valid_time(records: &[Value], sha: &str) -> Option<String> {
    records.iter().find_map(|r| {
        if r["record_type"] == "node" && r["kind"] == "Commit" && r["temporal"]["git_commit"] == sha
        {
            r["temporal"]["valid_time"].as_str().map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

/// Fixture: three history records for symbol `hello` at three commits.
fn fixture_temporal_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("temporal.jsonl");

    let sym_a = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "hello", "aaa"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "hello".to_owned(),
        "hello at commit aaa".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaaaaaaaaaaaaaa".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    });

    let sym_b = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "hello", "bbb"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "hello".to_owned(),
        "hello at commit bbb".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "bbbbbbbbbbbbbbbb".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-02T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-02T00:00:00Z".to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    });

    let sym_c = GraphRecord::symbol(
        stable_id(&["node", "Symbol", "src/lib.rs", "hello", "ccc"]),
        "fn",
        "src/lib.rs".to_owned(),
        span(1, 5),
        "hello".to_owned(),
        "hello at commit ccc".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "cccccccccccccccc".to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-03T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-03T00:00:00Z".to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    });

    let mut graph = Graph::new();
    graph.push(sym_a);
    graph.push(sym_b);
    graph.push(sym_c);
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write fixture");

    (temp, path)
}

const fn span(start_line: usize, end_line: usize) -> aletheia_egregore::SourceSpan {
    aletheia_egregore::SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn seed_single_commit_repo(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "test@example.invalid"]);
    git(repo, ["config", "user.name", "Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
    write_file(repo, "src/lib.rs", "pub fn hello() {}\n");
    commit_with_date(repo, "initial commit", "2026-01-01T00:00:00Z");
}

fn seed_three_commit_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "test@example.invalid"]);
    git(repo, ["config", "user.name", "Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write_file(repo, "src/lib.rs", "pub fn hello() {}\n");
    let sha1 = commit_with_date(repo, "first", "2026-01-01T00:00:00Z");

    write_file(repo, "src/lib.rs", "pub fn hello() -> u32 { 1 }\n");
    let sha2 = commit_with_date(repo, "second", "2026-01-02T00:00:00Z");

    write_file(repo, "src/lib.rs", "pub fn hello() -> u32 { 2 }\n");
    let sha3 = commit_with_date(repo, "third", "2026-01-03T00:00:00Z");

    [sha1, sha2, sha3]
}

fn write_file(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(path, contents).expect("write file");
}

fn commit_with_date(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "."]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        status.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git command {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout)
        .expect("git output is utf-8")
        .trim()
        .to_owned()
}
