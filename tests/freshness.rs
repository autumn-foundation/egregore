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

/// Z1: `query symbol --repo-path` on a combined store where the checkout was
/// scanned with `--repo-id-override` must still emit a freshness verdict even
/// when `--repo` is not given.
///
/// The sole-stamped fallback fires when exactly one Repository node in the
/// store carries a `source_snapshot`; without it the identity probe fails, the
/// verdict is owned by the wrong ID, and `stamp_freshness` omits the field.
#[test]
fn freshness_stamped_without_repo_flag_via_sole_stamped_fallback() {
    let fx = Fixture::committed();

    // Scan checkout A with an override ID → one stamped repo.
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(fx.graph())
        .args(["--repo-id-override", "sole-override-repo"])
        .assert()
        .success();

    // Add a second Repository node (no snapshot) to make a multi-repo store,
    // disabling the single-repo fallback.  Strip its source_snapshot (pre-stamping).
    let raw = std::fs::read_to_string(fx.graph()).unwrap();
    let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
    lines.push(
        r#"{"record_type":"node","schema_version":1,"id":"codegraph:v3:other-repo","kind":"Repository","display_name":"other-repo","summary":"Repository other-repo"}"#
            .to_owned(),
    );
    std::fs::write(fx.graph(), lines.join("\n")).unwrap();

    // Query without --repo: the sole-stamped fallback must find the one stamped
    // repo (sole-override-repo) and use it to stamp freshness on the result rows.
    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(fx.graph())
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert!(
        row.get("freshness").is_some(),
        "sole-stamped fallback must stamp freshness when --repo is absent: {row}"
    );
    assert_eq!(
        row["freshness"], "fresh",
        "sole-stamped fallback must return fresh for a just-scanned override-ID repo: {row}"
    );
}

/// BB1: `scan-history` must stamp `source_snapshot` on its Repository node so
/// `eg freshness --graph history.graph.jsonl` can detect `stale_head` after new
/// commits, rather than always returning `unknown`.
#[test]
fn scan_history_stamps_source_snapshot() {
    let fx = Fixture::committed();
    let history_graph = fx.work.path().join("history.jsonl");

    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&history_graph)
        .assert()
        .success();

    // The Repository node in the history graph must carry a source_snapshot.
    let jsonl = std::fs::read_to_string(&history_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node in history graph");
    assert!(
        repo_node["source_snapshot"].is_object(),
        "scan-history Repository node must carry source_snapshot: {repo_node}"
    );
    assert!(
        repo_node["source_snapshot"]["head"].is_object(),
        "source_snapshot must have a head: {repo_node}"
    );

    // Freshness check: a just-produced history graph at the current HEAD must
    // not report `unknown` — it should report `fresh` or `stale_dirty` at worst.
    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--graph")
        .arg(&history_graph)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_ne!(
        report["freshness"], "unknown",
        "freshness for a just-produced history graph must not be unknown: {report}"
    );
}

/// CC1: re-running `scan-history --out history.graph.jsonl` when the previous
/// in-tree output is still untracked must not stamp `dirty=true` on the new
/// snapshot.  Without the exclusion the pre-existing JSONL is counted as
/// untracked dirtiness and the freshly replayed store reports `stale_dirty`.
#[test]
fn scan_history_excludes_in_tree_output_from_dirty_probe() {
    let fx = Fixture::committed();
    let in_tree_history = fx.repo().join("history.graph.jsonl");

    // First scan-history — creates the in-tree artifact.
    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_history)
        .assert()
        .success();

    // Second scan-history: pre-existing untracked history.graph.jsonl must not
    // stamp dirty=true on the new snapshot.
    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_history)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&in_tree_history).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node in history graph");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(false),
        "re-run scan-history must not stamp dirty=true for its own output: {repo_node}"
    );
}

