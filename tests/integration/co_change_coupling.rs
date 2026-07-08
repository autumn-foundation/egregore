//! Integration tests for `eg query coupling` (issue #153): ranked historical
//! file co-change partners from a `scan-history` temporal store.
#![allow(missing_docs)]

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::TemporalMetadata;
use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind,
    ir::stable_id,
    query::{
        CO_CHANGE_DEFAULT_LIMIT, CO_CHANGE_DEFAULT_MIN_SUPPORT, CO_CHANGE_MAX_LIMIT,
        CO_CHANGE_MAX_MIN_SUPPORT, CoChangeCoupling, CoChangeCouplingError,
        CoChangeCouplingOptions, co_change_coupling,
    },
    scan_repository_history,
};
use assert_cmd::Command as CargoCommand;

// ---------------------------------------------------------------------------
// Synthetic record helpers (mirrors tests/integration/changes_query.rs)
// ---------------------------------------------------------------------------

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const T3: &str = "2026-01-03T00:00:00Z";
const T4: &str = "2026-01-04T00:00:00Z";
const T5: &str = "2026-01-05T00:00:00Z";

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

fn file_node(path: &str, commit_sha: &str, valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "file", "repo_test", path]);
    GraphRecord::node(
        id,
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("Source file {path}\nSource:\nfn secret_body() {{}}"),
    )
    .with_temporal(temporal(commit_sha, &[], valid_time))
}

fn file_id(path: &str) -> String {
    stable_id(&["node", "file", "repo_test", path])
}

fn changed_in(path: &str, commit_sha: &str, valid_time: &str) -> GraphRecord {
    let src = file_id(path);
    let tgt = stable_id(&["node", "commit", "repo_test", commit_sha]);
    GraphRecord::edge(
        EdgeLabel::ChangedIn,
        src,
        tgt,
        Some("1.0".to_owned()),
        format!("{path} changed in commit {commit_sha}"),
    )
    .with_temporal(temporal(commit_sha, &[], valid_time))
}

/// Five linear commits with an engineered co-change structure around the
/// target `src/alpha.rs`:
///
/// | file           | changed in           | vs alpha (c1..c4, 4 changes)      |
/// |----------------|----------------------|-----------------------------------|
/// | `src/alpha.rs` | c1, c2, c3, c4       | target                            |
/// | `src/beta.rs`  | c1, c2, c3           | co=3, jaccard=3/4, confidence=3/4 |
/// | `src/delta.rs` | c1, c2, c5           | co=2, jaccard=2/5, confidence=2/4 |
/// | `src/gamma.rs` | c1                   | co=1 (below default min support)  |
/// | `src/eps.rs`   | (never)              | File node, zero in-scope changes  |
fn synthetic_records() -> Vec<GraphRecord> {
    let mut records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        commit("c3sha0000", &["c2sha0000"], T3),
        commit("c4sha0000", &["c3sha0000"], T4),
        commit("c5sha0000", &["c4sha0000"], T5),
        file_node("src/alpha.rs", "c1sha0000", T1),
        file_node("src/beta.rs", "c1sha0000", T1),
        file_node("src/delta.rs", "c1sha0000", T1),
        file_node("src/gamma.rs", "c1sha0000", T1),
        file_node("src/eps.rs", "c1sha0000", T1),
    ];
    for (path, commits) in [
        (
            "src/alpha.rs",
            vec![
                ("c1sha0000", T1),
                ("c2sha0000", T2),
                ("c3sha0000", T3),
                ("c4sha0000", T4),
            ],
        ),
        (
            "src/beta.rs",
            vec![("c1sha0000", T1), ("c2sha0000", T2), ("c3sha0000", T3)],
        ),
        (
            "src/delta.rs",
            vec![("c1sha0000", T1), ("c2sha0000", T2), ("c5sha0000", T5)],
        ),
        ("src/gamma.rs", vec![("c1sha0000", T1)]),
    ] {
        for (sha, vt) in commits {
            records.push(changed_in(path, sha, vt));
        }
    }
    records
}

fn options() -> CoChangeCouplingOptions<'static> {
    CoChangeCouplingOptions::default()
}

fn partner_paths<'a>(report: &'a CoChangeCoupling<'_>) -> Vec<&'a str> {
    report
        .partners
        .iter()
        .map(|p| p.repo_relative_path)
        .collect()
}

// ---------------------------------------------------------------------------
// Ranking, metric, and threshold semantics over synthetic history
// ---------------------------------------------------------------------------

