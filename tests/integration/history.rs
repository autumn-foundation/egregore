#![allow(missing_docs)]

use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{scan_repository_history, stable_id};
use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use serde_json::Value;

#[test]
fn git_history_replay_emits_bitemporal_records_without_mutating_checkout() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    let [first, second, third] = seed_history_repo(repo);

    let head_before = git_output(repo, ["rev-parse", "HEAD"]);
    let lib_before = fs::read_to_string(repo.join("src/lib.rs")).expect("fixture file");

    let first_jsonl = scan_repository_history(repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let second_jsonl = scan_repository_history(repo)
        .expect("history should scan twice")
        .to_jsonl()
        .expect("history graph should serialize twice");

    assert_eq!(first_jsonl, second_jsonl, "history replay must be stable");
    assert_eq!(git_output(repo, ["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        fs::read_to_string(repo.join("src/lib.rs")).expect("fixture file"),
        lib_before,
        "history replay must not mutate the checkout"
    );

    let records = parse_jsonl(&first_jsonl);
    assert_eq!(count_nodes(&records, "Commit"), 3);
    assert!(
        count_nodes(&records, "Change") >= 3,
        "each commit should emit at least one change record"
    );
    assert_edge_label(&records, "PARENT_OF");
    assert_edge_label(&records, "CHANGED_IN");

    assert_commit_node(&records, &first, "2026-01-01T00:00:00Z");
    assert_commit_node(&records, &second, "2026-01-02T00:00:00Z");
    assert_commit_node(&records, &third, "2026-01-03T00:00:00Z");
    assert_temporal_symbol(&records, "renamed", &second, "2026-01-02T00:00:00Z");
    assert_temporal_file(&records, "src/extra.rs", &third, "2026-01-03T00:00:00Z");
}

#[test]
fn scan_history_cli_writes_temporal_jsonl() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_history_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());

    let jsonl = fs::read_to_string(&graph_path).expect("scan-history should write JSONL");
    assert!(jsonl.contains(r#""kind":"Commit""#));
    assert!(jsonl.contains(r#""kind":"Change""#));
    assert!(jsonl.contains(r#""label":"PARENT_OF""#));
    assert!(jsonl.contains(r#""label":"CHANGED_IN""#));
    assert!(jsonl.contains(r#""valid_time":"2026-01-02T00:00:00Z""#));
}

#[test]
fn changed_in_commit_edges_only_target_changed_paths() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_two_file_history_repo(&repo);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);
    // Repo has no remote; identity is derived from the root commit SHA (= first commit).
    let repository_id = stable_id(&["repository", "local-root-commit", &first]);
    let second_commit_id = stable_id(&["node", "commit", &repository_id, &second]);
    let changed_sources = changed_in_sources_targeting(&records, &second_commit_id);

    assert_path_has_changed_source(&records, &changed_sources, "src/a.rs");
    assert_path_has_no_changed_source(&records, &changed_sources, "src/b.rs");
}

#[test]
fn merge_commits_with_rust_resolutions_keep_commit_changed_in_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let merge = seed_conflict_resolution_merge_repo(&repo);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);
    // Repo has no remote; identity is derived from the root commit SHA.
    let root_sha = git_root_sha(&repo);
    let repository_id = stable_id(&["repository", "local-root-commit", &root_sha]);
    let merge_commit_id = stable_id(&["node", "commit", &repository_id, &merge]);
    let changed_sources = changed_in_sources_targeting(&records, &merge_commit_id);

    assert_path_has_changed_source(&records, &changed_sources, "src/lib.rs");
}

/// Issue #438: history replay must not hard-abort on a non-UTF-8 committed
/// source blob. It skips the undecodable blob, emits a deterministic
/// `Diagnostic` naming the commit + path, and keeps replaying every decodable
/// file across the history. `git show` reads objects only, never mutating the
/// checkout.
#[test]
fn scan_history_skips_non_utf8_blob_and_records_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(&repo, ["config", "user.name", "Codegraph Test"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    write(&repo, "src/lib.rs", "pub fn good() -> u32 { 1 }\n");
    // A committed `.rs` blob whose bytes are UTF-16LE (BOM 0xFF 0xFE then LE
    // code units) — genuine text, never valid UTF-8.
    let mut bad = vec![0xFF, 0xFE];
    for unit in "pub fn hidden() {}".encode_utf16() {
        bad.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(repo.join("src/bad.rs"), &bad).expect("bad blob should be written");
    let commit_sha = commit(
        &repo,
        "add source and non-utf8 blob",
        "2026-04-01T00:00:00Z",
    );

    let head_before = git_output(&repo, ["rev-parse", "HEAD"]);

    let jsonl = scan_repository_history(&repo)
        .expect("history replay must complete over a non-UTF-8 blob")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);

    // The decodable file is still extracted across the history.
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Symbol"
                && r["repo_relative_path"] == "src/lib.rs"
                && r["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with("good"))
        }),
        "the decodable blob's symbol must be extracted"
    );
    // A Diagnostic names the skipped blob with a non-UTF-8 reason.
    let diagnostic = records.iter().find(|r| {
        r["record_type"] == "node"
            && r["kind"] == "Diagnostic"
            && r["repo_relative_path"] == "src/bad.rs"
    });
    let diagnostic = diagnostic.expect("a Diagnostic must name the skipped non-UTF-8 blob");
    assert!(
        diagnostic["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("UTF-8"),
        "diagnostic must state the non-UTF-8 decode failure: {diagnostic}"
    );
    // The blob's own bytes never enter the graph.
    assert!(
        !jsonl.contains("hidden"),
        "raw blob text must never enter the graph"
    );
    // The checkout is untouched (git show reads objects only).
    assert_eq!(git_output(&repo, ["rev-parse", "HEAD"]), head_before);
    let _ = commit_sha;
}

/// History replay must index a committed source file whose repo-relative path
/// carries non-ASCII bytes (issue #117 pre-fix).
///
/// `git ls-tree` C-quotes any path with a byte outside the printable ASCII
/// range unless `core.quotePath=false` is set, so `crates/café/src/lib.rs` is
/// listed as the literal ten-character escape sequence
/// `"crates/caf\303\251/src/lib.rs"` (quotes included). The replay then feeds
/// that quoted string straight back to `git show <sha>:<path>`, which fails —
/// so an entire real directory silently vanishes from the graph. Every other
/// Git call site in the crate already sets the flag (`src/fs.rs:236,368,465`,
/// `src/identity.rs:430,554`); `src/history.rs::git_output` was the one that
/// did not.
#[test]
fn history_replay_indexes_non_ascii_paths_unquoted() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(&repo, ["config", "user.name", "Codegraph Test"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    write(
        &repo,
        "crates/café/src/lib.rs",
        "pub fn accented() -> u32 { 1 }\n",
    );
    write(&repo, "src/plain.rs", "pub fn plain() -> u32 { 2 }\n");
    commit(&repo, "add accented path", "2026-05-01T00:00:00Z");

    let jsonl = scan_repository_history(&repo)
        .expect("history replay must complete over a non-ASCII path")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);

    // The accented file is indexed under its LITERAL repo-relative path — not a
    // C-quoted escape, and not absent.
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "File"
                && r["repo_relative_path"] == "crates/café/src/lib.rs"
        }),
        "the non-ASCII path must be indexed verbatim; got paths: {:?}",
        records
            .iter()
            .filter(|r| r["kind"] == "File")
            .map(|r| r["repo_relative_path"].clone())
            .collect::<Vec<_>>()
    );
    assert!(
        records.iter().any(|r| {
            r["record_type"] == "node"
                && r["kind"] == "Symbol"
                && r["repo_relative_path"] == "crates/café/src/lib.rs"
                && r["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with("accented"))
        }),
        "the symbol under the non-ASCII path must be extracted"
    );
    // No octal-escaped or quote-wrapped form leaks into the graph.
    assert!(
        !jsonl.contains(r"caf\303\251"),
        "a C-quoted octal path must never reach the graph"
    );
    // The ASCII sibling is unaffected (characterization).
    assert!(
        records
            .iter()
            .any(|r| r["kind"] == "File" && r["repo_relative_path"] == "src/plain.rs"),
        "ASCII paths must keep working"
    );
}

fn seed_history_repo(repo: &Path) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/lib.rs", "pub fn original() -> u32 { 1 }\n");
    let first = commit(repo, "initial symbol", "2026-01-01T00:00:00Z");

    write(repo, "src/lib.rs", "pub fn renamed() -> u32 { 2 }\n");
    let second = commit(repo, "rename symbol", "2026-01-02T00:00:00Z");

    write(
        repo,
        "src/extra.rs",
        "pub struct Added;\nimpl Added { pub fn value(&self) -> u32 { 3 } }\n",
    );
    let third = commit(repo, "add extra module", "2026-01-03T00:00:00Z");

    [first, second, third]
}

/// Salts the initial commit's content so two fixture repositories have
/// distinct root commits — and therefore distinct repository identities.
/// Git commit identity does not include the checkout path, so byte-identical
/// seeds would produce byte-identical identities (issue #224).
fn seed_history_repo_salted(repo: &Path, salt: &str) -> [String; 3] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(
        repo,
        "src/lib.rs",
        &format!("pub fn original() -> u32 {{ 1 }}\n// fixture salt: {salt}\n"),
    );
    let first = commit(repo, "initial symbol", "2026-01-01T00:00:00Z");

    write(repo, "src/lib.rs", "pub fn renamed() -> u32 { 2 }\n");
    let second = commit(repo, "rename symbol", "2026-01-02T00:00:00Z");

    write(
        repo,
        "src/extra.rs",
        "pub struct Added;\nimpl Added { pub fn value(&self) -> u32 { 3 } }\n",
    );
    let third = commit(repo, "add extra module", "2026-01-03T00:00:00Z");

    [first, second, third]
}

fn seed_two_file_history_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/a.rs", "pub fn a() -> u32 { 1 }\n");
    write(repo, "src/b.rs", "pub fn b() -> u32 { 1 }\n");
    let first = commit(repo, "initial files", "2026-02-01T00:00:00Z");

    write(repo, "src/a.rs", "pub fn a() -> u32 { 2 }\n");
    let second = commit(repo, "change only a", "2026-02-02T00:00:00Z");

    [first, second]
}

fn seed_conflict_resolution_merge_repo(repo: &Path) -> String {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/lib.rs", "pub fn value() -> u32 { 1 }\n");
    commit(repo, "base value", "2026-03-01T00:00:00Z");
    let base_branch = git_output(repo, ["branch", "--show-current"]);

    git(repo, ["checkout", "-b", "feature"]);
    write(repo, "src/lib.rs", "pub fn value() -> u32 { 2 }\n");
    commit(repo, "feature value", "2026-03-02T00:00:00Z");

    git(repo, ["checkout", &base_branch]);
    write(repo, "src/lib.rs", "pub fn value() -> u32 { 3 }\n");
    commit(repo, "main value", "2026-03-03T00:00:00Z");

    let merge = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge", "feature"])
        .stdin(Stdio::null())
        .output()
        .expect("git merge should execute");
    assert!(
        !merge.status.success(),
        "merge should require a conflict resolution\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );

    write(repo, "src/lib.rs", "pub fn value() -> u32 { 4 }\n");
    commit(repo, "merge resolved value", "2026-03-04T00:00:00Z")
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit(repo: &Path, message: &str, date: &str) -> String {
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
    assert!(
        status.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
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

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
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
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}

fn git_root_sha(repo: &Path) -> String {
    git_output(repo, ["rev-list", "--max-parents=0", "HEAD"])
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn count_nodes(records: &[Value], kind: &str) -> usize {
    records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == kind)
        .count()
}

fn assert_edge_label(records: &[Value], label: &str) {
    assert!(
        records
            .iter()
            .any(|record| record["record_type"] == "edge" && record["label"] == label),
        "missing {label} edge"
    );
}

fn changed_in_sources_targeting(records: &[Value], target_id: &str) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["label"] == "CHANGED_IN"
                && record["target"] == target_id
        })
        .filter_map(|record| record["source"].as_str().map(ToOwned::to_owned))
        .collect()
}

fn assert_path_has_changed_source(
    records: &[Value],
    changed_sources: &BTreeSet<String>,
    path: &str,
) {
    assert!(
        path_changed_sources(records, changed_sources, path).count() > 0,
        "expected {path} to be attributed to the changed commit"
    );
}

fn assert_path_has_no_changed_source(
    records: &[Value],
    changed_sources: &BTreeSet<String>,
    path: &str,
) {
    let unexpected = path_changed_sources(records, changed_sources, path)
        .map(|record| format!("{} {}", record["kind"], record["name"]))
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "unchanged path {path} was attributed to the commit: {unexpected:?}"
    );
}