/// DD1: the dirty probe (`git status`) must run with `-c core.excludesFile=`
/// matching the scanner's own gitignore checks.  Without the override,
/// `git status` honours the user's global excludes file and silently hides
/// untracked files that the scanner (which also runs with `-c core.excludesFile=`)
/// has already indexed.  The mismatch causes `dirty=false` to be stamped even
/// though indexed untracked files are present, and a later edit to those files
/// produces a false `fresh` verdict.
///
/// This test sets `GIT_CONFIG_GLOBAL` to a temporary gitconfig that installs a
/// global excludes file matching `globally_hidden.rs`, then verifies that:
/// 1. The scanner still indexes the file (its `git check-ignore` suppresses the
///    global config).
/// 2. The dirty probe also suppresses the global config and stamps `dirty=true`
///    for the untracked file.
#[test]
fn dirty_probe_suppresses_global_excludes_file() {
    let fx = Fixture::committed();

    let global_dir = tempfile::tempdir().unwrap();
    let global_excludes = global_dir.path().join("excludes");
    let global_gitconfig = global_dir.path().join("gitconfig");

    std::fs::write(&global_excludes, "globally_hidden.rs\n").unwrap();
    std::fs::write(
        &global_gitconfig,
        format!("[core]\n\texcludesFile = {}\n", global_excludes.display()),
    )
    .unwrap();

    // Untracked Rust file that matches the global ignore pattern.
    std::fs::write(
        fx.repo().join("src").join("globally_hidden.rs"),
        "pub fn hidden() {}\n",
    )
    .unwrap();

    let out_graph = fx.work.path().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&out_graph)
        .env("GIT_CONFIG_GLOBAL", &global_gitconfig)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&out_graph).unwrap();
    assert!(
        jsonl.contains("globally_hidden"),
        "globally-ignored untracked file must be indexed by the scanner: {jsonl}"
    );

    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node in graph");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(true),
        "globally-ignored untracked file must be visible to the dirty probe: {repo_node}"
    );
}

/// EE1: `discover_rust_source_files` must not descend into git submodules (or
/// linked worktrees), whose `.git` entry is a FILE rather than a directory.
///
/// Without this guard, paths inside the submodule are fed to `git check-ignore
/// --stdin`, which can exit 128 and disable gitignore filtering for the entire
/// superproject scan.  Symbols from the submodule would then also appear in the
/// superproject's graph.
#[test]
fn scan_does_not_descend_into_submodule_directory() {
    let fx = Fixture::committed();

    // Simulate a submodule by creating a directory with `.git` as a FILE
    // (the real on-disk shape of a checked-out submodule or linked worktree).
    let sub_dir = fx.repo().join("deps").join("sub");
    std::fs::create_dir_all(&sub_dir).unwrap();
    std::fs::write(sub_dir.join(".git"), "gitdir: ../../.git/modules/sub\n").unwrap();
    std::fs::create_dir_all(sub_dir.join("src")).unwrap();
    std::fs::write(
        sub_dir.join("src").join("lib.rs"),
        "pub fn submodule_fn() {}\n",
    )
    .unwrap();

    let out_graph = fx.work.path().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&out_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&out_graph).unwrap();
    assert!(
        !jsonl.contains("submodule_fn"),
        "scanner must not descend into submodule directories: {jsonl}"
    );
    assert!(
        jsonl.contains("hello"),
        "superproject symbols must still appear in the graph: {jsonl}"
    );
}

/// FF1: `eg freshness --graph graph.jsonl` must exclude an unignored in-tree
/// `.egregore` embedded store (the companion of the documented ingest workflow)
/// from the dirty probe, just as `eg scan` does.  Otherwise the same freshly
/// scanned graph reads `stale_dirty` solely because the store directory exists.
#[test]
fn freshness_graph_ignores_untracked_egregore_store_dir() {
    let fx = Fixture::committed();
    // Graph lives in the work dir (outside the tree); the only in-tree untracked
    // artifact is the `.egregore` store, which is NOT gitignored here.
    fx.scan();
    let store = fx.repo().join(".egregore");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("records.bin"), b"\x00\x01embedded-store").unwrap();

    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "fresh",
        "an untracked in-tree .egregore store must not make a graph store read stale_dirty: {report}"
    );
}