#[test]
fn coupling_ranks_partners_by_jaccard_with_counts_and_confidence() {
    let records = synthetic_records();
    let report =
        co_change_coupling(&records, "src/alpha.rs", None, &options()).expect("should resolve");

    assert_eq!(report.target.repo_relative_path, "src/alpha.rs");
    assert_eq!(report.target.record_id, file_id("src/alpha.rs"));
    assert_eq!(report.target.change_count, 4);
    assert_eq!(report.scope.selector, "full_history");
    assert_eq!(report.scope.commit_count, 5);
    assert_eq!(report.min_support, CO_CHANGE_DEFAULT_MIN_SUPPORT);
    assert_eq!(report.limit, CO_CHANGE_DEFAULT_LIMIT);
    assert_eq!(report.coupling_metric, "jaccard_v1");

    // gamma (co=1) is suppressed by the default min-support threshold of 2.
    assert_eq!(partner_paths(&report), vec!["src/beta.rs", "src/delta.rs"]);
    assert_eq!(report.total_partners, 2);
    assert!(!report.truncated);

    let beta = &report.partners[0];
    assert_eq!(beta.record_id, file_id("src/beta.rs"));
    assert_eq!(beta.co_change_count, 3);
    assert_eq!(beta.partner_change_count, 3);
    assert_eq!(beta.target_change_count, 4);
    assert!((beta.coupling - 0.75).abs() < f64::EPSILON);
    assert!((beta.confidence - 0.75).abs() < f64::EPSILON);
    // Newest shared commit is the citable co-change handle.
    assert_eq!(beta.last_co_change_commit, "c3sha0000");
    assert_eq!(beta.last_co_change_valid_time, Some(T3));
    assert_eq!(beta.trust, "historical_co_change_lead");

    let delta = &report.partners[1];
    assert_eq!(delta.co_change_count, 2);
    assert_eq!(delta.partner_change_count, 3);
    assert!((delta.coupling - 0.4).abs() < f64::EPSILON);
    assert!((delta.confidence - 0.5).abs() < f64::EPSILON);
    assert_eq!(delta.last_co_change_commit, "c2sha0000");

    assert!(report.disclaimer.contains("not proof"));
    assert!(report.diagnostics.is_empty());
}

#[test]
fn coupling_min_support_one_admits_single_shared_commit_partners() {
    let records = synthetic_records();
    let mut opts = options();
    opts.min_support = 1;
    let report = co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("should resolve");
    assert_eq!(report.min_support, 1);
    assert_eq!(
        partner_paths(&report),
        vec!["src/beta.rs", "src/delta.rs", "src/gamma.rs"]
    );
    let gamma = &report.partners[2];
    assert_eq!(gamma.co_change_count, 1);
    assert!((gamma.coupling - 0.25).abs() < f64::EPSILON);
}

#[test]
fn coupling_limit_truncates_and_reports_completeness() {
    let records = synthetic_records();
    let mut opts = options();
    opts.limit = 1;
    let report = co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("should resolve");
    assert_eq!(report.limit, 1);
    assert_eq!(partner_paths(&report), vec!["src/beta.rs"]);
    assert_eq!(report.total_partners, 2);
    assert!(report.truncated);
}

#[test]
fn coupling_target_with_no_in_scope_changes_is_explicit_empty_success() {
    let records = synthetic_records();
    let report =
        co_change_coupling(&records, "src/eps.rs", None, &options()).expect("should resolve");
    assert_eq!(report.target.change_count, 0);
    assert!(report.partners.is_empty());
    assert_eq!(report.total_partners, 0);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "target_never_changed_in_scope")
    );
}

#[test]
fn coupling_no_partner_reaches_min_support_is_explicit_empty_success() {
    let records = synthetic_records();
    let mut opts = options();
    opts.min_support = 100;
    let report = co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("should resolve");
    assert!(report.partners.is_empty());
    assert_eq!(report.total_partners, 0);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "no_partner_at_or_above_min_support")
    );
}

#[test]
fn coupling_normalizes_target_path_spelling() {
    let records = synthetic_records();
    let report = co_change_coupling(&records, "./src\\alpha.rs", None, &options())
        .expect("normalized path should resolve");
    assert_eq!(report.target.repo_relative_path, "src/alpha.rs");
}

// ---------------------------------------------------------------------------
// Temporal selectors (range / --at / --as-of), consistent with issue #118
// ---------------------------------------------------------------------------

