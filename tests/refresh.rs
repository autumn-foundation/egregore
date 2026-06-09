//! Integration tests for `eg refresh` (issue #98).
//!
//! Coverage map:
//!   AC1  — end-to-end refresh workflow exists and succeeds
//!   AC2  — JSON report contains rebuilt/reused/tombstoned sets (proportional work)
//!   AC3  — query reflects add / remove / move transitions after refresh
//!   AC4  — resurrection: removed-then-re-added symbol is live again
//!   AC5  — no-op refresh (unchanged files) writes no new records or tombstones
//!   AC6  — successful refresh reports `freshness_after_refresh` = "fresh"
//!   AC7  — non-codegraph records survive a refresh unchanged
//!   AC8  — `embed_status` field documents embedding state
//!   AC9  — missing store → exit 2 `"no_prior_scan"`; wrong identity → exit 2 `"repository_identity_mismatch"`
//!   AC10 — identical rebuilt/reused/tombstoned sets across 5 independent runs

#![allow(missing_docs)]

use std::{fs, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

/// Bootstrap: full `eg scan` + `eg ingest --adapter embedded` into `data_dir`.
/// This creates the embedded store that `eg refresh` will later update.
#[cfg(feature = "embedded-aletheiadb")]
fn initial_ingest(temp: &tempfile::TempDir, repo: &Path, data_dir: &Path) {
    let graph_path = temp.path().join("initial.jsonl");
    egregore()
        .arg("scan")
        .arg(repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();
    egregore()
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

// ── AC9: Precondition failure — no prior store ─────────────────────────────

/// AC9: `--data-dir` does not exist → exit 2 with `no_prior_scan` diagnostic.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_without_prior_store_exits_2_with_no_prior_scan() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write");
    let missing_store = temp.path().join("nonexistent-store");

    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&missing_store)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(r#""code":"no_prior_scan""#));
}

// ── AC9: Precondition failure — wrong repository identity ──────────────────

/// AC9: Cache has a different `repository_id` → exit 2 with `repository_identity_mismatch`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_with_mismatched_repo_identity_exits_2() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // Inject a cache file claiming a completely different repository_id.
    let cache_path = data_dir.join("codegraph-cache.json");
    let wrong_cache = serde_json::json!({
        "schema_version": 4,
        "repository_id": "codegraph:v4:completely-different-repo-id",
        "files": { "src/lib.rs": { "hash": "aabbccdd", "records": [] } }
    });
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&wrong_cache).expect("json"),
    )
    .expect("write cache");

    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            r#""code":"repository_identity_mismatch""#,
        ));
}

// ── AC1 + AC2: Basic success, JSON report fields present ──────────────────

/// AC1 + AC2: `refresh` succeeds and `--format json` output contains all required fields.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_format_json_contains_required_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    let stdout = egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON output");
    assert!(
        parsed.get("rebuilt_files").is_some(),
        "must have rebuilt_files"
    );
    assert!(
        parsed.get("reused_files").is_some(),
        "must have reused_files"
    );
    assert!(
        parsed.get("tombstoned_files").is_some(),
        "must have tombstoned_files"
    );
    assert!(
        parsed.get("rebuilt_count").is_some(),
        "must have rebuilt_count"
    );
    assert!(
        parsed.get("reused_count").is_some(),
        "must have reused_count"
    );
    assert!(
        parsed.get("tombstoned_count").is_some(),
        "must have tombstoned_count"
    );
    assert!(
        parsed.get("embed_status").is_some(),
        "must have embed_status"
    );
    assert!(
        parsed.get("freshness_after_refresh").is_some(),
        "must have freshness_after_refresh"
    );
}

// ── AC5 + AC2: No-op for unchanged repo ───────────────────────────────────

/// AC5: Second refresh on an unchanged repo reports all files reused, none rebuilt/tombstoned.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_is_noop_when_files_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // First refresh builds the cache.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Second refresh: nothing changed.
    let stdout = egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert_eq!(parsed["rebuilt_count"], 0, "no files should be rebuilt");
    assert_eq!(
        parsed["tombstoned_count"], 0,
        "no files should be tombstoned"
    );
    assert!(
        parsed["reused_count"].as_u64().unwrap_or(0) > 0,
        "unchanged files must be reused"
    );
}