/// GG1: re-running `scan-history` in a repository that already holds an unignored
/// in-tree `.egregore` store must not stamp `dirty=true` on the replayed
/// snapshot.  History replay reads only Git objects, so a companion store is not
/// source dirtiness.
#[test]
fn scan_history_ignores_untracked_egregore_store_dir() {
    let fx = Fixture::committed();
    let store = fx.repo().join(".egregore");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("records.bin"), b"\x00\x01embedded-store").unwrap();

    let history_graph = fx.work.path().join("history.graph.jsonl");
    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&history_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&history_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node in history graph");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(false),
        "an untracked in-tree .egregore store must not stamp scan-history dirty=true: {repo_node}"
    );
}

/// `HH1a`: unignored build output under `target/` must not count as source
/// dirtiness.  `discover_rust_source_files` skips every `target` directory
/// (`fs::should_descend`), so neither the stamped snapshot nor the freshness
/// probe may treat it as a working-tree change.
#[test]
fn freshness_ignores_unignored_target_build_output() {
    let fx = Fixture::committed();
    // Build output present before the scan — `target/` is not gitignored here.
    let target = fx.repo().join("target").join("debug");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("app"), b"binary-artifact").unwrap();

    fx.scan();
    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "fresh",
        "unignored target/ build output must not be counted as source dirtiness: {report}"
    );
}

/// `HH1b`: a dirty submodule must not make the superproject read `stale_dirty`.
/// The scanner never descends into submodules (`fs::should_descend` skips
/// `.git`-file directories), so `--ignore-submodules=all` keeps the dirty probe
/// scoped to the indexed source set.
#[test]
fn freshness_ignores_dirty_submodule() {
    let fx = Fixture::committed();

    // A separate repo to register as a submodule.
    let sub_src = tempfile::tempdir().unwrap();
    git_init(sub_src.path());
    std::fs::write(sub_src.path().join("README.md"), "sub\n").unwrap();
    commit_all(sub_src.path(), "sub initial");

    // Local-path submodule adds require protocol.file.allow on modern Git.
    git(
        fx.repo(),
        [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_src.path().to_str().unwrap(),
            "sub",
        ],
    );
    commit_all(fx.repo(), "add submodule");

    fx.scan();

    // Dirty the submodule working tree; the superproject's `git status` now shows
    // the submodule as modified by default.
    std::fs::write(fx.repo().join("sub").join("README.md"), "sub changed\n").unwrap();

    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "fresh",
        "a dirty submodule must not make the superproject read stale_dirty: {report}"
    );
}

/// II1: `query symbol --repo-path` must exclude an unignored in-tree `.egregore`
/// companion store from the dirty probe, exactly as `freshness_cmd` does.
/// Otherwise rows are stamped `stale_dirty` solely because the store exists,
/// even though the graph was scanned from a clean tree.
#[test]
fn query_symbol_repo_path_ignores_untracked_egregore_store_dir() {
    let fx = Fixture::committed();
    // Graph lives outside the tree; the only in-tree untracked artifact is the
    // (not gitignored) `.egregore` store.
    fx.scan();
    let store = fx.repo().join(".egregore");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("records.bin"), b"\x00\x01embedded-store").unwrap();

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
        "an untracked in-tree .egregore store must not stamp query rows stale_dirty: {row}"
    );
}

/// `JJ1`: unignored per-crate `target/` build output in a workspace must not
/// count as source dirtiness.  `fs::should_descend` skips every `target`
/// directory at any depth, so a nested `crates/*/target/` tree is invisible to
/// the scanner and must be excluded from the dirty probe too (the root-only
/// `:(exclude)target` pathspec would otherwise miss it).
#[test]
fn freshness_ignores_unignored_nested_target_build_output() {
    let fx = Fixture::committed();
    // Nested per-crate build output, present before the scan and not gitignored.
    let nested = fx
        .repo()
        .join("crates")
        .join("a")
        .join("target")
        .join("debug");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("out.txt"), b"artifact").unwrap();

    fx.scan();
    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "fresh",
        "unignored nested target/ build output must not be counted as source dirtiness: {report}"
    );
}

