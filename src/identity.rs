//! Repository identity computation for stable cross-clone node IDs.
//!
//! Three identity cases, selected in priority order:
//! 1. `Remote` – `.git` exists and has at least one remote URL.
//! 2. `LocalRootCommit` – `.git` exists with commits but no remotes.
//! 3. `LocalPath` – no `.git` or no commits; uses canonical absolute path.
//!
//! A fourth case, `OperatorOverride`, is used when `--repo-id-override` is passed.
//! See `docs/schema/repository-identity.md` for the full specification.

use std::{
    path::Path,
    process::{Command, Stdio},
};

use crate::ir::{
    IdentitySource, RepositoryIdentityPayload, SnapshotHead, stable_id, versioned_stable_id,
};

/// Computed repository identity, including its stable ID and full payload.
#[derive(Debug, Clone)]
pub struct RepositoryIdentity {
    /// Stable graph record ID for this repository.
    pub id: String,
    /// Structured payload describing how the ID was derived.
    pub payload: RepositoryIdentityPayload,
}

/// Computes the stable identity for a repository at `repo_root`.
///
/// Falls through three cases in order:
/// 1. Remote URL (if `.git` exists and has at least one remote)
/// 2. Root commit SHA (if `.git` exists with commits but no remotes)
/// 3. Canonical absolute path (fallback for non-git directories)
///
/// If `override_id` is `Some`, it directly replaces the canonical-inputs
/// hash and forces `identity_source = OperatorOverride`.
#[must_use]
pub fn compute_repository_identity(
    repo_root: &Path,
    override_id: Option<&str>,
) -> RepositoryIdentity {
    let basename = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("repository")
        .to_owned();

    if let Some(override_str) = override_id {
        let id = stable_id(&["repository", "operator-override", override_str]);
        return RepositoryIdentity {
            id,
            payload: RepositoryIdentityPayload {
                identity_source: IdentitySource::OperatorOverride,
                remote_url: None,
                root_commit_sha: None,
                canonical_path: None,
                basename: override_str.to_owned(),
            },
        };
    }

    // Case 1: .git with at least one non-local remote → use lowest-name-sorted remote URL.
    // Case 1b: all remotes are machine-local (file://, /path, ../rel) → skip Case 2 and go
    //   directly to Case 3 (LocalPath) so the daemon's local-path shared-store guard applies.
    //   LocalRootCommit is intentionally skipped: root commits are shared across unrelated forks
    //   that clone the same template, so they are not a reliable shared-store-safe identity.
    let all_local_remotes = match git_canonical_remote_url(repo_root) {
        RemoteStatus::Found(canonical_url) => {
            let id = stable_id(&["repository", "remote", &canonical_url]);
            let stable_basename = canonical_url
                .find("://")
                .and_then(|i| canonical_url.get(i + 3..))
                .and_then(|rest| rest.split_once('/'))
                .map(|(_, path)| path.to_owned())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| basename.clone());
            return RepositoryIdentity {
                id,
                payload: RepositoryIdentityPayload {
                    identity_source: IdentitySource::Remote,
                    remote_url: Some(canonical_url),
                    root_commit_sha: None,
                    canonical_path: None,
                    basename: stable_basename,
                },
            };
        }
        RemoteStatus::AllLocal => true,
        RemoteStatus::None => false,
    };

    // Case 2: .git with commits and no remotes at all → use root commit SHA.
    // Skipped when all configured remotes are machine-local (all_local_remotes = true).
    if !all_local_remotes && let Some(root_sha) = git_root_commit_sha(repo_root) {
        let id = stable_id(&["repository", "local-root-commit", &root_sha]);
        let stable_basename = format!("commit-{}", root_sha.chars().take(12).collect::<String>());
        return RepositoryIdentity {
            id,
            payload: RepositoryIdentityPayload {
                identity_source: IdentitySource::LocalRootCommit,
                remote_url: None,
                root_commit_sha: Some(root_sha),
                canonical_path: None,
                basename: stable_basename,
            },
        };
    }

    // Case 3: fallback to canonical absolute path.
    let canonical = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let canonical_str = canonical.to_string_lossy().into_owned();
    let id = stable_id(&["repository", "local-path", &canonical_str]);
    RepositoryIdentity {
        id,
        payload: RepositoryIdentityPayload {
            identity_source: IdentitySource::LocalPath,
            remote_url: None,
            root_commit_sha: None,
            canonical_path: Some(canonical_str),
            basename,
        },
    }
}

