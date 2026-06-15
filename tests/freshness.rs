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
        Self::committed_with("pub fn hello() {}\n")
    }

    /// Fresh Git repo with `body` as the committed `src/lib.rs`. Distinct bodies
    /// produce distinct root commits, hence distinct repository identities.
    fn committed_with(body: &str) -> Self {
        let repo = tempfile::tempdir().unwrap();
        git_init(repo.path());
        write_lib(repo.path(), body);
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

// ---------------------------------------------------------------------------
// Codex review follow-ups (issue #82)
// ---------------------------------------------------------------------------

/// In a multi-repository store, `--repo-path` freshness must stamp only rows
/// belonging to that checkout's repository — never mislabel another repo's rows.
#[test]
fn query_freshness_only_stamps_matching_repository() {
    // Two independent git repos, each defining `hello`, scanned into one graph.
    // Their initial commits differ (distinct content) so they get distinct
    // repository identities rather than colliding on an identical root commit.
    let a = Fixture::committed();
    let b = Fixture::committed_with("pub fn hello() {}\npub fn b_only() {}\n");
    a.scan();
    b.scan();

    let combined = a.work.path().join("combined.jsonl");
    let mut bytes = std::fs::read(a.graph()).unwrap();
    bytes.extend_from_slice(&std::fs::read(b.graph()).unwrap());
    std::fs::write(&combined, bytes).unwrap();

    // Compare against repo A's (clean) working tree.
    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(&combined)
        .arg("--repo-path")
        .arg(a.repo())
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let rows: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 2, "both repos define `hello`: {stdout}");

    let stamped: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("freshness").is_some())
        .collect();
    assert_eq!(
        stamped.len(),
        1,
        "exactly one repo's row should carry freshness: {stdout}"
    );
    assert_eq!(stamped[0]["freshness"], "fresh");
    // The stamped row must belong to repo A, not the other repository.
    let other = rows.iter().find(|r| r.get("freshness").is_none()).unwrap();
    assert_ne!(
        stamped[0]["repository_id"], other["repository_id"],
        "the two rows must be attributed to different repositories"
    );
}

/// `git status` must not write `.git/index` during a freshness probe
/// (`GIT_OPTIONAL_LOCKS=0`), even when a tracked file's mtime changed.
#[test]
fn freshness_does_not_write_git_index() {
    let fx = Fixture::committed();
    fx.scan();

    let index_path = fx.repo().join(".git").join("index");
    // Touch a tracked file so a default `git status` would refresh + rewrite the
    // index stat cache; with GIT_OPTIONAL_LOCKS=0 it must not.
    let lib = fx.repo().join("src").join("lib.rs");
    let contents = std::fs::read(&lib).unwrap();
    std::fs::write(&lib, &contents).unwrap();

    let before = std::fs::read(&index_path).unwrap();
    let report = fx.freshness_graph();
    // HEAD unchanged, no content change → fresh.
    assert_eq!(report["freshness"], "fresh");
    let after = std::fs::read(&index_path).unwrap();
    assert_eq!(before, after, "freshness probe must not rewrite .git/index");
}

/// After refreshing a stale embedded store, the re-stamped snapshot must make
/// `eg freshness --data-dir` report `fresh` (not `unknown`).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_restamps_snapshot_so_store_is_fresh() {
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

    // Move HEAD: the store is now stale_head.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn added() {}\n");
    commit_all(fx.repo(), "second");
    let stale = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let stale: Value = serde_json::from_slice(&stale.get_output().stdout).unwrap();
    assert_eq!(stale["freshness"], "stale_head");

    // Refresh re-stamps the Repository node with the current snapshot.
    eg().args(["refresh"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let fresh = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let fresh: Value = serde_json::from_slice(&fresh.get_output().stdout).unwrap();
    assert_eq!(
        fresh["freshness"], "fresh",
        "refresh must re-stamp the snapshot so the store reports fresh"
    );
}

/// The in-tree store artifact (`--graph` written under the repo) must not by
/// itself make the working tree look `stale_dirty` (PR #186 #4).
#[test]
fn freshness_excludes_in_tree_graph_artifact() {
    let fx = Fixture::committed();
    // Scan the store *into* the working tree (the documented `eg scan . --out
    // graph.jsonl` shape), where graph.jsonl is an untracked file.
    let in_tree_graph = fx.repo().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_graph)
        .assert()
        .success();
    // Sanity: the untracked artifact really is present in the tree.
    assert!(in_tree_graph.exists());

    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--graph")
        .arg(&in_tree_graph)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness"], "fresh",
        "the checked store artifact must be excluded from the dirty probe"
    );

    // A *source* edit is still detected as stale_dirty (exclusion is artifact-only).
    write_lib(fx.repo(), "pub fn hello() {}\npub fn edited() {}\n");
    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--graph")
        .arg(&in_tree_graph)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(report["freshness"], "stale_dirty");
}

