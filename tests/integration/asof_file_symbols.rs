//! Integration tests for `eg query file --at/--as-of` (issue #158):
//! reconstructing a file's defined-symbol set at a past commit or instant.
#![allow(missing_docs)]

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{
    GraphRecord, NodeKind, TemporalMetadata,
    query::{FileAtPointError, FileAtPointSelector, file_symbols_at_point},
    stable_id,
};
use assert_cmd::Command as CargoCommand;

// ---------------------------------------------------------------------------
// Synthetic record helpers (mirrors tests/integration/range_deltas.rs)
// ---------------------------------------------------------------------------

fn temporal(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

fn commit(sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "commit", "repo_test", sha]);
    GraphRecord::node(
        id,
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Commit {sha}"),
    )
    .with_temporal(temporal(sha, parents, valid_time))
}

fn file_snapshot(path: &str, body: &str, commit: &str, valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "file", "repo_test", path]);
    GraphRecord::node(
        id,
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("Rust source file {path}\nSource:\n{body}"),
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

fn symbol_snapshot(
    name: &str,
    path: &str,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    let id = stable_id(&["node", "symbol", "repo_test", path, name]);
    GraphRecord::node(
        id,
        NodeKind::Symbol,
        Some(path.to_owned()),
        None,
        Some(name.to_owned()),
        format!("Symbol {name} in {path}\nSource:\n{body}"),
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const T3: &str = "2026-01-03T00:00:00Z";

/// Issue #158 acceptance fixture: file `F` (`src/f.rs`) defines {A,B} at C1,
/// adds C at C2, and removes B at C3 (tombstoned by snapshot absence).
/// `src/empty.rs` exists at every commit but never defines a symbol.
/// `src/late.rs` first appears at C2.
fn synthetic_history() -> Vec<GraphRecord> {
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        // C1 snapshots
        file_snapshot("src/f.rs", "F1", "c1sha0000", T1),
        file_snapshot("src/empty.rs", "E1", "c1sha0000", T1),
        symbol_snapshot("sym_a", "src/f.rs", "A1", "c1sha0000", T1),
        symbol_snapshot("sym_b", "src/f.rs", "B1", "c1sha0000", T1),
        // C2 snapshots: add sym_c, add src/late.rs
        file_snapshot("src/f.rs", "F2", "c2sha0000", T2),
        file_snapshot("src/empty.rs", "E1", "c2sha0000", T2),
        file_snapshot("src/late.rs", "L1", "c2sha0000", T2),
        symbol_snapshot("sym_a", "src/f.rs", "A1", "c2sha0000", T2),
        symbol_snapshot("sym_b", "src/f.rs", "B1", "c2sha0000", T2),
        symbol_snapshot("sym_c", "src/f.rs", "C1", "c2sha0000", T2),
        symbol_snapshot("late_fn", "src/late.rs", "L1", "c2sha0000", T2),
        // C3 snapshots: sym_b removed (no snapshot at c3)
        file_snapshot("src/f.rs", "F3", "c3sha0000", T3),
        file_snapshot("src/empty.rs", "E1", "c3sha0000", T3),
        file_snapshot("src/late.rs", "L1", "c3sha0000", T3),
        symbol_snapshot("sym_a", "src/f.rs", "A1", "c3sha0000", T3),
        symbol_snapshot("sym_c", "src/f.rs", "C1", "c3sha0000", T3),
        symbol_snapshot("late_fn", "src/late.rs", "L1", "c3sha0000", T3),
    ]
}

fn names<'a>(result: &'a aletheia_egregore::query::FileSymbolsAtPoint<'a>) -> Vec<&'a str> {
    result.symbols.iter().map(|s| s.name).collect()
}

// ---------------------------------------------------------------------------
// Point resolution and tombstone honoring (AC2)
// ---------------------------------------------------------------------------

#[test]
fn file_at_commit_returns_exact_symbol_set() {
    let records = synthetic_history();
    let result = file_symbols_at_point(
        &records,
        "src/f.rs",
        FileAtPointSelector::At("c2sha0000"),
        None,
    )
    .expect("point should resolve");
    assert_eq!(result.resolved_commit, "c2sha0000");
    assert_eq!(result.resolved_valid_time, Some(T2));
    assert_eq!(names(&result), vec!["sym_a", "sym_b", "sym_c"]);
    assert_eq!(result.returned, 3);
    assert!(result.diagnostics.is_empty());
}

#[test]
fn file_at_commit_never_returns_tombstoned_symbols() {
    let records = synthetic_history();
    let result = file_symbols_at_point(
        &records,
        "src/f.rs",
        FileAtPointSelector::At("c3sha0000"),
        None,
    )
    .expect("point should resolve");
    // sym_b was removed at C3: it must never appear.
    assert_eq!(names(&result), vec!["sym_a", "sym_c"]);
}

#[test]
fn file_at_accepts_unique_commit_prefix() {
    let records = synthetic_history();
    let result = file_symbols_at_point(&records, "src/f.rs", FileAtPointSelector::At("c1"), None)
        .expect("unique prefix should resolve");
    assert_eq!(result.resolved_commit, "c1sha0000");
    assert_eq!(names(&result), vec!["sym_a", "sym_b"]);
}

#[test]
fn file_as_of_instant_resolves_most_recent_commit_at_or_before() {
    let records = synthetic_history();
    // Between C2 and C3 resolves to C2.
    let result = file_symbols_at_point(
        &records,
        "src/f.rs",
        FileAtPointSelector::AsOf("2026-01-02T12:00:00Z"),
        None,
    )
    .expect("instant should resolve");
    assert_eq!(result.resolved_commit, "c2sha0000");
    assert_eq!(names(&result), vec!["sym_a", "sym_b", "sym_c"]);
    // Exactly at C3 resolves to C3 (inclusive), honoring the tombstone.
    let result = file_symbols_at_point(&records, "src/f.rs", FileAtPointSelector::AsOf(T3), None)
        .expect("instant should resolve");
    assert_eq!(result.resolved_commit, "c3sha0000");
    assert_eq!(names(&result), vec!["sym_a", "sym_c"]);
}

// ---------------------------------------------------------------------------
// Citable handles resolved as-of the point (AC3, AC4)
// ---------------------------------------------------------------------------

#[test]
fn every_row_carries_record_id_path_and_point_commit() {
    let records = synthetic_history();
    let result = file_symbols_at_point(
        &records,
        "src/f.rs",
        FileAtPointSelector::At("c2sha0000"),
        None,
    )
    .expect("point should resolve");
    assert!(!result.file_record_id.is_empty());
    for row in &result.symbols {
        assert!(!row.record_id.is_empty());
        assert!(row.schema_version >= 1);
        assert_eq!(row.kind, "Symbol");
        assert_eq!(row.repo_relative_path, "src/f.rs");
        assert_eq!(row.commit, "c2sha0000");
        assert_eq!(row.valid_time, Some(T2));
        assert!(
            row.span.is_some() || row.absent_span_reason.is_some(),
            "rows must carry a span or a documented absence reason"
        );
    }
}

// ---------------------------------------------------------------------------
// Not-found vs empty-but-found (AC5)
// ---------------------------------------------------------------------------

#[test]
fn unknown_path_is_a_distinct_error() {
    let records = synthetic_history();
    let err = file_symbols_at_point(
        &records,
        "src/never_existed.rs",
        FileAtPointSelector::At("c2sha0000"),
        None,
    )
    .unwrap_err();
    match err {
        FileAtPointError::UnknownPath { path } => assert_eq!(path, "src/never_existed.rs"),
        other => panic!("expected UnknownPath, got {other:?}"),
    }
}

#[test]
fn path_absent_at_point_is_distinct_from_unknown_path() {
    let records = synthetic_history();
    // src/late.rs first appears at C2; at C1 it did not exist.
    let err = file_symbols_at_point(
        &records,
        "src/late.rs",
        FileAtPointSelector::At("c1sha0000"),
        None,
    )
    .unwrap_err();
    match err {
        FileAtPointError::FileAbsentAtPoint {
            path,
            resolved_commit,
        } => {
            assert_eq!(path, "src/late.rs");
            assert_eq!(resolved_commit, "c1sha0000");
        }
        other => panic!("expected FileAbsentAtPoint, got {other:?}"),
    }
}

#[test]
fn zero_symbol_file_is_an_explicit_empty_but_found_result() {
    let records = synthetic_history();
    let result = file_symbols_at_point(
        &records,
        "src/empty.rs",
        FileAtPointSelector::At("c2sha0000"),
        None,
    )
    .expect("file existed at the point");
    assert!(result.symbols.is_empty());
    assert_eq!(result.returned, 0);
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].code, "empty_symbol_set");
}

