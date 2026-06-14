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

use crate::ir::{IdentitySource, RepositoryIdentityPayload, SnapshotHead, stable_id};

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

/// Returns `true` if `repo_root` is exactly the root of its git repository
/// (not a subdirectory of one).
fn git_is_repo_root(repo_root: &Path) -> bool {
    let Some(top_level) = git_top_level(repo_root) else {
        return false;
    };
    let canonical_root =
        std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let canonical_top = std::fs::canonicalize(&top_level).unwrap_or(top_level);
    canonical_root == canonical_top
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
    // No scheme: scp form ([user@]host:path) if ':' appears before any '/'.
    // colon_pos > 1 rejects Windows drive letters like C:/repos (single-char prefix).
    if let Some(colon_pos) = url.find(':').filter(|&p| p > 1 && !url[..p].contains('/')) {
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
/// `git status --porcelain` and never writes to the repository or the index.
#[must_use]
pub fn working_tree_snapshot(repo_root: &Path) -> (SnapshotHead, bool) {
    if !git_is_repo_root(repo_root) {
        return (SnapshotHead::NoGit, false);
    }
    // Repository root but `git rev-parse HEAD` failing → HEAD has no commits.
    git_head_commit_sha(repo_root).map_or((SnapshotHead::UnbornHead, false), |sha| {
        let dirty = git_tree_dirty(repo_root).unwrap_or(false);
        (SnapshotHead::Commit { sha }, dirty)
    })
}

/// Returns the full SHA that `HEAD` resolves to, or `None` when HEAD is unborn
/// (no commits yet), the path is not a repository, or Git is unavailable.
fn git_head_commit_sha(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
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
/// changes; a non-empty output means the tree is dirty. Returns `None` when Git
/// is unavailable or the status probe fails.
fn git_tree_dirty(repo_root: &Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(!text.trim().is_empty())
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
    match payload.identity_source {
        IdentitySource::Remote => payload.remote_url.as_deref().is_some_and(|url| {
            stable_id(&["repository", "remote", &normalize_remote_url(url)]) == submitted_id
        }),
        IdentitySource::LocalRootCommit => payload.root_commit_sha.as_deref().is_some_and(|sha| {
            stable_id(&["repository", "local-root-commit", sha]) == submitted_id
        }),
        IdentitySource::LocalPath => payload
            .canonical_path
            .as_deref()
            .is_some_and(|path| stable_id(&["repository", "local-path", path]) == submitted_id),
        IdentitySource::OperatorOverride => {
            stable_id(&["repository", "operator-override", &payload.basename]) == submitted_id
        }
    }
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
    }
}
