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

    // Corpus disclosure (issue #427): this hand-built fixture carries no
    // Repository source_snapshot, so head == union and the disclosure is
    // `single_snapshot`; the co-change analysis spans commits regardless.
    assert_eq!(report.corpus_mode, "single_snapshot");
    assert_eq!(report.corpus_mode_source, "default");
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

/// Repository scoping without `--repo` (Codex review on PR #312, P2 #1): in
/// an unscoped shared store two repositories can carry the same Git commit
/// SHA (forks, mirrored history). The per-file commit sets count bare SHAs,
/// so without owner gating a file from the *other* repository would surface
/// as a co-change partner even though it never changed in the target's
/// repository. Partners must be restricted to the target file's owning
/// repository.
#[test]
fn coupling_unscoped_multi_repo_store_never_bleeds_partners_across_repos() {
    let shared_sha = "shax0000";

    let repo_node = |repo_id: &str| {
        GraphRecord::node(
            repo_id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(repo_id.to_owned()),
            format!("Repository {repo_id}"),
        )
    };
    let commit_in = |repo_id: &str| {
        GraphRecord::node(
            stable_id(&["node", "commit", repo_id, shared_sha]),
            NodeKind::Commit,
            None,
            None,
            Some(shared_sha.to_owned()),
            format!("Commit {shared_sha} in {repo_id}"),
        )
        .with_temporal(temporal(shared_sha, &[], T1))
    };
    let file_in = |repo_id: &str, path: &str| {
        GraphRecord::node(
            stable_id(&["node", "file", repo_id, path]),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("File {path} in {repo_id}"),
        )
        .with_temporal(temporal(shared_sha, &[], T1))
    };
    let contains = |source: String, target: String| {
        GraphRecord::edge(
            EdgeLabel::Contains,
            source,
            target,
            Some("1.0".to_owned()),
            "containment".to_owned(),
        )
    };
    let changed_in_repo = |repo_id: &str, path: &str| {
        GraphRecord::edge(
            EdgeLabel::ChangedIn,
            stable_id(&["node", "file", repo_id, path]),
            stable_id(&["node", "commit", repo_id, shared_sha]),
            Some("1.0".to_owned()),
            format!("{path} changed in {shared_sha}"),
        )
        .with_temporal(temporal(shared_sha, &[], T1))
    };

    let records = vec![
        repo_node("repo_a"),
        repo_node("repo_b"),
        commit_in("repo_a"),
        commit_in("repo_b"),
        contains(
            "repo_a".to_owned(),
            stable_id(&["node", "commit", "repo_a", shared_sha]),
        ),
        contains(
            "repo_b".to_owned(),
            stable_id(&["node", "commit", "repo_b", shared_sha]),
        ),
        file_in("repo_a", "src/alpha.rs"),
        file_in("repo_b", "src/other.rs"),
        contains(
            "repo_a".to_owned(),
            stable_id(&["node", "file", "repo_a", "src/alpha.rs"]),
        ),
        contains(
            "repo_b".to_owned(),
            stable_id(&["node", "file", "repo_b", "src/other.rs"]),
        ),
        changed_in_repo("repo_a", "src/alpha.rs"),
        changed_in_repo("repo_b", "src/other.rs"),
    ];

    let mut opts = options();
    opts.min_support = 1;
    let report =
        co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("target should resolve");

    assert_eq!(report.target.change_count, 1);
    assert!(
        report.partners.is_empty(),
        "a file from another repository sharing only a commit SHA must never \
         appear as a partner, got {:?}",
        report
            .partners
            .iter()
            .map(|p| p.repo_relative_path)
            .collect::<Vec<_>>()
    );
}

