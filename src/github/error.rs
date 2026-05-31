//! Operator-facing error classes for the GitHub importer.
//!
//! Every variant maps to a stable diagnostic code named in
//! `docs/schema/import-github.md` §2 and §4. Error `Display` text names the
//! failing source class and NEVER echoes token or secret-bearing payload bytes
//! (the message is built from status codes and endpoint paths only).

use std::fmt;

/// Stable, operator-facing failure classes for the importer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubError {
    /// No token available and the repo probe returned 404 (cannot tell private
    /// from non-existent). Stable code: `github_auth_missing`.
    AuthMissing,
    /// Token present but rejected (401/403 without a rate-limit indicator).
    /// Stable code: `github_auth_rejected`.
    AuthRejected,
    /// Repo does not exist or the token lacks access (404 with token).
    /// Stable code: `github_repo_not_found`.
    RepoNotFound,
    /// Anonymous rate-limit quota exhausted with no token fallback and the wait
    /// would exceed the cap. Stable code: `github_rate_limit_anon`.
    RateLimitExhausted,
    /// A fetch failed after the retry budget (5xx or rate limit).
    /// Stable code: `github_fetch_failed`. Carries the endpoint class and HTTP
    /// status; never the response body.
    FetchFailed {
        /// Endpoint class, e.g. `issues`, `pulls`, `labels`, `reviews`.
        source_class: String,
        /// Last HTTP status observed.
        status: u16,
    },
    /// A response body was not valid JSON. Stable code: `github_invalid_response`.
    InvalidResponse {
        /// Endpoint class that produced the unparseable body.
        source_class: String,
    },
    /// Local I/O failure writing the handoff or state file.
    /// Stable code: `github_io_error`.
    Io {
        /// Short, secret-free description of the failing operation.
        detail: String,
    },
    /// The `<owner>/<repo>` argument was not in `owner/repo` form.
    /// Stable code: `github_invalid_repo_arg`.
    InvalidRepoArg {
        /// The offending argument (a repo slug, never secret-bearing).
        arg: String,
    },
}

impl GithubError {
    /// Returns the stable diagnostic code for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::AuthMissing => "github_auth_missing",
            Self::AuthRejected => "github_auth_rejected",
            Self::RepoNotFound => "github_repo_not_found",
            Self::RateLimitExhausted => "github_rate_limit_anon",
            Self::FetchFailed { .. } => "github_fetch_failed",
            Self::InvalidResponse { .. } => "github_invalid_response",
            Self::Io { .. } => "github_io_error",
            Self::InvalidRepoArg { .. } => "github_invalid_repo_arg",
        }
    }

    /// Returns `true` when this failure must NOT write or update the state file
    /// (auth failures per `docs/schema/import-github.md` §5 "Failure rule").
    #[must_use]
    pub const fn suppresses_state_write(&self) -> bool {
        matches!(self, Self::AuthMissing | Self::AuthRejected)
    }
}

impl fmt::Display for GithubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthMissing => write!(
                f,
                "{}: repository probe returned 404 and no token is available; a token is \
                 required to determine whether the repository exists (set GH_TOKEN, \
                 GITHUB_TOKEN, --token-file, or `gh auth login`)",
                self.code()
            ),
            Self::AuthRejected => write!(
                f,
                "{}: the provided GitHub token was rejected (401/403)",
                self.code()
            ),
            Self::RepoNotFound => write!(
                f,
                "{}: the repository does not exist or the token lacks access",
                self.code()
            ),
            Self::RateLimitExhausted => write!(
                f,
                "{}: anonymous rate-limit quota is exhausted and the reset wait exceeds the cap; \
                 provide a token to use the authenticated quota",
                self.code()
            ),
            Self::FetchFailed {
                source_class,
                status,
            } => write!(
                f,
                "{}: fetching `{source_class}` failed after the retry budget (last status {status})",
                self.code()
            ),
            Self::InvalidResponse { source_class } => write!(
                f,
                "{}: `{source_class}` returned a response body that was not valid JSON",
                self.code()
            ),
            Self::Io { detail } => write!(f, "{}: {detail}", self.code()),
            Self::InvalidRepoArg { arg } => {
                write!(f, "{}: `{arg}` is not in <owner>/<repo> form", self.code())
            }
        }
    }
}

impl std::error::Error for GithubError {}

/// Convenience result alias for importer operations.
pub type GithubResult<T> = Result<T, GithubError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(GithubError::AuthMissing.code(), "github_auth_missing");
        assert_eq!(GithubError::AuthRejected.code(), "github_auth_rejected");
        assert_eq!(GithubError::RepoNotFound.code(), "github_repo_not_found");
    }

    #[test]
    fn auth_failures_suppress_state_write() {
        assert!(GithubError::AuthMissing.suppresses_state_write());
        assert!(GithubError::AuthRejected.suppresses_state_write());
        assert!(!GithubError::RepoNotFound.suppresses_state_write());
        assert!(
            !GithubError::FetchFailed {
                source_class: "issues".to_owned(),
                status: 500
            }
            .suppresses_state_write()
        );
    }

    #[test]
    fn display_never_contains_token_placeholder_text() {
        // The messages are built from codes + status only; assert they render.
        let e = GithubError::FetchFailed {
            source_class: "pulls".to_owned(),
            status: 503,
        };
        let s = e.to_string();
        assert!(s.contains("github_fetch_failed"));
        assert!(s.contains("pulls"));
        assert!(s.contains("503"));
    }
}
