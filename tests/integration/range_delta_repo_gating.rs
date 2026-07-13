//! Repository gating of range-delta commit-endpoint resolution (issue #341).
//!
//! In an unscoped multi-repository store `eg query deltas` and
//! `eg query public-api-deltas` must refuse to resolve `--base`/`--head`
//! ambiguously across repositories — a mirrored/forked SHA present in two
//! repositories, or endpoints that split across repositories — instead of
//! silently collapsing same-SHA commits or splicing two repositories'
//! topologies. Single-repository stores and `--repo`-scoped calls stay
//! byte-identical.
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, IdentitySource, NodeKind, RepositoryIdentityPayload, SourceSpan,
    TemporalMetadata,
    ir::{Graph, stable_id},
};
use assert_cmd::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn eg() -> Command {
    Command::cargo_bin("egregore").expect("binary should build")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: start_line * 10,
        end_byte: end_line * 10,
        start_line,
        end_line,
    }
}

fn temporal(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|p| (*p).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

const REMOTE_A: &str = "https://example.test/widget-a.git";
const REMOTE_B: &str = "https://example.test/widget-b.git";

fn repo_a_id() -> String {
    stable_id(&["repository", "remote", REMOTE_A])
}

fn repo_b_id() -> String {
    stable_id(&["repository", "remote", REMOTE_B])
}

/// One repository's slice: a `Repository` identity node, a `src/lib.rs` `File`,
/// a linear `c1 -> c2 -> dupsha` commit chain (all `CONTAINS`-owned by the
/// repository so `RepositoryIndex` attributes them), and one symbol unique to
/// each commit (`<prefix>_<sha-tag>`, a single snapshot each). Between `c1` and
/// `c2` this yields exactly one `added_symbol` (`c2`'s symbol) and one
/// `removed_symbol` (`c1`'s), both provably owned by this repository. `dupsha`
/// is the shared/mirrored SHA passed identically for both repositories.
///
/// The single-snapshot-per-commit shape (matching `deps.rs`'s
/// `seed_multi_repo_history`) is referentially clean, so it ingests into the
/// embedded store without any dangling-edge or duplicate-id rejection.
fn repo_slice(
    graph: &mut Graph,
    remote: &str,
    display: &str,
    sym_prefix: &str,
    commits: &[(&str, &[&str], &str)],
) -> String {
    let repo_id = stable_id(&["repository", "remote", remote]);
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
            basename: display.to_owned(),
        }),
    );

    // Single File node, owned by the repository.
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

    for (sha, parents, valid_time) in commits {
        // Commit node, CONTAINS-owned by the repository.
        let commit_id = stable_id(&["node", "commit", &repo_id, sha]);
        graph.push(
            GraphRecord::node(
                commit_id.clone(),
                NodeKind::Commit,
                None,
                None,
                Some((*sha).to_owned()),
                format!("Commit {sha} in {display}"),
            )
            .with_temporal(temporal(sha, parents, valid_time)),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.clone(),
            commit_id.clone(),
            Some("1.0".to_owned()),
            "Repository contains commit".to_owned(),
        ));

        // One symbol unique to this commit (single snapshot).
        let sym_name = format!("{sym_prefix}_{sha}");
        let sym_id = stable_id(&["node", "symbol", &repo_id, "src/lib.rs", &sym_name]);
        graph.push(
            GraphRecord::symbol(
                sym_id.clone(),
                "function",
                "src/lib.rs".to_owned(),
                span(10, 20),
                sym_name.clone(),
                format!("Rust function {sym_name} in {display}"),
            )
            .with_temporal(temporal(sha, parents, valid_time)),
        );
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            sym_id,
            Some("1.0".to_owned()),
            "file defines symbol".to_owned(),
        ));
    }

    repo_id
}

