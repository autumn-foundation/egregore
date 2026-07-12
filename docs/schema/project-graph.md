# Project-Graph Domain Schema - v1

**Status:** Active. This document is the single source of truth for the
project-graph domain schema.

**Schema version:** `schema_version` = `1` (`PROJECT_SCHEMA_VERSION`).

**Domain:** `project`.

The project graph records intent-shaped work state: tasks, acceptance
criteria, source-system handles, and reserved project-management shapes. It
does not replace code-graph facts, verification evidence, or agent-memory
claims. It gives future GitHub issue and local JSONL importers a contract so
they do not invent inline task fields.

GitHub import policy: [`docs/schema/import-github.md`](import-github.md) is
the single source of truth for auth, network-access boundaries, redaction field
map, GitHub-to-record-shape mapping, idempotency state, rate-limit handling,
and PR-review-thread normalization. That document's mapping table in section 6
is normative for any GitHub importer that writes records into this domain.

## 1 - Trust Class

Project-graph records are **intent-shaped, externally-anchored when possible**.

Rules:

- GitHub-sourced records MUST carry an `ExternalLink` to the canonical GitHub
  URL and MUST use the GitHub-side `updated_at` as `valid_time_source`.
- Local-JSONL-sourced records MUST carry a `source_handle` containing file
  path, line or record ID, and hash. The local JSONL `source_handle` is
  `<encoded_file_path>:<encoded_local_id>:<record_hash>` as concretized by
  [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md). The source
  line's `updated_at` is the `valid_time`, with `valid_time_source` set to
  `local_jsonl_updated_at`.
- Project-graph records MAY express intent but MUST NOT impersonate code-graph
  or verification facts. Closing a `Task` does not produce a `Verification`.
  Merging a `PR` does not produce a `Change`. Future cross-walk slices may
  emit edges, but never silent overwrites.
- Project-graph records carry `confidence` only for fields the importer had to
  guess, such as status enum mapping from a non-standard label. They do not
  carry confidence for `id`, `title`, or `body_handle`.
- Project-graph records are mutable. Status, labels, assignees, and body can
  change over time. The bi-temporal selector from #8 is the read mechanism:
  every mutation writes a new row with a new `transaction_time` and the same
  `entity_id`.

## 2 - Shared Fields

Every project-domain node has these fields unless the record is explicitly
reserved and has no producer yet.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `project:v1:{hash}` | yes | Stable record ID. |
| `entity_id` | record ID | yes | Stable within `(domain, schema_version)` across mutations. In v1 it equals `id`. |
| `kind` | `NodeKind` | yes | One of the project-domain kinds below. |
| `domain` | `"project"` | yes | Reject any project kind with a different domain. |
| `schema_version` | `1` | yes | `PROJECT_SCHEMA_VERSION`. |
| `valid_time` | RFC3339 | yes | Domain event time. |
| `valid_time_source` | string | yes | For GitHub, `github_updated_at`; for local JSONL, `local_jsonl_updated_at`. |
| `transaction_time` | RFC3339 | yes | Store write time for append-with-same-entity-id mutation rows. |
| `confidence` | float string | guessed fields only | Omit for stable source fields. |
| `source_handle` | string | local JSONL | File path + line/record ID + hash. For local JSONL, `source_handle` is `<encoded_file_path>:<encoded_local_id>:<record_hash>` and is concretized by [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md). |
| `summary` | string | yes | Human-readable one-line summary. |

## 3 - Task record shape

Task record shape.

