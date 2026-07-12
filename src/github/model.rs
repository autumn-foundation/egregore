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

/// A GitHub team as returned in a pull request's `requested_teams` array.
///
/// Only the `slug` is modelled (issue #335): a requested team is recorded as a
/// `Diagnostic`, never expanded to member logins, so no other team field is
/// consumed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Team {
    /// URL-safe team identifier (e.g. `backend-reviewers`).
    #[serde(default)]
    pub slug: String,
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
    /// Individual reviewers whose review was requested on this PR (issue #335).
    /// Sourced from the already-fetched `/pulls` payload — no new endpoint.
    /// Each becomes a `REQUESTED_REVIEW_FROM` edge to the reviewer's identity.
    #[serde(default)]
    pub requested_reviewers: Vec<User>,
    /// Teams whose review was requested on this PR (issue #335). Recorded as a
    /// `github_team_review_request_unexpanded` `Diagnostic` carrying the slug —
    /// never expanded to member logins. Sourced from the `/pulls` payload.
    #[serde(default)]
    pub requested_teams: Vec<Team>,
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
    /// Commit SHA the review was anchored to — the exact commit the reviewer
    /// looked at (issue #334). Absent on rare payloads (e.g. an unsubmitted
    /// `PENDING` review); a genuinely-absent value is diagnosed, never faked.
    #[serde(default)]
    pub commit_id: Option<String>,
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
    /// Diff hunk context (redacted per the `diff_hunk` sub-policy).
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

/// The dismissed-review payload carried on a `review_dismissed` timeline event
/// (issue #336).
///
/// GitHub nests the id of the review that was dismissed, its resulting state,
/// and the optional operator-supplied dismissal message. `review_id` is what
/// binds the transition back to the `Review` record the importer already minted.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DismissedReview {
    /// Server id of the dismissed review (== the `pr_review` review id).
    #[serde(default)]
    pub review_id: u64,
    /// Resulting review state (`dismissed`).
    #[serde(default)]
    pub state: String,
    /// Optional free-text dismissal message; redacted into a `body_handle`,
    /// never stored raw in the graph.
    #[serde(default)]
    pub dismissal_message: Option<String>,
}

/// A GitHub PR-timeline event from `GET /repos/{o}/{r}/issues/{n}/timeline`
/// (issue #336).
///
/// Only the closed review-state-transition event kinds are consumed
/// (`review_dismissed`, `review_requested`, `review_request_removed`); all other
/// timeline events are filtered out before parsing. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimelineEvent {
    /// Event kind discriminator (e.g. `review_dismissed`).
    #[serde(default)]
    pub event: String,
    /// Server-native event id; seeds the `ReviewStateTransition` stable id as
    /// `timeline:<id>`.
    #[serde(default)]
    pub id: u64,
    /// Event timestamp (RFC 3339); the transition's `valid_time`.
    #[serde(default)]
    pub created_at: String,
    /// Actor who performed the transition (the dismisser / requester login).
    #[serde(default)]
    pub actor: Option<User>,
    /// Present on `review_dismissed`; names the dismissed review.
    #[serde(default)]
    pub dismissed_review: Option<DismissedReview>,
    /// Present on `review_requested` / `review_request_removed` for an
    /// individual reviewer.
    #[serde(default)]
    pub requested_reviewer: Option<User>,
    /// Present on `review_requested` / `review_request_removed` for a team.
    #[serde(default)]
    pub requested_team: Option<Team>,
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

    #[test]
    fn pull_parses_requested_reviewers_and_teams() {
        // Issue #335: requested reviewers/teams ride on the already-fetched
        // /pulls payload — no new endpoint.
        let json = r#"{"number":7,"requested_reviewers":[{"login":"alice"},{"login":"bob"}],"requested_teams":[{"slug":"backend"}]}"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.requested_reviewers.len(), 2);
        assert_eq!(pr.requested_reviewers[0].login, "alice");
        assert_eq!(pr.requested_teams.len(), 1);
        assert_eq!(pr.requested_teams[0].slug, "backend");
    }

    #[test]
    fn pull_parses_without_requested_reviewers() {
        // Issue #335: an absent requested_reviewers/requested_teams defaults to
        // empty rather than failing.
        let json = r#"{"number":7}"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert!(pr.requested_reviewers.is_empty());
        assert!(pr.requested_teams.is_empty());
    }

    #[test]
    fn review_parses_with_commit_id() {
        // Issue #334: a submitted review carries the commit_id it reviewed.
        let json = r#"{"id":10,"state":"APPROVED","commit_id":"deadbeef"}"#;
        let review: Review = serde_json::from_str(json).unwrap();
        assert_eq!(review.commit_id.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn review_parses_without_commit_id() {
        // Issue #334: an absent commit_id (rare, e.g. a PENDING review)
        // deserializes to None rather than failing — the gap is diagnosed, not
        // faked, downstream.
        let json = r#"{"id":11,"state":"PENDING"}"#;
        let review: Review = serde_json::from_str(json).unwrap();
        assert_eq!(review.commit_id, None);
    }

    #[test]
    fn timeline_review_dismissed_parses() {
        // Issue #336: a review_dismissed timeline event carries the dismissed
        // review id, the actor, and an optional dismissal message.
        let json = r#"{"event":"review_dismissed","id":5001,"created_at":"2026-01-03T00:00:00Z","actor":{"login":"maintainer"},"dismissed_review":{"review_id":301,"state":"dismissed","dismissal_message":"stale"}}"#;
        let ev: TimelineEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.event, "review_dismissed");
        assert_eq!(ev.id, 5001);
        assert_eq!(ev.actor.as_ref().unwrap().login, "maintainer");
        let dr = ev.dismissed_review.unwrap();
        assert_eq!(dr.review_id, 301);
        assert_eq!(dr.dismissal_message.as_deref(), Some("stale"));
    }

    #[test]
    fn timeline_review_requested_parses_without_dismissed_review() {
        // Issue #336: a review_requested event has a requested_reviewer and no
        // dismissed_review; the absent field deserializes to None, not an error.
        let json = r#"{"event":"review_requested","id":42,"created_at":"2026-01-02T00:00:00Z","actor":{"login":"author"},"requested_reviewer":{"login":"alice"}}"#;
        let ev: TimelineEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.event, "review_requested");
        assert!(ev.dismissed_review.is_none());
        assert_eq!(ev.requested_reviewer.unwrap().login, "alice");
    }

    #[test]
    fn review_comment_parses_commit_id() {
        // ReviewComment already carried commit_id; #334 begins USING it.
        let json = r#"{"id":12,"commit_id":"cafef00d","pull_request_url":"https://api.github.com/repos/o/r/pulls/3"}"#;
        let c: ReviewComment = serde_json::from_str(json).unwrap();
        assert_eq!(c.commit_id.as_deref(), Some("cafef00d"));
    }
}
