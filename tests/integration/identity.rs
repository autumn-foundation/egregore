//! Tests for Repository node identity derivation (issue #7).
//!
//! These tests verify that Repository IDs are derived from VCS remote,
//! root commit SHA, or canonical path — not from directory basename.

#![allow(missing_docs)]

use std::{
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{scan_repository, scan_repository_history, scan_repository_with_override};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "test@example.invalid"]);
    git(repo, ["config", "user.name", "Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn git_add_remote(repo: &Path, url: &str) {
    git(repo, ["remote", "add", "origin", url]);
}

fn write_src_lib(repo: &Path) {
    let src = repo.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();
}

fn commit_with_date(repo: &Path, msg: &str, date: &str) {
    git(repo, ["add", "."]);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", msg])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        out.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn find_repository_node(jsonl: &str) -> Value {
    jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("graph should contain a Repository node")
}

fn repo_id_from_jsonl(jsonl: &str) -> String {
    find_repository_node(jsonl)["id"]
        .as_str()
        .expect("Repository node should have an id")
        .to_owned()
}

fn repo_identity_source(jsonl: &str) -> String {
    find_repository_node(jsonl)["repository_identity"]["identity_source"]
        .as_str()
        .expect("Repository node should have repository_identity.identity_source")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Test (a): same remote → same Repository ID regardless of clone path
// ---------------------------------------------------------------------------

#[test]
fn same_remote_different_paths_produces_identical_repository_ids() {
    let temp1 = tempfile::tempdir().expect("temp dir 1");
    let temp2 = tempfile::tempdir().expect("temp dir 2");
    let repo1 = temp1.path();
    let repo2 = temp2.path();

    for repo in [repo1, repo2] {
        git_init(repo);
        git_add_remote(repo, "https://github.com/foo/bar.git");
        write_src_lib(repo);
    }

    let g1 = scan_repository(repo1)
        .expect("repo1 should scan")
        .to_jsonl()
        .expect("repo1 should serialize");
    let g2 = scan_repository(repo2)
        .expect("repo2 should scan")
        .to_jsonl()
        .expect("repo2 should serialize");

    let id1 = repo_id_from_jsonl(&g1);
    let id2 = repo_id_from_jsonl(&g2);

    assert_eq!(
        id1, id2,
        "two clones of the same remote must produce the same Repository node ID"
    );
    assert_eq!(repo_identity_source(&g1), "remote");
}

// ---------------------------------------------------------------------------
// Test (b): same basename, different remotes → different Repository IDs
// ---------------------------------------------------------------------------

#[test]
fn different_remotes_same_basename_produces_different_repository_ids() {
    let temp1 = tempfile::tempdir().expect("temp dir 1");
    let temp2 = tempfile::tempdir().expect("temp dir 2");
    let repo1 = temp1.path().join("egregore");
    let repo2 = temp2.path().join("egregore");
    std::fs::create_dir_all(&repo1).unwrap();
    std::fs::create_dir_all(&repo2).unwrap();

    git_init(&repo1);
    git_add_remote(&repo1, "https://github.com/mark/egregore.git");
    write_src_lib(&repo1);

    git_init(&repo2);
    git_add_remote(&repo2, "https://github.com/other/egregore.git");
    write_src_lib(&repo2);

    let g1 = scan_repository(&repo1)
        .expect("repo1 should scan")
        .to_jsonl()
        .expect("serialize");
    let g2 = scan_repository(&repo2)
        .expect("repo2 should scan")
        .to_jsonl()
        .expect("serialize");

    let id1 = repo_id_from_jsonl(&g1);
    let id2 = repo_id_from_jsonl(&g2);

    assert_ne!(
        id1, id2,
        "same basename but different remotes must produce different Repository node IDs"
    );
}

// ---------------------------------------------------------------------------
// Test (c): no remote, one commit → stable ID; second repo with diff timestamp → diff ID
// ---------------------------------------------------------------------------

#[test]
fn no_remote_commit_produces_stable_id_different_from_distinct_root_commit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();

    git_init(repo);
    write_src_lib(repo);
    commit_with_date(repo, "initial commit", "2026-01-01T00:00:00Z");

    let g1 = scan_repository(repo)
        .expect("first scan")
        .to_jsonl()
        .expect("serialize");
    let g2 = scan_repository(repo)
        .expect("second scan")
        .to_jsonl()
        .expect("serialize");

    let id1 = repo_id_from_jsonl(&g1);
    let id2 = repo_id_from_jsonl(&g2);
    assert_eq!(
        id1, id2,
        "two scans of the same no-remote repo must produce the same ID"
    );
    assert_eq!(repo_identity_source(&g1), "local_root_commit");

    // A second repo with the same commit message but different timestamp → different SHA → different ID
    let temp2 = tempfile::tempdir().expect("temp dir 2");
    let repo2 = temp2.path();
    git_init(repo2);
    write_src_lib(repo2);
    commit_with_date(repo2, "initial commit", "2026-06-15T12:00:00Z");

    let g3 = scan_repository(repo2)
        .expect("repo2 scan")
        .to_jsonl()
        .expect("serialize");
    let id3 = repo_id_from_jsonl(&g3);

    assert_ne!(
        id1, id3,
        "repos with different root commit SHAs must produce different Repository node IDs"
    );
}

// ---------------------------------------------------------------------------
// Test (d): plain directory with no .git → local_path identity
// ---------------------------------------------------------------------------

#[test]
fn no_git_produces_local_path_identity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();
    write_src_lib(repo);

    let jsonl = scan_repository(repo)
        .expect("plain dir should scan")
        .to_jsonl()
        .expect("serialize");

    assert_eq!(
        repo_identity_source(&jsonl),
        "local_path",
        "a plain directory without .git should produce local_path identity"
    );

    let node = find_repository_node(&jsonl);
    assert!(
        node["repository_identity"]["canonical_path"].is_string(),
        "local_path identity must carry canonical_path"
    );
    assert!(
        node["repository_identity"].get("remote_url").is_none()
            || node["repository_identity"]["remote_url"].is_null(),
        "local_path identity must not carry remote_url"
    );
}

