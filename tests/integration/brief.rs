//! Integration tests for `eg brief` (issue #214): brief the agent on graph
//! evidence scoped to the working-tree diff.
//!
//! Fixture shape: a temp Git repository with two Rust source files, committed,
//! then scanned with `eg scan` into a graph JSONL that lives OUTSIDE the repo
//! (so the store artifact never pollutes the diff). Tests then mutate the
//! working tree and assert the briefing resolves handles, sections, and the
//! staleness marker.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command as CargoCommand;
use serde_json::Value;

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should be built")
}

fn git(repo: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()
        .expect("git should run");
    assert!(status.success(), "git {args:?} failed");
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    // Probes only: GIT_OPTIONAL_LOCKS=0 keeps even `git status` from
    // refreshing the index, so the probes never perturb what they measure.
    let output = StdCommand::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("git should run");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("utf8")
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

/// Seeds a two-file fixture crate, commits it, and scans it into a graph
/// JSONL outside the repo. Returns `(tempdir, repo, graph)`.
fn fixture_repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    write(
        &repo,
        "src/a.rs",
        "pub fn alpha() -> u64 {\n    1\n}\n\npub fn beta() -> u64 {\n    2\n}\n",
    );
    write(&repo, "src/b.rs", "pub fn gamma() -> u64 {\n    3\n}\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let graph = temp.path().join("code.graph.jsonl");
    egregore()
        .args(["scan"])
        .arg(&repo)
        .args(["--out"])
        .arg(&graph)
        .assert()
        .success();
    (temp, repo, graph)
}

/// Runs `eg brief` and returns `(exit_code, stdout)`.
fn brief(repo: &Path, graph: &Path, extra: &[&str]) -> (i32, String) {
    let assert = egregore()
        .arg("brief")
        .arg(repo)
        .args(["--graph"])
        .arg(graph)
        .args(extra)
        .assert();
    let code = assert.get_output().status.code().unwrap_or(-1);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    (code, stdout)
}

fn parse_ok(stdout: &str) -> Value {
    let value: Value = serde_json::from_str(stdout).expect("brief output should be JSON");
    assert_eq!(value["ok"], true, "brief should succeed: {stdout}");
    value["result"].clone()
}

#[test]
fn brief_resolves_changed_file_and_intersecting_symbol() {
    let (_temp, repo, graph) = fixture_repo();
    // Change line 2 (inside `alpha`, lines 1-3); `beta` (lines 5-7) untouched.
    write(
        &repo,
        "src/a.rs",
        "pub fn alpha() -> u64 {\n    10\n}\n\npub fn beta() -> u64 {\n    2\n}\n",
    );
    let (code, stdout) = brief(&repo, &graph, &[]);
    assert_eq!(code, 0);
    let result = parse_ok(&stdout);

    let files = result["changed_files"].as_array().expect("array");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "src/a.rs");
    assert_eq!(files[0]["status"], "modified");
    assert!(
        files[0]["record_id"].is_string(),
        "changed file resolves to its File record"
    );

    let symbols = result["changed_symbols"].as_array().expect("array");
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().expect("name"))
        .collect();
    // The scanner qualifies names (`a::alpha`); match on the suffix.
    assert!(
        names.iter().any(|n| n.ends_with("alpha")),
        "intersecting symbol is reported: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.ends_with("beta")),
        "non-intersecting symbol is excluded: {names:?}"
    );
    assert!(
        result["unindexed"].as_array().expect("array").is_empty(),
        "indexed files are not unindexed"
    );
}

#[test]
fn brief_reports_untracked_file_as_unindexed() {
    let (_temp, repo, graph) = fixture_repo();
    write(&repo, "src/new.rs", "pub fn fresh() -> u64 {\n    42\n}\n");
    let (code, stdout) = brief(&repo, &graph, &[]);
    assert_eq!(code, 0);
    let result = parse_ok(&stdout);
    let unindexed = result["unindexed"].as_array().expect("array");
    assert_eq!(unindexed.len(), 1);
    assert_eq!(unindexed[0]["path"], "src/new.rs");
    assert_eq!(unindexed[0]["status"], "untracked");
}

#[test]
fn brief_clean_tree_is_empty_but_well_formed() {
    let (_temp, repo, graph) = fixture_repo();
    let (code, stdout) = brief(&repo, &graph, &[]);
    assert_eq!(code, 0, "clean tree is not an error");
    let result = parse_ok(&stdout);
    for section in [
        "changed_files",
        "changed_symbols",
        "unindexed",
        "source_facts",
        "observations",
        "decisions",
        "in_flight_tasks",
        "verification_evidence",
        "drift_warnings",
    ] {
        assert!(
            result[section].as_array().expect("array").is_empty(),
            "{section} should be empty on a clean tree"
        );
    }
    assert!(
        result["prior_failures"]["runtime_failures"]
            .as_array()
            .expect("array")
            .is_empty()
    );
    // The store-coverage map is still populated: domain-absent vs
    // entity-absent stays derivable on an empty briefing.
    assert_eq!(result["store_coverage"]["code_graph"], true);
}

