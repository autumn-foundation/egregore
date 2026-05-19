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

use crate::ir::{IdentitySource, RepositoryIdentityPayload, stable_id};

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

    // Case 1: .git with at least one remote → use lowest-name-sorted remote URL.
    if let Some(canonical_url) = git_canonical_remote_url(repo_root) {
        let id = stable_id(&["repository", "remote", &canonical_url]);
        let stable_basename = canonical_url
            .strip_prefix("https://")
            .or_else(|| canonical_url.strip_prefix("http://"))
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

    // Case 2: .git with commits but no remotes → use root commit SHA.
    if let Some(root_sha) = git_root_commit_sha(repo_root) {
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

/// Returns the normalized canonical URL of the lowest-name-sorted remote,
/// or `None` if `.git` does not exist or has no usable non-local remote.
fn git_canonical_remote_url(repo_root: &Path) -> Option<String> {
    if !git_is_repo_root(repo_root) {
        return None;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["remote"])
        .stdin(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let remotes_text = String::from_utf8(output.stdout).ok()?;
    let mut remotes: Vec<&str> = remotes_text
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect();

    if remotes.is_empty() {
        return None;
    }

    remotes.sort_unstable();

    // Iterate sorted remotes; skip local remotes and use the first non-local URL.
    for remote in &remotes {
        let url_output = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["remote", "get-url", remote])
            .stdin(Stdio::null())
            .output()
            .ok()?;

        if !url_output.status.success() {
            continue;
        }

        let url = String::from_utf8(url_output.stdout).ok()?;
        let url = url.trim();

        if url.is_empty() || is_local_remote_url(url) {
            continue;
        }

        return Some(normalize_remote_url(url));
    }

    None
}

/// Returns `true` for URLs that are machine-specific local paths rather than
/// portable remote addresses: absolute paths, `file://` URLs, and relative paths.
///
/// Scp-form remotes (`[user@]host:path`) are portable and return `false`.
/// Userless scp form (`host:path`, no `@`) is distinguished from relative paths
/// by the presence of `:` with no `/` before it.
fn is_local_remote_url(url: &str) -> bool {
    if url.starts_with('/') || url.starts_with("file://") {
        return true;
    }
    if url.contains("://") {
        return false;
    }
    // No scheme: scp form ([user@]host:path) if ':' appears before any '/'.
    // colon_pos > 1 rejects Windows drive letters like C:/repos (single-char prefix).
    if url
        .find(':')
        .is_some_and(|colon_pos| colon_pos > 1 && !url[..colon_pos].contains('/'))
    {
        return false; // scp form — portable
    }
    true // relative path or other local form
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
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim().to_owned();

    if sha.is_empty() { None } else { Some(sha) }
}

/// Normalizes a git remote URL to its canonical `https` form.
///
/// Normalizations applied (in order):
/// - `git@host:owner/repo` → `https://host/owner/repo`
/// - `ssh://[user@]host/path` → `https://host/path`
/// - Scheme coerced from `http` to `https`
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
    }

    // SSH URL form: ssh://[user@]host/path
    if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map_or(rest, |(_, after)| after);
        if let Some(slash) = rest.find('/') {
            let host = rest[..slash].to_lowercase();
            let path = &rest[slash..];
            return format!("https://{host}{}", strip_git_suffix(path));
        }
        let host = rest.to_lowercase();
        return format!("https://{host}");
    }

    // https://, http://, or git:// (git:// coerced to https)
    let scheme_rest = if let Some(s) = url.strip_prefix("https://") {
        s
    } else if let Some(s) = url.strip_prefix("http://") {
        s
    } else if let Some(s) = url.strip_prefix("git://") {
        s
    } else {
        return url.to_owned();
    };

    scheme_rest.find('/').map_or_else(
        || {
            let host = strip_userinfo(scheme_rest).to_lowercase();
            format!("https://{host}")
        },
        |slash| {
            let host = strip_userinfo(&scheme_rest[..slash]).to_lowercase();
            let path = &scheme_rest[slash..];
            format!("https://{host}{}", strip_git_suffix(path))
        },
    )
}

/// Strips a leading `user@` from a host string, if present.
fn strip_userinfo(host: &str) -> &str {
    host.split_once('@').map_or(host, |(_, h)| h)
}

/// Trims trailing slashes then strips a trailing `.git` extension, then trims again.
fn strip_git_suffix(s: &str) -> &str {
    let s = s.trim_end_matches('/');
    s.strip_suffix(".git").unwrap_or(s).trim_end_matches('/')
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
}
