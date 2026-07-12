# `eg import github` — GitHub Issues/PRs Import

`eg import github <owner>/<repo>` imports one GitHub repository's issues, pull
requests, comments, and reviews into project-graph JSONL. It is the operator
**pull-import** path: it fetches over the REST API, writes a local JSONL handoff
plus an idempotency state file, and exits. It never runs inside the daemon,
polls, subscribes to webhooks, or crawls beyond the named repository.

- Policy (single source of truth): [`docs/schema/import-github.md`](../schema/import-github.md)
- Target record shapes: [`docs/schema/project-graph.md`](../schema/project-graph.md)
- Redaction pipeline: [`docs/schema/redaction.md`](../schema/redaction.md)

This CLI is also reachable as `egregore import github`.

## What it emits

| GitHub resource | Records | Edges |
|-----------------|---------|-------|
| Issue | one `Task` (`source_kind: github_issue`) + one `ExternalLink` | `EXTERNAL_HANDLE` Task→Link |
| Pull request | one `Task` (`source_kind: github_pr`) + one `ExternalLink` | `EXTERNAL_HANDLE` Task→Link |
| Issue comment | one `Review` (`review_kind: issue_comment`) | `REFERENCES_TASK` Review→Task |
| PR review summary | one `Review` (`review_kind: pr_review`) | `REFERENCES_TASK` Review→Task |
| PR review comment | one `Review` (`review_kind: pr_review_comment`) | `REFERENCES_TASK` Review→Task, `TOUCHES_FILE` Review→File* |
| PR timeline `review_dismissed` (issue #336) | one `ReviewStateTransition` (`transition_kind: review_dismissed`) | `TRANSITIONS_REVIEW` Transition→Review |
| PR timeline `review_requested` / `review_request_removed` (issue #336) | one standalone `ReviewStateTransition` | none |

\* `TOUCHES_FILE` is emitted only when the comment's file resolves
**unambiguously** against a seeded code-graph store (see `--code-graph`).
Missing, renamed, or ambiguous files produce a project `Diagnostic` record
carrying the source handles instead of a guessed link.

GitHub-only metadata that has no dedicated v1 `Task` field (`state_reason`,
milestone title, PR `merged_at`/`draft`/head SHA/base ref, merge commit SHA,
`closed_at`) is preserved in the redacted `Task.body_handle` blob for
round-trip fidelity.

## Review-state history (issue #336)

For each PR whose `/pulls` list entry changed, the importer also fetches
`GET /repos/{owner}/{repo}/issues/{n}/timeline` — the same per-PR trigger that
drives the review-summary fetch — and records the closed set of review-state
transitions `{review_dismissed, review_requested, review_request_removed}` as
append-only `ReviewStateTransition` records. Every other timeline event kind is
skipped. Raw timeline text never enters the graph: a dismissal message is
redacted into a `body_handle`, and the actor login and event kind are stored
plaintext.

**Epistemic contract.** A `Review`'s `review_state` field is a **current-state
summary, last-write-wins by design** — a dismissal overwrites `"approved"` →
`"dismissed"` under the SAME review record id. That summary alone cannot tell you
an approval ever existed. The `ReviewStateTransition` records are the **history**:
each transition is a separate append-only record keyed on the timeline event's own
id (never the review's id), so a dismissal never erases the earlier approval. A
`review_dismissed` transition carries a `TRANSITIONS_REVIEW` edge to the review it
dismissed. **A consumer that needs "review state as of time T" (e.g. review
coverage evaluated retroactively) must join the transitions, not read the summary
field.** A transition names *which* review changed and *when* — never that the
change was correct.

## Authentication

The token source is a closed enumeration, resolved in order:

1. `GH_TOKEN` environment variable
2. `GITHUB_TOKEN` environment variable
3. `--token-file <path>` (read at import time; never logged)
4. `gh auth token` (the local GitHub CLI keychain)

No token is stored to disk, and tokens never appear in stdout/stderr: every
diagnostic line passes through a scrubber that replaces GitHub-token-shaped
substrings with `[REDACTED_GH_TOKEN]`. Public repositories import anonymously
when no token is available.

## Shortest local workflow

Import a repository, inspect the result by domain and kind, ingest into a
temporary local AletheiaDB store, and read back a citable record:

```bash
# 1. Import issues, PRs, comments, and reviews into a JSONL handoff.
#    (Set GH_TOKEN for private repos; public repos work anonymously.)
eg import github octocat/Hello-World --out github.graph.jsonl

# 2. Inspect what was imported — counts by domain and kind.
eg inspect github.graph.jsonl
#   records: 42
#   nodes: 30
#   edges: 12
#   schema_version project Task v1: 8
#   schema_version project Review v1: 9
#   schema_version project ExternalLink v1: 8
#   ...

# 3. Ingest into a temporary local store (dry-run needs no daemon/AletheiaDB).
eg ingest github.graph.jsonl --adapter dry-run
#   attempted: 42
#   succeeded: 42
#   failed: 0

#    Or into a real embedded store under a temporary data dir:
eg ingest github.graph.jsonl --adapter embedded --data-dir /tmp/eg-github

# 4. Prove a GitHub issue/PR or review is citable from the graph.
#    Every Task carries a stable entity_id and a source_external_link_id;
#    every Review carries review_kind, system_native_id, and a REFERENCES_TASK
#    edge back to its parent Task. For example, list the imported tasks:
grep '"kind":"Task"' github.graph.jsonl \
  | jq -r '"\(.source_kind)\t\(.entity_id)\t\(.title)"'

#    …and the review feedback that references them:
grep '"kind":"Review"' github.graph.jsonl \
  | jq -r '"\(.review_kind)\t\(.system_native_id)"'
```

To resolve PR-review-comment file anchors against your code graph, scan the repo
first and pass the result as `--code-graph`:

```bash
eg scan . --out code.graph.jsonl
eg import github octocat/Hello-World --out github.graph.jsonl --code-graph code.graph.jsonl
# Review-comment records whose file exists in code.graph.jsonl now carry a
# TOUCHES_FILE edge to the matching File record.
```

## Idempotency

Re-importing an unchanged repository is conditional and cheap: the importer
stores per-page ETags and update watermarks in `.github-import-state.json`
(next to the handoff output by default; override with `--state-file`). On
re-import it sends `If-None-Match` for every cached endpoint; unchanged
endpoints return `304 Not Modified` and emit zero per-resource records (only a
top-level handoff record). When a single issue or PR changes, only that
resource's records are re-emitted. Re-importing identical state produces
byte-identical record IDs.

The `/pulls` conditional fast path is additionally gated on a fingerprint of the
seeded `--code-graph`, because PR merge-link resolution (`MERGED_AS`) depends on
the local seed graph that GitHub's ETag cannot see. A **changed** seed graph
(added, removed, or a different set of `Commit` SHAs — including running with
`--code-graph` after a run without it) suppresses the `/pulls` ETag so GitHub
returns a full payload and merge links are recomputed; an **unchanged** seed
graph keeps the `/pulls` `304` fast path since merge links cannot have changed.
Only `/pulls` is refetched — issues, labels, comments, and reviews keep their own
conditional fast path.

## Failure modes

Failures exit non-zero with a stable, token-scrubbed `{"code":"github_..."}`
diagnostic on stderr that names the failing source class:

| Code | Meaning |
|------|---------|
| `github_auth_missing` | Repo probe returned 404 and no token is available (cannot tell private from non-existent). |
| `github_repo_not_found` | Repo does not exist or the token lacks access. |
| `github_auth_rejected` | The token was rejected (401/403). |
| `github_rate_limit_anon` | Anonymous quota exhausted and the reset wait exceeds the cap. |
| `github_fetch_failed` | A fetch failed after the retry budget (5xx / rate limit). |
| `github_invalid_response` | A response body was not valid JSON. |
| `github_invalid_repo_arg` | The argument was not in `<owner>/<repo>` form. |

On auth failure the state file is **not** written, so a failed run leaves no
partial state behind.

## Flags

| Flag | Purpose |
|------|---------|
| `--out <path>` | Output JSONL handoff path (required). |
| `--state-file <path>` | Idempotency state file. Defaults to `<out-dir>/.github-import-state.json`. |
| `--code-graph <path>` | Seeded code-graph JSONL (from `eg scan`) for `TOUCHES_FILE` resolution. |
| `--token-file <path>` | Operator-managed token file. |
| `--api-base <url>` | API base URL override (for testing against a mock server). |
| `--transaction-time <rfc3339>` | Fixed transaction time for deterministic output. |

## Out of scope (v1)

No webhook receiver, watch mode, scheduled polling, org-wide import, multi-repo
crawling, two-way sync back to GitHub, or hosted indexing service. Multi-repo
shapes (`--org`, `--file`) are reserved in the policy and require their own ADR.
A future webhook/review-agent slice (a separate push-import surface with its own
ADR and network-boundary review) may build on a GitHub App SDK; it is
intentionally not part of this pull-import workflow.
```
