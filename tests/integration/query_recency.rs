//! Integration tests for `eg query recency` (issue #219): rank indexed symbols
//! by least-recent last change (most dormant first) across a `scan-history`
//! temporal store.
#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata, scan_repository_history,
};
use assert_cmd::Command as CargoCommand;

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should be built")
}

// ── Real fixture Git repo with an engineered dormant/active symbol pair ───────

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

/// Seeds a repo where `src/dormant.rs` changes only in the first commit and
/// `src/active.rs` changes through the last commit, so dormancy ranks
/// `dormant` above `active`.
fn seed_recency_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/dormant.rs", "pub fn dormant() -> u32 { 1 }\n");
    write(repo, "src/active.rs", "pub fn active() -> u32 { 1 }\n");
    let first = commit(repo, "initial files", "2026-01-01T00:00:00Z");

    write(repo, "src/active.rs", "pub fn active() -> u32 { 2 }\n");
    let second = commit(repo, "change active", "2026-01-02T00:00:00Z");

    write(repo, "src/active.rs", "pub fn active() -> u32 { 3 }\n");
    let third = commit(repo, "change active again", "2026-01-03T00:00:00Z");

    [first, second, third]
}

fn fixture_history_graph() -> (tempfile::TempDir, PathBuf, [String; 3]) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_recency_repo(&repo);
    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let graph_path = temp.path().join("history.graph.jsonl");
    fs::write(&graph_path, &jsonl).expect("graph should be written");
    (temp, graph_path, shas)
}

