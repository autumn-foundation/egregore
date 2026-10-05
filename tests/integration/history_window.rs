#![allow(missing_docs)]

//! Commit-window scoping for `eg scan-history` (issue #256).
//!
//! `--max-commits N`, `--since <RFC 3339>`, and `--from <rev> --to <rev>`
//! bound the replayed history; the resolved window is recorded on a
//! `HistoryReplayWindow` graph node so a windowed store is never mistaken for
//! full history.

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use serde_json::Value;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .expect("git should execute");
    assert!(status.success(), "git {args:?} failed");
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit(repo: &Path, message: &str, date: &str) -> String {
    git(repo, &["add", "."]);
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
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, &["rev-parse", "HEAD"])
}

/// Seeds five single-file commits on consecutive UTC midnights; returns their
/// SHAs oldest-first.
fn seed_five_commit_repo(repo: &Path) -> Vec<String> {
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "codegraph@example.invalid"]);
    git(repo, &["config", "user.name", "Codegraph Test"]);
    git(repo, &["config", "core.autocrlf", "false"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    let mut shas = Vec::new();
    for day in 1..=5 {
        write(
            repo,
            "src/lib.rs",
            &format!("pub fn f{day}() -> u32 {{ {day} }}\n"),
        );
        shas.push(commit(
            repo,
            &format!("commit {day}"),
            &format!("2026-01-0{day}T00:00:00Z"),
        ));
    }
    shas
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn count_nodes(records: &[Value], kind: &str) -> usize {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == kind)
        .count()
}

/// Full SHAs of the replayed `Commit` nodes (the node's `name` carries the SHA).
fn replayed_commit_shas(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Commit")
        .filter_map(|record| record["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

fn window_nodes(records: &[Value]) -> Vec<&Value> {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "HistoryReplayWindow")
        .collect()
}

fn window_payload(records: &[Value]) -> &Value {
    let nodes = window_nodes(records);
    assert_eq!(
        nodes.len(),
        1,
        "expected exactly one HistoryReplayWindow node"
    );
    &nodes[0]["history_replay_window"]
}

fn contains_edge_to(records: &[Value], target_id: &str) -> bool {
    records.iter().any(|record| {
        record["record_type"] == "edge"
            && record["label"] == "CONTAINS"
            && record["target"] == target_id
            && record["source"].as_str().is_some_and(|source| {
                records.iter().any(|node| {
                    node["record_type"] == "node"
                        && node["id"] == source
                        && node["kind"] == "Repository"
                })
            })
    })
}

fn scan_history_ok(repo: &Path, out: &Path, extra_args: &[&str]) -> Vec<Value> {
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(out)
        .args(extra_args)
        .assert()
        .success();
    let jsonl = fs::read_to_string(out).expect("scan-history should write JSONL");
    parse_jsonl(&jsonl)
}

/// Runs `scan-history` expecting a window rejection: exit code 2, a
/// single-line machine-readable JSON diagnostic on stderr, and no `--out`
/// file written.
fn assert_window_rejected(repo: &Path, out: &Path, extra_args: &[&str], expected_code: &str) {
    assert!(
        !out.exists(),
        "precondition: the output file must not exist before the run"
    );
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(out)
        .args(extra_args)
        .assert()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "window rejection must emit a single-line diagnostic, got: {stderr:?}"
    );
    let diagnostic: Value =
        serde_json::from_str(lines[0]).expect("diagnostic must be single-line JSON");
    assert_eq!(
        diagnostic["code"].as_str(),
        Some(expected_code),
        "unexpected diagnostic code in {diagnostic}"
    );
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "diagnostic must carry a human-readable message: {diagnostic}"
    );
    assert!(
        !out.exists(),
        "a rejected window must not write a partial output file"
    );
}

#[test]
fn window_max_commits_replays_only_n_most_recent() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    let records = scan_history_ok(&repo, &out, &["--max-commits", "2"]);

    // Sorted comparison: `to_jsonl` orders lines canonically, not in replay
    // order.
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = vec![shas[3].clone(), shas[4].clone()];
    expected.sort();
    assert_eq!(
        replayed, expected,
        "count window must replay exactly the N most-recent commits"
    );
    let payload = window_payload(&records);
    assert_eq!(payload["window"], Value::String("count".to_owned()));
    assert_eq!(payload["max_commits"], Value::from(2));
    assert_eq!(payload["selected_commit_count"], Value::from(2));
    assert_eq!(payload["oldest_commit_sha"], Value::String(shas[3].clone()));
    assert_eq!(payload["newest_commit_sha"], Value::String(shas[4].clone()));
    let window_id = window_nodes(&records)[0]["id"]
        .as_str()
        .expect("window node should have an id");
    assert!(
        contains_edge_to(&records, window_id),
        "the window node must be contained by its Repository"
    );
}