/// MM1: `eg refresh --data-dir <out-of-tree>` must exclude an unignored default
/// in-tree `.egregore` store left by a prior ingest from the dirty probe, so the
/// re-stamped snapshot is not `dirty=true` and a later freshness check is not
/// spuriously `stale_dirty`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_ignores_untracked_default_egregore_store_dir() {
    let fx = Fixture::committed();
    fx.scan();

    // Establish the out-of-tree refresh target via the documented scan→ingest
    // workflow (`eg refresh` requires a prior ingest into the data-dir).
    let out_of_tree = fx.work.path().join("store");
    eg().args(["ingest"])
        .arg(fx.graph())
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&out_of_tree)
        .assert()
        .success();

    // A leftover untracked default `.egregore` store inside the repo (not the
    // refresh target, which is the out-of-tree work dir).
    let stale_store = fx.repo().join(".egregore");
    std::fs::create_dir_all(&stale_store).unwrap();
    std::fs::write(stale_store.join("records.bin"), b"\x00\x01old-store").unwrap();

    // Refresh into the out-of-tree data-dir.
    eg().args(["refresh"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&out_of_tree)
        .assert()
        .success();

    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&out_of_tree)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness"], "fresh",
        "a leftover in-tree .egregore store must not make a refreshed out-of-tree store stale_dirty: {report}"
    );
}

/// `LL1`: a tracked `.rs` file marked `assume-unchanged` hides its working-tree
/// state from `git status`, so the dirty probe must conservatively report the
/// tree dirty — otherwise a graph built from a full checkout could read `fresh`
/// after the file is edited or removed.
#[test]
fn freshness_treats_assume_unchanged_rust_source_as_dirty() {
    let fx = Fixture::committed();
    fx.scan();
    // After this, `git status` can no longer see edits/removal of src/lib.rs.
    git(
        fx.repo(),
        ["update-index", "--assume-unchanged", "src/lib.rs"],
    );

    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "stale_dirty",
        "an assume-unchanged tracked .rs file must make the probe conservatively dirty: {report}"
    );
}

/// `LL1`: same conservative treatment for `skip-worktree`, the flag sparse
/// checkout sets to keep files index-only.
#[test]
fn freshness_treats_skip_worktree_rust_source_as_dirty() {
    let fx = Fixture::committed();
    fx.scan();
    git(fx.repo(), ["update-index", "--skip-worktree", "src/lib.rs"]);

    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "stale_dirty",
        "a skip-worktree tracked .rs file must make the probe conservatively dirty: {report}"
    );
}

/// `LL1`: the index-flag check is scoped to `.rs` files (the only files the
/// scanner indexes), so an `assume-unchanged` non-source file must not flip the
/// verdict to dirty.
#[test]
fn freshness_ignores_assume_unchanged_non_rust_file() {
    let fx = Fixture::committed();
    // A tracked non-source file, committed before the scan stamps the snapshot.
    std::fs::write(fx.repo().join("README.md"), "docs\n").unwrap();
    commit_all(fx.repo(), "add readme");
    fx.scan();
    git(
        fx.repo(),
        ["update-index", "--assume-unchanged", "README.md"],
    );

    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "fresh",
        "assume-unchanged on a non-.rs file must not affect freshness: {report}"
    );
}

/// NN1: a `query symbol --repo-path` against a `scan-history` graph must carry a
/// freshness verdict.  History File/Symbol rows have no repository ownership in
/// the topology the index uses, so the verdict is stamped via the unambiguous
/// single-repo path rather than left absent.
#[test]
fn query_symbol_repo_path_stamps_freshness_on_history_graph() {
    let fx = Fixture::committed();
    let history_graph = fx.work.path().join("history.graph.jsonl");
    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&history_graph)
        .assert()
        .success();

    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(&history_graph)
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--format", "json"])
        .assert()
        .success();
    let line = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert!(
        row.get("freshness").is_some(),
        "history-graph query rows must carry a freshness verdict in a single-repo store: {row}"
    );
    assert_eq!(row["freshness"], "fresh");
}