fn path_changed_sources<'a>(
    records: &'a [Value],
    changed_sources: &'a BTreeSet<String>,
    path: &'a str,
) -> impl Iterator<Item = &'a Value> {
    records.iter().filter(move |record| {
        record["record_type"] == "node"
            && matches!(record["kind"].as_str(), Some("File" | "Symbol"))
            && record["repo_relative_path"] == path
            && record["id"]
                .as_str()
                .is_some_and(|id| changed_sources.contains(id))
    })
}

fn assert_commit_node(records: &[Value], sha: &str, valid_time: &str) {
    let candidates = records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Commit")
        .map(|record| {
            format!(
                "{} @ {}",
                record["temporal"]["git_commit"], record["temporal"]["valid_time"]
            )
        })
        .collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Commit"
                && record["temporal"]["git_commit"] == sha
                && record["temporal"]["valid_time"] == valid_time
                && record["temporal"]["author_time"] == valid_time
                && record["author_name"] == "Codegraph Test"
                && record["author_email"] == "codegraph@example.invalid"
        }),
        "missing commit node for {sha}; candidates: {candidates:?}"
    );
}

fn assert_temporal_symbol(records: &[Value], name: &str, sha: &str, valid_time: &str) {
    assert!(
        records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["name"] == name
                && record["temporal"]["git_commit"] == sha
                && record["temporal"]["valid_time"] == valid_time
        }),
        "missing temporal symbol {name} at {sha}"
    );
}

