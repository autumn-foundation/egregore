//! Repository-scoped query workflows (issue #67).
//!
//! A shared local store may contain multiple repositories with colliding
//! repo-relative paths and symbol names. These tests pin the contract that
//! public code-oriented queries either scope to exactly one repository or keep
//! the repository boundary visible on every returned row — never silently
//! picking one repository.
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, GraphRecord, IdentitySource, MetricKind, NodeKind,
    RepositoryIdentityPayload, SelectionBasis, SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    ir::{Graph, stable_id},
};
use assert_cmd::Command;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: start_line * 10,
        end_byte: end_line * 10,
        start_line,
        end_line,
    }
}

fn eg() -> Command {
    Command::cargo_bin("egregore").expect("binary should build")
}

/// Parses one-JSON-object-per-line stdout into values.
fn parse_jsonl(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("stdout line should be JSON"))
        .collect()
}

/// Parses the first JSON object found on stderr (machine-readable diagnostics).
fn parse_stderr_json(stderr: &[u8]) -> serde_json::Value {
    String::from_utf8_lossy(stderr)
        .lines()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .expect("stderr should contain a machine-readable JSON diagnostic")
}

/// Seeds a store by really scanning two local repositories that both contain
/// `src/lib.rs` defining the same-named Rust symbol `widget`, then merging the
/// two scans into one multi-repo store file. No network, no git required:
/// operator overrides force distinct, deterministic repository identities.
///
/// Returns `(tempdir, store_path, repo_a_id, repo_b_id)`.
fn scanned_collision_store() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");

    for (dir, body) in [
        ("widget-a", "pub fn widget() -> u32 { 1 }\n"),
        ("widget-b", "pub fn widget() -> u32 { 2 }\n"),
    ] {
        let src = temp.path().join(dir).join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("lib.rs"), body).expect("write lib.rs");
    }

    let out_a = temp.path().join("a.jsonl");
    let out_b = temp.path().join("b.jsonl");
    for (dir, out, override_id) in [
        ("widget-a", &out_a, "widget-a"),
        ("widget-b", &out_b, "widget-b"),
    ] {
        eg().arg("scan")
            .arg(temp.path().join(dir))
            .arg("--out")
            .arg(out)
            .args(["--repo-id-override", override_id])
            .assert()
            .success();
    }

    let merged = format!(
        "{}{}",
        fs::read_to_string(&out_a).expect("read scan a"),
        fs::read_to_string(&out_b).expect("read scan b"),
    );
    let store = temp.path().join("store.jsonl");
    fs::write(&store, merged).expect("write merged store");

    let repo_id = |jsonl: &str, basename: &str| -> String {
        jsonl
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["kind"] == "Repository" && v["repository_identity"]["basename"] == basename)
            .and_then(|v| v["id"].as_str().map(str::to_owned))
            .expect("scan output should contain the Repository record")
    };
    let store_text = fs::read_to_string(&store).expect("read store");
    let repo_a = repo_id(&store_text, "widget-a");
    let repo_b = repo_id(&store_text, "widget-b");
    assert_ne!(
        repo_a, repo_b,
        "fixture repos must have distinct identities"
    );

    (temp, store, repo_a, repo_b)
}

