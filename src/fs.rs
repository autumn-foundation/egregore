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
    let mut files = Vec::new();
    collect_rust_source_files(repo_root, &mut files)?;
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

fn collect_rust_source_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
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
            if should_descend(&path) {
                collect_rust_source_files(&path, files)?;
            }
        } else if metadata.is_file() && path.extension() == Some(OsStr::new("rs")) {
            files.push(path);
        }
    }

    Ok(())
}

fn should_descend(path: &Path) -> bool {
    !path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, ".git" | "target"))
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
