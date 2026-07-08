//! Integration tests for `eg query ownership` (issue #245): per-file Git
//! authorship aggregated into ownership shares, a primary owner, and a
//! bus-factor signal.
#![allow(missing_docs)]

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{
    GraphRecord, NodeKind, TemporalMetadata,
    bundle::scrub_record,
    query::{
        OWNERSHIP_DEFAULT_LIMIT, OWNERSHIP_DEFAULT_THRESHOLD_PERCENT, OWNERSHIP_MAX_LIMIT,
        OwnershipError, OwnershipMap, OwnershipOptions, ownership_map,
    },
    scan_repository_history, stable_id,
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

fn commit(
    sha: &str,
    parents: &[&str],
    valid_time: &str,
    author_name: &str,
    author_email: &str,
) -> GraphRecord {
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
    .with_author(Some(author_name.to_owned()), Some(author_email.to_owned()))
}

fn file_snapshot(path: &str, commit: &str, valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "file", "repo_test", path]);
    GraphRecord::node(
        id,
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("Rust source file {path}"),
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

fn change(path: &str, status: &str, commit: &str, valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "change", "repo_test", commit, status, path]);
    GraphRecord::node(
        id,
        NodeKind::Change,
        Some(path.to_owned()),
        None,
        Some(format!("{status} {path}")),
        format!("Git change {status} to {path} in commit {commit}"),
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";
const T3: &str = "2026-01-03T00:00:00Z";
const T4: &str = "2026-01-04T00:00:00Z";
const T5: &str = "2026-01-05T00:00:00Z";

/// Five linear commits (alice, bob, carol, alice, bob) with an engineered
/// authorship distribution:
///
/// - `src/hot.rs`: changed in every commit → alice 2 (c1, c4), bob 2
///   (c2, c5), carol 1 (c3); alice/bob tie on count and the tie breaks to
///   the lexicographically smallest author identity (alice).
/// - `src/shared.rs`: alice (c1), bob (c2), carol (c3) — one commit each →
///   three-way tie, bus factor 2, primary owner alice by tie-break.
/// - `src/gone.rs`: exists only at c1..c2 (deleted in c3) → never a row at
///   HEAD, still a row with `--at <c2>`.
fn synthetic_ownership_records() -> Vec<GraphRecord> {
    vec![
        commit("c1sha0000", &[], T1, "Alice Dev", "alice@example.com"),
        commit(
            "c2sha0000",
            &["c1sha0000"],
            T2,
            "Bob Dev",
            "bob@example.com",
        ),
        commit(
            "c3sha0000",
            &["c2sha0000"],
            T3,
            "Carol Dev",
            "carol@example.com",
        ),
        commit(
            "c4sha0000",
            &["c3sha0000"],
            T4,
            "Alice Dev",
            "alice@example.com",
        ),
        commit(
            "c5sha0000",
            &["c4sha0000"],
            T5,
            "Bob Dev",
            "bob@example.com",
        ),
        // src/hot.rs: changed in every commit.
        file_snapshot("src/hot.rs", "c1sha0000", T1),
        file_snapshot("src/hot.rs", "c2sha0000", T2),
        file_snapshot("src/hot.rs", "c3sha0000", T3),
        file_snapshot("src/hot.rs", "c4sha0000", T4),
        file_snapshot("src/hot.rs", "c5sha0000", T5),
        change("src/hot.rs", "A", "c1sha0000", T1),
        change("src/hot.rs", "M", "c2sha0000", T2),
        change("src/hot.rs", "M", "c3sha0000", T3),
        change("src/hot.rs", "M", "c4sha0000", T4),
        change("src/hot.rs", "M", "c5sha0000", T5),
        // src/shared.rs: alice (c1), bob (c2), carol (c3).
        file_snapshot("src/shared.rs", "c1sha0000", T1),
        file_snapshot("src/shared.rs", "c2sha0000", T2),
        file_snapshot("src/shared.rs", "c3sha0000", T3),
        file_snapshot("src/shared.rs", "c4sha0000", T4),
        file_snapshot("src/shared.rs", "c5sha0000", T5),
        change("src/shared.rs", "A", "c1sha0000", T1),
        change("src/shared.rs", "M", "c2sha0000", T2),
        change("src/shared.rs", "M", "c3sha0000", T3),
        // src/gone.rs: present at c1..c2 only.
        file_snapshot("src/gone.rs", "c1sha0000", T1),
        file_snapshot("src/gone.rs", "c2sha0000", T2),
        change("src/gone.rs", "A", "c1sha0000", T1),
        change("src/gone.rs", "D", "c3sha0000", T3),
    ]
}

/// The engineered distribution above gives `src/hot.rs` these per-author
/// commit counts at HEAD: alice 2 (c1, c4), bob 2 (c2, c5), carol 1 (c3).
/// alice/bob tie on count; the tie breaks to the smaller author identity.
const HOT_TOTAL: usize = 5;

fn options<'q>() -> OwnershipOptions<'q> {
    OwnershipOptions {
        path: None,
        at_commit: None,
        as_of: None,
        repo_scope: None,
        threshold_percent: OWNERSHIP_DEFAULT_THRESHOLD_PERCENT,
        limit: OWNERSHIP_DEFAULT_LIMIT,
    }
}

// ---------------------------------------------------------------------------
// Diagnostics (machine-readable failures, never silent empty output)
// ---------------------------------------------------------------------------

#[test]
fn ownership_empty_history_errors() {
    let records = vec![file_snapshot("src/lib.rs", "c1sha0000", T1)];
    let err = ownership_map(&records, &options()).unwrap_err();
    assert!(matches!(err, OwnershipError::EmptyHistory));
}

#[test]
fn ownership_missing_commit_errors() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.at_commit = Some("ffff");
    let err = ownership_map(&records, &opts).unwrap_err();
    match err {
        OwnershipError::MissingCommit { commit_prefix } => assert_eq!(commit_prefix, "ffff"),
        other => panic!("expected MissingCommit, got {other:?}"),
    }
}