/// Builds a synthetic two-repository store with remote-derived identities,
/// history-backed colliding symbols, and one semantic drift record per repo.
///
/// Repo `acme/widget-a`: symbol `widget` at commits `aaaa0001…` (valid
/// 2026-01-01) and `aaaa000200000000` (valid 2026-01-03), plus a drift node
/// (score 0.5) targeting the later version.
/// Repo `acme/widget-b`: symbol `widget` at commit `aaaa000299990000` (valid
/// 2026-01-01), plus a drift node (score 0.4). The commit deliberately shares
/// the `aaaa0002` prefix with repo A's second commit so commit-prefix
/// ambiguity across the repository boundary can be exercised.
///
/// Returns `(tempdir, store_path, repo_a_id, repo_b_id)`.
#[allow(clippy::too_many_lines, clippy::type_complexity)]
fn synthetic_collision_store() -> (tempfile::TempDir, PathBuf, String, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = temp.path().join("history.jsonl");

    let mut graph = Graph::new();
    let mut repo_ids = Vec::new();

    let repos: [(&str, &str, &str, &[(&str, &str)], f64); 2] = [
        (
            "acme/widget-a",
            "widget-a",
            "https://example.com/acme/widget-a",
            &[
                ("aaaa000100000000", "2026-01-01T00:00:00Z"),
                ("aaaa000200000000", "2026-01-03T00:00:00Z"),
            ],
            0.5,
        ),
        (
            "acme/widget-b",
            "widget-b",
            "https://example.com/acme/widget-b",
            &[("aaaa000299990000", "2026-01-01T00:00:00Z")],
            0.4,
        ),
    ];

    for (display, basename, remote, commits, drift_score) in repos {
        let repo_id = stable_id(&["repository", "remote", remote]);
        repo_ids.push(repo_id.clone());
        graph.push(
            GraphRecord::node(
                repo_id.clone(),
                NodeKind::Repository,
                None,
                None,
                Some(display.to_owned()),
                format!("Repository {display}"),
            )
            .with_repository_identity(RepositoryIdentityPayload {
                identity_source: IdentitySource::Remote,
                remote_url: Some(remote.to_owned()),
                root_commit_sha: None,
                canonical_path: None,
                basename: basename.to_owned(),
            }),
        );

        let file_id = stable_id(&["node", "file", &repo_id, "src/lib.rs"]);
        graph.push(GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            format!("Rust source file src/lib.rs in {display}"),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            file_id.clone(),
            Some("1.0".to_owned()),
            "Repository contains source file".to_owned(),
        ));

        let mut last_symbol_id = String::new();
        for (commit, valid_time) in commits {
            let symbol_id = stable_id(&[
                "node",
                "symbol",
                "function",
                &repo_id,
                "src/lib.rs",
                "widget",
                commit,
            ]);
            graph.push(
                GraphRecord::symbol(
                    symbol_id.clone(),
                    "function",
                    "src/lib.rs".to_owned(),
                    span(10, 20),
                    "widget".to_owned(),
                    format!("Rust function widget in {display} at {commit}"),
                )
                .with_temporal(TemporalMetadata {
                    git_commit: (*commit).to_owned(),
                    git_parent_commits: vec![],
                    valid_time: (*valid_time).to_owned(),
                    author_time: None,
                    observed_at: (*valid_time).to_owned(),
                    valid_time_source: None,
                }),
            );
            graph.push(GraphRecord::edge(
                EdgeLabel::Defines,
                file_id.clone(),
                symbol_id.clone(),
                Some("1.0".to_owned()),
                "file defines symbol".to_owned(),
            ));
            last_symbol_id = symbol_id;
        }

        let drift_id = stable_id(&["node", "semantic-drift", &repo_id, "widget"]);
        graph.push(
            GraphRecord::node(
                drift_id.clone(),
                NodeKind::SemanticDrift,
                Some("src/lib.rs".to_owned()),
                None,
                Some("widget".to_owned()),
                format!("semantic drift for widget in {display}"),
            )
            .with_semantic_drift(SemanticDriftMetadata {
                embedding_model: EmbeddingModel {
                    provider: "test".to_owned(),
                    name: "test-model-v1".to_owned(),
                    version: "v1".to_owned(),
                    dim: 384,
                    content_hash: "fixture".to_owned(),
                },
                target_record_id: last_symbol_id.clone(),
                prior_record_id: last_symbol_id.clone(),
                before_git_commit: "aaaaaaaa".to_owned(),
                after_git_commit: "bbbbbbbb".to_owned(),
                before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
                after_valid_time: "2026-01-03T00:00:00Z".to_owned(),
                metric_kind: MetricKind::CosineDistance,
                score: drift_score,
                selection_threshold: 0.2,
                selection_basis: SelectionBasis::ThresholdOnly,
            }),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::DriftsFrom,
            drift_id,
            last_symbol_id,
            Some("1.0".to_owned()),
            "drift targets symbol".to_owned(),
        ));
    }

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&store, jsonl).expect("write fixture");
    let repo_b = repo_ids.pop().expect("repo b id");
    let repo_a = repo_ids.pop().expect("repo a id");
    (temp, store, repo_a, repo_b)
}