// ---------------------------------------------------------------------------
// Test (e): scan and scan-history of the same repo produce the same Repository ID
// ---------------------------------------------------------------------------

#[test]
fn scan_and_scan_history_produce_same_repository_id() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();

    git_init(repo);
    git_add_remote(repo, "https://github.com/test/identity-e-fixture.git");
    write_src_lib(repo);
    commit_with_date(repo, "initial commit", "2026-01-01T00:00:00Z");

    let scan_jsonl = scan_repository(repo)
        .expect("scan should succeed")
        .to_jsonl()
        .expect("serialize");
    let history_jsonl = scan_repository_history(repo)
        .expect("scan-history should succeed")
        .to_jsonl()
        .expect("serialize");

    let scan_id = repo_id_from_jsonl(&scan_jsonl);
    let history_id = repo_id_from_jsonl(&history_jsonl);

    assert_eq!(
        scan_id, history_id,
        "scan and scan-history must produce the same Repository node ID"
    );
}

// ---------------------------------------------------------------------------
// Test (f): --repo-id-override produces operator_override identity
// ---------------------------------------------------------------------------

#[test]
fn repo_id_override_produces_operator_override_identity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path();
    write_src_lib(repo);

    let g1 = scan_repository_with_override(repo, Some("my-custom-id"))
        .expect("override scan should succeed")
        .to_jsonl()
        .expect("serialize");

    assert_eq!(repo_identity_source(&g1), "operator_override");

    // Two scans with the same override produce byte-identical Repository IDs.
    let g2 = scan_repository_with_override(repo, Some("my-custom-id"))
        .expect("second override scan")
        .to_jsonl()
        .expect("serialize");
    assert_eq!(
        repo_id_from_jsonl(&g1),
        repo_id_from_jsonl(&g2),
        "same override string must produce same Repository ID"
    );

    // Different override string → different ID.
    let g3 = scan_repository_with_override(repo, Some("other-id"))
        .expect("different override scan")
        .to_jsonl()
        .expect("serialize");
    assert_ne!(
        repo_id_from_jsonl(&g1),
        repo_id_from_jsonl(&g3),
        "different override strings must produce different Repository IDs"
    );
}
