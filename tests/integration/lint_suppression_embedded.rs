//! `LintSuppression` embedded round-trip tests (F2).
//!
//! The scanner emits `NodeKind::LintSuppression` records for
//! `#[allow(...)]` attributes (issue #227), and the embedded sink writes
//! them (label + `lint_suppression_json` payload), but the read-back kind
//! parser had no `LintSuppression` arm — so any ingest of a graph
//! containing one failed with "read-back verification failed … unknown
//! embedded node kind LintSuppression", and `inspect --data-dir` failed on
//! every store that contained one. These tests pin the round-trip: scan a
//! fixture with an `#[allow(...)]`, ingest it embedded, and prove the
//! suppression reads back from the store with its payload intact.

#![allow(missing_docs, clippy::doc_markdown)]

use std::fs;

use assert_cmd::Command as CargoCommand;
use tempfile::TempDir;

fn egregore() -> CargoCommand {
    CargoCommand::cargo_bin("egregore").expect("binary should run")
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Seeds a one-file Rust fixture whose `#[allow(dead_code)]` attribute makes
/// the scanner emit exactly one `LintSuppression` node.
fn seed_suppression_repo(dir: &TempDir) -> std::path::PathBuf {
    let repo = dir.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir");
    fs::write(
        src.join("lib.rs"),
        "#[allow(dead_code)]\nfn unused_helper() -> u32 { 42 }\n\npub fn alpha() -> u32 { 7 }\n",
    )
    .expect("fixture file");
    repo
}

#[test]
fn lint_suppression_round_trips_through_embedded_store() {
    let dir = TempDir::new().expect("temp dir");
    let repo = seed_suppression_repo(&dir);
    let graph = dir.path().join("graph.jsonl");
    let store = dir.path().join("store");

    // Scan: the graph must actually contain a LintSuppression node, or the
    // rest of this test proves nothing.
    let scan = egregore()
        .args([
            "scan",
            repo.to_str().unwrap(),
            "--out",
            graph.to_str().unwrap(),
        ])
        .output()
        .expect("scan runs");
    assert!(
        scan.status.success(),
        "scan failed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&scan),
        stderr_of(&scan)
    );
    let jsonl = fs::read_to_string(&graph).expect("graph readable");
    assert!(
        jsonl.contains("\"LintSuppression\""),
        "fixture scan must emit a LintSuppression node"
    );

    // Ingest embedded: pre-fix this fails per-record with
    // "unknown embedded node kind LintSuppression".
    let ingest = egregore()
        .args([
            "ingest",
            graph.to_str().unwrap(),
            "--adapter",
            "embedded",
            "--data-dir",
            store.to_str().unwrap(),
        ])
        .output()
        .expect("ingest runs");
    assert!(
        ingest.status.success(),
        "ingest of a graph containing LintSuppression must succeed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&ingest),
        stderr_of(&ingest)
    );
    assert!(
        stdout_of(&ingest).contains("failed: 0"),
        "ingest must report zero failures\nstdout:\n{}",
        stdout_of(&ingest)
    );

    // Inspect the store: pre-fix this fails with the same read-back error.
    let inspect = egregore()
        .args(["inspect", "--data-dir", store.to_str().unwrap()])
        .output()
        .expect("inspect runs");
    assert!(
        inspect.status.success(),
        "inspect --data-dir must succeed on a store containing LintSuppression\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&inspect),
        stderr_of(&inspect)
    );

    // Query the suppressions lane from the store: the suppression must read
    // back with its lint payload (dead_code) intact.
    let query = egregore()
        .args([
            "query",
            "suppressions",
            "--data-dir",
            store.to_str().unwrap(),
        ])
        .output()
        .expect("query runs");
    assert!(
        query.status.success(),
        "query suppressions --data-dir must succeed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&query),
        stderr_of(&query)
    );
    let out = stdout_of(&query);
    assert!(
        out.contains("dead_code"),
        "suppression payload (dead_code) must survive the embedded round-trip\nstdout:\n{out}"
    );
}