/// Two-repository history: repo A owns `aa10000000 -> aa20000000 -> dupsha0000`
/// with symbols `alpha_*`; repo B owns `bb10000000 -> bb20000000 -> dupsha0000`
/// with symbols `beta_*`. `dupsha0000` is present in BOTH repositories (a
/// mirrored/forked SHA) with distinct repo-scoped record ids.
fn multi_repo_store() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = temp.path().join("history.jsonl");
    let mut graph = Graph::new();

    repo_slice(
        &mut graph,
        REMOTE_A,
        "widget-a",
        "alpha",
        &[
            ("aa10000000", &[], "2026-01-01T00:00:00Z"),
            ("aa20000000", &["aa10000000"], "2026-01-02T00:00:00Z"),
            ("dupsha0000", &["aa20000000"], "2026-01-03T00:00:00Z"),
        ],
    );
    repo_slice(
        &mut graph,
        REMOTE_B,
        "widget-b",
        "beta",
        &[
            ("bb10000000", &[], "2026-01-01T00:00:00Z"),
            ("bb20000000", &["bb10000000"], "2026-01-02T00:00:00Z"),
            ("dupsha0000", &["bb20000000"], "2026-01-03T00:00:00Z"),
        ],
    );

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&store, jsonl).expect("write fixture");
    (temp, store)
}

/// A one-repository store: repo A only, `aa10000000 -> aa20000000` with an
/// `alpha_added` symbol introduced at `aa20000000`.
fn single_repo_store() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = temp.path().join("history.jsonl");
    let mut graph = Graph::new();
    repo_slice(
        &mut graph,
        REMOTE_A,
        "widget-a",
        "alpha",
        &[
            ("aa10000000", &[], "2026-01-01T00:00:00Z"),
            ("aa20000000", &["aa10000000"], "2026-01-02T00:00:00Z"),
        ],
    );
    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&store, jsonl).expect("write fixture");
    (temp, store)
}

/// Parses the compact one-line error envelope `{"ok":false,"error":{...}}`.
fn parse_error(stdout: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(stdout);
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .expect("stdout should carry a JSON line");
    serde_json::from_str(line).expect("stdout line should be JSON")
}

/// Parses the pretty success envelope `{"ok":true,...}`.
fn parse_success(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).expect("stdout should be a JSON object")
}

/// Every record id string appearing anywhere in a JSON value.
fn collect_record_ids(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if k == "record_id"
                    && let Some(s) = v.as_str()
                {
                    out.push(s.to_owned());
                }
                collect_record_ids(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_record_ids(item, out);
            }
        }
        _ => {}
    }
}

fn assert_repo_scope_required(stdout: &[u8]) {
    let env = parse_error(stdout);
    assert_eq!(env["ok"], serde_json::json!(false));
    assert_eq!(
        env["error"]["error_type"],
        serde_json::json!("repo_scope_required")
    );
    let candidates = env["error"]["candidate_repositories"]
        .as_array()
        .expect("candidate_repositories must be an array");
    let ids: Vec<&str> = candidates
        .iter()
        .filter_map(|c| c["repository_id"].as_str())
        .collect();
    assert!(
        ids.contains(&repo_a_id().as_str()),
        "candidates must name repo A: {ids:?}"
    );
    assert!(
        ids.contains(&repo_b_id().as_str()),
        "candidates must name repo B: {ids:?}"
    );
    // Exactly one JSON line, no delta rows spilled onto stdout.
    let text = String::from_utf8_lossy(stdout);
    assert_eq!(
        text.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "refusal stdout must be a single JSON line"
    );
}

// ---------------------------------------------------------------------------
// `eg query deltas` — --graph
// ---------------------------------------------------------------------------

#[test]
fn deltas_unscoped_ambiguous_endpoint_refuses() {
    let (_temp, store) = multi_repo_store();
    let out = eg()
        .args(["query", "deltas", "aa10000000", "dupsha0000", "--graph"])
        .arg(&store)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    assert_repo_scope_required(&out);
}

#[test]
fn deltas_unscoped_split_endpoints_refuses() {
    let (_temp, store) = multi_repo_store();
    let out = eg()
        .args(["query", "deltas", "aa10000000", "bb20000000", "--graph"])
        .arg(&store)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    assert_repo_scope_required(&out);
}