/// The scanner must skip git-ignored Rust files so they never enter the graph
/// (PR #186 #7), keeping the indexed set aligned with the read-only dirty probe.
#[test]
fn scanner_skips_gitignored_rust_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git_init(repo);
    write_lib(repo, "pub fn tracked_fn() {}\n");
    // A generated, git-ignored Rust file.
    let gen_dir = repo.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(gen_dir.join("generated.rs"), "pub fn ignored_fn() {}\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/gen/\n").unwrap();
    commit_all(repo, "initial");

    let work = tempfile::tempdir().unwrap();
    let graph = work.path().join("graph.jsonl");
    eg().args(["scan"])
        .arg(repo)
        .arg("--out")
        .arg(&graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&graph).unwrap();
    assert!(
        jsonl.contains("tracked_fn"),
        "tracked source must be indexed"
    );
    assert!(
        !jsonl.contains("ignored_fn"),
        "git-ignored source must not be indexed: {jsonl}"
    );
}

/// The gitignore filter must be gated to the repository root: scanning an in-repo
/// sub-directory stays filesystem-local and must NOT honor a parent `.gitignore`
/// (PR #186, follow-up review).
#[test]
fn gitignore_filter_gated_to_repo_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git_init(root);
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    let sub_src = root.join("crate_a").join("src");
    std::fs::create_dir_all(&sub_src).unwrap();
    std::fs::write(sub_src.join("ignored.rs"), "pub fn ignored_fn() {}\n").unwrap();
    std::fs::write(sub_src.join("keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    commit_all(root, "initial");

    let work = tempfile::tempdir().unwrap();
    let graph = work.path().join("graph.jsonl");
    // Scan the sub-directory, not the repository root.
    eg().args(["scan"])
        .arg(root.join("crate_a"))
        .arg("--out")
        .arg(&graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&graph).unwrap();
    assert!(jsonl.contains("keep_fn"));
    assert!(
        jsonl.contains("ignored_fn"),
        "a sub-directory scan must ignore the parent .gitignore (filesystem-local): {jsonl}"
    );
}

/// X1: when `--repo` selects an operator-override repository whose ID differs
/// from the auto-detected identity, the hint must take priority so the correct
/// stored snapshot is returned in a multi-repo store.
///
/// Without hint-first ordering the identity-first lookup would find the
/// auto-detected-identity snapshot (which is `stale_head` after a new commit) and
/// return `stale_head` even though the override-ID scan is fresh.
#[test]
fn freshness_hint_takes_priority_over_identity_in_multi_repo_store() {
    let fx = Fixture::committed();

    // Scan 1: auto-detected identity at initial commit.
    let scan1 = fx.work.path().join("scan1.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&scan1)
        .assert()
        .success();

    // Advance HEAD so the auto-detected-identity snapshot becomes stale_head.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn extra() {}\n");
    commit_all(fx.repo(), "second");

    // Scan 2: override ID at the new HEAD — this snapshot is fresh.
    let scan2 = fx.work.path().join("scan2.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&scan2)
        .args(["--repo-id-override", "my-override-repo"])
        .assert()
        .success();

    // Combined store: two Repository nodes — one identity-ID (stale_head) and
    // one override-ID (fresh).
    let combined = fx.work.path().join("combined.jsonl");
    let mut bytes = std::fs::read(&scan1).unwrap();
    bytes.extend_from_slice(&std::fs::read(&scan2).unwrap());
    std::fs::write(&combined, bytes).unwrap();

    // Query scoped to the override repo with --repo-path: hint-first lookup
    // must find the override snapshot (fresh), not the identity snapshot (stale_head).
    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(&combined)
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--repo", "my-override-repo"])
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(
        row["freshness"], "fresh",
        "hint-first lookup must find the override snapshot (fresh), not the identity snapshot (stale_head): {row}"
    );
}

/// Rows owned by a `--repo-id-override` repository must still receive a freshness
/// verdict when `--repo-path` is supplied (PR #186, follow-up review): the
/// single-repository fallback's owner ID, not the recomputed identity, is stamped.
#[test]
fn query_freshness_stamps_override_repo_rows() {
    let fx = Fixture::committed();
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(fx.graph())
        .args(["--repo-id-override", "my-fixture-repo"])
        .assert()
        .success();

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
    assert_eq!(
        row["freshness"], "fresh",
        "override-stamped rows must still receive a freshness verdict: {row}"
    );
}

/// `query context` must omit the top-level freshness verdict when source facts
/// span multiple repositories (PR #186, follow-up review).
#[test]
fn query_context_omits_freshness_across_repositories() {
    let a = Fixture::committed();
    let b = Fixture::committed_with("pub fn hello() {}\npub fn b_only() {}\n");
    a.scan();
    b.scan();

    let combined = a.work.path().join("combined.jsonl");
    let mut bytes = std::fs::read(a.graph()).unwrap();
    bytes.extend_from_slice(&std::fs::read(b.graph()).unwrap());
    std::fs::write(&combined, bytes).unwrap();

    let out = eg()
        .args(["query", "context", "hello"])
        .arg("--graph")
        .arg(&combined)
        .arg("--repo-path")
        .arg(a.repo())
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(report["ok"], Value::Bool(true));
    assert!(
        report.get("freshness").is_none(),
        "context freshness must be omitted when facts span repositories: {report}"
    );
}

/// E: scanning into the working tree a second time while the previous
/// `graph.jsonl` is still untracked must not stamp `dirty = true` on the new
/// output — the snapshot excludes its own output file (PR #186 follow-up A/E/F).
#[test]
fn repeated_in_tree_scan_does_not_stamp_dirty() {
    let fx = Fixture::committed();
    let in_tree_graph = fx.repo().join("graph.jsonl");

    // First scan — creates the untracked artifact.
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_graph)
        .assert()
        .success();
    assert!(in_tree_graph.exists());

    // Second scan — artifact already present as untracked, must still produce fresh.
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_graph)
        .assert()
        .success();

    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--graph")
        .arg(&in_tree_graph)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness"], "fresh",
        "repeated in-tree scan must not stamp dirty=true due to its own previous output: {report}"
    );
}