// ---------------------------------------------------------------------------
// Selector diagnostics (AC6)
// ---------------------------------------------------------------------------

#[test]
fn missing_commit_errors() {
    let records = synthetic_history();
    let err = file_symbols_at_point(&records, "src/f.rs", FileAtPointSelector::At("ffff"), None)
        .unwrap_err();
    match err {
        FileAtPointError::MissingCommit { commit_prefix } => assert_eq!(commit_prefix, "ffff"),
        other => panic!("expected MissingCommit, got {other:?}"),
    }
}

#[test]
fn ambiguous_commit_prefix_errors() {
    let records = synthetic_history();
    let err = file_symbols_at_point(&records, "src/f.rs", FileAtPointSelector::At("c"), None)
        .unwrap_err();
    match err {
        FileAtPointError::AmbiguousCommitPrefix {
            commit_prefix,
            matches,
        } => {
            assert_eq!(commit_prefix, "c");
            assert_eq!(matches.len(), 3);
        }
        other => panic!("expected AmbiguousCommitPrefix, got {other:?}"),
    }
}

#[test]
fn malformed_instant_errors() {
    let records = synthetic_history();
    let err = file_symbols_at_point(
        &records,
        "src/f.rs",
        FileAtPointSelector::AsOf("not-a-timestamp"),
        None,
    )
    .unwrap_err();
    assert!(matches!(err, FileAtPointError::InvalidInstant { .. }));
}