fn run_recency_json(graph: &Path, extra: &[&str]) -> serde_json::Value {
    let output = egregore()
        .args(["query", "recency", "--graph"])
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
fn recency_ranks_most_dormant_symbol_first_over_fixture_history() {
    let (_temp, graph, shas) = fixture_history_graph();

    let parsed = run_recency_json(&graph, &[]);
    assert_eq!(parsed["ok"], true);
    let result = &parsed["result"];
    let symbols = result["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 2, "exactly the two indexed symbols rank");

    // Dormancy anchor is the newest indexed commit, never wall-clock: the
    // symbol untouched since the first commit is most dormant, the one changed
    // at the anchor commit has dormancy 0.
    assert_eq!(symbols[0]["rank"], 1);
    assert!(
        symbols[0]["symbol_name"]
            .as_str()
            .is_some_and(|n| n.contains("dormant")),
        "row 0 names the dormant symbol"
    );
    assert_eq!(symbols[0]["repo_relative_path"], "src/dormant.rs");
    assert_eq!(symbols[0]["last_change_commit"], shas[0].as_str());
    assert_eq!(symbols[0]["dormancy_days"], 2);
    assert_eq!(symbols[0]["dormancy_seconds"], 2 * 86_400);

    assert_eq!(symbols[1]["rank"], 2);
    assert!(
        symbols[1]["symbol_name"]
            .as_str()
            .is_some_and(|n| n.contains("active")),
        "row 1 names the active symbol"
    );
    assert_eq!(symbols[1]["repo_relative_path"], "src/active.rs");
    assert_eq!(symbols[1]["last_change_commit"], shas[2].as_str());
    assert_eq!(symbols[1]["dormancy_days"], 0);
    assert_eq!(symbols[1]["dormancy_seconds"], 0);

    // The anchor block names the newest indexed commit dormancy is measured
    // against.
    let anchors = result["anchors"].as_array().expect("anchors array");
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0]["commit_sha"], shas[2].as_str());

    // Citation completeness: every row carries the full handle set.
    for row in symbols {
        assert!(row["record_id"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(
            row["repo_relative_path"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert!(
            row["span"].is_object() || row["absent_span_reason"].as_str().is_some(),
            "each row cites a span or a documented absent-span reason"
        );
        assert!(
            row["last_change_commit"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert!(
            row["last_change_valid_time"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
    }

    assert_eq!(result["ranking_basis"], "least_recent_last_change");
    assert_eq!(result["truncated"], false);
    assert_eq!(result["total_symbol_count"], 2);
    assert_eq!(result["returned_symbol_count"], 2);
    assert!(
        result["dormancy_basis"]
            .as_str()
            .expect("dormancy_basis")
            .contains("newest indexed commit"),
        "dormancy basis must document the newest-indexed-commit anchor, never wall-clock"
    );
}

#[test]
fn recency_full_ranking_is_byte_identical_across_five_runs() {
    let (_temp, graph, _shas) = fixture_history_graph();

    let run = || {
        egregore()
            .args(["query", "recency", "--graph"])
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
            "recency ranking must be byte-identical across runs on unchanged history"
        );
    }
}

#[test]
fn recency_query_does_not_mutate_the_checkout() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_recency_repo(&repo);
    let head_before = git_output(&repo, ["rev-parse", "HEAD"]);
    let status_before = git_output(&repo, ["status", "--porcelain"]);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let graph_path = temp.path().join("history.graph.jsonl");
    fs::write(&graph_path, &jsonl).expect("graph should be written");

    egregore()
        .args(["query", "recency", "--graph"])
        .arg(&graph_path)
        .assert()
        .success();

    assert_eq!(git_output(&repo, ["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_output(&repo, ["status", "--porcelain"]), status_before);
}

// ── Synthetic graphs for ordering, tie-break, limit, scoping, honesty codes ──

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

const fn line_span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
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

/// One per-commit snapshot of a symbol. History replay emits a snapshot at
/// every commit where the symbol exists; all snapshots share the stable
/// record ID (ADR-0004) and differ only in body and commit provenance.
fn symbol_snapshot(
    id: &str,
    path: &str,
    name: &str,
    sha: &str,
    body: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(line_span(1, 1)),
        Some(name.to_owned()),
        body.to_owned(),
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

/// Four commits c1→c2→c3→c4. `old` changes only at c1, `mid` at c2, `recent`
/// at c4 (the anchor). Dormancy: old (3d) > mid (2d) > recent (0d).
#[allow(clippy::too_many_lines)]
fn ordering_fixture() -> (tempfile::TempDir, PathBuf) {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
        commit_node("commit:3", "c3", &["c2"], "2026-01-03T00:00:00Z"),
        commit_node("commit:4", "c4", &["c3"], "2026-01-04T00:00:00Z"),
        contains("repo:main", "commit:1"),
        contains("repo:main", "commit:2"),
        contains("repo:main", "commit:3"),
        contains("repo:main", "commit:4"),
        // old: unchanged after introduction at c1.
        symbol_snapshot(
            "sym:old",
            "src/old.rs",
            "old",
            "c1",
            "A",
            "2026-01-01T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:old",
            "src/old.rs",
            "old",
            "c2",
            "A",
            "2026-01-02T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:old",
            "src/old.rs",
            "old",
            "c3",
            "A",
            "2026-01-03T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:old",
            "src/old.rs",
            "old",
            "c4",
            "A",
            "2026-01-04T00:00:00Z",
        ),
        // mid: changed at c2, stable afterwards.
        symbol_snapshot(
            "sym:mid",
            "src/mid.rs",
            "mid",
            "c1",
            "A",
            "2026-01-01T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:mid",
            "src/mid.rs",
            "mid",
            "c2",
            "B",
            "2026-01-02T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:mid",
            "src/mid.rs",
            "mid",
            "c3",
            "B",
            "2026-01-03T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:mid",
            "src/mid.rs",
            "mid",
            "c4",
            "B",
            "2026-01-04T00:00:00Z",
        ),
        // recent: changed at c4 (anchor) -> dormancy 0.
        symbol_snapshot(
            "sym:recent",
            "src/recent.rs",
            "recent",
            "c1",
            "A",
            "2026-01-01T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:recent",
            "src/recent.rs",
            "recent",
            "c2",
            "A",
            "2026-01-02T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:recent",
            "src/recent.rs",
            "recent",
            "c3",
            "A",
            "2026-01-03T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:recent",
            "src/recent.rs",
            "recent",
            "c4",
            "B",
            "2026-01-04T00:00:00Z",
        ),
        contains("repo:main", "sym:old"),
        contains("repo:main", "sym:mid"),
        contains("repo:main", "sym:recent"),
    ];
    write_graph(&records)
}

#[test]
fn recency_ranks_distinct_last_change_commits_most_dormant_first() {
    let (_temp, graph) = ordering_fixture();
    let parsed = run_recency_json(&graph, &[]);
    let symbols = parsed["result"]["symbols"]
        .as_array()
        .expect("symbols array");
    assert_eq!(symbols.len(), 3);

    assert_eq!(symbols[0]["record_id"], "sym:old");
    assert_eq!(symbols[0]["dormancy_days"], 3);
    assert_eq!(symbols[0]["last_change_commit"], "c1");

    assert_eq!(symbols[1]["record_id"], "sym:mid");
    assert_eq!(symbols[1]["dormancy_days"], 2);
    assert_eq!(symbols[1]["last_change_commit"], "c2");

    // Changed at the anchor commit -> dormancy 0.
    assert_eq!(symbols[2]["record_id"], "sym:recent");
    assert_eq!(symbols[2]["dormancy_days"], 0);
    assert_eq!(symbols[2]["dormancy_seconds"], 0);
    assert_eq!(symbols[2]["last_change_commit"], "c4");

    // The anchor is the newest indexed commit, not wall-clock.
    let anchors = parsed["result"]["anchors"].as_array().expect("anchors");
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0]["commit_sha"], "c4");
    assert_eq!(anchors[0]["repository_id"], "repo:main");
}

/// Two symbols share a last-change commit (c1); a third changed at c2 is less
/// dormant. Anchor c2.
fn tie_fixture() -> (tempfile::TempDir, PathBuf) {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
        contains("repo:main", "commit:1"),
        contains("repo:main", "commit:2"),
        // sym:b and sym:a both last change at c1 (tie); path breaks the tie.
        symbol_snapshot("sym:b", "src/b.rs", "b", "c1", "A", "2026-01-01T00:00:00Z"),
        symbol_snapshot("sym:b", "src/b.rs", "b", "c2", "A", "2026-01-02T00:00:00Z"),
        symbol_snapshot("sym:a", "src/a.rs", "a", "c1", "A", "2026-01-01T00:00:00Z"),
        symbol_snapshot("sym:a", "src/a.rs", "a", "c2", "A", "2026-01-02T00:00:00Z"),
        // sym:z changed at c2 -> less dormant, ranks last.
        symbol_snapshot("sym:z", "src/z.rs", "z", "c1", "A", "2026-01-01T00:00:00Z"),
        symbol_snapshot("sym:z", "src/z.rs", "z", "c2", "B", "2026-01-02T00:00:00Z"),
        contains("repo:main", "sym:a"),
        contains("repo:main", "sym:b"),
        contains("repo:main", "sym:z"),
    ];
    write_graph(&records)
}

#[test]
fn recency_ties_break_on_path_then_record_id_ascending() {
    let (_temp, graph) = tie_fixture();
    let parsed = run_recency_json(&graph, &[]);
    let symbols = parsed["result"]["symbols"]
        .as_array()
        .expect("symbols array");
    assert_eq!(symbols.len(), 3);
    // Equal dormancy + shared last-change commit: documented tie-break on path.
    assert_eq!(symbols[0]["repo_relative_path"], "src/a.rs");
    assert_eq!(symbols[0]["dormancy_days"], 1);
    assert_eq!(symbols[1]["repo_relative_path"], "src/b.rs");
    assert_eq!(symbols[1]["dormancy_days"], 1);
    // Less dormant symbol ranks last.
    assert_eq!(symbols[2]["repo_relative_path"], "src/z.rs");
    assert_eq!(symbols[2]["dormancy_days"], 0);
}

#[test]
fn recency_limit_truncates_and_reports_completeness() {
    let (_temp, graph) = ordering_fixture();

    let parsed = run_recency_json(&graph, &["--limit", "1"]);
    let result = &parsed["result"];
    assert_eq!(result["limit"], 1);
    assert_eq!(result["total_symbol_count"], 3);
    assert_eq!(result["returned_symbol_count"], 1);
    assert_eq!(result["truncated"], true);
    let symbols = result["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["record_id"], "sym:old");

    let parsed_default = run_recency_json(&graph, &[]);
    assert_eq!(parsed_default["result"]["limit"], 50);
    assert_eq!(parsed_default["result"]["truncated"], false);
}

#[test]
fn recency_rejects_out_of_range_limit() {
    let (_temp, graph) = ordering_fixture();
    for bad_limit in ["0", "501"] {
        let output = egregore()
            .args(["query", "recency", "--graph"])
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
fn recency_without_history_commits_reports_no_history() {
    // A current-tree-only scan: Symbol nodes carry node-level valid time but no
    // commit provenance, and no Commit node exists. Recency must report
    // UNAVAILABLE rather than imply every symbol is brand-new (AC5).
    let records = vec![
        repo_node("repo:main", "main-repo"),
        GraphRecord::node(
            "sym:a".to_owned(),
            NodeKind::Symbol,
            Some("src/a.rs".to_owned()),
            Some(line_span(1, 1)),
            Some("a".to_owned()),
            "A".to_owned(),
        ),
        contains("repo:main", "sym:a"),
    ];
    let (_temp, graph) = write_graph(&records);

    let output = egregore()
        .args(["query", "recency", "--graph"])
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
fn recency_with_commits_but_no_symbols_reports_no_match() {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        contains("repo:main", "commit:1"),
    ];
    let (_temp, graph) = write_graph(&records);

    let output = egregore()
        .args(["query", "recency", "--graph"])
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

/// Two repositories with independent histories and anchors. `main` has two
/// commits (m1→m2); `sibling` has one (s1).
fn two_repo_fixture() -> (tempfile::TempDir, PathBuf) {
    let records = vec![
        repo_node("repo:main", "main-repo"),
        repo_node("repo:sibling", "sibling-repo"),
        commit_node("commit:m1", "m1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:m2", "m2", &["m1"], "2026-01-02T00:00:00Z"),
        commit_node("commit:s1", "s1", &[], "2026-01-05T00:00:00Z"),
        contains("repo:main", "commit:m1"),
        contains("repo:main", "commit:m2"),
        contains("repo:sibling", "commit:s1"),
        // main symbol: last change m1, anchor m2 -> dormancy 1 day.
        symbol_snapshot(
            "sym:main-foo",
            "src/foo.rs",
            "foo",
            "m1",
            "A",
            "2026-01-01T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:main-foo",
            "src/foo.rs",
            "foo",
            "m2",
            "A",
            "2026-01-02T00:00:00Z",
        ),
        // sibling symbol: introduced s1, anchor s1 -> dormancy 0.
        symbol_snapshot(
            "sym:sib-bar",
            "src/bar.rs",
            "bar",
            "s1",
            "A",
            "2026-01-05T00:00:00Z",
        ),
        contains("repo:main", "sym:main-foo"),
        contains("repo:sibling", "sym:sib-bar"),
    ];
    write_graph(&records)
}

#[test]
fn recency_repo_scope_restricts_rows_and_uses_per_repo_anchor() {
    let (_temp, graph) = two_repo_fixture();

    // Scoped to sibling: only its symbol, anchored on its own newest commit.
    let parsed = run_recency_json(&graph, &["--repo", "sibling-repo"]);
    let result = &parsed["result"];
    let symbols = result["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["record_id"], "sym:sib-bar");
    assert_eq!(symbols[0]["repository_id"], "repo:sibling");
    assert_eq!(symbols[0]["dormancy_days"], 0);
    let anchors = result["anchors"].as_array().expect("anchors");
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0]["commit_sha"], "s1");
    // No cross-repo bleed: main's symbol must be absent.
    let rendered = serde_json::to_string(&parsed).expect("serialize");
    assert!(!rendered.contains("sym:main-foo"));
    assert!(!rendered.contains("src/foo.rs"));

    // Scoped to main: per-repo anchor is m2 (not sibling's later s1), so its
    // dormancy is measured within its own history.
    let parsed_main = run_recency_json(&graph, &["--repo", "main-repo"]);
    let symbols_main = parsed_main["result"]["symbols"]
        .as_array()
        .expect("symbols array");
    assert_eq!(symbols_main.len(), 1);
    assert_eq!(symbols_main[0]["record_id"], "sym:main-foo");
    assert_eq!(symbols_main[0]["dormancy_days"], 1);
    let anchors_main = parsed_main["result"]["anchors"]
        .as_array()
        .expect("anchors");
    assert_eq!(anchors_main[0]["commit_sha"], "m2");

    // Unscoped: both repositories' symbols appear, each with its own anchor.
    let parsed_all = run_recency_json(&graph, &[]);
    let symbols_all = parsed_all["result"]["symbols"].as_array().expect("symbols");
    assert_eq!(symbols_all.len(), 2);
    let anchors_all = parsed_all["result"]["anchors"].as_array().expect("anchors");
    assert_eq!(anchors_all.len(), 2);
}

#[test]
fn recency_unknown_repo_selector_exits_1() {
    let (_temp, graph) = two_repo_fixture();
    let output = egregore()
        .args(["query", "recency", "--graph"])
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
fn recency_excludes_tombstoned_symbols() {
    // A symbol deleted from the current tree is GONE, not dormant.
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:sym-gone".to_owned(),
        schema_version: 4,
        deleted_id: "sym:gone".to_owned(),
        summary: "symbol gone".to_owned(),
        producer: None,
    };
    let records = vec![
        repo_node("repo:main", "main-repo"),
        commit_node("commit:1", "c1", &[], "2026-01-01T00:00:00Z"),
        commit_node("commit:2", "c2", &["c1"], "2026-01-02T00:00:00Z"),
        contains("repo:main", "commit:1"),
        contains("repo:main", "commit:2"),
        symbol_snapshot(
            "sym:live",
            "src/live.rs",
            "live",
            "c1",
            "A",
            "2026-01-01T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:live",
            "src/live.rs",
            "live",
            "c2",
            "A",
            "2026-01-02T00:00:00Z",
        ),
        symbol_snapshot(
            "sym:gone",
            "src/gone.rs",
            "gone",
            "c1",
            "A",
            "2026-01-01T00:00:00Z",
        ),
        contains("repo:main", "sym:live"),
        contains("repo:main", "sym:gone"),
        tombstone,
    ];
    let (_temp, graph) = write_graph(&records);

    let parsed = run_recency_json(&graph, &[]);
    let symbols = parsed["result"]["symbols"]
        .as_array()
        .expect("symbols array");
    assert_eq!(symbols.len(), 1, "tombstoned symbols never rank");
    assert_eq!(symbols[0]["record_id"], "sym:live");
    let rendered = serde_json::to_string(&parsed).expect("serialize");
    assert!(!rendered.contains("src/gone.rs"));
}

#[test]
fn recency_text_format_lists_ranking() {
    let (_temp, graph) = ordering_fixture();
    let output = egregore()
        .args(["query", "recency", "--graph"])
        .arg(&graph)
        .args(["--format", "text", "--limit", "2"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout should be utf-8");
    assert!(stdout.contains("src/old.rs"));
    assert!(stdout.contains("dormant=3d"));
    assert!(stdout.contains("anchor"));
    assert!(
        stdout.contains("truncated"),
        "text output must state the ranking was truncated"
    );
    assert!(
        !stdout.contains("src/recent.rs"),
        "rows past the limit must not leak into text output"
    );
}