fn assert_temporal_file(records: &[Value], path: &str, sha: &str, valid_time: &str) {
    assert!(
        records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "File"
                && record["repo_relative_path"] == path
                && record["temporal"]["git_commit"] == sha
                && record["temporal"]["valid_time"] == valid_time
        }),
        "missing temporal file {path} at {sha}"
    );
}

/// Issue #444: the history pipeline runs the import-target pass per commit
/// slice, so each `File —IMPORTS→ Module|File` edge carries that commit's
/// temporal provenance (git commit + valid time), exactly like the
/// per-commit CALLS/IMPLEMENTS edges.
#[test]
fn history_replay_stamps_import_target_edges_with_commit_temporal() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(repo, "src/lib.rs", "pub mod b;\n");
    write(repo, "src/b.rs", "pub fn f() {}\n");
    write(repo, "src/consumer.rs", "use crate::b;\n");
    let sha = commit(repo, "add import", "2026-02-01T00:00:00Z");

    let jsonl = scan_repository_history(repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records = parse_jsonl(&jsonl);

    // The target-side edge is the IMPORTS edge whose summary names the
    // importer and the resolved module target ("X imports Y (Rust import …)");
    // the extractor's containment edges ("consumer imports crate::b") carry
    // no `(Rust import …)` marker.
    let target_edges: Vec<&Value> = records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["label"] == "IMPORTS"
                && record["summary"]
                    .as_str()
                    .is_some_and(|summary| summary.contains("(Rust import "))
        })
        .collect();
    assert_eq!(
        target_edges.len(),
        1,
        "history replay should mint exactly one import-target edge"
    );
    let edge = target_edges[0];
    assert_eq!(
        edge["temporal"]["git_commit"], sha,
        "the edge must carry its commit's temporal provenance"
    );
    assert_eq!(
        edge["temporal"]["valid_time"], "2026-02-01T00:00:00Z",
        "the edge must carry its commit's valid time"
    );
}

