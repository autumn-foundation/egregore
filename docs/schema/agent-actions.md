# Agent-Actions and PatchArtifact Schema - v1

**Status:** Active at v1. This document is the single source of truth for the
day-one `ToolCall`, `FileEdit`, and `PatchArtifact` record shapes that transcript
importers emit. Producers may add optional fields, new enum values, or new edge
labels only as additive v1 changes. Renaming or removing existing fields or enum
values requires a `/v2/` schema bump.

**Source of truth:** This document. `docs/schema/agent-memory.md` owns the
cross-domain edge registry, `docs/schema/verification.md` owns verification
evidence records, and this document owns the field sets, domain placement, trust
class, redaction field list, and patch-validity rules for the three agent-action
shapes.

**Related documents:**
- Agent-memory domain and edge registry: [`docs/schema/agent-memory.md`](agent-memory.md)
- Verification-evidence domain: [`docs/schema/verification.md`](verification.md)
- Redaction policy: [`docs/schema/redaction.md`](redaction.md)
- Vision PRD: [`docs/prd/0000-egregore-vision.md`](../prd/0000-egregore-vision.md)
- Daemon API: [`docs/schema/daemon-api.md`](daemon-api.md)

---

## 1 - Domain Placement

These assignments are fixed for v1.

- PatchArtifact -> `artifact` domain. This follows the vision PRD Domain Model,
  where `Patch` belongs to the Artifact Graph. The base `domain` field from
  issue #3 is `artifact`, and the artifact-domain `schema_version` is `1`.
- FileEdit -> `agent_memory` domain. A file edit is a session-bound event
  describing what an agent attempted, not a durable artifact. Patch bytes, when
  present, become a separate `PatchArtifact` linked by `PRODUCED_PATCH`.
- ToolCall -> `agent_memory` domain. A tool invocation is a session-bound event,
  not a durable artifact. Tool output that constitutes verification evidence
  becomes a `CommandRun` or `TestRun` record from
  [`docs/schema/verification.md`](verification.md), linked by
  `PRODUCED_EVIDENCE`.

| Record | Domain | `schema_version` | ID prefix |
|--------|--------|------------------|-----------|
| `PatchArtifact` | `artifact` | `1` | `artifact:v1:` |
| `FileEdit` | `agent_memory` | `1` | `agent_memory:v1:` |
| `ToolCall` | `agent_memory` | `1` | `agent_memory:v1:` |

---

## 2 - Trust Classes

### Artifact: `PatchArtifact`

`PatchArtifact` is an agent-derived structured artifact with deterministic
content identity. The patch content is reproducible from the patch bytes, and
the record carries `patch_bytes_hash` as the identity component. Patch validity
is runtime-derived: a patch either applied cleanly to a base commit, applied
with conflicts, failed syntax checks, lacked a base, was rejected by validation,
or has not been verified.

Trust rules:

1. `patch_status` is required and append-only. A `PatchArtifact` with
   `patch_status: invalid_syntax` is never edited into
   `patch_status: applied_clean`; a new `PatchArtifact` is emitted.
2. The `patch_status` enum is:
   `applied_clean | applied_with_conflicts | invalid_syntax | invalid_no_base | rejected_validation | unverified | superseded`.
3. Patch bytes are stored by handle, never as a queryable field. The
   `patch_handle.inline` field is a convenience copy and is allowed only when
   the redacted bytes are at most 16 KiB.

Patch status rules:

| Value | Rule |
|-------|------|
| `applied_clean` | The patch applied to `base_commit` without conflicts or validation failures. |
| `applied_with_conflicts` | The patch applied with conflict markers or manual resolution requirements. |
| `invalid_syntax` | The bytes are not a valid patch format; `target_files` is empty. |
| `invalid_no_base` | The producer cannot identify a base commit and validation requires one. |
| `rejected_validation` | Patch syntax was usable, but a validation gate rejected it. |
| `unverified` | The patch bytes are captured, but no apply or validation result is known. |
| `superseded` | A later patch replaces this one; the graph links the records with `SUPERSEDES`. |

### Agent Action: `ToolCall`, `FileEdit`

