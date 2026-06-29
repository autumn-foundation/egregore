//! Filesystem discovery for local repository scans.

use std::{
    collections::HashSet,
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::languages;

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

/// Discovers supported source files (Rust, Python, TypeScript, Go) under a repository root.
///
/// # Errors
///
/// Returns an error when directory traversal cannot read an entry or when a
/// discovered source file cannot be relativized against the repository root.
pub fn discover_source_files(repo_root: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();

    if crate::identity::is_repo_root(repo_root) {
        if let Some(tracked) = git_tracked_files(repo_root) {
            for path in tracked {
                if !path.is_file() || !languages::is_supported_source(&path) {
                    continue;
                }
                let Ok(rel) = repo_relative_path(repo_root, &path) else {
                    continue;
                };
                let has_target_or_git = rel.split('/').any(|part| part == "target" || part == ".git");
                if !has_target_or_git {
                    files.push(path);
                }
            }

            // Report skipped files
            let (skipped_rust_count, skipped_python_count, skipped_typescript_count, skipped_go_count) =
                git_count_skipped_files(repo_root).unwrap_or((0, 0, 0, 0));

            eprintln!(
                "Skipped {skipped_rust_count} .rs, {skipped_python_count} .py, {skipped_typescript_count} .ts/.tsx, {skipped_go_count} .go files by ignore rules"
            );
        } else {
            // Git command failed, fallback
            let ignored_dirs = git_ignored_dir_prefixes(repo_root);
            collect_source_files(repo_root, &ignored_dirs, &mut files)?;
            filter_git_ignored(repo_root, &mut files);
        }
    } else {
        // Fallback to pure filesystem walk for non-Git trees
        let ignored_dirs = HashSet::new();
        collect_source_files(repo_root, &ignored_dirs, &mut files)?;
    }

    let mut source_files: Vec<SourceFile> = files
        .into_iter()
        .map(|path| {
            let repo_relative_path = repo_relative_path(repo_root, &path)?;
            Ok(SourceFile {
                path,
                repo_relative_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    source_files.sort_by(|left, right| left.repo_relative_path.cmp(&right.repo_relative_path));
    Ok(source_files)
}

fn git_tracked_files(repo_root: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["-c", "core.excludesFile="])
        .args(["-c", "core.quotePath=false"])
        .arg("-C")
        .arg(repo_root)
        .args(["ls-files", "-z"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for line in text.split('\0') {
        if line.is_empty() {
            continue;
        }
        let path = repo_root.join(line);
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Some(paths)
}

fn git_count_skipped_files(repo_root: &Path) -> Option<(usize, usize, usize, usize)> {
    let mut skipped_rust = 0;
    let mut skipped_python = 0;
    let mut skipped_typescript = 0;
    let mut skipped_go = 0;

    let mut process_output = |args: &[&str]| -> Option<()> {
        let output = Command::new("git")
            .args(["-c", "core.excludesFile="])
            .args(["-c", "core.quotePath=false"])
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.split('\0') {
            if line.is_empty() {
                continue;
            }
            let has_target_or_git = line.split('/').any(|part| part == "target" || part == ".git");
            if has_target_or_git {
                continue;
            }
            if let Some(ext) = Path::new(line).extension().and_then(OsStr::to_str) {
                if ext.eq_ignore_ascii_case("rs") {
                    skipped_rust += 1;
                } else if ext.eq_ignore_ascii_case("py") {
                    skipped_python += 1;
                } else if ext.eq_ignore_ascii_case("ts") || ext.eq_ignore_ascii_case("tsx") {
                    skipped_typescript += 1;
                } else if ext.eq_ignore_ascii_case("go") {
                    skipped_go += 1;
                }
            }
        }
        Some(())
    };

    process_output(&["ls-files", "--others", "--exclude-standard", "-z"])?;
    process_output(&["ls-files", "--others", "--ignored", "--exclude-standard", "-z"])?;

    Some((skipped_rust, skipped_python, skipped_typescript, skipped_go))
}

fn collect_source_files(
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
        let file_type = entry
            .file_type()
            .map_err(|source| CodegraphError::InspectPath {
                path: path.clone(),
                source,
            })?;

        if file_type.is_dir() {
            if should_descend(&path) && !ignored_dirs.contains(&path) {
                collect_source_files(&path, ignored_dirs, files)?;
            }
        } else if file_type.is_file() && languages::is_supported_source(&path) {
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

/// Runs `git ls-files --others --ignored --directory --exclude-standard -z` to
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
        .args(["-c", "core.quotePath=false"])
        .arg("-C")
        .arg(repo_root)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--directory",
            "--exclude-standard",
            "-z",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split('\0')
        .filter(|line| !line.is_empty())
        .map(|l| {
            let clean_rel = l.trim_end_matches('/');
            let mut path = repo_root.to_path_buf();
            for part in clean_rel.split('/') {
                path.push(part);
            }
            path
        })
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
            let rel = file.strip_prefix(repo_root).unwrap_or(file);
            normalize_path(rel)
        })
        .collect();
    let Some(ignored) = git_check_ignored(repo_root, &rels) else {
        return;
    };
    if ignored.is_empty() {
        return;
    }
    let mut idx = 0;
    files.retain(|_| {
        let keep = !ignored.contains(&rels[idx]);
        idx += 1;
        keep
    });
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
        .args(["-c", "core.quotePath=false"])
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

    let code = output.status.code();
    if code != Some(0) && code != Some(1) {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.split('\0')
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}
