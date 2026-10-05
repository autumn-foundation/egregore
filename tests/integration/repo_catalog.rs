//! `eg repos` — repository catalog with query selectors (issue #193).
//!
//! These fixtures prove the acceptance criteria: one read-only call lists
//! every repository an embedded store contains with the exact handles the #67
//! repository-scoped query path accepts verbatim; two-repository
//! recall/precision are 100%; every emitted selector round-trips through a
//! scoped `eg query symbol --repo` with correctly-scoped rows; selector
//! collisions surface distinct stable IDs; recency hints are surfaced; an
//! empty store reports an explicit diagnostic; a missing store fails naming
//! the path; output is byte-identical across runs while the store is never
//! mutated; and no raw payload text leaks into the output.

#![allow(missing_docs)]

#[cfg(feature = "embedded-aletheiadb")]
use std::{collections::BTreeMap, fs, path::Path, path::PathBuf};

use assert_cmd::Command;

#[cfg(feature = "embedded-aletheiadb")]
use aletheia_egregore::{
    EdgeLabel, GraphRecord, IdentitySource, NodeKind, RepositoryIdentityPayload, SCHEMA_VERSION,
    SnapshotHead, SourceSnapshotPayload,
};

/// Sentinel narrative that must never leak into catalog output.
#[cfg(feature = "embedded-aletheiadb")]
const RAW_PAYLOAD_SENTINEL: &str = "RAW-TRANSCRIPT-SENTINEL-DO-NOT-PRINT";

#[cfg(feature = "embedded-aletheiadb")]
fn repo_id(tag: &str) -> String {
    format!("codegraph:v{SCHEMA_VERSION}:repo-{tag}")
}

#[cfg(feature = "embedded-aletheiadb")]
fn remote_payload(owner_name: &str) -> RepositoryIdentityPayload {
    RepositoryIdentityPayload {
        identity_source: IdentitySource::Remote,
        remote_url: Some(format!("https://github.com/{owner_name}")),
        root_commit_sha: None,
        canonical_path: None,
        basename: owner_name.to_owned(),
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn override_payload(name: &str) -> RepositoryIdentityPayload {
    RepositoryIdentityPayload {
        identity_source: IdentitySource::OperatorOverride,
        remote_url: None,
        root_commit_sha: None,
        canonical_path: None,
        basename: name.to_owned(),
    }
}

/// Two repositories (`acme/alpha`, `acme/beta`) that both contain `src/lib.rs`
/// defining the same-named symbol `greet`, each with a remote identity and a
/// HEAD-commit source snapshot. The symbol summary carries the raw-payload
/// sentinel so the tests can prove it never leaks into catalog output.
#[cfg(feature = "embedded-aletheiadb")]
fn two_repo_fixture_jsonl() -> String {
    let mut records: Vec<GraphRecord> = Vec::new();
    for (tag, owner_name, sha) in [
        ("alpha", "acme/alpha", "aaa111"),
        ("beta", "acme/beta", "bbb222"),
    ] {
        let rid = repo_id(tag);
        let repo = GraphRecord::node(
            rid.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(owner_name.to_owned()),
            format!("Repository {owner_name}"),
        )
        .with_repository_identity(remote_payload(owner_name))
        .with_source_snapshot(SourceSnapshotPayload {
            head: SnapshotHead::Commit {
                sha: sha.to_owned(),
            },
            dirty: false,
            repository_id: rid.clone(),
            scanned_at: "2026-01-01T00:00:00Z".to_owned(),
        });
        let file_id = format!("codegraph:v{SCHEMA_VERSION}:file-{tag}");
        let sym_id = format!("codegraph:v{SCHEMA_VERSION}:sym-{tag}");
        let file = GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("lib.rs".to_owned()),
            "file src/lib.rs".to_owned(),
        );
        let sym = GraphRecord::node(
            sym_id.clone(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("greet".to_owned()),
            format!("symbol greet {RAW_PAYLOAD_SENTINEL}"),
        );
        records.push(repo);
        records.push(file);
        records.push(sym);
        records.push(GraphRecord::edge(
            EdgeLabel::Contains,
            rid.clone(),
            file_id.clone(),
            None,
            "repository contains file".to_owned(),
        ));
        records.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id,
            sym_id,
            None,
            "file defines symbol".to_owned(),
        ));
    }
    let mut jsonl = String::new();
    for record in &records {
        jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
        jsonl.push('\n');
    }
    jsonl
}

