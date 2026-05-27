# GitHub Issues/PRs Import Policy - v1

**Status:** Active at v1. This document is the single source of truth for the
GitHub Issues/PRs import policy — covering auth, network-access boundaries,
redaction field map, GitHub-to-record-shape mapping, idempotency state,
rate-limit handling, and PR-review-thread normalization.

**Schema version:** `schema_version` = `1`.

**CLI entry point:** `eg import github <owner>/<repo>`

Rationale for writing this policy before implementation: the vision PRD names
GitHub as a Priority 1 project/task source, but the GitHub-facing import
contract was unwritten. Shipping without it risks implicit decisions on
pagination, ETags, redaction, idempotency, and network-boundary scope.

Coordination links:

- [`README.md`](../../README.md) — top-level documentation
- [`docs/prd/0000-egregore-vision.md`](../prd/0000-egregore-vision.md) — names this doc as the GitHub import policy
- [`docs/schema/project-graph.md`](project-graph.md) — owns `Task`, `PR`, `Review`, `ExternalLink` target shapes
- [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md) — shares target record shapes; this doc owns GitHub-side fetch policy
- [`docs/schema/redaction.md`](redaction.md) — redaction pipeline; section 8 of this doc names which GitHub fields enter it

---

## 1 - Network-Access Boundary

**Normative MUSTs:**

- The importer is invoked exclusively via the explicit CLI verb `eg import github <owner>/<repo>`.
- The importer MUST NOT run inside daemon process. It runs as a separate `eg`
  subcommand that fetches from GitHub, writes a local JSONL handoff file, then
  exits. The daemon remains a local-only HTTP server.
- Rationale: ADR 0003 freezes the daemon's local-first guarantee. Allowing
  network HTTP clients inside the daemon would violate that guarantee and
  create an implicit background service.
- The importer MUST NOT subscribe to webhooks, poll on a schedule, or
  enumerate any repository beyond the explicitly-named `<owner>/<repo>`.
- **watch mode** (continuous polling or webhook subscription) is explicitly
  reserved as out of scope for v1. It will require its own ADR and network
  boundary review before any implementation.

Future multi-repo shapes (`--org <name>`, `--file <repos.txt>`) are reserved
in section 10 and are not implemented in v1.

---

## 2 - Auth Surface

The v1 auth surface is a **closed enumeration**. No other token source is
permitted without a policy update.

| Source | Description |
|--------|-------------|
| `GH_TOKEN` env var | Canonical CI path. Read first. |
| `GITHUB_TOKEN` env var | GitHub Actions parity alias. Read if `GH_TOKEN` is absent. |
| `gh auth token` subprocess | Canonical local-dev path via the `gh` CLI keychain. Invoked when neither env var is set. Token is used in-process and never persisted to disk. |
| `--token-file <path>` | Operator-managed token file. Read at import time; content never logged. |

Rules:

- NO config file storage of the token. NO daemon-runtime file storage.
- If no token is available **and** the target repository is private (HTTP 404
  on the anonymous probe): exit non-zero with error code `github_auth_missing`.
- Every stdout/stderr/log line MUST pass through a token-scrubber that replaces
  any string matching `gh[pousr]_[A-Za-z0-9]{36,}` with `[REDACTED_GH_TOKEN]`
  before the bytes leave the process.

---

## 3 - Canonical GitHub API Surface (REST for v1)

The v1 importer uses the **REST** API at `api.github.com`. GraphQL is
explicitly reserved and not used in v1.

