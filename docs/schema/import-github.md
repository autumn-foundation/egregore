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

- **Repository probe and auth-missing rule:** Before fetching issue or PR data
  the importer uses a two-step probe:

  **Step 1 — Unauthenticated probe:** `GET /repos/{owner}/{repo}` without token.
  - `200 OK` → public repository; proceed (with token if available, without if absent).
  - `404 Not Found` without token → cannot determine if private or non-existent.
    Exit non-zero with `github_auth_missing`; stderr MUST explain that a token is
    required to determine whether the repository exists.
  - `404 Not Found` with token available → proceed to Step 2.
  - `403` with `X-RateLimit-Remaining: 0` and **token available** → anonymous IP
    quota exhausted, but authenticated quota is a separate 5,000/hour bucket.
    Skip Step 1 backoff entirely and proceed immediately to Step 2 with the token.
    Do NOT exit with `github_auth_rejected` for this case.
  - `403` with `X-RateLimit-Remaining: 0` and **no token** → anonymous quota
    exhausted with no fallback; apply the section 4 rate-limit backoff (wait
    until `X-RateLimit-Reset`, then retry Step 1). Exit non-zero with a
    `github_rate_limit_anon` diagnostic if the wait would exceed 5 minutes.
  - `401` / `403` without rate-limit indicator → exit with `github_auth_rejected`.

  **Step 2 — Authenticated re-probe** (only when Step 1 returned 404 and a token
  is available): `GET /repos/{owner}/{repo}` with `Authorization: Bearer <token>`.
  - `200 OK` → private repository with valid credentials; proceed.
  - `404 Not Found` → repository does not exist or token lacks sufficient access.
    Exit non-zero with `github_repo_not_found`; do NOT use `github_auth_missing`.
  - `401` / `403` → token is invalid or revoked.
    Exit non-zero with `github_auth_rejected`. No retry.

- Every stdout/stderr/log line MUST pass through a token-scrubber that replaces
  any substring matching the patterns below with `[REDACTED_GH_TOKEN]` before
  the bytes leave the process:
  - Classic and OAuth PATs: `gh[pousr]_[A-Za-z0-9]{36,}`
  - Fine-grained PATs: `github_pat_[A-Za-z0-9_]{36,}`

---

## 3 - Canonical GitHub API Surface (REST for v1)

The v1 importer uses the **REST** API at `api.github.com`. GraphQL is
explicitly reserved and not used in v1.

Rationale: REST's ETag-based conditional fetch (`If-None-Match` / `304 Not
Modified`) is the load-bearing idempotency primitive for this policy. GraphQL's
cursor-based pagination and lack of per-resource ETags make it harder to
implement the unchanged-re-import budget guarantee in section 5.

**v1 active endpoints (fetched and ETag-cached):**

All list requests MUST include `per_page=100` to maximise page size before pagination.
GitHub's default is 30 items; omitting `per_page=100` breaks the single-page budget fixture.

| Endpoint | Purpose |
|----------|---------|
| `GET /repos/{owner}/{repo}/issues?state=all&per_page=100` | All issues. **Note:** GitHub's issues API includes pull-request objects; the importer MUST discard any response item where `pull_request` key is present. |
| `GET /repos/{owner}/{repo}/pulls?state=all&per_page=100` | All pull requests |
| `GET /repos/{owner}/{repo}/labels?per_page=100` | Label list (flattened into Task records) |

**Endpoints deferred with `Review` kind promotion:**
These are the intended full v1 surface once `Review` is promoted from reserved.
They are NOT fetched or ETag-cached by the v1 implementation (see section 6).

| Deferred Endpoint | Future Purpose |
|-------------------|---------------|
| `GET /repos/{owner}/{repo}/issues/comments?per_page=100` | All issue comments → `Review` records |
| `GET /repos/{owner}/{repo}/pulls/comments?per_page=100` | All PR review comments → `Review` records |
| `GET /repos/{owner}/{repo}/pulls/{n}/reviews` | Per-PR reviews → `Review` records |

**Pagination rule (normative):** The importer MUST follow the `Link: <url>; rel="next"` header on
every paginated response until no `next` relation is present.
Stopping at page 1 is a conformance violation that silently truncates imports
on any repository with more than 100 issues, PRs, or labels.

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
- When **authenticated** (token present): MUST pause until the
  `X-RateLimit-Reset` UNIX timestamp when `remaining < 100`. The 5,000/hour
  authenticated limit makes this threshold meaningful.
- When **unauthenticated** (public-repo import, no token): the limit is 60/hour.
  Apply a proportional threshold of `remaining < 5` instead, to avoid an
  immediate forced sleep after the very first response.
- HTTP 403 with a secondary-rate-limit body: honour the `Retry-After` header;
  if absent, use exponential backoff starting at 60 s, capped at 600 s, with a
  maximum of 5 retries before exiting non-zero.
- HTTP 429 (primary or secondary rate limit — GitHub may return either 403 or
  429 for rate-limit events): treat identically to the 403 secondary-rate-limit
  case above. Honour `Retry-After` or `X-RateLimit-Reset`; exponential backoff
  if neither header is present; maximum 5 retries before exiting non-zero.
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
  etags: { "<endpoint>?page=<n>": "<etag>" },
  cursors: { "<endpoint>": String | null },
  last_seen_updated_at: {
    issues: RFC3339,
    pulls: RFC3339
  },
  label_list_hash: String | null,
  resource_hashes: { "<issue|pr>:<n>": "<hash>" },
  code_graph_fingerprint: String | null
}
```