/// Range endpoints resolve within the target's repository (Codex review on
/// PR #312, round 3): in an unscoped multi-repository store, `--base`/
/// `--head`, `--at`, and `--as-of` must be resolved against the target
/// file's owning repository — not every `Commit` node in the store — so
/// endpoints from another repository are rejected with the same
/// machine-readable errors the `--repo`-scoped path emits
/// (`missing_commit` / `no_commit_at_or_before`), never answered with a
/// misleading zero-change empty success.
#[test]
#[allow(clippy::too_many_lines)]
fn coupling_unscoped_multi_repo_store_rejects_endpoints_from_another_repo() {
    let repo_node = |repo_id: &str| {
        GraphRecord::node(
            repo_id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(repo_id.to_owned()),
            format!("Repository {repo_id}"),
        )
    };
    let commit_in = |repo_id: &str, sha: &str, parents: &[&str], vt: &str| {
        GraphRecord::node(
            stable_id(&["node", "commit", repo_id, sha]),
            NodeKind::Commit,
            None,
            None,
            Some(sha.to_owned()),
            format!("Commit {sha} in {repo_id}"),
        )
        .with_temporal(temporal(sha, parents, vt))
    };
    let file_in = |repo_id: &str, path: &str, sha: &str, vt: &str| {
        GraphRecord::node(
            stable_id(&["node", "file", repo_id, path]),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("File {path} in {repo_id}"),
        )
        .with_temporal(temporal(sha, &[], vt))
    };
    let contains = |source: String, target: String| {
        GraphRecord::edge(
            EdgeLabel::Contains,
            source,
            target,
            Some("1.0".to_owned()),
            "containment".to_owned(),
        )
    };
    let changed_in_repo = |repo_id: &str, path: &str, sha: &str, vt: &str| {
        GraphRecord::edge(
            EdgeLabel::ChangedIn,
            stable_id(&["node", "file", repo_id, path]),
            stable_id(&["node", "commit", repo_id, sha]),
            Some("1.0".to_owned()),
            format!("{path} changed in {sha}"),
        )
        .with_temporal(temporal(sha, &[], vt))
    };

    // Repo B's history (bbbb1111 -> bbbb2222) predates repo A's
    // (aaaa1111 -> aaaa2222); the target lives in repo A only.
    let records = vec![
        repo_node("repo_a"),
        repo_node("repo_b"),
        commit_in("repo_a", "aaaa1111", &[], T3),
        commit_in("repo_a", "aaaa2222", &["aaaa1111"], T4),
        commit_in("repo_b", "bbbb1111", &[], T1),
        commit_in("repo_b", "bbbb2222", &["bbbb1111"], T2),
        contains(
            "repo_a".to_owned(),
            stable_id(&["node", "commit", "repo_a", "aaaa1111"]),
        ),
        contains(
            "repo_a".to_owned(),
            stable_id(&["node", "commit", "repo_a", "aaaa2222"]),
        ),
        contains(
            "repo_b".to_owned(),
            stable_id(&["node", "commit", "repo_b", "bbbb1111"]),
        ),
        contains(
            "repo_b".to_owned(),
            stable_id(&["node", "commit", "repo_b", "bbbb2222"]),
        ),
        file_in("repo_a", "src/alpha.rs", "aaaa1111", T3),
        file_in("repo_b", "src/other.rs", "bbbb1111", T1),
        contains(
            "repo_a".to_owned(),
            stable_id(&["node", "file", "repo_a", "src/alpha.rs"]),
        ),
        contains(
            "repo_b".to_owned(),
            stable_id(&["node", "file", "repo_b", "src/other.rs"]),
        ),
        changed_in_repo("repo_a", "src/alpha.rs", "aaaa1111", T3),
        changed_in_repo("repo_a", "src/alpha.rs", "aaaa2222", T4),
        changed_in_repo("repo_b", "src/other.rs", "bbbb1111", T1),
        changed_in_repo("repo_b", "src/other.rs", "bbbb2222", T2),
    ];

    // --base/--head from the other repository: missing_commit, never a
    // zero-change empty success.
    let mut opts = options();
    opts.base = Some("bbbb1111");
    opts.head = Some("bbbb2222");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    match err {
        CoChangeCouplingError::MissingCommit { commit_prefix } => {
            assert_eq!(commit_prefix, "bbbb1111");
        }
        other => panic!("expected MissingCommit for a foreign-repo base, got {other:?}"),
    }

    // --at from the other repository: missing_commit.
    let mut opts = options();
    opts.at = Some("bbbb2222");
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    assert!(
        matches!(err, CoChangeCouplingError::MissingCommit { .. }),
        "expected MissingCommit for a foreign-repo --at, got {err:?}"
    );

    // --as-of before every commit of the target's repository: the other
    // repository's older commits must not satisfy the bound.
    let mut opts = options();
    opts.as_of = Some(T2);
    let err = co_change_coupling(&records, "src/alpha.rs", None, &opts).unwrap_err();
    assert!(
        matches!(err, CoChangeCouplingError::NoCommitAtOrBefore { .. }),
        "expected NoCommitAtOrBefore when only foreign-repo commits predate the instant, got {err:?}"
    );

    // The full-history commit universe is the target repository's timeline.
    let mut opts = options();
    opts.min_support = 1;
    let report =
        co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("target should resolve");
    assert_eq!(report.scope.commit_count, 2);
    assert_eq!(report.target.change_count, 2);
    assert!(report.partners.is_empty());

    // Same-repo endpoints keep working unscoped.
    let mut opts = options();
    opts.base = Some("aaaa1111");
    opts.head = Some("aaaa2222");
    opts.min_support = 1;
    let report =
        co_change_coupling(&records, "src/alpha.rs", None, &opts).expect("range should resolve");
    assert_eq!(report.scope.commit_count, 1);
    assert_eq!(report.target.change_count, 1);
}

