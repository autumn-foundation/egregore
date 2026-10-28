//! `eg scan` / `eg scan-history` on a Rust file whose Tree-sitter parse root
//! is `ERROR` (a wholly unparseable file — e.g. a starter template carrying
//! `{{crate_name}}` placeholders).
//!
//! Before this fix, the debug build panicked in `file_level_cfg_gates`
//! (`debug_assert_eq!(root.kind(), "source_file")`, egregore
//! src/languages/rust.rs) and aborted the whole-repo scan (exit 101, no
//! graph written); the release build walked the ERROR root and produced
//! undefined partial output. The pinned contract mirrors the issue-#438
//! skip classes (unreadable / non-UTF-8): the file is SKIPPED with a
//! deterministic `Diagnostic`, coverage counts it UNINDEXED, every other
//! file extracts normally, and history replay records the same class of
//! diagnostic per commit instead of aborting.
//!
//! A file with a valid `source_file` root that merely CONTAINS error nodes
//! (partially broken syntax) is a different case and must keep extracting
//! exactly as before — that is pinned here too.

#![allow(missing_docs)]

use std::{fs, path::Path, process::Command};

use aletheia_egregore::scan_repository_at_with_override;
use serde_json::Value;

/// The minimal proven ERROR-root repro (delta-minimized from Autumn's
/// `autumn-cli/src/starters/cms/tests/integration_test.rs` starter
/// template): a templated `use` inside a function body makes Tree-sitter's
/// Rust grammar return an `ERROR` root instead of `source_file`.
const UNPARSEABLE_TEMPLATE: &str = "\
async fn moderation_queue_renders_a_pager() {
    use {{crate_name}}::schema::comments;
";

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git should run");
    assert!(status.success(), "git {args:?} failed");
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn coverage_node(records: &[Value]) -> Value {
    let nodes: Vec<&Value> = records
        .iter()
        .filter(|r| r["record_type"] == "node" && r["kind"] == "ScanCoverage")
        .collect();
    assert_eq!(nodes.len(), 1, "exactly one ScanCoverage node: {nodes:?}");
    nodes[0].clone()
}

fn git_fixture_repo(temp: &tempfile::TempDir) -> std::path::PathBuf {
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/good.rs"), "pub fn good() -> u32 { 1 }\n").expect("good source");
    fs::write(repo.join("src/bad.rs"), UNPARSEABLE_TEMPLATE).expect("bad source");
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "test@example.invalid"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "fixture"]);
    repo
}

#[test]
fn scan_skips_unparseable_rust_and_records_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_fixture_repo(&temp);

    let graph_path = repo.join("graph.jsonl");
    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .output()
        .expect("scan should run");
    // The scan completes rather than panicking/aborting on the bad file.
    assert!(
        output.status.success(),
        "scan must exit 0 on an unparseable source: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write JSONL");
    let records = parse_jsonl(&jsonl);

    // The parseable file is fully extracted.
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Symbol"
                && r["repo_relative_path"] == "src/good.rs"
                && r["name"].as_str().is_some_and(|n| n.ends_with("good"))
        }),
        "the good file's symbol must be extracted"
    );

    // The unparseable file gets no File node (mirrors the #438 skip classes).
    assert!(
        !records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "File"
                && r["repo_relative_path"] == "src/bad.rs"
        }),
        "the unparseable file must not receive a File node"
    );

    // A Diagnostic names the bad file with the unparseable class + reason.
    let diagnostic = records
        .iter()
        .find(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Diagnostic"
                && r["repo_relative_path"] == "src/bad.rs"
        })
        .expect("a Diagnostic must name the skipped unparseable file");
    assert_eq!(
        diagnostic["name"], "unparseable_source",
        "diagnostic must carry the unparseable_source class: {diagnostic}"
    );
    let summary = diagnostic["summary"].as_str().unwrap_or_default();
    assert!(
        summary.contains("could not be parsed"),
        "diagnostic must state the parse failure: {summary}"
    );

    // Coverage counts the bad file as skipped/unindexed.
    let node = coverage_node(&records);
    let walked = node["scan_coverage"]["files_walked"].as_u64().unwrap();
    let indexed = node["scan_coverage"]["files_indexed"].as_u64().unwrap();
    assert_eq!(walked, 2, "coverage: {node}");
    assert_eq!(
        indexed, 1,
        "the bad .rs must not be counted indexed: {node}"
    );
    assert_eq!(
        node["scan_coverage"]["skipped_by_extension"]["rs"], 1,
        "the unparseable .rs is skipped: {node}"
    );

    // A graph carrying the diagnostic validates cleanly.
    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .arg("validate")
        .arg(&graph_path)
        .assert()
        .success();

    // Determinism: fixed-time scans are byte-identical (diagnostic included).
    let fixed_time = "2026-05-19T00:00:00Z";
    let id = Some("unparseable-stable-fixture");
    let first = scan_repository_at_with_override(&repo, fixed_time, id)
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let second = scan_repository_at_with_override(&repo, fixed_time, id)
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    assert_eq!(
        first, second,
        "scan over an unparseable file must be byte-stable"
    );
    assert!(
        first.contains("skipped source file: could not be parsed"),
        "the skip diagnostic must be present in the fixed-time scan"
    );
}

#[test]
fn scan_history_skips_unparseable_rust_and_records_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = git_fixture_repo(&temp);

    let graph_path = repo.join("history.jsonl");
    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .output()
        .expect("scan-history should run");
    assert!(
        output.status.success(),
        "scan-history must exit 0 on an unparseable committed source: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let jsonl = fs::read_to_string(&graph_path).expect("scan-history should write JSONL");
    let records = parse_jsonl(&jsonl);
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Diagnostic"
                && r["repo_relative_path"] == "src/bad.rs"
                && r["name"] == "unparseable_source"
        }),
        "history replay must record the unparseable diagnostic"
    );
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Symbol"
                && r["repo_relative_path"] == "src/good.rs"
        }),
        "history replay must still extract the good file"
    );
}

/// A valid `source_file` root that contains ERROR nodes (partially broken
/// syntax) is NOT the unparseable class: extraction proceeds exactly as
/// before and no skip diagnostic is minted for the file.
#[test]
fn partially_broken_rust_still_extracts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/mix.rs"),
        "pub fn ok() -> u32 { 1 }\npub fn broken( {\n",
    )
    .expect("mixed source");
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "test@example.invalid"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    run_git(&repo, &["add", "-A", "-f"]);
    run_git(&repo, &["commit", "-m", "fixture"]);

    let graph_path = repo.join("graph.jsonl");
    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .output()
        .expect("scan should run");
    assert!(
        output.status.success(),
        "scan must exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write JSONL");
    let records = parse_jsonl(&jsonl);
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Symbol"
                && r["repo_relative_path"] == "src/mix.rs"
                && r["name"].as_str().is_some_and(|n| n.ends_with("ok"))
        }),
        "the intact symbol of a partially broken file must be extracted"
    );
    assert!(
        !records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Diagnostic"
                && r["name"] == "unparseable_source"
                && r["repo_relative_path"] == "src/mix.rs"
        }),
        "a partially broken file must not be classified unparseable"
    );
}