#[test]
fn deltas_unscoped_single_common_owner_succeeds_scoped() {
    let (_temp, store) = multi_repo_store();
    let out = eg()
        .args(["query", "deltas", "aa10000000", "aa20000000", "--graph"])
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env = parse_success(&out);
    assert_eq!(env["ok"], serde_json::json!(true));

    let mut ids = Vec::new();
    collect_record_ids(&env, &mut ids);
    assert!(
        !ids.is_empty(),
        "single-common-owner range should yield rows"
    );
    let repo_b = repo_b_id();
    for id in &ids {
        assert!(
            !id.contains(&repo_b),
            "no repo-B record id may leak into a repo-A range: {id}"
        );
    }
    // The added symbol is repo A's second-commit symbol (`alpha_*`).
    let added = env["added_symbols"]
        .as_array()
        .expect("added_symbols array");
    assert!(
        added
            .iter()
            .any(|r| r["name"].as_str().is_some_and(|n| n.starts_with("alpha_"))),
        "repo A's added symbol must be reported: {added:?}"
    );
    assert!(
        !added
            .iter()
            .any(|r| r["name"].as_str().is_some_and(|n| n.starts_with("beta_"))),
        "no repo-B (`beta_*`) symbol may appear"
    );
}

#[test]
fn deltas_scoped_returns_only_selected_repo_rows() {
    let (_temp, store) = multi_repo_store();

    for (base, head, want, avoid, repo) in [
        ("aa10000000", "aa20000000", "alpha_", "beta_", "widget-a"),
        ("bb10000000", "bb20000000", "beta_", "alpha_", "widget-b"),
    ] {
        let out = eg()
            .args(["query", "deltas", base, head, "--graph"])
            .arg(&store)
            .args(["--repo", repo])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let env = parse_success(&out);
        assert_eq!(env["ok"], serde_json::json!(true));
        let added = env["added_symbols"]
            .as_array()
            .expect("added_symbols array");
        assert!(
            added
                .iter()
                .any(|r| r["name"].as_str().is_some_and(|n| n.starts_with(want))),
            "scoped {repo} range must report a `{want}` symbol: {added:?}"
        );
        assert!(
            !added
                .iter()
                .any(|r| r["name"].as_str().is_some_and(|n| n.starts_with(avoid))),
            "scoped {repo} range must never report a `{avoid}` symbol"
        );
    }
}

// ---------------------------------------------------------------------------
// `eg query public-api-deltas` — --graph
// ---------------------------------------------------------------------------

#[test]
fn public_api_deltas_unscoped_ambiguous_endpoint_refuses() {
    let (_temp, store) = multi_repo_store();
    let out = eg()
        .args([
            "query",
            "public-api-deltas",
            "aa10000000",
            "dupsha0000",
            "--graph",
        ])
        .arg(&store)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    assert_repo_scope_required(&out);
}

#[test]
fn public_api_deltas_unscoped_single_common_owner_succeeds() {
    let (_temp, store) = multi_repo_store();
    let out = eg()
        .args([
            "query",
            "public-api-deltas",
            "aa10000000",
            "aa20000000",
            "--graph",
        ])
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env = parse_success(&out);
    assert_eq!(env["ok"], serde_json::json!(true));
    let mut ids = Vec::new();
    collect_record_ids(&env, &mut ids);
    let repo_b = repo_b_id();
    for id in &ids {
        assert!(
            !id.contains(&repo_b),
            "no repo-B record id may leak into a repo-A public-api range: {id}"
        );
    }
}