// ── AC2: Proportional rebuild ──────────────────────────────────────────────

/// AC2: Only the changed file is rebuilt; the unchanged file is reused.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_rebuilds_only_changed_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write lib.rs");
    fs::write(repo.join("src/extra.rs"), "pub fn g() -> usize { 2 }\n").expect("write extra.rs");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // First refresh: both files rebuilt (no cache yet).
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Modify only lib.rs.
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 42 }\n").expect("modify lib.rs");

    // Second refresh: only lib.rs rebuilt, extra.rs reused.
    let stdout = egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert_eq!(parsed["rebuilt_count"], 1, "only the changed file rebuilt");
    assert_eq!(parsed["reused_count"], 1, "unchanged file reused");
    assert_eq!(parsed["tombstoned_count"], 0, "no tombstones");
    let rebuilt = parsed["rebuilt_files"].as_array().expect("array");
    assert!(
        rebuilt.iter().any(|f| f.as_str() == Some("src/lib.rs")),
        "rebuilt_files must contain src/lib.rs"
    );
    let reused = parsed["reused_files"].as_array().expect("array");
    assert!(
        reused.iter().any(|f| f.as_str() == Some("src/extra.rs")),
        "reused_files must contain src/extra.rs"
    );
}

// ── AC2: Removed file tombstoned ───────────────────────────────────────────

/// AC2: A deleted source file appears in `tombstoned_files` in the JSON report.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_tombstones_removed_file_in_report() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write lib.rs");
    fs::write(
        repo.join("src/gone.rs"),
        "pub fn will_go() -> usize { 0 }\n",
    )
    .expect("write gone.rs");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // First refresh creates the cache.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    fs::remove_file(repo.join("src/gone.rs")).expect("remove gone.rs");

    let stdout = egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert_eq!(parsed["tombstoned_count"], 1, "one file tombstoned");
    let tombstoned = parsed["tombstoned_files"].as_array().expect("array");
    assert!(
        tombstoned.iter().any(|f| f.as_str() == Some("src/gone.rs")),
        "tombstoned_files must contain src/gone.rs"
    );
}

// ── AC3 (add): New symbol visible after refresh ────────────────────────────

/// AC3: A symbol added to the working tree is returned by `eg query symbol` after refresh.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_query_reflects_added_symbol() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn original() -> usize { 1 }\n",
    )
    .expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Add a new function.
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn original() -> usize { 1 }\npub fn brand_new() -> usize { 99 }\n",
    )
    .expect("update");

    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    egregore()
        .args(["query", "symbol", "brand_new", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("brand_new"));
}

// ── AC3 (remove): Deleted symbol absent after refresh ─────────────────────