// ---------------------------------------------------------------------------
// AC: scoped symbol lookup returns only the selected repository, with a fully
// citable row shape (record ID, schema version, repository identity handle,
// path, span).
// ---------------------------------------------------------------------------

#[test]
fn scoped_symbol_query_returns_only_selected_repo_rows() {
    let (_temp, store, repo_a, repo_b) = scanned_collision_store();

    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--repo", "widget-a"])
        .assert()
        .success();

    let rows = parse_jsonl(&assert.get_output().stdout);
    assert!(!rows.is_empty(), "scoped query should match repo A symbol");
    for row in &rows {
        assert_eq!(row["repository_id"].as_str(), Some(repo_a.as_str()));
        assert_eq!(row["repository"].as_str(), Some("widget-a"));
        assert!(row["record_id"].is_string(), "row must carry record_id");
        assert!(
            row["schema_version"].is_number(),
            "row must carry schema_version"
        );
        assert_eq!(row["repo_relative_path"].as_str(), Some("src/lib.rs"));
        assert!(
            row["span"].is_object(),
            "row must carry span when available"
        );
        assert_ne!(
            row["repository_id"].as_str(),
            Some(repo_b.as_str()),
            "no repo B rows may leak into a repo A scoped result"
        );
    }
}

#[test]
fn scoped_symbol_query_accepts_stable_repository_record_id() {
    let (_temp, store, repo_a, _repo_b) = scanned_collision_store();

    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--repo", &repo_a])
        .assert()
        .success();

    let rows = parse_jsonl(&assert.get_output().stdout);
    assert!(!rows.is_empty());
    for row in &rows {
        assert_eq!(row["repository_id"].as_str(), Some(repo_a.as_str()));
    }
}

// ---------------------------------------------------------------------------
// AC: unscoped collision queries return every matching repository with
// repository identity on every row — never an implicit single-repo pick.
// ---------------------------------------------------------------------------

#[test]
fn unscoped_symbol_collision_returns_all_repos_with_identity_on_every_row() {
    let (_temp, store, repo_a, repo_b) = scanned_collision_store();

    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .assert()
        .success();

    let rows = parse_jsonl(&assert.get_output().stdout);
    let repos: std::collections::BTreeSet<&str> = rows
        .iter()
        .map(|r| {
            r["repository_id"]
                .as_str()
                .expect("every row must carry repository_id in a multi-repo store")
        })
        .collect();
    assert!(repos.contains(repo_a.as_str()), "repo A row missing");
    assert!(repos.contains(repo_b.as_str()), "repo B row missing");
    for row in &rows {
        assert!(
            row["repository"].is_string(),
            "every row must carry the human repository handle"
        );
    }
}

// ---------------------------------------------------------------------------
// AC: scoped file lookup excludes the colliding path in the other repository
// and reports it only through diagnostics/counts.
// ---------------------------------------------------------------------------

