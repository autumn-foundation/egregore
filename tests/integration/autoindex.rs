//! Auto-built sidecar index tests.
//!
//! The sidecar index (`<graph>.idx`, issue #447) is a pure access-path
//! optimization for targeted `eg query … --graph` lanes, but before this
//! change its only writer was the explicit `eg index` command — so the
//! default `scan → ingest` / `scan → query` flows never produced one and
//! every targeted query silently cold-scanned the whole JSONL.
//!
//! These tests pin the producer-side contract: `eg scan`, `eg scan-history`,
//! and `eg ingest` build (or refresh) a valid sidecar for the graph they
//! produce/consume by default, `--no-index` opts out, and a
//! `scan-history --resume-from` no-op (which leaves `--out` untouched) does
//! not build or rebuild one either.

#![allow(missing_docs, clippy::doc_markdown)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::graph_index::GraphIndex;
use assert_cmd::Command as CargoCommand;

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should run")
}

fn idx_path(graph: &Path) -> PathBuf {
    let mut s = graph.as_os_str().to_os_string();
    s.push(".idx");
    PathBuf::from(s)
}

/// Asserts the sidecar next to `graph` exists and is content-addressed to the
/// graph's current bytes (the exact validity a query lane checks).
fn assert_valid_index(graph: &Path) {
    let idx = idx_path(graph);
    assert!(
        idx.exists(),
        "sidecar index should exist at {}",
        idx.display()
    );
    GraphIndex::load_for(graph).unwrap_or_else(|error| {
        panic!(
            "sidecar index for {} should validate against the current graph: {error}",
            graph.display()
        )
    });
}

fn write_source(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn seed_plain_repo(repo: &Path) {
    write_source(
        repo,
        "src/lib.rs",
        "pub fn alpha() -> u32 { beta() }\npub fn beta() -> u32 { 7 }\n",
    );
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

fn commit(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        output.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn seed_git_repo(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
    write_source(repo, "src/lib.rs", "pub fn original() -> u32 { 1 }\n");
    commit(repo, "initial symbol", "2026-01-01T00:00:00Z");
    write_source(repo, "src/lib.rs", "pub fn renamed() -> u32 { 2 }\n");
    commit(repo, "rename symbol", "2026-01-02T00:00:00Z");
}

fn run_scan(repo: &Path, out: &Path, extra: &[&str]) {
    let mut cmd = egregore();
    cmd.arg("scan").arg(repo).arg("--out").arg(out).args(extra);
    cmd.assert().success();
}

fn run_scan_history(repo: &Path, out: &Path, extra: &[&str]) {
    let mut cmd = egregore();
    cmd.arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(out)
        .args(extra);
    cmd.assert().success();
}

fn run_query(graph: &Path, args: &[&str]) -> (Vec<u8>, i32) {
    let output = egregore()
        .args(args)
        .arg("--graph")
        .arg(graph)
        .output()
        .expect("query should run");
    (output.stdout, output.status.code().unwrap_or(-1))
}

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

#[test]
fn scan_builds_valid_sidecar_by_default() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    seed_plain_repo(&repo);
    let graph = temp.path().join("graph.jsonl");

    run_scan(&repo, &graph, &[]);

    assert_valid_index(&graph);
}

#[test]
fn scan_no_index_opts_out() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    seed_plain_repo(&repo);
    let graph = temp.path().join("graph.jsonl");

    run_scan(&repo, &graph, &["--no-index"]);

    assert!(
        !idx_path(&graph).exists(),
        "--no-index must not write a sidecar"
    );
}

#[test]
fn rescan_refreshes_sidecar() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    seed_plain_repo(&repo);
    let graph = temp.path().join("graph.jsonl");

    run_scan(&repo, &graph, &[]);
    assert_valid_index(&graph);
    let first_idx = fs::read(idx_path(&graph)).expect("read first idx");

    // Change the code and re-scan to the same output path: the sidecar must
    // be rebuilt over the new graph bytes, not left stale.
    write_source(
        &repo,
        "src/lib.rs",
        "pub fn alpha() -> u32 { beta() }\npub fn beta() -> u32 { 7 }\npub fn delta() -> u32 { 9 }\n",
    );
    run_scan(&repo, &graph, &[]);

    assert_valid_index(&graph);
    let second_idx = fs::read(idx_path(&graph)).expect("read second idx");
    assert_ne!(first_idx, second_idx, "re-scan must rebuild the sidecar");
    let (out, code) = run_query(&graph, &["query", "file", "src/lib.rs"]);
    assert_eq!(code, 0);
    assert!(
        String::from_utf8_lossy(&out).contains("delta"),
        "query over the refreshed index must see the new symbol"
    );
}