/// F: after `eg scan . --out graph.jsonl && eg ingest graph.jsonl --data-dir .egregore`
/// `eg freshness --data-dir` must report `fresh`. The documented workflow expects the
/// project's `.gitignore` to cover `*.jsonl` so `git status` never sees the
/// intermediate graph file (the egregore repo ships this rule; the fixture below
/// mirrors it). The data-dir exclusion ensures `.egregore` itself is also unseen.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn in_tree_graph_not_counted_when_checking_data_dir() {
    let fx = Fixture::committed();
    // graph.jsonl lives inside the repository; gitignore it so git status ignores it,
    // mirroring the documented `*.jsonl` rule in egregore's own .gitignore.
    let in_tree_graph = fx.repo().join("graph.jsonl");
    let data_dir = fx.repo().join(".egregore");
    std::fs::write(fx.repo().join(".gitignore"), "*.jsonl\n.egregore*/\n").unwrap();
    commit_all(fx.repo(), "add gitignore");

    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_graph)
        .assert()
        .success();

    eg().args(["ingest"])
        .arg(&in_tree_graph)
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Both graph.jsonl and .egregore are gitignored; freshness must report `fresh`.
    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness"], "fresh",
        "gitignored store artifacts must not make the data-dir read stale_dirty: {report}"
    );
}