/// Chronological last-co-change selection (Codex review on PR #312, P2 #2):
/// `scan-history` preserves non-UTC committer offsets (`normalize_timestamp`
/// only rewrites `+00:00` to `Z`), so `2026-01-01T23:30:00-05:00` is
/// chronologically *later* than `2026-01-02T02:00:00Z` yet sorts *earlier*
/// as a string. The newest-shared-commit pick must parse valid times the
/// way the `--as-of` path does, with the SHA tie-break applied on equal
/// instants.
#[test]
fn coupling_last_co_change_commit_compares_valid_times_chronologically() {
    const EARLY_UTC: &str = "2026-01-02T02:00:00Z"; // lexicographically LATER
    const LATE_OFFSET: &str = "2026-01-01T23:30:00-05:00"; // = 04:30Z, chronologically LATER

    let records = vec![
        commit("cearly000", &[], EARLY_UTC),
        commit("clate0000", &["cearly000"], LATE_OFFSET),
        file_node("src/alpha.rs", "cearly000", EARLY_UTC),
        file_node("src/beta.rs", "cearly000", EARLY_UTC),
        changed_in("src/alpha.rs", "cearly000", EARLY_UTC),
        changed_in("src/beta.rs", "cearly000", EARLY_UTC),
        changed_in("src/alpha.rs", "clate0000", LATE_OFFSET),
        changed_in("src/beta.rs", "clate0000", LATE_OFFSET),
    ];

    let report = co_change_coupling(&records, "src/alpha.rs", None, &options())
        .expect("target should resolve");
    let beta = report
        .partners
        .iter()
        .find(|p| p.repo_relative_path == "src/beta.rs")
        .expect("beta must be a partner");
    assert_eq!(beta.co_change_count, 2);
    assert_eq!(
        beta.last_co_change_commit, "clate0000",
        "the chronologically newest shared commit must win even when its \
         non-UTC valid time sorts earlier as a string"
    );
    assert_eq!(beta.last_co_change_valid_time, Some(LATE_OFFSET));
}