#[test]
fn ownership_ambiguous_prefix_errors() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.at_commit = Some("c");
    let err = ownership_map(&records, &opts).unwrap_err();
    match err {
        OwnershipError::AmbiguousCommitPrefix {
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
fn ownership_malformed_timestamp_errors() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.as_of = Some("not-a-timestamp");
    let err = ownership_map(&records, &opts).unwrap_err();
    assert!(matches!(err, OwnershipError::MalformedTimestamp { .. }));
}

#[test]
fn ownership_as_of_before_all_commits_errors() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.as_of = Some("2020-01-01T00:00:00Z");
    let err = ownership_map(&records, &opts).unwrap_err();
    assert!(matches!(err, OwnershipError::NoCommitsAtTime { .. }));
}

#[test]
fn ownership_unknown_path_errors() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.path = Some("src/does_not_exist.rs");
    let err = ownership_map(&records, &opts).unwrap_err();
    match err {
        OwnershipError::UnknownPath { path } => assert_eq!(path, "src/does_not_exist.rs"),
        other => panic!("expected UnknownPath, got {other:?}"),
    }
}

#[test]
fn ownership_invalid_threshold_errors() {
    let records = synthetic_ownership_records();
    for bad in [0_u32, 101] {
        let mut opts = options();
        opts.threshold_percent = bad;
        let err = ownership_map(&records, &opts).unwrap_err();
        assert!(
            matches!(err, OwnershipError::InvalidThreshold { .. }),
            "threshold {bad} must be rejected"
        );
    }
}

