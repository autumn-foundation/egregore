#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

#[test]
fn scan_and_inspect_work_without_aletheiadb() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");

    Command::cargo_bin("aletheia-codegraph")
        .expect("binary should run")
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");
    assert!(jsonl.contains(r#""kind":"Repository""#));
    assert!(jsonl.contains(r#""kind":"Symbol""#));
    assert!(jsonl.contains(r#""label":"DEFINES""#));

    Command::cargo_bin("aletheia-codegraph")
        .expect("binary should run")
        .arg("inspect")
        .arg(&graph_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("records:"))
        .stdout(predicate::str::contains("nodes:"))
        .stdout(predicate::str::contains("edges:"))
        .stdout(predicate::str::contains("diagnostics:"))
        .stderr(predicate::str::is_empty());
}