/// Two repositories sharing one human selector (`acme`, different stable
/// IDs) — the collision case.
#[cfg(feature = "embedded-aletheiadb")]
fn collision_fixture_jsonl() -> String {
    let mut records: Vec<GraphRecord> = Vec::new();
    for tag in ["one", "two"] {
        let rid = repo_id(&format!("collide-{tag}"));
        records.push(
            GraphRecord::node(
                rid.clone(),
                NodeKind::Repository,
                None,
                None,
                Some("acme".to_owned()),
                "Repository acme".to_owned(),
            )
            .with_repository_identity(override_payload("acme")),
        );
        let file_id = format!("codegraph:v{SCHEMA_VERSION}:file-collide-{tag}");
        let sym_id = format!("codegraph:v{SCHEMA_VERSION}:sym-collide-{tag}");
        records.push(GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("lib.rs".to_owned()),
            "file src/lib.rs".to_owned(),
        ));
        records.push(GraphRecord::node(
            sym_id.clone(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("greet".to_owned()),
            "symbol greet".to_owned(),
        ));
        records.push(GraphRecord::edge(
            EdgeLabel::Contains,
            rid,
            file_id.clone(),
            None,
            "repository contains file".to_owned(),
        ));
        records.push(GraphRecord::edge(
            EdgeLabel::Defines,
            file_id,
            sym_id,
            None,
            "file defines symbol".to_owned(),
        ));
    }
    let mut jsonl = String::new();
    for record in &records {
        jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
        jsonl.push('\n');
    }
    jsonl
}

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_embedded(graph_path: &Path, data_dir: &Path) {
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
fn repos_stdout(data_dir: &Path, format: Option<&str>) -> String {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.arg("repos").arg("--data-dir").arg(data_dir);
    if let Some(format) = format {
        cmd.arg("--format").arg(format);
    }
    let output = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(output).expect("repos output should be UTF-8")
}

/// Parses one-JSON-object-per-line stdout into values.
#[cfg(feature = "embedded-aletheiadb")]
fn parse_jsonl(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("stdout line should be JSON"))
        .collect()
}

#[cfg(feature = "embedded-aletheiadb")]
fn scoped_symbol_rows(data_dir: &Path, selector: &str) -> Vec<serde_json::Value> {
    let output = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "symbol", "greet"])
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--repo")
        .arg(selector)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    parse_jsonl(&String::from_utf8(output).expect("query output should be UTF-8"))
}

