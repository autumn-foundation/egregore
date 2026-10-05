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

// ── Conditional-compilation gates (issue #190) ────────────────────────────

/// Seeds a scratch repo exercising the acceptance shapes: a directly-gated
/// fn, a symbol inheriting its enclosing module's gate, and an ungated fn.
fn seed_cfg_repo() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let src = temp.path().join("src");
    fs::create_dir(&src).expect("src dir should be created");
    fs::write(
        src.join("lib.rs"),
        r#"#[cfg(feature = "gated-fn")]
pub fn gated_fn() {}

#[cfg(feature = "outer")]
pub mod gated_mod {
    pub fn nested_fn() {}
}

pub fn plain_fn() {}
"#,
    )
    .expect("fixture source should be written");
    let graph_path = temp.path().join("graph.jsonl");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(temp.path())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();
    (temp, graph_path)
}

#[test]
fn query_symbol_json_and_text_expose_cfg() {
    let (_temp, graph_path) = seed_cfg_repo();
    let graph = graph_path.to_str().expect("graph path should be UTF-8");

    // JSON: the gated fn carries its verbatim predicate; the ungated fn
    // carries no `cfg` key at all.
    let out = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "symbol", "gated_fn", "--graph", graph])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&out).expect("query symbol should emit JSON");
    assert_eq!(
        json["cfg"],
        serde_json::json!(["feature = \"gated-fn\""]),
        "directly gated fn carries its verbatim predicate"
    );

    let out = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "symbol", "plain_fn", "--graph", graph])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&out).expect("query symbol should emit JSON");
    assert!(
        json.get("cfg").is_none(),
        "ungated fn carries no cfg key: {json}"
    );

    // Text: the gate renders on its own line; absence prints nothing.
    let out = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args([
            "query", "symbol", "gated_fn", "--graph", graph, "--format", "text",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("text output should be UTF-8");
    assert!(
        text.contains("cfg: feature = \"gated-fn\""),
        "text render exposes the gate: {text}"
    );
}

#[test]
fn query_file_rows_carry_inherited_cfg() {
    let (_temp, graph_path) = seed_cfg_repo();
    let graph = graph_path.to_str().expect("graph path should be UTF-8");

    let out = Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "file", "src/lib.rs", "--graph", graph])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("output should be UTF-8");
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each row should be JSON"))
        .collect();
    let nested = rows
        .iter()
        .find(|row| row["name"] == "gated_mod::nested_fn")
        .expect("nested fn should be listed");
    assert_eq!(
        nested["cfg"],
        serde_json::json!(["feature = \"outer\""]),
        "file rows inherit the enclosing module gate"
    );
    let plain = rows
        .iter()
        .find(|row| row["name"] == "plain_fn")
        .expect("plain fn should be listed");
    assert!(
        plain.get("cfg").is_none(),
        "ungated file rows carry no cfg key"
    );
}
