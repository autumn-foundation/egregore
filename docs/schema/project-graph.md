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

GitHub issue and PR importers both write `Task`; GitHub-only fields can land in
reserved `GitHubIssue` or `PR` records in a later slice.

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
| `PR` | GitHub pull-request metadata - reserved for fields such as `merged_at`, `base_ref`, and `head_ref`. |
| `Review` | Review comment, finding, approval, requested change, or blocker - reserved; depends on #6 `EvidenceLink` array shape for review-to-AC citations. |
| `LocalTask` | Named in the PRD as a sibling of `GitHubIssue` - reserved; day-one shape collapses it into `Task` with `source_kind: local_jsonl`. |

## 7 - Cross-Domain Edge Rows

These rows live in the shared registry table in
[`docs/schema/agent-memory.md`](agent-memory.md). This document owns the
project-domain side of the contract, but the registry remains the one from #6.

| Label | FROM domain(s) | TO domain(s) | FROM kind(s) | TO kind(s) | Cardinality | `confidence` required |
|-------|---------------|-------------|-------------|-----------|-------------|----------------------|
| `REFERENCES_TASK` | `agent_memory`, `project` *(Review, reserved)* | `project` | `Observation`, `Decision`, `Failure`, `Lesson`; `Review` *(reserved — ships when Review is promoted)* | `Task` | many:many | no |
| `CLOSES_ACCEPTANCE_CRITERION` | `project` | `verification` | `AcceptanceCriterion` | `Verification`, `CommandRun`, `TestRun` | many:1 | no |
| `OWNED_BY_TASK` | `project` | `project` | `AcceptanceCriterion` | `Task` | many:1 | no |
| `EXTERNAL_HANDLE` | `project` | `project` | `Task`, `AcceptanceCriterion` | `ExternalLink` | many:1 | no |
| `TOUCHES_FILE` | `project` | `codegraph` | `Task`; `Review` *(reserved — ships when Review is promoted)* | `File` | many:many | no |
| `MENTIONS_SYMBOL` | `project` | `codegraph` | `Task` | `Symbol` | many:many | yes |

`REFERENCES_TASK` is promoted from reserved to defined: #6 already reserved the
label, and this slice fills in `project.Task` as the target.

The daemon applier synthesizes `OWNED_BY_TASK`, `EXTERNAL_HANDLE`, and
`CLOSES_ACCEPTANCE_CRITERION` from the denormalized project fields. Directly
submitted project edges must obey the same FROM/TO kind rules.

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