/// Returns the canonicalized absolute path of `repo_root`'s git top-level,
/// or `None` if git cannot discover a repository at `repo_root`.
fn git_top_level(repo_root: &Path) -> Option<std::path::PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path_str = String::from_utf8(output.stdout).ok()?;
    Some(std::path::PathBuf::from(path_str.trim()))
}

/// Returns `true` if `path` is exactly the root of its git repository (not a
/// subdirectory of one). Used to gate Git-derived behavior — identity, snapshot
/// stamping, and gitignore-aware discovery — to the repository root so scans of
/// in-repo sub-directories stay filesystem-local and independent of the
/// surrounding checkout.
#[must_use]
pub(crate) fn is_repo_root(path: &Path) -> bool {
    git_is_repo_root(path)
}

/// Returns `true` if `repo_root` is exactly the root of its git repository
/// (not a subdirectory of one).
fn git_is_repo_root(repo_root: &Path) -> bool {
    let Some(top_level) = git_top_level(repo_root) else {
        return false;
    };
    if let (Ok(r1), Ok(r2)) = (
        std::fs::canonicalize(repo_root),
        std::fs::canonicalize(&top_level),
    ) {
        r1 == r2
    } else {
        let s1 = repo_root
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        let s2 = top_level
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        s1.trim_end_matches('/') == s2.trim_end_matches('/')
    }
}

/// Outcome of the remote-URL probe: distinguishes "no remotes configured" from
/// "remotes exist but every one is a machine-local path".
enum RemoteStatus {
    /// At least one non-local remote was found; contains its normalized URL.
    Found(String),
    /// Remotes are configured but every URL is machine-local (`file://`, `/path`, `../rel`).
    /// Shared-store protection requires these repos to use `LocalPath` identity (not root-commit).
    AllLocal,
    /// No git repository root, no remotes configured, or git is unavailable.
    None,
}

/// Returns the normalized canonical URL of the lowest-name-sorted remote, or a
/// `RemoteStatus` indicating why no portable URL is available.
fn git_canonical_remote_url(repo_root: &Path) -> RemoteStatus {
    if !git_is_repo_root(repo_root) {
        return RemoteStatus::None;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["remote"])
        .stdin(Stdio::null())
        .output();

    let Ok(output) = output else {
        return RemoteStatus::None;
    };

    if !output.status.success() {
        return RemoteStatus::None;
    }

    let Ok(remotes_text) = String::from_utf8(output.stdout) else {
        return RemoteStatus::None;
    };

    let mut remotes: Vec<&str> = remotes_text
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect();

    if remotes.is_empty() {
        return RemoteStatus::None;
    }

    remotes.sort_unstable();

    // Iterate sorted remotes; return the first non-local URL.
    // If all remotes are local, return AllLocal so the caller can use LocalPath identity.
    let mut saw_local = false;
    for remote in &remotes {
        // Use `git config --get remote.<name>.url` to read the raw stored URL,
        // avoiding `insteadOf`/`pushInsteadOf` rewrites that `get-url` applies.
        let url_output = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["config", "--get", &format!("remote.{remote}.url")])
            .stdin(Stdio::null())
            .output();

        let Ok(url_output) = url_output else { continue };

        if !url_output.status.success() {
            continue;
        }

        let Ok(url_str) = String::from_utf8(url_output.stdout) else {
            continue;
        };
        let url = url_str.trim();

        if url.is_empty() {
            continue;
        }

        if is_local_remote_url(url) {
            saw_local = true;
            continue;
        }

        return RemoteStatus::Found(normalize_remote_url(url));
    }

    if saw_local {
        RemoteStatus::AllLocal
    } else {
        RemoteStatus::None
    }
}