#[test]
fn instant_before_first_commit_errors() {
    let records = synthetic_history();
    let err = file_symbols_at_point(
        &records,
        "src/f.rs",
        FileAtPointSelector::AsOf("2020-01-01T00:00:00Z"),
        None,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        FileAtPointError::NoCommitAtOrBeforeInstant { .. }
    ));
}

#[test]
fn empty_history_errors() {
    let records = vec![symbol_snapshot("lonely", "src/f.rs", "A", "c1sha0000", T1)];
    let err = file_symbols_at_point(&records, "src/f.rs", FileAtPointSelector::At("c1"), None)
        .unwrap_err();
    assert!(matches!(err, FileAtPointError::EmptyHistory));
}

// ---------------------------------------------------------------------------
// Multi-repository stores: --as-of must resolve on the path's own timeline
// ---------------------------------------------------------------------------

/// Attributed two-repository fixture: repo A owns `src/f.rs` (symbol `sym_a`
/// at commit `aaaa1111`, T1); repo B owns `src/other.rs` (symbol `other_fn` at
/// commit `bbbb2222`, T2 — newer than every repo A commit).
fn two_repo_history() -> Vec<GraphRecord> {
    fn repo_node(repo_id: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            repo_id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(name.to_owned()),
            format!("Repository {name}"),
        )
    }
    fn contains(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            aletheia_egregore::EdgeLabel::Contains,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "containment".to_owned(),
        )
    }
    fn defines(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            aletheia_egregore::EdgeLabel::Defines,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "defines".to_owned(),
        )
    }
    fn commit_in(repo_id: &str, sha: &str, valid_time: &str) -> GraphRecord {
        let id = stable_id(&["node", "commit", repo_id, sha]);
        GraphRecord::node(
            id,
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("Commit {sha}"),
        )
        .with_temporal(temporal(sha, &[], valid_time))
    }
    fn file_in(repo_id: &str, path: &str, commit: &str, valid_time: &str) -> GraphRecord {
        let id = stable_id(&["node", "file", repo_id, path]);
        GraphRecord::node(
            id,
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("Source file {path}"),
        )
        .with_temporal(temporal(commit, &[], valid_time))
    }
    fn symbol_in(
        repo_id: &str,
        name: &str,
        path: &str,
        commit: &str,
        valid_time: &str,
    ) -> GraphRecord {
        let id = stable_id(&["node", "symbol", repo_id, path, name]);
        GraphRecord::node(
            id,
            NodeKind::Symbol,
            Some(path.to_owned()),
            None,
            Some(name.to_owned()),
            format!("Symbol {name} in {path}"),
        )
        .with_temporal(temporal(commit, &[], valid_time))
    }

    let commit_a = stable_id(&["node", "commit", "repo:a", "aaaa1111"]);
    let file_a = stable_id(&["node", "file", "repo:a", "src/f.rs"]);
    let sym_a = stable_id(&["node", "symbol", "repo:a", "src/f.rs", "sym_a"]);
    let commit_b = stable_id(&["node", "commit", "repo:b", "bbbb2222"]);
    let file_b = stable_id(&["node", "file", "repo:b", "src/other.rs"]);
    let sym_b = stable_id(&["node", "symbol", "repo:b", "src/other.rs", "other_fn"]);

    vec![
        repo_node("repo:a", "repo-a"),
        repo_node("repo:b", "repo-b"),
        // Repo A history (older commit).
        commit_in("repo:a", "aaaa1111", T1),
        file_in("repo:a", "src/f.rs", "aaaa1111", T1),
        symbol_in("repo:a", "sym_a", "src/f.rs", "aaaa1111", T1),
        contains("repo:a", &commit_a),
        contains("repo:a", &file_a),
        defines(&file_a, &sym_a),
        // Repo B history (newest commit in the shared store).
        commit_in("repo:b", "bbbb2222", T2),
        file_in("repo:b", "src/other.rs", "bbbb2222", T2),
        symbol_in("repo:b", "other_fn", "src/other.rs", "bbbb2222", T2),
        contains("repo:b", &commit_b),
        contains("repo:b", &file_b),
        defines(&file_b, &sym_b),
    ]
}