/// End-to-end proof of the offset premise: a fixture repo committed with a
/// `-05:00` committer date keeps that offset through `scan-history`, and the
/// coupling answer picks the chronologically newest shared commit.
#[test]
fn coupling_fixture_repo_with_offset_committer_dates_picks_chronological_last() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");

    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(&repo, ["config", "user.name", "Codegraph Test"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    write(&repo, "src/alpha.rs", "pub fn alpha() -> u32 { 1 }\n");
    write(&repo, "src/beta.rs", "pub fn beta() -> u32 { 1 }\n");
    let _c1 = commit_fixture(&repo, "seed together", "2026-01-02T02:00:00Z");

    write(&repo, "src/alpha.rs", "pub fn alpha() -> u32 { 2 }\n");
    write(&repo, "src/beta.rs", "pub fn beta() -> u32 { 2 }\n");
    // Chronologically later (04:30Z), lexicographically earlier.
    let c2 = commit_fixture(&repo, "change together", "2026-01-01T23:30:00-05:00");

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let report = co_change_coupling(&records, "src/alpha.rs", None, &options())
        .expect("target should resolve");
    let beta = report
        .partners
        .iter()
        .find(|p| p.repo_relative_path == "src/beta.rs")
        .expect("beta must be a partner");
    assert_eq!(beta.co_change_count, 2);
    assert_eq!(beta.last_co_change_commit, c2);
    assert_eq!(
        beta.last_co_change_valid_time,
        Some("2026-01-01T23:30:00-05:00"),
        "scan-history must preserve the non-UTC committer offset"
    );
}

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

/// Deletion commits count as changes (Codex review on PR #312): a file
/// deleted in a commit has a `Change` record for that commit but no `File`
/// snapshot and no `CHANGED_IN` edge, because `scan-history` only replays
/// paths present in the commit's tree. Co-deletion is real co-change — a
/// struct and its fixture removed together is exactly the hidden-coupling
/// signal this query exists for — so the index must fold deletion `Change`
/// records into the per-file commit sets.
#[test]
fn coupling_counts_co_deletion_commits_from_change_records() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");

    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(&repo, ["config", "user.name", "Codegraph Test"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    // c1: alpha and beta are born together (gamma is control noise).
    write(&repo, "src/alpha.rs", "pub fn alpha() -> u32 { 1 }\n");
    write(&repo, "src/beta.rs", "pub fn beta() -> u32 { 1 }\n");
    write(&repo, "src/gamma.rs", "pub fn gamma() -> u32 { 1 }\n");
    let _c1 = commit_fixture(&repo, "add alpha and beta together", T1);

    // c2: unrelated churn only.
    write(&repo, "src/gamma.rs", "pub fn gamma() -> u32 { 2 }\n");
    let _c2 = commit_fixture(&repo, "gamma alone", T2);

    // c3: alpha and beta die together.
    fs::remove_file(repo.join("src/alpha.rs")).expect("alpha should be removed");
    fs::remove_file(repo.join("src/beta.rs")).expect("beta should be removed");
    let c3 = commit_fixture(&repo, "delete alpha and beta together", T3);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    // Evidence for the record-shape claim: the deletion commit carries a
    // `D`-status Change record for alpha but no File snapshot (and thus no
    // CHANGED_IN edge) at that commit.
    let alpha_delete_change = records.iter().any(|r| {
        matches!(
            r,
            GraphRecord::Node {
                kind: NodeKind::Change,
                name: Some(name),
                temporal: Some(t),
                ..
            } if name == "D src/alpha.rs" && t.git_commit == c3
        )
    });
    assert!(
        alpha_delete_change,
        "scan-history must record the deletion as a Change node"
    );
    let alpha_snapshot_at_c3 = records.iter().any(|r| {
        matches!(
            r,
            GraphRecord::Node {
                kind: NodeKind::File,
                repo_relative_path: Some(p),
                temporal: Some(t),
                ..
            } if p == "src/alpha.rs" && t.git_commit == c3
        )
    });
    assert!(
        !alpha_snapshot_at_c3,
        "a deleted path has no File snapshot at the deletion commit"
    );

    // The deletion commit counts toward change_count and co-change: alpha
    // changed in {c1, c3}, beta in {c1, c3}, so at the default min-support
    // of 2 beta is a partner with co=2 and confidence 1.0, and the newest
    // shared commit is the co-deletion commit.
    let report = co_change_coupling(&records, "src/alpha.rs", None, &options())
        .expect("deleted target should still resolve to its File node");
    assert_eq!(
        report.target.change_count, 2,
        "deletion must count as a change of the target"
    );
    let beta = report
        .partners
        .iter()
        .find(|p| p.repo_relative_path == "src/beta.rs")
        .expect("co-deleted partner must survive the min-support threshold");
    assert_eq!(beta.co_change_count, 2);
    assert_eq!(beta.partner_change_count, 2);
    assert!((beta.coupling - 1.0).abs() < f64::EPSILON);
    assert!((beta.confidence - 1.0).abs() < f64::EPSILON);
    assert_eq!(
        beta.last_co_change_commit, c3,
        "the co-deletion commit is the newest shared commit"
    );

    // gamma never co-changed with alpha above the threshold.
    assert!(
        report
            .partners
            .iter()
            .all(|p| p.repo_relative_path != "src/gamma.rs"),
        "control file must stay suppressed"
    );

    // Determinism holds with Change-record folding in the index.
    let baseline = serde_json::to_string(&report).expect("report should serialize");
    for _ in 0..4 {
        let again = serde_json::to_string(
            &co_change_coupling(&records, "src/alpha.rs", None, &options())
                .expect("deleted target should still resolve"),
        )
        .expect("report should serialize");
        assert_eq!(baseline, again, "repeated runs must be byte-equivalent");
    }

    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(status_after.is_empty(), "query must not mutate the tree");
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