/// Recursive snapshot of every regular file under `dir`: relative path -> bytes.
#[cfg(feature = "embedded-aletheiadb")]
fn dir_snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("store dir should be readable") {
            let entry = entry.expect("store dir entry should be readable");
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("entry should be under root")
                    .to_path_buf();
                let bytes = fs::read(&path).expect("store file should be readable");
                out.insert(relative, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_lists_exactly_two_repositories_with_stable_shape() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, two_repo_fixture_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let stdout = repos_stdout(&data_dir, None);
    let entries = parse_jsonl(&stdout);
    assert_eq!(
        entries.len(),
        2,
        "catalog must list exactly the two seeded repositories: {stdout}"
    );

    // Canonical ordering by stable record ID.
    assert_eq!(
        entries[0]["repository_id"].as_str(),
        Some(repo_id("alpha").as_str())
    );
    assert_eq!(
        entries[1]["repository_id"].as_str(),
        Some(repo_id("beta").as_str())
    );

    for (entry, tag, owner_name, sha) in [
        (&entries[0], "alpha", "acme/alpha", "aaa111"),
        (&entries[1], "beta", "acme/beta", "bbb222"),
    ] {
        assert_eq!(entry["repository_id"].as_str(), Some(repo_id(tag).as_str()));
        assert_eq!(entry["identity_source"].as_str(), Some("remote"));
        assert_eq!(
            entry["selector"].as_str(),
            Some(owner_name),
            "the human-usable scope selector"
        );
        assert_eq!(entry["selector_collision"], false);
        assert!(entry["colliding_selector"].is_null());
        assert_eq!(entry["node_count"], 3, "repository node + file + symbol");
        assert_eq!(entry["recency_kind"].as_str(), Some("commit"));
        assert_eq!(entry["recency_handle"].as_str(), Some(sha));
    }

    // The allow-list holds: no raw payload text leaks into the catalog.
    assert!(
        !stdout.contains(RAW_PAYLOAD_SENTINEL),
        "catalog output must never include raw record payload text"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_selectors_round_trip_through_scoped_query() {
    // Issue #193 AC: every selector the catalog emits is accepted verbatim
    // by the #67 repository-scoped query path and scopes to the right
    // repository — proven end to end through the CLI.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, two_repo_fixture_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let stdout = repos_stdout(&data_dir, None);
    let entries = parse_jsonl(&stdout);
    assert_eq!(entries.len(), 2);

    for entry in &entries {
        let selector = entry["selector"]
            .as_str()
            .expect("selector must be a string");
        let expected_id = entry["repository_id"]
            .as_str()
            .expect("id must be a string");
        let rows = scoped_symbol_rows(&data_dir, selector);
        assert!(
            !rows.is_empty(),
            "scoped query with catalog selector {selector:?} must return rows"
        );
        for row in &rows {
            assert_eq!(
                row["repository_id"].as_str(),
                Some(expected_id),
                "a scoped query must never cite the wrong repository"
            );
        }
    }
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_collision_surfaces_distinct_stable_ids() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, collision_fixture_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let stdout = repos_stdout(&data_dir, None);
    let entries = parse_jsonl(&stdout);
    assert_eq!(
        entries.len(),
        2,
        "colliding repositories are never silently de-duplicated: {stdout}"
    );

    let id_one = repo_id("collide-one");
    let id_two = repo_id("collide-two");
    assert_eq!(entries[0]["repository_id"].as_str(), Some(id_one.as_str()));
    assert_eq!(entries[1]["repository_id"].as_str(), Some(id_two.as_str()));
    for entry in &entries {
        assert_eq!(entry["selector_collision"], true);
        assert_eq!(entry["colliding_selector"].as_str(), Some("acme"));
        // The emitted selector falls back to the distinct stable ID, which
        // the #67 path accepts verbatim and scopes correctly.
        let selector = entry["selector"]
            .as_str()
            .expect("selector must be a string");
        let expected_id = entry["repository_id"]
            .as_str()
            .expect("id must be a string");
        assert_eq!(selector, expected_id);
        let rows = scoped_symbol_rows(&data_dir, selector);
        assert!(!rows.is_empty());
        for row in &rows {
            assert_eq!(row["repository_id"].as_str(), Some(expected_id));
        }
    }
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_empty_store_reports_explicit_empty_roster() {
    // A store whose only repository is tombstoned has no LIVE repositories:
    // the catalog reports an explicit empty roster with a distinct stable
    // diagnostic — never silent success.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    let dead = repo_id("dead");
    let records = vec![
        GraphRecord::node(
            dead.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("dead".to_owned()),
            "Repository dead".to_owned(),
        )
        .with_repository_identity(override_payload("dead")),
        GraphRecord::Tombstone {
            id: format!("codegraph:v{SCHEMA_VERSION}:tomb-{dead}"),
            schema_version: SCHEMA_VERSION,
            deleted_id: dead,
            summary: "removed".to_owned(),
            producer: None,
        },
    ];
    let mut jsonl = String::new();
    for record in &records {
        jsonl.push_str(&serde_json::to_string(record).expect("record should serialize"));
        jsonl.push('\n');
    }
    fs::write(&graph_path, jsonl).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let stdout = repos_stdout(&data_dir, None);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("empty roster must be valid JSON");
    assert_eq!(
        value["repositories"],
        serde_json::Value::Array(Vec::new()),
        "the roster is explicitly empty"
    );
    assert_eq!(
        value["diagnostic"]["code"].as_str(),
        Some("empty_repository_roster"),
        "a distinct, stable diagnostic"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_missing_data_dir_fails_naming_the_path() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let missing = temp.path().join("no-such-store");
    let assert = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("repos")
        .arg("--data-dir")
        .arg(&missing)
        .assert()
        .failure();
    let stderr =
        String::from_utf8(assert.get_output().stderr.clone()).expect("stderr should be UTF-8");
    assert!(
        stderr.contains(missing.to_string_lossy().as_ref()),
        "the diagnostic must name the path: {stderr}"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_is_read_only_and_byte_identical_across_runs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, two_repo_fixture_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let before = dir_snapshot(&data_dir);
    let first = repos_stdout(&data_dir, None);
    for run in 2..=5 {
        let again = repos_stdout(&data_dir, None);
        assert_eq!(
            again, first,
            "run {run} must be byte-identical to run 1 on an unchanged store"
        );
    }
    let after = dir_snapshot(&data_dir);
    assert_eq!(
        before, after,
        "the catalog must create or mutate zero records/indexes/runtime files"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_text_format_matches_eg_style() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    fs::write(&graph_path, two_repo_fixture_jsonl()).expect("fixture should write");
    ingest_embedded(&graph_path, &data_dir);

    let stdout = repos_stdout(&data_dir, Some("text"));
    assert!(
        stdout.contains(&format!("repository: {}", repo_id("alpha"))),
        "text mode lists each repository: {stdout}"
    );
    assert!(stdout.contains("identity_source: remote"), "{stdout}");
    assert!(stdout.contains("selector: acme/alpha"), "{stdout}");
    assert!(stdout.contains("nodes: 3"), "{stdout}");
    assert!(stdout.contains("recency: commit aaa111"), "{stdout}");
    assert!(
        !stdout.contains(RAW_PAYLOAD_SENTINEL),
        "text mode must not leak raw payload text either"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn repos_reads_graph_jsonl_directly() {
    // The `--graph` path catalogs the same fixture without a store.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, two_repo_fixture_jsonl()).expect("fixture should write");

    let output = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("repos")
        .arg(&graph_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("repos output should be UTF-8");
    let entries = parse_jsonl(&stdout);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["selector"].as_str(), Some("acme/alpha"));
    assert_eq!(entries[1]["selector"].as_str(), Some("acme/beta"));
}
