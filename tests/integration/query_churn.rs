//! Integration tests for `eg query churn` (issue #128): rank files by change
//! frequency across a `scan-history` temporal store.
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, TemporalMetadata, scan_repository_history, stable_id,
};
use assert_cmd::Command as CargoCommand;

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should be built")
}

// ── Real fixture Git repo with one engineered high-churn file ────────────────

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

/// Seeds a repo whose churn ranking is engineered: `src/hot.rs` changes in 4
/// commits, `src/warm.rs` in 2, `src/cold.rs` in 1. A committed non-source
/// file, a gitignored file, and an untracked file must never appear.
fn seed_churn_repo(repo: &Path) -> [String; 4] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, ".gitignore", "scratch/\n");
    write(repo, "README.md", "# fixture\n");
    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 1 }\n");
    write(repo, "src/warm.rs", "pub fn warm() -> u32 { 1 }\n");
    write(repo, "src/cold.rs", "pub fn cold() -> u32 { 1 }\n");
    let first = commit(repo, "initial files", "2026-01-01T00:00:00Z");

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 2 }\n");
    write(repo, "src/warm.rs", "pub fn warm() -> u32 { 2 }\n");
    let second = commit(repo, "change hot and warm", "2026-01-02T00:00:00Z");

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 3 }\n");
    let third = commit(repo, "change hot again", "2026-01-03T00:00:00Z");

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 4 }\n");
    let fourth = commit(repo, "change hot a third time", "2026-01-04T00:00:00Z");

    // Ignored and untracked paths: never committed, must never appear.
    write(
        repo,
        "scratch/ignored.rs",
        "pub fn ignored() -> u32 { 1 }\n",
    );
    write(repo, "untracked.rs", "pub fn untracked() -> u32 { 1 }\n");

    [first, second, third, fourth]
}

fn fixture_history_graph() -> (tempfile::TempDir, PathBuf, [String; 4], String) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_churn_repo(&repo);
    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let graph_path = temp.path().join("history.graph.jsonl");
    fs::write(&graph_path, &jsonl).expect("graph should be written");
    // Repo has no remote; identity is derived from the root commit SHA.
    let repository_id = stable_id(&["repository", "local-root-commit", &shas[0]]);
    (temp, graph_path, shas, repository_id)
}

fn run_churn_json(graph: &Path, extra: &[&str]) -> serde_json::Value {
    let output = egregore()
        .args(["query", "churn", "--graph"])
        .arg(graph)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf-8");
    serde_json::from_str(stdout.trim()).expect("stdout should be one JSON envelope")
}

#[test]
fn churn_ranks_engineered_high_churn_file_first_over_fixture_history() {
    let (_temp, graph, shas, repository_id) = fixture_history_graph();

    let parsed = run_churn_json(&graph, &[]);
    assert_eq!(parsed["ok"], true);
    let result = &parsed["result"];
    let files = result["files"].as_array().expect("files array");
    assert_eq!(
        files.len(),
        3,
        "exactly the three indexed source files rank"
    );

    // Engineered ordering: hot (4) > warm (2) > cold (1).
    assert_eq!(files[0]["rank"], 1);
    assert_eq!(files[0]["repo_relative_path"], "src/hot.rs");
    assert_eq!(files[0]["commit_count"], 4);
    assert_eq!(files[1]["rank"], 2);
    assert_eq!(files[1]["repo_relative_path"], "src/warm.rs");
    assert_eq!(files[1]["commit_count"], 2);
    assert_eq!(files[2]["rank"], 3);
    assert_eq!(files[2]["repo_relative_path"], "src/cold.rs");
    assert_eq!(files[2]["commit_count"], 1);

    // Every row resolves to an existing File node in the store (0 dangling).
    let graph_text = fs::read_to_string(&graph).expect("graph should read");
    for row in files {
        let file_id = row["file_record_id"].as_str().expect("file_record_id");
        let path = row["repo_relative_path"].as_str().expect("path");
        let expected_id = stable_id(&["node", "file", &repository_id, path]);
        assert_eq!(file_id, expected_id, "handle must be the File node ID");
        let is_file_node = graph_text.lines().any(|line| {
            let record: serde_json::Value = serde_json::from_str(line).expect("valid JSONL");
            record["id"] == file_id && record["kind"] == "File"
        });
        assert!(
            is_file_node,
            "row handle {file_id} must resolve to a File node"
        );
        // Rows carry the inclusive commit range used.
        assert_eq!(row["first_commit"], shas[0].as_str());
        assert_eq!(row["last_commit"], shas[3].as_str());
        assert_eq!(row["repository_id"], repository_id.as_str());
    }

    // Non-source, ignored, and untracked paths never appear.
    let rendered = serde_json::to_string(&parsed).expect("serialize");
    assert!(!rendered.contains("README.md"));
    assert!(!rendered.contains("scratch/ignored.rs"));
    assert!(!rendered.contains("untracked.rs"));

    // The envelope states the inclusive commit range/scope used.
    let ranges = result["commit_ranges"].as_array().expect("commit_ranges");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0]["repository_id"], repository_id.as_str());
    assert_eq!(ranges[0]["first_commit"], shas[0].as_str());
    assert_eq!(ranges[0]["last_commit"], shas[3].as_str());
    assert_eq!(ranges[0]["commit_count"], 4);

    // Completeness signal: nothing truncated at the default limit.
    assert_eq!(result["truncated"], false);
    assert_eq!(result["total_file_count"], 3);
    assert_eq!(result["returned_file_count"], 3);
    assert_eq!(result["ranking_basis"], "distinct_commit_count");
    assert_eq!(result["tie_break"], "repo_relative_path");
}