/// OO1: `eg refresh` must report the freshness verdict a follow-up
/// `eg freshness --data-dir` would compute, not an unconditional `"fresh"`.
/// When the working tree has uncommitted `.rs` edits, the refreshed store is
/// stamped dirty, so both must agree on `stale_dirty`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn refresh_reports_stale_dirty_when_tree_has_uncommitted_edits() {
    let fx = Fixture::committed();
    fx.scan();
    let data_dir = fx.work.path().join("store");
    eg().args(["ingest"])
        .arg(fx.graph())
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Uncommitted edit to a tracked .rs file → dirty working tree.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn scratch() {}\n");

    let out = eg()
        .args(["refresh"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness_after_refresh"], "stale_dirty",
        "refresh must report the real verdict for a dirty tree, not always fresh: {report}"
    );

    // A follow-up freshness check must agree with the refresh report.
    let out2 = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--format", "json"])
        .assert()
        .success();
    let report2: Value = serde_json::from_slice(&out2.get_output().stdout).unwrap();
    assert_eq!(
        report2["freshness"], "stale_dirty",
        "follow-up freshness must agree with the refresh report: {report2}"
    );
}

/// RR1: `eg freshness --data-dir` cannot name the sibling `graph.jsonl` produced
/// by the documented scan→ingest workflow, so an untracked, non-gitignored
/// companion graph output must not make the store read `stale_dirty`. JSONL
/// outputs are never indexed as Rust source, so they are excluded from the probe.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn freshness_data_dir_ignores_untracked_companion_graph_jsonl() {
    let fx = Fixture::committed();
    // In-tree graph output, deliberately NOT gitignored.
    let in_tree_graph = fx.repo().join("graph.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&in_tree_graph)
        .assert()
        .success();
    let data_dir = fx.repo().join(".egregore");
    eg().args(["ingest"])
        .arg(&in_tree_graph)
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

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
        "an untracked companion graph.jsonl must not make a data-dir store read stale_dirty: {report}"
    );
}

