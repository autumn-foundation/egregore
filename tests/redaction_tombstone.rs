#![allow(missing_docs)]

use aletheia_egregore::GraphRecord;
use assert_cmd::Command as CargoCommand;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(path, contents).expect("write file");
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
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(status.status.success());
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_refresh_tombstones_stale_redaction_evidence() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;

    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test User"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    // Write a file with a secret
    write(
        &repo,
        "src/lib.rs",
        "pub const KEY: &str = \"sk-live-5555566666777778888899999000001111122222\";",
    );
    commit(&repo, "initial commit", "2026-06-30T12:00:00Z");

    let scan_out = temp.path().join("scan.graph.jsonl");
    let data_dir = temp.path().join(".egregore");

    // Perform initial scan & ingest
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_out)
        .assert()
        .success();

    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("ingest")
        .arg(&scan_out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Now, rewrite the file to REMOVE the secret
    write(&repo, "src/lib.rs", "pub const KEY: &str = \"safe_value\";");

    // Run refresh (by default, raw_literals is false)
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Verify in database that the redaction evidence node is now tombstoned/removed
    let reopened_again = EmbeddedAletheiaSink::open(&data_dir).expect("open store");
    let active_records = reopened_again
        .read_all_records()
        .expect("read active records");
    let mut found_diag_again = false;
    for record in &active_records {
        if let GraphRecord::Node { name, .. } = record
            && name.as_deref() == Some("redaction_evidence")
        {
            found_diag_again = true;
        }
    }
    assert!(
        !found_diag_again,
        "redaction_evidence diagnostic node must be removed from active view"
    );

    let all_records = reopened_again
        .read_all_records_including_superseded()
        .expect("read all records");

    let mut found_tombstone = false;
    for record in all_records {
        if let GraphRecord::Tombstone { summary, .. } = record
            && summary.contains("redaction evidence")
        {
            found_tombstone = true;
        }
    }
    assert!(
        found_tombstone,
        "must have found redaction_evidence tombstone"
    );
}

/// Regression for Codex C8: when every secret-bearing file is unchanged and the
/// reused cache records already carry the redaction marker, a second default
/// `eg refresh` must NOT tombstone the `redaction_evidence` node — masked
/// literals still exist in the graph, so the audit evidence must survive.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_second_refresh_preserves_evidence_for_unchanged_masked_files() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;

    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test User"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    // Write a file with a secret and commit it.
    write(
        &repo,
        "src/lib.rs",
        "pub const KEY: &str = \"sk-live-5555566666777778888899999000001111122222\";",
    );
    commit(&repo, "initial commit", "2026-06-30T12:00:00Z");

    let scan_out = temp.path().join("scan.graph.jsonl");
    let data_dir = temp.path().join(".egregore");

    // Initial scan & ingest.
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_out)
        .assert()
        .success();

    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("ingest")
        .arg(&scan_out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // First default refresh: rebuilds the file, redacts, and writes a redacted
    // cache plus a live redaction_evidence diagnostic.
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Second default refresh with the file UNCHANGED. The cache is reused and its
    // records are already masked, so no NEW literals are redacted this run. The
    // masked literals still exist, so the evidence must be preserved.
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("open store");
    let active_records = reopened.read_all_records().expect("read active records");

    let mut found_diag = false;
    let mut found_masked_literal = false;
    for record in &active_records {
        if let GraphRecord::Node { name, summary, .. } = record {
            if name.as_deref() == Some("redaction_evidence") {
                found_diag = true;
            }
            if summary.contains("«redacted:secret»") {
                found_masked_literal = true;
            }
        }
    }
    assert!(
        found_masked_literal,
        "store must still contain masked literals after the second refresh"
    );
    assert!(
        found_diag,
        "redaction_evidence diagnostic must stay live while masked literals remain"
    );
}
