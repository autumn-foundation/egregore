#![allow(missing_docs)]

//! Tests for `eg query resolve <record_id>` (issue #160): the read-back half
//! of the citation contract.
//!
//! A `record_id` handle emitted by `eg query symbol|file|semantic`
//! dereferences back to its record with a typed drift verdict:
//!
//! - `valid` — the handle names a live record (byte-identical
//!   `repo_relative_path` + `span` when the store is at the same commit).
//! - `drifted` — the record exists at the pinned `--at`/`--as-of` snapshot
//!   but its coordinates or content differ from (or are gone in) the current
//!   view.
//! - `dangling` — no record matches the id in the requested view: a
//!   structured `dangling_handle` envelope on stdout, exit 2 — never a silent
//!   empty success, never a fuzzy nearest-match guess.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use serde_json::Value;

fn eg() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should run")
}

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

fn scan_graph(graph_path: &Path) {
    eg().arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(graph_path)
        .assert()
        .success();
}

/// First JSONL row of `eg query symbol <name> --graph <graph>`.
fn query_symbol_row(graph: &Path, name: &str) -> Value {
    query_symbol_row_scoped(graph, name, false)
}

/// First JSONL row of `eg query symbol <name> --graph <graph>`, optionally
/// across the union of all commit snapshots (for symbols deleted at HEAD).
fn query_symbol_row_scoped(graph: &Path, name: &str, all_history: bool) -> Value {
    let mut cmd = eg();
    cmd.arg("query")
        .arg("symbol")
        .arg(name)
        .arg("--graph")
        .arg(graph);
    if all_history {
        cmd.arg("--all-history");
    }
    let output = cmd.assert().success().get_output().stdout.clone();
    let line = String::from_utf8(output).expect("query output is UTF-8");
    serde_json::from_str(line.lines().next().expect("at least one row")).expect("row is JSON")
}

/// First JSONL row of `eg query file <path> --graph <graph>`.
fn query_file_row(graph: &Path, path: &str) -> Value {
    let output = eg()
        .arg("query")
        .arg("file")
        .arg(path)
        .arg("--graph")
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let line = String::from_utf8(output).expect("query output is UTF-8");
    serde_json::from_str(line.lines().next().expect("at least one row")).expect("row is JSON")
}

/// Run `eg query resolve` and return (exit code, stdout, stderr).
fn resolve(graph: &Path, args: &[&str]) -> (i32, String, String) {
    let output = eg()
        .arg("query")
        .arg("resolve")
        .args(args)
        .arg("--graph")
        .arg(graph)
        .output()
        .expect("resolve should execute");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// ---------------------------------------------------------------------------
// Round-trip: the citation contract reads back byte-identical (AC2, AC5, AC7)
// ---------------------------------------------------------------------------

#[test]
fn resolve_round_trip_symbol_is_byte_identical_and_valid() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    let row = query_symbol_row(&graph, "answer");
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    let (code, stdout, _stderr) = resolve(&graph, &[record_id]);
    assert_eq!(code, 0, "resolve of a live handle exits 0:\n{stdout}");
    let answer: Value = serde_json::from_str(stdout.trim()).expect("answer is JSON");
    assert_eq!(answer["verdict"], "valid");
    assert_eq!(answer["record_id"], record_id);
    assert_eq!(
        answer["repo_relative_path"], row["repo_relative_path"],
        "path round-trips byte-identical"
    );
    assert_eq!(
        answer["span"], row["span"],
        "span round-trips byte-identical"
    );

    // Determinism (AC7): the identical invocation is byte-identical output.
    let (code2, stdout2, _) = resolve(&graph, &[record_id]);
    assert_eq!(code2, 0);
    assert_eq!(stdout, stdout2, "resolve is deterministic");
}

#[test]
fn resolve_round_trip_file_is_byte_identical_and_valid() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    let row = query_file_row(&graph, "src/lib.rs");
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    let (code, stdout, _) = resolve(&graph, &[record_id]);
    assert_eq!(code, 0, "resolve of a live file handle exits 0:\n{stdout}");
    let answer: Value = serde_json::from_str(stdout.trim()).expect("answer is JSON");
    assert_eq!(answer["verdict"], "valid");
    assert_eq!(answer["record_id"], record_id);
    assert_eq!(answer["repo_relative_path"], row["repo_relative_path"]);
    assert_eq!(answer["span"], row["span"]);
}

