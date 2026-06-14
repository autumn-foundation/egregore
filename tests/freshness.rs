//! Integration tests for store source-snapshot stamping and freshness reporting
//! (issue #82).
//!
//! Covers the full fixture matrix — `fresh`, `stale_head`, `stale_dirty`, and
//! `unknown` (non-Git directory and pre-stamping store) — plus the read-only
//! guarantee, determinism of the snapshot's deterministic portion, and the
//! non-fatal freshness field surfaced by the structural query commands.
//!
//! Store artifacts (`graph.jsonl`, `.egregore/`) are written to a separate `work`
//! directory, never into the scanned working tree, so the tree's clean/dirty
//! state reflects only the test's own edits.

#![allow(missing_docs)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Git + scan helpers
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

fn write_lib(repo: &Path, body: &str) {
    let src = repo.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), body).unwrap();
}

fn commit_all(repo: &Path, msg: &str) {
    git(repo, ["add", "."]);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", msg])
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        out.status.success(),
        "git commit failed\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn eg() -> AssertCommand {
    AssertCommand::cargo_bin("egregore").expect("egregore binary should build")
}

/// A committed Git working tree plus a separate `work` dir for store artifacts.
struct Fixture {
    repo: TempDir,
    work: TempDir,
}

impl Fixture {
    /// Fresh Git repo with one committed `src/lib.rs`.
    fn committed() -> Self {
        let repo = tempfile::tempdir().unwrap();
        git_init(repo.path());
        write_lib(repo.path(), "pub fn hello() {}\n");
        commit_all(repo.path(), "initial");
        Self {
            repo,
            work: tempfile::tempdir().unwrap(),
        }
    }