Rationale: REST's ETag-based conditional fetch (`If-None-Match` / `304 Not
Modified`) is the load-bearing idempotency primitive for this policy. GraphQL's
cursor-based pagination and lack of per-resource ETags make it harder to
implement the unchanged-re-import budget guarantee in section 5.

**Exact v1 endpoints:**

| Endpoint | Purpose |
|----------|---------|
| `GET /repos/{owner}/{repo}/issues?state=all` | All issues (excludes PRs unless state filters differ) |
| `GET /repos/{owner}/{repo}/pulls?state=all` | All pull requests |
| `GET /repos/{owner}/{repo}/issues/comments` | All issue comments |
| `GET /repos/{owner}/{repo}/pulls/comments` | All PR review comments |
| `GET /repos/{owner}/{repo}/pulls/{n}/reviews` | Reviews per PR |
| `GET /repos/{owner}/{repo}/labels` | Label list (flattened into Task records) |

**Reserved for future slices (not implemented in v1):**

| Endpoint | Definition |
|----------|-----------|
| `/repos/{owner}/{repo}/commits` | Commit history backfill |
| `/repos/{owner}/{repo}/statuses/{sha}` | Commit status checks |
| `/repos/{owner}/{repo}/check-runs` | Actions check runs |
| `/repos/{owner}/{repo}/actions/runs` | Actions workflow runs |
| `/repos/{owner}/{repo}/releases` | Release backfill |

---

## 4 - Rate-Limit and Backoff Policy

Rules (normative behaviour for every HTTP response):

- MUST read `X-RateLimit-Remaining` and `X-RateLimit-Reset` headers on every
  response.
- MUST pause until the `X-RateLimit-Reset` UNIX timestamp when
  `remaining < 100`.
- HTTP 403 with a secondary-rate-limit body: honour the `Retry-After` header;
  if absent, use exponential backoff starting at 60 s, capped at 600 s, with a
  maximum of 5 retries before exiting non-zero.
- HTTP 502 / 503 / 504: exponential backoff starting at 5 s, capped at 60 s,
  maximum 5 retries.
- HTTP 401 or HTTP 403 (authentication rejection, not secondary rate): exit
  non-zero immediately with error code `github_auth_rejected`. No retry.

Per-run summary written to per-run stderr at the end of every run:

```text
egregore-github-import: requests=<n> quota_remaining=<n> elapsed=<s>s
```

The summary line must contain: total requests count, remaining quota at
finish (`X-RateLimit-Remaining` from the last response), and total wall-clock
time in seconds.

---

## 5 - Idempotency State File

**Default path:** `<output-dir>/.github-import-state.json`
**Operator override:** `--state-file <path>`

Schema (one JSON object per `<owner>/<repo>`, abbreviated type notation):

```text
{
  schema_version: 1,
  source_repo: "<owner>/<repo>",
  api_base_url: String,
  last_run_at_unix_ms: u128,
  etags: { "<endpoint>": "<etag>" },
  cursors: { "<endpoint>": String | null },
  last_seen_updated_at: {
    issues: RFC3339,
    pulls: RFC3339,
    issue_comments: RFC3339,
    pull_comments: RFC3339
  }
}
```

**Re-run behaviour:**

- On re-run, send `If-None-Match: <stored-etag>` for each endpoint that has a
  cached ETag.
- On `304 Not Modified`: skip that page; the stored records are unchanged.
- On `200 OK`: update the stored ETag and continue processing.

**Normative budget:**

- An unchanged re-import MUST issue ≤6 HTTP requests (one conditional probe
  per v1 endpoint) and produce a handoff with only the top-level
  idempotency record (zero per-issue records).
- A changed-issue re-import MUST produce exactly that issue's records plus
  the top-level handoff metadata record.

**Failure rule:** if the importer exits non-zero due to auth failure
(`github_auth_missing` or `github_auth_rejected`), MUST NOT write or update
the state file. A partial state file from a crashed previous run is detected
via `schema_version` field validation; an invalid or truncated file is treated
as a missing state file.

---

## 6 - GitHub-to-Record-Shape Mapping

Single normative reference. The target record kinds are defined in
[`docs/schema/project-graph.md`](project-graph.md).

| GitHub Resource | Target Record(s) | Key Fields |
|-----------------|------------------|-----------|
| Issue | `Task` + `GitHubIssue` + `ExternalLink` | `status` (from `state` + `state_reason`), `number`, `url`, `labels`, `milestone`, `assignees`, `author`, `created_at`, `updated_at`, `closed_at` |
| Pull Request | `Task` + `PR` + `ExternalLink` | `status` (from `state` + `merged` + `draft`), `number`, `url`, head/base refs, merge commit SHA, `mergeable_state`, requested reviewers |
| Issue Comment | `Review` (`review_kind: "issue_comment"`) → `REFERENCES_TASK` | Attached to parent `Task`; `body` passes through redaction |
| PR Review | `Review` (`review_kind: "pr_review"`, `state` preserved) → `REFERENCES_TASK` | Attached to parent PR `Task` |
| PR Review Comment | `Review` (`review_kind: "pr_review_comment"`, `file_path`, `line`, `start_line`, `side`, `diff_hunk` summary, `in_reply_to_id`) → `REFERENCES_TASK` + `TOUCHED_FILE` | Attached to parent PR `Task` and `File` node when file exists at PR head SHA |
| Label | Flattened into `Task.labels` array | No separate record kind in v1 |
| Milestone | Flattened into `Task.milestone` (`id`, `title`, `due_on`) | No separate record kind in v1 |

**`valid_time_source`:** All GitHub-sourced records use `github_updated_at`.
**`source_kind`:** Issues use `github_issue`; PRs use `github_pr`.

---

## 7 - PR Review Thread Normalization

A "thread" is the transitive closure of `Review` records connected by
`in_reply_to_id`.

**Rules:**

- A `ReviewThread` is NOT a separate record kind in v1. Threads are
  reconstructible via traversal and are reserved for a future `ReviewThread`
  aggregation record.
- Rule: an unresolved review thread on merged PR is a real, queryable state —
  "merged with unresolved feedback" — and MUST be surfaceable via graph query.
- Traversal: from a PR `Task`, follow `REFERENCES_TASK` edges to `Review`
  records with `review_kind: "pr_review_comment"`, group by the `in_reply_to_id`
  chain, and surface unresolved thread roots (comments whose `in_reply_to_id`
  is null and whose thread has at least one unresolved descendant).

---

## 8 - Redaction Field Map

These GitHub fields pass through the redaction pipeline defined in
[`docs/schema/redaction.md`](redaction.md) before persistence:

| GitHub field | Redacted | Notes |
|-------------|---------|-------|
| `Issue.body` | yes | Full body |
| `Issue.comments[].body` | yes | All comment bodies |
| `PullRequest.body` | yes | PR description body |
| `Review.body` | yes | PR review summary body |
| `ReviewComment.body` | yes | Inline review comment body |
| `ReviewComment.diff_hunk` | yes (partial) | diff_hunk sub-policy below |

**Plaintext fields (queryable, never redacted):**

Repo name, issue/PR number, URL, state, labels, assignees, author login,
`created_at`, `updated_at`, `closed_at`, merge commit SHA, head/base branch
names, milestone title.

**diff_hunk sub-policy:** Preserve file path, line numbers, and structural
diff markers (`+`, `-`, `@@`). Redact only value-bearing content lines whose
content matches a known secret pattern from the redaction policy (e.g., lines
containing `TOKEN=`, `SECRET=`, or matching the GitHub token regex
`gh[pousr]_[A-Za-z0-9]{36,}`).

---

## 9 - Stable ID Composition

Following the pattern from `docs/schema/project-graph.md` and the conventions
established by issues #6 and #11:

```text
id = project:v<schema_version>:<blake3(domain || kind || source_repo || github_number || subkind_identity)>
```

Subkind identities:

| Record | `subkind_identity` |
|--------|-------------------|
| Issue `Task` | `"issue:<n>"` |
| PR `Task` | `"pr:<n>"` |
| Issue Comment `Review` | `"issue_comment:<n>:<comment_id>"` |
| PR Review `Review` | `"pr_review:<n>:<review_id>"` |
| PR Review Comment `Review` | `"pr_review_comment:<n>:<comment_id>"` |

**Normative rule:** re-importing identical GitHub state produces byte-identical IDs.
This is the idempotency invariant — if the ID changes between two runs for the
same GitHub resource, it is a bug in ID composition, not a new record.

---

## 10 - Reserved Future Slices

These CLI shapes are reserved with their documented form but are not
implemented in v1:

| Reserved slice | CLI shape | Rationale for deferral |
|----------------|-----------|----------------------|
| Multi-repo org import | `--org <name>` | Org-scoped import expands the network boundary and rate-limit budget non-trivially; requires its own ADR |
| Multi-repo file input | `--file <repos.txt>` | Convenience wrapper over `--org`; blocked on the same boundary review |

Both slices MUST NOT be wired to the daemon process when they ship.

---

## 11 - Conformance Tests

File: `tests/import_github.rs`, using mocked HTTP (no live network access).

The test suite covers:

| Test | Assertion |
|------|-----------|
| Fresh import | Produces documented record shapes and edge counts |
| Unchanged re-import | ≤6 HTTP requests; handoff contains zero per-issue records |
| Changed-issue re-import | Produces exactly that issue's records |
| Auth-missing run | Exits with `github_auth_missing`; no partial state file |
| Token in body | `ghp_` / `ghs_` strings → `[REDACTED_GH_TOKEN]` in Task.summary and Review.body |
| PR review thread | Three comments via `in_reply_to_id`; thread reconstructed |
| Stderr summary | Documented fields present on every run |

Implementation of behaviour tests is deferred to the `eg import github` CLI
slice. The `#[ignore]` stubs in `tests/import_github.rs` form the red suite
for that slice.