`ToolCall` and `FileEdit` are agent-derived event records. They are
deterministic as transcribed: if the trajectory says the agent called tool X
with args Y, that is a fact about the trajectory. They are not source-derived;
they cannot be recovered from `git`.

Rules:

- They MUST carry `agent_id`, `session_id`, `observed_at`,
  `source_artifact_path`, and `source_artifact_hash`.
- They MUST NOT carry `confidence`; the event happened or it did not.
- They MAY carry `evidence_quality` from
  [`docs/schema/verification.md`](verification.md) when the producer needs to
  mark inline payload quality as verbatim, summarized, or referenced only.

---

## 3 - PatchArtifact Record Shape

### PatchArtifact record shape

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | string | yes | `artifact:v1:<hash>`. |
| `record_type` | `"node"` | yes | Base record field. |
| `kind` | `"PatchArtifact"` | yes | NodeKind. |
| `domain` | `"artifact"` | yes | Fixed domain assignment. |
| `schema_version` | `1` | yes | `schema_version` is `1`. |
| `summary` | string | yes | One-line agent-facing description. |
| `patch_status` | enum | yes | See status enum in section 2. |
| `base_commit` | Git SHA or `null` | yes | SHA the patch was authored against; `null` only with `unknown_base_reason`. |
| `unknown_base_reason` | `"unknown_base"` or `null` | when `base_commit` is `null` | Explicit reason the base is unknown. |
| `target_files` | string array | yes | Repo-relative paths touched by the patch; empty for `invalid_syntax`. |
| `patch_bytes_hash` | BLAKE3 hex | yes | BLAKE3 of raw patch bytes; artifact identity component. |
| `patch_bytes_size` | u64 | yes | Exact raw patch byte length. |
| `patch_handle` | object | yes | `{ path: String, inline: Option<String> }`. |
| `validation_summary` | string | yes | Human-readable status reason; redacted per #4. |
| `source_artifact_path` | string | yes | Upstream `.traj` or session file. |
| `source_artifact_hash` | BLAKE3 hex | yes | Hash of the upstream source artifact. |
| `producer_session_id` | record id | yes | Links to the `AgentSession` that produced the patch. |
| `valid_time` | RFC3339 | yes | Produced-at time from #8 temporal fields. |
| `valid_time_source` | `"produced_at"` | yes | Fixed source for artifact production time. |
| `ingested_at` | RFC3339 | yes | Transaction-time provenance from agent-memory convention. |

`patch_handle.inline` MUST be `None` when `patch_bytes_size > 16 KiB` or when
the inline string itself exceeds 16 KiB after redaction. The daemon rejects this
with `inline_payload_exceeds_ceiling`.

---

## 4 - FileEdit Record Shape

### FileEdit record shape

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | string | yes | `agent_memory:v1:<hash>`. |
| `record_type` | `"node"` | yes | Base record field. |
| `kind` | `"FileEdit"` | yes | NodeKind. |
| `domain` | `"agent_memory"` | yes | Fixed domain assignment. |
| `schema_version` | `1` | yes | `schema_version` is `1`. |
| `summary` | string | yes | One-line description. |
| `repo_relative_path` | string | yes | Path edited by the agent. |
| `edit_kind` | enum | yes | `create | modify | delete | rename`; additive. |
| `before_hash` | BLAKE3 hex or `null` | yes | `null` for `create`. |
| `after_hash` | BLAKE3 hex or `null` | yes | `null` for `delete`. |
| `rename_to` | repo-relative path | iff `edit_kind: rename` | Required for renames. |
| `hunk_count` | u32 | yes | Bounded by producer; for `delete`/`create`, may be 0 or 1. |
| `linked_patch_id` | record id or `null` | no | Optional link to `artifact.PatchArtifact`. |
| `linked_turn_id` | record id | yes | Link to the owning `agent_memory.AgentTurn`. |
| `agent_id` | string | yes | Provenance from agent-memory. |
| `session_id` | string | yes | Provenance from agent-memory. |
| `observed_at` | RFC3339 | yes | When the edit was observed in the transcript. |
| `source_artifact_path` | string | yes | Upstream `.traj` or session file. |
| `source_artifact_hash` | BLAKE3 hex | yes | Hash of the upstream source artifact. |
| `ingested_at` | RFC3339 | yes | Transaction-time provenance. |