    /// Plain (non-Git) directory with one `src/lib.rs`.
    fn non_git() -> Self {
        let repo = tempfile::tempdir().unwrap();
        write_lib(repo.path(), "pub fn hello() {}\n");
        Self {
            repo,
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn repo(&self) -> &Path {
        self.repo.path()
    }

    fn graph(&self) -> PathBuf {
        self.work.path().join("graph.jsonl")
    }

    fn data_dir(&self) -> PathBuf {
        self.work.path().join(".egregore")
    }

    /// `eg scan <repo> --out work/graph.jsonl`.
    fn scan(&self) {
        eg().args(["scan"])
            .arg(self.repo())
            .arg("--out")
            .arg(self.graph())
            .assert()
            .success();
    }

    /// `eg freshness <repo> --graph work/graph.jsonl --format json`.
    fn freshness_graph(&self) -> Value {
        let out = eg()
            .args(["freshness"])
            .arg(self.repo())
            .arg("--graph")
            .arg(self.graph())
            .args(["--format", "json"])
            .assert()
            .success();
        serde_json::from_slice(&out.get_output().stdout).expect("freshness JSON should parse")
    }
}

// ---------------------------------------------------------------------------
// AC1 + AC6: scan stamps a deterministic source-snapshot identity
// ---------------------------------------------------------------------------

#[test]
fn scan_stamps_source_snapshot_with_head_and_dirty() {
    let fx = Fixture::committed();
    fx.scan();

    let jsonl = std::fs::read_to_string(fx.graph()).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node");

    let snapshot = &repo_node["source_snapshot"];
    assert_eq!(snapshot["head"]["state"], "commit");
    assert!(
        snapshot["head"]["sha"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "commit SHA should be stamped: {snapshot}"
    );
    assert_eq!(snapshot["dirty"], Value::Bool(false));
    assert!(snapshot["repository_id"].as_str().is_some());
    assert!(snapshot["scanned_at"].as_str().is_some());
}

#[test]
fn snapshot_deterministic_portion_is_reproducible() {
    use aletheia_egregore::scan_repository_at_with_override;

    let fx = Fixture::committed();
    // Two scans of an unchanged clean tree at a fixed commit must produce
    // byte-identical JSONL (deterministic head + dirty, fixed scanned_at) — AC6.
    let first =
        scan_repository_at_with_override(fx.repo(), "2026-05-19T00:00:00Z", Some("fixture"))
            .unwrap()
            .to_jsonl()
            .unwrap();
    let second =
        scan_repository_at_with_override(fx.repo(), "2026-05-19T00:00:00Z", Some("fixture"))
            .unwrap()
            .to_jsonl()
            .unwrap();
    assert_eq!(
        first, second,
        "snapshot stamping must not break determinism"
    );
    assert!(first.contains(r#""source_snapshot""#));
}

// ---------------------------------------------------------------------------
// AC3 + AC7: the freshness classification matrix
// ---------------------------------------------------------------------------

#[test]
fn fresh_when_clean_tree_at_head() {
    let fx = Fixture::committed();
    fx.scan();

    let report = fx.freshness_graph();
    assert_eq!(report["freshness"], "fresh");
    assert_eq!(report["fresh"], Value::Bool(true));
}

#[test]
fn stale_head_when_head_moves() {
    let fx = Fixture::committed();
    fx.scan();

    // Commit a change so HEAD moves past the stored snapshot. Tree stays clean.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn added() {}\n");
    commit_all(fx.repo(), "second");

    let report = fx.freshness_graph();
    assert_eq!(report["freshness"], "stale_head");
    assert_eq!(report["fresh"], Value::Bool(false));
}

#[test]
fn stale_dirty_when_uncommitted_edit() {
    let fx = Fixture::committed();
    fx.scan();

    // Uncommitted edit: HEAD unchanged, tree dirty.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn scratch() {}\n");

    let report = fx.freshness_graph();
    assert_eq!(report["freshness"], "stale_dirty");
    assert_eq!(report["fresh"], Value::Bool(false));
}

#[test]
fn unknown_for_non_git_directory() {
    let fx = Fixture::non_git();
    fx.scan();

    let report = fx.freshness_graph();
    assert_eq!(report["freshness"], "unknown");
    assert_eq!(report["fresh"], Value::Bool(false));
}

#[test]
fn unknown_for_pre_stamping_store() {
    // A graph JSONL whose Repository node carries no `source_snapshot`, as
    // produced before snapshot stamping existed.
    let fx = Fixture::committed();
    let legacy = format!(
        "{}\n",
        serde_json::json!({
            "record_type": "node",
            "id": "codegraph:v3:legacy",
            "kind": "Repository",
            "schema_version": 4,
            "summary": "Repository legacy",
            "repository_identity": {
                "identity_source": "operator_override",
                "basename": "legacy"
            }
        })
    );
    std::fs::write(fx.graph(), legacy).unwrap();

    // A single-repo store with no stamped snapshot classifies as unknown.
    let report = fx.freshness_graph();
    assert_eq!(report["freshness"], "unknown");
}

// ---------------------------------------------------------------------------
// AC4: the freshness check is strictly read-only
// ---------------------------------------------------------------------------

/// Recursively snapshots `(relative path, len, modified)` for every file under
/// `root`, used to prove a command made no on-disk changes.
fn dir_fingerprint(root: &Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
    let mut map = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                map.insert(rel, (meta.len(), meta.modified().unwrap()));
            }
        }
    }
    map
}

#[test]
fn freshness_is_read_only_for_graph() {
    let fx = Fixture::committed();
    fx.scan();

    let before = dir_fingerprint(fx.work.path());
    let report = fx.freshness_graph();
    assert_eq!(report["freshness"], "fresh");
    let after = dir_fingerprint(fx.work.path());

    assert_eq!(
        before, after,
        "freshness check must not create, modify, or delete any file"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn freshness_is_read_only_for_data_dir() {
    let fx = Fixture::committed();
    fx.scan();

    let data_dir = fx.data_dir();
    eg().args(["ingest"])
        .arg(fx.graph())
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let before = dir_fingerprint(&data_dir);
    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value =
        serde_json::from_slice(&out.get_output().stdout).expect("freshness JSON should parse");
    assert_eq!(report["freshness"], "fresh");

    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "freshness check must not touch any store file, index, or receipt"
    );
}

// ---------------------------------------------------------------------------
// AC5: structural query commands surface the freshness state
// ---------------------------------------------------------------------------

#[test]
fn query_symbol_surfaces_freshness_when_stale() {
    let fx = Fixture::committed();
    fx.scan();

    // Make the tree dirty so the store is stale.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn scratch() {}\n");

    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(fx.graph())
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--format", "json"])
        .assert()
        .success();
    let line = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(row["freshness"], "stale_dirty");
    // The result itself is never suppressed.
    assert_eq!(row["name"], "hello");
    assert!(row["repo_relative_path"].as_str().is_some());
}

#[test]
fn query_symbol_omits_freshness_without_repo_path() {
    let fx = Fixture::committed();
    fx.scan();

    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(fx.graph())
        .args(["--format", "json"])
        .assert()
        .success();
    let line = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert!(
        row.get("freshness").is_none(),
        "freshness must be absent without --repo-path (back-compat): {row}"
    );
}

#[test]
fn query_context_surfaces_freshness_when_stale() {
    let fx = Fixture::committed();
    fx.scan();

    write_lib(fx.repo(), "pub fn hello() {}\npub fn scratch() {}\n");

    let out = eg()
        .args(["query", "context", "hello"])
        .arg("--graph")
        .arg(fx.graph())
        .arg("--repo-path")
        .arg(fx.repo())
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(report["freshness"], "stale_dirty");
    assert_eq!(report["ok"], Value::Bool(true));
}