#[test]
fn ownership_invalid_limit_errors() {
    let records = synthetic_ownership_records();
    for bad in [0_usize, OWNERSHIP_MAX_LIMIT + 1] {
        let mut opts = options();
        opts.limit = bad;
        let err = ownership_map(&records, &opts).unwrap_err();
        assert!(
            matches!(err, OwnershipError::InvalidLimit { .. }),
            "limit {bad} must be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// Aggregation, shares, primary owner, bus factor
// ---------------------------------------------------------------------------

#[test]
fn ownership_aggregates_shares_and_bus_factor() {
    let records = synthetic_ownership_records();
    let map = ownership_map(&records, &options()).expect("ownership should resolve");

    assert_eq!(map.threshold_percent, 50);
    assert_eq!(map.total_file_count, 2, "gone file must not appear at HEAD");
    assert_eq!(map.returned_file_count, 2);
    assert!(!map.truncated);

    let hot = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/hot.rs")
        .expect("src/hot.rs row");
    assert_eq!(hot.total_commits, HOT_TOTAL);
    assert_eq!(
        hot.record_id,
        stable_id(&["node", "file", "repo_test", "src/hot.rs"]),
        "file handle must be the File node's stable record ID"
    );
    // alice 2, bob 2, carol 1 → ranked alice, bob, carol.
    let identities: Vec<(Option<&str>, usize)> = hot
        .authors
        .iter()
        .map(|a| (a.author_email, a.commits))
        .collect();
    assert_eq!(
        identities,
        vec![
            (Some("alice@example.com"), 2),
            (Some("bob@example.com"), 2),
            (Some("carol@example.com"), 1),
        ]
    );
    let shares: Vec<f64> = hot.authors.iter().map(|a| a.share).collect();
    assert!((shares[0] - 0.4).abs() < f64::EPSILON);
    assert!((shares[1] - 0.4).abs() < f64::EPSILON);
    assert!((shares[2] - 0.2).abs() < f64::EPSILON);
    // Count tie: primary owner breaks to the smaller identity.
    assert_eq!(hot.primary_owner.author_email, Some("alice@example.com"));
    // 40% < 50% ≤ 80% → two top authors needed.
    assert_eq!(hot.bus_factor, 2);

    let shared = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/shared.rs")
        .expect("src/shared.rs row");
    assert_eq!(shared.total_commits, 3);
    assert_eq!(shared.authors.len(), 3);
    for author in &shared.authors {
        assert_eq!(author.commits, 1);
        assert!((author.share - 1.0 / 3.0).abs() < f64::EPSILON);
    }
    assert_eq!(
        shared.primary_owner.author_email,
        Some("alice@example.com"),
        "three-way tie must break to the lexicographically smallest identity"
    );
    assert_eq!(shared.bus_factor, 2, "1/3 < 50% ≤ 2/3");

    // The disclaimer labels rows as empirical history, never authority.
    assert!(map.disclaimer.contains("not declared ownership"));
}

#[test]
fn ownership_single_dominant_author_has_bus_factor_one() {
    // A file authored 80% by one person reports bus-factor 1 (issue #245 AC).
    let records = vec![
        commit("c1sha0000", &[], T1, "Alice Dev", "alice@example.com"),
        commit(
            "c2sha0000",
            &["c1sha0000"],
            T2,
            "Alice Dev",
            "alice@example.com",
        ),
        commit(
            "c3sha0000",
            &["c2sha0000"],
            T3,
            "Alice Dev",
            "alice@example.com",
        ),
        commit(
            "c4sha0000",
            &["c3sha0000"],
            T4,
            "Alice Dev",
            "alice@example.com",
        ),
        commit(
            "c5sha0000",
            &["c4sha0000"],
            T5,
            "Bob Dev",
            "bob@example.com",
        ),
        file_snapshot("src/solo.rs", "c5sha0000", T5),
        change("src/solo.rs", "A", "c1sha0000", T1),
        change("src/solo.rs", "M", "c2sha0000", T2),
        change("src/solo.rs", "M", "c3sha0000", T3),
        change("src/solo.rs", "M", "c4sha0000", T4),
        change("src/solo.rs", "M", "c5sha0000", T5),
    ];
    let map = ownership_map(&records, &options()).expect("ownership should resolve");
    let solo = &map.files[0];
    assert_eq!(solo.primary_owner.author_email, Some("alice@example.com"));
    assert!((solo.primary_owner.share - 0.8).abs() < f64::EPSILON);
    assert_eq!(solo.bus_factor, 1);
}

#[test]
fn ownership_counts_distinct_commits_not_change_rows() {
    // A merge-style commit records one Change row per parent diff; the same
    // commit must still count once toward the author.
    let records = vec![
        commit("c1sha0000", &[], T1, "Alice Dev", "alice@example.com"),
        file_snapshot("src/lib.rs", "c1sha0000", T1),
        change("src/lib.rs", "A", "c1sha0000", T1),
        change("src/lib.rs", "M", "c1sha0000", T1),
    ];
    let map = ownership_map(&records, &options()).expect("ownership should resolve");
    let row = &map.files[0];
    assert_eq!(row.total_commits, 1);
    assert_eq!(row.authors.len(), 1);
    assert_eq!(row.authors[0].commits, 1);
    assert!((row.authors[0].share - 1.0).abs() < f64::EPSILON);
    assert_eq!(row.bus_factor, 1);
}

#[test]
fn ownership_path_filter_returns_single_row() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.path = Some("src/shared.rs");
    let map = ownership_map(&records, &opts).expect("ownership should resolve");
    assert_eq!(map.total_file_count, 1);
    assert_eq!(map.files.len(), 1);
    assert_eq!(map.files[0].repo_relative_path, "src/shared.rs");
}

#[test]
fn ownership_threshold_is_honored() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.threshold_percent = 100;
    let map = ownership_map(&records, &opts).expect("ownership should resolve");
    let hot = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/hot.rs")
        .expect("src/hot.rs row");
    assert_eq!(hot.bus_factor, 3, "100% needs every author");
    assert_eq!(map.threshold_percent, 100);
}

#[test]
fn ownership_limit_truncates_with_signal() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.limit = 1;
    let map = ownership_map(&records, &opts).expect("ownership should resolve");
    assert_eq!(map.total_file_count, 2);
    assert_eq!(map.returned_file_count, 1);
    assert_eq!(map.files.len(), 1);
    assert!(map.truncated, "the answer must state it was truncated");
}