#[test]
fn resolve_text_format_is_a_human_readable_line() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    let row = query_symbol_row(&graph, "answer");
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    let output = eg()
        .arg("query")
        .arg("resolve")
        .arg(record_id)
        .arg("--graph")
        .arg(&graph)
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("text output is UTF-8");
    let line = text.trim();
    assert!(
        !line.contains('\n'),
        "text format is one human-readable line, got: {line}"
    );
    assert!(line.contains("valid"), "line carries the verdict: {line}");
    assert!(
        line.contains(row["repo_relative_path"].as_str().unwrap()),
        "line carries the cited path: {line}"
    );
}

// ---------------------------------------------------------------------------
// Dangling handles: structured envelope, exit 2, never silent (AC4)
// ---------------------------------------------------------------------------

/// Latest-write-wins over an append-only `--graph`: a record re-ingested AFTER
/// its own forget tombstone is live again, so it resolves `valid` exactly as it
/// does over the embedded store — not `dangling_handle`.
#[test]
fn resolve_revived_record_is_live_over_graph() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    let row = query_file_row(&graph, "src/lib.rs");
    let record_id = row["record_id"].as_str().expect("row carries record_id");
    let original = fs::read_to_string(&graph).expect("graph readable");
    let node_line = original
        .lines()
        .find(|line| {
            serde_json::from_str::<Value>(line)
                .is_ok_and(|v| v["record_type"] == "node" && v["id"] == record_id)
        })
        .expect("the file's node line");
    let tombstone = serde_json::json!({
        "record_type": "tombstone",
        "id": format!("tomb:{record_id}"),
        "deleted_id": record_id,
        "schema_version": 1,
        "summary": "tombstone",
    })
    .to_string();

    // Tombstoned and NOT revived: dangling.
    let tombstoned = temp.path().join("tombstoned.jsonl");
    fs::write(&tombstoned, format!("{original}\n{tombstone}\n")).expect("write");
    let (code, stdout, _) = resolve(&tombstoned, &[record_id]);
    assert_eq!(code, 2, "a live tombstone still dangles:\n{stdout}");

    // Tombstoned, then re-ingested: the later write wins.
    let revived = temp.path().join("revived.jsonl");
    fs::write(&revived, format!("{original}\n{tombstone}\n{node_line}\n")).expect("write");
    let (code, stdout, _) = resolve(&revived, &[record_id]);
    assert_eq!(code, 0, "a revived record resolves live:\n{stdout}");
    let answer: Value = serde_json::from_str(stdout.trim()).expect("answer is JSON");
    assert_eq!(answer["verdict"], "valid");
}

#[test]
fn resolve_dangling_handle_returns_envelope_and_exit_2() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    // Well-formed but minted from nothing: no record can match it.
    let bogus = "codegraph:v1:0000000000000000000000000000000000000000000000000000000000000000";
    let (code, stdout, _stderr) = resolve(&graph, &[bogus]);
    assert_eq!(code, 2, "dangling handle exits 2, got {code}:\n{stdout}");
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("envelope is JSON");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "dangling_handle");
    assert_eq!(envelope["error"]["record_id"], bogus);
}

#[test]
fn resolve_malformed_handle_exits_1() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    let (code, stdout, _stderr) = resolve(&graph, &["not-a-handle"]);
    assert_eq!(code, 1, "malformed handle exits 1, got {code}");
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("envelope is JSON");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "malformed_handle");
}

#[test]
fn resolve_non_codegraph_domain_handle_is_unsupported_not_dangling() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    // Agent-memory handles are out of scope for this lane (issue #160):
    // refused explicitly, never misread as dangling codegraph handles.
    let (code, stdout, _stderr) = resolve(&graph, &["agent_memory:v1:abc123"]);
    assert_eq!(code, 1, "non-codegraph handle exits 1, got {code}");
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("envelope is JSON");
    assert_eq!(envelope["error"]["code"], "unsupported_handle_domain");
}

#[test]
fn resolve_at_and_as_of_conflict() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    eg().arg("query")
        .arg("resolve")
        .arg("codegraph:v1:abc")
        .arg("--graph")
        .arg(&graph)
        .arg("--at")
        .arg("deadbee")
        .arg("--as-of")
        .arg("2026-01-01T00:00:00Z")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn resolve_requires_a_transport_flag() {
    eg().arg("query")
        .arg("resolve")
        .arg("codegraph:v1:abc")
        .assert()
        .failure()
        .stderr(predicate::str::contains("provide --graph"));
}