// ── Issue #224: incrementally update history from new commits ─────────────

use aletheia_egregore::{
    CodegraphError, adapters::records_from_jsonl, history_resume_point,
    scan_repository_history_resumed,
};

/// Seeds `commits` linear commits, each rewriting `src/lib.rs`, dated
/// 2026-01-01 + i days. Returns the commit SHAs oldest-first.
fn seed_linear_repo(repo: &Path, commits: usize) -> Vec<String> {
    seed_linear_repo_salted(repo, commits, "canonical")
}

/// Salts the initial commit's content so two fixture repositories have
/// distinct root commits — and therefore distinct repository identities.
/// Git commit identity does not include the checkout path, so byte-identical
/// seeds would produce byte-identical identities (issue #224).
fn seed_linear_repo_salted(repo: &Path, commits: usize, salt: &str) -> Vec<String> {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    let mut shas = Vec::with_capacity(commits);
    for i in 0..commits {
        let contents = if i == 0 {
            format!("pub fn f{i}() -> u32 {{ {i} }}\n// fixture salt: {salt}\n")
        } else {
            format!("pub fn f{i}() -> u32 {{ {i} }}\n")
        };
        write(repo, "src/lib.rs", &contents);
        let date = format!("2026-01-{:02}T00:00:00Z", i + 1);
        shas.push(commit(repo, &format!("commit {i}"), &date));
    }
    shas
}