#[test]
fn churn_full_ranking_is_byte_identical_across_five_runs() {
    let (_temp, graph, _shas, _repo_id) = fixture_history_graph();

    let run = || {
        egregore()
            .args(["query", "churn", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let first = run();
    for _ in 0..4 {
        assert_eq!(
            first,
            run(),
            "churn ranking must be byte-identical across runs on unchanged history"
        );
    }
}

#[test]
fn churn_query_does_not_mutate_the_checkout() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_churn_repo(&repo);
    let head_before = git_output(&repo, ["rev-parse", "HEAD"]);
    let status_before = git_output(&repo, ["status", "--porcelain"]);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let graph_path = temp.path().join("history.graph.jsonl");
    fs::write(&graph_path, &jsonl).expect("graph should be written");

    egregore()
        .args(["query", "churn", "--graph"])
        .arg(&graph_path)
        .assert()
        .success();

    assert_eq!(git_output(&repo, ["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_output(&repo, ["status", "--porcelain"]), status_before);
}

// ── Synthetic graphs for tie-break, limit, scoping, and honesty codes ────────

fn temporal_with_parents(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|&s| s.to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: None,
    }
}

fn repo_node(id: &str, name: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some(name.to_owned()),
        format!("Repository {name}"),
    )
}

fn commit_node(id: &str, sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("commit {sha}"),
    )
    .with_temporal(temporal_with_parents(sha, parents, valid_time))
}

fn file_node(id: &str, path: &str, sha: &str, valid_time: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("source file {path}"),
    )
    .with_temporal(temporal_with_parents(sha, &[], valid_time))
}

fn contains(source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Contains,
        source.to_owned(),
        target.to_owned(),
        None,
        String::new(),
    )
}

fn changed_in(source: &str, target: &str, sha: &str, valid_time: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::ChangedIn,
        source.to_owned(),
        target.to_owned(),
        None,
        String::new(),
    )
    .with_temporal(temporal_with_parents(sha, &[], valid_time))
}

fn write_graph(records: &[GraphRecord]) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let content = records
        .iter()
        .map(|r| serde_json::to_string(r).expect("record should serialize"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&graph_path, content).expect("graph should be written");
    (temp, graph_path)
}