#[test]
fn as_of_resolves_on_the_path_owning_repository_timeline() {
    let records = two_repo_history();
    // The store-wide newest commit at or before T3 is repo B's bbbb2222, but
    // src/f.rs lives in repo A: the instant must resolve on repo A's own
    // timeline (aaaa1111), never report the file absent at an unrelated
    // repository's commit.
    let result = file_symbols_at_point(&records, "src/f.rs", FileAtPointSelector::AsOf(T3), None)
        .expect("path must resolve on its owning repository's timeline");
    assert_eq!(result.resolved_commit, "aaaa1111");
    assert_eq!(names(&result), vec!["sym_a"]);
}

#[test]
fn as_of_scoped_to_one_repository_resolves_within_it() {
    let records = two_repo_history();
    let result = file_symbols_at_point(
        &records,
        "src/f.rs",
        FileAtPointSelector::AsOf(T3),
        Some("repo:a"),
    )
    .expect("scoped query should resolve");
    assert_eq!(result.resolved_commit, "aaaa1111");
    assert_eq!(names(&result), vec!["sym_a"]);
}

#[test]
fn as_of_unscoped_path_collision_across_repositories_fails_closed() {
    let mut records = two_repo_history();
    // Give repo B its own `src/f.rs` too: an unscoped single-answer time view
    // must never pick one repository's timeline implicitly (issue #67).
    let file_b2 = stable_id(&["node", "file", "repo:b", "src/f.rs"]);
    records.push(
        GraphRecord::node(
            file_b2.clone(),
            NodeKind::File,
            Some("src/f.rs".to_owned()),
            None,
            Some("src/f.rs".to_owned()),
            "Source file src/f.rs".to_owned(),
        )
        .with_temporal(temporal("bbbb2222", &[], T2)),
    );
    records.push(GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        "repo:b".to_owned(),
        file_b2,
        Some("1.0".to_owned()),
        "containment".to_owned(),
    ));

    let err = file_symbols_at_point(&records, "src/f.rs", FileAtPointSelector::AsOf(T3), None)
        .unwrap_err();
    match err {
        FileAtPointError::AmbiguousRepository { path, repositories } => {
            assert_eq!(path, "src/f.rs");
            assert_eq!(repositories, vec!["repo:a".to_owned(), "repo:b".to_owned()]);
        }
        other => panic!("expected AmbiguousRepository, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Seeded fixture repo: end-to-end through scan-history + CLI
// ---------------------------------------------------------------------------

/// Issue #158 acceptance fixture as a real Git history: `src/f.rs` defines
/// {alpha, beta} at C1, adds gamma at C2, and removes beta at C3.
fn seed_fixture_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(
        repo,
        "src/f.rs",
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\n",
    );
    let first = commit_fixture(repo, "seed alpha and beta", T1);

    write(
        repo,
        "src/f.rs",
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\npub fn gamma() -> u32 { 3 }\n",
    );
    let second = commit_fixture(repo, "add gamma", T2);

    write(
        repo,
        "src/f.rs",
        "pub fn alpha() -> u32 { 1 }\npub fn gamma() -> u32 { 3 }\n",
    );
    let third = commit_fixture(repo, "remove beta", T3);

    [first, second, third]
}

fn scan_history_graph(repo: &Path, graph_path: &Path) {
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(graph_path)
        .assert()
        .success();
}

fn row_names(stdout: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(stdout).expect("stdout should be JSON");
    body["symbols"]
        .as_array()
        .expect("symbols should be an array")
        .iter()
        .map(|row| row["name"].as_str().expect("name").to_owned())
        .collect()
}

#[test]
fn query_file_at_cli_reconstructs_past_symbol_sets() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [_first, second, third] = seed_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");
    scan_history_graph(&repo, &graph_path);

    // --at C2 returns exactly {alpha, beta, gamma}.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/f.rs", "--at", &second])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // Non-root files carry the scanner's module-path-qualified symbol names.
    assert_eq!(row_names(&out), vec!["f::alpha", "f::beta", "f::gamma"]);
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], true);
    assert_eq!(body["resolved_commit"], second.as_str());
    assert_eq!(body["at"], second.as_str());
    for row in body["symbols"].as_array().unwrap() {
        assert!(row["record_id"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(row["repo_relative_path"], "src/f.rs");
        assert_eq!(row["commit"], second.as_str());
        assert!(
            row["span"].is_object() || row["absent_span_reason"].is_string(),
            "each row must carry a span handle or a documented absence reason"
        );
    }

    // --at C3 returns exactly {alpha, gamma}: beta is tombstoned.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/f.rs", "--at", &third])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(row_names(&out), vec!["f::alpha", "f::gamma"]);

    // --as-of between C2 and C3 resolves to C2.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query",
            "file",
            "src/f.rs",
            "--as-of",
            "2026-01-02T12:00:00Z",
        ])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["resolved_commit"], second.as_str());
    assert_eq!(row_names(&out), vec!["f::alpha", "f::beta", "f::gamma"]);
}