ETag keys include the page number (`?page=<n>`) so each page of a paginated
endpoint has its own stored ETag and can be individually probed on re-import.

**Label change tracking:** GitHub's `/labels` list endpoint does not provide
per-label `updated_at` timestamps (see GitHub REST docs: List labels for a
repository). The `last_seen_updated_at` watermarks cannot be used for label
change detection. Instead, the importer hashes the full sorted label list
(name + color + description) from the most recent 200 response and stores it
in `label_list_hash`. On re-import, if the endpoint returns 200 AND the new
hash differs from the stored hash, all Task records whose `labels` array changed
MUST be re-emitted. If the hash is unchanged but the ETag changed (can happen
if GitHub re-signs the response), no label records are re-emitted.

**Per-resource hash store (`resource_hashes`):** The watermark tie-breaking
rule requires comparing each candidate resource against the previously stored
state. `resource_hashes` provides that storage: it maps `"issue:<n>"` or
`"pr:<n>"` to the BLAKE3 hex hash of the canonical JSON of the key fields
(**all fields that can affect the emitted `Task` or `ExternalLink`:**
`number`, `state`, `state_reason`, `title`, `body`, `labels`, `assignees`,
`milestone` (full object), `updated_at`, `closed_at`, and PR-specific fields
`merged_at`, `draft`, `head.sha`, `head.ref`, `base.ref`, `merge_commit_sha`;
omitting any of these fields from the hash means a change to that field is
permanently missed on re-import). For a PR the hash **also** folds in the
`MERGED_AS` merge-link **resolution outcome** against the current seeded
`--code-graph` (issue #333): the target `Commit` record ID when the
`merge_commit_sha` resolves, or a stable `unresolved` / `ambiguous:<count>` /
`none` marker otherwise. The merge-link output depends on the seed graph while
the PR payload does not, so a re-import whose seed graph now contains (or no
longer contains) the merge commit changes this outcome, changes the hash, and
re-emits the PR **with** its `MERGED_AS` edge/diagnostic — even though the PR
payload is byte-identical. An unchanged seed graph produces the same marker and
stays idempotent (AC8). This participates in change detection only; it never
affects the stable record identity.
On re-import, a resource selected by the `>= last_seen_updated_at` watermark is
compared against its stored hash; if identical, no new record is emitted and the
hash entry is left unchanged. If different, a new record is emitted and the hash
entry is updated. An absent entry is treated as "never imported" — always emit.

**Seed-graph fingerprint gate on the `/pulls` conditional request (`code_graph_fingerprint`, issue #333):**
The merge-link resolution outcome folded into the PR hash (above) can only be
recomputed when the pulls list is actually re-processed. But `/pulls` is a
conditional endpoint: when its cached ETag matches, GitHub replies `304 Not
Modified` and PR processing short-circuits **before** the merge-link marker is
ever computed. GitHub's `/pulls` ETag reflects only the remote PR payload — it
cannot see the local `--code-graph` seed on which merge-link resolution depends.
So a `/pulls` ETag is only trustworthy when the seed graph is **also** unchanged.
The importer therefore persists a `code_graph_fingerprint`: a deterministic,
byte-identical BLAKE3 digest over the sorted `commit_sha → [Commit record id]`
mapping of the seeded code graph, or the distinct stable marker `"none"` when no
`--code-graph` is supplied. Before issuing the conditional `/pulls` request, the
importer compares the current fingerprint against the stored one; if they differ
— including `none → some`, `some → different`, `some → none`, and a missing
(pre-fingerprint) stored value treated as "unknown" — it **suppresses the
`If-None-Match` for `/pulls` only**, so GitHub returns a full `200` payload,
`/pulls` is re-processed, and the merge-link marker (and any `MERGED_AS`
edge/`github_commit_unresolved` diagnostic) is recomputed. Composed with the
round-4 hash marker, an actually-changed merge link is re-emitted while an
unchanged one stays idempotent (zero records). When the fingerprint **matches**,
the `/pulls` ETag fast path is kept (a `304` is allowed) because merge links
cannot have changed. Only `/pulls` is affected — `MERGED_AS` lives solely on PR
tasks — so issues, labels, comments, and reviews keep their own conditional fast
path. After every successful run the current fingerprint is persisted, so the
next unchanged-seed re-import takes the `304` fast path again. The fingerprint
participates in conditional-request gating only; it never affects stable record
identity, and it is **not** part of a `STATE_SCHEMA_VERSION` bump — a
pre-fingerprint state file loads normally with `code_graph_fingerprint = null`,
which reads as "unknown" and forces exactly one fail-safe `/pulls` refetch before
the real fingerprint is stored.

**Prior merge-artifact tracking (`pr_merge_artifacts`, issue #333, Codex
round-6):** the state file also maps `"pr:<n>"` to the **record ID** of the
last-emitted `MERGED_AS` artifact (the merge edge ID on a unique resolution, or
the `github_commit_unresolved` diagnostic ID otherwise; absent when the PR emits
no merge artifact). On a re-import whose merge-resolution outcome **changed**, the
importer emits a `Tombstone` retracting this prior artifact before emitting the
current one (see §6, "Superseded merge-resolution retraction"), so a persistent
store never shows stale + fresh merge evidence for one PR. The full record ID
(not a lossy marker) is stored so a changed `merge_commit_sha` under a
still-unresolved outcome still retracts the diagnostic keyed on the old SHA. The
field is `#[serde(default)]` and is **not** part of a `STATE_SCHEMA_VERSION` bump:
a state file written before it loads normally with an empty map (an absent prior
reads as "no known artifact" and emits no tombstone), avoiding a heavy full
refetch that a version bump would force.

**Deferred endpoint ETag rule:** The v1 importer MUST NOT store ETags for
deferred comment/review endpoints (`/issues/comments`, `/pulls/comments`,
`/pulls/{n}/reviews`). Caching ETags before records are emitted would cause
304 responses to silently skip the backfill when `Review` is later promoted.
Those endpoints start being fetched and ETag-cached only after Review promotion.

**Re-run behaviour:**

- On re-run, send `If-None-Match: <stored-etag>` for each endpoint that has a
  cached ETag.
- On `304 Not Modified`: skip that page; the stored records are unchanged.
- On `200 OK`: update the stored ETag and continue processing.

**Watermark tie-breaking rule:** When a changed endpoint returns 200, the
importer uses `>= last_seen_updated_at` (inclusive) to select candidates.
Using strict `>` would silently drop resources updated at the exact watermark
timestamp during a concurrent write tick. After selecting candidates, the
importer MUST compare the full resource state (by hashing key fields) against
the previously stored state before emitting a new record, so resources that
match the watermark but are genuinely unchanged do not produce duplicate records.

**Normative budget:**

- An unchanged re-import MUST issue exactly one conditional `If-None-Match` probe per stored ETag
  (one per page per endpoint) plus one repo-probe, and produce a
  handoff with only the top-level idempotency record (zero per-issue records).
  For repositories whose content fits on a single page per each of the 3 active
  v1 endpoints (issues, pulls, labels) this totals 4 requests
  (1 repo-probe + 3 endpoint probes); paginated repositories require
  one additional probe per extra stored page.
- Per-PR `/pulls/{n}/reviews` requests fire only when Review is promoted and
  the pulls list changes on re-import.
- A changed endpoint re-import MUST produce only the records for resources that
  changed since the last run (identified by comparing stored `last_seen_updated_at` values
  against the re-fetched resource list) plus the top-level handoff metadata record. Per-PR review fetches (`/pulls/{n}/reviews`) count against the run total
  but are NOT part of the unchanged-re-import conditional budget.

**Failure rule:** if the importer exits non-zero due to auth failure
(`github_auth_missing` or `github_auth_rejected`), MUST NOT write or update
the state file. A partial state file from a crashed previous run is detected
via `schema_version` field validation; an invalid or truncated file is treated
as a missing state file.

**State-format version (`STATE_SCHEMA_VERSION`, issue #333):** the state file
carries its own format version, bumped `1 → 2` when the emitted per-resource
contract changes in a way a cached conditional probe could otherwise hide. A
loaded state whose version does not match the current binary is discarded (same
path as a missing/partial file), so upgrading forces exactly ONE full refresh
that re-fetches every endpoint and re-emits the current contract — this is what
lets the #333 promoted flat PR `Task` fields reach existing importer users whose
pre-#333 `/pulls` ETag would otherwise return HTTP 304 and skip the pulls branch.
The refresh writes a current-version state, so a subsequent unchanged re-import
is idempotent again (emits zero per-resource records). This is strictly a
state-cache migration; it never backfills already-persisted AletheiaDB stores.

Issue #334 bumps `STATE_SCHEMA_VERSION` again (`2 → 3`) for the same reason on the
review side: reviews began emitting a new `review_commit_sha` field and
`REVIEWS_COMMIT` edge, and a pre-#334 cached review-endpoint ETag
(`/pulls/comments`, `/pulls/{n}/reviews`) would otherwise 304 and suppress the new
contract for unchanged reviews. Unlike the `1 → 2` bump, the `2 → 3` bump does **not**
discard the whole file: a v2 file carries #333's `pr_merge_artifacts` tracking, so a
blunt discard could leave a stale + fresh merge artifact both live on the first v3
run. The loader instead **migrates** v2 → v3, preserving every field that provides
idempotency or prior-artifact tracking (resource hashes, watermarks, the
`code_graph_fingerprint`, and `pr_merge_artifacts`; the additive
`review_commit_artifacts` map is `#[serde(default)]`) and **clearing every ETag**.
Clearing all ETags — including the `/pulls` list ETag — is required, not just the
review-endpoint ones: the per-PR `/pulls/{n}/reviews` fetch is gated behind
`if pulls_changed`, which is true only when the `/pulls` list returns 200, so a
preserved `/pulls` list ETag would let an unchanged PR list 304 and the per-PR
reviews would never be fetched, silently dropping the review-anchor contract for
existing reviews on upgrade. The forced full refetch stays idempotent for every
non-review domain because emission is gated by the preserved resource hashes (an
unchanged issue/PR/label re-fetches but its unchanged hash suppresses re-emission),
while the v2→v3 review-hash formula change re-emits each existing review exactly once
with the new anchor. In steady state the commit-anchored review endpoints are
additionally gated on the same `code_graph_fingerprint` as `/pulls` so a later-added
code graph still re-resolves review anchors (not only across the one-time migration).

---

## 6 - GitHub-to-Record-Shape Mapping

> **Implementation status (issue #46):** the `Review` kind has since been
> promoted from reserved. The shipped `eg import github` importer now emits
> `Task` + `ExternalLink` per issue/PR **and** `project.Review` records for
> issue comments (`review_kind: issue_comment`), PR review summaries
> (`pr_review`), and PR review comments (`pr_review_comment`), wired with
> `REFERENCES_TASK` and (for file-anchored review comments resolvable against a
> seeded code graph) `TOUCHES_FILE` edges. The "v1 emission scope" table below
> records the original first-slice plan; the deferral notes it contains are
> superseded by the #46 slice. The workflow is documented in
> [`docs/cli/github-import.md`](../cli/github-import.md).

Single normative reference. The target record kinds are defined in
[`docs/schema/project-graph.md`](project-graph.md).

**Full intended mapping (normative target shape for final implementation):**

| GitHub Resource | Intended Target Record(s) | Key Fields |
|-----------------|--------------------------|-----------|
| Issue | `Task` + `GitHubIssue` + `ExternalLink` | `status` (from `state` + `state_reason`), `number`, `url`, `labels`, `milestone`, `assignees`, `author`, `created_at`, `updated_at`, `closed_at` |
| Pull Request | `Task` + `PR` + `ExternalLink` | `status` (from `state` + `merged_at` + `draft`; use `merged_at != null` to distinguish merged vs closed-unmerged — the `/pulls?state=all` list endpoint returns `merged_at` as RFC3339-or-null, NOT the `merged` boolean which is only on per-PR GET responses), `number`, `url`, head/base refs, merge commit SHA, requested reviewers. **Note:** `mergeable_state` is NOT available from the `/pulls?state=all` list endpoint; GitHub computes mergeability lazily for individual PR GETs only. v1 omits `mergeable_state` unless a per-PR GET is explicitly added to the active endpoint set. |
| Issue Comment | `Review` (`review_kind: "issue_comment"`) → `REFERENCES_TASK` | Attached to parent `Task`; `body` passes through redaction |
| PR Review | `Review` (`review_kind: "pr_review"`, `state` preserved) → `REFERENCES_TASK` | Attached to parent PR `Task` |
| PR Review Comment | `Review` (`review_kind: "pr_review_comment"`, `file_path`, `line`, `start_line`, `side`, `diff_hunk` summary, `in_reply_to_id`) → `REFERENCES_TASK` + `TOUCHES_FILE` | Attached to parent PR `Task` and `File` node when file exists at PR head SHA |
| Label | Flattened into `Task.labels` array | No separate record kind in v1 |
| Milestone | Stored in `Task.body_handle` as part of the GitHub-only metadata blob (no `Task.milestone` field exists in the v1 `Task` schema — see [`docs/schema/project-graph.md`](project-graph.md) section 3; `Task.milestone` is undefined in v1 and emitting it would be rejected) | No separate record kind in v1 |

**v1 emission scope (what the first implementation actually emits):**

`GitHubIssue`, `PR`, and `Review` are marked **reserved** in
[`docs/schema/project-graph.md`](project-graph.md) with no defined payload.
v1 MUST NOT emit records whose shape is undefined; those three kinds ship in a
follow-up slice that promotes them from reserved.

| GitHub Resource | v1 Emission |
|-----------------|------------|
| Issue | `Task + ExternalLink` only (`source_kind: github_issue`). GitHub-only metadata (`state_reason`, `milestone`, etc.) stored in `Task.body_handle` for round-trip fidelity; `GitHubIssue` deferred. `Task.priority` defaults to `unknown` (GitHub issues have no native priority field; a future label-mapping rule may override this). |
| Pull Request | `Task + ExternalLink` (`source_kind: github_pr`). Six PR-specific fields are promoted to first-class **optional flat `Task` fields** (issue #333): `head_sha`, `head_ref`, `base_ref`, `merge_commit_sha`, `merged_at`, `draft`. Present only on PR-derived Tasks; issue Tasks omit them (serde-skipped). They are additive plaintext query substrate (see §8) and continue to also appear inside `Task.body_handle` for round-trip fidelity, so pre-#333 body-blob readers are unaffected. The `merge_commit_sha` flat field is populated only for actually-merged PRs (`merged_at` present); an unmerged PR never carries it even when the REST payload supplied a temporary test-merge SHA. A merged PR whose `merge_commit_sha` resolves against a seeded `--code-graph` `Commit` also emits a `MERGED_AS` `Task → Commit` edge (resolve-or-diagnose, below). Remaining PR-only fields (requested reviewers, `mergeable_state`, …) stay deferred to `PR` record promotion. `Task.priority` defaults to `unknown`. Downstream consumers: issues #334 (compliance/evidence surfaces) and #338 build directly on this exact field schema. |
| Issue Comment | **Deferred.** Comment endpoints are still fetched and ETag-cached; records emitted when `Review` is promoted. |
| PR Review | **Deferred.** Same rationale as issue comments. |
| PR Review Comment | **Deferred.** Same rationale. `REFERENCES_TASK` from `project.Review` and `TOUCHED_FILE` from `project.Review` must also be registered in `project-graph.md` before emission. |

**Edge pre-registration note:** When the `Review` kind is promoted, two new
edge registrations must be added to the cross-domain edge table in
[`docs/schema/project-graph.md`](project-graph.md):
- `REFERENCES_TASK` from `project.Review` TO `project.Task` (extends the existing registration to add `project.Review` alongside the `agent_memory` FROM kinds)
- `TOUCHES_FILE` from `project.Review` TO `codegraph.File` (extends the existing registration to add `project.Review` alongside the `project.Task` FROM kind)

**`MERGED_AS` edge (issue #333):** A PR `Task` whose promoted `merge_commit_sha`
resolves against a seeded `--code-graph` is linked to the merge commit it landed
as.

| Label | FROM | TO | Meaning |
|-------|------|----|---------|
| `MERGED_AS` | `project.Task` (`source_kind: github_pr`) | `codegraph.Commit` | The PR was merged as this specific commit. |

Resolution is **resolve-or-diagnose** (mirroring the `TOUCHES_FILE` discipline):
the flat `merge_commit_sha` field and the `MERGED_AS` edge/diagnostic are emitted
**only for actually-merged PRs** (`merged_at` present). For a
mergeable-but-unmerged PR (open, or closed-unmerged) GitHub's REST API can
populate `merge_commit_sha` with a *temporary test-merge* commit rather than a
landed merge commit; that SHA is never merge evidence, so an unmerged PR carries
no flat `merge_commit_sha` field and produces neither a `MERGED_AS` edge nor a
`github_commit_unresolved` diagnostic — even when the seeded `--code-graph`
contains a `Commit` with that exact SHA. For a merged PR,
a `merge_commit_sha` matching **exactly one** `Commit` (whose `name` equals the
SHA) emits one `MERGED_AS` edge; **zero or multiple** matches emit a project
`Diagnostic` node with code `github_commit_unresolved` carrying the SHA and the
Task record ID — never a guessed link. This diagnostic's stable ID is
**repo-scoped** (issue #333, Codex round-7): `source_repo` is part of the ID
composition, exactly like the Task/Review/ExternalLink IDs, so a shared
multi-repo store never collides diagnostics for the same PR number + merge SHA
across repositories (one repo's import could otherwise overwrite or tombstone
another repo's merge evidence). A merged PR with no `merge_commit_sha`
emits neither edge nor diagnostic. Without a seeded
`--code-graph`, no `MERGED_AS` edges and no unresolved diagnostics are produced.
`MERGED_AS` is a project-only, evidence-class edge label: it is rejected on
`agent_memory:v1:` edges, exactly like `TOUCHES_FILE` and `EXTERNAL_HANDLE`. The
edge itself is a **project-domain edge**: its own record ID carries the
`project:v1:` prefix and `PROJECT_SCHEMA_VERSION`, so the daemon project-edge
validator sees it and `project:v1:` consumers find the link (its `Commit` *target*
stays a `codegraph:` node). Only the `(Task, Commit, MERGED_AS)` triple
identifies the edge — the promoted flat PR fields never enter its stable ID, so
output stays byte-identical across runs.

**Superseded merge-resolution retraction (issue #333, Codex round-6):** the
importer is otherwise purely additive, so when a PR's merge-resolution **outcome
changes** on a re-import — the SHA newly resolves, stops resolving, resolves to a
different `Commit`, or the merged `merge_commit_sha` itself changes — the *new*
artifact carries a *new* record ID and, without retraction, the *prior* artifact
(edge or diagnostic) would linger live in a persistent store, so both stale and
fresh merge evidence would coexist for one PR. To prevent that, the importer
persists the last-emitted merge artifact's record ID per PR (`pr_merge_artifacts`
in the state file) and, when the current outcome differs, emits a project-domain
`Tombstone` naming the prior artifact via `deleted_id` **before** emitting the
current outcome. In a persistent embedded store the tombstone suppresses the
superseded record from the current read view (a later write's higher sequence
wins), so the current view shows only the fresh outcome. Handled transitions
include resolved-A → resolved-B, resolved → unresolved, unresolved → resolved,
unresolved ↔ ambiguous, and a changed merged `merge_commit_sha` under a
still-unresolved outcome (the diagnostic keyed on the old SHA is retracted). When
the outcome is **unchanged** the change hash is unchanged, the PR is skipped, and
no tombstone is emitted (AC8 idempotency preserved). The tombstone's own ID is
derived deterministically from `(pr, deleted_id)`, so output stays byte-identical
across runs. When a resolution **cycles back** to a previously resolved-and-then-
tombstoned outcome (resolved-A → unresolved/B → resolved-A), the re-emitted
artifact reconstructs the *same* record ID and bytes as the first run; the
embedded sink therefore **revives the tombstoned ID** by forcing a fresh
observation whenever a re-emitted record's stable ID is actively tombstoned
(issue #333, Codex round-7), so the newer sequence post-dates the tombstone and
the current read view surfaces the re-resolved merge link again rather than
leaving it suppressed. The full prior record ID (not a lossy marker) is persisted so the
old-SHA diagnostic case retracts correctly; the `pr_merge_artifacts` field is
`#[serde(default)]`, so legacy state files load without a state-schema bump (an
absent prior is treated as "no known artifact" and emits no tombstone).

**`review_commit_sha` field + `REVIEWS_COMMIT` edge (issue #334):** the review-side
mirror of `MERGED_AS`. Every `pr_review` and `pr_review_comment` `Review` record
carries an optional flat `review_commit_sha` field sourced from the GitHub
payload's `commit_id` — the exact commit the reviewer looked at. Unlike a PR's
`merge_commit_sha` (which may be a throwaway *test-merge* SHA and is therefore
gated on `merged_at`), a review's `commit_id` is a real observed commit, so
`review_commit_sha` is populated **whenever the payload carries `commit_id`, with
no gate**. `issue_comment` reviews are general PR-conversation comments with no
commit anchor: they are **exempt** — `review_commit_sha` stays absent and no
`REVIEWS_COMMIT` edge or unanchored diagnostic is ever emitted for them.

| Label | FROM | TO | Meaning |
|-------|------|----|---------|
| `REVIEWS_COMMIT` | `project.Review` (`source_kind: github_review`) | `codegraph.Commit` | The review was anchored to (looked at) this specific commit. |

Resolution is **resolve-or-diagnose** against a seeded `--code-graph`, with three
outcomes for a commit-anchored review: (1) `commit_id` matching **exactly one**
`Commit` (whose `name` equals the SHA) emits one `REVIEWS_COMMIT` edge; (2)
`commit_id` present but matching **zero or multiple** `Commit`s emits a project
`Diagnostic` node with code `github_commit_unresolved` carrying the SHA and the
Review record ID — never a guessed link; (3) `commit_id` **genuinely absent** on a
commit-anchored review emits a distinct `github_review_unanchored` diagnostic
(diagnose the gap; never fabricate a SHA). Both diagnostic IDs are **repo-scoped**
(they key on the already-repo-scoped Review record ID, which embeds `source_repo`),
so a shared multi-repo store never collides review-anchor diagnostics across
repositories (contract mirrors `github_commit_unresolved` for `MERGED_AS`). Without
a seeded `--code-graph`, `review_commit_sha` is still populated but no
`REVIEWS_COMMIT` edges and no diagnostics are produced. `REVIEWS_COMMIT` is a
project-only, evidence-class edge label: it is rejected on `agent_memory:v1:`
edges, exactly like `MERGED_AS`. The edge itself is a **project-domain edge**: its
own record ID carries the `project:v1:` prefix and `PROJECT_SCHEMA_VERSION`, so the
daemon project-edge validator sees it (and requires the FROM `Review` node's
`source_kind` to be `github_review`, the review-side analog of `MERGED_AS`'s
`github_pr` gate); its `Commit` *target* stays a `codegraph:` node. "Anchored at
this SHA" is **not** "approved all changes in a range": the edge names the tree the
review observed, never a verdict on it. Only the `(Review, Commit, REVIEWS_COMMIT)`
triple identifies the edge, so output stays byte-identical across runs.

The review-anchor resolution outcome is folded into each review's change-detection
hash and the commit-anchored review endpoints (`/pulls/comments`,
`/pulls/{n}/reviews`) are gated on the same seed-graph fingerprint as `/pulls`, so
a code graph added or changed after the reviews were first imported re-resolves and
re-emits the anchor even across a would-be `304`; an unchanged seed keeps re-imports
idempotent. Superseded review-anchor artifacts are retracted via a project-domain
`Tombstone` exactly as for `MERGED_AS` (prior artifact IDs stored per review in
`review_commit_artifacts`, an additive `#[serde(default)]` state field), and the
generic revive-after-tombstone rule covers a resolved → superseded → resolved cycle.
Because reviews now emit a new field + edge that a cached review `ETag` could hide,
the state-schema version is bumped (see §5 idempotency), forcing exactly one full
refresh on the first upgraded run.

**`valid_time_source`:** All GitHub-sourced records use `github_updated_at`.
**`source_kind`:** Issues use `github_issue`; PRs use `github_pr`; every `Review`
node (issue_comment / pr_review / pr_review_comment) uses `github_review` — the
daemon `REVIEWS_COMMIT` project-edge validator requires this exact source kind on
the FROM node (issue #334).

---

## 7 - PR Review Thread Normalization

A "thread" is the transitive closure of `Review` records connected by
`in_reply_to_id`.

**Rules:**

- A `ReviewThread` is NOT a separate record kind in v1. Threads are
  reconstructible via traversal and are reserved for a future `ReviewThread`
  aggregation record.
- **REST limitation on thread resolution state:** GitHub's REST review-comment
  API exposes `in_reply_to_id` (reply structure) but does NOT expose whether a
  thread is resolved (`isResolved`). That field is only available through the
  GraphQL API, which is reserved for v2. Therefore, v1 stores all review-comment
  threads without a resolved/unresolved distinction; the "merged with unresolved
  feedback" query state is deferred to the GraphQL slice.
- Traversal (once `Review` is promoted from reserved): `REFERENCES_TASK` is
  registered FROM `project.Review` TO `project.Task`, so traversal from a PR
  `Task` follows **incoming** `REFERENCES_TASK` edges (i.e., find all `Review`
  records that point TO this `Task`). Group the resulting `Review` records with
  `review_kind: "pr_review_comment"` by the `in_reply_to_id` chain to reconstruct
  threads. Resolution state is not stored in v1.
- **Commit anchoring (issue #334):** each `pr_review` and `pr_review_comment`
  `Review` carries `review_commit_sha` (the payload `commit_id` — the exact commit
  reviewed) and, when a seeded `--code-graph` resolves that SHA, a `REVIEWS_COMMIT`
  edge to the `codegraph.Commit`. `issue_comment` reviews are exempt (no anchor).
  A review's anchor SHA commonly differs from the PR head SHA after a force-push;
  the anchor names the tree the reviewer actually saw, never a range verdict.

---

## 8 - Redaction Field Map

These GitHub fields pass through the redaction pipeline defined in
[`docs/schema/redaction.md`](redaction.md) before persistence:

| GitHub field | Redacted | Notes |
|-------------|---------|-------|
| `Issue.title` | yes | Maps to `Task.title`; `project-graph.md` requires `Task.title` to be redacted |
| `Issue.body` | yes | Full body |
| `Issue.comments[].body` | yes | All comment bodies |
| `PullRequest.title` | yes | Maps to `Task.title`; same rule as `Issue.title` |
| `PullRequest.body` | yes | PR description body |
| `Review.body` | yes | PR review summary body |
| `ReviewComment.body` | yes | Inline review comment body |
| `ReviewComment.diff_hunk` | yes (partial) | diff_hunk sub-policy below |

**Plaintext fields (queryable, never redacted):**

Repo name, issue/PR number, state, author login, `created_at`, `updated_at`,
`closed_at`, merge commit SHA, head/base branch names, review anchor commit SHA
(`Review.review_commit_sha`, issue #334).

**First-class plaintext PR `Task` fields (issue #333):** The six promoted flat
`Task` fields — `head_sha`, `head_ref`, `base_ref`, `merge_commit_sha`,
`merged_at`, and `draft` — are permitted plaintext query substrate and are
**deliberately NOT routed through the redaction pipeline**. They are not listed
in the sensitive-field index (`docs/schema/redaction.md`) and therefore pass the
redaction gate unchanged: a commit SHA, branch name, merge timestamp, or draft
flag is non-secret structural metadata that must remain joinable and citable.
They survive verbatim in a redaction-on export. Downstream consumers #334
(compliance/evidence) and #338 rely on this plaintext guarantee.

**First-class plaintext `Review.review_commit_sha` (issue #334):** The review
anchor SHA on `pr_review` / `pr_review_comment` `Review` records is a commit SHA —
non-secret structural metadata — and inherits the same §8 plaintext-SHA carve-out
as the #333 PR fields above: it is **deliberately NOT routed through redaction**,
is not listed in the sensitive-field index (`docs/schema/redaction.md`), and
survives verbatim in a redaction-on export so review→commit anchors stay joinable
and citable. `review_side` (LEFT/RIGHT) shares this treatment.

**Redacted body-stored metadata:** Milestone title (`Task.body_handle` field) is
NOT in the plaintext carve-out. `Task.body_handle.inline` is a redactable field
per [`docs/schema/project-graph.md`](project-graph.md) section 8; milestone
titles MAY contain operator-entered text with secret-like values and MUST pass
through the redaction pipeline before persistence.

**Redaction note:** `Task.labels`, `Task.assignees`, and `ExternalLink.url` are
NOT plaintext — they MUST pass through the redaction policy before persistence,
per [`docs/schema/project-graph.md`](project-graph.md) and
[`docs/schema/redaction.md`](redaction.md). This aligns with how the local-JSONL
importer handles the same fields.

**diff_hunk sub-policy:** Preserve file path, line numbers, and structural
diff markers (`+`, `-`, `@@`). Redact only value-bearing content lines whose
content matches a known secret pattern from the redaction policy (e.g., lines
containing `TOKEN=`, `SECRET=`, or matching the token scrubber patterns
`gh[pousr]_[A-Za-z0-9]{36,}` or `github_pat_[A-Za-z0-9_]{36,}`).

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
| Issue `ExternalLink` | `(system="github", system_native_id="issue:<n>")` following the `(system, system_native_id)` shape from `project-graph.md` section 9 |
| PR `ExternalLink` | `(system="github", system_native_id="pr:<n>")` — distinct from the issue ExternalLink even for the same numeric n, because GitHub PRs and issues share a number space but differ in kind |
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
| Webhook / review-agent receiver | (separate push-import surface) | A future capability where GitHub pushes events to a long-lived receiver so an agent can respond to review comments. This is a **push** surface, distinct from this **pull** importer, and is exactly the `watch mode` reserved in section 1. It requires its own ADR and network-boundary review, would carry its own auth model (App JWT + installation tokens + webhook-secret HMAC), and is a candidate consumer of a GitHub App SDK (e.g. `github-bot-sdk`). It MUST NOT be wired into this token-based pull importer. |

Both org/file slices MUST NOT be wired to the daemon process when they ship.

---

## 11 - Conformance Tests

File: `tests/import_github.rs`, using mocked HTTP (no live network access).

The test suite covers:

| Test | Assertion |
|------|-----------|
| Fresh import | v1 scope: one `Task + ExternalLink` per issue/PR; zero `GitHubIssue`/`PR`/`Review` (reserved) |
| Unchanged re-import (single-page) | One `If-None-Match` probe per stored ETag + repo-probe; handoff contains zero per-issue records |
| Changed-issue re-import | Endpoint returns 200; importer uses `last_seen_updated_at` to emit only changed issue's records |
| Auth-missing run (no token) | Exits with `github_auth_missing`; no partial state file |
| Auth-missing run (token + 404) | Exits with `github_repo_not_found`; no partial state file |
| Token in body | `ghp_`/`github_pat_` strings absent from `Task.body_handle.inline`; `<REDACTED:api_token:hash>` present |
| PR review thread | Three `Review` records; incoming `REFERENCES_TASK` traversal from Task; `in_reply_to_id` chain |
| Review commit anchor (issue #334) | `pr_review`/`pr_review_comment` carry `review_commit_sha` from `commit_id`; a seeded `--code-graph` resolving the SHA emits one `REVIEWS_COMMIT` `project.Review → codegraph.Commit` edge (`project:v1:` id) |
| Review anchor resolve-or-diagnose (issue #334) | Zero/multiple `Commit` matches → `github_commit_unresolved` diagnostic (repo-scoped); absent `commit_id` → `github_review_unanchored`; `issue_comment` exempt (field absent, no diagnostic); SHA never fabricated |
| Review anchor seed-graph invalidation (issue #334) | A code graph added/changed after import re-resolves review anchors across a would-be `304`; outcome change tombstones the superseded artifact; resolved→superseded→resolved revives; unchanged seed stays idempotent |
| Review anchor redaction carve-out (issue #334) | `review_commit_sha` survives a redaction-on export in plaintext; never enumerated as sensitive |
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