/// A: `eg refresh` on a tree with uncommitted edits must not stamp `dirty = true`
/// for the `.egregore` data-dir itself, so a follow-up `eg freshness --data-dir`
/// distinguishes "real source edit" from "store artifact is untracked"
/// (PR #186 follow-up A).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_excludes_in_tree_data_dir_from_dirty_probe() {
    let fx = Fixture::committed();
    fx.scan();

    // Ingest into the repository (in-tree store, like the documented workflow).
    let in_tree_data_dir = fx.repo().join(".egregore");
    eg().args(["ingest"])
        .arg(fx.graph())
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&in_tree_data_dir)
        .assert()
        .success();

    // Commit a source change — the store is stale_head.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn v2() {}\n");
    commit_all(fx.repo(), "v2");

    // Refresh — the data-dir is inside the repo (untracked); it must not be
    // counted as dirty in the stamped snapshot.
    eg().args(["refresh"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&in_tree_data_dir)
        .assert()
        .success();

    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&in_tree_data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness"], "fresh",
        "data-dir must be excluded from the dirty probe during refresh: {report}"
    );
}

// ---------------------------------------------------------------------------
// Codex follow-up round 3 (commit 67dcda5)
// ---------------------------------------------------------------------------

/// Finding 5: `status.showUntrackedFiles=no` must not suppress the dirty probe.
/// An untracked source file must still be detected (PR #186 follow-up).
#[test]
fn dirty_probe_detects_untracked_files_despite_config() {
    let fx = Fixture::committed();
    fx.scan();

    // Simulate `status.showUntrackedFiles=no` in the repo config.
    git(fx.repo(), ["config", "status.showUntrackedFiles", "no"]);

    // Add an untracked .rs file — without --untracked-files=all this would be hidden.
    std::fs::write(fx.repo().join("src").join("new.rs"), "pub fn new_fn() {}\n").unwrap();

    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--graph")
        .arg(fx.graph())
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness"], "stale_dirty",
        "untracked files must be detected even when showUntrackedFiles=no: {report}"
    );
}

/// Finding 4: `--as-of` branch must stamp freshness (PR #186 follow-up).
/// `--as-of` works on valid-time in current-scan output (no scan-history needed).
#[test]
fn query_symbol_as_of_surfaces_freshness() {
    let fx = Fixture::committed();
    fx.scan();

    // Use a far-future timestamp so the current record is always "most recent".
    let out = eg()
        .args([
            "query",
            "symbol",
            "hello",
            "--as-of",
            "2099-01-01T00:00:00Z",
        ])
        .arg("--graph")
        .arg(fx.graph())
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--format", "json"])
        .assert()
        .success();
    let line = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(
        row["freshness"], "fresh",
        "`--as-of` branch must stamp freshness: {row}"
    );
}

/// Finding 9: a full `eg scan` must not stamp `dirty=true` due to an untracked
/// in-tree `.egregore` data-dir (PR #186 follow-up). The fix is at stamp time
/// (the snapshot stored in the JSONL must have `dirty: false`). The freshness
/// check-time gap for an untracked `.egregore` is covered by gitignore (F-class).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn scan_excludes_in_tree_egregore_data_dir() {
    let fx = Fixture::committed();
    // graph.jsonl is gitignored; .egregore is left untracked (not gitignored) to
    // exercise the auto-detect exclusion in the scan's dirty probe.
    let in_tree_graph = fx.repo().join("graph.jsonl");
    let in_tree_data_dir = fx.repo().join(".egregore");
    std::fs::write(fx.repo().join(".gitignore"), "*.jsonl\n").unwrap();
    commit_all(fx.repo(), "add gitignore");

    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_graph)
        .assert()
        .success();
    eg().args(["ingest"])
        .arg(&in_tree_graph)
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&in_tree_data_dir)
        .assert()
        .success();

    // Second scan: .egregore is untracked; must stamp dirty=false (not dirty=true).
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_graph)
        .assert()
        .success();

    // Verify the stamp directly from the JSONL (stamp-time fix, not check-time).
    let jsonl = std::fs::read_to_string(&in_tree_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node in graph");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(false),
        "in-tree .egregore must not stamp dirty=true on the snapshot: {repo_node}"
    );
}

