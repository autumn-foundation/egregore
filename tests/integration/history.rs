#![allow(missing_docs)]

use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{scan_repository_history, stable_id};
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

    CargoCommand::cargo_bin("egregore")
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

#[test]
fn changed_in_commit_edges_only_target_changed_paths() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_two_file_history_repo(&repo);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);
    // Repo has no remote; identity is derived from the root commit SHA (= first commit).
    let repository_id = stable_id(&["repository", "local-root-commit", &first]);
    let second_commit_id = stable_id(&["node", "commit", &repository_id, &second]);
    let changed_sources = changed_in_sources_targeting(&records, &second_commit_id);

    assert_path_has_changed_source(&records, &changed_sources, "src/a.rs");
    assert_path_has_no_changed_source(&records, &changed_sources, "src/b.rs");
}

#[test]
fn merge_commits_with_rust_resolutions_keep_commit_changed_in_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let merge = seed_conflict_resolution_merge_repo(&repo);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);
    // Repo has no remote; identity is derived from the root commit SHA.
    let root_sha = git_root_sha(&repo);
    let repository_id = stable_id(&["repository", "local-root-commit", &root_sha]);
    let merge_commit_id = stable_id(&["node", "commit", &repository_id, &merge]);
    let changed_sources = changed_in_sources_targeting(&records, &merge_commit_id);

    assert_path_has_changed_source(&records, &changed_sources, "src/lib.rs");
}

/// Issue #438: history replay must not hard-abort on a non-UTF-8 committed
/// source blob. It skips the undecodable blob, emits a deterministic
/// `Diagnostic` naming the commit + path, and keeps replaying every decodable
/// file across the history. `git show` reads objects only, never mutating the
/// checkout.
#[test]
fn scan_history_skips_non_utf8_blob_and_records_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(&repo, ["config", "user.name", "Codegraph Test"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    write(&repo, "src/lib.rs", "pub fn good() -> u32 { 1 }\n");
    // A committed `.rs` blob whose bytes are UTF-16LE (BOM 0xFF 0xFE then LE
    // code units) — genuine text, never valid UTF-8.
    let mut bad = vec![0xFF, 0xFE];
    for unit in "pub fn hidden() {}".encode_utf16() {
        bad.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(repo.join("src/bad.rs"), &bad).expect("bad blob should be written");
    let commit_sha = commit(
        &repo,
        "add source and non-utf8 blob",
        "2026-04-01T00:00:00Z",
    );

    let head_before = git_output(&repo, ["rev-parse", "HEAD"]);

    let jsonl = scan_repository_history(&repo)
        .expect("history replay must complete over a non-UTF-8 blob")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);

    // The decodable file is still extracted across the history.
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Symbol"
                && r["repo_relative_path"] == "src/lib.rs"
                && r["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with("good"))
        }),
        "the decodable blob's symbol must be extracted"
    );
    // A Diagnostic names the skipped blob with a non-UTF-8 reason.
    let diagnostic = records.iter().find(|r| {
        r["record_type"] == "node"
            && r["kind"] == "Diagnostic"
            && r["repo_relative_path"] == "src/bad.rs"
    });
    let diagnostic = diagnostic.expect("a Diagnostic must name the skipped non-UTF-8 blob");
    assert!(
        diagnostic["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("UTF-8"),
        "diagnostic must state the non-UTF-8 decode failure: {diagnostic}"
    );
    // The blob's own bytes never enter the graph.
    assert!(
        !jsonl.contains("hidden"),
        "raw blob text must never enter the graph"
    );
    // The checkout is untouched (git show reads objects only).
    assert_eq!(git_output(&repo, ["rev-parse", "HEAD"]), head_before);
    let _ = commit_sha;
}

fn seed_history_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

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

fn seed_two_file_history_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/a.rs", "pub fn a() -> u32 { 1 }\n");
    write(repo, "src/b.rs", "pub fn b() -> u32 { 1 }\n");
    let first = commit(repo, "initial files", "2026-02-01T00:00:00Z");

    write(repo, "src/a.rs", "pub fn a() -> u32 { 2 }\n");
    let second = commit(repo, "change only a", "2026-02-02T00:00:00Z");

    [first, second]
}

fn seed_conflict_resolution_merge_repo(repo: &Path) -> String {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/lib.rs", "pub fn value() -> u32 { 1 }\n");
    commit(repo, "base value", "2026-03-01T00:00:00Z");
    let base_branch = git_output(repo, ["branch", "--show-current"]);

    git(repo, ["checkout", "-b", "feature"]);
    write(repo, "src/lib.rs", "pub fn value() -> u32 { 2 }\n");
    commit(repo, "feature value", "2026-03-02T00:00:00Z");

    git(repo, ["checkout", &base_branch]);
    write(repo, "src/lib.rs", "pub fn value() -> u32 { 3 }\n");
    commit(repo, "main value", "2026-03-03T00:00:00Z");

    let merge = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge", "feature"])
        .stdin(Stdio::null())
        .output()
        .expect("git merge should execute");
    assert!(
        !merge.status.success(),
        "merge should require a conflict resolution\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );

    write(repo, "src/lib.rs", "pub fn value() -> u32 { 4 }\n");
    commit(repo, "merge resolved value", "2026-03-04T00:00:00Z")
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

fn git_root_sha(repo: &Path) -> String {
    git_output(repo, ["rev-list", "--max-parents=0", "HEAD"])
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

fn changed_in_sources_targeting(records: &[Value], target_id: &str) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["label"] == "CHANGED_IN"
                && record["target"] == target_id
        })
        .filter_map(|record| record["source"].as_str().map(ToOwned::to_owned))
        .collect()
}

fn assert_path_has_changed_source(
    records: &[Value],
    changed_sources: &BTreeSet<String>,
    path: &str,
) {
    assert!(
        path_changed_sources(records, changed_sources, path).count() > 0,
        "expected {path} to be attributed to the changed commit"
    );
}

fn assert_path_has_no_changed_source(
    records: &[Value],
    changed_sources: &BTreeSet<String>,
    path: &str,
) {
    let unexpected = path_changed_sources(records, changed_sources, path)
        .map(|record| format!("{} {}", record["kind"], record["name"]))
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "unchanged path {path} was attributed to the commit: {unexpected:?}"
    );
}

fn path_changed_sources<'a>(
    records: &'a [Value],
    changed_sources: &'a BTreeSet<String>,
    path: &'a str,
) -> impl Iterator<Item = &'a Value> {
    records.iter().filter(move |record| {
        record["record_type"] == "node"
            && matches!(record["kind"].as_str(), Some("File" | "Symbol"))
            && record["repo_relative_path"] == path
            && record["id"]
                .as_str()
                .is_some_and(|id| changed_sources.contains(id))
    })
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
                && record["author_name"] == "Codegraph Test"
                && record["author_email"] == "codegraph@example.invalid"
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
