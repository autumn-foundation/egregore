//! Read-only working-tree diff computation (issue #214).
//!
//! Computes the set of changed files between `HEAD` and the working tree using
//! Git plumbing only, and parses unified-diff hunk headers into changed line
//! ranges (worktree/new-file terms) so callers can intersect recorded symbol
//! spans with the diff without re-reading file contents.
//!
//! Strictly read-only: every Git invocation runs with `GIT_OPTIONAL_LOCKS=0`
//! (Git never refreshes the index) and `-c core.excludesFile=` (probes see the
//! same file set regardless of user-level gitignore), mirroring
//! [`crate::identity`]'s freshness probes. No checkout, stash, reset, or write
//! is ever performed.

use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};

/// A 1-based inclusive `(start, end)` line range.
type LineRange = (u32, u32);

/// One parsed file's hunks: per-hunk old-file ranges, optional new-file
/// ranges (`None` for pure-deletion hunks), and raw `(old_count, new_count)`
/// line counts for changed-line accounting.
struct ParsedFileHunks {
    old: Vec<LineRange>,
    new: Vec<Option<LineRange>>,
    counts: Vec<(u32, u32)>,
}

/// One file's parsed hunks together with its repo-relative path.
struct FileHunks {
    path: String,
    hunks: ParsedFileHunks,
}

/// The parsed ranges of one hunk header: the old-file range, the optional
/// new-file range (`None` for pure-deletion hunks), and the raw
/// `(old_count, new_count)` line counts.
struct HunkRanges {
    old: LineRange,
    new: Option<LineRange>,
    counts: (u32, u32),
}

/// How a path changed between `HEAD` and the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStatus {
    /// Staged new file.
    Added,
    /// Tracked file with content changes.
    Modified,
    /// Tracked file deleted from the working tree.
    Deleted,
    /// Renamed; carries the old repo-relative path.
    Renamed {
        /// Repo-relative path on the `HEAD` side.
        from: String,
    },
    /// Present in the working tree but unknown to Git (never staged).
    Untracked,
}

impl PathStatus {
    /// Stable `snake_case` status string for JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed { .. } => "renamed",
            Self::Untracked => "untracked",
        }
    }
}

/// One changed path plus its changed line ranges on both sides of the diff.
///
/// Ranges are 1-based inclusive `(start, end)` pairs. `new_ranges` are in
/// worktree (new-file) terms; `old_ranges` are in `HEAD` (old-file) terms —
/// the recorded graph spans describe the indexed `HEAD` version, so symbol
/// intersection uses `old_ranges`. Pure-deletion hunks contribute only an old
/// range; insertion-only hunks (`old_count == 0`) contribute a bounded anchor
/// `(old_start, old_start + 1)` around the insertion point so an insertion
/// inside an existing symbol still matches it. Untracked files carry the
/// whole-file range in `new_ranges` and no old ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// Repo-relative path on the worktree side (`to` side for renames).
    pub path: String,
    /// How the path changed.
    pub status: PathStatus,
    /// Changed line ranges as 1-based inclusive `(start, end)` pairs in the
    /// worktree file. Empty for deletions; whole-file for untracked files.
    pub new_ranges: Vec<(u32, u32)>,
    /// Changed line ranges as 1-based inclusive `(start, end)` pairs in the
    /// `HEAD` file. Empty for untracked files.
    pub old_ranges: Vec<(u32, u32)>,
    /// Total changed lines: per hunk, the new-side line count plus the
    /// old-side line count for pure-deletion hunks; the whole-file line count
    /// for untracked files.
    pub changed_lines: u64,
    /// Number of hunks (1 for an untracked file, 0 for an empty one).
    pub hunk_count: usize,
}

/// The working-tree diff vs `HEAD`; paths sorted ascending for determinism.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingTreeDiff {
    /// Changed paths, sorted by path.
    pub paths: Vec<ChangedPath>,
}

/// Errors from working-tree diff computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffError {
    /// `repo_root` is not the root of a Git repository.
    NotAGitRepo {
        /// The path that failed the repository-root check.
        path: String,
    },
    /// The repository has no commits yet, so there is no `HEAD` to diff against.
    UnbornHead,
    /// A Git plumbing command failed.
    GitFailed {
        /// Human-readable description of the failure.
        message: String,
    },
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAGitRepo { path } => write!(f, "not a git repository root: {path}"),
            Self::UnbornHead => write!(f, "repository has no commits yet (unborn HEAD)"),
            Self::GitFailed { message } => write!(f, "git command failed: {message}"),
        }
    }
}