#[test]
fn query_file_at_cli_is_read_only_and_byte_deterministic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [_first, second, _third] = seed_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");
    scan_history_graph(&repo, &graph_path);

    let graph_bytes_before = fs::read(&graph_path).expect("graph should be readable");
    let listing_before = dir_listing(temp.path());
    let status_before = git_output(&repo, ["status", "--porcelain"]);
    assert!(status_before.is_empty(), "fixture tree must start clean");

    // Five consecutive runs are byte-identical (AC7).
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let assert = CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "file", "src/f.rs", "--at", &second[..12]])
            .arg("--graph")
            .arg(&graph_path)
            .assert()
            .success();
        outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    for output in &outputs[1..] {
        assert_eq!(&outputs[0], output, "CLI output must be byte-identical");
    }

    // Read-only: zero records/indexes/runtime files created, modified, or
    // deleted, and the checkout is untouched.
    let graph_bytes_after = fs::read(&graph_path).expect("graph should be readable");
    assert_eq!(
        graph_bytes_before, graph_bytes_after,
        "query must not modify the graph store"
    );
    assert_eq!(
        listing_before,
        dir_listing(temp.path()),
        "query must not create or delete files"
    );
    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(
        status_after.is_empty(),
        "query must not mutate the working tree: {status_after}"
    );

    // Redaction-safe: no raw source text, patch hunks, or commit messages.
    assert!(
        !outputs[0].contains("Source:"),
        "raw snapshot bodies must never leak into the response"
    );
    assert!(
        !outputs[0].contains("-> u32"),
        "source text must never leak into the response"
    );
    assert!(
        !outputs[0].contains("add gamma"),
        "commit-message bodies must never leak into the response"
    );
}