// ---------------------------------------------------------------------------
// Temporal selectors
// ---------------------------------------------------------------------------

#[test]
fn ownership_at_commit_reports_state_as_of_that_commit() {
    let records = synthetic_ownership_records();
    let mut opts = options();
    opts.at_commit = Some("c2sha0000");
    let map = ownership_map(&records, &opts).expect("ownership should resolve");

    // src/gone.rs still exists at c2 and gets a row.
    let gone = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/gone.rs")
        .expect("src/gone.rs must appear as-of c2");
    assert_eq!(gone.total_commits, 1);
    assert_eq!(gone.primary_owner.author_email, Some("alice@example.com"));

    // src/hot.rs only counts c1..c2: alice 1, bob 1.
    let hot = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/hot.rs")
        .expect("src/hot.rs row");
    assert_eq!(hot.total_commits, 2);
    assert_eq!(hot.bus_factor, 1, "1/2 reaches the 50% threshold");
    assert_eq!(hot.primary_owner.author_email, Some("alice@example.com"));

    // The anchor is reported so the answer is citable.
    assert_eq!(map.anchors.len(), 1);
    assert_eq!(map.anchors[0].commit_sha, "c2sha0000");
}

#[test]
fn ownership_as_of_time_matches_at_commit_view() {
    let records = synthetic_ownership_records();
    let mut at_opts = options();
    at_opts.at_commit = Some("c2sha0000");
    let at_map = ownership_map(&records, &at_opts).expect("at view should resolve");

    let mut as_of_opts = options();
    as_of_opts.as_of = Some("2026-01-02T12:00:00Z");
    let as_of_map = ownership_map(&records, &as_of_opts).expect("as-of view should resolve");

    let at_json = serde_json::to_string(&at_map).expect("serialize");
    let as_of_json = serde_json::to_string(&as_of_map).expect("serialize");
    assert_eq!(
        at_json, as_of_json,
        "--as-of between c2 and c3 must equal the --at c2 view"
    );
}

// ---------------------------------------------------------------------------
// Determinism (byte-identical across repeated runs)
// ---------------------------------------------------------------------------

#[test]
fn ownership_is_byte_identical_across_runs() {
    let records = synthetic_ownership_records();
    let baseline = serde_json::to_string(&ownership_map(&records, &options()).expect("resolve"))
        .expect("serialize");
    for _ in 0..4 {
        let again: OwnershipMap<'_> = ownership_map(&records, &options()).expect("resolve");
        assert_eq!(
            baseline,
            serde_json::to_string(&again).expect("serialize"),
            "repeated runs must be byte-equivalent"
        );
    }
}

// ---------------------------------------------------------------------------
// Redaction (issue #245 AC: author email is redaction-eligible PII)
// ---------------------------------------------------------------------------

#[test]
fn ownership_over_redacted_export_carries_markers_never_raw_emails() {
    let records: Vec<GraphRecord> = synthetic_ownership_records()
        .into_iter()
        .map(scrub_record)
        .collect();
    let map = ownership_map(&records, &options()).expect("ownership should resolve");
    let json = serde_json::to_string(&map).expect("serialize");
    assert!(
        !json.contains("@example.com"),
        "a redaction-on export must contain zero raw email addresses"
    );
    assert!(
        json.contains("<REDACTED:email:"),
        "redacted identities must surface as markers, not disappear"
    );
    // Aggregation still groups deterministically: the marker is stable per
    // raw address, so the hot file still ranks three distinct identities.
    let hot = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/hot.rs")
        .expect("src/hot.rs row");
    assert_eq!(hot.authors.len(), 3);
    assert_eq!(hot.bus_factor, 2);
}

