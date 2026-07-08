//! Integration tests for `eg query deltas` (issue #118): symbol- and
//! file-level change classification across a commit range.
#![allow(missing_docs)]

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{
    EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis, SemanticDriftMetadata,
    TemporalMetadata,
    query::{RangeDeltas, RangeDeltasError, range_deltas},
    scan_repository_history, stable_id,
};
use assert_cmd::Command as CargoCommand;

// ---------------------------------------------------------------------------
// Synthetic record helpers (mirrors tests/integration/changes_query.rs)
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

fn drift_node(target_id: &str, before: &str, after: &str, score: f64) -> GraphRecord {
    GraphRecord::node(
        format!("drift:{target_id}:{after}"),
        NodeKind::SemanticDrift,
        None,
        None,
        Some("drift".to_owned()),
        format!("Semantic drift for {target_id}"),
    )
    .with_temporal(temporal(after, &[], "2026-01-02T00:00:00Z"))
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "fake-code-model".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: target_id.to_owned(),
        prior_record_id: target_id.to_owned(),
        before_git_commit: before.to_owned(),
        after_git_commit: after.to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    })
}

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const T3: &str = "2026-01-03T00:00:00Z";

/// Three linear commits exercising every change class between `c1` and `c3`:
/// symbol added (`new_name`, `fresh_fn`), removed (`old_name`, `gone_helper`),
/// renamed (`old_name` -> `new_name`), modified in place (`tweaked`), a new
/// file (`src/fresh.rs`), and a deleted file (`src/gone.rs`).
fn synthetic_range_records() -> Vec<GraphRecord> {
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        // c1 snapshots
        file_snapshot("src/lib.rs", "L1", "c1sha0000", T1),
        file_snapshot("src/gone.rs", "G1", "c1sha0000", T1),
        symbol_snapshot("keep", "src/lib.rs", "A", "c1sha0000", T1),
        symbol_snapshot("old_name", "src/lib.rs", "B", "c1sha0000", T1),
        symbol_snapshot("tweaked", "src/lib.rs", "C1", "c1sha0000", T1),
        symbol_snapshot("gone_helper", "src/gone.rs", "D", "c1sha0000", T1),
        // c2 snapshots: rename old_name -> new_name, modify tweaked
        file_snapshot("src/lib.rs", "L2", "c2sha0000", T2),
        file_snapshot("src/gone.rs", "G1", "c2sha0000", T2),
        symbol_snapshot("keep", "src/lib.rs", "A", "c2sha0000", T2),
        symbol_snapshot("new_name", "src/lib.rs", "B", "c2sha0000", T2),
        symbol_snapshot("tweaked", "src/lib.rs", "C2", "c2sha0000", T2),
        symbol_snapshot("gone_helper", "src/gone.rs", "D", "c2sha0000", T2),
        // c3 snapshots: add src/fresh.rs, delete src/gone.rs
        file_snapshot("src/lib.rs", "L2", "c3sha0000", T3),
        file_snapshot("src/fresh.rs", "F1", "c3sha0000", T3),
        symbol_snapshot("keep", "src/lib.rs", "A", "c3sha0000", T3),
        symbol_snapshot("new_name", "src/lib.rs", "B", "c3sha0000", T3),
        symbol_snapshot("tweaked", "src/lib.rs", "C2", "c3sha0000", T3),
        symbol_snapshot("fresh_fn", "src/fresh.rs", "E", "c3sha0000", T3),
    ]
}

fn names_and_commits<'a>(
    items: &'a [aletheia_egregore::query::RangeDeltaItem<'a>],
) -> Vec<(&'a str, &'a str)> {
    items
        .iter()
        .map(|item| (item.name.unwrap_or(item.repo_relative_path), item.commit))
        .collect()
}

// ---------------------------------------------------------------------------
// Diagnostics (machine-readable failures, never partial output)
// ---------------------------------------------------------------------------

#[test]
fn range_deltas_empty_history_errors() {
    let records = vec![symbol_snapshot(
        "lonely",
        "src/lib.rs",
        "A",
        "c1sha0000",
        T1,
    )];
    let err = range_deltas(&records, "c1", "c2", None).unwrap_err();
    assert!(matches!(err, RangeDeltasError::EmptyHistory));
}

#[test]
fn range_deltas_missing_commit_errors() {
    let records = synthetic_range_records();
    let err = range_deltas(&records, "ffff", "c3sha0000", None).unwrap_err();
    match err {
        RangeDeltasError::MissingCommit { commit_prefix } => assert_eq!(commit_prefix, "ffff"),
        other => panic!("expected MissingCommit, got {other:?}"),
    }
}