/// Three files across three commits: `src/a.rs` and `src/b.rs` both change in
/// two commits (tie), `src/z.rs` changes in three.
fn tie_fixture() -> (tempfile::TempDir, PathBuf) {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
        commit_node("commit:3", "c3", &["c2"], "2026-01-03T00:00:00Z"),
        contains("repo:main", "commit:1"),
        contains("repo:main", "commit:2"),
        contains("repo:main", "commit:3"),
        file_node("file:a", "src/a.rs", "c1", "2026-01-01T00:00:00Z"),
        file_node("file:b", "src/b.rs", "c1", "2026-01-01T00:00:00Z"),
        file_node("file:z", "src/z.rs", "c1", "2026-01-01T00:00:00Z"),
        contains("repo:main", "file:a"),
        contains("repo:main", "file:b"),
        contains("repo:main", "file:z"),
        changed_in("file:a", "commit:1", "c1", "2026-01-01T00:00:00Z"),
        changed_in("file:b", "commit:1", "c1", "2026-01-01T00:00:00Z"),
        changed_in("file:z", "commit:1", "c1", "2026-01-01T00:00:00Z"),
        changed_in("file:a", "commit:2", "c2", "2026-01-02T00:00:00Z"),
        changed_in("file:b", "commit:2", "c2", "2026-01-02T00:00:00Z"),
        changed_in("file:z", "commit:2", "c2", "2026-01-02T00:00:00Z"),
        changed_in("file:z", "commit:3", "c3", "2026-01-03T00:00:00Z"),
    ];
    write_graph(&records)
}

#[test]
fn churn_ties_break_on_repo_relative_path_ascending() {
    let (_temp, graph) = tie_fixture();
    let parsed = run_churn_json(&graph, &[]);
    let files = parsed["result"]["files"].as_array().expect("files array");
    assert_eq!(files.len(), 3);
    assert_eq!(files[0]["repo_relative_path"], "src/z.rs");
    assert_eq!(files[0]["commit_count"], 3);
    // Equal counts: documented stable tie-break on repo-relative path.
    assert_eq!(files[1]["repo_relative_path"], "src/a.rs");
    assert_eq!(files[1]["commit_count"], 2);
    assert_eq!(files[2]["repo_relative_path"], "src/b.rs");
    assert_eq!(files[2]["commit_count"], 2);
}

