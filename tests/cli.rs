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

    Command::cargo_bin("egregore")
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

    Command::cargo_bin("egregore")
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

#[test]
fn eg_alias_runs_cli() {
    Command::cargo_bin("eg")
        .expect("short alias binary should run")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Manage agentic SWE knowledge graphs",
        ));
}