/// AC3: A symbol removed from the working tree is no longer returned by `eg query symbol`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_query_does_not_return_removed_symbol() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn keep_me() -> usize { 1 }\npub fn remove_me() -> usize { 2 }\n",
    )
    .expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // First refresh puts both symbols in the store.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Remove one function.
    fs::write(repo.join("src/lib.rs"), "pub fn keep_me() -> usize { 1 }\n").expect("update");

    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    egregore()
        .args(["query", "symbol", "remove_me", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no match"));
}

// ── AC3 (move): Updated span after refresh ────────────────────────────────

/// AC3: A symbol whose body shifted to a new line returns the updated `start_line`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_query_returns_updated_span_for_moved_symbol() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    // `move_me` starts on line 1.
    fs::write(repo.join("src/lib.rs"), "pub fn move_me() -> usize { 1 }\n").expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Push `move_me` to line 2 by prepending a comment.
    fs::write(
        repo.join("src/lib.rs"),
        "// preamble\npub fn move_me() -> usize { 1 }\n",
    )
    .expect("update");

    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let stdout = egregore()
        .args(["query", "symbol", "move_me", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert_eq!(
        parsed["span"]["start_line"], 2,
        "moved symbol must report updated start_line = 2"
    );
}

// ── AC4: Resurrection ─────────────────────────────────────────────────────

/// AC4: A symbol removed in refresh 2 and re-added in refresh 3 is returned as live.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_resurrects_symbol_after_remove_then_readd() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn resurrect_me() -> usize { 1 }\n",
    )
    .expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // Refresh 1: `resurrect_me` enters the store.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Refresh 2: remove it.
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn other_fn() -> usize { 0 }\n",
    )
    .expect("remove");
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Verify it is gone.
    egregore()
        .args(["query", "symbol", "resurrect_me", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .code(2);

    // Refresh 3: re-add it.
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn other_fn() -> usize { 0 }\npub fn resurrect_me() -> usize { 1 }\n",
    )
    .expect("readd");
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Must be findable again — not permanently suppressed by its earlier tombstone.
    egregore()
        .args(["query", "symbol", "resurrect_me", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("resurrect_me"));
}

// ── AC6: Freshness reported after refresh ─────────────────────────────────

/// AC6: Successful refresh reports `freshness_after_refresh = "fresh"`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_reports_freshness_after_refresh() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    let stdout = egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert_eq!(
        parsed["freshness_after_refresh"], "fresh",
        "successful refresh must report freshness_after_refresh = \"fresh\""
    );
}

// ── AC7: Trust preservation ────────────────────────────────────────────────

/// AC7: Agent-memory Observation records written to the store are not removed by refresh.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_preserves_non_codegraph_records() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // Write and ingest an Observation into the store.
    let obs_path = temp.path().join("obs.jsonl");
    egregore()
        .args([
            "write",
            "observation",
            "--agent-id",
            "test-agent",
            "--session-id",
            "sess-001",
            "--observed-at",
            "2026-06-01T00:00:00Z",
            "--source-handle",
            "src/lib.rs:sha256:abc",
            "--text",
            "observation that must survive refresh",
            "--evidence-domain",
            "codegraph",
            "--evidence-target",
            "codegraph:v4:dummy",
            "--out",
        ])
        .arg(&obs_path)
        .assert()
        .success();
    egregore()
        .arg("ingest")
        .arg(&obs_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Modify a source file and refresh.
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 42 }\n").expect("modify");
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Open the embedded store and verify the Observation record still exists.
    let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&data_dir)
        .expect("embedded store should reopen after refresh");
    let records = sink
        .read_all_records()
        .expect("should read all records after refresh");
    assert!(
        records.iter().any(|r| matches!(
            r,
            aletheia_egregore::GraphRecord::Node {
                kind: aletheia_egregore::NodeKind::Observation,
                ..
            }
        )),
        "agent-memory Observation must survive refresh without deletion or corruption"
    );
}

// ── AC8: Embed status ──────────────────────────────────────────────────────

/// AC8: Without `--embed`, `embed_status` is `"not_requested"` in the JSON report.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_embed_status_is_not_requested_without_embed_flag() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // First refresh to build cache.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Second refresh (no changes): embed_status must be "not_requested".
    let stdout = egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert_eq!(
        parsed["embed_status"], "not_requested",
        "embed_status must be \"not_requested\" when --embed is absent"
    );
}

// ── AC3 (deleted file): Symbols from deleted file tombstoned ──────────────