/// Returns `true` for URLs that are machine-specific local paths rather than
/// portable remote addresses: absolute paths, `file://` URLs, and relative paths.
///
/// Scp-form remotes (`[user@]host:path`) are portable and return `false`.
/// Userless scp form (`host:path`, no `@`) is distinguished from relative paths
/// by the presence of `:` with no `/` before it.
pub(crate) fn is_local_remote_url(url: &str) -> bool {
    if url.starts_with('/') {
        return true;
    }
    // file:// scheme is local regardless of case (git may preserve the user's casing).
    if url.len() >= 7 && url[..7].eq_ignore_ascii_case("file://") {
        return true;
    }
    if let Some(scheme_end) = url.find("://") {
        // file:// already caught above; treat loopback hosts as local.
        let after_scheme = &url[scheme_end + 3..];
        let host = extract_host_from_authority(after_scheme);
        return is_loopback_host(host);
    }
    // No scheme: scp form ([user@]host:path) if ':' appears before any '/' or '\'.
    // colon_pos > 1 rejects Windows drive letters like C:/repos (single-char prefix).
    if let Some(colon_pos) = url
        .find(':')
        .filter(|&p| p > 1 && !url[..p].contains('/') && !url[..p].contains('\\'))
    {
        // scp form: [user@]host:path — portable unless the host is loopback.
        let host_field = &url[..colon_pos];
        let host = host_field.split('@').next_back().unwrap_or(host_field);
        return is_loopback_host(host);
    }
    true // relative path or other local form
}

/// Extracts the host from a URL authority component (`[user@]host[:port]` or `[user@][host]:port`).
fn extract_host_from_authority(authority: &str) -> &str {
    let after_at = authority
        .split_once('@')
        .map_or(authority, |(_, rest)| rest);
    if let Some(rest) = after_at.strip_prefix('[') {
        // IPv6 bracketed form: [::1] or [::1]:port
        return rest.split_once(']').map_or(rest, |(host, _)| host);
    }
    // Plain host or host:port — strip path and port.
    let host_port = after_at.split_once('/').map_or(after_at, |(h, _)| h);
    host_port
        .split_once(':')
        .map_or(host_port, |(host, _)| host)
}

/// Returns `true` for loopback host names and addresses.
fn is_loopback_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "localhost" || h == "::1" || h.starts_with("127.")
}