#[test]
fn range_deltas_ambiguous_prefix_errors() {
    let records = synthetic_range_records();
    let err = range_deltas(&records, "c1", "c", None).unwrap_err();
    match err {
        RangeDeltasError::AmbiguousCommitPrefix {
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
fn range_deltas_identical_endpoints_error() {
    let records = synthetic_range_records();
    // Two different spellings of the same commit still resolve identically.
    let err = range_deltas(&records, "c2", "c2sha0000", None).unwrap_err();
    match err {
        RangeDeltasError::IdenticalEndpoints { commit } => assert_eq!(commit, "c2sha0000"),
        other => panic!("expected IdenticalEndpoints, got {other:?}"),
    }
}

#[test]
fn range_deltas_reversed_range_errors() {
    let records = synthetic_range_records();
    let err = range_deltas(&records, "c3", "c1", None).unwrap_err();
    assert!(matches!(err, RangeDeltasError::ReversedRange { .. }));
}

#[test]
fn range_deltas_no_path_errors() {
    let records = vec![
        commit("aaaa1111", &[], T1),
        commit("bbbb2222", &[], T2), // unrelated root
    ];
    let err = range_deltas(&records, "aaaa", "bbbb", None).unwrap_err();
    assert!(matches!(err, RangeDeltasError::NoPath { .. }));
}

// ---------------------------------------------------------------------------
// Change-class grouping over synthetic history
// ---------------------------------------------------------------------------

#[test]
fn range_deltas_groups_synthetic_changes_by_class() {
    let records = synthetic_range_records();
    let deltas = range_deltas(&records, "c1", "c3", None).expect("range should resolve");

    assert_eq!(deltas.base, "c1sha0000");
    assert_eq!(deltas.head, "c3sha0000");
    assert_eq!(deltas.range_commit_count, 2);

    assert_eq!(
        names_and_commits(&deltas.added_symbols),
        vec![("fresh_fn", "c3sha0000"), ("new_name", "c2sha0000")],
    );
    assert_eq!(
        names_and_commits(&deltas.removed_symbols),
        vec![("gone_helper", "c3sha0000"), ("old_name", "c2sha0000")],
    );
    assert_eq!(
        names_and_commits(&deltas.modified_symbols),
        vec![("tweaked", "c2sha0000")],
    );
    assert_eq!(
        names_and_commits(&deltas.added_files),
        vec![("src/fresh.rs", "c3sha0000")],
    );
    assert_eq!(
        names_and_commits(&deltas.removed_files),
        vec![("src/gone.rs", "c3sha0000")],
    );
    assert_eq!(
        names_and_commits(&deltas.modified_files),
        vec![("src/lib.rs", "c2sha0000")],
    );
    assert!(deltas.unresolved.is_empty(), "no unresolved rows expected");

    // Class labels are stable and every row carries citable identity fields.
    for (items, class) in [
        (&deltas.added_symbols, "added_symbol"),
        (&deltas.removed_symbols, "removed_symbol"),
        (&deltas.modified_symbols, "modified_symbol"),
        (&deltas.added_files, "added_file"),
        (&deltas.removed_files, "removed_file"),
        (&deltas.modified_files, "modified_file"),
    ] {
        for item in items {
            assert_eq!(item.change_class, class);
            assert!(!item.record_id.is_empty());
            assert!(item.schema_version >= 1);
            assert!(!item.repo_relative_path.is_empty());
            assert!(
                item.valid_time.is_some(),
                "row must carry commit valid time"
            );
        }
    }

    // The disclaimer labels rows as observed deltas, never behavior proof.
    assert!(deltas.disclaimer.contains("not proof"));
}

#[test]
fn range_deltas_valid_time_matches_introducing_commit() {
    let records = synthetic_range_records();
    let deltas = range_deltas(&records, "c1", "c3", None).expect("range should resolve");
    let new_name = deltas
        .added_symbols
        .iter()
        .find(|item| item.name == Some("new_name"))
        .expect("new_name should be reported as added");
    assert_eq!(new_name.commit, "c2sha0000");
    assert_eq!(new_name.valid_time, Some(T2));
    let fresh = deltas
        .added_files
        .iter()
        .find(|item| item.repo_relative_path == "src/fresh.rs")
        .expect("src/fresh.rs should be reported as added");
    assert_eq!(fresh.valid_time, Some(T3));
}

#[test]
fn range_deltas_subrange_excludes_changes_outside_range() {
    let records = synthetic_range_records();
    // c2..c3 must not report the c1->c2 rename or modification.
    let deltas = range_deltas(&records, "c2", "c3", None).expect("range should resolve");
    assert_eq!(
        names_and_commits(&deltas.added_symbols),
        vec![("fresh_fn", "c3sha0000")],
    );
    assert_eq!(
        names_and_commits(&deltas.removed_symbols),
        vec![("gone_helper", "c3sha0000")],
    );
    assert!(deltas.modified_symbols.is_empty());
    assert!(deltas.modified_files.is_empty());
}

// ---------------------------------------------------------------------------
// Semantic drift folding
// ---------------------------------------------------------------------------

#[test]
fn range_deltas_marks_drift_unavailable_without_drift_records() {
    let records = synthetic_range_records();
    let deltas = range_deltas(&records, "c1", "c3", None).expect("range should resolve");
    assert_eq!(deltas.semantic_drift.status, "unavailable");
    assert!(deltas.semantic_drift.reason.is_some());
    assert!(deltas.semantic_drift.rows.is_empty());
}

#[test]
fn range_deltas_surfaces_in_range_drift_as_semantic_movement() {
    let mut records = synthetic_range_records();
    let tweaked_id = stable_id(&["node", "symbol", "repo_test", "src/lib.rs", "tweaked"]);
    records.push(drift_node(&tweaked_id, "c1sha0000", "c2sha0000", 0.42));
    // A drift landing outside the queried range must not be listed.
    records.push(drift_node("other_target", "c0sha0000", "c1sha0000", 0.9));

    let deltas = range_deltas(&records, "c1", "c3", None).expect("range should resolve");
    assert_eq!(deltas.semantic_drift.status, "available");
    assert_eq!(
        deltas.semantic_drift.label,
        "semantic_movement_not_structural_change"
    );
    assert_eq!(deltas.semantic_drift.rows.len(), 1);
    let row = &deltas.semantic_drift.rows[0];
    assert_eq!(row.target_record_id, tweaked_id);
    assert_eq!(row.after_git_commit, "c2sha0000");
    assert!((row.score - 0.42).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------------------
// Seeded fixture repo: end-to-end through scan-history + CLI (AC1, AC8, AC9)
// ---------------------------------------------------------------------------

/// Documented fixture range: three commits where `first..third` exercises one
/// symbol added (`brand_new`), one removed (`doomed`), one renamed
/// (`old_name` -> `new_name`), one modified in place (`tweaked`), one new file
/// (`src/fresh.rs`), and one deleted file (`src/gone.rs`).
fn seed_delta_fixture_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(
        repo,
        "src/lib.rs",
        "pub fn keep() -> u32 { 1 }\npub fn old_name() -> u32 { 2 }\npub fn doomed() -> u32 { 3 }\npub fn tweaked() -> u32 { 4 }\n",
    );
    write(repo, "src/gone.rs", "pub fn gone_helper() -> u32 { 9 }\n");
    let first = commit_fixture(repo, "seed symbols", "2026-01-01T00:00:00Z");

    write(
        repo,
        "src/lib.rs",
        "pub fn keep() -> u32 { 1 }\npub fn new_name() -> u32 { 2 }\npub fn tweaked() -> u32 { 44 }\npub fn brand_new() -> u32 { 5 }\n",
    );
    let second = commit_fixture(repo, "rename, modify, remove, add", "2026-01-02T00:00:00Z");

    write(repo, "src/fresh.rs", "pub fn fresh_fn() -> u32 { 7 }\n");
    fs::remove_file(repo.join("src/gone.rs")).expect("fixture file should be removed");
    let third = commit_fixture(
        repo,
        "add fresh file, delete gone file",
        "2026-01-03T00:00:00Z",
    );

    [first, second, third]
}

#[test]
fn range_deltas_fixture_repo_classifies_all_expected_deltas() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second, third] = seed_delta_fixture_repo(&repo);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let status_before = git_output(&repo, ["status", "--porcelain"]);
    assert!(status_before.is_empty(), "fixture tree must start clean");

    let deltas = range_deltas(&records, &first, &third, None).expect("range should resolve");

    // 100% of expected additions and removals, with introducing commits.
    // Non-root files carry the scanner's module-path-qualified symbol names.
    let added: Vec<(&str, &str)> = names_and_commits(&deltas.added_symbols);
    assert!(added.contains(&("brand_new", second.as_str())));
    assert!(added.contains(&("new_name", second.as_str())));
    assert!(added.contains(&("fresh::fresh_fn", third.as_str())));

    let removed: Vec<(&str, &str)> = names_and_commits(&deltas.removed_symbols);
    assert!(removed.contains(&("doomed", second.as_str())));
    assert!(removed.contains(&("old_name", second.as_str())));
    assert!(removed.contains(&("gone::gone_helper", third.as_str())));

    // In-place modification.
    let modified: Vec<(&str, &str)> = names_and_commits(&deltas.modified_symbols);
    assert!(modified.contains(&("tweaked", second.as_str())));

    // Unchanged symbol never appears in any group.
    for group in [
        &deltas.added_symbols,
        &deltas.removed_symbols,
        &deltas.modified_symbols,
    ] {
        assert!(group.iter().all(|item| item.name != Some("keep")));
    }

    assert_eq!(
        names_and_commits(&deltas.added_files),
        vec![("src/fresh.rs", third.as_str())],
    );
    assert_eq!(
        names_and_commits(&deltas.removed_files),
        vec![("src/gone.rs", third.as_str())],
    );
    assert_eq!(
        names_and_commits(&deltas.modified_files),
        vec![("src/lib.rs", second.as_str())],
    );

    // Every symbol row carries a span (real spans from Tree-sitter parsing).
    for item in deltas
        .added_symbols
        .iter()
        .chain(&deltas.removed_symbols)
        .chain(&deltas.modified_symbols)
    {
        assert!(
            item.span.is_some() || item.absent_span_reason.is_some(),
            "symbol rows must carry a span or a documented absence reason"
        );
    }

    // Range resolution reads Git objects only: tree is byte-for-byte clean.
    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(
        status_after.is_empty(),
        "query must not mutate the working tree: {status_after}"
    );

    // Repeating the query yields byte-equivalent canonical output (AC9).
    let baseline = serde_json::to_string(&deltas).expect("deltas should serialize");
    for _ in 0..4 {
        let again: RangeDeltas<'_> =
            range_deltas(&records, &first, &third, None).expect("range should resolve");
        let serialized = serde_json::to_string(&again).expect("deltas should serialize");
        assert_eq!(
            baseline, serialized,
            "repeated runs must be byte-equivalent"
        );
    }
}

#[test]
fn query_deltas_cli_is_deterministic_and_redaction_safe() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_delta_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Unique 12-char prefixes must resolve exactly like full SHAs.
    let base_prefix = &first[..12];
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let assert = CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "deltas", base_prefix, &third])
            .arg("--graph")
            .arg(&graph_path)
            .assert()
            .success();
        outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    for output in &outputs[1..] {
        assert_eq!(&outputs[0], output, "CLI output must be byte-identical");
    }

    let body: serde_json::Value = serde_json::from_str(&outputs[0]).expect("stdout should be JSON");
    assert_eq!(body["ok"], true);
    assert_eq!(body["base"], first);
    assert_eq!(body["head"], third);
    for group in [
        "added_symbols",
        "removed_symbols",
        "modified_symbols",
        "added_files",
        "removed_files",
        "modified_files",
        "unresolved",
    ] {
        assert!(
            body[group].is_array(),
            "group {group} must always be present"
        );
    }
    assert_eq!(body["semantic_drift"]["status"], "unavailable");
    assert!(
        body["disclaimer"].as_str().unwrap().contains("not proof"),
        "disclaimer must label rows as observed deltas"
    );

    // Output never includes raw source bodies, only bounded identity fields.
    assert!(
        !outputs[0].contains("Source:"),
        "raw snapshot bodies must never leak into the response"
    );
    assert!(
        !outputs[0].contains("-> u32"),
        "source text must never leak into the response"
    );

    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(
        status_after.is_empty(),
        "CLI query must not mutate the tree"
    );
}

#[test]
fn query_deltas_cli_exit_codes_for_diagnostics() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, _second, third] = seed_delta_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Identical endpoints: exit 1, machine-readable diagnostic.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "deltas", &first, &first])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["error_type"], "identical_endpoints");

    // Unknown commit: exit 2.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "deltas", "ffffffffffff", &third])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "missing_commit");

    // Reversed range: exit 1.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "deltas", &third, &first])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "reversed_range");
}

// ---------------------------------------------------------------------------
// Git fixture helpers (mirrors tests/integration/history.rs)
// ---------------------------------------------------------------------------

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