impl std::error::Error for DiffError {}

/// Computes the working-tree diff vs `HEAD` using read-only Git plumbing.
///
/// With `staged_only == false` the diff combines staged and unstaged changes
/// vs `HEAD` (`git diff HEAD`) and additionally reports untracked
/// (non-ignored) files; with `staged_only == true` only staged changes are
/// reported (`git diff --cached HEAD`) and untracked files are excluded.
///
/// # Errors
///
/// Returns [`DiffError::NotAGitRepo`] when `repo_root` is not a Git repository
/// root, [`DiffError::UnbornHead`] when the repository has no commits, and
/// [`DiffError::GitFailed`] when a plumbing command fails.
pub fn working_tree_diff(
    repo_root: &Path,
    staged_only: bool,
) -> Result<WorkingTreeDiff, DiffError> {
    validate_repo_root(repo_root)?;
    check_head_exists(repo_root)?;

    let mut paths = tracked_changes(repo_root, staged_only)?;
    if !staged_only {
        append_untracked_files(repo_root, &mut paths)?;
    }

    // Deterministic canonical ordering.
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WorkingTreeDiff { paths })
}

/// Verifies `repo_root` is itself a Git repository root (not a subdirectory
/// or a non-repo path), so diff paths are always repo-relative.
fn validate_repo_root(repo_root: &Path) -> Result<(), DiffError> {
    // Exact repository-root check: `repo_root` itself must be the toplevel, so
    // callers cannot accidentally diff a subdirectory (which would yield
    // worktree-relative paths) or a non-repo path.
    let toplevel = read_only_git(repo_root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|err| DiffError::GitFailed {
            message: format!("git rev-parse --show-toplevel failed: {err}"),
        })?;
    if !toplevel.status.success() {
        return Err(DiffError::NotAGitRepo {
            path: repo_root.to_string_lossy().into_owned(),
        });
    }
    let toplevel = String::from_utf8_lossy(&toplevel.stdout).trim().to_owned();
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|err| DiffError::GitFailed {
            message: format!("cannot canonicalize repo root: {err}"),
        })?;
    let canonical_toplevel =
        Path::new(&toplevel)
            .canonicalize()
            .map_err(|err| DiffError::GitFailed {
                message: format!("cannot canonicalize git toplevel: {err}"),
            })?;
    if canonical_root != canonical_toplevel {
        return Err(DiffError::NotAGitRepo {
            path: repo_root.to_string_lossy().into_owned(),
        });
    }
    Ok(())
}

