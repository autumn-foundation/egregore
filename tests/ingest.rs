#![allow(missing_docs)]

use std::{fs, path::PathBuf};

#[cfg(feature = "embedded-aletheiadb")]
use std::{
    path::Path,
    process::{Command as ProcessCommand, Stdio},
};

#[cfg(feature = "embedded-aletheiadb")]
use aletheia_codegraph::adapters::{EmbeddedAletheiaSink, records_from_jsonl};
#[cfg(feature = "embedded-aletheiadb")]
use aletheia_codegraph::scan_repository_history;
use aletheia_codegraph::{
    adapters::{FakeSink, ingest_records},
    scan_repository,
};
use assert_cmd::Command;
use predicates::prelude::*;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

#[test]
fn adapter_reports_partial_success() {
    let records = scan_repository(fixture_repo())
        .expect("fixture repo should scan")
        .records()
        .to_vec();
    let original = records.clone();
    let mut sink = FakeSink::fail_after(2);

    let report = ingest_records(&records, &mut sink);

    assert_eq!(records, original, "ingest must not mutate retry input");
    assert_eq!(report.attempted, records.len());
    assert_eq!(report.succeeded, 2);
    assert_eq!(report.failed, records.len() - 2);
    assert!(!report.is_success());
    assert!(report.failures[0].message.contains("fake adapter failure"));
}

#[test]
fn dry_run_ingest_preserves_jsonl_for_retry() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");

    Command::cargo_bin("aletheia-codegraph")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let before = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");

    Command::cargo_bin("aletheia-codegraph")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("attempted:"))
        .stdout(predicate::str::contains("succeeded:"))
        .stdout(predicate::str::contains("failed: 0"))
        .stderr(predicate::str::is_empty());

    let after = fs::read_to_string(&graph_path).expect("graph JSONL should still exist");
    assert_eq!(before, after, "dry-run ingest must preserve retry input");
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_cli_ingest_accepts_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("aletheia-store");

    Command::cargo_bin("aletheia-codegraph")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("aletheia-codegraph")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("failed: 0"))
        .stderr(predicate::str::is_empty());
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_ingest_reads_back_and_traverses_repository_file_symbol() {
    let jsonl = scan_repository(fixture_repo())
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    let records = records_from_jsonl(&jsonl).expect("graph JSONL should parse");
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let mut sink = EmbeddedAletheiaSink::open(temp.path()).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    let repository = records
        .iter()
        .find(|record| record.node_kind_name() == Some("Repository"))
        .expect("fixture should include repository node");
    assert_eq!(
        sink.read_back(repository.id()).expect("read back"),
        Some(repository.clone())
    );
    assert!(
        sink.has_repository_file_symbol_path(repository.id())
            .expect("embedded traversal should run"),
        "embedded store should contain Repository -> File -> Symbol path"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_history_ingest_traverses_commit_change_symbol() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_history_repo(&repo);
    let records = scan_repository_history(&repo)
        .expect("history should scan")
        .records()
        .to_vec();
    let mut sink =
        EmbeddedAletheiaSink::open(temp.path().join("store")).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    let commit = records
        .iter()
        .find(|record| record.node_kind_name() == Some("Commit"))
        .expect("history should include a commit");
    assert!(
        sink.has_commit_change_symbol_path(commit.id())
            .expect("embedded traversal should run"),
        "embedded store should contain Commit -> Change -> Symbol path"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn seed_history_repo(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);

    write(repo, "src/lib.rs", "pub fn original() -> u32 { 1 }\n");
    commit(repo, "initial symbol", "2026-01-01T00:00:00Z");

    write(repo, "src/lib.rs", "pub fn renamed() -> u32 { 2 }\n");
    commit(repo, "rename symbol", "2026-01-02T00:00:00Z");
}

#[cfg(feature = "embedded-aletheiadb")]
fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

#[cfg(feature = "embedded-aletheiadb")]
fn commit(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        output.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = ProcessCommand::new("git")
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