#[test]
fn window_max_commits_single_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    let records = scan_history_ok(&repo, &out, &["--max-commits", "1"]);

    assert_eq!(replayed_commit_shas(&records), vec![shas[4].clone()]);
    assert_eq!(
        window_payload(&records)["selected_commit_count"],
        Value::from(1)
    );
}

#[test]
fn window_max_commits_beyond_history_replays_everything() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    let records = scan_history_ok(&repo, &out, &["--max-commits", "100"]);

    // `Graph::to_jsonl` emits canonically sorted lines, so commit nodes are
    // not in replay order on the wire; compare as sets like the no-flags
    // full-history test does.
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = shas;
    expected.sort();
    assert_eq!(replayed, expected);
    assert_eq!(
        window_payload(&records)["selected_commit_count"],
        Value::from(5)
    );
}

#[test]
fn window_since_filters_by_committer_time() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    // The boundary instant is inclusive: the commit AT the instant is replayed.
    let records = scan_history_ok(&repo, &out, &["--since", "2026-01-04T00:00:00Z"]);

    // Sorted comparison: `to_jsonl` orders lines canonically, not in replay
    // order.
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = vec![shas[3].clone(), shas[4].clone()];
    expected.sort();
    assert_eq!(replayed, expected);
    let payload = window_payload(&records);
    assert_eq!(payload["window"], Value::String("since".to_owned()));
    assert_eq!(
        payload["since_instant"],
        Value::String("2026-01-04T00:00:00Z".to_owned())
    );
    assert_eq!(payload["selected_commit_count"], Value::from(2));
}

#[test]
fn window_since_accepts_non_utc_offset_and_normalizes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    // 2026-01-04T02:00:00+02:00 == 2026-01-04T00:00:00Z.
    let records = scan_history_ok(&repo, &out, &["--since", "2026-01-04T02:00:00+02:00"]);

    // Sorted comparison: `to_jsonl` orders lines canonically, not in replay
    // order.
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = vec![shas[3].clone(), shas[4].clone()];
    expected.sort();
    assert_eq!(replayed, expected);
    assert_eq!(
        window_payload(&records)["since_instant"],
        Value::String("2026-01-04T00:00:00Z".to_owned()),
        "the recorded instant is normalized to UTC"
    );
}

#[test]
fn window_range_from_to_replays_exclusive_start_inclusive_end() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    let records = scan_history_ok(&repo, &out, &["--from", &shas[1], "--to", &shas[3]]);

    // Sorted comparison: `to_jsonl` orders lines canonically, not in replay
    // order.
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = vec![shas[2].clone(), shas[3].clone()];
    expected.sort();
    assert_eq!(
        replayed, expected,
        "range window is from-exclusive, to-inclusive"
    );
    let payload = window_payload(&records);
    assert_eq!(payload["window"], Value::String("range".to_owned()));
    assert_eq!(payload["from_rev"], Value::String(shas[1].clone()));
    assert_eq!(payload["to_rev"], Value::String(shas[3].clone()));
    assert_eq!(payload["from_sha"], Value::String(shas[1].clone()));
    assert_eq!(payload["to_sha"], Value::String(shas[3].clone()));
}

#[test]
fn window_from_alone_defaults_end_to_head() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    let records = scan_history_ok(&repo, &out, &["--from", &shas[2]]);

    // Sorted comparison: `to_jsonl` orders lines canonically, not in replay
    // order.
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = vec![shas[3].clone(), shas[4].clone()];
    expected.sort();
    assert_eq!(replayed, expected);
    let payload = window_payload(&records);
    assert_eq!(payload["to_rev"], Value::String("HEAD".to_owned()));
    assert_eq!(payload["to_sha"], Value::String(shas[4].clone()));
}