fn repository_id_for(repo: &Path) -> String {
    stable_id(&["repository", "local-root-commit", &git_root_sha(repo)])
}

fn full_replay_jsonl(repo: &Path) -> String {
    scan_repository_history(repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize")
}

#[test]
fn full_replay_stamps_history_replay_tip_node() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let shas = seed_history_repo(&repo);
    let head = git_output(&repo, ["rev-parse", "HEAD"]);

    let records = parse_jsonl(&full_replay_jsonl(&repo));

    let tips: Vec<&Value> = records
        .iter()
        .filter(|record| record["kind"] == "HistoryReplayTip")
        .collect();
    assert_eq!(tips.len(), 1, "a full replay stamps exactly one tip node");
    let tip = tips[0];
    let payload = &tip["history_replay_tip"];
    assert_eq!(payload["tip_sha"], head.as_str());
    assert_eq!(payload["covered_commit_count"], 3);
    assert_eq!(payload["tip_committed_at"], "2026-01-03T00:00:00Z");

    // The tip is keyed per repository identity and CONTAINS-attached.
    let repository_id = stable_id(&["repository", "local-root-commit", &shas[0]]);
    assert_eq!(payload["repository_id"], repository_id.as_str());
    let tip_id = stable_id(&["node", "history-replay-tip", &repository_id]);
    assert_eq!(tip["id"], tip_id.as_str());
    assert!(
        records.iter().any(|record| record["record_type"] == "edge"
            && record["label"] == "CONTAINS"
            && record["source"] == repository_id.as_str()
            && record["target"] == tip_id.as_str()),
        "the tip node must be CONTAINS-attached to its Repository"
    );
}

#[test]
fn resume_point_extraction_is_keyed_per_repository_identity() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_a).expect("repo dir should be created");
    fs::create_dir_all(&repo_b).expect("repo dir should be created");
    seed_history_repo_salted(&repo_a, "a");
    seed_history_repo_salted(&repo_b, "b");
    let head_a = git_output(&repo_a, ["rev-parse", "HEAD"]);
    let head_b = git_output(&repo_b, ["rev-parse", "HEAD"]);

    // A multi-repo frontier: both replays concatenated.
    let mut records =
        records_from_jsonl(&full_replay_jsonl(&repo_a)).expect("frontier A should parse");
    records
        .extend(records_from_jsonl(&full_replay_jsonl(&repo_b)).expect("frontier B should parse"));

    let id_a = repository_id_for(&repo_a);
    let id_b = repository_id_for(&repo_b);
    assert_ne!(id_a, id_b, "fixture repos must have distinct identities");

    let point_a = history_resume_point(&records, &id_a).expect("tip for repo A");
    assert_eq!(point_a.repository_id, id_a);
    assert_eq!(point_a.tip_sha, head_a);
    assert_eq!(
        point_a.prior_records.len(),
        records.len(),
        "the merge keeps every frontier record"
    );

    let point_b = history_resume_point(&records, &id_b).expect("tip for repo B");
    assert_eq!(point_b.repository_id, id_b);
    assert_eq!(point_b.tip_sha, head_b);

    assert!(
        history_resume_point(&records, "codegraph:v11:no-such-repo").is_none(),
        "an unknown repository identity has no resume point"
    );
}