/// Verifies `HEAD` exists: there is nothing to diff against on an unborn HEAD.
fn check_head_exists(repo_root: &Path) -> Result<(), DiffError> {
    let head = read_only_git(repo_root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map_err(|err| DiffError::GitFailed {
            message: format!("git rev-parse --verify HEAD failed: {err}"),
        })?;
    if !head.status.success() {
        return Err(DiffError::UnbornHead);
    }
    Ok(())
}

/// Runs one read-only `git` diff plumbing command, mapping spawn and
/// non-zero-exit failures into [`DiffError::GitFailed`].
fn run_read_only_diff(
    repo_root: &Path,
    args: &[&str],
    what: &str,
) -> Result<std::process::Output, DiffError> {
    let out = read_only_git(repo_root)
        .args(args)
        .output()
        .map_err(|err| DiffError::GitFailed {
            message: format!("git {what} failed: {err}"),
        })?;
    if !out.status.success() {
        return Err(DiffError::GitFailed {
            message: format!(
                "git {what} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(out)
}

/// Computes the tracked-file changes: name-status for the authoritative path
/// list, then a unified diff for per-hunk ranges on both sides.
/// `--find-renames` keeps renames as one entry on both passes.
fn tracked_changes(repo_root: &Path, staged_only: bool) -> Result<Vec<ChangedPath>, DiffError> {
    // Options come before the `HEAD` revision, which is the only form
    // `git diff` documents.
    let mut status_args = vec![
        "diff",
        "--name-status",
        "-z",
        "--find-renames",
        "--no-ext-diff",
    ];
    let mut hunk_args = vec!["diff", "--unified=0", "--no-ext-diff", "--find-renames"];
    if staged_only {
        status_args.push("--cached");
        hunk_args.push("--cached");
    }
    status_args.push("HEAD");
    hunk_args.push("HEAD");
    let status_out = run_read_only_diff(repo_root, &status_args, "diff --name-status")?;
    let hunk_out = run_read_only_diff(repo_root, &hunk_args, "diff --unified=0")?;
    let hunk_text = String::from_utf8_lossy(&hunk_out.stdout);
    let hunks = parse_unified_hunk_ranges(&hunk_text);

    let mut paths: Vec<ChangedPath> = Vec::new();
    for (status, path, old_path) in parse_name_status(&status_out.stdout) {
        let path_status = match status {
            'A' => PathStatus::Added,
            'M' => PathStatus::Modified,
            'D' => PathStatus::Deleted,
            'R' => PathStatus::Renamed {
                from: old_path.unwrap_or_default(),
            },
            other => {
                return Err(DiffError::GitFailed {
                    message: format!("unexpected name-status code {other:?} for {path}"),
                });
            }
        };
        let (old_ranges, new_ranges, changed_lines, hunk_count) = hunk_summary_for(&hunks, &path);
        paths.push(ChangedPath {
            path,
            status: path_status,
            new_ranges,
            old_ranges,
            changed_lines,
            hunk_count,
        });
    }
    Ok(paths)
}

/// Appends untracked (non-ignored) files as whole-file changes. Only used in
/// full mode: staged-only is strictly the index-vs-HEAD delta.
fn append_untracked_files(repo_root: &Path, paths: &mut Vec<ChangedPath>) -> Result<(), DiffError> {
    let others = run_read_only_diff(
        repo_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        "ls-files --others",
    )?;
    for raw in others.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = String::from_utf8_lossy(raw).into_owned();
        // Skip anything the tracked pass already reported (defensive;
        // ls-files --others never overlaps name-status, but paths with
        // unusual bytes could parse differently on the two passes).
        if paths.iter().any(|p| p.path == path) {
            continue;
        }
        let line_count = count_lines(&repo_root.join(&path));
        let (new_ranges, hunk_count) = if line_count == 0 {
            (Vec::new(), 0)
        } else {
            (vec![(1, line_count)], 1)
        };
        paths.push(ChangedPath {
            path,
            status: PathStatus::Untracked,
            new_ranges,
            old_ranges: Vec::new(),
            changed_lines: u64::from(line_count),
            hunk_count,
        });
    }
    Ok(())
}

/// Looks up the hunk ranges for `path` in parsed unified-diff output and
/// summarizes them into `(old_ranges, new_ranges, changed_lines, hunk_count)`.
fn hunk_summary_for(
    hunks: &[FileHunks],
    path: &str,
) -> (Vec<LineRange>, Vec<LineRange>, u64, usize) {
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    let mut changed_lines: u64 = 0;
    let mut hunk_count = 0;
    for file in hunks {
        if file.path != path {
            continue;
        }
        for (i, old_range) in file.hunks.old.iter().enumerate() {
            let (old_count, new_count) = file.hunks.counts[i];
            old_ranges.push(*old_range);
            if let Some(new_range) = file.hunks.new[i] {
                new_ranges.push(new_range);
            }
            changed_lines += u64::from(new_count)
                + if new_count == 0 {
                    u64::from(old_count)
                } else {
                    0
                };
            hunk_count += 1;
        }
    }
    (old_ranges, new_ranges, changed_lines, hunk_count)
}

/// Counts lines in a file via buffered line iteration. Missing or unreadable
/// files count as zero lines.
fn count_lines(path: &Path) -> u32 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    let count = std::io::BufReader::new(file).lines().count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Parses `git diff --name-status -z` output into `(status, path, old_path)`
/// triples. `old_path` is `Some` only for renames.
fn parse_name_status(output: &[u8]) -> Vec<(char, String, Option<String>)> {
    let mut entries = Vec::new();
    let mut fields = output.split(|b| *b == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let status = field[0] as char;
        // Rename/copy entries carry the similarity score in the status field
        // (`R100`) and are followed by old-path then new-path.
        let is_rename = status == 'R' || status == 'C';
        let first = fields.next().unwrap_or_default();
        if is_rename {
            let second = fields.next().unwrap_or_default();
            entries.push((
                'R',
                String::from_utf8_lossy(second).into_owned(),
                Some(String::from_utf8_lossy(first).into_owned()),
            ));
        } else {
            entries.push((status, String::from_utf8_lossy(first).into_owned(), None));
        }
    }
    entries
}

/// Parses a unified diff (from `git diff -U0`) into per-file changed line
/// ranges in new-file terms: `(path, [(start, end), ...])` with 1-based
/// inclusive ranges. Pure-deletion hunks contribute no range.
///
/// Test-only: production uses [`parse_unified_hunk_ranges`], which also
/// returns old-file ranges for `HEAD`-span intersection.
#[cfg(test)]
fn parse_unified_hunks(text: &str) -> Vec<(String, Vec<LineRange>)> {
    parse_unified_hunk_ranges(text)
        .into_iter()
        .map(|file| {
            let new_ranges: Vec<LineRange> = file.hunks.new.into_iter().flatten().collect();
            (file.path, new_ranges)
        })
        .collect()
}

/// Extracts the new-side path from a `diff --git <old> <new>` header line
/// (the part after the `diff --git ` prefix). Handles quoted paths that
/// contain spaces: git emits `"a/<old>" "b/<new>"` when quoting is on; with
/// `core.quotePath=false` paths are literal and space-free.
fn parse_diff_git_new_path(rest: &str) -> String {
    let path = rest.strip_prefix('"').map_or_else(
        || rest.rsplit_once(' ').map_or(rest, |(_, b)| b),
        |quoted| {
            // `"a/<old>" "b/<new>"` — split on the `" "b/` separator.
            quoted
                .find("\" \"b/")
                .map_or(rest, |idx| &quoted[idx + 5..quoted.len().saturating_sub(1)])
        },
    );
    path.strip_prefix("b/").unwrap_or(path).to_owned()
}

/// Parses a unified diff into per-file [`FileHunks`]: for hunk `i`,
/// `hunks.old[i]` is the old-file range (a bounded `(old_start, old_start + 1)`
/// anchor for insertion-only hunks), `hunks.new[i]` is the `Some` new-file
/// range except for pure-deletion hunks, and `hunks.counts[i]` is
/// `(old_count, new_count)`.
fn parse_unified_hunk_ranges(text: &str) -> Vec<FileHunks> {
    let mut files: Vec<FileHunks> = Vec::new();
    let mut current: Option<usize> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // The new side is the merge key and matches name-status's new path
            // (quotePath=false keeps it literal).
            let path = parse_diff_git_new_path(rest);
            // Reuse the existing entry when git repeats a file (defensive).
            current = files.iter().position(|f| f.path == path).or_else(|| {
                files.push(FileHunks {
                    path,
                    hunks: ParsedFileHunks {
                        old: Vec::new(),
                        new: Vec::new(),
                        counts: Vec::new(),
                    },
                });
                Some(files.len() - 1)
            });
        } else if line.strip_prefix("@@ ").is_some() {
            let Some(idx) = current else { continue };
            // Pass the full line: the header parser expects the `@@ ` prefix.
            if let Some(ranges) = parse_hunk_header_both(line) {
                let hunks = &mut files[idx].hunks;
                hunks.old.push(ranges.old);
                hunks.new.push(ranges.new);
                hunks.counts.push(ranges.counts);
            }
        }
    }
    files
}

/// Parses one `@@ -old_start[,old_count] +new_start[,new_count] @@` hunk header
/// into the new-file `(start, end)` range, or `None` for a pure-deletion hunk.
///
/// Test-only: production uses [`parse_hunk_header_both`].
#[cfg(test)]
fn parse_hunk_header(header: &str) -> Option<LineRange> {
    parse_hunk_header_both(header).and_then(|ranges| ranges.new)
}

/// Parses one `@@ -old_start[,old_count] +new_start[,new_count] @@` hunk header
/// into [`HunkRanges`]. The old range is a bounded `(old_start, old_start + 1)`
/// anchor for insertion-only hunks (`old_count == 0`); the new range is `None`
/// for pure-deletion hunks. Returns `None` when the header does not parse.
fn parse_hunk_header_both(header: &str) -> Option<HunkRanges> {
    // Header shape: `@@ -old_start[,old_count] +new_start[,new_count] @@ ...`
    let inner = header.strip_prefix("@@ ")?;
    let (ranges, _) = inner.split_once(" @@")?;
    let mut parts = ranges.split_whitespace();
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_range_part(old_part)?;
    let (new_start, new_count) = parse_range_part(new_part)?;
    let old = if old_count == 0 {
        // Insertion-only: bounded anchor around the insertion point so an
        // insertion inside an existing symbol still intersects its span.
        (old_start, old_start + 1)
    } else {
        (old_start, old_start + old_count - 1)
    };
    let new = if new_count == 0 {
        None
    } else {
        Some((new_start, new_start + new_count - 1))
    };
    Some(HunkRanges {
        old,
        new,
        counts: (old_count, new_count),
    })
}

/// Parses one `start[,count]` hunk range part; a missing count means 1.
fn parse_range_part(part: &str) -> Option<LineRange> {
    match part.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((part.parse().ok()?, 1)),
    }
}

/// Builds a `git` command rooted at `repo_root` that never writes the index.
///
/// Mirrors [`crate::identity`]'s private builder: `GIT_OPTIONAL_LOCKS=0`
/// disables the optional index-refresh write, keeping the probe strictly
/// read-only.
fn read_only_git(repo_root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.excludesFile="])
        .args(["-c", "core.quotePath=false"])
        .arg("-C")
        .arg(repo_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Run git in the fixture root. Panics on failure; used only in tests.
    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(Stdio::null())
            .status()
            .expect("git should run in tests");
        assert!(status.success(), "git {args:?} failed in test");
    }

    #[test]
    fn name_status_parses_added_modified_deleted() {
        let raw = b"A\0src/new.rs\0M\0src/changed.rs\0D\0src/gone.rs\0";
        let parsed = parse_name_status(raw);
        assert_eq!(
            parsed,
            vec![
                ('A', "src/new.rs".to_owned(), None),
                ('M', "src/changed.rs".to_owned(), None),
                ('D', "src/gone.rs".to_owned(), None),
            ]
        );
    }

    #[test]
    fn name_status_parses_rename_with_old_path() {
        let raw = b"R100\0src/old.rs\0src/new.rs\0";
        let parsed = parse_name_status(raw);
        assert_eq!(
            parsed,
            vec![('R', "src/new.rs".to_owned(), Some("src/old.rs".to_owned()))]
        );
    }

    #[test]
    fn hunk_header_parses_new_file_range() {
        // `@@ -old_start,old_count +new_start,new_count @@`
        assert_eq!(parse_hunk_header("@@ -0,0 +1,12 @@"), Some((1, 12)));
        assert_eq!(parse_hunk_header("@@ -5,3 +7,4 @@"), Some((7, 10)));
        // Count omitted means 1.
        assert_eq!(parse_hunk_header("@@ -5 +7 @@"), Some((7, 7)));
    }

    #[test]
    fn hunk_header_pure_deletion_yields_no_range() {
        assert_eq!(parse_hunk_header("@@ -5,3 +4,0 @@"), None);
    }

    #[test]
    fn unified_hunks_groups_ranges_per_file() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n\
            index 1111111..2222222 100644\n\
            --- a/src/a.rs\n\
            +++ b/src/a.rs\n\
            @@ -3,2 +3,3 @@\n\
             ctx\n\
            +new line\n\
             ctx2\n\
            diff --git a/src/b.rs b/src/b.rs\n\
            --- a/src/b.rs\n\
            +++ b/src/b.rs\n\
            @@ -10 +12,2 @@\n\
            +x\n\
            +y\n";
        let hunks = parse_unified_hunks(diff);
        assert_eq!(
            hunks,
            vec![
                ("src/a.rs".to_owned(), vec![(3, 5)]),
                ("src/b.rs".to_owned(), vec![(12, 13)]),
            ]
        );
    }

    #[test]
    fn unified_hunks_skips_pure_deletion_hunks() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n\
            --- a/src/a.rs\n\
            +++ b/src/a.rs\n\
            @@ -5,3 +4,0 @@\n\
            -gone\n";
        let hunks = parse_unified_hunks(diff);
        assert_eq!(hunks, vec![("src/a.rs".to_owned(), vec![])]);
    }

    #[test]
    fn unified_hunks_handles_deleted_file() {
        let diff = "diff --git a/src/gone.rs b/src/gone.rs\n\
            deleted file mode 100644\n\
            --- a/src/gone.rs\n\
            +++ /dev/null\n\
            @@ -1,4 +0,0 @@\n\
            -a\n";
        let hunks = parse_unified_hunks(diff);
        assert_eq!(hunks, vec![("src/gone.rs".to_owned(), vec![])]);
    }

    #[test]
    fn diff_on_non_repo_returns_not_a_git_repo() {
        let dir = tempfile::tempdir().expect("temp dir");
        let err = working_tree_diff(dir.path(), false).expect_err("non-repo must fail");
        assert_eq!(
            err,
            DiffError::NotAGitRepo {
                path: dir.path().to_string_lossy().into_owned()
            }
        );
    }

    #[test]
    fn diff_on_repo_without_commits_returns_unborn_head() {
        let dir = tempfile::tempdir().expect("temp dir");
        let status = Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .stdin(Stdio::null())
            .status()
            .expect("git init");
        assert!(status.success());
        let err = working_tree_diff(dir.path(), false).expect_err("unborn HEAD must fail");
        assert_eq!(err, DiffError::UnbornHead);
    }

    #[test]
    fn diff_reports_modified_file_with_hunk_ranges() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "test"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        // Modify line 2 (unstaged).
        std::fs::write(root.join("a.txt"), "one\nTWO\nthree\n").expect("write");
        let diff = working_tree_diff(root, false).expect("diff");
        assert_eq!(diff.paths.len(), 1);
        assert_eq!(diff.paths[0].path, "a.txt");
        assert_eq!(diff.paths[0].status, PathStatus::Modified);
        assert_eq!(diff.paths[0].new_ranges, vec![(2, 2)]);
    }

    #[test]
    fn diff_staged_only_excludes_unstaged_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "test"]);
        std::fs::write(root.join("a.txt"), "one\n").expect("write");
        std::fs::write(root.join("b.txt"), "one\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        // Stage a change to a.txt; leave b.txt unstaged.
        std::fs::write(root.join("a.txt"), "one\ntwo\n").expect("write");
        std::fs::write(root.join("b.txt"), "one\ntwo\n").expect("write");
        git(root, &["add", "a.txt"]);
        let staged = working_tree_diff(root, true).expect("diff");
        assert_eq!(
            staged
                .paths
                .iter()
                .map(|p| p.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.txt"]
        );
        let all = working_tree_diff(root, false).expect("diff");
        assert_eq!(all.paths.len(), 2);
    }

    #[test]
    fn diff_reports_untracked_file_with_whole_file_range() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "test"]);
        std::fs::write(root.join("a.txt"), "one\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("new.txt"), "l1\nl2\nl3\n").expect("write");
        let diff = working_tree_diff(root, false).expect("diff");
        let untracked: Vec<_> = diff
            .paths
            .iter()
            .filter(|p| p.status == PathStatus::Untracked)
            .collect();
        assert_eq!(untracked.len(), 1);
        assert_eq!(untracked[0].path, "new.txt");
        assert_eq!(untracked[0].new_ranges, vec![(1, 3)]);
    }

    #[test]
    fn diff_is_read_only_for_the_index() {
        // The index mtime must not move: GIT_OPTIONAL_LOCKS=0 keeps every probe
        // from refreshing it, so a concurrent git operation never contends.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "test"]);
        std::fs::write(root.join("a.txt"), "one\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").expect("write");
        let index = root.join(".git/index");
        let before = std::fs::metadata(&index)
            .expect("index")
            .modified()
            .expect("mtime");
        // Sleep past coarse mtime granularity, then probe twice.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let _ = working_tree_diff(root, false).expect("diff");
        let _ = working_tree_diff(root, true).expect("diff");
        let after = std::fs::metadata(&index)
            .expect("index")
            .modified()
            .expect("mtime");
        assert_eq!(before, after, "read-only probes must not touch the index");
    }

    #[test]
    fn diff_git_header_parses_quoted_paths_with_spaces() {
        assert_eq!(
            parse_diff_git_new_path("\"a/my old.rs\" \"b/my new.rs\""),
            "my new.rs"
        );
        assert_eq!(parse_diff_git_new_path("a/src/a.rs b/src/a.rs"), "src/a.rs");
    }

    #[test]
    fn diff_reports_pure_rename_with_zero_changed_lines() {
        // A rename with no content change still resolves as a changed file
        // (with empty ranges) so the rename fallback has something to work on.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "test"]);
        std::fs::write(root.join("old.rs"), "fn f() {}\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::rename(root.join("old.rs"), root.join("new.rs")).expect("rename");
        git(root, &["add", "."]);
        let diff = working_tree_diff(root, false).expect("diff");
        assert_eq!(diff.paths.len(), 1);
        assert_eq!(diff.paths[0].path, "new.rs");
        assert_eq!(
            diff.paths[0].status,
            PathStatus::Renamed {
                from: "old.rs".to_owned()
            }
        );
        assert!(diff.paths[0].old_ranges.is_empty());
        assert_eq!(diff.paths[0].changed_lines, 0);
    }
}