/// Returns the root commit SHA (oldest first-parent ancestor of HEAD),
/// or `None` if the path is not a git repo root or HEAD has no commits.
fn git_root_commit_sha(repo_root: &Path) -> Option<String> {
    if !git_is_repo_root(repo_root) {
        return None;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-list", "--first-parent", "--max-parents=0", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    // Take only the first line: --first-parent --max-parents=0 should yield exactly one root,
    // but take the first line defensively to avoid joining multiple SHAs into a composite key.
    let sha = stdout.lines().next()?.trim().to_owned();

    if sha.is_empty() { None } else { Some(sha) }
}

/// Captures the current working-tree source-snapshot head and dirty flag at `repo_root`.
///
/// Mirrors the repository-identity module's git-root gate (issue #82): a commit
/// SHA and dirty flag are reported only when `repo_root` is the actual Git
/// repository root. Sub-directories of a repository, non-Git directories, and
/// environments where Git is unavailable return [`SnapshotHead::NoGit`] so that
/// fixture scans of in-repo sub-directories stay byte-for-byte deterministic and
/// never leak the surrounding repository's HEAD. A repository root whose HEAD has
/// no commits yet returns [`SnapshotHead::UnbornHead`].
///
/// This function is strictly read-only: it runs `git rev-parse HEAD` and
/// `git status` with `GIT_OPTIONAL_LOCKS=0` so Git never refreshes/writes the
/// index or takes the `index.lock` (the freshness check must not mutate the
/// repository or contend with concurrent Git operations).
///
/// A failed dirty probe is treated conservatively as **dirty** so the store can
/// never be falsely classified `fresh` when the working-tree state could not be
/// verified (e.g. a locked or unreadable index).
#[must_use]
pub fn working_tree_snapshot(repo_root: &Path) -> (SnapshotHead, bool) {
    working_tree_snapshot_excluding(repo_root, &[])
}

/// Like [`working_tree_snapshot`], but excludes the given repo-relative paths
/// from the dirty probe (issue #82 / PR #186).
///
/// A freshness check passes the store artifact it is reading (the `--graph` file
/// or `--data-dir` directory) when that artifact lives under `repo_root`, so the
/// documented in-tree workflow (`eg scan . --out graph.jsonl`) is not reported
/// `stale_dirty` merely because the store it just wrote is itself an untracked
/// change. Excluded paths are matched by Git pathspec, so a directory excludes
/// everything beneath it.
#[must_use]
pub fn working_tree_snapshot_excluding(
    repo_root: &Path,
    exclude_rel: &[String],
) -> (SnapshotHead, bool) {
    if !git_is_repo_root(repo_root) {
        return (SnapshotHead::NoGit, false);
    }
    // Repository root but `git rev-parse HEAD` failing → HEAD has no commits.
    git_head_commit_sha(repo_root).map_or((SnapshotHead::UnbornHead, false), |sha| {
        // Conservative default: an unverifiable dirty state is treated as dirty,
        // never silently downgraded to clean (which could report a false `fresh`).
        let dirty = git_tree_dirty(repo_root, exclude_rel).unwrap_or(true);
        (SnapshotHead::Commit { sha }, dirty)
    })
}

/// Returns the current `HEAD` state without probing working-tree dirtiness.
///
/// History replay records committed state only (its snapshot is always
/// `dirty=false`), so it needs just the committed HEAD and must not pay the full
/// `git status` worktree walk that [`working_tree_snapshot_excluding`] performs —
/// which is wasteful in repositories with large untracked/generated trees
/// (VV1 / PR #186 follow-up).
#[must_use]
pub fn working_tree_head(repo_root: &Path) -> SnapshotHead {
    if !git_is_repo_root(repo_root) {
        return SnapshotHead::NoGit;
    }
    git_head_commit_sha(repo_root)
        .map_or(SnapshotHead::UnbornHead, |sha| SnapshotHead::Commit { sha })
}

/// Builds a `git` command rooted at `repo_root` that never writes the index.
///
/// `GIT_OPTIONAL_LOCKS=0` disables the optional index-refresh write `git status`
/// performs by default, keeping the freshness probe strictly read-only.
/// `-c core.excludesFile=` suppresses the user/system-level global gitignore so
/// probes see the same file set as the scanner (which runs with the same override),
/// keeping freshness results reproducible across developer environments.
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

/// Returns the full SHA that `HEAD` resolves to, or `None` when HEAD is unborn
/// (no commits yet), the path is not a repository, or Git is unavailable.
fn git_head_commit_sha(repo_root: &Path) -> Option<String> {
    let output = read_only_git(repo_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Returns `true` when the working tree has uncommitted or untracked changes.
///
/// Uses `git status --porcelain`, which reports staged, unstaged, and untracked
/// changes; a non-empty output means the tree is dirty. Runs read-only
/// (`GIT_OPTIONAL_LOCKS=0`, so Git never writes the index). `exclude_rel` paths
/// are dropped from consideration via `:(exclude)` pathspecs. Returns `None` when
/// Git is unavailable or the status probe fails, which callers treat as dirty.
///
/// The probe is scoped to the same source set the scanner actually indexes
/// (PR #186 follow-up HH1): `discover_source_files` skips every `target`
/// directory and never descends into submodules (`fs::should_descend`), so an
/// unignored `target/` build tree or a dirty/out-of-date submodule must not count
/// as source dirtiness here either — neither can produce a cited span.
fn git_tree_dirty(repo_root: &Path, exclude_rel: &[String]) -> Option<bool> {
    let mut command = read_only_git(repo_root);
    // `--untracked-files=all` ensures we see untracked files so we can detect
    // untracked .gitignore files (which affect ignore rules).
    // `--ignore-submodules=all` drops submodule state, which `git status` reports
    // by default but the scanner never indexes (HH1).
    command.args([
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignore-submodules=all",
    ]);
    // Scope the probe to the indexed source set: only supported source files
    // (`.rs`, `.py`, `.ts`, `.tsx`) can produce cited graph spans, so the
    // freshness verdict must ignore every non-source artifact in the working
    // tree — graph/JSONL outputs, embedded-store directories, refresh caches
    // (including custom out-of-tree ones the caller cannot name), and build
    // output — regardless of name or location (RR1/XX1). The positive
    // `:(glob)**/*.rs` / `:(glob)**/*.py` / `:(glob)**/*.ts` / `:(glob)**/*.tsx`
    // pathspecs match source files at any depth (root included); deletions and
    // renames of tracked source files still surface.
    //
    // `:(glob)**/.gitignore` also includes versioned ignore files: the indexed set
    // depends on them (the scanner drops gitignored untracked `.rs`), so adding or
    // changing an ignore rule after a scan changes which sources belong in the
    // graph and must read as dirty rather than `fresh` (BBB1 / PR #186 follow-up).
    //
    // Build output under any `target/` directory is pruned by the scanner
    // (`fs::should_descend`), so it is excluded here too (HH1/JJ1):
    // `:(exclude)target` drops the root `target/` and `:(exclude,glob)**/target/**`
    // drops nested per-crate `target/`. `exclude_rel` still drops any explicitly
    // named store artifact a caller passes (redundant under source scoping, but
    // harmless).
    command.args([
        "--",
        ":(glob)**/*.rs",
        ":(glob)**/*.py",
        ":(glob)**/*.ts",
        ":(glob)**/*.tsx",
        ":(glob)**/.gitignore",
        ":(exclude)target",
        ":(exclude,glob)**/target/**",
    ]);
    for rel in exclude_rel {
        command.arg(format!(":(exclude){rel}"));
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout_bytes = output.stdout;

    // Check if there are any dirty changes: modifications to tracked files,
    // or new untracked .gitignore files. Untracked source files are ignored.
    let mut has_dirty_changes = false;
    for line in stdout_bytes.split(|&b| b == b'\n') {
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];
        let path_bytes = &line[3..];
        if status == b"??" {
            if path_bytes == b".gitignore"
                || path_bytes.ends_with(b"/.gitignore")
                || path_bytes.ends_with(b"\\.gitignore")
            {
                has_dirty_changes = true;
                break;
            }
        } else {
            has_dirty_changes = true;
            break;
        }
    }

    // `git status` cannot see edits to tracked files marked `assume-unchanged`
    // or `skip-worktree` (e.g. after a sparse-checkout change). The scanner reads
    // those files when present, so a graph built from a full checkout would read
    // `fresh` after sparse checkout removes a cited `.rs` file. Treat any such
    // index-hidden source input as dirtiness so the verdict stays conservative
    // (PR #186 follow-up LL1).
    Some(has_dirty_changes || git_index_hidden_source_inputs(repo_root))
}

/// Runs `git ls-files -v` (strictly read-only) and returns its stdout, or `None`
/// when Git is unavailable or the listing fails. `-c core.quotePath=false` keeps
/// non-ASCII paths verbatim so they match graph `repo_relative_path` values.
fn git_ls_files_v(repo_root: &Path) -> Option<String> {
    let output = read_only_git(repo_root)
        .args(["-c", "core.quotePath=false", "ls-files", "-v"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Parses one `git ls-files -v` line, returning the repo-relative path of a
/// source-set input — a supported source file (`.rs`, `.py`, `.ts`, `.tsx`) or a
/// versioned `.gitignore`, outside any `target/` directory — that carries an index flag
/// hiding its state from `git status` (`skip-worktree` = `S`, `assume-unchanged`
/// = a lowercase tag). Returns `None` for any other line.
///
/// `.gitignore` counts because the indexed set depends on it (BBB1/EEE1); files
/// under `target/` are excluded to match the scanner's pruning (WW1).
fn hidden_source_input_path(line: &str) -> Option<&str> {
    let tag = line.chars().next()?;
    // Uppercase `S` = skip-worktree; any lowercase tag = assume-unchanged.
    if tag != 'S' && !tag.is_ascii_lowercase() {
        return None;
    }
    // Format is `<tag><space><path>`, so the path starts at byte 2.
    let rel = line.get(2..)?;
    let path = Path::new(rel);
    if path.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    let is_source_input = crate::languages::is_supported_source(path)
        || path.file_name().and_then(|n| n.to_str()) == Some(".gitignore");
    is_source_input.then_some(rel)
}

/// Returns `true` when any source-set input (a supported source file or versioned
/// `.gitignore`) that is **present in the working tree** carries an index flag
/// hiding its state from `git status` (PR #186 follow-up LL1/EEE1).
///
/// The file must exist on disk to count: a clean sparse checkout marks omitted
/// files `skip-worktree` AND leaves them absent, so the scanner never indexed
/// them and they are not part of the stored source set — flagging them would make
/// `eg scan` of a sparse checkout immediately read `stale_dirty` (AAA1). A present
/// index-hidden file (e.g. `assume-unchanged`) WAS scannable, so hidden edits to
/// it still warrant the conservative dirty verdict. (Absent index-hidden inputs
/// are surfaced separately by [`index_hidden_absent_source_inputs`] for the
/// store-aware removal check.)
fn git_index_hidden_source_inputs(repo_root: &Path) -> bool {
    let Some(text) = git_ls_files_v(repo_root) else {
        return false;
    };
    let mut dir_exists_cache = std::collections::HashMap::new();
    text.lines()
        .filter_map(hidden_source_input_path)
        .any(|rel| {
            let path = Path::new(rel);
            if let Some(parent) = path.parent() {
                let parent_abs = repo_root.join(parent);
                let exists = *dir_exists_cache
                    .entry(parent_abs.clone())
                    .or_insert_with(|| parent_abs.exists());
                if !exists {
                    return false;
                }
            }
            repo_root.join(rel).is_file()
        })
}

/// Returns the repo-relative paths of source-set inputs (`.rs` / versioned
/// `.gitignore`) that are index-hidden (`skip-worktree`/`assume-unchanged`) AND
/// absent from the working tree.
///
/// These are exactly the omissions [`git_index_hidden_source_inputs`] skips at
/// scan time (AAA1). At freshness time a caller cross-references them against the
/// stored graph: an absent path the store still cites is a previously scanned
/// source that a sparse-checkout cone change removed while `git status` stays
/// blind to it, so the store is stale (FFF1). Returns an empty vec when Git is
/// unavailable or there are no such omissions.
#[must_use]
pub fn index_hidden_absent_source_inputs(repo_root: &Path) -> Vec<String> {
    let Some(text) = git_ls_files_v(repo_root) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(hidden_source_input_path)
        .filter(|rel| !repo_root.join(rel).is_file())
        .map(ToOwned::to_owned)
        .collect()
}

/// Normalizes a git remote URL to its canonical `https` form.
///
/// Normalizations applied (in order):
/// - `git@host:owner/repo` → `https://host/owner/repo`
/// - `ssh://[user@]host/path` → `https://host/path`
/// - Scheme coerced from `http` to `https` (case-insensitively)
/// - Host portion lowercased
/// - Trailing `.git` stripped
#[must_use]
pub fn normalize_remote_url(url: &str) -> String {
    // SSH scp form: [user@]host:path  (e.g. git@github.com:owner/repo.git or alice@host:path)
    if !url.contains("://") {
        let without_user = url.split_once('@').map_or(url, |(_, rest)| rest);
        if let Some(colon) = without_user.find(':') {
            let host = without_user[..colon].to_lowercase();
            let path = &without_user[colon + 1..];
            return format!("https://{host}/{}", strip_git_suffix(path));
        }
        return url.to_owned();
    }

    // Normalize the scheme to lowercase before dispatch so that HTTPS://, SSH://, etc. are handled.
    // SAFETY: url.contains("://") was checked at the top of this arm, so find returns Some.
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let scheme_lower = url[..scheme_end].to_lowercase();
    let after_scheme_sep = &url[scheme_end + 3..]; // after "://"

    // SSH URL form: ssh://[user@]host[:22]/path
    if scheme_lower == "ssh" {
        let rest = after_scheme_sep
            .split_once('@')
            .map_or(after_scheme_sep, |(_, after)| after);
        if let Some(slash) = rest.find('/') {
            let host_lower = rest[..slash].to_lowercase();
            let host = strip_default_port(&host_lower, 22);
            let path = &rest[slash..];
            return format!("https://{host}{}", strip_git_suffix(path));
        }
        let host_lower = rest.to_lowercase();
        let host = strip_default_port(&host_lower, 22);
        return format!("https://{host}");
    }

    // https://, http://, or git:// (git:// coerced to https)
    let (scheme_rest, default_port) = if scheme_lower == "https" {
        (after_scheme_sep, 443u16)
    } else if scheme_lower == "http" {
        (after_scheme_sep, 80u16)
    } else if scheme_lower == "git" {
        (after_scheme_sep, 9418u16)
    } else {
        // Unknown scheme: lowercase the scheme and host, strip userinfo and .git suffix for
        // consistent identity (two remotes that differ only by case or trailing .git must hash
        // the same even when the scheme is not recognised by the normaliser).
        return after_scheme_sep.find('/').map_or_else(
            || {
                let host_lower = strip_userinfo(after_scheme_sep).to_lowercase();
                format!("{scheme_lower}://{host_lower}")
            },
            |slash| {
                let host_lower = strip_userinfo(&after_scheme_sep[..slash]).to_lowercase();
                let path = &after_scheme_sep[slash..];
                format!("{scheme_lower}://{host_lower}{}", strip_git_suffix(path))
            },
        );
    };

    scheme_rest.find('/').map_or_else(
        || {
            let host_lower = strip_userinfo(scheme_rest).to_lowercase();
            let host = strip_default_port(&host_lower, default_port);
            format!("https://{host}")
        },
        |slash| {
            let host_lower = strip_userinfo(&scheme_rest[..slash]).to_lowercase();
            let host = strip_default_port(&host_lower, default_port);
            let path = &scheme_rest[slash..];
            format!("https://{host}{}", strip_git_suffix(path))
        },
    )
}

/// Strips a leading `user@` from a host string, if present.
fn strip_userinfo(host: &str) -> &str {
    host.split_once('@').map_or(host, |(_, h)| h)
}

/// Strips a trailing `:<port>` if it matches the given default port.
fn strip_default_port(host: &str, default_port: u16) -> &str {
    let suffix = format!(":{default_port}");
    host.strip_suffix(suffix.as_str()).unwrap_or(host)
}

/// Trims trailing slashes then strips a trailing `.git` extension, then trims again.
fn strip_git_suffix(s: &str) -> &str {
    let s = s.trim_end_matches('/');
    s.strip_suffix(".git").unwrap_or(s).trim_end_matches('/')
}

/// Returns `true` when `submitted_id` is the stable ID that would be computed from `payload`.
///
/// A mismatch indicates either a tampered payload or a bug in the write path; such records
/// should be treated as machine-local unsafe regardless of the declared `identity_source`.
#[allow(dead_code)]
pub(crate) fn repository_id_matches_payload(
    submitted_id: &str,
    payload: &RepositoryIdentityPayload,
) -> bool {
    let version = if let Some((v, _)) = crate::ir::parse_codegraph_id(submitted_id) {
        v
    } else {
        crate::ir::SCHEMA_VERSION
    };

    let parts: Vec<String> = match payload.identity_source {
        IdentitySource::Remote => {
            let Some(url) = &payload.remote_url else {
                return false;
            };
            vec![
                "repository".to_owned(),
                "remote".to_owned(),
                normalize_remote_url(url),
            ]
        }
        IdentitySource::LocalRootCommit => {
            let Some(sha) = &payload.root_commit_sha else {
                return false;
            };
            vec![
                "repository".to_owned(),
                "local-root-commit".to_owned(),
                sha.clone(),
            ]
        }
        IdentitySource::LocalPath => {
            let Some(path) = &payload.canonical_path else {
                return false;
            };
            vec![
                "repository".to_owned(),
                "local-path".to_owned(),
                path.clone(),
            ]
        }
        IdentitySource::OperatorOverride => {
            vec![
                "repository".to_owned(),
                "operator-override".to_owned(),
                payload.basename.clone(),
            ]
        }
    };
    let parts_refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    versioned_stable_id(version, &parts_refs) == submitted_id
}

#[cfg(test)]
mod tests {
    use super::normalize_remote_url;

    #[test]
    fn ssh_remote_normalized() {
        assert_eq!(
            normalize_remote_url("git@github.com:owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn https_git_suffix_stripped() {
        assert_eq!(
            normalize_remote_url("https://github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn http_coerced_to_https() {
        assert_eq!(
            normalize_remote_url("http://example.com/owner/repo"),
            "https://example.com/owner/repo"
        );
    }

    #[test]
    fn host_lowercased() {
        assert_eq!(
            normalize_remote_url("https://GITHUB.COM/owner/repo"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn ssh_url_form_normalized() {
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn ssh_url_without_user_normalized() {
        assert_eq!(
            normalize_remote_url("ssh://github.com/owner/repo"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn ssh_url_arbitrary_user_stripped() {
        assert_eq!(
            normalize_remote_url("ssh://alice@github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn scp_arbitrary_user_normalized() {
        assert_eq!(
            normalize_remote_url("alice@example.com:owner/repo.git"),
            "https://example.com/owner/repo"
        );
    }

    #[test]
    fn scp_without_user_normalized() {
        assert_eq!(
            normalize_remote_url("github.com:owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn https_trailing_slash_stripped() {
        assert_eq!(
            normalize_remote_url("https://github.com/owner/repo/"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn https_git_suffix_and_trailing_slash_stripped() {
        assert_eq!(
            normalize_remote_url("https://github.com/owner/repo.git/"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn https_userinfo_stripped() {
        assert_eq!(
            normalize_remote_url("https://alice@github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn git_protocol_coerced_to_https() {
        assert_eq!(
            normalize_remote_url("git://github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn windows_drive_letter_not_treated_as_scp() {
        assert!(super::is_local_remote_url("C:/repos/mirror.git"));
    }

    #[test]
    fn file_url_uppercase_scheme_is_local() {
        assert!(super::is_local_remote_url("FILE:///var/mirror.git"));
        assert!(super::is_local_remote_url("File:///var/mirror.git"));
    }

    #[test]
    fn ssh_url_default_port_stripped() {
        assert_eq!(
            normalize_remote_url("ssh://git@github.com:22/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn ssh_url_nondefault_port_preserved() {
        assert_eq!(
            normalize_remote_url("ssh://git@github.com:2222/owner/repo.git"),
            "https://github.com:2222/owner/repo"
        );
    }

    #[test]
    fn https_url_default_port_stripped() {
        assert_eq!(
            normalize_remote_url("https://github.com:443/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn http_url_default_port_stripped() {
        assert_eq!(
            normalize_remote_url("http://example.com:80/owner/repo"),
            "https://example.com/owner/repo"
        );
    }

    #[test]
    fn uppercase_https_scheme_normalized() {
        assert_eq!(
            normalize_remote_url("HTTPS://github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn mixed_case_ssh_scheme_normalized() {
        assert_eq!(
            normalize_remote_url("SSH://git@github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn mixed_case_http_scheme_normalized() {
        assert_eq!(
            normalize_remote_url("HTTP://example.com/owner/repo"),
            "https://example.com/owner/repo"
        );
    }

    #[test]
    fn unknown_scheme_host_lowercased_and_git_suffix_stripped() {
        assert_eq!(
            normalize_remote_url("FTP://EXAMPLE.com/org/repo.git"),
            "ftp://example.com/org/repo"
        );
    }

    #[test]
    fn unknown_scheme_userinfo_stripped() {
        assert_eq!(
            normalize_remote_url("ftp://user@EXAMPLE.com/org/repo"),
            "ftp://example.com/org/repo"
        );
    }

    #[test]
    fn unknown_scheme_no_path() {
        assert_eq!(
            normalize_remote_url("FTP://EXAMPLE.com"),
            "ftp://example.com"
        );
    }

    #[test]
    fn scp_localhost_is_local() {
        assert!(super::is_local_remote_url("localhost:/srv/repo.git"));
        assert!(super::is_local_remote_url("git@localhost:/srv/repo.git"));
    }

    #[test]
    fn scp_loopback_ipv4_is_local() {
        assert!(super::is_local_remote_url("git@127.0.0.1:/repo.git"));
        assert!(super::is_local_remote_url("127.0.0.1:/repo.git"));
    }

    #[test]
    fn scp_real_host_is_not_local() {
        assert!(!super::is_local_remote_url("git@github.com:owner/repo.git"));
        assert!(!super::is_local_remote_url("github.com:owner/repo.git"));
    }

    #[test]
    fn ssh_localhost_is_local() {
        assert!(super::is_local_remote_url(
            "ssh://localhost/path/to/repo.git"
        ));
        assert!(super::is_local_remote_url(
            "ssh://user@localhost/path/to/repo.git"
        ));
    }

    #[test]
    fn https_loopback_ipv4_is_local() {
        assert!(super::is_local_remote_url("https://127.0.0.1/repo.git"));
        assert!(super::is_local_remote_url("https://127.1.2.3/repo.git"));
    }

    #[test]
    fn https_loopback_ipv6_is_local() {
        assert!(super::is_local_remote_url("https://[::1]/repo.git"));
        assert!(super::is_local_remote_url("git://[::1]:9418/repo.git"));
    }

    #[test]
    fn ssh_github_is_not_local() {
        assert!(!super::is_local_remote_url(
            "ssh://github.com/owner/repo.git"
        ));
    }

    #[test]
    fn repository_id_matches_remote_payload() {
        use crate::ir::{IdentitySource, RepositoryIdentityPayload};
        let payload = RepositoryIdentityPayload {
            identity_source: IdentitySource::Remote,
            remote_url: Some("git@github.com:owner/repo.git".to_owned()),
            root_commit_sha: None,
            canonical_path: None,
            basename: "owner/repo".to_owned(),
        };
        let expected_id =
            crate::ir::stable_id(&["repository", "remote", "https://github.com/owner/repo"]);
        assert!(super::repository_id_matches_payload(&expected_id, &payload));
        assert!(!super::repository_id_matches_payload(
            "codegraph:v3:wrong-hash",
            &payload
        ));

        // Test prior version-awareness (v4 ID matches v4 payload check)
        let v4_id = crate::ir::versioned_stable_id(
            4,
            &["repository", "remote", "https://github.com/owner/repo"],
        );
        assert!(super::repository_id_matches_payload(&v4_id, &payload));
    }
}