#[test]
fn public_api_deltas_scoped_succeeds() {
    let (_temp, store) = multi_repo_store();
    let out = eg()
        .args([
            "query",
            "public-api-deltas",
            "aa10000000",
            "aa20000000",
            "--graph",
        ])
        .arg(&store)
        .args(["--repo", "widget-a"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env = parse_success(&out);
    assert_eq!(env["ok"], serde_json::json!(true));
}

// ---------------------------------------------------------------------------
// Determinism + single-repo regression
// ---------------------------------------------------------------------------

#[test]
fn deltas_refusal_is_byte_identical_across_runs() {
    let (_temp, store) = multi_repo_store();
    let run = || {
        eg().args(["query", "deltas", "aa10000000", "dupsha0000", "--graph"])
            .arg(&store)
            .assert()
            .code(1)
            .get_output()
            .stdout
            .clone()
    };
    let first = run();
    for _ in 0..4 {
        assert_eq!(run(), first, "refusal output must be byte-identical");
    }
}

#[test]
fn single_repo_store_unscoped_is_unchanged() {
    let (_temp, store) = single_repo_store();
    // Both lanes succeed unscoped on a single-repository store.
    let deltas = eg()
        .args(["query", "deltas", "aa10000000", "aa20000000", "--graph"])
        .arg(&store)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env = parse_success(&deltas);
    assert_eq!(env["ok"], serde_json::json!(true));
    let added = env["added_symbols"]
        .as_array()
        .expect("added_symbols array");
    assert!(
        added
            .iter()
            .any(|r| r["name"].as_str().is_some_and(|n| n.starts_with("alpha_"))),
        "single-repo deltas still reports the added symbol"
    );

    eg().args([
        "query",
        "public-api-deltas",
        "aa10000000",
        "aa20000000",
        "--graph",
    ])
    .arg(&store)
    .assert()
    .success();

    // A missing commit stays exit 2 (no-match class), unaffected by the gate.
    eg().args(["query", "deltas", "ffffffffff", "aa20000000", "--graph"])
        .arg(&store)
        .assert()
        .code(2);
}

// ---------------------------------------------------------------------------
// Embedded store (--data-dir)
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_embedded(store_jsonl: &std::path::Path, data_dir: &std::path::Path) {
    eg().arg("ingest")
        .arg(store_jsonl)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn deltas_repo_gating_over_embedded_store() {
    let (_temp, store) = multi_repo_store();
    let temp_db = tempfile::tempdir().expect("temp db");
    let data_dir = temp_db.path().join("store");
    ingest_embedded(&store, &data_dir);

    // Ambiguous unscoped → refuse.
    let out = eg()
        .args(["query", "deltas", "aa10000000", "dupsha0000", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    assert_repo_scope_required(&out);

    // Single-common-owner unscoped → repo A only.
    let out = eg()
        .args(["query", "deltas", "aa10000000", "aa20000000", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env = parse_success(&out);
    assert_eq!(env["ok"], serde_json::json!(true));
    let mut ids = Vec::new();
    collect_record_ids(&env, &mut ids);
    let repo_b = repo_b_id();
    for id in &ids {
        assert!(
            !id.contains(&repo_b),
            "no repo-B leak over embedded store: {id}"
        );
    }

    // Scoped → succeeds.
    eg().args(["query", "deltas", "bb10000000", "bb20000000", "--data-dir"])
        .arg(&data_dir)
        .args(["--repo", "widget-b"])
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn public_api_deltas_repo_gating_over_embedded_store() {
    let (_temp, store) = multi_repo_store();
    let temp_db = tempfile::tempdir().expect("temp db");
    let data_dir = temp_db.path().join("store");
    ingest_embedded(&store, &data_dir);

    // Ambiguous unscoped → refuse.
    let out = eg()
        .args([
            "query",
            "public-api-deltas",
            "aa10000000",
            "dupsha0000",
            "--data-dir",
        ])
        .arg(&data_dir)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    assert_repo_scope_required(&out);

    // Scoped → succeeds.
    eg().args([
        "query",
        "public-api-deltas",
        "aa10000000",
        "aa20000000",
        "--data-dir",
    ])
    .arg(&data_dir)
    .args(["--repo", "widget-a"])
    .assert()
    .success();
}
