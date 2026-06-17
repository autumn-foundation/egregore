//! Filesystem discovery for local repository scans.

use std::{
    collections::HashSet,
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{
    error::{CodegraphError, Result},
    normalize_path,
};

/// Source file discovered under a repository root.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SourceFile {
    /// Absolute or caller-relative path used for filesystem reads.
    pub path: PathBuf,
    /// Repository-relative portable path used in graph records.
    pub repo_relative_path: String,
}

/// Discovers Rust source files under a repository root.
///
/// # Errors
///
/// Returns an error when directory traversal cannot read an entry or when a
/// discovered source file cannot be relativized against the repository root.
pub fn discover_rust_source_files(repo_root: &Path) -> Result<Vec<SourceFile>> {
    // Pre-compute the set of gitignored directories so traversal can skip them
    // entirely rather than descending into them and failing on unreadable content
    // (PR #186 follow-up).  Purely filesystem-local for non-Git trees.
    let ignored_dirs = if crate::identity::is_repo_root(repo_root) {
        git_ignored_dir_prefixes(repo_root)
    } else {
        HashSet::new()
    };
    let mut files = Vec::new();
    collect_rust_source_files(repo_root, &ignored_dirs, &mut files)?;
    // Respect .gitignore so generated/ignored Rust files are not indexed (issue
    // #82 / PR #186): a git-ignored file has no citable graph spans, and the
    // read-only freshness probe (`git status`, which omits ignored files) then
    // covers exactly the indexed set. No-op outside a Git work tree, preserving
    // the filesystem-local behavior for non-Git trees and determinism for fixtures.
    filter_git_ignored(repo_root, &mut files);
    files.sort_by(|left, right| {
        let left = normalize_path(left);
        let right = normalize_path(right);
        left.cmp(&right)
    });

    files
        .into_iter()
        .map(|path| {
            let repo_relative_path = repo_relative_path(repo_root, &path)?;
            Ok(SourceFile {
                path,
                repo_relative_path,
            })
        })
        .collect()
}

fn collect_rust_source_files(
    directory: &Path,
    ignored_dirs: &HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries = std::fs::read_dir(directory).map_err(|source| CodegraphError::ReadDirectory {
        path: directory.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| CodegraphError::ReadDirectoryEntry {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|source| CodegraphError::InspectPath {
                path: path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            if should_descend(&path) && !ignored_dirs.contains(&path) {
                collect_rust_source_files(&path, ignored_dirs, files)?;
            }
        } else if metadata.is_file() && path.extension() == Some(OsStr::new("rs")) {
            files.push(path);
        }
    }

    Ok(())
}

fn should_descend(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, ".git" | "target"))
    {
        return false;
    }
    // Don't descend into any nested Git working tree, whose files belong to a
    // different repository and are invisible to the superproject's `git status`:
    // - submodules and linked worktrees store `.git` as a FILE (EE1); without this
    //   guard their paths also reach `git check-ignore --stdin` and can trigger a
    //   fatal exit 128 that disables gitignore filtering for the whole scan;
    // - an untracked nested clone (e.g. `vendor/`) has a `.git` DIRECTORY. Git
    //   reports it only as one untracked directory and emits no per-file status, so
    //   indexing its sources would create spans the freshness probe cannot verify
    //   (FFF2 / PR #186 follow-up).
    !path.join(".git").exists()
}

/// Runs `git ls-files --others --ignored --directory --exclude-standard` to
/// obtain the set of gitignored top-level directories.  Returns their absolute
/// paths so callers can skip them during traversal without descending into
/// potentially unreadable or very large subtrees.
///
/// Returns an empty set when Git is unavailable or `repo_root` is not a Git
/// work tree, preserving the filesystem-local behavior for non-Git trees.
fn git_ignored_dir_prefixes(repo_root: &Path) -> HashSet<PathBuf> {
    let Ok(output) = Command::new("git")
        // Override core.excludesFile to suppress user/system-level global gitignore
        // patterns (PR #186 follow-up AA1): scans must be reproducible across
        // different developer environments and must only honour repository-controlled
        // ignore rules (.gitignore, .git/info/exclude), not operator-specific globals.
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_root)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--directory",
            "--exclude-standard",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stderr(Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    if !output.status.success() && output.status.code() != Some(1) {
        return HashSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| repo_root.join(l.trim_end_matches('/')))
        .collect()
}

fn repo_relative_path(repo_root: &Path, path: &Path) -> Result<String> {
    let relative =
        path.strip_prefix(repo_root)
            .map_err(|_| CodegraphError::PathOutsideRepository {
                path: path.to_path_buf(),
                root: repo_root.to_path_buf(),
            })?;
    Ok(normalize_path(relative))
}

/// Removes candidate files that Git ignores at `repo_root` (issue #82 / PR #186).
///
/// Feeds the candidate paths to `git check-ignore` (read-only) and drops any it
/// reports as ignored. A no-op when Git is unavailable or `repo_root` is not in a
/// Git work tree, so non-Git trees keep their pure filesystem-local behavior.
fn filter_git_ignored(repo_root: &Path, files: &mut Vec<PathBuf>) {
    if files.is_empty() {
        return;
    }
    // Gate to the actual Git repository root (PR #186): scanning an in-repo
    // sub-directory (e.g. a test fixture inside a larger checkout) is treated as
    // `no_git` everywhere else, and applying a parent repository's `.gitignore`
    // here would let the surrounding checkout silently drop files from the graph.
    if !crate::identity::is_repo_root(repo_root) {
        return;
    }
    let rels: Vec<String> = files
        .iter()
        .map(|file| {
            file.strip_prefix(repo_root)
                .unwrap_or(file)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let Some(ignored) = git_check_ignored(repo_root, &rels) else {
        return;
    };
    if ignored.is_empty() {
        return;
    }
    let mut kept = Vec::with_capacity(files.len());
    for (file, rel) in files.iter().zip(rels.iter()) {
        if !ignored.contains(rel) {
            kept.push(file.clone());
        }
    }
    *files = kept;
}

/// Runs `git check-ignore -z --stdin` for `rels` under `repo_root`, returning the
/// set of ignored relative paths.
///
/// Returns `None` when Git is unavailable or `repo_root` is not a Git work tree
/// (exit code 128), in which case no filtering is applied. Exit code 1 ("nothing
/// ignored") is a normal, non-error result. The probe is strictly read-only.
fn git_check_ignored(repo_root: &Path, rels: &[String]) -> Option<HashSet<String>> {
    let mut child = Command::new("git")
        // Suppress user/system-level global gitignore (core.excludesFile) so the
        // scan is reproducible across developer environments; only honour
        // repository-controlled rules (.gitignore, .git/info/exclude) (AA1).
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_root)
        .args(["check-ignore", "-z", "--stdin"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Write the NUL-separated paths from a dedicated thread so a large stdin
    // cannot deadlock against a filling stdout pipe.
    let mut stdin = child.stdin.take()?;
    let payload: Vec<u8> = rels.iter().fold(Vec::new(), |mut buf, rel| {
        buf.extend_from_slice(rel.as_bytes());
        buf.push(0);
        buf
    });
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });

    let output = child.wait_with_output().ok();
    let _ = writer.join();
    let output = output?;

    // 128 → not a Git repository / Git error: skip filtering entirely.
    if output.status.code() == Some(128) {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(
        text.split('\0')
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}
