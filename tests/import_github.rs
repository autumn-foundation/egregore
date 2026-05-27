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
            "unchanged re-import MUST issue",
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
        ],
    );

    // AC 8: PR review thread normalization
    assert_contains_all(
        "docs/schema/import-github.md",
        &[
            "transitive closure",
            "ReviewThread",
            "unresolved review thread on merged PR",
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
    //   - one Task per issue, one Task per PR
    //   - one GitHubIssue per issue, one PR record per pull-request
    //   - ExternalLink with system = "github" for every Task
    //   - Review records for issue comments, PR reviews, and PR review comments
    //   - Edge counts match fixture cardinality
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn reimport_unchanged_issues_sends_at_most_six_conditional_requests() {
    // 1. Run a fresh import against a wiremock server (repo probe + five repo-wide
    //    endpoints respond with 200 + ETag headers; no per-PR review fetches needed
    //    when pulls returns 304).
    // 2. Reset the mock server: repo probe returns 200; five repo-wide endpoints
    //    return 304 Not Modified (no per-PR review fetches triggered).
    // 3. Run a second import with the same --state-file.
    // Assert:
    //   - Exactly 6 HTTP requests on the second run:
    //     1 repo probe + 5 conditional If-None-Match probes (all 304)
    //   - The handoff JSONL contains only the top-level record (zero per-issue records)
    //   - The .github-import-state.json is updated with the new last_run_at_unix_ms
    //   - No /pulls/{n}/reviews requests are issued (pulls list unchanged → skip)
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn reimport_with_one_changed_issue_produces_exactly_that_issues_records() {
    // 1. Fresh import: two issues, all unchanged.
    // 2. Re-import: issue #1 returns 304, issue #2 returns 200 with updated body.
    // Assert:
    //   - Only issue #2's Task and GitHubIssue records appear in the delta handoff
    //   - Issue #1 produces no records on the second run
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
    // Assert:
    //   - The resulting Task.summary (and Review.body for comments) does NOT contain
    //     the literal `ghp_AAAA...` string
    //   - The string `[REDACTED_GH_TOKEN]` appears in its place
    todo!("implement after eg import github CLI ships")
}

#[test]
#[ignore = "requires eg import github implementation (follow-up slice)"]
fn pr_review_thread_reconstructs_via_in_reply_to_id() {
    // Configure a PR fixture with three review comments in one thread:
    //   comment A (root), comment B (in_reply_to_id = A), comment C (in_reply_to_id = B)
    // Run import. Query the graph.
    // Assert:
    //   - Three Review records with review_kind = "pr_review_comment"
    //   - Traversal from PR Task → REFERENCES_TASK → Review records
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