#[test]
fn resolve_rejects_graph_and_data_dir_together() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    eg().arg("query")
        .arg("resolve")
        .arg("codegraph:v1:abc")
        .arg("--graph")
        .arg(temp.path().join("graph.jsonl"))
        .arg("--data-dir")
        .arg(temp.path().join("store"))
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// Temporal selectors: drift verdicts against history (AC3, AC6)
// ---------------------------------------------------------------------------

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git should execute")
        .status;
    assert!(status.success(), "git {args:?} failed");
}

fn git_commit(repo: &Path, message: &str, date: &str) -> String {
    git(repo, &["add", "."]);
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .expect("git commit should execute");
    assert!(output.status.success(), "git commit failed");
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse should execute")
            .stdout,
    )
    .expect("sha is UTF-8")
    .trim()
    .to_owned()
}

/// Seeds a linear history where `target`'s span moves between commits and the
/// symbol is deleted at HEAD:
///
/// - c1: `target` at the top of `src/lib.rs`
/// - c2: a new function is inserted above it (span shifts down)
/// - c3: `target` is deleted outright
fn seed_drift_repo(repo: &Path) -> [String; 3] {
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "codegraph@example.invalid"]);
    git(repo, &["config", "user.name", "Codegraph Test"]);
    git(repo, &["config", "core.autocrlf", "false"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn target() -> u32 { 1 }\n").expect("lib.rs");
    let first = git_commit(repo, "add target", "2026-03-01T00:00:00Z");

    fs::write(
        repo.join("src/lib.rs"),
        "pub fn above() -> u32 { 0 }\n\npub fn target() -> u32 { 1 }\n",
    )
    .expect("lib.rs");
    let second = git_commit(repo, "insert function above target", "2026-03-02T00:00:00Z");

    fs::write(repo.join("src/lib.rs"), "pub fn above() -> u32 { 0 }\n").expect("lib.rs");
    let third = git_commit(repo, "delete target", "2026-03-03T00:00:00Z");

    [first, second, third]
}

/// Seeds a two-commit history where `target`'s span moves but the symbol
/// stays live at HEAD (no deletion):
///
/// - c1: `target` at the top of `src/lib.rs`
/// - c2: a new function is inserted above it (span shifts down)
fn seed_span_shift_repo(repo: &Path) -> [String; 2] {
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "codegraph@example.invalid"]);
    git(repo, &["config", "user.name", "Codegraph Test"]);
    git(repo, &["config", "core.autocrlf", "false"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(repo.join("src")).expect("src dir");
    fs::write(repo.join("src/lib.rs"), "pub fn target() -> u32 { 1 }\n").expect("lib.rs");
    let first = git_commit(repo, "add target", "2026-03-01T00:00:00Z");

    fs::write(
        repo.join("src/lib.rs"),
        "pub fn above() -> u32 { 0 }\n\npub fn target() -> u32 { 1 }\n",
    )
    .expect("lib.rs");
    let second = git_commit(repo, "insert function above target", "2026-03-02T00:00:00Z");

    [first, second]
}

fn scan_history_graph(repo: &Path, graph_path: &Path) {
    eg().arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(graph_path)
        .assert()
        .success();
}

fn resolve_with_selector(graph: &Path, record_id: &str, selector: &[&str]) -> (i32, Value) {
    let mut cmd = eg();
    cmd.arg("query").arg("resolve").arg(record_id);
    for flag in selector {
        cmd.arg(flag);
    }
    cmd.arg("--graph").arg(graph);
    let output = cmd.output().expect("resolve should execute");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let answer: Value = serde_json::from_str(stdout.trim()).expect("resolve answer is JSON");
    (code, answer)
}

#[test]
fn resolve_drifted_when_span_moves_between_commits() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    // Two-commit fixture: the span moves at c2 but `target` stays live at
    // HEAD, so the bare current-view resolve is valid (the deleted-at-HEAD
    // case has its own test below).
    let [first, second] = seed_span_shift_repo(&repo);
    let graph = temp.path().join("history.graph.jsonl");
    scan_history_graph(&repo, &graph);

    // The stable id is commit-independent (ADR-0004): one handle for `target`
    // across the history. Resolve it at the CURRENT view first.
    let row = query_symbol_row_scoped(&graph, "target", true);
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    // Current view: the handle is live.
    let (code, answer) = resolve_with_selector(&graph, record_id, &[]);
    assert_eq!(code, 0);
    assert_eq!(answer["verdict"], "valid");

    // Pinned at c1: the span there differs from the current view — drifted,
    // with the current coordinates disclosed.
    let (code, answer) = resolve_with_selector(&graph, record_id, &["--at", &first]);
    assert_eq!(code, 0, "drifted is a successful dereference");
    assert_eq!(answer["verdict"], "drifted");
    assert_eq!(answer["record_id"], record_id);
    assert_ne!(
        answer["span"], answer["current_span"],
        "the pinned span and the current span differ"
    );
    assert!(answer["current_span"].is_object());
    assert_eq!(answer["current_repo_relative_path"], "src/lib.rs");

    // Pinned at c2 (the last commit where the coordinates match the current
    // view): valid again.
    let (code, answer) = resolve_with_selector(&graph, record_id, &["--at", &second]);
    assert_eq!(code, 0);
    assert_eq!(answer["verdict"], "valid");
    assert_eq!(answer["git_commit"], second.as_str());
}

