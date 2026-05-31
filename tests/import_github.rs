//! Conformance tests for the GitHub Issues/PRs import policy.
//!
//! This file has two categories:
//! 1. Doc-validation tests — verify `docs/schema/import-github.md` exists and
//!    contains every normative phrase required by issue #22. These pass once the
//!    policy document ships.
//! 2. Behaviour conformance tests — verify the `eg import github` importer
//!    produces the correct record shapes, respects rate limits, scrubs tokens,
//!    and honours idempotency. These tests are `#[ignore]` because the CLI
//!    implementation ships in a follow-up slice.
//!
//! NO live network access is permitted in this file. Behaviour tests use a
//! `wiremock` local server.
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]

use std::{fs, path::Path};

fn read_repo_text(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("{path} should be readable: {error}"))
}

fn assert_contains_all(path: &str, needles: &[&str]) {
    let text = read_repo_text(path);
    for needle in needles {
        assert!(
            text.contains(needle),
            "{path} must document or link `{needle}`"
        );
    }
}

fn assert_contains_none(path: &str, needles: &[&str]) {
    let text = read_repo_text(path);
    for needle in needles {
        assert!(
            !text.contains(needle),
            "{path} must not document stale contract `{needle}`"
        );
    }
}

// ─── Doc-validation tests ────────────────────────────────────────────────────

#[test]
fn import_github_schema_doc_locks_policy_contract() {
    // AC 1: document identity
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "# GitHub Issues/PRs Import Policy - v1",
            "single source of truth",
        ],
    );

    // AC 2: network-access boundary
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "eg import github <owner>/<repo>",
            "MUST NOT run inside daemon",
            "ADR 0003",
            "watch mode",
            "out of scope for v1",
        ],
    );

    // AC 3: auth surface
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "gh auth token",
            "--token-file <path>",
            "github_auth_missing",
            "github_repo_not_found",
            r"gh[pousr]_[A-Za-z0-9]{36,}",
            r"github_pat_[A-Za-z0-9_]{36,}",
            "[REDACTED_GH_TOKEN]",
            "NO config file storage",
        ],
    );

    // AC 4: canonical GitHub API surface
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "REST",
            "api.github.com",
            // Active v1 endpoints (with required per_page=100)
            "/issues?state=all&per_page=100",
            "/pulls?state=all&per_page=100",
            "/labels?per_page=100",
            // Deferred endpoints (documented but not fetched in v1)
            "/issues/comments",
            "/pulls/comments",
            "/pulls/{n}/reviews",
            "ETag",
            // per_page rule
            "per_page=100",
            "GitHub's default is 30 items",
            // Pagination rule
            r#"Link: <url>; rel="next""#,
            "Stopping at page 1 is a conformance violation",
        ],
    );

    // AC 5: rate-limit and backoff
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "X-RateLimit-Remaining",
            "X-RateLimit-Reset",
            "remaining < 100",
            "remaining < 5",
            "Retry-After",
            "github_auth_rejected",
            "exponential backoff",
            "502",
            "503",
            "504",
            "per-run stderr",
        ],
    );

    // AC 6: idempotency state file
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            ".github-import-state.json",
            "--state-file",
            "schema_version: 1",
            "source_repo",
            "api_base_url",
            "last_run_at_unix_ms",
            // ETag keys include page number
            r#"etags: { "<endpoint>?page=<n>": "<etag>" }"#,
            "cursors",
            "last_seen_updated_at",
            "If-None-Match",
            "304 Not Modified",
            // Budget is now expressed per-ETag
            "one conditional `If-None-Match` probe per stored ETag",
            "last_seen_updated_at` values",
            // Deferred endpoint ETag rule
            "MUST NOT store ETags for",
        ],
    );

    // AC 7: GitHub-to-record-shape mapping table
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "GitHubIssue",
            "ExternalLink",
            "state_reason",
            "review_kind",
            "issue_comment",
            "pr_review",
            "pr_review_comment",
            "diff_hunk",
            "in_reply_to_id",
            "REFERENCES_TASK",
            // Registered edge label is TOUCHES_FILE (not TOUCHED_FILE)
            "TOUCHES_FILE",
            // v1 emission scope
            "v1 emission scope",
            "GitHubIssue` deferred",
            "Review` is promoted",
            "REFERENCES_TASK` from `project.Review",
            "TOUCHES_FILE` from `project.Review",
            // Deferred endpoint ETag rule
            "MUST NOT store ETags for",
        ],
    );

    // AC 8: PR review thread normalization
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "transitive closure",
            "ReviewThread",
            // Resolution state not available over REST; no longer a v1 guarantee
            "REST limitation on thread resolution state",
            "isResolved",
            "deferred to the GraphQL slice",
        ],
    );

    // AC 9: redaction field map
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            // Title fields (Task.title is required to be redacted per project-graph.md)
            "Issue.title",
            "PullRequest.title",
            "Issue.body",
            "PullRequest.body",
            "Review.body",
            "ReviewComment.body",
            "ReviewComment.diff_hunk",
            "diff_hunk sub-policy",
            // Labels/assignees/URL must NOT be in plaintext carve-out
            "`Task.labels`, `Task.assignees`, and `ExternalLink.url` are",
            "NOT plaintext",
        ],
    );

    // AC 10: stable ID composition
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "blake3(domain || kind || source_repo || github_number || subkind_identity)",
            "issue:<n>",
            "pr:<n>",
            // ExternalLink IDs (v1 emits ExternalLink for every issue/PR)
            r#"system_native_id="issue:<n>""#,
            r#"system_native_id="pr:<n>""#,
            "issue_comment:<n>:<comment_id>",
            "pr_review:<n>:<review_id>",
            "pr_review_comment:<n>:<comment_id>",
            "byte-identical IDs",
        ],
    );

    // AC 11: reserved future slices
    assert_contains_all(
        "docs/schema/import-github.md",
        &["--org <name>", "--file <repos.txt>"],
    );

    // AC 14: schema versioning
    assert_contains_all(
        "docs/schema/import-github.md",
        &["schema_version` = `1`", "daemon-process exclusion"],
    );

    // Stale-contract guard
    assert_contains_none(
        "docs/schema/import-github.md",
        &[
            // Guard against daemon-process network clients
            "daemon polls",
            // Guard against v1 GraphQL endpoint use (the doc may mention
            // GraphQL to explain why it is excluded, but must not commit to
            // using a GraphQL endpoint in v1)
            "api.github.com/graphql",
        ],
    );
}

#[test]
fn import_github_schema_doc_is_linked_and_coordinated() {
    // AC 1: linked from README
    assert_contains_all("README.md", &["docs/schema/import-github.md"]);

    // AC 1: linked from vision PRD
    assert_contains_all("docs/prd/0000-egregore-vision.md", &["import-github.md"]);

    // AC 1: linked from project-task schema
    assert_contains_all("docs/schema/project-graph.md", &["import-github.md"]);

    // AC 1: linked from local-JSONL schema
    assert_contains_all("docs/schema/local-project-jsonl.md", &["import-github.md"]);
}

// ─── Behaviour conformance tests ───
//
// The observable contract the original `#[ignore]` stubs described is now
// implemented in `tests/import_github_behaviour.rs`, which drives the real
// `eg import github` CLI against a local mock HTTP server (no live network
// access). See that file for the fresh-import shape, idempotent re-import,
// redaction, failure-mode, file-link, and threaded-review conformance tests.