#[test]
fn query_file_at_cli_exit_codes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second, _third] = seed_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");
    scan_history_graph(&repo, &graph_path);

    // Unknown path: exit 2, machine-readable diagnostic, never empty success.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/nope.rs", "--at", &second])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["error_type"], "unknown_path");

    // Unknown commit: exit 2.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/f.rs", "--at", "ffffffffffff"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "missing_commit");

    // Malformed instant: exit 2 (invalid input, documented, never coerced).
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/f.rs", "--as-of", "not-a-time"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "invalid_instant");

    // --at with --as-of: rejected by the flag contract (clap conflict, exit 2).
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/f.rs", "--at", &first, "--as-of", T2])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);

    // --tx-as-of: not implemented for query file, exit 1 with the documented
    // machine-readable envelope, never silently ignored.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query",
            "file",
            "src/f.rs",
            "--tx-as-of",
            "2026-01-02T00:00:00Z",
        ])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "not_implemented");

    // Empty-but-found is exit 0 with an explicit diagnostic: a file that
    // existed but defined zero symbols is not a no-match.
    write(repo.as_path(), "src/plain.rs", "// no symbols here\n");
    let _fourth = commit_fixture(&repo, "add symbol-free file", "2026-01-04T00:00:00Z");
    let graph_path2 = temp.path().join("history2.graph.jsonl");
    scan_history_graph(&repo, &graph_path2);
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query",
            "file",
            "src/plain.rs",
            "--as-of",
            "2026-01-05T00:00:00Z",
        ])
        .arg("--graph")
        .arg(&graph_path2)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], true);
    assert_eq!(body["symbols"].as_array().unwrap().len(), 0);
    assert_eq!(body["diagnostics"][0]["code"], "empty_symbol_set");
}

/// Sorted `(relative path, bytes)` fingerprint of every file under `root`
/// (mirrors `tests/integration/evidence_freshness.rs`).
#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let ft = entry.file_type().unwrap();
            let path = entry.path();
            if ft.is_dir() {
                walk(&path, base, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// The point-in-time listing is strictly read-only for `--data-dir` too:
/// opening the embedded engine in place re-persists its on-disk index files,
/// so the query must read a throwaway copy and leave the live store
/// byte-for-byte untouched (issue #158 read-only contract; mirrors the
/// evidence-freshness and audit lanes).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_file_at_data_dir_is_strictly_read_only() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [_first, second, _third] = seed_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");
    scan_history_graph(&repo, &graph_path);

    let data_dir = temp.path().join("store");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let before = dir_fingerprint(&data_dir);
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/f.rs", "--at", &second, "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(row_names(&out), vec!["f::alpha", "f::beta", "f::gamma"]);

    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "query file --at must not modify any store file when reading --data-dir"
    );
}

