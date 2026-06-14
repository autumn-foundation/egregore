//! Integration tests for `eg doctor` — local setup preflight report (issue #75).
//!
//! These tests exercise the full CLI binary via `assert_cmd`. They are
//! intentionally feature-independent (no `#[cfg(feature = ...)]` gates) so
//! they pass under `--no-default-features` and `--all-features`.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn git(repo: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git should execute");
    assert!(status.success(), "git {args:?} failed");
}

fn init_git_repo(dir: &std::path::Path) {
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "test@example.invalid"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    // Create one commit so history is readable.
    fs::write(dir.join("README.md"), "# test").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

fn eg() -> Command {
    Command::cargo_bin("egregore").unwrap()
}

// ── Structural happy path ─────────────────────────────────────────────────────

#[test]
fn doctor_git_repo_exits_0_structural_ready() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    let out = tmp.path().join("graph.jsonl");
    let data_dir = tmp.path().join(".egregore");

    let output = eg()
        .args([
            "doctor",
            tmp.path().to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("eg doctor should execute");

    assert!(
        output.status.success(),
        "doctor on a git repo should exit 0; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");

    assert_eq!(json["schema_version"], 1, "schema_version must be 1");
    assert_eq!(
        json["structural_ready"], true,
        "structural_ready must be true for a git repo with writable paths"
    );
    assert_eq!(
        json["overall_ready"], json["structural_ready"],
        "overall_ready must equal structural_ready"
    );

    // checks is a sorted array
    let checks = json["checks"].as_array().expect("checks must be an array");
    assert!(!checks.is_empty(), "checks must be non-empty");

    // Each check has required fields
    for check in checks {
        assert!(check["id"].is_string(), "every check must have an id");
        assert!(
            check["status"].is_string(),
            "every check must have a status"
        );
        assert!(check["requirement"].is_string());
        assert!(check["gate"].is_string());
        assert!(check["summary"].is_string());
    }
}

// ── --require-history failure path ────────────────────────────────────────────

#[test]
fn doctor_non_git_require_history_exits_1() {
    let tmp = TempDir::new().unwrap();
    // Deliberately NOT a git repo.

    let out = tmp.path().join("graph.jsonl");
    let data_dir = tmp.path().join(".egregore");

    let output = eg()
        .args([
            "doctor",
            tmp.path().to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--require-history",
            "--format",
            "json",
        ])
        .output()
        .expect("eg doctor should execute");

    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor with --require-history on a non-git dir should exit 1"
    );

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("stdout should still be valid JSON even on failure");

    assert_eq!(json["structural_ready"], false);
    assert_eq!(json["overall_ready"], false);

    // Find the git_history_readable check
    let history_check = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "git_history_readable")
        .expect("git_history_readable check must appear");

    assert_eq!(history_check["status"], "fail");
    assert_eq!(
        history_check["requirement"], "required",
        "with --require-history the check must be required"
    );
}

// ── --format text ─────────────────────────────────────────────────────────────

#[test]
fn doctor_text_format_shows_glyphs() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    eg().args(["doctor", tmp.path().to_str().unwrap(), "--format", "text"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ok]").or(predicate::str::contains("[--]")));
}

#[test]
fn doctor_text_format_fail_shows_fail_glyph() {
    let tmp = TempDir::new().unwrap();
    // NOT a git repo + --require-history → [fail]

    eg().args([
        "doctor",
        tmp.path().to_str().unwrap(),
        "--require-history",
        "--format",
        "text",
    ])
    .assert()
    .failure()
    .stdout(predicate::str::contains("[fail]"));
}

// ── No --network → no hf_reachable check ─────────────────────────────────────

#[test]
fn doctor_without_network_flag_omits_hf_reachable() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    let output = eg()
        .args(["doctor", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .expect("eg doctor should execute");

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let has_hf_reachable = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == "hf_reachable");

    assert!(
        !has_hf_reachable,
        "hf_reachable check must not appear without --network flag"
    );
}

// ── Default invocation (no args) ──────────────────────────────────────────────

#[test]
fn doctor_default_invocation_produces_json() {
    // Run from the egregore repo itself — which IS a git repo.
    let output = eg()
        .arg("doctor")
        .output()
        .expect("eg doctor should execute without args");

    // Should produce valid JSON regardless of exit code.
    let _json: Value = serde_json::from_slice(&output.stdout)
        .expect("default invocation should produce valid JSON on stdout");
}

// ── Secret safety ─────────────────────────────────────────────────────────────

#[test]
fn doctor_output_contains_no_secret_markers() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    let output = eg()
        .args(["doctor", tmp.path().to_str().unwrap(), "--format", "json"])
        // Set fake HF token env vars — must NOT appear in output.
        .env("HF_TOKEN", "hf_INTEGRATION_FAKE_TOKEN_abc123")
        .env("HUGGING_FACE_HUB_TOKEN", "hf_INTEGRATION_FAKE_TOKEN_xyz456")
        .output()
        .expect("eg doctor should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("hf_INTEGRATION_FAKE_TOKEN_abc123"),
        "HF_TOKEN value must not appear in doctor output"
    );
    assert!(
        !stdout.contains("hf_INTEGRATION_FAKE_TOKEN_xyz456"),
        "HUGGING_FACE_HUB_TOKEN value must not appear in doctor output"
    );
}

