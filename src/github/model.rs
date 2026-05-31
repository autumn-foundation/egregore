//! Serde models for the subset of the GitHub REST API the importer consumes.
//!
//! Only the fields named in `docs/schema/import-github.md` §6 (the
//! GitHub-to-record mapping) plus the fields that feed change-detection hashing
//! (§5) are modelled. Unknown fields are ignored so GitHub additive changes do
//! not break parsing.

use serde::{Deserialize, Serialize};

/// A GitHub user/actor login wrapper (`user`, `assignees[]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    /// Login handle (e.g. `octocat`).
    pub login: String,
}

/// A GitHub label as returned by the issues/pulls and `/labels` endpoints.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Label {
    /// Label name.
    pub name: String,
    /// Hex color (no `#`); part of the label-list change hash.
    #[serde(default)]
    pub color: String,
    /// Optional description; part of the label-list change hash.
    #[serde(default)]
    pub description: Option<String>,
}

/// A GitHub milestone (only the title is round-tripped into the body blob).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Milestone {
    /// Milestone title.
    pub title: String,
    /// Milestone number.
    #[serde(default)]
    pub number: Option<u64>,
}

/// Marker object present on `/issues` items that are actually pull requests.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PullRequestMarker {
    /// HTML URL of the PR (presence is what matters).
    #[serde(default)]
    pub html_url: Option<String>,
}

/// A GitHub issue from `GET /repos/{o}/{r}/issues?state=all`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Issue {
    /// Issue number.
    pub number: u64,
    /// Title (redacted before persistence).
    #[serde(default)]
    pub title: String,
    /// Body markdown (redacted before persistence).
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    #[serde(default)]
    pub state: String,
    /// `completed`, `not_planned`, or `reopened` when closed.
    #[serde(default)]
    pub state_reason: Option<String>,
    /// Labels attached to the issue.
    #[serde(default)]
    pub labels: Vec<Label>,
    /// Assignee logins.
    #[serde(default)]
    pub assignees: Vec<User>,
    /// Author login.
    #[serde(default)]
    pub user: Option<User>,
    /// Milestone, when assigned.
    #[serde(default)]
    pub milestone: Option<Milestone>,
    /// Creation timestamp (RFC 3339).
    #[serde(default)]
    pub created_at: String,
    /// Last-update timestamp (RFC 3339); the `valid_time` source.
    #[serde(default)]
    pub updated_at: String,
    /// Close timestamp, when closed.
    #[serde(default)]
    pub closed_at: Option<String>,
    /// Canonical HTML URL.
    #[serde(default)]
    pub html_url: String,
    /// Present iff this `/issues` item is really a PR (then it is discarded).
    #[serde(default)]
    pub pull_request: Option<PullRequestMarker>,
}

/// Git ref (head/base) on a pull request.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitRef {
    /// Branch ref name.
    #[serde(rename = "ref", default)]
    pub ref_name: String,
    /// Commit SHA at the ref.
    #[serde(default)]
    pub sha: String,
}

/// A GitHub pull request from `GET /repos/{o}/{r}/pulls?state=all`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PullRequest {
    /// PR number.
    pub number: u64,
    /// Title (redacted before persistence).
    #[serde(default)]
    pub title: String,
    /// Body markdown (redacted before persistence).
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    #[serde(default)]
    pub state: String,
    /// Merge timestamp; `Some` means merged (vs closed-unmerged).
    #[serde(default)]
    pub merged_at: Option<String>,
    /// Draft flag.
    #[serde(default)]
    pub draft: bool,
    /// Labels attached to the PR.
    #[serde(default)]
    pub labels: Vec<Label>,
    /// Assignee logins.
    #[serde(default)]
    pub assignees: Vec<User>,
    /// Author login.
    #[serde(default)]
    pub user: Option<User>,
    /// Milestone, when assigned.
    #[serde(default)]
    pub milestone: Option<Milestone>,
    /// Creation timestamp (RFC 3339).
    #[serde(default)]
    pub created_at: String,
    /// Last-update timestamp (RFC 3339); the `valid_time` source.
    #[serde(default)]
    pub updated_at: String,
    /// Close timestamp, when closed.
    #[serde(default)]
    pub closed_at: Option<String>,
    /// Head ref (source branch).
    #[serde(default)]
    pub head: Option<GitRef>,
    /// Base ref (target branch).
    #[serde(default)]
    pub base: Option<GitRef>,
    /// Merge commit SHA, when merged.
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    /// Canonical HTML URL.
    #[serde(default)]
    pub html_url: String,
}