/// Finding 7: custom `--cache` path must be excluded from the refresh dirty probe
/// at stamp time (PR #186 follow-up). A pre-existing cache does not stamp dirty=true
/// on the refreshed Repository snapshot. Uses an in-repo cache path to exercise the
/// exclusion; the data-dir is out-of-repo so the check-time probe stays clean.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_excludes_custom_cache_from_dirty_probe() {
    let fx = Fixture::committed();
    fx.scan();
    // data-dir lives outside the repo (work dir) so check-time sees it as ignored.
    let data_dir = fx.data_dir();
    // cache lives inside the repo (untracked) to exercise stamp-time exclusion.
    let custom_cache = fx.repo().join("my-cache.json");

    eg().args(["ingest"])
        .arg(fx.graph())
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // First refresh creates the custom cache (in-tree, untracked).
    eg().args(["refresh"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--cache")
        .arg(&custom_cache)
        .assert()
        .success();

    // Second refresh: custom cache already exists (in-tree, untracked).
    // Must stamp dirty=false on the snapshot (stamp-time fix).
    eg().args(["refresh"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--cache")
        .arg(&custom_cache)
        .assert()
        .success();

    // Verify stamp-time: stored_snapshot.dirty must be false (the cache was excluded).
    // The check-time probe sees my-cache.json as untracked (F-class limitation; fix
    // the gap by gitignoring the custom cache in production use).
    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["stored_snapshot"]["dirty"],
        Value::Bool(false),
        "custom cache must not stamp dirty=true on the refreshed snapshot: {report}"
    );
}

/// X3a: a `.egregore`-prefixed *file* (not a directory) must not be auto-excluded
/// from the scan dirty probe — only untracked *directories* are store outputs.
/// Modifying a tracked `.egregore`-prefixed source file must produce `dirty=true`.
#[test]
fn scan_does_not_exclude_tracked_egregore_prefixed_file() {
    let fx = Fixture::committed();
    // Add a tracked source file whose name starts with `.egregore`.
    std::fs::write(fx.repo().join(".egregore_plugin.rs"), "// plugin\n").unwrap();
    commit_all(fx.repo(), "add egregore plugin file");

    // Modify the tracked file (uncommitted) — the tree is dirty.
    std::fs::write(fx.repo().join(".egregore_plugin.rs"), "// modified\n").unwrap();

    // Scan: the file is tracked, so it must NOT be excluded from the dirty probe.
    let out_graph = fx.work.path().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&out_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&out_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(true),
        "tracked .egregore-prefixed file must not be excluded: {repo_node}"
    );
}

/// X3b: a `.egregore`-prefixed directory that contains tracked content must not
/// be auto-excluded from the scan dirty probe — only directories with no tracked
/// content are store outputs.
#[test]
fn scan_does_not_exclude_egregore_directory_with_tracked_content() {
    let fx = Fixture::committed();
    // Add a tracked file inside an `.egregore_src/` directory.
    let tracked_dir = fx.repo().join(".egregore_src");
    std::fs::create_dir_all(&tracked_dir).unwrap();
    std::fs::write(tracked_dir.join("mod.rs"), "// module\n").unwrap();
    commit_all(fx.repo(), "add egregore_src directory");

    // Modify the tracked file (uncommitted).
    std::fs::write(tracked_dir.join("mod.rs"), "// modified\n").unwrap();

    let out_graph = fx.work.path().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&out_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&out_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(true),
        "tracked .egregore-prefixed directory must not be excluded: {repo_node}"
    );
}

/// Y1: an untracked `.egregore`-prefixed directory that contains `.rs` source
/// files must not be auto-excluded from the dirty probe — its files appear in
/// the graph and must be covered by the freshness signal.
#[test]
fn scan_does_not_exclude_untracked_egregore_dir_with_rust_sources() {
    let fx = Fixture::committed();
    // Create an untracked `.egregore_plugin/` directory containing Rust source.
    // It has no tracked content (git ls-files is empty) but it has `.rs` files,
    // so it must not be excluded from the dirty probe.
    let plugin_dir = fx.repo().join(".egregore_plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("lib.rs"), "// plugin\n").unwrap();

    // Modify the `.rs` file (not committed, not gitignored) — tree is dirty.
    std::fs::write(plugin_dir.join("lib.rs"), "// modified\n").unwrap();

    let out_graph = fx.work.path().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&out_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&out_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(true),
        "untracked .egregore-prefixed directory with .rs files must not be excluded: {repo_node}"
    );
}

