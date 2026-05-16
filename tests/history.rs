#![allow(missing_docs)]

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_codegraph::scan_repository_history;
use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use serde_json::Value;

#[test]
fn git_history_replay_emits_bitemporal_records_without_mutating_checkout() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    let [first, second, third] = seed_history_repo(repo);

    let head_before = git_output(repo, ["rev-parse", "HEAD"]);
    let lib_before = fs::read_to_string(repo.join("src/lib.rs")).expect("fixture file");

    let first_jsonl = scan_repository_history(repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let second_jsonl = scan_repository_history(repo)
        .expect("history should scan twice")
        .to_jsonl()
        .expect("history graph should serialize twice");

    assert_eq!(first_jsonl, second_jsonl, "history replay must be stable");
    assert_eq!(git_output(repo, ["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        fs::read_to_string(repo.join("src/lib.rs")).expect("fixture file"),
        lib_before,
        "history replay must not mutate the checkout"
    );

    let records = parse_jsonl(&first_jsonl);
    assert_eq!(count_nodes(&records, "Commit"), 3);
    assert!(
        count_nodes(&records, "Change") >= 3,
        "each commit should emit at least one change record"
    );
    assert_edge_label(&records, "PARENT_OF");
    assert_edge_label(&records, "CHANGED_IN");

    assert_commit_node(&records, &first, "2026-01-01T00:00:00Z");
    assert_commit_node(&records, &second, "2026-01-02T00:00:00Z");
    assert_commit_node(&records, &third, "2026-01-03T00:00:00Z");
    assert_temporal_symbol(&records, "renamed", &second, "2026-01-02T00:00:00Z");
    assert_temporal_file(&records, "src/extra.rs", &third, "2026-01-03T00:00:00Z");
}

#[test]
fn scan_history_cli_writes_temporal_jsonl() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_history_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("aletheia-codegraph")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());

    let jsonl = fs::read_to_string(&graph_path).expect("scan-history should write JSONL");
    assert!(jsonl.contains(r#""kind":"Commit""#));
    assert!(jsonl.contains(r#""kind":"Change""#));
    assert!(jsonl.contains(r#""label":"PARENT_OF""#));
    assert!(jsonl.contains(r#""label":"CHANGED_IN""#));
    assert!(jsonl.contains(r#""valid_time":"2026-01-02T00:00:00Z""#));
}

fn seed_history_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);

    write(repo, "src/lib.rs", "pub fn original() -> u32 { 1 }\n");
    let first = commit(repo, "initial symbol", "2026-01-01T00:00:00Z");

    write(repo, "src/lib.rs", "pub fn renamed() -> u32 { 2 }\n");
    let second = commit(repo, "rename symbol", "2026-01-02T00:00:00Z");

    write(
        repo,
        "src/extra.rs",
        "pub struct Added;\nimpl Added { pub fn value(&self) -> u32 { 3 } }\n",
    );
    let third = commit(repo, "add extra module", "2026-01-03T00:00:00Z");

    [first, second, third]
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit(repo: &Path, message: &str, date: &str) -> String {
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
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
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
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
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

fn assert_edge_label(records: &[Value], label: &str) {
    assert!(
        records
            .iter()
            .any(|record| record["record_type"] == "edge" && record["label"] == label),
        "missing {label} edge"
    );
}

fn assert_commit_node(records: &[Value], sha: &str, valid_time: &str) {
    let candidates = records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Commit")
        .map(|record| {
            format!(
                "{} @ {}",
                record["temporal"]["git_commit"], record["temporal"]["valid_time"]
            )
        })
        .collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Commit"
                && record["temporal"]["git_commit"] == sha
                && record["temporal"]["valid_time"] == valid_time
                && record["temporal"]["author_time"] == valid_time
        }),
        "missing commit node for {sha}; candidates: {candidates:?}"
    );
}

fn assert_temporal_symbol(records: &[Value], name: &str, sha: &str, valid_time: &str) {
    assert!(
        records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["name"] == name
                && record["temporal"]["git_commit"] == sha
                && record["temporal"]["valid_time"] == valid_time
        }),
        "missing temporal symbol {name} at {sha}"
    );
}

fn assert_temporal_file(records: &[Value], path: &str, sha: &str, valid_time: &str) {
    assert!(
        records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "File"
                && record["repo_relative_path"] == path
                && record["temporal"]["git_commit"] == sha
                && record["temporal"]["valid_time"] == valid_time
        }),
        "missing temporal file {path} at {sha}"
    );
}