#[test]
fn brief_never_mutates_the_working_tree_or_index() {
    let (_temp, repo, graph) = fixture_repo();
    write(
        &repo,
        "src/a.rs",
        "pub fn alpha() -> u64 {\n    10\n}\n\npub fn beta() -> u64 {\n    2\n}\n",
    );
    write(&repo, "src/new.rs", "pub fn fresh() -> u64 {\n    42\n}\n");
    let before = git_output(&repo, &["status", "--porcelain"]);
    let index_before = fs::metadata(repo.join(".git/index"))
        .expect("index")
        .modified()
        .expect("mtime");
    let (code, _) = brief(&repo, &graph, &[]);
    assert_eq!(code, 0);
    // Snapshot the index BEFORE the `git status` probe below: the probe is
    // read-only (GIT_OPTIONAL_LOCKS=0), but measuring first keeps the test
    // honest about whose write it would catch.
    let index_after = fs::metadata(repo.join(".git/index"))
        .expect("index")
        .modified()
        .expect("mtime");
    assert_eq!(
        index_before, index_after,
        "brief must not refresh the git index"
    );
    let after = git_output(&repo, &["status", "--porcelain"]);
    assert_eq!(before, after, "brief must not change working-tree status");
}

#[test]
fn brief_output_is_byte_identical_across_runs() {
    let (_temp, repo, graph) = fixture_repo();
    write(
        &repo,
        "src/a.rs",
        "pub fn alpha() -> u64 {\n    10\n}\n\npub fn beta() -> u64 {\n    2\n}\n",
    );
    let (code1, out1) = brief(&repo, &graph, &[]);
    let (code2, out2) = brief(&repo, &graph, &[]);
    assert_eq!(code1, 0);
    assert_eq!(code2, 0);
    assert_eq!(out1, out2, "repeated runs must be byte-identical");
}

#[test]
fn brief_staged_only_restricts_to_staged_changes() {
    let (_temp, repo, graph) = fixture_repo();
    write(
        &repo,
        "src/a.rs",
        "pub fn alpha() -> u64 {\n    10\n}\n\npub fn beta() -> u64 {\n    2\n}\n",
    );
    write(&repo, "src/b.rs", "pub fn gamma() -> u64 {\n    30\n}\n");
    git(&repo, &["add", "src/a.rs"]);
    let (code, stdout) = brief(&repo, &graph, &["--staged-only"]);
    assert_eq!(code, 0);
    let result = parse_ok(&stdout);
    assert_eq!(result["staged_only"], true);
    let files = result["changed_files"].as_array().expect("array");
    let paths: Vec<&str> = files
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, vec!["src/a.rs"], "only staged changes: {paths:?}");
}

#[test]
fn brief_carries_staleness_marker_on_dirty_tree() {
    let (_temp, repo, graph) = fixture_repo();
    write(
        &repo,
        "src/a.rs",
        "pub fn alpha() -> u64 {\n    10\n}\n\npub fn beta() -> u64 {\n    2\n}\n",
    );
    let (code, stdout) = brief(&repo, &graph, &[]);
    assert_eq!(code, 0);
    let result = parse_ok(&stdout);
    let staleness = &result["staleness"];
    assert_eq!(
        staleness["freshness"], "stale_dirty",
        "dirty tree over a clean-scan store reads stale_dirty"
    );
    assert_eq!(staleness["fresh"], false);
    assert!(
        staleness["current_dirty"].as_bool().unwrap_or(false),
        "the marker reports the dirty working tree"
    );
}

#[test]
fn brief_text_format_renders_sections() {
    let (_temp, repo, graph) = fixture_repo();
    write(
        &repo,
        "src/a.rs",
        "pub fn alpha() -> u64 {\n    10\n}\n\npub fn beta() -> u64 {\n    2\n}\n",
    );
    let (code, stdout) = brief(&repo, &graph, &["--format", "text"]);
    assert_eq!(code, 0);
    for marker in [
        "working set",
        "source facts",
        "prior failures",
        "observations",
        "decisions",
        "in-flight tasks",
        "verification evidence",
        "drift warnings",
        "store coverage",
        "staleness",
    ] {
        assert!(
            stdout.contains(marker),
            "text output has {marker:?}:\n{stdout}"
        );
    }
}

#[test]
fn brief_on_non_git_directory_is_a_diagnostic_not_a_panic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("empty.graph.jsonl");
    fs::write(&graph, "").expect("write");
    let (code, stdout) = brief(temp.path(), &graph, &[]);
    assert_eq!(code, 2);
    let value: Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "not_a_git_repo");
}

#[test]
fn brief_structural_only_store_marks_memory_domains_absent() {
    // `eg scan` alone never ingests agent memory: the coverage map must report
    // those domains absent so an empty observations section reads as
    // domain-absent, not "no prior lessons".
    let (_temp, repo, graph) = fixture_repo();
    let (code, stdout) = brief(&repo, &graph, &[]);
    assert_eq!(code, 0);
    let result = parse_ok(&stdout);
    let coverage = &result["store_coverage"];
    assert_eq!(coverage["code_graph"], true);
    assert_eq!(coverage["agent_memory"], false);
    assert_eq!(coverage["project"], false);
    assert_eq!(coverage["artifact"], false);
    assert_eq!(coverage["verification"], false);
}