/// Y2: `query context` with `--repo-path` on an override-ID store must emit a
/// freshness verdict — the context owner is now used as the hint so the lookup
/// finds the override-ID snapshot rather than the auto-detected identity.
#[test]
fn query_context_uses_context_owner_as_freshness_hint() {
    let fx = Fixture::committed();
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(fx.graph())
        .args(["--repo-id-override", "my-context-repo"])
        .assert()
        .success();

    let out = eg()
        .args(["query", "context", "hello"])
        .arg("--graph")
        .arg(fx.graph())
        .arg("--repo-path")
        .arg(fx.repo())
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(report["ok"], Value::Bool(true));
    assert!(
        report.get("freshness").is_some(),
        "context must include freshness when owner hint resolves the override-ID snapshot: {report}"
    );
    assert_eq!(
        report["freshness"], "fresh",
        "context freshness must be fresh for a just-scanned override-ID repo: {report}"
    );
}

/// Y3: when the selected repo's `Repository` node has no `source_snapshot`
/// (pre-stamping / legacy store) and the hint differs from the auto-detected
/// identity, the `unknown` verdict must still be stamped on the matching rows
/// rather than silently dropped.
///
/// Without the hint-owner fallback the owner defaults to `identity.id`; then
/// `stamp_freshness` compares against a different ID and emits no field at all.
#[test]
fn freshness_unknown_stamped_on_legacy_override_repo_rows() {
    let fx = Fixture::committed();

    // Scan with override ID — produces a stamped store.
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(fx.graph())
        .args(["--repo-id-override", "legacy-override-repo"])
        .assert()
        .success();

    // Strip `source_snapshot` from every record to simulate a pre-stamping store:
    // rewrite the JSONL removing that field from Repository nodes.
    let raw = std::fs::read_to_string(fx.graph()).unwrap();
    let stripped: String = raw
        .lines()
        .map(|line| {
            let Ok(mut v) = serde_json::from_str::<Value>(line) else {
                return line.to_owned();
            };
            if v["kind"] == "Repository" {
                v.as_object_mut().map(|o| o.remove("source_snapshot"));
            }
            serde_json::to_string(&v).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(fx.graph(), stripped).unwrap();

    // Query symbol with --repo legacy-override-repo --repo-path:
    // the `unknown` verdict must be stamped on the row (not absent).
    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(fx.graph())
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--repo", "legacy-override-repo"])
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let first_line = stdout.lines().next().unwrap_or("{}");
    let row: Value = serde_json::from_str(first_line).unwrap();
    assert!(
        row.get("freshness").is_some(),
        "legacy override-ID rows must still carry a freshness field: {row}"
    );
    assert_eq!(
        row["freshness"], "unknown",
        "legacy pre-stamping store must emit unknown, not omit the field: {row}"
    );
}

/// Y4: `scan` must not descend into gitignored directories, so a repo with a
/// gitignored directory that contains unreadable content does not cause a
/// traversal failure.
///
/// We test the observable outcome — directory pruning — by verifying that when
/// a directory is listed in `.gitignore`, no symbols from it appear in the graph
/// even though its `.rs` files exist on disk.
#[test]
fn scan_skips_gitignored_directory_before_traversal() {
    let fx = Fixture::committed();
    // Create a gitignored `generated/` directory with a Rust file.
    let gen_dir = fx.repo().join("generated");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(gen_dir.join("gen.rs"), "pub fn generated_fn() {}\n").unwrap();
    std::fs::write(fx.repo().join(".gitignore"), "/generated/\n").unwrap();
    commit_all(fx.repo(), "add gitignore");

    let out_graph = fx.work.path().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&out_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&out_graph).unwrap();
    assert!(
        !jsonl.contains("generated_fn"),
        "gitignored directory must be pruned before traversal, not post-filtered: {jsonl}"
    );
    assert!(
        jsonl.contains("hello"),
        "tracked source symbols must still appear in the graph: {jsonl}"
    );
}
