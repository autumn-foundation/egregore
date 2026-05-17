#![allow(missing_docs)]

use std::{fs, path::PathBuf};

#[cfg(feature = "embedded-aletheiadb")]
use std::{
    path::Path,
    process::{Command as ProcessCommand, Stdio},
};

#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::adapters::{EmbeddedAletheiaSink, records_from_jsonl};
#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::{EdgeLabel, GraphRecord, NodeKind, scan_repository_history, stable_id};
use aletheia_egregore::{
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

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let before = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");

    Command::cargo_bin("egregore")
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

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
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
#[test]
fn embedded_read_back_reconstructs_persisted_node_and_edge_after_reopen() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_history_repo(&repo);
    let records = scan_repository_history(&repo)
        .expect("history should scan")
        .records()
        .to_vec();
    let data_dir = temp.path().join("store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
    let symbol = records
        .iter()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(name),
                    temporal: Some(_),
                    span: Some(_),
                    symbol_kind: Some(_),
                    ..
                } if name == "renamed"
            )
        })
        .expect("history should include renamed symbol");
    let defines_edge = records
        .iter()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Edge {
                    label: EdgeLabel::Defines,
                    target,
                    temporal: Some(_),
                    ..
                } if target == symbol.id()
            )
        })
        .expect("history should include a temporal DEFINES edge");

    assert_eq!(
        reopened.read_back(symbol.id()).expect("node read-back"),
        Some(symbol.clone())
    );
    assert_eq!(
        reopened
            .read_back(defines_edge.id())
            .expect("edge read-back"),
        Some(defines_edge.clone())
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn embedded_history_edges_attach_to_matching_temporal_symbol_node() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_stable_symbol_history_repo(&repo);
    let records = scan_repository_history(&repo)
        .expect("history should scan")
        .records()
        .to_vec();
    let data_dir = temp.path().join("temporal-store");
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

    let report = ingest_records(&records, &mut sink);

    assert!(report.is_success(), "{report:?}");
    sink.persist_indexes()
        .expect("embedded indexes should persist");
    drop(sink);

    let db = reopen_embedded_db(&data_dir);
    let first_commit_id = stable_id(&["node", "commit", "repo", &first]);
    let symbol_observation_commits =
        changed_symbol_git_commits_for_commit(&db, &first_commit_id, "stable");

    assert!(
        symbol_observation_commits.contains(&first),
        "first commit should point at the first temporal stable symbol, got {symbol_observation_commits:?}"
    );
    assert!(
        !symbol_observation_commits.contains(&second),
        "first commit was wired to a later stable symbol observation: {symbol_observation_commits:?}"
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
fn seed_stable_symbol_history_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);

    write(repo, "src/lib.rs", "pub fn stable() -> u32 { 1 }\n");
    let first = commit_with_sha(repo, "initial stable symbol", "2026-03-01T00:00:00Z");

    write(repo, "src/lib.rs", "pub fn stable() -> u32 { 2 }\n");
    let second = commit_with_sha(repo, "change stable symbol body", "2026-03-02T00:00:00Z");

    [first, second]
}

#[cfg(feature = "embedded-aletheiadb")]
fn reopen_embedded_db(data_dir: &Path) -> ::aletheiadb::AletheiaDB {
    let config = ::aletheiadb::config::durable_config_for_data_dir(data_dir);
    ::aletheiadb::AletheiaDB::with_unified_config(config)
        .expect("embedded AletheiaDB store should reopen")
}

#[cfg(feature = "embedded-aletheiadb")]
fn changed_symbol_git_commits_for_commit(
    db: &::aletheiadb::AletheiaDB,
    commit_record_id: &str,
    symbol_name: &str,
) -> Vec<String> {
    let commit_node_id = node_id_by_codegraph_id(db, commit_record_id);
    let mut commits = Vec::new();
    for contains_edge_id in db.get_outgoing_edges_with_label(commit_node_id, "CONTAINS") {
        let change_node_id = db
            .get_edge_target(contains_edge_id)
            .expect("CONTAINS edge should have a target");
        let change = db
            .get_node(change_node_id)
            .expect("CONTAINS target should be readable");
        if node_property(&change, "kind") != Some("Change") {
            continue;
        }

        for changed_edge_id in db.get_incoming_edges_with_label(change_node_id, "CHANGED_IN") {
            let source_node_id = db
                .get_edge_source(changed_edge_id)
                .expect("CHANGED_IN edge should have a source");
            let source = db
                .get_node(source_node_id)
                .expect("CHANGED_IN source should be readable");
            if node_property(&source, "kind") == Some("Symbol")
                && node_property(&source, "name") == Some(symbol_name)
                && let Some(git_commit) = node_property(&source, "git_commit")
            {
                commits.push(git_commit.to_owned());
            }
        }
    }
    commits.sort();
    commits.dedup();
    commits
}

#[cfg(feature = "embedded-aletheiadb")]
fn node_id_by_codegraph_id(db: &::aletheiadb::AletheiaDB, record_id: &str) -> ::aletheiadb::NodeId {
    db.get_all_node_ids()
        .into_iter()
        .find(|node_id| {
            db.get_node(*node_id)
                .expect("node should be readable")
                .get_property("codegraph_id")
                .and_then(|value| value.as_str())
                == Some(record_id)
        })
        .unwrap_or_else(|| panic!("missing embedded node for {record_id}"))
}

#[cfg(feature = "embedded-aletheiadb")]
fn node_property<'a>(node: &'a ::aletheiadb::Node, key: &str) -> Option<&'a str> {
    node.get_property(key).and_then(|value| value.as_str())
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
fn commit_with_sha(repo: &Path, message: &str, date: &str) -> String {
    commit(repo, message, date);
    git_output(repo, ["rev-parse", "HEAD"])
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

#[cfg(feature = "embedded-aletheiadb")]
fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
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
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}