#[test]
fn coupling_commit_range_bounds_the_in_scope_commit_set() {
    let records = synthetic_records();
    let mut opts = options();
    opts.base = Some("c2sha0000");
    opts.head = Some("c4");
    opts.min_support = 1;
    let report = co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("should resolve");
    // Range (base, head] = {c3, c4}: alpha changed twice, beta once at c3.
    assert_eq!(report.scope.selector, "commit_range");
    assert_eq!(report.scope.base, Some("c2sha0000"));
    assert_eq!(report.scope.head, Some("c4sha0000"));
    assert_eq!(report.scope.commit_count, 2);
    assert_eq!(report.target.change_count, 2);
    assert_eq!(partner_paths(&report), vec!["src/beta.rs"]);
    let beta = &report.partners[0];
    assert_eq!(beta.co_change_count, 1);
    assert_eq!(beta.partner_change_count, 1);
    assert!((beta.coupling - 0.5).abs() < f64::EPSILON);
    assert!((beta.confidence - 0.5).abs() < f64::EPSILON);
}

#[test]
fn coupling_at_bound_scopes_to_history_reachable_from_commit() {
    let records = synthetic_records();
    let mut opts = options();
    opts.at = Some("c3");
    let report = co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("should resolve");
    assert_eq!(report.scope.selector, "at_commit");
    assert_eq!(report.scope.at, Some("c3sha0000"));
    assert_eq!(report.scope.commit_count, 3);
    assert_eq!(report.target.change_count, 3);
    assert_eq!(partner_paths(&report), vec!["src/beta.rs", "src/delta.rs"]);
    let beta = &report.partners[0];
    assert_eq!(beta.co_change_count, 3);
    assert!((beta.coupling - 1.0).abs() < f64::EPSILON);
}

#[test]
fn coupling_as_of_bound_scopes_by_valid_time_with_stable_tie_break() {
    let records = synthetic_records();
    let mut opts = options();
    opts.as_of = Some(T2);
    let report = co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("should resolve");
    assert_eq!(report.scope.selector, "as_of");
    assert_eq!(report.scope.as_of, Some(T2));
    assert_eq!(report.scope.commit_count, 2);
    assert_eq!(report.target.change_count, 2);
    // beta and delta are tied (co=2, jaccard=1.0): the documented tie-break
    // is ascending repo-relative path.
    assert_eq!(partner_paths(&report), vec!["src/beta.rs", "src/delta.rs"]);
}

// ---------------------------------------------------------------------------
// Machine-readable diagnostics (never silent empty output)
// ---------------------------------------------------------------------------

#[test]
fn coupling_unknown_path_errors() {
    let records = synthetic_records();
    let err = co_change_coupling(&records, "src/nope.rs", None, &options()).unwrap_err();
    match err {
        CoChangeCouplingError::UnknownFile { path } => assert_eq!(path, "src/nope.rs"),
        other => panic!("expected UnknownFile, got {other:?}"),
    }
}

#[test]
fn coupling_malformed_path_errors() {
    let records = synthetic_records();
    for raw in ["", "   ", "././"] {
        let err = co_change_coupling(&records, raw, None, &options()).unwrap_err();
        assert!(
            matches!(err, CoChangeCouplingError::MalformedPath { .. }),
            "expected MalformedPath for {raw:?}, got {err:?}"
        );
    }
}

#[test]
fn coupling_empty_history_errors() {
    let records = vec![file_node("src/alpha.rs", "c1sha0000", T1)];
    let err = co_change_coupling(&records, "src/alpha.rs", None, &options()).unwrap_err();
    assert!(matches!(err, CoChangeCouplingError::EmptyHistory));
}

#[test]
fn coupling_missing_and_ambiguous_commit_handles_error() {
    let records = synthetic_records();

    let mut opts = options();
    opts.base = Some("ffff");
    opts.head = Some("c4sha0000");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    match err {
        CoChangeCouplingError::MissingCommit { commit_prefix } => {
            assert_eq!(commit_prefix, "ffff");
        }
        other => panic!("expected MissingCommit, got {other:?}"),
    }

    let mut opts = options();
    opts.at = Some("c");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    match err {
        CoChangeCouplingError::AmbiguousCommitPrefix {
            commit_prefix,
            matches,
        } => {
            assert_eq!(commit_prefix, "c");
            assert_eq!(matches.len(), 5);
        }
        other => panic!("expected AmbiguousCommitPrefix, got {other:?}"),
    }
}

#[test]
fn coupling_reversed_and_identical_ranges_error() {
    let records = synthetic_records();

    let mut opts = options();
    opts.base = Some("c4sha0000");
    opts.head = Some("c2sha0000");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    assert!(matches!(err, CoChangeCouplingError::ReversedRange { .. }));

    let mut opts = options();
    opts.base = Some("c2");
    opts.head = Some("c2sha0000");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    assert!(matches!(
        err,
        CoChangeCouplingError::IdenticalEndpoints { .. }
    ));
}