#[test]
fn resolve_as_of_selects_the_historical_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    seed_drift_repo(&repo);
    let graph = temp.path().join("history.graph.jsonl");
    scan_history_graph(&repo, &graph);

    let row = query_symbol_row_scoped(&graph, "target", true);
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    // An instant inside c1's reign resolves the c1 snapshot: drifted vs now.
    let (code, answer) =
        resolve_with_selector(&graph, record_id, &["--as-of", "2026-03-01T12:00:00Z"]);
    assert_eq!(code, 0);
    assert_eq!(answer["verdict"], "drifted");

    // An instant before the first commit: the snapshot has no such record.
    let (code, answer) =
        resolve_with_selector(&graph, record_id, &["--as-of", "2026-01-01T00:00:00Z"]);
    assert_eq!(code, 2, "pre-history snapshot dangles");
    assert_eq!(answer["error"]["code"], "dangling_handle");
}

#[test]
fn resolve_drifted_when_symbol_is_gone_at_head() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    let [first, ..] = seed_drift_repo(&repo);
    let graph = temp.path().join("history.graph.jsonl");
    scan_history_graph(&repo, &graph);

    // `target` is deleted at HEAD: the current view has no live record, so a
    // bare resolve dangles — but pinned at c1 the record exists and is
    // reported drifted (gone now), not dangling.
    let row = query_symbol_row_scoped(&graph, "target", true);
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    let (code, answer) = resolve_with_selector(&graph, record_id, &[]);
    assert_eq!(
        code, 2,
        "deleted-at-HEAD handle dangles in the current view"
    );
    assert_eq!(answer["error"]["code"], "dangling_handle");

    let (code, answer) = resolve_with_selector(&graph, record_id, &["--at", &first]);
    assert_eq!(code, 0);
    assert_eq!(answer["verdict"], "drifted");
    assert!(
        answer.get("current_span").is_none_or(Value::is_null),
        "no current coordinates when the record is gone: {answer}"
    );
}

#[test]
fn resolve_malformed_as_of_exits_1() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    scan_graph(&graph);

    let row = query_symbol_row(&graph, "answer");
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    let (code, answer) = resolve_with_selector(&graph, record_id, &["--as-of", "not-a-time"]);
    assert_eq!(code, 1, "malformed --as-of exits 1");
    assert_eq!(answer["ok"], false);
}

// ---------------------------------------------------------------------------
// Direct --data-dir transport: the citation contract holds on the embedded
// store too (AC1: --graph, --data-dir, or --daemon).
// ---------------------------------------------------------------------------

/// Ingests `graph` into a fresh embedded data dir via the documented CLI.
fn ingest_graph_to_data_dir(graph: &Path, data_dir: &Path) {
    eg().arg("ingest")
        .arg(graph)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn resolve_round_trip_data_dir_is_byte_identical_and_valid() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph = temp.path().join("graph.jsonl");
    let data_dir = temp.path().join("store");
    scan_graph(&graph);
    ingest_graph_to_data_dir(&graph, &data_dir);

    let row = query_symbol_row(&graph, "answer");
    let record_id = row["record_id"].as_str().expect("row carries record_id");

    let output = eg()
        .arg("query")
        .arg("resolve")
        .arg(record_id)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("resolve should execute");
    assert!(
        output.status.success(),
        "resolve via --data-dir exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: Value = serde_json::from_slice(&output.stdout).expect("answer is JSON");
    assert_eq!(answer["verdict"], "valid");
    assert_eq!(answer["record_id"], record_id);
    assert_eq!(
        answer["repo_relative_path"], row["repo_relative_path"],
        "path round-trips byte-identical via --data-dir"
    );
    assert_eq!(
        answer["span"], row["span"],
        "span round-trips byte-identical via --data-dir"
    );
}