#[test]
fn window_to_alone_replays_all_reachable_from_rev() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    let records = scan_history_ok(&repo, &out, &["--to", &shas[1]]);

    // Sorted comparison: `to_jsonl` orders lines canonically, not in replay
    // order.
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = vec![shas[0].clone(), shas[1].clone()];
    expected.sort();
    assert_eq!(replayed, expected);
    assert_eq!(
        window_payload(&records)["to_sha"],
        Value::String(shas[1].clone())
    );
}

#[test]
fn window_conflicting_forms_are_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);

    assert_window_rejected(
        &repo,
        &temp.path().join("a.jsonl"),
        &["--max-commits", "2", "--since", "2026-01-01T00:00:00Z"],
        "conflicting_window",
    );
    assert_window_rejected(
        &repo,
        &temp.path().join("b.jsonl"),
        &["--max-commits", "2", "--from", "HEAD~1"],
        "conflicting_window",
    );
    assert_window_rejected(
        &repo,
        &temp.path().join("c.jsonl"),
        &["--since", "2026-01-01T00:00:00Z", "--to", "HEAD"],
        "conflicting_window",
    );
}

#[test]
fn window_zero_max_commits_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);

    assert_window_rejected(
        &repo,
        &temp.path().join("out.jsonl"),
        &["--max-commits", "0"],
        "invalid_window",
    );
}

#[test]
fn window_non_integer_max_commits_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);

    assert_window_rejected(
        &repo,
        &temp.path().join("out.jsonl"),
        &["--max-commits", "many"],
        "invalid_window",
    );
}

#[test]
fn window_unparseable_since_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);

    assert_window_rejected(
        &repo,
        &temp.path().join("out.jsonl"),
        &["--since", "yesterday-ish"],
        "invalid_window",
    );
}

#[test]
fn window_unresolvable_rev_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);

    assert_window_rejected(
        &repo,
        &temp.path().join("out.jsonl"),
        &["--from", "no-such-rev", "--to", "HEAD"],
        "unresolvable_rev",
    );
}

#[test]
fn window_empty_range_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);

    assert_window_rejected(
        &repo,
        &temp.path().join("out.jsonl"),
        &["--from", "HEAD", "--to", "HEAD"],
        "empty_window",
    );
}

#[test]
fn window_future_since_is_rejected_as_empty() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);

    assert_window_rejected(
        &repo,
        &temp.path().join("out.jsonl"),
        &["--since", "2999-01-01T00:00:00Z"],
        "empty_window",
    );
}

#[test]
fn windowed_replay_is_byte_stable_across_runs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);
    let first = temp.path().join("first.jsonl");
    let second = temp.path().join("second.jsonl");

    for out in [&first, &second] {
        CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .arg("scan-history")
            .arg(&repo)
            .arg("--out")
            .arg(out)
            .arg("--max-commits")
            .arg("3")
            .assert()
            .success();
    }

    assert_eq!(
        fs::read(&first).expect("first output"),
        fs::read(&second).expect("second output"),
        "a fixed window over a fixed repository must replay byte-identically"
    );
}

#[test]
fn full_replay_without_flags_emits_no_window_node() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_five_commit_repo(&repo);
    let out = temp.path().join("full.jsonl");

    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    assert.stderr(predicate::str::is_empty());

    let jsonl = fs::read_to_string(&out).expect("scan-history should write JSONL");
    let records = parse_jsonl(&jsonl);
    let mut replayed = replayed_commit_shas(&records);
    replayed.sort();
    let mut expected = shas;
    expected.sort();
    assert_eq!(
        replayed, expected,
        "default behavior replays the full history"
    );
    assert_eq!(
        count_nodes(&records, "HistoryReplayWindow"),
        0,
        "an unwindowed replay records no window node"
    );
}

#[test]
fn windowed_replay_does_not_mutate_the_checkout() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_five_commit_repo(&repo);
    let out = temp.path().join("window.jsonl");

    let head_before = git_output(&repo, &["rev-parse", "HEAD"]);
    let lib_before = fs::read_to_string(repo.join("src/lib.rs")).expect("fixture file");

    scan_history_ok(&repo, &out, &["--max-commits", "2"]);

    assert_eq!(git_output(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        fs::read_to_string(repo.join("src/lib.rs")).expect("fixture file"),
        lib_before,
        "windowed replay must not mutate the checkout"
    );
}