`edit_kind` additions are additive. Renaming or removing `create`, `modify`,
`delete`, or `rename` requires a v2 schema.

---

## 5 - ToolCall Record Shape

### ToolCall record shape

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | string | yes | `agent_memory:v1:<hash>`. |
| `record_type` | `"node"` | yes | Base record field. |
| `kind` | `"ToolCall"` | yes | NodeKind. |
| `domain` | `"agent_memory"` | yes | Fixed domain assignment. |
| `schema_version` | `1` | yes | `schema_version` is `1`. |
| `summary` | string | yes | One-line description. |
| `tool_name` | string | yes | Producer tool name, e.g. `Bash`, `Edit`, `Read`, `Grep`, `Glob`, `apply_patch`. |
| `tool_kind` | enum | yes | `bash | file_edit | file_read | search | network_request | code_execution | other`; additive. |
| `arguments_summary` | string | yes | One-line redacted argument summary. |
| `arguments_handle` | object | yes | `{ inline: Option<String>, hash: String, bytes: u64 }`; inline only when <= 16 KiB and redacted. |
| `result_handle` | object or `null` | no | Same shape for non-verification output. |
| `produced_evidence_id` | record id or `null` | no | Link to `verification.CommandRun` or `verification.TestRun`. |
| `linked_turn_id` | record id | yes | Link to the owning `agent_memory.AgentTurn`. |
| `started_at` | RFC3339 | yes | Tool-call start time. |
| `finished_at` | RFC3339 or `null` | no | `null` when interrupted. |
| `status` | enum | yes | `succeeded | failed | interrupted | unknown`; additive. |
| `agent_id` | string | yes | Provenance from agent-memory. |
| `session_id` | string | yes | Provenance from agent-memory. |
| `observed_at` | RFC3339 | yes | When the call was observed in the transcript. |
| `source_artifact_path` | string | yes | Upstream `.traj` or session file. |
| `source_artifact_hash` | BLAKE3 hex | yes | Hash of the upstream source artifact. |
| `evidence_quality` | enum | no | Reuses verification quality enum when useful. |
| `ingested_at` | RFC3339 | yes | Transaction-time provenance. |

`tool_kind` additions are additive. Renaming or removing existing values
requires a v2 schema.

---

## 6 - Cross-Domain Edge Contributions

The registry remains in [`docs/schema/agent-memory.md`](agent-memory.md). This
slice contributes these rows:

| Label | FROM domain(s) | TO domain(s) | FROM kind(s) | TO kind(s) | Cardinality | `confidence` required |
|-------|---------------|-------------|-------------|-----------|-------------|----------------------|
| `PRODUCED_PATCH` | `agent_memory` | `artifact` | `FileEdit`, `AgentTurn` | `PatchArtifact` | many:1; FileEdit at most one | no |
| `TOUCHED_FILE` | `agent_memory`, `verification` | `codegraph` | `FileEdit`, `ToolCall`, `CommandRun` | `File` | many:many | no |
| `PRODUCED_EVIDENCE` | `agent_memory` | `verification` | `ToolCall` | `CommandRun`, `TestRun` | many:1 | no |

`PRODUCED_PATCH` lets a session's file edits or turn cite the patch artifact
they produced. `TOUCHED_FILE` lets agent actions and command evidence point to
code-graph files. `PRODUCED_EVIDENCE` lets a tool call cite verification output
without requiring verification records to back-link to tool calls.

---

## 7 - Stable ID Composition

All ID inputs are null-delimited before hashing. The hash function is BLAKE3.

| Record | ID format |
|--------|-----------|
| `PatchArtifact` | `artifact:v<schema_version>:<blake3(domain || kind || patch_bytes_hash || producer_session_id)>` |
| `FileEdit` | `agent_memory:v<schema_version>:<blake3(domain || kind || agent_id || session_id || linked_turn_id || repo_relative_path || edit_kind || before_hash || after_hash)>` |
| `ToolCall` | `agent_memory:v<schema_version>:<blake3(domain || kind || agent_id || session_id || linked_turn_id || tool_name || arguments_handle.hash || started_at)>` |