/// `SS1`: in a combined store where `--repo` selects a legacy/pre-stamping repo
/// but the checkout's auto-detected identity is ALSO present and stamped, the
/// selected repo must remain the verdict owner. The old hint branch fell back to
/// the auto-detected identity's snapshot, so `stamp_freshness` compared a
/// different owner and omitted the field; the selected legacy rows must instead
/// be stamped `unknown`.
#[test]
fn freshness_unknown_stamped_on_legacy_rows_when_identity_also_stamped() {
    let fx = Fixture::committed();

    // (1) Normal scan → the checkout's auto-detected identity, stamped.
    let stamped_graph = fx.work.path().join("stamped.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&stamped_graph)
        .assert()
        .success();

    // (2) Override-ID scan → a second repo; strip its source_snapshot to simulate
    //     legacy/pre-stamping rows that have no stamped snapshot.
    let legacy_graph = fx.work.path().join("legacy.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&legacy_graph)
        .args(["--repo-id-override", "legacy-override-repo"])
        .assert()
        .success();
    let legacy_stripped: String = std::fs::read_to_string(&legacy_graph)
        .unwrap()
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

    // (3) Combine: stamped auto-detected identity repo + legacy (unstamped) repo.
    let combined = fx.work.path().join("combined.jsonl");
    let stamped_raw = std::fs::read_to_string(&stamped_graph).unwrap();
    std::fs::write(
        &combined,
        format!("{}\n{}\n", stamped_raw.trim_end(), legacy_stripped),
    )
    .unwrap();

    // (4) Query the legacy repo with --repo-path: the verdict must be owned by the
    //     selected legacy repo and stamped `unknown`, not borrowed from the
    //     stamped auto-detected identity (which would omit the field).
    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(&combined)
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--repo", "legacy-override-repo"])
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(stdout.lines().next().unwrap_or("{}")).unwrap();
    assert!(
        row.get("freshness").is_some(),
        "the selected legacy rows must still carry a freshness field: {row}"
    );
    assert_eq!(
        row["freshness"], "unknown",
        "the selected legacy repo (no snapshot) must report unknown, not borrow the stamped identity's verdict: {row}"
    );
}

/// `TT1`: history replay reflects committed Git objects only, so the stamped
/// snapshot records the committed HEAD with `dirty=false` even when the working
/// tree has uncommitted edits. Otherwise reverting those edits (HEAD unchanged)
/// would leave the replayed graph permanently `stale_dirty`.
#[test]
fn scan_history_stamps_committed_state_not_worktree_dirt() {
    let fx = Fixture::committed();
    // Uncommitted edit to a tracked .rs file — must not enter the history snapshot.
    write_lib(fx.repo(), "pub fn hello() {}\npub fn scratch() {}\n");

    let history_graph = fx.work.path().join("history.graph.jsonl");
    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&history_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&history_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node in history graph");
    assert_eq!(
        repo_node["source_snapshot"]["dirty"],
        Value::Bool(false),
        "history snapshot must record committed state (dirty=false) regardless of worktree edits: {repo_node}"
    );
}

/// `TT3`: the history snapshot timestamp is derived from HEAD's committer date
/// (deterministic), not wall-clock, so repeated scans of an unchanged repository
/// are byte-stable even across a seconds boundary. The fixture commits at a fixed
/// committer date, so `scanned_at` must equal it.
#[test]
fn scan_history_snapshot_timestamp_is_deterministic_from_head_commit() {
    let fx = Fixture::committed();
    let history_graph = fx.work.path().join("history.graph.jsonl");
    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&history_graph)
        .assert()
        .success();

    let jsonl = std::fs::read_to_string(&history_graph).unwrap();
    let repo_node = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["record_type"] == "node" && v["kind"] == "Repository")
        .expect("Repository node in history graph");
    assert_eq!(
        repo_node["source_snapshot"]["scanned_at"], "2026-01-01T00:00:00Z",
        "history scanned_at must derive from HEAD committer date (deterministic), not wall-clock: {repo_node}"
    );

    // Determinism: a second scan of the unchanged repository is byte-identical.
    let history_graph_2 = fx.work.path().join("history.graph.2.jsonl");
    eg().args(["scan-history"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&history_graph_2)
        .assert()
        .success();
    assert_eq!(
        jsonl,
        std::fs::read_to_string(&history_graph_2).unwrap(),
        "repeated history scans of an unchanged repository must be byte-identical"
    );
}

/// `UU1`: an explicit `--repo` that matches the checkout's auto-detected identity
/// must remain authoritative. If that selected repo is legacy/pre-stamping and a
/// different repo in the combined store is stamped, the verdict must be `unknown`
/// owned by the selected repo — not borrowed from the other stamped repo via the
/// sole-stamped fallback (which would omit the field from the selected rows).
#[test]
fn freshness_unknown_when_explicit_repo_matches_auto_identity_but_is_legacy() {
    let fx = Fixture::committed();

    // (1) Normal scan → the checkout's auto-detected identity B, stamped. Capture
    //     its stable record id to select it explicitly, then strip its snapshot.
    let b_graph = fx.work.path().join("b.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&b_graph)
        .assert()
        .success();
    let b_raw = std::fs::read_to_string(&b_graph).unwrap();
    let b_id = b_raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["kind"] == "Repository")
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .expect("B repository id");
    let b_stripped: String = b_raw
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

    // (2) A second, stamped repo C (override id), so a sole-stamped repo exists.
    let c_graph = fx.work.path().join("c.jsonl");
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(&c_graph)
        .args(["--repo-id-override", "other-stamped-repo"])
        .assert()
        .success();
    let c_raw = std::fs::read_to_string(&c_graph).unwrap();

    // (3) Combine: legacy (selected) B + stamped C.
    let combined = fx.work.path().join("combined.jsonl");
    std::fs::write(&combined, format!("{}\n{}\n", b_stripped, c_raw.trim_end())).unwrap();

    // (4) Explicitly select B (== the auto-detected identity) by its record id.
    let out = eg()
        .args(["query", "symbol", "hello"])
        .arg("--graph")
        .arg(&combined)
        .arg("--repo-path")
        .arg(fx.repo())
        .args(["--repo", &b_id])
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row: Value = serde_json::from_str(stdout.lines().next().unwrap_or("{}")).unwrap();
    assert!(
        row.get("freshness").is_some(),
        "explicitly selecting the auto-identity (legacy) repo must still stamp freshness: {row}"
    );
    assert_eq!(
        row["freshness"], "unknown",
        "must report unknown for the selected legacy repo, not borrow the other stamped repo's verdict: {row}"
    );
}

/// `WW1`: the index-hidden check (assume-unchanged/skip-worktree) must apply the
/// scanner's `target/` pruning. A tracked `.rs` under `target/` is never indexed,
/// so an index-hidden flag on it must not force `stale_dirty`.
#[test]
fn freshness_ignores_index_hidden_rust_under_target() {
    let fx = Fixture::committed();
    let target = fx.repo().join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("gen.rs"), "pub fn gen() {}\n").unwrap();
    commit_all(fx.repo(), "add target rs");
    fx.scan();
    // Hide the target .rs from `git status`; the scanner never indexed it.
    git(
        fx.repo(),
        ["update-index", "--assume-unchanged", "target/gen.rs"],
    );

    let report = fx.freshness_graph();
    assert_eq!(
        report["freshness"], "fresh",
        "an index-hidden .rs under target/ must not stale the store (it is never indexed): {report}"
    );
}