// ── JSON shape stability ──────────────────────────────────────────────────────

#[test]
fn doctor_json_shape_stable_across_five_runs() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    let out = tmp.path().join("graph.jsonl");
    let data_dir = tmp.path().join(".egregore");

    let first_output = eg()
        .args([
            "doctor",
            tmp.path().to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("eg doctor should execute");
    let first_json: Value = serde_json::from_slice(&first_output.stdout).unwrap();

    for _ in 1..5 {
        let output = eg()
            .args([
                "doctor",
                tmp.path().to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--format",
                "json",
            ])
            .output()
            .expect("eg doctor should execute");

        let json: Value = serde_json::from_slice(&output.stdout).unwrap();

        // Compare the stable shape: schema_version, readiness flags, check IDs and statuses.
        assert_eq!(
            first_json["schema_version"], json["schema_version"],
            "schema_version must be stable"
        );
        assert_eq!(
            first_json["structural_ready"], json["structural_ready"],
            "structural_ready must be stable"
        );
        assert_eq!(
            first_json["semantic_ready"], json["semantic_ready"],
            "semantic_ready must be stable"
        );

        let first_ids: Vec<&str> = first_json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        let ids: Vec<&str> = json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(first_ids, ids, "check order must be stable across runs");

        let first_statuses: Vec<&str> = first_json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["status"].as_str().unwrap())
            .collect();
        let statuses: Vec<&str> = json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["status"].as_str().unwrap())
            .collect();
        assert_eq!(
            first_statuses, statuses,
            "check statuses must be stable across runs"
        );
    }
}

// ── Repository path must be a directory ───────────────────────────────────────

#[test]
fn doctor_file_path_exits_1_not_a_directory() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("lib.rs");
    fs::write(&file, "fn main() {}").unwrap();

    let output = eg()
        .args(["doctor", file.to_str().unwrap(), "--format", "json"])
        .output()
        .expect("eg doctor should execute");

    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor on a file path should exit 1 (scan rejects non-directories)"
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["structural_ready"], false);

    let repo_check = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "repository_path")
        .expect("repository_path check must appear");
    assert_eq!(repo_check["status"], "fail");
    assert_eq!(repo_check["requirement"], "required");
}

// ── Existing unwritable --out target ──────────────────────────────────────────

#[test]
fn doctor_out_path_is_existing_directory_exits_1() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    // --out points at an existing directory: scan's fs::write(out) would fail.
    let out_dir = tmp.path().join("out-is-a-dir");
    fs::create_dir(&out_dir).unwrap();

    let output = eg()
        .args([
            "doctor",
            tmp.path().to_str().unwrap(),
            "--out",
            out_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("eg doctor should execute");

    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor with --out pointing at a directory should exit 1"
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let out_check = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "output_path_writable")
        .expect("output_path_writable check must appear");
    assert_eq!(out_check["status"], "fail");
    assert_eq!(out_check["requirement"], "required");
}

// ── Model cache: bare slug dir without snapshots is not "present" ─────────────

#[test]
fn doctor_empty_model_slug_dir_is_not_ready() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    // Simulate an interrupted priming run: top-level slug dir exists but has no
    // snapshots/ contents. Point HF_HUB_CACHE at it.
    let hf_cache = tmp.path().join("hf-cache");
    let slug_dir = hf_cache.join("models--sentence-transformers--all-MiniLM-L6-v2");
    fs::create_dir_all(&slug_dir).unwrap();

    let output = eg()
        .args(["doctor", tmp.path().to_str().unwrap(), "--format", "json"])
        .env("HF_HUB_CACHE", hf_cache.to_str().unwrap())
        .output()
        .expect("eg doctor should execute");

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["semantic_ready"], false,
        "an empty slug dir without snapshots must not be semantic-ready"
    );
    let model_check = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "model_cache_present")
        .expect("model_cache_present check must appear");
    assert_eq!(model_check["status"], "fail");
}

#[test]
fn doctor_model_snapshot_present_is_detected() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());

    // A populated snapshots/ dir signals a usable cache.
    let hf_cache = tmp.path().join("hf-cache");
    let snapshot = hf_cache
        .join("models--sentence-transformers--all-MiniLM-L6-v2")
        .join("snapshots")
        .join("abc123");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("config.json"), "{}").unwrap();

    let output = eg()
        .args(["doctor", tmp.path().to_str().unwrap(), "--format", "json"])
        .env("HF_HUB_CACHE", hf_cache.to_str().unwrap())
        .output()
        .expect("eg doctor should execute");

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let model_check = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "model_cache_present")
        .expect("model_cache_present check must appear");
    assert_eq!(
        model_check["status"], "pass",
        "a populated snapshots/ dir must be detected as present"
    );
}
