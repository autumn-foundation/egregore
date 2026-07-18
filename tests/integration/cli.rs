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
fn version_flag_prints_crate_version_on_both_binaries() {
    let version = env!("CARGO_PKG_VERSION");
    for bin in ["egregore", "eg"] {
        Command::cargo_bin(bin)
            .expect("binary should run")
            .arg("--version")
            .assert()
            .success()
            .stdout(predicate::str::contains(version));
    }
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

#[test]
fn write_observation_missing_agent_id_emits_json_error() {
    // When --agent-id is omitted the CLI must exit non-zero and write a
    // machine-readable JSON envelope to stderr, never a clap usage error.
    let temp = tempfile::tempdir().expect("temp dir");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "write",
            "observation",
            "--session-id",
            "s1",
            "--observed-at",
            "2026-05-30T10:00:00Z",
            "--source-handle",
            "src/lib.rs:sha256:abc",
            "--text",
            "test",
            "--evidence-target",
            "codegraph:v4:abc",
            "--out",
            temp.path().join("out.jsonl").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(r#""code":"missing_field""#))
        .stderr(predicate::str::contains(r#""field":"agent_id""#));
}

#[test]
fn cargo_run_defaults_to_egregore_binary() {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(manifest_path).expect("manifest should be readable");
    let package_section = manifest
        .split("[[bin]]")
        .next()
        .expect("package section should exist before binary declarations");

    assert!(
        package_section.contains(r#"default-run = "egregore""#),
        "Cargo.toml must set default-run so documented `cargo run -- ...` commands select the primary binary"
    );
}