#[test]
fn scoped_file_query_excludes_other_repo_and_reports_exclusion_diagnostic() {
    let (_temp, store, repo_a, repo_b) = scanned_collision_store();

    let assert = eg()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&store)
        .args(["--repo", "widget-a"])
        .assert()
        .success();

    let output = assert.get_output();
    let rows = parse_jsonl(&output.stdout);
    assert!(!rows.is_empty(), "repo A defines widget in src/lib.rs");
    for row in &rows {
        assert_eq!(row["repository_id"].as_str(), Some(repo_a.as_str()));
        assert_ne!(row["repository_id"].as_str(), Some(repo_b.as_str()));
    }

    let diag = parse_stderr_json(&output.stderr);
    assert_eq!(
        diag["code"].as_str(),
        Some("excluded_other_repositories"),
        "exclusion must be reported as a stable diagnostic: {diag}"
    );
    assert_eq!(diag["excluded_repository_count"].as_u64(), Some(1));
    assert!(diag["excluded_row_count"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn unscoped_file_collision_returns_identity_on_every_row() {
    let (_temp, store, repo_a, repo_b) = scanned_collision_store();

    let assert = eg()
        .args(["query", "file", "src/lib.rs", "--graph"])
        .arg(&store)
        .assert()
        .success();

    let rows = parse_jsonl(&assert.get_output().stdout);
    let repos: std::collections::BTreeSet<&str> = rows
        .iter()
        .map(|r| {
            r["repository_id"]
                .as_str()
                .expect("every row must carry repository_id in a multi-repo store")
        })
        .collect();
    assert!(repos.contains(repo_a.as_str()));
    assert!(repos.contains(repo_b.as_str()));
}

// ---------------------------------------------------------------------------
// AC: repository selection supports the stable record ID plus a human-usable
// selector; unknown/ambiguous selectors fail with stable machine-readable
// diagnostics.
// ---------------------------------------------------------------------------

#[test]
fn unknown_repo_selector_fails_with_stable_diagnostic() {
    let (_temp, store, _repo_a, _repo_b) = scanned_collision_store();

    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--repo", "no-such-repo"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());

    let diag = parse_stderr_json(&assert.get_output().stderr);
    assert_eq!(diag["code"].as_str(), Some("unknown_repository_selector"));
    assert_eq!(diag["selector"].as_str(), Some("no-such-repo"));
}

#[test]
fn ambiguous_repo_selector_fails_with_stable_diagnostic_listing_candidates() {
    // Two distinct repositories whose identity payloads share the basename
    // `widget`: the bare basename selector cannot pick one.
    let temp = tempfile::tempdir().expect("temp dir");
    let store = temp.path().join("ambiguous.jsonl");

    let mut graph = Graph::new();
    let mut ids = Vec::new();
    for owner in ["acme", "globex"] {
        let remote = format!("https://example.com/{owner}/widget");
        let repo_id = stable_id(&["repository", "remote", &remote]);
        ids.push(repo_id.clone());
        graph.push(
            GraphRecord::node(
                repo_id,
                NodeKind::Repository,
                None,
                None,
                Some(format!("{owner}/widget")),
                format!("Repository {owner}/widget"),
            )
            .with_repository_identity(RepositoryIdentityPayload {
                identity_source: IdentitySource::Remote,
                remote_url: Some(remote),
                root_commit_sha: None,
                canonical_path: None,
                basename: "widget".to_owned(),
            }),
        );
    }
    fs::write(&store, graph.to_jsonl().expect("serialize")).expect("write fixture");
    ids.sort();

    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--repo", "widget"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());

    let diag = parse_stderr_json(&assert.get_output().stderr);
    assert_eq!(diag["code"].as_str(), Some("ambiguous_repository_selector"));
    assert_eq!(diag["selector"].as_str(), Some("widget"));
    let candidates: Vec<&str> = diag["candidates"]
        .as_array()
        .expect("candidates listed")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        candidates,
        ids.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// AC: temporal selectors compose with repository scope; single-result temporal
// paths never pick a repository implicitly on a collision.
// ---------------------------------------------------------------------------

#[test]
fn as_of_composes_with_repo_scope() {
    let (_temp, store, repo_a, _repo_b) = synthetic_collision_store();

    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--as-of", "2026-01-02T00:00:00Z"])
        .args(["--repo", "acme/widget-a"])
        .assert()
        .success();

    let rows = parse_jsonl(&assert.get_output().stdout);
    assert_eq!(rows.len(), 1, "one repo, one time view, one row");
    assert_eq!(rows[0]["repository_id"].as_str(), Some(repo_a.as_str()));
    assert_eq!(rows[0]["repository"].as_str(), Some("acme/widget-a"));
    assert_eq!(
        rows[0]["git_commit"].as_str(),
        Some("aaaa000100000000"),
        "the 2026-01-01 version is the one valid at the requested instant"
    );
}

#[test]
fn at_commit_prefix_ambiguity_is_scoped_to_selected_repo() {
    let (_temp, store, repo_a, _repo_b) = synthetic_collision_store();

    // `aaaa0002` matches repo A's `aaaa000200000000` and repo B's
    // `aaaa000299990000`: unscoped, the prefix is genuinely ambiguous.
    eg().args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--at", "aaaa0002"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("ambiguous commit prefix"));

    // Scoped to repo A the prefix matches exactly one commit: the scoped
    // query must succeed instead of failing on the other repository's commit.
    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--at", "aaaa0002"])
        .args(["--repo", "acme/widget-a"])
        .assert()
        .success();
    let rows = parse_jsonl(&assert.get_output().stdout);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["git_commit"].as_str(), Some("aaaa000200000000"));
    assert_eq!(rows[0]["repository_id"].as_str(), Some(repo_a.as_str()));
}

#[test]
fn unscoped_as_of_collision_fails_with_ambiguous_repository_diagnostic() {
    let (_temp, store, repo_a, repo_b) = synthetic_collision_store();

    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--as-of", "2026-01-02T00:00:00Z"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());

    let diag = parse_stderr_json(&assert.get_output().stderr);
    assert_eq!(diag["code"].as_str(), Some("ambiguous_repository"));
    let mut expected = vec![repo_a.as_str(), repo_b.as_str()];
    expected.sort_unstable();
    let repos: Vec<&str> = diag["repositories"]
        .as_array()
        .expect("repositories listed")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(repos, expected, "ambiguity must list every candidate repo");
}

#[test]
fn tx_as_of_rows_carry_repository_identity_and_compose_with_repo_scope() {
    let (_temp, store, repo_a, repo_b) = synthetic_collision_store();

    // Unscoped: complete repository-disambiguated rows.
    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--tx-as-of", "2026-02-01T00:00:00Z"])
        .assert()
        .success();
    let envelope = parse_jsonl(&assert.get_output().stdout)
        .pop()
        .expect("tx envelope");
    let rows = envelope["records"].as_array().expect("records array");
    let repos: std::collections::BTreeSet<&str> = rows
        .iter()
        .map(|r| {
            r["repository_id"]
                .as_str()
                .expect("every tx row must carry repository_id")
        })
        .collect();
    assert!(repos.contains(repo_a.as_str()));
    assert!(repos.contains(repo_b.as_str()));

    // Scoped: only the selected repository's rows.
    let assert = eg()
        .args(["query", "symbol", "widget", "--graph"])
        .arg(&store)
        .args(["--tx-as-of", "2026-02-01T00:00:00Z"])
        .args(["--repo", "acme/widget-b"])
        .assert()
        .success();
    let envelope = parse_jsonl(&assert.get_output().stdout)
        .pop()
        .expect("tx envelope");
    let rows = envelope["records"].as_array().expect("records array");
    assert!(!rows.is_empty());
    for row in rows {
        assert_eq!(row["repository_id"].as_str(), Some(repo_b.as_str()));
    }
}

// ---------------------------------------------------------------------------
// AC: drift workflows carry repository identity and scope without merging
// records across the repository boundary.
// ---------------------------------------------------------------------------

#[test]
fn drift_rows_carry_repository_identity_and_scope_filters_them() {
    let (_temp, store, repo_a, repo_b) = synthetic_collision_store();

    let assert = eg()
        .args(["query", "drift", "--graph"])
        .arg(&store)
        .assert()
        .success();
    let rows = parse_jsonl(&assert.get_output().stdout);
    assert_eq!(rows.len(), 2, "one drift row per repository");
    let repos: std::collections::BTreeSet<&str> = rows
        .iter()
        .map(|r| {
            r["repository_id"]
                .as_str()
                .expect("every drift row must carry repository_id")
        })
        .collect();
    assert!(repos.contains(repo_a.as_str()));
    assert!(repos.contains(repo_b.as_str()));

    let assert = eg()
        .args(["query", "drift", "--graph"])
        .arg(&store)
        .args(["--repo", "acme/widget-b"])
        .assert()
        .success();
    let rows = parse_jsonl(&assert.get_output().stdout);
    assert_eq!(rows.len(), 1, "scoped drift returns only repo B's record");
    assert_eq!(rows[0]["repository_id"].as_str(), Some(repo_b.as_str()));
}

// ---------------------------------------------------------------------------
// AC: deterministic ordering, diagnostics, and pass/fail behavior across 5
// repeated runs of the same scoped and unscoped collision queries.
// ---------------------------------------------------------------------------

#[test]
fn repeated_collision_queries_are_deterministic_across_5_runs() {
    let (_temp, store, _repo_a, _repo_b) = scanned_collision_store();

    let run = |args: &[&str]| -> (Vec<u8>, Vec<u8>) {
        let assert = eg()
            .args(["query"])
            .args(args)
            .arg("--graph")
            .arg(&store)
            .assert()
            .success();
        let out = assert.get_output();
        (out.stdout.clone(), out.stderr.clone())
    };

    let scoped_first = run(&["symbol", "widget", "--repo", "widget-a"]);
    let unscoped_first = run(&["symbol", "widget"]);
    let file_first = run(&["file", "src/lib.rs", "--repo", "widget-a"]);
    for _ in 0..4 {
        assert_eq!(
            run(&["symbol", "widget", "--repo", "widget-a"]),
            scoped_first,
            "scoped symbol output must be byte-identical across runs"
        );
        assert_eq!(
            run(&["symbol", "widget"]),
            unscoped_first,
            "unscoped symbol output must be byte-identical across runs"
        );
        assert_eq!(
            run(&["file", "src/lib.rs", "--repo", "widget-a"]),
            file_first,
            "scoped file output (rows and diagnostics) must be byte-identical"
        );
    }
}

// ---------------------------------------------------------------------------
// AC: output carries only bounded handles — never raw record bodies, source
// text payloads, or summaries.
// ---------------------------------------------------------------------------

#[test]
fn scoped_query_output_never_includes_raw_payloads() {
    let (_temp, store, _repo_a, _repo_b) = scanned_collision_store();

    for args in [
        vec!["symbol", "widget", "--repo", "widget-a"],
        vec!["file", "src/lib.rs", "--repo", "widget-a"],
        vec!["symbol", "widget"],
    ] {
        let assert = eg()
            .args(["query"])
            .args(&args)
            .arg("--graph")
            .arg(&store)
            .assert()
            .success();
        let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
        assert!(
            !stdout.contains("Source:"),
            "raw source payload leaked into query output for {args:?}"
        );
        assert!(
            !stdout.contains("\"summary\""),
            "record summary leaked into query output for {args:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC: the daemon query verbs honor the same repository scope contract:
// `params.repo` filters rows, every row carries repository identity, and
// selector failures map to the stable error codes.
// ---------------------------------------------------------------------------

/// Ingests a graph JSONL into an embedded store and spawns a daemon for it,
/// waiting until the runtime metadata reports a running daemon.
#[cfg(feature = "embedded-aletheiadb")]
fn ingest_and_start_daemon(
    store: &std::path::Path,
    data_dir: &std::path::Path,
) -> std::process::Child {
    use std::process::{Command as ProcessCommand, Stdio};

    eg().arg("ingest")
        .arg(store)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success();

    let daemon = ProcessCommand::new(assert_cmd::cargo::cargo_bin("egregore"))
        .arg("daemon")
        .arg("run")
        .arg("--data-dir")
        .arg(data_dir)
        .args(["--port", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon should spawn");

    let mut runtime_name = data_dir
        .file_name()
        .expect("data dir has a name")
        .to_os_string();
    runtime_name.push(".egregore-runtime");
    let runtime_metadata = data_dir.with_file_name(runtime_name).join("egregored.json");
    let started = std::time::Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(&runtime_metadata)
            && contents.contains("\"running\"")
        {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "daemon should start and write running metadata"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    daemon
}

#[cfg(feature = "embedded-aletheiadb")]
fn stop_daemon(data_dir: &std::path::Path, daemon: &mut std::process::Child) {
    eg().args(["daemon", "stop", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success();
    let _ = daemon.wait();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn daemon_query_verbs_scope_by_repository_and_reject_unknown_selectors() {
    let (temp, store, repo_a, repo_b) = scanned_collision_store();
    let data_dir = temp.path().join("egregore-store");
    let mut daemon = ingest_and_start_daemon(&store, &data_dir);

    let result = std::panic::catch_unwind(|| {
        // Scoped daemon symbol query: only repo A rows, each with identity.
        let assert = eg()
            .args(["query", "symbol", "widget", "--daemon", "--data-dir"])
            .arg(&data_dir)
            .args(["--repo", "widget-a"])
            .assert()
            .success();
        let rows = parse_jsonl(&assert.get_output().stdout);
        assert!(!rows.is_empty(), "daemon scoped query should match repo A");
        for row in &rows {
            assert_eq!(row["repository_id"].as_str(), Some(repo_a.as_str()));
            assert_eq!(row["repository"].as_str(), Some("widget-a"));
        }

        // Unscoped daemon collision query: rows from both repos, identity on every row.
        let assert = eg()
            .args(["query", "symbol", "widget", "--daemon", "--data-dir"])
            .arg(&data_dir)
            .assert()
            .success();
        let rows = parse_jsonl(&assert.get_output().stdout);
        let repos: std::collections::BTreeSet<&str> = rows
            .iter()
            .map(|r| {
                r["repository_id"]
                    .as_str()
                    .expect("every daemon row must carry repository_id")
            })
            .collect();
        assert!(repos.contains(repo_a.as_str()));
        assert!(repos.contains(repo_b.as_str()));

        // Scoped daemon file query: rows restricted to the selected repository.
        let assert = eg()
            .args(["query", "file", "src/lib.rs", "--daemon", "--data-dir"])
            .arg(&data_dir)
            .args(["--repo", &repo_b])
            .assert()
            .success();
        let rows = parse_jsonl(&assert.get_output().stdout);
        assert!(!rows.is_empty());
        for row in &rows {
            assert_eq!(row["repository_id"].as_str(), Some(repo_b.as_str()));
        }

        // Unknown selector: the daemon-routed CLI must keep the same
        // machine-readable stderr contract as the local paths.
        let assert = eg()
            .args(["query", "symbol", "widget", "--daemon", "--data-dir"])
            .arg(&data_dir)
            .args(["--repo", "no-such-repo"])
            .assert()
            .failure()
            .stdout(predicate::str::is_empty());
        let diag = parse_stderr_json(&assert.get_output().stderr);
        assert_eq!(diag["code"].as_str(), Some("unknown_repository_selector"));
        assert_eq!(diag["selector"].as_str(), Some("no-such-repo"));
    });

    stop_daemon(&data_dir, &mut daemon);

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn daemon_temporal_queries_compose_with_repo_scope_and_fail_closed_on_collisions() {
    let (temp, store, repo_a, repo_b) = synthetic_collision_store();
    let data_dir = temp.path().join("egregore-history-store");
    let mut daemon = ingest_and_start_daemon(&store, &data_dir);

    let result = std::panic::catch_unwind(|| {
        // Commit-prefix ambiguity is scoped to the selected repository: the
        // `aaaa0002` prefix collides across repos, but repo A has exactly one
        // matching commit, so the scoped daemon query must succeed.
        let assert = eg()
            .args(["query", "symbol", "widget", "--daemon", "--data-dir"])
            .arg(&data_dir)
            .args(["--at", "aaaa0002"])
            .args(["--repo", "acme/widget-a"])
            .assert()
            .success();
        let rows = parse_jsonl(&assert.get_output().stdout);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["git_commit"].as_str(), Some("aaaa000200000000"));
        assert_eq!(rows[0]["repository_id"].as_str(), Some(repo_a.as_str()));

        // Unscoped single-answer time view on a collision fails closed with
        // the same ambiguous_repository diagnostic as the non-daemon CLI.
        let assert = eg()
            .args(["query", "symbol", "widget", "--daemon", "--data-dir"])
            .arg(&data_dir)
            .args(["--as-of", "2026-01-02T00:00:00Z"])
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty());
        let diag = parse_stderr_json(&assert.get_output().stderr);
        assert_eq!(diag["code"].as_str(), Some("ambiguous_repository"));

        // Scoped, the same time view returns exactly the selected repo's row.
        let assert = eg()
            .args(["query", "symbol", "widget", "--daemon", "--data-dir"])
            .arg(&data_dir)
            .args(["--as-of", "2026-01-02T00:00:00Z"])
            .args(["--repo", "acme/widget-b"])
            .assert()
            .success();
        let rows = parse_jsonl(&assert.get_output().stdout);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["repository_id"].as_str(), Some(repo_b.as_str()));
    });

    stop_daemon(&data_dir, &mut daemon);

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

// ---------------------------------------------------------------------------
// AC: scoped lookup that matches nothing in the selected repository is a
// clean no-match, not a silent fallback to another repository.
// ---------------------------------------------------------------------------

#[test]
fn scoped_file_query_with_no_match_in_selected_repo_exits_no_match() {
    let temp = tempfile::tempdir().expect("temp dir");

    // repo A has src/lib.rs; repo B has src/other.rs only.
    let src_a = temp.path().join("widget-a").join("src");
    fs::create_dir_all(&src_a).expect("create src");
    fs::write(src_a.join("lib.rs"), "pub fn widget() -> u32 { 1 }\n").expect("write");
    let src_b = temp.path().join("widget-b").join("src");
    fs::create_dir_all(&src_b).expect("create src");
    fs::write(src_b.join("other.rs"), "pub fn widget() -> u32 { 2 }\n").expect("write");

    let out_a = temp.path().join("a.jsonl");
    let out_b = temp.path().join("b.jsonl");
    for (dir, out, override_id) in [
        ("widget-a", &out_a, "widget-a"),
        ("widget-b", &out_b, "widget-b"),
    ] {
        eg().arg("scan")
            .arg(temp.path().join(dir))
            .arg("--out")
            .arg(out)
            .args(["--repo-id-override", override_id])
            .assert()
            .success();
    }
    let store = temp.path().join("store.jsonl");
    fs::write(
        &store,
        format!(
            "{}{}",
            fs::read_to_string(&out_a).expect("read a"),
            fs::read_to_string(&out_b).expect("read b")
        ),
    )
    .expect("write store");

    // src/other.rs exists only in repo B; scoping to repo A must be a no-match
    // (exit 2), never a silent answer from repo B.
    eg().args(["query", "file", "src/other.rs", "--graph"])
        .arg(&store)
        .args(["--repo", "widget-a"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
}