#[test]
fn resumed_run_matches_full_replay_byte_for_byte() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 8);

    let frontier_jsonl = full_replay_jsonl(&repo);
    let repository_id = repository_id_for(&repo);
    let prior_records = records_from_jsonl(&frontier_jsonl).expect("frontier should parse");
    let resume = history_resume_point(&prior_records, &repository_id).expect("tip should exist");

    // N new commits land on the M-commit repo.
    for i in 8..11 {
        write(
            &repo,
            "src/lib.rs",
            &format!("pub fn f{i}() -> u32 {{ {i} }}\n"),
        );
        commit(
            &repo,
            &format!("commit {i}"),
            &format!("2026-01-{:02}T00:00:00Z", i + 1),
        );
    }

    let outcome =
        scan_repository_history_resumed(&repo, None, resume).expect("resume should succeed");
    assert_eq!(outcome.processed, 3, "only the new commits are processed");
    assert_eq!(
        outcome.skipped, 8,
        "the already-represented commits are skipped"
    );

    let full_jsonl = full_replay_jsonl(&repo);
    let resumed_jsonl = outcome
        .graph
        .to_jsonl()
        .expect("resumed graph should serialize");
    assert_eq!(
        resumed_jsonl, full_jsonl,
        "a resumed run must be byte-identical to a full replay"
    );

    // The merged graph carries exactly one tip node, at the new HEAD.
    let records = parse_jsonl(&resumed_jsonl);
    let tips: Vec<&Value> = records
        .iter()
        .filter(|record| record["kind"] == "HistoryReplayTip")
        .collect();
    assert_eq!(tips.len(), 1, "the old tip is replaced, not duplicated");
    assert_eq!(
        tips[0]["history_replay_tip"]["tip_sha"],
        git_output(&repo, ["rev-parse", "HEAD"]).as_str()
    );
    assert_eq!(tips[0]["history_replay_tip"]["covered_commit_count"], 11);
}

/// Issue #224, AC4: resuming one repository in a shared multi-repo frontier
/// must leave every other repository's records byte-untouched — only the
/// resumed repository's records get the re-derived producer envelope.
///
/// A naive merge re-stamps the whole graph, which rewrites the untouched
/// repository's producer envelope (`producer_started_at` follows the
/// advancing repository's new HEAD committer date and the language
/// inventory is recomputed over the merged graph). This test pins the
/// promise: B's serialized record lines are identical before and after A
/// is resumed with new commits.
#[test]
fn resumed_run_leaves_other_repositories_records_byte_untouched() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_a).expect("repo dir should be created");
    fs::create_dir_all(&repo_b).expect("repo dir should be created");
    seed_linear_repo_salted(&repo_a, 5, "a");
    seed_linear_repo_salted(&repo_b, 5, "b");

    // A shared multi-repo frontier: both full replays concatenated, the way
    // a multi-repo store holds them.
    let jsonl_a = full_replay_jsonl(&repo_a);
    let jsonl_b = full_replay_jsonl(&repo_b);
    let mut frontier_records = records_from_jsonl(&jsonl_a).expect("frontier A should parse");
    frontier_records.extend(records_from_jsonl(&jsonl_b).expect("frontier B should parse"));

    // B's record IDs and serialized lines, before the resume. Lines are
    // compared as a multiset (sorted Vec): history may legitimately repeat
    // a stable record ID across commits, so a plain ID->line map would
    // silently drop duplicates.
    let b_ids: BTreeSet<String> = parse_jsonl(&jsonl_b)
        .iter()
        .map(|record| {
            record["id"]
                .as_str()
                .expect("every record has an id")
                .to_owned()
        })
        .collect();
    let mut b_lines_before: Vec<String> = jsonl_b.lines().map(str::to_owned).collect();
    b_lines_before.sort();

    // New commits land on A only; B is untouched on disk too.
    let resume = history_resume_point(&frontier_records, &repository_id_for(&repo_a))
        .expect("tip for repo A should exist");
    for i in 5..7 {
        write(
            &repo_a,
            "src/lib.rs",
            &format!("pub fn f{i}() -> u32 {{ {i} }}\n"),
        );
        commit(
            &repo_a,
            &format!("commit {i}"),
            &format!("2026-01-{:02}T00:00:00Z", i + 1),
        );
    }

    let outcome =
        scan_repository_history_resumed(&repo_a, None, resume).expect("resume should succeed");
    assert_eq!(outcome.processed, 2, "only A's new commits are processed");
    assert_eq!(
        outcome.skipped, 5,
        "A's already-represented commits are skipped"
    );

    // B's records in the merged graph: same multiset of serialized lines,
    // byte-for-byte — no drops, no rewrites.
    let merged_jsonl = outcome
        .graph
        .to_jsonl()
        .expect("merged graph should serialize");
    let mut b_lines_after: Vec<String> = merged_jsonl
        .lines()
        .filter(|line| {
            let record: Value =
                serde_json::from_str(line).expect("merged line should parse as JSON");
            record["id"].as_str().is_some_and(|id| b_ids.contains(id))
        })
        .map(str::to_owned)
        .collect();
    b_lines_after.sort();
    assert_eq!(
        b_lines_after, b_lines_before,
        "resuming repo A must leave repo B's records byte-identical"
    );
}

