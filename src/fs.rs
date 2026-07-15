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

/// File-level scan-coverage accounting captured during discovery (issue #135).
///
/// Computed over the same walked set discovery visits, so `eg scan` can report
/// how much of the repository it indexed without a second traversal. Excluded
/// directories (`.git`, `target`, nested Git worktrees) are never walked, so
/// they never appear in `files_walked` or `skipped_by_extension` (AC7).
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ScanCoverageTally {
    /// Files the walk visited (never counting excluded-directory files).
    pub files_walked: usize,
    /// Files that matched the indexed-source filter.
    pub files_indexed: usize,
    /// Per-lowercased-extension count of walked-but-not-indexed files
    /// (`""` keys a file with no extension); a sorted map for stable output.
    pub skipped_by_extension: std::collections::BTreeMap<String, usize>,
    /// `true` only on the Git-tracked-files path, which yields a complete
    /// walked/skipped denominator. The non-Git filesystem-walk fallback sets
    /// this `false` (it enumerates only matching files, so it has no
    /// denominator) and never fabricates one.
    pub coverage_complete: bool,
}

/// Discovers supported source files (Rust, Python, TypeScript, Go) under a repository root.
///
/// # Errors
///
/// Returns an error when directory traversal cannot read an entry or when a
/// discovered source file cannot be relativized against the repository root.
pub fn discover_source_files(repo_root: &Path) -> Result<Vec<SourceFile>> {
    Ok(discover_source_files_with_coverage(repo_root)?.0)
}

/// Discovers supported source files together with the file-level scan-coverage
/// tally (issue #135).
///
/// The coverage-returning variant of [`discover_source_files`]: the scan path
/// uses it to stamp a `ScanCoverage` node, while every other caller keeps the
/// tally-discarding [`discover_source_files`] signature.
///
/// # Errors
///
/// Returns an error when directory traversal cannot read an entry or when a
/// discovered source file cannot be relativized against the repository root.
pub fn discover_source_files_with_coverage(
    repo_root: &Path,
) -> Result<(Vec<SourceFile>, ScanCoverageTally)> {
    discover_files_matching(repo_root, &languages::is_supported_source)
}

/// Discovers `Cargo.toml` manifests under a repository root for dependency
/// extraction (issue #180), honoring the same Git-scope and ignore rules as
/// source discovery.
///
/// # Errors
///
/// Returns an error when directory traversal cannot read an entry or when a
/// discovered manifest cannot be relativized against the repository root.
pub fn discover_cargo_manifests(repo_root: &Path) -> Result<Vec<SourceFile>> {
    Ok(discover_files_matching(repo_root, &is_cargo_manifest)?.0)
}

fn is_cargo_manifest(path: &Path) -> bool {
    path.file_name().and_then(OsStr::to_str) == Some("Cargo.toml")
}

fn discover_files_matching(
    repo_root: &Path,
    matcher: &dyn Fn(&Path) -> bool,
) -> Result<(Vec<SourceFile>, ScanCoverageTally)> {
    let mut files = Vec::new();
    let mut tally = ScanCoverageTally::default();

    if crate::identity::is_repo_root(repo_root) {
        if let Some(tracked) = git_tracked_files(repo_root) {
            for path in tracked {
                let is_file = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
                if !is_file {
                    continue;
                }
                let Ok(rel) = repo_relative_path(repo_root, &path) else {
                    continue;
                };
                // Excluded directories (issue #135 AC7): files under `.git`,
                // `target`, or a nested Git worktree are never walked, so they
                // never dilute the coverage denominator or the skip tally.
                let has_target_or_git = rel
                    .split('/')
                    .any(|part| part == "target" || part == ".git");
                if has_target_or_git {
                    continue;
                }
                // The tracked-files walk visits every non-excluded file, so it
                // yields a complete indexed-vs-skipped accounting (AC4).
                tally.files_walked += 1;
                if matcher(&path) {
                    files.push(path);
                } else {
                    let ext = path
                        .extension()
                        .and_then(OsStr::to_str)
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_default();
                    *tally.skipped_by_extension.entry(ext).or_default() += 1;
                }
            }
            tally.coverage_complete = true;
        } else {
            // Git command failed, fallback
            let ignored_dirs = git_ignored_dir_prefixes(repo_root);
            collect_matching_files(repo_root, matcher, &ignored_dirs, &mut files)?;
            filter_git_ignored(repo_root, &mut files);
        }
    } else {
        // Fallback to pure filesystem walk for non-Git trees
        let ignored_dirs = HashSet::new();
        collect_matching_files(repo_root, matcher, &ignored_dirs, &mut files)?;
    }

    // `files_indexed` is exactly the count of matched files collected, on both
    // the tracked-files walk and the fallback branches, so set it once here
    // rather than track a parallel counter (issue #135).
    tally.files_indexed = files.len();
    // The fallback branches (non-Git tree, or a failed `git ls-files`) enumerate
    // only matching files, so they carry no walked/skipped denominator. Report a
    // best-effort `files_walked == files_indexed` with `coverage_complete` left
    // `false` rather than fabricate a skip tally (issue #135).
    if !tally.coverage_complete {
        tally.files_walked = files.len();
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
    Ok((source_files, tally))
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
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
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for chunk in output.stdout.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let rel_path = bytes_to_path(chunk);
        let path = repo_root.join(rel_path);
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Some(paths)
}

fn collect_matching_files(
    directory: &Path,
    matcher: &dyn Fn(&Path) -> bool,
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
                collect_matching_files(&path, matcher, ignored_dirs, files)?;
            }
        } else if file_type.is_file() && matcher(&path) {
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
    let mut dirs = HashSet::new();
    for chunk in output.stdout.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let rel_path = bytes_to_path(chunk);
        let mut path = repo_root.to_path_buf();
        for component in rel_path.components() {
            if let std::path::Component::Normal(part) = component {
                path.push(part);
            }
        }
        dirs.insert(path);
    }
    dirs
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
    let rels: Vec<PathBuf> = files
        .iter()
        .map(|file| {
            let rel = file.strip_prefix(repo_root).unwrap_or(file);
            rel.components().collect()
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
fn git_check_ignored(repo_root: &Path, rels: &[PathBuf]) -> Option<HashSet<PathBuf>> {
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
    let mut payload = Vec::new();
    for rel in rels {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            payload.extend_from_slice(rel.as_os_str().as_bytes());
        }
        #[cfg(not(unix))]
        {
            payload.extend_from_slice(rel.to_string_lossy().as_bytes());
        }
        payload.push(0);
    }
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
    let mut ignored = HashSet::new();
    for chunk in output.stdout.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let path = bytes_to_path(chunk);
        ignored.insert(path.components().collect());
    }
    Some(ignored)
}