/// AC3: Symbols from a deleted source file are no longer returned by `eg query symbol`
/// after refresh.  Regression: `file_tombstone` only tombstoned the File node, leaving
/// Symbol nodes and DEFINES edges live in the store.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_query_does_not_return_symbol_from_deleted_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn keep_me() -> usize { 1 }\n").expect("write lib.rs");
    fs::write(
        repo.join("src/gone.rs"),
        "pub fn in_gone_file() -> usize { 0 }\n",
    )
    .expect("write gone.rs");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // First refresh: both files in the store.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Verify `in_gone_file` is initially live in the store.
    {
        let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&data_dir)
            .expect("store should open");
        let records = sink.read_all_records().expect("should read records");
        assert!(
            records.iter().any(|r| matches!(
                r,
                aletheia_egregore::GraphRecord::Node { name: Some(n), .. }
                    if n.contains("in_gone_file")
            )),
            "symbol from gone.rs must be live in the store before file deletion"
        );
    }

    // Delete gone.rs.
    fs::remove_file(repo.join("src/gone.rs")).expect("remove gone.rs");

    // Refresh: gone.rs tombstoned along with all its symbol records.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Symbols from the deleted file must not be live in the store after refresh.
    {
        let sink = aletheia_egregore::adapters::EmbeddedAletheiaSink::open(&data_dir)
            .expect("store should reopen after refresh");
        let records = sink
            .read_all_records()
            .expect("should read records after refresh");
        assert!(
            !records.iter().any(|r| matches!(
                r,
                aletheia_egregore::GraphRecord::Node { name: Some(n), .. }
                    if n.contains("in_gone_file")
            )),
            "symbol from deleted file must not be live in the store after refresh"
        );
    }
}

// ── AC3 (remove-again cycle): Second removal must re-dead the symbol ───────

/// AC3: A symbol removed, re-added, then removed again must remain dead after the
/// second removal.  Regression: the same tombstone ID was produced for every removal
/// of the same symbol, so the second write was skipped as Matched, leaving the
/// re-added node (with higher `egregore_seq`) as the winner.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_query_does_not_return_symbol_after_remove_readd_remove() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn cycle_me() -> usize { 1 }\n",
    )
    .expect("write");
    let data_dir = temp.path().join("store");
    initial_ingest(&temp, &repo, &data_dir);

    // Refresh 1: cycle_me enters the store.
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Refresh 2: remove cycle_me.
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn other_fn() -> usize { 0 }\n",
    )
    .expect("remove");
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    egregore()
        .args(["query", "symbol", "cycle_me", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .code(2);

    // Refresh 3: re-add cycle_me.
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn other_fn() -> usize { 0 }\npub fn cycle_me() -> usize { 1 }\n",
    )
    .expect("readd");
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();
    egregore()
        .args(["query", "symbol", "cycle_me", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("cycle_me"));

    // Refresh 4: remove cycle_me again.
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn other_fn() -> usize { 0 }\n",
    )
    .expect("remove again");
    egregore()
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Must be dead again — the new tombstone must supersede the re-added node.
    egregore()
        .args(["query", "symbol", "cycle_me", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no match"));
}

// ── AC10: Determinism ──────────────────────────────────────────────────────

/// AC10: Applying the same working-tree change five times from independent identical
/// prior states produces byte-identical `rebuilt_files`/`reused_files`/`tombstoned_files`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_is_deterministic_across_five_runs() {
    let mut outputs: Vec<(serde_json::Value, serde_json::Value, serde_json::Value)> = Vec::new();

    for _ in 0..5 {
        let temp = tempfile::tempdir().expect("temp dir");
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join("src")).expect("src dir");
        fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 1 }\n").expect("write lib.rs");
        fs::write(repo.join("src/extra.rs"), "pub fn g() -> usize { 2 }\n")
            .expect("write extra.rs");
        let data_dir = temp.path().join("store");
        initial_ingest(&temp, &repo, &data_dir);

        // First refresh builds the cache.
        egregore()
            .arg("refresh")
            .arg(&repo)
            .arg("--data-dir")
            .arg(&data_dir)
            .assert()
            .success();

        // Apply the same change: modify lib.rs.
        fs::write(repo.join("src/lib.rs"), "pub fn f() -> usize { 42 }\n").expect("modify");

        // Capture the report for the incremental refresh.
        let stdout = egregore()
            .arg("refresh")
            .arg(&repo)
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--format")
            .arg("json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();

        let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
        outputs.push((
            parsed["rebuilt_files"].clone(),
            parsed["reused_files"].clone(),
            parsed["tombstoned_files"].clone(),
        ));
    }

    assert!(
        outputs.windows(2).all(|w| w[0] == w[1]),
        "refresh must produce byte-identical rebuilt/reused/tombstoned sets across 5 independent runs"
    );
}
