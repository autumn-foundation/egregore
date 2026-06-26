#![allow(missing_docs)]

use assert_cmd::Command as CargoCommand;
use predicates::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, PartialEq, Eq, Clone)]
struct GitSnapshot {
    status: String,
    head_sha: Option<String>,
    staged: String,
    stash: String,
    reflog: String,
    files: BTreeMap<String, (String, Option<std::time::SystemTime>)>,
    git_head: String,
    git_refs: BTreeMap<String, String>,
    git_logs: BTreeMap<String, String>,
    orig_head: Option<String>,
    merge_head: Option<String>,
    lock_files: BTreeMap<String, bool>,
}

fn walkdir_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if dir.is_file() {
        files.push(dir.to_path_buf());
        return files;
    }
    let mut dirs_to_visit = vec![dir.to_path_buf()];
    while let Some(current_dir) = dirs_to_visit.pop() {
        if let Ok(entries) = fs::read_dir(current_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default();
                    if name != ".git" {
                        dirs_to_visit.push(path);
                    }
                } else {
                    files.push(path);
                }
            }
        }
    }
    files.sort();
    files
}

fn run_git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed: git {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn run_git_opt(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8(output.stdout).unwrap().trim().to_owned())
    } else {
        None
    }
}

fn take_snapshot(repo: &Path) -> GitSnapshot {
    let status = run_git(repo, &["status", "--porcelain=v2", "--branch"]);
    let head_sha = run_git_opt(repo, &["rev-parse", "HEAD"]);
    let staged = run_git(repo, &["ls-files", "--stage"]);
    let stash = run_git(repo, &["stash", "list"]);
    let reflog = run_git(repo, &["reflog", "-n", "100"]);

    let mut files = BTreeMap::new();
    for file_path in walkdir_files(repo) {
        let relative = file_path
            .strip_prefix(repo)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let content = fs::read(&file_path).unwrap();
        let hash = blake3::hash(&content).to_hex().to_string();
        let mtime = fs::metadata(&file_path)
            .ok()
            .and_then(|m| m.modified().ok());
        files.insert(relative, (hash, mtime));
    }

    let git_dir = repo.join(".git");
    let git_head = if git_dir.join("HEAD").exists() {
        fs::read_to_string(git_dir.join("HEAD"))
            .unwrap()
            .trim()
            .to_owned()
    } else {
        String::new()
    };

    let mut git_refs = BTreeMap::new();
    let refs_dir = git_dir.join("refs");
    if refs_dir.exists() {
        for ref_file in walkdir_files(&refs_dir) {
            let relative = ref_file
                .strip_prefix(&git_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let val = fs::read_to_string(&ref_file)
                .unwrap_or_default()
                .trim()
                .to_owned();
            git_refs.insert(relative, val);
        }
    }

    let mut git_logs = BTreeMap::new();
    let logs_dir = git_dir.join("logs");
    if logs_dir.exists() {
        for log_file in walkdir_files(&logs_dir) {
            let relative = log_file
                .strip_prefix(&git_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let val = fs::read_to_string(&log_file).unwrap_or_default();
            git_logs.insert(relative, val);
        }
    }

    let orig_head = if git_dir.join("ORIG_HEAD").exists() {
        Some(
            fs::read_to_string(git_dir.join("ORIG_HEAD"))
                .unwrap()
                .trim()
                .to_owned(),
        )
    } else {
        None
    };

    let merge_head = if git_dir.join("MERGE_HEAD").exists() {
        Some(
            fs::read_to_string(git_dir.join("MERGE_HEAD"))
                .unwrap()
                .trim()
                .to_owned(),
        )
    } else {
        None
    };

    let mut lock_files = BTreeMap::new();
    for name in &["index.lock", "HEAD.lock", "refs/heads/main.lock"] {
        lock_files.insert(name.to_string(), git_dir.join(name).exists());
    }

    GitSnapshot {
        status,
        head_sha,
        staged,
        stash,
        reflog,
        files,
        git_head,
        git_refs,
        git_logs,
        orig_head,
        merge_head,
        lock_files,
    }
}

fn init_git_repo(repo: &Path) {
    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.email", "safety@example.invalid"]);
    run_git(repo, &["config", "user.name", "Safety Invariant Validator"]);
    run_git(repo, &["config", "core.autocrlf", "false"]);
    run_git(repo, &["config", "commit.gpgsign", "false"]);
}

fn commit_file(repo: &Path, relative: &str, content: &str, msg: &str) {
    let path = repo.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    run_git(repo, &["add", relative]);
    run_git(repo, &["commit", "-m", msg]);
}

fn assert_run_does_not_mutate(repo: &Path, test_scan: bool) {
    let out_dir = tempfile::tempdir().unwrap();
    let out_file = out_dir.path().join("graph.jsonl");

    let snapshot_before = take_snapshot(repo);

    let mut cmd = CargoCommand::cargo_bin("egregore").unwrap();
    if test_scan {
        cmd.arg("scan");
    } else {
        cmd.arg("scan-history");
    }
    cmd.arg(repo).arg("--out").arg(&out_file).assert().success();

    let snapshot_after = take_snapshot(repo);

    assert_eq!(
        snapshot_before,
        snapshot_after,
        "mutation detected after running {}!",
        if test_scan { "scan" } else { "scan-history" }
    );
}

#[test]
fn test_safety_matrix_state_a_clean_branch() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn hello() {}\n", "initial commit");

    assert_run_does_not_mutate(repo, true);
    assert_run_does_not_mutate(repo, false);
}

#[test]
fn test_safety_matrix_state_b_unstaged_edits() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn hello() {}\n", "initial commit");

    // Introduce unstaged edits
    fs::write(repo.join("src/lib.rs"), "pub fn hello_modified() {}\n").unwrap();

    assert_run_does_not_mutate(repo, true);
    assert_run_does_not_mutate(repo, false);
}