#[test]
fn coupling_as_of_diagnostics() {
    let records = synthetic_records();

    let mut opts = options();
    opts.as_of = Some("not-a-timestamp");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    assert!(matches!(
        err,
        CoChangeCouplingError::InvalidAsOfTimestamp { .. }
    ));

    let mut opts = options();
    opts.as_of = Some("2020-01-01T00:00:00Z");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    assert!(matches!(
        err,
        CoChangeCouplingError::NoCommitAtOrBefore { .. }
    ));
}

#[test]
fn coupling_threshold_and_limit_bounds_are_validated() {
    let records = synthetic_records();

    for bad in [0, CO_CHANGE_MAX_MIN_SUPPORT + 1] {
        let mut opts = options();
        opts.min_support = bad;
        let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
        assert!(
            matches!(err, CoChangeCouplingError::InvalidMinSupport { .. }),
            "min_support={bad} must be rejected, got {err:?}"
        );
    }
    for bad in [0, CO_CHANGE_MAX_LIMIT + 1] {
        let mut opts = options();
        opts.limit = bad;
        let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
        assert!(
            matches!(err, CoChangeCouplingError::InvalidLimit { .. }),
            "limit={bad} must be rejected, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Determinism over synthetic history
// ---------------------------------------------------------------------------

#[test]
fn coupling_is_byte_identical_across_repeated_runs() {
    let records = synthetic_records();
    let baseline = serde_json::to_string(
        &co_change_coupling(&records, "src/alpha.rs", None, &options()).expect("should resolve"),
    )
    .expect("report should serialize");
    for _ in 0..4 {
        let again = serde_json::to_string(
            &co_change_coupling(&records, "src/alpha.rs", None, &options())
                .expect("should resolve"),
        )
        .expect("report should serialize");
        assert_eq!(baseline, again, "repeated runs must be byte-equivalent");
    }
}

// ---------------------------------------------------------------------------
// Seeded fixture repo: end-to-end through scan-history + CLI
// ---------------------------------------------------------------------------

/// Fixture history for the issue #153 success metric: `src/beta.rs` was
/// modified in the same commit as `src/alpha.rs` in 3 of alpha's 4 commits;
/// `src/gamma.rs` shares only the initial commit.
fn seed_coupling_fixture_repo(repo: &Path) -> [String; 5] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/alpha.rs", "pub fn alpha() -> u32 { 1 }\n");
    write(repo, "src/beta.rs", "pub fn beta() -> u32 { 1 }\n");
    write(repo, "src/gamma.rs", "pub fn gamma() -> u32 { 1 }\n");
    let c1 = commit_fixture(repo, "seed all files", T1);

    write(repo, "src/alpha.rs", "pub fn alpha() -> u32 { 2 }\n");
    write(repo, "src/beta.rs", "pub fn beta() -> u32 { 2 }\n");
    let c2 = commit_fixture(repo, "co-change alpha and beta", T2);

    write(repo, "src/alpha.rs", "pub fn alpha() -> u32 { 3 }\n");
    write(repo, "src/beta.rs", "pub fn beta() -> u32 { 3 }\n");
    let c3 = commit_fixture(repo, "co-change alpha and beta again", T3);

    write(repo, "src/alpha.rs", "pub fn alpha() -> u32 { 4 }\n");
    let c4 = commit_fixture(repo, "alpha alone", T4);

    write(repo, "src/gamma.rs", "pub fn gamma() -> u32 { 2 }\n");
    let c5 = commit_fixture(repo, "gamma alone", T5);

    [c1, c2, c3, c4, c5]
}

#[test]
fn coupling_fixture_repo_ranks_engineered_partner_first_and_leaves_tree_clean() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [_c1, _c2, c3, _c4, _c5] = seed_coupling_fixture_repo(&repo);

    // Untracked and ignored paths must never appear as target or partner.
    write(&repo, ".gitignore", "src/ignored.rs\n");
    git(&repo, ["add", ".gitignore"]);
    git(&repo, ["commit", "-m", "ignore rules"]);
    write(
        &repo,
        "src/untracked.rs",
        "pub fn untracked() -> u32 { 0 }\n",
    );
    write(&repo, "src/ignored.rs", "pub fn ignored() -> u32 { 0 }\n");

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let status_before = git_output(&repo, ["status", "--porcelain"]);

    let report = co_change_coupling(&records, "src/alpha.rs", None, &options())
        .expect("fixture target should resolve");

    // Success metric: beta is the #1 partner with co-change count 3 out of
    // alpha's 4 in-scope changes (confidence 3/4).
    assert_eq!(report.target.change_count, 4);
    assert!(!report.partners.is_empty());
    let top = &report.partners[0];
    assert_eq!(top.repo_relative_path, "src/beta.rs");
    assert_eq!(top.co_change_count, 3);
    assert_eq!(top.target_change_count, 4);
    assert!((top.confidence - 0.75).abs() < f64::EPSILON);
    assert_eq!(top.last_co_change_commit, c3);

    // gamma shares only one commit with alpha: suppressed at min support 2.
    assert!(partner_paths(&report).iter().all(|p| *p != "src/gamma.rs"));

    // Zero dangling handles: every partner resolves to a File node.
    for partner in &report.partners {
        assert!(
            records.iter().any(|r| matches!(
                r,
                GraphRecord::Node { id, kind: NodeKind::File, .. } if id == partner.record_id
            )),
            "partner {} must resolve to an existing File node",
            partner.repo_relative_path
        );
    }

    // Untracked / ignored paths never appear as partner or target.
    for path in ["src/untracked.rs", "src/ignored.rs"] {
        assert!(partner_paths(&report).iter().all(|p| *p != path));
        let err = co_change_coupling(&records, path, None, &options()).unwrap_err();
        assert!(matches!(err, CoChangeCouplingError::UnknownFile { .. }));
    }

    // Byte-identical output across 5 runs on unchanged history.
    let baseline = serde_json::to_string(&report).expect("report should serialize");
    for _ in 0..4 {
        let again = serde_json::to_string(
            &co_change_coupling(&records, "src/alpha.rs", None, &options())
                .expect("fixture target should resolve"),
        )
        .expect("report should serialize");
        assert_eq!(baseline, again, "repeated runs must be byte-equivalent");
    }

    // History-backed resolution reads Git objects only: clean tree in == out.
    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert_eq!(
        status_before, status_after,
        "query must not mutate the working tree"
    );
}

#[test]
fn query_coupling_cli_is_deterministic_and_redaction_safe() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [_c1, _c2, _c3, _c4, _c5] = seed_coupling_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let mut outputs = Vec::new();
    for _ in 0..5 {
        let assert = CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "coupling", "src/alpha.rs"])
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
    assert_eq!(body["min_support"], 2);
    assert_eq!(body["coupling_metric"], "jaccard_v1");
    assert_eq!(body["truncated"], false);
    assert_eq!(body["target"]["repo_relative_path"], "src/alpha.rs");
    assert_eq!(body["target"]["change_count"], 4);
    let partners = body["partners"].as_array().expect("partners array");
    assert_eq!(partners[0]["repo_relative_path"], "src/beta.rs");
    assert_eq!(partners[0]["co_change_count"], 3);
    assert_eq!(partners[0]["target_change_count"], 4);
    assert_eq!(partners[0]["trust"], "historical_co_change_lead");
    assert!(
        body["disclaimer"].as_str().unwrap().contains("not proof"),
        "disclaimer must label rows as historical leads"
    );

    // Redaction-safe: no blob contents or source text in the answer.
    assert!(!outputs[0].contains("-> u32"));
    assert!(!outputs[0].contains("Source:"));

    // Human-readable form renders without error and names the partner.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "coupling", "src/alpha.rs", "--format", "text"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(text.contains("src/beta.rs"));
    assert!(!text.contains("-> u32"));

    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(
        status_after.is_empty(),
        "CLI query must not mutate the tree"
    );
}

#[test]
fn query_coupling_cli_exit_codes_for_diagnostics() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [c1, _c2, _c3, _c4, c5] = seed_coupling_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Unknown path (no File node): exit 2, machine-readable diagnostic.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "coupling", "src/nope.rs"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["error_type"], "unknown_file");

    // Missing commit endpoint: exit 2.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query",
            "coupling",
            "src/alpha.rs",
            "--base",
            "ffffffffffff",
            "--head",
        ])
        .arg(&c5)
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
        .args(["query", "coupling", "src/alpha.rs", "--base"])
        .arg(&c5)
        .arg("--head")
        .arg(&c1)
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "reversed_range");

    // Out-of-bounds threshold and limit: exit 1.
    for args in [
        ["--min-support", "0"],
        ["--min-support", "101"],
        ["--limit", "0"],
        ["--limit", "501"],
    ] {
        CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "coupling", "src/alpha.rs"])
            .args(args)
            .arg("--graph")
            .arg(&graph_path)
            .assert()
            .code(1);
    }
}

// ---------------------------------------------------------------------------
// Git fixture helpers (mirrors tests/integration/range_deltas.rs)
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