#[test]
fn resumed_run_with_zero_new_commits_is_a_noop() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 5);

    let frontier_jsonl = full_replay_jsonl(&repo);
    let repository_id = repository_id_for(&repo);
    let prior_records = records_from_jsonl(&frontier_jsonl).expect("frontier should parse");
    let resume = history_resume_point(&prior_records, &repository_id).expect("tip should exist");

    // No new commits land.
    let outcome =
        scan_repository_history_resumed(&repo, None, resume).expect("resume should succeed");
    assert_eq!(outcome.processed, 0);
    assert_eq!(outcome.skipped, 5);
    assert_eq!(
        outcome.graph.to_jsonl().expect("graph should serialize"),
        frontier_jsonl,
        "a zero-new-commit resume converges byte-for-byte with the frontier"
    );
}

#[test]
fn resumed_run_detects_history_rewrite() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 5);

    let frontier_jsonl = full_replay_jsonl(&repo);
    let repository_id = repository_id_for(&repo);
    let prior_records = records_from_jsonl(&frontier_jsonl).expect("frontier should parse");
    let resume = history_resume_point(&prior_records, &repository_id).expect("tip should exist");

    // Rewrite history: the stored tip is no longer an ancestor of HEAD.
    git(&repo, ["reset", "--hard", "HEAD~2"]);
    write(&repo, "src/lib.rs", "pub fn diverged() -> u32 { 99 }\n");
    commit(&repo, "diverged history", "2026-02-01T00:00:00Z");

    let error = scan_repository_history_resumed(&repo, None, resume)
        .expect_err("a rewritten history must be detected");
    match error {
        CodegraphError::HistoryResume { code, .. } => {
            assert_eq!(code, "history_rewrite_detected");
        }
        other => panic!("expected a HistoryResume error, got: {other:?}"),
    }
}

#[test]
fn resumed_run_rejects_resume_point_for_another_repository() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_a).expect("repo dir should be created");
    fs::create_dir_all(&repo_b).expect("repo dir should be created");
    seed_linear_repo_salted(&repo_a, 3, "a");
    seed_linear_repo_salted(&repo_b, 3, "b");

    let records_a = records_from_jsonl(&full_replay_jsonl(&repo_a)).expect("frontier should parse");
    // Present repo A's untouched resume point to repo B: the frontier's
    // repository identity does not match the repository being scanned.
    let resume =
        history_resume_point(&records_a, &repository_id_for(&repo_a)).expect("tip should exist");

    let error = scan_repository_history_resumed(&repo_b, None, resume)
        .expect_err("a cross-repository resume point must be rejected");
    match error {
        CodegraphError::HistoryResume { code, .. } => {
            assert_eq!(code, "repository_identity_mismatch");
        }
        other => panic!("expected a HistoryResume error, got: {other:?}"),
    }
}

