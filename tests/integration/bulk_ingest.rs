//! Bulk JSONL ingest (batched-transaction write path) CLI tests.
//!
//! The embedded sink commits ingest records in multi-record transactions by
//! default — the JSONL analog of `AletheiaDB`'s bulk CSV import path — with
//! `--no-bulk` restoring the historical one-transaction-per-record path and
//! `--bulk-batch-size` sizing the chunks. The operator-visible contract is
//! equivalence: the ingest report and the resulting store must be identical
//! whichever path (or chunk size) produced them, and a bad record inside a
//! chunk must fail alone with the same message in both modes.

#![allow(missing_docs, clippy::doc_markdown)]

use std::fs;
use std::sync::Mutex;

use assert_cmd::Command as CargoCommand;
use tempfile::TempDir;

/// Serializes this module's tests: concurrent embedded-store processes have
/// intermittently stalled during store open in this environment (see
/// `reingest_noop.rs`), so each test holds this guard for its whole duration.
static STORE_GUARD: Mutex<()> = Mutex::new(());

fn lock_store_guard() -> std::sync::MutexGuard<'static, ()> {
    STORE_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should run")
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn fixture_repo() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

/// Scans the fixture repo and returns the graph JSONL text.
fn scan_fixture(dir: &TempDir) -> (std::path::PathBuf, String) {
    let graph = dir.path().join("graph.jsonl");
    let output = egregore()
        .args([
            "scan",
            fixture_repo().to_str().unwrap(),
            "--out",
            graph.to_str().unwrap(),
            "--no-index",
        ])
        .output()
        .expect("scan should run");
    assert!(
        output.status.success(),
        "scan failed: {}",
        stderr_of(&output)
    );
    let text = fs::read_to_string(&graph).expect("graph should read");
    (graph, text)
}

fn ingest(
    dir: &TempDir,
    graph: &std::path::Path,
    store: &str,
    extra: &[&str],
) -> std::process::Output {
    let data_dir = dir.path().join(store);
    let mut cmd = egregore();
    cmd.args([
        "ingest",
        graph.to_str().unwrap(),
        "--adapter",
        "embedded",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--no-index",
    ])
    .args(extra);
    cmd.output().expect("ingest should run")
}

/// Exports a store's canonical JSONL dump (byte-identical for identical
/// stores, per the `eg export` contract).
fn export_store(dir: &TempDir, store: &str) -> String {
    let out = dir.path().join(format!("{store}.export.jsonl"));
    let output = egregore()
        .args([
            "export",
            "--data-dir",
            dir.path().join(store).to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("export should run");
    assert!(
        output.status.success(),
        "export of {store} failed: {}",
        stderr_of(&output)
    );
    fs::read_to_string(&out).expect("export should read")
}

/// The ingest report lines (`attempted:` / `inserted:` / …) from stdout.
fn report_lines(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| {
            [
                "attempted:",
                "inserted:",
                "unchanged:",
                "succeeded:",
                "failed:",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn bulk_default_matches_no_bulk_report_and_store() {
    let _guard = lock_store_guard();
    let dir = TempDir::new().expect("temp dir");
    let (graph, _) = scan_fixture(&dir);

    let bulk = ingest(&dir, &graph, "store-bulk", &[]);
    assert!(
        bulk.status.success(),
        "bulk ingest failed: {}",
        stderr_of(&bulk)
    );
    let per_record = ingest(&dir, &graph, "store-per-record", &["--no-bulk"]);
    assert!(
        per_record.status.success(),
        "per-record ingest failed: {}",
        stderr_of(&per_record)
    );

    assert_eq!(
        report_lines(&stdout_of(&bulk)),
        report_lines(&stdout_of(&per_record)),
        "ingest reports must be identical across write paths"
    );
    assert_eq!(
        export_store(&dir, "store-bulk"),
        export_store(&dir, "store-per-record"),
        "stores built by the bulk and per-record paths must be identical"
    );
}

#[test]
fn bulk_batch_size_one_matches_default() {
    let _guard = lock_store_guard();
    let dir = TempDir::new().expect("temp dir");
    let (graph, _) = scan_fixture(&dir);

    let default = ingest(&dir, &graph, "store-default", &[]);
    assert!(default.status.success());
    let singles = ingest(&dir, &graph, "store-singles", &["--bulk-batch-size", "1"]);
    assert!(
        singles.status.success(),
        "batch-size-1 ingest failed: {}",
        stderr_of(&singles)
    );

    assert_eq!(
        report_lines(&stdout_of(&default)),
        report_lines(&stdout_of(&singles))
    );
    assert_eq!(
        export_store(&dir, "store-default"),
        export_store(&dir, "store-singles")
    );
}

#[test]
fn bad_edge_fails_identically_in_bulk_and_per_record_modes() {
    let _guard = lock_store_guard();
    let dir = TempDir::new().expect("temp dir");
    let (_, graph_text) = scan_fixture(&dir);

    // Append an edge whose target node does not exist anywhere: the sink
    // must reject exactly that record, with the same message, in both modes.
    let mut edge: serde_json::Value = graph_text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["record_type"] == "edge")
        .expect("fixture graph must contain an edge");
    edge["id"] = serde_json::json!("codegraph:v1:bulk-ghost-edge");
    edge["target"] = serde_json::json!("codegraph:v1:bulk-ghost-node");
    let mut bad_graph_text = graph_text;
    bad_graph_text.push_str(&serde_json::to_string(&edge).expect("edge serializes"));
    bad_graph_text.push('\n');
    let bad_graph = dir.path().join("bad-graph.jsonl");
    fs::write(&bad_graph, bad_graph_text).expect("bad graph writes");

    let bulk = ingest(&dir, &bad_graph, "store-bad-bulk", &[]);
    let per_record = ingest(&dir, &bad_graph, "store-bad-per-record", &["--no-bulk"]);
    assert!(
        !bulk.status.success(),
        "bulk ingest must fail on the bad edge"
    );
    assert!(
        !per_record.status.success(),
        "per-record ingest must fail on the bad edge"
    );
    assert_eq!(
        report_lines(&stdout_of(&bulk)),
        report_lines(&stdout_of(&per_record)),
        "failure accounting must be identical across write paths"
    );
    let bulk_stderr = stderr_of(&bulk);
    let bulk_failures: Vec<&str> = bulk_stderr
        .lines()
        .filter(|line| line.contains("bulk-ghost-edge"))
        .collect();
    let per_record_stderr = stderr_of(&per_record);
    let per_record_failures: Vec<&str> = per_record_stderr
        .lines()
        .filter(|line| line.contains("bulk-ghost-edge"))
        .collect();
    assert_eq!(
        bulk_failures.len(),
        1,
        "exactly one failure line: {bulk_failures:?}"
    );
    assert_eq!(
        bulk_failures, per_record_failures,
        "the bad record's failure message must be identical across write paths"
    );
    assert!(
        bulk_failures[0].contains("has not been written"),
        "{}",
        bulk_failures[0]
    );
}