Patch identity is content-derived plus session-derived so identical bytes
produced by different sessions remain distinguishable. File-edit and tool-call
identity are session-bound and content-derived so re-importing the same `.traj`
produces the same IDs.

---

## 8 - Redaction Call-Out

Fields that pass through #4 redaction:

- `PatchArtifact.validation_summary`
- `PatchArtifact.patch_handle.inline`
- `ToolCall.arguments_summary`
- `ToolCall.arguments_handle.inline`
- `ToolCall.result_handle.inline`

Fields that are not redacted because they are query substrate:

- `patch_bytes_hash`
- `before_hash`
- `after_hash`
- `arguments_handle.hash`
- `result_handle.hash`
- `tool_name`
- `tool_kind`
- `edit_kind`
- `patch_status`
- `target_files`
- `repo_relative_path`

`FileEdit` has no inline content fields. `before_hash` and `after_hash` are
hashes, not content; actual file content lives in the file or external artifact
storage and is not a graph redaction concern.

---

## 9 - Validity-Pinning

The validity-pinning rule: a `PatchArtifact` validity result is append-only. An
invalid patch may not be later rewritten to valid by editing the record. If a
later session repairs and applies the patch, that repair emits a new
`PatchArtifact` with a new `patch_bytes_hash` because the bytes changed. The
original invalid record remains queryable as `invalid_syntax`, `invalid_no_base`,
`rejected_validation`, or `unverified`; the graph links it to the replacement
with the `SUPERSEDES` edge from the agent-memory edge registry.
This is the issue #13 `SUPERSEDED_BY` relation expressed with the existing
registry label: the replacement patch `SUPERSEDES` the prior patch, and readers
may materialize `superseded_by` on the prior record.

The daemon enforces attempted mutation of `PatchArtifact.patch_status` with the
`patch_status_pinned` error code. This is the operational rule behind the M2
exit criterion: an invalid or unverified patch remains labeled
invalid/unverified instead of becoming a false success memory.

---

## 10 - Reserved Artifact Shapes

The artifact domain is introduced by this slice's `PatchArtifact` shape. The
following durable-document shapes are reserved by name so producers cannot
collide, but their full payload specs belong to future producer slices:

| Shape | One-line reservation |
|-------|----------------------|
| `ADR` | Architecture decision document artifact. |
| `PRD` | Product requirements document artifact. |
| `Plan` | Implementation or project plan artifact. |
| `Transcript` | Raw or normalized session transcript artifact. |
| `BenchmarkReport` | Human-readable benchmark report artifact. |
| `ReleaseNote` | Release-note artifact. |

---

## 11 - Versioning Rules

`schema_version` is `1` for the artifact-domain `PatchArtifact` record and for
the agent-memory `FileEdit` and `ToolCall` records.

Additive v1 changes:

- Adding a new `patch_status` enum value.
- Adding a new `edit_kind` enum value.
- Adding a new `tool_kind` enum value.
- Adding optional fields.

Breaking changes requiring `/v2/`:

- Renaming or removing existing enum values.
- Renaming or removing required fields.
- Changing domain placement.
- Allowing inline payloads above the 16 KiB ceiling without a new storage mode.

---

## 12 - Coordination Notes

- **#4 redaction:** The redactable fields named in section 8 are reserved by
  this schema; #4 owns the policy, and this document owns the field-name list.
- **#5 daemon wire:** `patch_status_pinned` is added to the daemon error-code
  enum; `inline_payload_exceeds_ceiling` is reused for handle ceilings.
- **#6 agent memory:** `PRODUCED_PATCH`, the `TOUCHED_FILE` extension, and
  `PRODUCED_EVIDENCE` are added to the registry in
  [`docs/schema/agent-memory.md`](agent-memory.md); the registry stays in #6.
- **#9 traj importer:** The three day-one shapes are defined here. The importer
  consumes this schema rather than defining the field set inline; `linked_patch_id`,
  `linked_turn_id`, and `producer_session_id` wire agent-action records to the
  agent-memory records #6 owns.
- **#11 verification:** `ToolCall.produced_evidence_id` references
  `verification.CommandRun` or `verification.TestRun` IDs that #11 defines. The
  cross-domain reference is one-way; verification records do not back-link to
  tool calls.