#[test]
fn ownership_local_store_retains_raw_emails() {
    let records = synthetic_ownership_records();
    let map = ownership_map(&records, &options()).expect("ownership should resolve");
    let json = serde_json::to_string(&map).expect("serialize");
    assert!(
        json.contains("alice@example.com"),
        "a redaction-off local store retains raw author emails"
    );
}

// ---------------------------------------------------------------------------
// Fixture repo end-to-end: scan-history + CLI
// ---------------------------------------------------------------------------

struct FixtureAuthor {
    name: &'static str,
    email: &'static str,
}

const ALICE: FixtureAuthor = FixtureAuthor {
    name: "Alice Dev",
    email: "alice@example.invalid",
};
const BOB: FixtureAuthor = FixtureAuthor {
    name: "Bob Dev",
    email: "bob@example.invalid",
};
const CAROL: FixtureAuthor = FixtureAuthor {
    name: "Carol Dev",
    email: "carol@example.invalid",
};

/// Seeds a repo with an engineered authorship distribution:
///
/// - `src/hot.rs`: alice authors 4 of 5 touching commits (80%) → bus factor 1.
/// - `src/shared.rs`: alice, bob, carol author one commit each → bus factor 2.
/// - `README.md`: tracked but not an indexed source file → never a row.
/// - `ignored.tmp`: listed in `.gitignore`, never committed → never a row.
fn seed_ownership_fixture_repo(repo: &Path) -> Vec<String> {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, ".gitignore", "ignored.tmp\n");
    write(repo, "ignored.tmp", "never committed\n");
    write(repo, "README.md", "# fixture\n");
    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 1 }\n");
    write(repo, "src/shared.rs", "pub fn shared() -> u32 { 1 }\n");
    let c1 = commit_fixture(repo, "seed", "2026-01-01T00:00:00Z", &ALICE);

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 2 }\n");
    write(repo, "src/shared.rs", "pub fn shared() -> u32 { 2 }\n");
    let c2 = commit_fixture(repo, "bob touches", "2026-01-02T00:00:00Z", &BOB);

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 3 }\n");
    write(repo, "src/shared.rs", "pub fn shared() -> u32 { 3 }\n");
    let c3 = commit_fixture(repo, "carol touches", "2026-01-03T00:00:00Z", &CAROL);

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 4 }\n");
    let c4 = commit_fixture(repo, "alice again", "2026-01-04T00:00:00Z", &ALICE);

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 5 }\n");
    let c5 = commit_fixture(repo, "alice again", "2026-01-05T00:00:00Z", &ALICE);

    write(repo, "src/hot.rs", "pub fn hot() -> u32 { 6 }\n");
    let c6 = commit_fixture(repo, "alice again", "2026-01-06T00:00:00Z", &ALICE);

    vec![c1, c2, c3, c4, c5, c6]
}