#[test]
fn test_safety_matrix_state_c_staged_edits() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn hello() {}\n", "initial commit");

    // Stage modifications
    fs::write(repo.join("src/lib.rs"), "pub fn hello_staged() {}\n").unwrap();
    run_git(repo, &["add", "src/lib.rs"]);

    assert_run_does_not_mutate(repo, true);
    assert_run_does_not_mutate(repo, false);
}

#[test]
fn test_safety_matrix_state_d_untracked_files() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn hello() {}\n", "initial commit");

    // Add untracked file
    fs::write(repo.join("src/untracked.rs"), "pub struct Untracked;\n").unwrap();

    assert_run_does_not_mutate(repo, true);
    assert_run_does_not_mutate(repo, false);
}

#[test]
fn test_safety_matrix_state_e_detached_head() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn hello() {}\n", "first commit");
    let sha = run_git_opt(repo, &["rev-parse", "HEAD"]).unwrap();
    commit_file(repo, "src/extra.rs", "pub fn extra() {}\n", "second commit");

    // Detach HEAD to the first commit
    run_git(repo, &["checkout", &sha]);

    assert_run_does_not_mutate(repo, true);
    assert_run_does_not_mutate(repo, false);
}

#[test]
fn test_safety_matrix_state_f_stash_present() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    init_git_repo(repo);
    commit_file(repo, "src/lib.rs", "pub fn hello() {}\n", "initial commit");

    // Make edit and stash it
    fs::write(repo.join("src/lib.rs"), "pub fn hello_stashed() {}\n").unwrap();
    run_git(repo, &["stash", "-u"]);

    assert_run_does_not_mutate(repo, true);
    assert_run_does_not_mutate(repo, false);
}

#[test]
fn test_scan_history_correctness_under_dirty_states() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    init_git_repo(&repo);
    commit_file(
        &repo,
        "src/lib.rs",
        "pub fn val() -> u32 { 1 }\n",
        "first commit",
    );

    let out_clean = temp.path().join("clean.jsonl");
    let out_dirty = temp.path().join("dirty.jsonl");

    // 1. Scan when clean
    CargoCommand::cargo_bin("egregore")
        .unwrap()
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out_clean)
        .assert()
        .success();

    // 2. Introduce dirty state (unstaged, staged, untracked, stash)
    fs::write(repo.join("src/lib.rs"), "pub fn val() -> u32 { 99 }\n").unwrap();
    run_git(&repo, &["add", "src/lib.rs"]);
    fs::write(repo.join("src/lib.rs"), "pub fn val() -> u32 { 42 }\n").unwrap();
    fs::write(repo.join("src/untracked.rs"), "pub struct U;\n").unwrap();
    run_git(&repo, &["stash", "-u"]);

    // 3. Scan when dirty
    CargoCommand::cargo_bin("egregore")
        .unwrap()
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&out_dirty)
        .assert()
        .success();

    let clean_jsonl = fs::read_to_string(&out_clean).unwrap();
    let dirty_jsonl = fs::read_to_string(&out_dirty).unwrap();

    // Replayed history JSONL must be identical despite dirty working tree state.
    assert_eq!(
        clean_jsonl, dirty_jsonl,
        "scan-history output was corrupted by uncommitted state!"
    );
}

#[test]
fn test_history_replay_precondition_failures() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();

    // 1. Not a git repository
    let out_file = repo.join("graph.jsonl");
    CargoCommand::cargo_bin("egregore")
        .unwrap()
        .arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(&out_file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(r#""code":"not_a_git_repository""#));

    // 2. Git repo but uninitialized history (no commits)
    init_git_repo(repo);
    CargoCommand::cargo_bin("egregore")
        .unwrap()
        .arg("scan-history")
        .arg(repo)
        .arg("--out")
        .arg(&out_file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            r#""code":"git_history_unreadable""#,
        ));
}