/// A re-ingest that rewrites a snapshot for an existing commit (e.g. after an
/// extractor change) supersedes the prior physical version of the same
/// `(record_id, git_commit)` pair. A plain valid-time point query against
/// `--data-dir` must return exactly one row per symbol — the current version —
/// never a duplicate or stale superseded row (those belong to the
/// transaction-time lane, issue #66).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn query_file_at_data_dir_collapses_reingested_snapshots() {
    use aletheia_egregore::SourceSpan;
    use aletheia_egregore::ir::Graph;

    const fn line_span(start_line: usize, end_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line,
            end_line,
        }
    }
    fn spanned_symbol(
        name: &str,
        path: &str,
        body: &str,
        sha: &str,
        end_line: usize,
    ) -> GraphRecord {
        let id = stable_id(&["node", "symbol", "repo_test", path, name]);
        GraphRecord::node(
            id,
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(line_span(1, end_line)),
            Some(name.to_owned()),
            format!("Symbol {name} in {path}\nSource:\n{body}"),
        )
        .with_temporal(temporal(sha, &[], T1))
    }

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");

    // First ingest: original extraction of commit c1sha0000.
    let mut graph_v1 = Graph::new();
    graph_v1.push(commit("c1sha0000", &[], T1));
    graph_v1.push(file_snapshot("src/f.rs", "F1", "c1sha0000", T1));
    graph_v1.push(spanned_symbol("sym_a", "src/f.rs", "A1", "c1sha0000", 5));
    let graph_v1_path = temp.path().join("v1.graph.jsonl");
    fs::write(&graph_v1_path, graph_v1.to_jsonl().expect("serialize"))
        .expect("fixture graph should be written");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_v1_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    // Re-ingest: an extractor change rewrote the same commit's snapshot with a
    // different span. Same stable IDs, same commit — the prior physical
    // version is superseded, not tombstoned.
    let mut graph_v2 = Graph::new();
    graph_v2.push(commit("c1sha0000", &[], T1));
    graph_v2.push(file_snapshot("src/f.rs", "F1", "c1sha0000", T1));
    graph_v2.push(spanned_symbol("sym_a", "src/f.rs", "A2", "c1sha0000", 9));
    let graph_v2_path = temp.path().join("v2.graph.jsonl");
    fs::write(&graph_v2_path, graph_v2.to_jsonl().expect("serialize"))
        .expect("fixture graph should be written");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_v2_path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query",
            "file",
            "src/f.rs",
            "--at",
            "c1sha0000",
            "--data-dir",
        ])
        .arg(&data_dir)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    let symbols = body["symbols"].as_array().expect("symbols array");
    assert_eq!(
        symbols.len(),
        1,
        "one symbol must yield exactly one row — never a superseded duplicate: {out}"
    );
    assert_eq!(symbols[0]["name"], "sym_a");
    assert_eq!(
        symbols[0]["span"]["end_line"], 9,
        "the row must carry the current (re-ingested) span, not the stale one"
    );
}

// ---------------------------------------------------------------------------
// Fixture helpers (mirrors tests/integration/range_deltas.rs)
// ---------------------------------------------------------------------------

/// Recursive sorted listing of every path under `root` (excluding Git's own
/// bookkeeping inside the fixture repo, which the read-only assertion covers
/// separately via `git status`).
fn dir_listing(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let entries = fs::read_dir(dir).expect("directory should be listable");
        for entry in entries {
            let entry = entry.expect("directory entry should be readable");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("path should be under root")
                .to_string_lossy()
                .replace('\\', "/");
            if relative.starts_with("repo/.git") {
                continue;
            }
            out.push(relative);
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit_fixture(repo: &Path, message: &str, date: &str) -> String {
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