#[test]
fn auto_built_index_query_output_is_byte_identical_to_cold() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    seed_plain_repo(&repo);
    let graph = temp.path().join("graph.jsonl");

    run_scan(&repo, &graph, &[]);
    assert_valid_index(&graph);

    for args in [
        &["query", "symbol", "alpha"][..],
        &["query", "file", "src/lib.rs"][..],
        &["query", "deps", "alpha"][..],
        &["query", "context", "alpha"][..],
    ] {
        let (indexed_out, indexed_code) = run_query(&graph, args);
        fs::remove_file(idx_path(&graph)).expect("remove idx for cold run");
        let (cold_out, cold_code) = run_query(&graph, args);
        assert_eq!(cold_code, indexed_code, "exit code differs for {args:?}");
        assert_eq!(
            String::from_utf8_lossy(&cold_out),
            String::from_utf8_lossy(&indexed_out),
            "stdout differs cold vs auto-indexed for {args:?}"
        );
        // Restore the auto-built index for the next lane.
        egregore().args(["index"]).arg(&graph).assert().success();
    }
}

// ---------------------------------------------------------------------------
// scan-history
// ---------------------------------------------------------------------------

#[test]
fn scan_history_builds_valid_sidecar_by_default() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_git_repo(&repo);
    let graph = temp.path().join("history.jsonl");

    run_scan_history(&repo, &graph, &[]);

    assert_valid_index(&graph);
    // A history graph's index records the temporal signal (the loader then
    // cold-scans by design, #457) — pin that the auto-built index is a real
    // history index, not a plain one.
    let index = GraphIndex::load_for(&graph).expect("valid index");
    assert!(
        index.body.has_temporal_history,
        "auto-built history index must set has_temporal_history"
    );
}

#[test]
fn scan_history_no_index_opts_out() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_git_repo(&repo);
    let graph = temp.path().join("history.jsonl");

    run_scan_history(&repo, &graph, &["--no-index"]);

    assert!(
        !idx_path(&graph).exists(),
        "--no-index must not write a sidecar"
    );
}

#[test]
fn scan_history_resume_noop_builds_no_sidecar_and_resume_with_commits_rebuilds() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_git_repo(&repo);
    let graph = temp.path().join("history.jsonl");

    run_scan_history(&repo, &graph, &[]);
    assert_valid_index(&graph);
    let graph_bytes = fs::read(&graph).expect("read graph");

    // A zero-new-commit resume leaves --out untouched; it must not build or
    // rebuild a sidecar either (the frontier's state did not change).
    fs::remove_file(idx_path(&graph)).expect("remove idx");
    egregore()
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph)
        .arg("--resume-from")
        .arg(&graph)
        .assert()
        .success();
    assert_eq!(
        fs::read(&graph).expect("graph after resume"),
        graph_bytes,
        "no-op resume must leave the graph byte-identical"
    );
    assert!(
        !idx_path(&graph).exists(),
        "no-op resume must not build a sidecar"
    );

    // A resume that does replay new commits rewrites the graph, so it must
    // leave a fresh, valid sidecar behind.
    write_source(&repo, "src/lib.rs", "pub fn renamed() -> u32 { 3 }\n");
    commit(&repo, "bump renamed", "2026-01-03T00:00:00Z");
    egregore()
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph)
        .arg("--resume-from")
        .arg(&graph)
        .assert()
        .success();
    assert_valid_index(&graph);
}

// ---------------------------------------------------------------------------
// ingest
// ---------------------------------------------------------------------------

/// Produces a graph file with NO sidecar (via `scan --no-index`) so the
/// ingest tests isolate ingest as the sidecar's builder.
fn scan_without_index(temp: &Path) -> PathBuf {
    let repo = temp.join("repo");
    seed_plain_repo(&repo);
    let graph = temp.join("graph.jsonl");
    run_scan(&repo, &graph, &["--no-index"]);
    assert!(!idx_path(&graph).exists());
    graph
}

#[test]
fn ingest_dry_run_rebuilds_sidecar() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = scan_without_index(temp.path());

    egregore()
        .arg("ingest")
        .arg(&graph)
        .arg("--adapter")
        .arg("dry-run")
        .assert()
        .success();

    assert_valid_index(&graph);
}

#[test]
fn ingest_embedded_rebuilds_sidecar() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = scan_without_index(temp.path());
    let data_dir = temp.path().join("store");

    egregore()
        .arg("ingest")
        .arg(&graph)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    assert_valid_index(&graph);
}

#[test]
fn ingest_no_index_opts_out() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = scan_without_index(temp.path());

    egregore()
        .arg("ingest")
        .arg(&graph)
        .arg("--adapter")
        .arg("dry-run")
        .arg("--no-index")
        .assert()
        .success();

    assert!(
        !idx_path(&graph).exists(),
        "--no-index must not write a sidecar"
    );
}

#[test]
fn ingest_refreshes_stale_sidecar() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = scan_without_index(temp.path());

    // Build a sidecar, then change the graph bytes behind its back so the
    // sidecar is stale; ingest must rebuild it over the current bytes.
    egregore().args(["index"]).arg(&graph).assert().success();
    let mut bytes = fs::read(&graph).expect("read graph");
    bytes.extend_from_slice(b"\n");
    fs::write(&graph, &bytes).expect("rewrite graph");
    assert!(
        GraphIndex::load_for(&graph).is_err(),
        "fixture sidecar should be stale before ingest"
    );

    egregore()
        .arg("ingest")
        .arg(&graph)
        .arg("--adapter")
        .arg("dry-run")
        .assert()
        .success();

    assert_valid_index(&graph);
}