#[test]
fn scan_history_resume_cli_matches_full_replay_byte_for_byte() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 8);

    let frontier = temp.path().join("frontier.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&frontier)
        .assert()
        .success();

    for i in 8..11 {
        write(
            &repo,
            "src/lib.rs",
            &format!("pub fn f{i}() -> u32 {{ {i} }}\n"),
        );
        commit(
            &repo,
            &format!("commit {i}"),
            &format!("2026-01-{:02}T00:00:00Z", i + 1),
        );
    }

    let resumed = temp.path().join("resumed.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&resumed)
        .arg("--resume-from")
        .arg(&frontier)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("\"processed\":3"))
        .stderr(predicate::str::contains("\"skipped\":8"));

    let full = temp.path().join("full.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&full)
        .assert()
        .success();

    assert_eq!(
        fs::read(&resumed).expect("resumed output should exist"),
        fs::read(&full).expect("full output should exist"),
        "the resumed CLI output must be byte-identical to a full replay"
    );
}

#[test]
fn scan_history_resume_with_window_flag_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 3);

    let frontier = temp.path().join("frontier.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&frontier)
        .assert()
        .success();

    let out = temp.path().join("out.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .arg("--resume-from")
        .arg(&frontier)
        .arg("--max-commits")
        .arg("2")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("resume_with_window"));
    assert!(
        !out.exists(),
        "a rejected resume must not write partial output"
    );
}

#[test]
fn scan_history_resume_without_tip_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 3);

    // A plain `scan` frontier carries no history-replay tip.
    let plain = temp.path().join("plain.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&plain)
        .assert()
        .success();

    let out = temp.path().join("out.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .arg("--resume-from")
        .arg(&plain)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no_resume_point"));
    assert!(
        !out.exists(),
        "a rejected resume must not write partial output"
    );
}

#[test]
fn scan_history_resume_detects_rewrite() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 5);

    let frontier = temp.path().join("frontier.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&frontier)
        .assert()
        .success();

    git(&repo, ["reset", "--hard", "HEAD~2"]);
    write(&repo, "src/lib.rs", "pub fn diverged() -> u32 { 99 }\n");
    commit(&repo, "diverged history", "2026-02-01T00:00:00Z");

    let out = temp.path().join("out.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .arg("--resume-from")
        .arg(&frontier)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("history_rewrite_detected"));
    assert!(
        !out.exists(),
        "a rewrite-detected resume must not write partial output"
    );
}

#[test]
fn scan_history_resume_noop_leaves_frontier_untouched() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 5);

    let frontier = temp.path().join("frontier.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&frontier)
        .assert()
        .success();
    let before = fs::read(&frontier).expect("frontier should exist");

    // Zero new commits: resuming onto the same path is a no-op.
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&frontier)
        .arg("--resume-from")
        .arg(&frontier)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("\"processed\":0"))
        .stderr(predicate::str::contains("\"skipped\":5"));

    assert_eq!(
        fs::read(&frontier).expect("frontier should exist"),
        before,
        "a no-op resume must leave the frontier byte-identical"
    );
}

#[test]
fn scan_history_resume_with_garbage_frontier_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 3);

    let frontier = temp.path().join("frontier.jsonl");
    fs::write(&frontier, "this is not jsonl\n{{{nope").expect("garbage should write");

    let out = temp.path().join("out.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .arg("--resume-from")
        .arg(&frontier)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid_frontier"));
    assert!(
        !out.exists(),
        "an unreadable frontier must not write partial output"
    );
}

#[test]
fn scan_history_resume_with_foreign_frontier_reports_mismatch() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_a).expect("repo dir should be created");
    fs::create_dir_all(&repo_b).expect("repo dir should be created");
    seed_linear_repo_salted(&repo_a, 3, "a");
    seed_linear_repo_salted(&repo_b, 3, "b");

    let frontier_a = temp.path().join("frontier-a.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo_a)
        .arg("--out")
        .arg(&frontier_a)
        .assert()
        .success();

    // Repo A's frontier presented to repo B: the frontier carries a tip, but
    // for another repository identity.
    let out = temp.path().join("out.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo_b)
        .arg("--out")
        .arg(&out)
        .arg("--resume-from")
        .arg(&frontier_a)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("repository_identity_mismatch"));
    assert!(
        !out.exists(),
        "a rejected resume must not write partial output"
    );
}

#[test]
fn scan_history_resume_noop_does_not_create_out() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    seed_linear_repo(&repo, 5);

    let frontier = temp.path().join("frontier.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&frontier)
        .assert()
        .success();

    // Zero new commits and a *different* --out path: the no-op writes nothing.
    let out = temp.path().join("current.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .arg("--resume-from")
        .arg(&frontier)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("\"processed\":0"))
        .stderr(predicate::str::contains("\"skipped\":5"));
    assert!(
        !out.exists(),
        "a no-op resume must not create --out; the frontier already holds the converged records"
    );
}