/// `YY1`: `eg freshness --repo-id-override <wrong>` against a single-repo store
/// must report `unknown`, not borrow the sole repository's snapshot via the
/// single-repository fallback (which could even report `fresh`).
#[test]
fn freshness_unknown_for_wrong_explicit_override_in_single_repo_store() {
    let fx = Fixture::committed();
    fx.scan();

    let out = eg()
        .args(["freshness"])
        .arg(fx.repo())
        .arg("--graph")
        .arg(fx.graph())
        .args(["--repo-id-override", "totally-unrelated-id"])
        .args(["--format", "json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        report["freshness"], "unknown",
        "a wrong explicit override must report unknown, not borrow the sole repo's snapshot: {report}"
    );
}

/// `XX1`: a custom in-tree `--cache` path outside `--data-dir` (which the
/// follow-up `eg freshness --data-dir` cannot name) must not make the store read
/// `stale_dirty`. A cache file is never indexed source, so the dirty probe ignores
/// every non-`.rs` artifact regardless of name.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn freshness_data_dir_ignores_untracked_custom_cache() {
    let fx = Fixture::committed();
    fx.scan();
    let data_dir = fx.repo().join(".egregore");
    eg().args(["ingest"])
        .arg(fx.graph())
        .args(["--adapter", "embedded"])
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Custom cache placed in-tree, OUTSIDE data_dir, deliberately not gitignored.
    let custom_cache = fx.repo().join("my-refresh-cache.json");
    eg().args(["refresh"])
        .arg(fx.repo())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--cache")
        .arg(&custom_cache)
        .assert()
        .success();

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
        "an untracked custom refresh cache must not make the store read stale_dirty: {report}"
    );
}

/// `ZZ1`: when a store scanned with `--repo-id-override` is checked by
/// `eg freshness` WITHOUT repeating the override, the sole-repository fallback
/// classifies the right snapshot — and the reported `repository_id` must be that
/// stored Repository's owning ID, not the auto-detected checkout identity, so
/// consumers keying the verdict by repository are not misled.
#[test]
fn freshness_reports_snapshot_owner_for_override_store_without_override() {
    let fx = Fixture::committed();
    // Scan with an override id → a single-repo store owned by the override.
    eg().args(["scan"])
        .arg(fx.repo())
        .arg("--out")
        .arg(fx.graph())
        .args(["--repo-id-override", "override-repo"])
        .assert()
        .success();
    let owner_id = std::fs::read_to_string(fx.graph())
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["kind"] == "Repository")
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .expect("Repository id");

    // Run freshness WITHOUT the override: the auto-detected identity differs, so the
    // verdict comes from the sole-repository fallback.
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
        report["repository_id"],
        Value::String(owner_id),
        "freshness must report the snapshot's owning Repository id, not the auto-detected identity: {report}"
    );
    assert_eq!(
        report["freshness"], "fresh",
        "the override-scanned clean store must still classify fresh via the sole-repo fallback: {report}"
    );
}