`Task` is the day-one project work item and the target of
`REFERENCES_TASK` from agent memory.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| shared fields | see section 2 | yes | `domain = "project"`, `schema_version = 1`. |
| `title` | string | yes | Redacted per #4. |
| `body_handle` | `{ inline: Option<String>, hash: String, bytes: u64 }` | yes | Same shape and 16 KiB inline ceiling as `CommandRun.stdout_handle` from #11. |
| `status` | enum | yes | `open`, `in_progress`, `blocked`, `closed_completed`, `closed_dropped`, `unknown`; additive. |
| `source_kind` | enum | yes | `github_issue`, `github_pr`, `local_jsonl`, `harness_legacy`; additive. `source_kind: local_jsonl` consumes [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md). |
| `source_external_link_id` | record ID | yes | `ExternalLink` carrying the source-system handle. |
| `assignees` | string array | yes | Agent IDs or human identifiers; opaque strings, not resolved to `Agent` nodes in this slice. |
| `labels` | string array | yes | Redacted per #4. |
| `priority` | enum | yes | `low`, `normal`, `high`, `urgent`, `unknown`; additive. |
| `head_sha` | string | no (PR only) | GitHub PR head (source-branch) commit SHA (issue #333). Plaintext; omitted on non-PR Tasks. |
| `head_ref` | string | no (PR only) | GitHub PR head (source-branch) ref name (issue #333). Plaintext; omitted on non-PR Tasks. |
| `base_ref` | string | no (PR only) | GitHub PR base (target-branch) ref name (issue #333). Plaintext; omitted on non-PR Tasks. |
| `merge_commit_sha` | string | no (PR only) | GitHub PR merge commit SHA; present only when actually merged (`merged_at` present), gating out GitHub's temporary test-merge SHA on unmerged PRs (issue #333). Plaintext. Resolves to a `MERGED_AS` edge, below (also merged-only). |
| `merged_at` | RFC3339 | no (PR only) | GitHub PR merge timestamp; present only when merged (issue #333). Plaintext. |
| `draft` | bool | no (PR only) | GitHub PR draft flag (issue #333). Plaintext; omitted on non-PR Tasks. |

GitHub issue and PR importers both write `Task`; the six optional PR fields above
are promoted flat `Task` fields (issue #333, consumed by #334/#338). They are
additive — a pre-#333 `Task` that omits them is still valid at
`PROJECT_SCHEMA_VERSION = 1`. Remaining GitHub-only fields can land in reserved
`GitHubIssue` or `PR` records in a later slice.

## 4 - AcceptanceCriterion record shape

AcceptanceCriterion record shape.

`AcceptanceCriterion` is a falsifiable requirement attached to a task, issue,
PRD, or plan. It is the project-domain record that can be closed by
verification evidence.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| shared fields | see section 2 | yes | `domain = "project"`, `schema_version = 1`. |
| `parent_task_id` | record ID | yes | Owning `Task`; the `OWNED_BY_TASK` edge target MUST match this value. |
| `ordinal` | u32 | yes | Position within the parent AC list; reorderings are visible and change identity. |
| `text` | string | yes | Redacted verbatim falsifiable claim. |
| `status` | enum | yes | `unverified`, `verified`, `failed`, `superseded`, `unknown`; additive. |
| `verification_link_id` | record ID or null | no | Verification record that closed it. The `CLOSES_ACCEPTANCE_CRITERION` edge target MUST match this value. |

Rule: a status of `verified` MUST have a non-null `verification_link_id` whose
target exists in the `verification` domain. The daemon write applier rejects a
violation with `acceptance_criterion_missing_verification` (HTTP 422). If
`parent_task_id` is unresolved, the applier rejects with
`unresolved_evidence_target`.

## 5 - ExternalLink record shape

ExternalLink record shape.

`ExternalLink` is the source-system handle that lets GitHub-sourced and
local-JSONL-sourced tasks normalize into one `Task` shape.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| shared fields | see section 2 | yes | `domain = "project"`, `schema_version = 1`. |
| `system` | enum | yes | `github`, `gitlab`, `local_file`, `harness_legacy`, `other`; additive. |
| `url` | string | yes | Canonical URL or `file://` path; redacted per #4. |
| `system_native_id` | string | yes | GitHub issue number as a string, local JSONL source identity handle, etc. For local JSONL, use the hashless percent-encoded source identity handle from [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md), rendered as `<file_path>:<local_id>` when no escaping is needed. Not redacted by default. |
| `repository_remote` | string | when applicable | VCS remote from #7. |
| `discovered_at` | RFC3339 | yes | When the importer first saw this handle. |

## 6 - Reserved project-graph kinds

These kinds are reserved so future writers cannot collide on field names. Full
payload definitions belong to their own slices.

| Kind | Definition |
|------|------------|
| `Product` | Long-lived product/repository initiative - reserved for the multi-repo-aggregation slice. |
| `Project` | Bounded area of work under a `Product` - reserved. |
| `Plan` | Strategy, milestone, or implementation plan - reserved. `Plan` records are project-domain entries; `docs/plans/*.md` files are artifact-domain `Plan` records distinguished by `domain`. |
| `GitHubIssue` | GitHub-specific issue metadata - reserved; day-one shape collapses issues into `Task` with `source_kind` and `ExternalLink`. |
| `PR` | GitHub pull-request metadata - reserved. As of issue #333 the core PR fields (`head_sha`, `head_ref`, `base_ref`, `merge_commit_sha`, `merged_at`, `draft`) are promoted to first-class flat `Task` fields; a dedicated `PR` record remains reserved only for the residual PR-only surface (requested reviewers, `mergeable_state`, …). |
| `Review` | Review comment, finding, approval, requested change, or blocker. Shipped in issue #46 (no longer reserved): the GitHub importer emits `project.Review` records for issue comments, PR review summaries, and PR review comments. |
| `ExternalIdentity` | A source-system participant identity — a GitHub login (issue #335). Carries ONLY the login (in `author`) and the source system (`identity_system: "github"`); never email, display name, avatar, or profile URL. Keyed on `(system, login)` ALONE (deliberately NOT repo-scoped — a participant identity is global across repositories), so the same login observed in two repos maps to exactly one node: `project_stable_id(["project", "ExternalIdentity", "github", <login>])`. Trust class `project_state`. Consumed by #338/#339 (evidence packs / reviewer joins); future beneficiaries #245 (ownership) and #262. |
| `ReviewStateTransition` | One append-only review-state TRANSITION event minted from a GitHub PR-timeline event (issue #336). Carries the closed `transition_kind` (`review_dismissed` / `review_requested` / `review_request_removed`), the actor login (in `author`), the timeline event handle (`system_native_id: "timeline:<id>"`), and — for a dismissal that supplied one — the redacted dismissal message in `body_handle`. `valid_time` is the event's `created_at`. Keyed on the timeline event's own id: `project_stable_id(["project", "ReviewStateTransition", <source_repo>, "<n>", "timeline:<event_id>"])`, so it is append-only and **never participates in the parent `Review`'s identity**. Trust class `project_state`. The `Review.review_state` field is a last-write-wins current-state SUMMARY; these transitions are the HISTORY a dismissal would otherwise erase. Consumers needing "state as of T" (e.g. #339) join the transitions, not the summary. |
| `LocalTask` | Named in the PRD as a sibling of `GitHubIssue` - reserved; day-one shape collapses it into `Task` with `source_kind: local_jsonl`. |

## 7 - Cross-Domain Edge Rows

These rows live in the shared registry table in
[`docs/schema/agent-memory.md`](agent-memory.md). This document owns the
project-domain side of the contract, but the registry remains the one from #6.

| Label | FROM domain(s) | TO domain(s) | FROM kind(s) | TO kind(s) | Cardinality | `confidence` required |
|-------|---------------|-------------|-------------|-----------|-------------|----------------------|
| `REFERENCES_TASK` | `agent_memory`, `project` | `project` | `Observation`, `Decision`, `Failure`, `Lesson`; `Review` | `Task` | many:many | no |
| `CLOSES_ACCEPTANCE_CRITERION` | `project` | `verification` | `AcceptanceCriterion` | `Verification`, `CommandRun`, `TestRun` | many:1 | no |
| `OWNED_BY_TASK` | `project` | `project` | `AcceptanceCriterion` | `Task` | many:1 | no |
| `EXTERNAL_HANDLE` | `project` | `project` | `Task`, `AcceptanceCriterion` | `ExternalLink` | many:1 | no |
| `TOUCHES_FILE` | `project` | `codegraph` | `Task`; `Review` | `File` | many:many | no |
| `MERGED_AS` | `project` | `codegraph` | `Task` *(`source_kind: github_pr`)* | `Commit` | many:1 | no |
| `REVIEWS_COMMIT` | `project` | `codegraph` | `Review` *(`source_kind: github_review`)* | `Commit` | many:1 | no |
| `REVIEWED_BY` | `project` | `project` | `Review` *(`source_kind: github_review`)* | `ExternalIdentity` | many:1 | no |
| `REQUESTED_REVIEW_FROM` | `project` | `project` | `Task` *(`source_kind: github_pr`)* | `ExternalIdentity` | many:many | no |
| `TRANSITIONS_REVIEW` | `project` | `project` | `ReviewStateTransition` | `Review` | many:1 | no |
| `MENTIONS_SYMBOL` | `project` | `codegraph` | `Task` | `Symbol` | many:many | yes |

`REFERENCES_TASK` is promoted from reserved to defined: #6 already reserved the
label, and this slice fills in `project.Task` as the target.

The daemon applier synthesizes `OWNED_BY_TASK`, `EXTERNAL_HANDLE`, and
`CLOSES_ACCEPTANCE_CRITERION` from the denormalized project fields. Directly
submitted project edges (e.g. the importer's `TOUCHES_FILE` and `MERGED_AS`)
must obey the same FROM/TO kind rules and MUST carry a project-domain identity —
a `project:v<schema_version>:` record ID and `schema_version = PROJECT_SCHEMA_VERSION`
— so the daemon project-edge validator applies to them and `project:v1:` readers
find them. An edge that names a project-only label but carries a `codegraph:` ID
serializes under the wrong domain and is skipped by the project-edge validator.
`MERGED_AS` additionally requires the FROM `Task` to have
`source_kind = github_pr`: the validator rejects a `MERGED_AS` whose source Task
is a `github_issue`, `local_jsonl`, or otherwise-typed task (or carries no
`source_kind`), so a non-PR task can never be persisted as merge evidence.
`REVIEWS_COMMIT` (issue #334, the review-side mirror) is validated the same way:
the FROM node must be a `Review` whose `source_kind = github_review` (Review nodes
originate solely from the GitHub importer), the TO node a `codegraph.Commit`, so a
non-Review node can never be persisted as having reviewed a commit. It anchors a
review to the exact commit it looked at — never a range-approval verdict.

**Reviewer identity (issue #335).** `REVIEWED_BY` (FROM a `Review` whose
`source_kind = github_review`, TO an `ExternalIdentity`) records who authored a
review; `REQUESTED_REVIEW_FROM` (FROM a PR `Task` whose `source_kind = github_pr`,
TO an `ExternalIdentity`) records each reviewer whose review was requested. Both
are directly-submitted project edges carrying `project:v1:` identity, validated
by the daemon like `MERGED_AS`/`REVIEWS_COMMIT`: the FROM node's importer
`source_kind` is required and both terminate at `ExternalIdentity` ONLY, so a
forged or mistyped node can never mint a reviewer binding. The daemon also
requires an `ExternalIdentity` node to carry a non-empty `author` (the login)
and a non-empty `identity_system` before it is persisted, so a reviewer edge can
never bind to a login-less anonymous identity. The offline `eg validate` gate
mirrors this directionally: beyond the `ExternalIdentity`-only target rule, it
constrains the reviewer edges' SOURCE kinds — `REVIEWED_BY` must originate from a
`Review` and `REQUESTED_REVIEW_FROM` from a `Task` — so a wrong-source reviewer
edge (e.g. `Task —REVIEWED_BY→ ExternalIdentity`) is a named
`edge_source_kind_violation` defect. Every emitted `Review`
(all review kinds) gains exactly one `REVIEWED_BY` to its author's identity; a
requested TEAM is never expanded to member logins — it is recorded as a
`github_team_review_request_unexpanded` project `Diagnostic` carrying the team
slug and the PR `Task` id. A `REVIEWED_BY` binding proves a review NAMES a
participant, never a verdict on the review; a `REQUESTED_REVIEW_FROM` is an
invitation to review, never proof a review happened.

**Review-state history (issue #336).** `TRANSITIONS_REVIEW` (FROM a
`ReviewStateTransition`, TO the `Review` it acted on) records that a review was
dismissed. It exists so a dismissal never erases that an approval once existed:
`Review.review_state` is a **last-write-wins current-state summary** (a dismissal
overwrites `"approved"` → `"dismissed"` under the SAME review record id), while
each `ReviewStateTransition` is an **append-only history** record keyed on the
timeline event's own id — so the transition never participates in the review's
identity. The edge is minted ONLY for a `review_dismissed` timeline event (which
names the dismissed review); `review_requested` / `review_request_removed`
transitions name no single review and stand alone with no edge. `eg validate` and
the daemon both frame the edge directionally — source `ReviewStateTransition`,
target `Review` only. A `TRANSITIONS_REVIEW` binding proves WHICH review a
transition acted on, never that the dismissal was correct. **A consumer that needs
"review state as of time T" MUST join the transitions, not read the summary
field.**

**Merge-resolution lifecycle (issue #333, Codex round-6).** A PR's merge
evidence is one of two artifacts: a `MERGED_AS` edge (unique `Commit` match) or a
`github_commit_unresolved` project `Diagnostic` (zero/ambiguous match). When a
re-import changes that outcome — the SHA newly resolves, stops resolving,
resolves to a different `Commit`, or the merged `merge_commit_sha` itself changes
— the GitHub importer emits a project-domain `Tombstone` naming the prior
artifact's record ID via `deleted_id` before emitting the new one. In a
persistent store the tombstone suppresses the superseded artifact from the
current read view, so exactly one merge artifact per PR is ever live at once; an
unchanged outcome emits no tombstone. The `github_commit_unresolved` Diagnostic's
stable ID is **repo-scoped** (Codex round-7): `source_repo` is part of the ID, so
same-PR-number/same-SHA diagnostics never collide across repositories in a shared
store. When a resolution **cycles back** to a previously tombstoned outcome
(resolved-A → unresolved/B → resolved-A) the re-emitted artifact reconstructs the
same record ID and bytes; the embedded sink then **revives the tombstoned ID** by
forcing a fresh, tombstone-post-dating observation, so the current read view
surfaces the re-resolved artifact again. See
[`import-github.md`](import-github.md) §5–6 for the state tracking and the full
transition matrix.

## 8 - Redaction Call-Out

These project fields MUST pass through #4 redaction policy before persistence:

- `Task.title`
- `Task.body_handle.inline`
- `Task.labels` (each label string)
- `Task.assignees` (each identifier)
- `AcceptanceCriterion.text`
- `ExternalLink.url`

`ExternalLink.system_native_id` is NOT redacted by default because it is query
substrate, such as a GitHub issue number once the URL is recorded. A future
operator-policy slice MAY redact it for private repositories.

`Task.status` and `AcceptanceCriterion.status` enum values are not redacted.

## 9 - Stable ID Composition

`id` = `project:v<schema_version>:<blake3(domain || kind || source_kind || source_native_id || entity_kind_identity)>`

The `entity_kind_identity` component is:

| Kind | Entity identity input |
|------|-----------------------|
| `Task` | `source_external_link_id.system_native_id`, such as a GitHub issue number, or the hashless local-JSONL percent-encoded source identity handle rendered as `<file_path>:<local_id>` when no escaping is needed. |
| `AcceptanceCriterion` | `(parent_task_id, ordinal)`; reordering ACs changes the ID. |
| `ExternalLink` | `(system, system_native_id)`. |

The rule "IDs are unique within `(domain, schema_version)`" from #3 still
holds. Re-importing the same GitHub issue produces the same `Task` ID because
the source handle is stable. Local JSONL uses a percent-encoded source handle
for deterministic lookup.

## 10 - Mutation Model

Project-graph records are append-with-same-entity-id.

A GitHub issue moving from `open` to `closed_completed` writes a new row at the
same `entity_id` with a new `transaction_time`. The previous row is preserved
and is visible via `as_of.transaction_time` once the #8 transaction-time read
axis is wired up. The data model supports this now: the conformance fixture
imports the same task twice with different `status` values and asserts two
rows with the same `entity_id`, ordered by `transaction_time`.

## 11 - Vantage AC Ingestion Attachment

Every Vantage-filed `spec`/`pm` issue has a `## Acceptance Criteria` section
with `- [ ]` checklist items. The future M9 GitHub importer MUST parse one AC
per top-level checklist item under the `Acceptance Criteria` heading. In short:
one AC per top-level checklist item under the `Acceptance Criteria` heading.

Parser contract:

- Assign `ordinal` by source order, starting at 1.
- Checkbox state derives initial status: unchecked is `unverified`; checked is
  `verified` only when a verification record is also available, otherwise it
  remains importer-mapped intent.
- `~~strikethrough~~` marks a criterion as `superseded`.
- Preserve the checklist item's verbatim text after redaction.
- Nested checklist items are not AC records in v1.

This slice documents the parser contract but does not ship the parser.

## 12 - Conformance Tests

`tests/daemon.rs` enforces:

- Every project-domain `NodeKind` variant in `src/ir.rs` has a documented row
  here or is annotated reserved.
- The shared edge registry contains `REFERENCES_TASK` with project TO,
  `CLOSES_ACCEPTANCE_CRITERION`, `OWNED_BY_TASK`, `EXTERNAL_HANDLE`,
  `TOUCHES_FILE`, and `MENTIONS_SYMBOL` with project FROM.
- A verified `AcceptanceCriterion` with null `verification_link_id` is rejected
  with `acceptance_criterion_missing_verification`.
- Re-importing the same `Task` with a status change produces two rows at the
  same `entity_id`, distinguishable by `transaction_time`.
- An `AcceptanceCriterion` with a missing `parent_task_id` is rejected with
  `unresolved_evidence_target`.
- Trust-class enforcement rejects a `Task` whose record `domain` is not
  `project`, and rejects an `AcceptanceCriterion.verification_link_id` that
  resolves outside the `verification` domain.

## 13 - Versioning Rules

`PROJECT_SCHEMA_VERSION = 1` is recorded in `src/ir.rs`.

Additive in v1:

- New project-graph kinds.
- New optional fields.
- New status enum values.
- New `source_kind` values.

Requires `project:v2:` and `PROJECT_SCHEMA_VERSION = 2`:

- Renaming or removing existing kinds or fields.
- Changing the trust-class rules.
- Changing this AC parser contract attachment.
- Changing the `entity_id` composition for `Task`, `AcceptanceCriterion`, or
  `ExternalLink`.

## 14 - Coordination Notes

- **Issue #4 (redaction):** The six redactable fields on
  `Task`/`AcceptanceCriterion`/`ExternalLink` are reserved by this schema. #4
  owns the policy; this doc owns the field list.
- **Issue #5 (daemon wire):** `acceptance_criterion_missing_verification` is
  added to the daemon error-code enum. HTTP 422, non-retryable.
- **Issue #6 (agent memory):** `REFERENCES_TASK` is promoted from reserved to
  defined with `project.Task` as TO. A future `REFERENCES_TASK` edge whose
  target is a local-JSONL-sourced `Task` MUST resolve through the file path +
  local_id pair documented in [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md),
  not by guessing the file format. The new project edge rows are added to the
  registry table in `docs/schema/agent-memory.md`.
- **Issue #17 (local JSONL):** The `local-JSONL record path` mention in
  `system_native_id` is concretized as the hashless percent-encoded source
  identity handle from [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md),
  rendered as `<file_path>:<local_id>` when no escaping is needed, with
  `file_path` repo-relative. The shared local JSONL `source_handle` remains the
  hashed row handle `<encoded_file_path>:<encoded_local_id>:<record_hash>`.
- **Issue #10 (query verbs):** Future query verb `criteria_for_task` is
  reserved against this schema and distinct from existing reserved verbs.
- **Issue #11 (verification):** `CLOSES_ACCEPTANCE_CRITERION` targets
  `verification.Verification`, `verification.CommandRun`, or
  `verification.TestRun`; #11 owns the verification side, this doc owns the
  project side.