---

## 12 - Cross-Issue Coordination

This policy document coordinates with:

- **Issue #14** (Task/AcceptanceCriterion shapes): the GitHub-to-Task/PR/Review
  mapping table in section 6 is normative for the GitHub importer.
- **Issue #17** (local JSONL format): shared target record shapes; this doc
  owns the GitHub-side fetch policy.
- **Issue #4** (redaction pipeline): section 8 of this doc names which GitHub
  fields enter the pipeline.
- **Issue #6** (cross-domain edges): `REFERENCES_TASK` and `TOUCHED_FILE` edges
  from GitHub `Review` records are defined in section 6.
- **Issue #5** (HTTP wire contract): `github_auth_missing` and
  `github_auth_rejected` are reserved on the error-code enum.
- **Issue #18** (runtime-dir layout): the daemon runtime-dir MUST NOT include
  the GitHub idempotency state file; it lives alongside the JSONL handoff in
  the operator-specified `<output-dir>`.

---

## 13 - Schema Versioning

`schema_version` = `1` is this document's version.

**Additive changes (keep `schema_version = 1`):**

- New optional endpoints beyond the six named in section 3.
- New optional fields in the idempotency state file.
- New rows in the redaction field map.
- New rows in the GitHub-to-record mapping table.
- New optional CLI flags.

**Breaking changes (require new `/v2/` bump and `schema_version = 2`):**

- Changing the network-access boundary (e.g., allowing daemon-process network
  calls). This requires its own ADR because it breaks the daemon-process exclusion.
- Renaming or removing record-shape mappings from section 6.
- Changing the ID composition formula in section 9.
- Removing an auth-surface enumeration entry from section 2.
