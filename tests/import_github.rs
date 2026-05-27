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
            "/issues?state=all",
            "/pulls?state=all",
            "/issues/comments",
            "/pulls/comments",
            "/pulls/{n}/reviews",
            "/labels",
            "ETag",
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
            "etags",
            "cursors",
            "last_seen_updated_at",
            "If-None-Match",
            "304 Not Modified",
            // Budget is now expressed per-ETag (not a fixed ≤6 number)
            "one conditional `If-None-Match` probe per stored ETag",
            "last_seen_updated_at` values",
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
            "TOUCHED_FILE",
            // v1 emission scope
            "v1 emission scope",
            "GitHubIssue` deferred",
            "Review` is promoted",
            "REFERENCES_TASK` from `project.Review",
            "TOUCHES_FILE` from `project.Review",
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

// ─── Behaviour conformance tests (require eg import github implementation) ───
//
// These tests are `#[ignore]` because the `eg import github` CLI ships in a
// follow-up slice. They define the observable contract the implementation must
// satisfy; they become the green suite for that slice.

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn fresh_import_produces_documented_record_shapes() {
    // Set up a wiremock server, configure fixture responses for all six v1
    // endpoints, run `eg import github owner/repo --out <tmp>`, and assert:
    //
    // v1 emission scope (Task + ExternalLink only — GitHubIssue, PR, Review are reserved):
    //   - one Task (source_kind: github_issue) per issue
    //   - one Task (source_kind: github_pr) per pull request
    //   - one ExternalLink (system = "github") per Task
    //   - zero GitHubIssue, zero PR, zero Review records (those kinds are reserved;
    //     deferred to the slice that promotes them from project-graph.md reserved status)
    //   - one EXTERNAL_HANDLE edge per Task → ExternalLink
    //   - handoff metadata record present
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn reimport_unchanged_issues_sends_only_conditional_probes() {
    // Uses a single-page fixture (≤100 items per endpoint) so each endpoint
    // has exactly one stored ETag.
    //
    // 1. Fresh import: repo probe + five repo-wide endpoints respond 200 + ETag.
    //    No per-PR review fetches (deferred to Review-promotion slice).
    // 2. Re-import: repo probe returns 200; all five endpoint probes return 304.
    //    No per-PR review fetches triggered (pulls list unchanged).
    // Assert on the second run:
    //   - Exactly 6 HTTP requests (1 repo-probe + 5 If-None-Match all-304)
    //   - Handoff JSONL contains only the top-level idempotency record (zero per-issue records)
    //   - .github-import-state.json updated with new last_run_at_unix_ms
    //
    // Note: for paginated fixtures (>100 items), the count would be
    //   1 repo-probe + N_pages probes per endpoint. The single-page case is
    //   the canonical budget fixture; multi-page tests are separate.
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn reimport_with_one_changed_issue_produces_only_that_issues_records() {
    // ETag caching is at the repository-wide endpoint level, not per-issue.
    // Change detection within a changed endpoint uses last_seen_updated_at.
    //
    // 1. Fresh import: two issues. /issues?state=all returns 200 + ETag.
    //    Importer stores ETag and last_seen_updated_at for both issues.
    // 2. Re-import: /issues?state=all returns 200 (ETag changed — the list changed).
    //    The new response contains both issues, but only issue #2 has an updated_at
    //    later than the stored last_seen_updated_at. Issue #1 is unchanged.
    // Assert:
    //   - Only issue #2's Task + ExternalLink appear in the delta handoff
    //   - Issue #1 produces no new records (updated_at matches stored value)
    //   - GitHubIssue records are NOT produced (kind is deferred/reserved)
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn auth_missing_exits_with_github_auth_missing_error_code() {
    // Configure wiremock: anonymous repo probe (GET /repos/owner/private-repo)
    // returns 404 (private or non-existent; no token to distinguish).
    // Run `eg import github owner/private-repo --out <tmp>` with no token env vars.
    // Assert:
    //   - Process exits non-zero
    //   - stderr contains `github_auth_missing` (not `github_repo_not_found`)
    //   - stderr explains that a token is required to determine whether the repo exists
    //   - No partial .github-import-state.json is written
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn nonexistent_repo_with_token_exits_with_github_repo_not_found() {
    // Configure wiremock: anonymous probe returns 404, authenticated probe also 404.
    // Run `eg import github owner/gone-repo --out <tmp>` with GH_TOKEN set.
    // Assert:
    //   - Process exits non-zero
    //   - stderr contains `github_repo_not_found` (not `github_auth_missing`)
    //   - No partial .github-import-state.json is written
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn token_strings_in_body_are_redacted_before_persistence() {
    // Configure fixture issue body:
    //   "Fix the config. Token: ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA and done."
    // Run import.
    // Assert (two distinct redaction surfaces):
    //
    // 1. Stdout/stderr scrubber (in-flight): no `ghp_AAAA...` string appears in
    //    any stdout/stderr/log byte before the process exits. The scrubber marker
    //    `[REDACTED_GH_TOKEN]` MAY appear in stderr diagnostics.
    //
    // 2. Persisted graph record (Task.body_handle.inline): the raw token string
    //    MUST NOT appear. The persisted value MUST use the redaction marker grammar
    //    from docs/schema/redaction.md: `<REDACTED:api_token:<hash_prefix>>`.
    //    Checking for `[REDACTED_GH_TOKEN]` in the persisted record is NOT
    //    sufficient — a conforming importer uses the schema marker, not the
    //    scrubber placeholder.
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn pr_review_thread_reconstructs_via_in_reply_to_id() {
    // Requires Review kind to be promoted from reserved.
    // Configure a PR fixture with three review comments in one thread:
    //   comment A (root), comment B (in_reply_to_id = A), comment C (in_reply_to_id = B)
    // Run import. Query the graph.
    // Assert:
    //   - Three Review records with review_kind = "pr_review_comment"
    //   - REFERENCES_TASK is registered Review → Task (Review is FROM, Task is TO);
    //     traversal from PR Task follows INCOMING REFERENCES_TASK edges to find Review records
    //     (i.e., find all Reviews whose REFERENCES_TASK edge points TO this Task)
    //   - Grouping by in_reply_to_id chain yields {A, B, C} as one thread
    //   - Thread root A has no in_reply_to_id
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn per_run_stderr_summary_contains_documented_fields() {
    // Run a fresh import and capture stderr.
    // Assert the summary line contains:
    //   - total requests count
    //   - remaining quota at finish (X-RateLimit-Remaining value)
    //   - total wall-clock time
    todo!("implement after eg import github CLI ships")
}