#[test]
fn churn_limit_truncates_and_reports_completeness() {
    let (_temp, graph) = tie_fixture();

    let parsed = run_churn_json(&graph, &["--limit", "1"]);
    let result = &parsed["result"];
    assert_eq!(result["limit"], 1);
    assert_eq!(result["total_file_count"], 3);
    assert_eq!(result["returned_file_count"], 1);
    assert_eq!(result["truncated"], true);
    let files = result["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["repo_relative_path"], "src/z.rs");

    let parsed_default = run_churn_json(&graph, &[]);
    assert_eq!(parsed_default["result"]["limit"], 50);
    assert_eq!(parsed_default["result"]["truncated"], false);
}

#[test]
fn churn_rejects_out_of_range_limit() {
    let (_temp, graph) = tie_fixture();
    for bad_limit in ["0", "501"] {
        let output = egregore()
            .args(["query", "churn", "--graph"])
            .arg(&graph)
            .args(["--limit", bad_limit])
            .assert()
            .code(1)
            .get_output()
            .stderr
            .clone();
        let stderr = String::from_utf8(output).expect("stderr should be utf-8");
        let diag: serde_json::Value =
            serde_json::from_str(stderr.trim()).expect("stderr should be machine-readable JSON");
        assert_eq!(diag["code"], "invalid_limit");
        assert_eq!(diag["max"], 500);
    }
}

#[test]
fn churn_without_history_commits_reports_no_history() {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        GraphRecord::node(
            "file:a".to_owned(),
            NodeKind::File,
            Some("src/a.rs".to_owned()),
            None,
            Some("src/a.rs".to_owned()),
            "source file src/a.rs".to_owned(),
        ),
        contains("repo:main", "file:a"),
    ];
    let (_temp, graph) = write_graph(&records);

    let output = egregore()
        .args(["query", "churn", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf-8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("envelope");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"], "no_history");
}

#[test]
fn churn_with_commits_but_no_file_changes_reports_no_match() {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        contains("repo:main", "commit:1"),
    ];
    let (_temp, graph) = write_graph(&records);

    let output = egregore()
        .args(["query", "churn", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf-8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("envelope");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"], "no_match");
}

fn two_repo_fixture() -> (tempfile::TempDir, PathBuf) {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        repo_node("repo:sibling", "sibling-repo"),
        commit_node("commit:m1", "m1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:m2", "m2", &["m1"], "2026-01-02T00:00:00Z"),
        commit_node("commit:s1", "s1", &[], "2026-01-01T00:00:00Z"),
        contains("repo:main", "commit:m1"),
        contains("repo:main", "commit:m2"),
        contains("repo:sibling", "commit:s1"),
        file_node("file:main-lib", "src/lib.rs", "m1", "2026-01-01T00:00:00Z"),
        file_node("file:sib-lib", "src/lib.rs", "s1", "2026-01-01T00:00:00Z"),
        contains("repo:main", "file:main-lib"),
        contains("repo:sibling", "file:sib-lib"),
        changed_in("file:main-lib", "commit:m1", "m1", "2026-01-01T00:00:00Z"),
        changed_in("file:main-lib", "commit:m2", "m2", "2026-01-02T00:00:00Z"),
        changed_in("file:sib-lib", "commit:s1", "s1", "2026-01-01T00:00:00Z"),
    ];
    write_graph(&records)
}

#[test]
fn churn_repo_scope_restricts_rows_and_ranges() {
    let (_temp, graph) = two_repo_fixture();

    // Scoped: only the sibling repository's file and range appear.
    let parsed = run_churn_json(&graph, &["--repo", "sibling-repo"]);
    let result = &parsed["result"];
    let files = result["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["file_record_id"], "file:sib-lib");
    assert_eq!(files[0]["repository_id"], "repo:sibling");
    assert_eq!(files[0]["commit_count"], 1);
    let ranges = result["commit_ranges"].as_array().expect("commit_ranges");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0]["repository_id"], "repo:sibling");

    // Unscoped: rows from both repositories side by side, never merged.
    let parsed_all = run_churn_json(&graph, &[]);
    let files_all = parsed_all["result"]["files"].as_array().expect("files");
    assert_eq!(files_all.len(), 2);
    assert_eq!(files_all[0]["file_record_id"], "file:main-lib");
    assert_eq!(files_all[0]["commit_count"], 2);
    assert_eq!(files_all[1]["file_record_id"], "file:sib-lib");
    assert_eq!(files_all[1]["commit_count"], 1);
    let ranges_all = parsed_all["result"]["commit_ranges"]
        .as_array()
        .expect("commit_ranges");
    assert_eq!(ranges_all.len(), 2);
}

#[test]
fn churn_unknown_repo_selector_exits_1() {
    let (_temp, graph) = two_repo_fixture();
    let output = egregore()
        .args(["query", "churn", "--graph"])
        .arg(&graph)
        .args(["--repo", "does-not-exist"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).expect("stderr should be utf-8");
    let diag: serde_json::Value = serde_json::from_str(stderr.trim()).expect("diagnostic JSON");
    assert_eq!(diag["code"], "unknown_repository_selector");
}

#[test]
fn churn_excludes_tombstoned_files_and_symbol_edges() {
    let symbol = GraphRecord::node(
        "symbol:answer".to_owned(),
        NodeKind::Symbol,
        Some("src/a.rs".to_owned()),
        None,
        Some("answer".to_owned()),
        "answer body".to_owned(),
    )
    .with_temporal(temporal_with_parents("c1", &[], "2026-01-01T00:00:00Z"));
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:file-gone".to_owned(),
        schema_version: 4,
        deleted_id: "file:gone".to_owned(),
        summary: "file gone".to_owned(),
        producer: None,
    };
    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
        contains("repo:main", "commit:1"),
        contains("repo:main", "commit:2"),
        file_node("file:a", "src/a.rs", "c1", "2026-01-01T00:00:00Z"),
        file_node("file:gone", "src/gone.rs", "c1", "2026-01-01T00:00:00Z"),
        contains("repo:main", "file:a"),
        contains("repo:main", "file:gone"),
        symbol,
        contains("repo:main", "symbol:answer"),
        changed_in("file:a", "commit:1", "c1", "2026-01-01T00:00:00Z"),
        changed_in("file:gone", "commit:1", "c1", "2026-01-01T00:00:00Z"),
        changed_in("file:gone", "commit:2", "c2", "2026-01-02T00:00:00Z"),
        changed_in("symbol:answer", "commit:1", "c1", "2026-01-01T00:00:00Z"),
        tombstone,
    ];
    let (_temp, graph) = write_graph(&records);

    let parsed = run_churn_json(&graph, &[]);
    let files = parsed["result"]["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1, "tombstoned file and symbols never rank");
    assert_eq!(files[0]["file_record_id"], "file:a");
}

#[test]
fn churn_text_format_lists_ranking() {
    let (_temp, graph) = tie_fixture();
    let output = egregore()
        .args(["query", "churn", "--graph"])
        .arg(&graph)
        .args(["--format", "text", "--limit", "2"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf-8");
    assert!(stdout.contains("src/z.rs"));
    assert!(stdout.contains("commits=3"));
    assert!(stdout.contains("src/a.rs"));
    assert!(
        stdout.contains("truncated"),
        "text output must state the ranking was truncated"
    );
    assert!(
        !stdout.contains("src/b.rs"),
        "rows past the limit must not leak into text output"
    );
}

/// A `CHANGED_IN` edge without temporal provenance (the retractable class per
/// `eg forget`: commit-anchored edges stay retractable and the edge read path
/// must honor tombstones).
fn changed_in_untemporal(source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::ChangedIn,
        source.to_owned(),
        target.to_owned(),
        None,
        String::new(),
    )
}

fn tombstone_of(record: &GraphRecord, tombstone_id: &str) -> GraphRecord {
    GraphRecord::Tombstone {
        id: tombstone_id.to_owned(),
        schema_version: 4,
        deleted_id: record.id().to_owned(),
        summary: format!("retracted {}", record.id()),
        producer: None,
    }
}

#[test]
fn churn_excludes_retracted_change_edges_but_keeps_historical_provenance() {
    // file:a's only change marker is a retracted (tombstoned, non-temporal)
    // edge: it must not rank at all.
    let retracted_only = changed_in_untemporal("file:a", "commit:1");
    let retracted_only_tombstone = tombstone_of(&retracted_only, "tombstone:edge-a");
    // file:b has one live temporal edge and one retracted non-temporal edge:
    // the retracted edge must not inflate the count.
    let b_live = changed_in("file:b", "commit:1", "c1", "2026-01-01T00:00:00Z");
    let b_retracted = changed_in_untemporal("file:b", "commit:2");
    let b_retracted_tombstone = tombstone_of(&b_retracted, "tombstone:edge-b");
    // file:c's change edge carries temporal provenance; a current-state
    // tombstone on its stable ID does not erase the historical fact
    // (convention shared with `eg query changes` / `eg query subsystem`).
    let c_temporal = changed_in("file:c", "commit:1", "c1", "2026-01-01T00:00:00Z");
    let c_tombstone = tombstone_of(&c_temporal, "tombstone:edge-c");

    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
        contains("repo:main", "commit:1"),
        contains("repo:main", "commit:2"),
        file_node("file:a", "src/a.rs", "c1", "2026-01-01T00:00:00Z"),
        file_node("file:b", "src/b.rs", "c1", "2026-01-01T00:00:00Z"),
        file_node("file:c", "src/c.rs", "c1", "2026-01-01T00:00:00Z"),
        contains("repo:main", "file:a"),
        contains("repo:main", "file:b"),
        contains("repo:main", "file:c"),
        retracted_only,
        retracted_only_tombstone,
        b_live,
        b_retracted,
        b_retracted_tombstone,
        c_temporal,
        c_tombstone,
    ];
    let (_temp, graph) = write_graph(&records);

    let parsed = run_churn_json(&graph, &[]);
    let files = parsed["result"]["files"].as_array().expect("files array");
    assert_eq!(
        files.len(),
        2,
        "a file whose only change marker was retracted must not rank"
    );
    assert_eq!(files[0]["file_record_id"], "file:b");
    assert_eq!(
        files[0]["commit_count"], 1,
        "a retracted change edge must not inflate the commit count"
    );
    assert_eq!(files[1]["file_record_id"], "file:c");
    assert_eq!(
        files[1]["commit_count"], 1,
        "a temporal change edge keeps its historical provenance despite a tombstone"
    );
    let rendered = serde_json::to_string(&parsed).expect("serialize");
    assert!(
        !rendered.contains("src/a.rs"),
        "fully retracted file must not appear anywhere in the answer"
    );
}