/// An issue comment from `GET /repos/{o}/{r}/issues/comments`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueComment {
    /// Comment ID.
    pub id: u64,
    /// Comment body (redacted before persistence).
    #[serde(default)]
    pub body: Option<String>,
    /// Author login.
    #[serde(default)]
    pub user: Option<User>,
    /// API URL of the parent issue; the parent number is parsed from its tail.
    #[serde(default)]
    pub issue_url: String,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: String,
    /// Update timestamp; the `valid_time` source.
    #[serde(default)]
    pub updated_at: String,
    /// Canonical HTML URL.
    #[serde(default)]
    pub html_url: String,
}

/// A PR review summary from `GET /repos/{o}/{r}/pulls/{n}/reviews`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Review {
    /// Review ID.
    pub id: u64,
    /// Review body (redacted before persistence).
    #[serde(default)]
    pub body: Option<String>,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`, `PENDING`.
    #[serde(default)]
    pub state: String,
    /// Author login.
    #[serde(default)]
    pub user: Option<User>,
    /// Submission timestamp; the `valid_time` source.
    #[serde(default)]
    pub submitted_at: Option<String>,
    /// Canonical HTML URL.
    #[serde(default)]
    pub html_url: String,
}

/// A PR review comment from `GET /repos/{o}/{r}/pulls/comments`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewComment {
    /// Comment ID.
    pub id: u64,
    /// Comment body (redacted before persistence).
    #[serde(default)]
    pub body: Option<String>,
    /// Author login.
    #[serde(default)]
    pub user: Option<User>,
    /// Repo-relative file path the comment is anchored to.
    #[serde(default)]
    pub path: Option<String>,
    /// Line number in the diff.
    #[serde(default)]
    pub line: Option<u32>,
    /// Start line for multi-line comments.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// `LEFT` or `RIGHT`.
    #[serde(default)]
    pub side: Option<String>,
    /// Diff hunk context (redacted per the diff_hunk sub-policy).
    #[serde(default)]
    pub diff_hunk: Option<String>,
    /// Parent comment ID for threaded replies.
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    /// API URL of the parent PR; the parent number is parsed from its tail.
    #[serde(default)]
    pub pull_request_url: String,
    /// Commit SHA the comment is anchored to (PR head at comment time).
    #[serde(default)]
    pub commit_id: Option<String>,
    /// Creation timestamp.
    #[serde(default)]
    pub created_at: String,
    /// Update timestamp; the `valid_time` source.
    #[serde(default)]
    pub updated_at: String,
    /// Canonical HTML URL.
    #[serde(default)]
    pub html_url: String,
}

/// Parses the trailing numeric path segment of a GitHub API URL.
///
/// `issue_url` looks like `.../issues/42`; `pull_request_url` like
/// `.../pulls/42`. Returns `None` when no trailing number is present.
#[must_use]
pub fn trailing_number(url: &str) -> Option<u64> {
    url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_number_parses_issue_url() {
        assert_eq!(
            trailing_number("https://api.github.com/repos/o/r/issues/42"),
            Some(42)
        );
        assert_eq!(
            trailing_number("https://api.github.com/repos/o/r/pulls/7"),
            Some(7)
        );
        assert_eq!(trailing_number("not-a-url"), None);
    }

    #[test]
    fn issue_with_pull_request_marker_is_detectable() {
        let json = r#"{"number":1,"title":"t","pull_request":{"html_url":"x"}}"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert!(issue.pull_request.is_some());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{"number":1,"title":"t","some_new_field":123}"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.number, 1);
    }
}