#[test]
fn ownership_fixture_repo_end_to_end() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let commits = seed_ownership_fixture_repo(&repo);

    // `ignored.tmp` is gitignored, so porcelain status is fully clean.
    let status_before = git_output(&repo, ["status", "--porcelain"]);
    assert_eq!(status_before, "", "fixture tree must start clean");

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let map = ownership_map(&records, &options()).expect("ownership should resolve");

    // 100% correct primary owner and bus factor on the engineered fixture.
    let hot = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/hot.rs")
        .expect("src/hot.rs row");
    // hot: alice c1, c4, c5, c6 (4); bob c2; carol c3 → 4/6 ≥ 50% → 1.
    assert_eq!(hot.total_commits, 6);
    assert_eq!(hot.primary_owner.author_email, Some(ALICE.email));
    assert_eq!(hot.primary_owner.author_name, Some(ALICE.name));
    assert_eq!(hot.primary_owner.commits, 4);
    assert_eq!(hot.bus_factor, 1);

    let shared = map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/shared.rs")
        .expect("src/shared.rs row");
    // shared: alice c1, bob c2, carol c3 → tie → alice by identity, bus 2.
    assert_eq!(shared.total_commits, 3);
    assert_eq!(shared.primary_owner.author_email, Some(ALICE.email));
    assert_eq!(shared.bus_factor, 2);

    // Non-indexed and ignored paths never appear (0 dangling file handles).
    assert!(
        map.files
            .iter()
            .all(|f| f.repo_relative_path != "README.md" && f.repo_relative_path != "ignored.tmp"),
        "non-source and ignored paths must never appear"
    );
    // Every file handle resolves to an existing File node in the store.
    for row in &map.files {
        assert!(
            records
                .iter()
                .any(|r| matches!(r, GraphRecord::Node { id, .. } if id == row.record_id)),
            "file handle {} must resolve to an existing node",
            row.record_id
        );
    }

    // History-backed resolution reads Git objects only.
    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert_eq!(
        status_after, status_before,
        "query must leave the working tree byte-for-byte unchanged"
    );

    // Temporal selector honored: as-of c3 the hot file is a three-way 1/1/1.
    let mut at_opts = options();
    at_opts.at_commit = Some(commits[2].as_str());
    let at_map = ownership_map(&records, &at_opts).expect("at view should resolve");
    let hot_at = at_map
        .files
        .iter()
        .find(|f| f.repo_relative_path == "src/hot.rs")
        .expect("src/hot.rs row at c3");
    assert_eq!(hot_at.total_commits, 3);
    assert_eq!(hot_at.bus_factor, 2);
}

#[test]
fn query_ownership_cli_is_deterministic_and_bounded() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_ownership_fixture_repo(&repo);
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
            .args(["query", "ownership"])
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
    assert_eq!(body["threshold_percent"], 50);
    assert_eq!(body["truncated"], false);
    assert!(body["files"].is_array());
    assert_eq!(body["files"].as_array().unwrap().len(), 2);
    assert!(
        body["disclaimer"]
            .as_str()
            .unwrap()
            .contains("not declared ownership"),
        "the response must label rows as empirical history leads"
    );
    // Rows never carry raw blob contents or source text.
    assert!(
        !outputs[0].contains("-> u32"),
        "source text must never leak into the response"
    );

    // Bus-factor risk ordering: the more concentrated file sorts first.
    let files = body["files"].as_array().unwrap();
    assert_eq!(files[0]["repo_relative_path"], "src/hot.rs");
    assert_eq!(files[0]["bus_factor"], 1);
    assert_eq!(files[1]["repo_relative_path"], "src/shared.rs");
    assert_eq!(files[1]["bus_factor"], 2);

    // Text format is available and deterministic too.
    let mut text_outputs = Vec::new();
    for _ in 0..2 {
        let assert = CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "ownership", "--format", "text"])
            .arg("--graph")
            .arg(&graph_path)
            .assert()
            .success();
        text_outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    assert_eq!(text_outputs[0], text_outputs[1]);
    assert!(text_outputs[0].contains("bus_factor=1"));
    assert!(text_outputs[0].contains("src/hot.rs"));

    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert_eq!(status_after, "", "CLI query must not mutate the tree");
}

#[test]
fn query_ownership_cli_exit_codes_for_diagnostics() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_ownership_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Unknown path: exit 2 with a stable diagnostic.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "ownership", "src/nope.rs"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["error_type"], "unknown_path");

    // A tracked but non-indexed path is outside the store's coverage: exit 2.
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "ownership", "README.md"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);

    // Missing commit: exit 2.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "ownership", "--at", "ffffffffffff"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "missing_commit");

    // Malformed timestamp: exit 1.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "ownership", "--as-of", "yesterday"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "malformed_timestamp");

    // Invalid threshold: exit 1.
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "ownership", "--threshold", "0"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);

    // Invalid limit: exit 1.
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "ownership", "--limit", "0"])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);

    // Empty history (plain scan graph carries no Commit records): exit 2.
    let scan_graph = temp.path().join("plain.graph.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_graph)
        .assert()
        .success();
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "ownership"])
        .arg("--graph")
        .arg(&scan_graph)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "empty_history");
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

fn commit_fixture(repo: &Path, message: &str, date: &str, author: &FixtureAuthor) -> String {
    git(repo, ["add", "."]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_NAME", author.name)
        .env("GIT_AUTHOR_EMAIL", author.email)
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
