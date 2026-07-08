# Agent-Memory Domain Schema — v1

**Status:** Frozen at v1. Adding a new field, record shape, or edge label is
additive. Renaming, removing, or changing semantics of an existing field
requires a schema version bump. The `schema_version` field on every record is
the enforcement point.

**Source of truth:** This document. `src/ir.rs` (node kinds, edge labels, ID
functions) and `src/daemon.rs` (write applier validation) must conform to it.

**Related documents:**
- Wire contract: [`docs/schema/daemon-api.md`](daemon-api.md)
- Agent actions and patch artifacts: [`docs/schema/agent-actions.md`](agent-actions.md)
- Semantic drift domain: [`docs/schema/semantic-drift.md`](semantic-drift.md)
- Code-graph schema: [`docs/prd/0001-codebase-knowledge-graph.md`](../prd/0001-codebase-knowledge-graph.md)
- Vision PRD: [`docs/prd/0000-egregore-vision.md`](../prd/0000-egregore-vision.md)
- Daemon design: [`docs/plans/2026-05-17-egregore-daemon-design.md`](../plans/2026-05-17-egregore-daemon-design.md)

---

## 1 — Domain identity

| Field | Value |
|-------|-------|
| Domain name | `agent_memory` |
| `schema_version` | `1` |
| ID prefix | `agent_memory:v1:` |
| Rust constant | `AGENT_MEMORY_SCHEMA_VERSION = 1` |

Agent-memory IDs use the `agent_memory:v1:` prefix so they cannot collide with
code-graph `codegraph:v1:` IDs even when the Blake3 content hashes are
identical. The rule "IDs are unique within `(domain, schema_version)`" from
issue #3 holds for both domains.

Preference-promotion records live in the authorization-derived
[`user_context`](user-context.md) domain. Agent-memory observations may supply
evidence for `PromoteCandidate` records, but they do not become durable policy
without an approved user-context decision.

Existing in-store `Agent` and `AgentSession` records emitted by the daemon
before this schema was published are declared `schema_version: 1`
retroactively — the field set the daemon was already writing is the field set
this document now codifies.

---

## 2 — Base record fields

Every record (node or edge) in the Egregore graph carries:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | string | yes | Stable, content-addressed ID. See §1 for prefix. |
| `record_type` | `"node"` or `"edge"` | yes | Serde tag. |
| `kind` | NodeKind enum | yes (nodes) | See §4. |
| `schema_version` | integer | yes | `1` for all v1 agent-memory records. |
| `summary` | string | yes | Human-readable one-line description. |

---

## 3 — Required provenance fields

Every **agent-authored** node MUST carry the following fields on top of the
base record fields. Code-graph nodes (emitted by the extractor) do not carry
these fields.

| Field | Type | Required for | Notes |
|-------|------|-------------|-------|
| `agent_id` | string | all agent-memory nodes | Stable agent identity within a project scope. |
| `agent_kind` | enum string | all agent-memory nodes | See additive enum below. |
| `session_id` | string | all agent-memory nodes | Identifies the agent session that produced the record. |
| `observed_at` | RFC 3339 | all agent-memory nodes | Wall-clock time the agent observed the fact. |
| `ingested_at` | RFC 3339 | all agent-memory nodes | Transaction time: when the daemon committed the record. |
| `confidence` | float string `[0.0, 1.0]` | `Observation`, `Decision`, `Lesson` | Omitted for purely-derived shapes (`ToolCall`, etc.). |
| `source_handle` | string (path or hash) | when upstream artifact exists | The artifact path or hash the record was extracted from. |
| `redaction_policy_version` | string | when any field passed through redaction | Reserved by this schema; populated once issue #4 ships. |
| `domain` | `"agent_memory"` | traj-importer records | Explicit domain tag carried on every traj-importer node. |
| `importer_id` | string | traj-importer records | `"traj-importer"` — identifies the import pipeline. |
| `importer_version` | string | traj-importer records | Semver of the importer that emitted the record. |
| `source_artifact_path` | string | traj-importer records | Filesystem path to the `.traj` file. |
| `source_artifact_hash` | string | traj-importer records | BLAKE3 hex of the raw `.traj` bytes — idempotency anchor. |

**`agent_kind` additive enum** (new values are additive; renaming requires a
schema version bump):

| Value | Description |
|-------|-------------|
| `codex` | OpenAI Codex agent |
| `claude-code` | Anthropic Claude Code |
| `vantage` | Internal Vantage agent |
| `rust-swe-agent` | rust-swe-agent trajectory importer |
| `human` | Human operator writing directly |
| `other` | Any other agent kind |

---

## 4 — NodeKind registry

All `NodeKind` enum variants must appear in this table or in the code-graph
schema (`docs/prd/0001-codebase-knowledge-graph.md`). The exhaustive `match`
in `tests/daemon.rs::all_node_kinds_have_documented_schema` enforces this at
compile time.

### 4a — Agent-memory nodes (full schema, this slice)

| Kind | Domain | ID inputs | Summary template | Notes |
|------|--------|-----------|-----------------|-------|
| `Agent` | `agent_memory` | `["node", "agent", agent_id]` | `Agent {agent_id} ({agent_kind}) scoped to {project_scope}` | Stable identity for one agent process or human actor. |
| `AgentSession` | `agent_memory` | `["node", "agent_session", agent_id, session_id]` | `Session {session_id} for agent {agent_id}` | One bounded run or conversation session. |
| `AgentRun` | `agent_memory` | `["node", "agent_run", session_id, "run-0"]` | `AgentRun outcome={outcome} exit_reason={exit_reason}` | One trajectory / invocation of an agent within a session. |
| `AgentTurn` | `agent_memory` | `["node", "agent_turn", run_id, turn_index]` | `AgentTurn {turn_index}` | One assistant→user message pair in an AgentRun. |
| `ToolCall` | `agent_memory` | see `docs/schema/agent-actions.md` | `ToolCall {tool_name} status={status}` | Full field set belongs to [`docs/schema/agent-actions.md`](agent-actions.md). |
| `FileEdit` | `agent_memory` | see `docs/schema/agent-actions.md` | `FileEdit {repo_relative_path} kind={edit_kind}` | Full field set belongs to [`docs/schema/agent-actions.md`](agent-actions.md). |
| `Failure` | `agent_memory` | `["node", "failure", kind, turn_id, action_idx]` | `Failure {kind} turn={t}` | Failed command or invalid patch; `failure_kind` = `command_failure` or `patch_invalid`. |
| `Observation` | `agent_memory` | writer-chosen | free | Agent-authored claim with confidence and provenance. Carries `evidence_links`. |
| `Retraction` | `agent_memory` | `["node", "retraction", target_record_id]` | `Retraction event for {target_record_id}` | Auditable operator retraction event written by `eg forget` (issue #231). One per target: the ID is deterministic in the retracted handle. |

#### `Agent` record shape

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | |
| `kind` | `"Agent"` | yes | |
| `schema_version` | `1` | yes | |
| `name` | string | yes | Set to `agent_id`. |
| `summary` | string | yes | `Agent {agent_id} ({agent_kind}) scoped to {project_scope}` |
| provenance fields | see §3 | — | Carried at registration via the daemon's `POST /v1/agents/register` envelope. |

The `display_summary` field (human-readable version of the structured summary)
is preserved as the `summary` field value for backward read compatibility.

#### `AgentSession` record shape

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | |
| `kind` | `"AgentSession"` | yes | |
| `schema_version` | `1` | yes | |
| `name` | string | yes | Set to `session_id`. |
| `summary` | string | yes | `Session {session_id} for agent {agent_id}` |

#### `Observation` record shape

The canonical agent-memory node. Used for any agent-authored claim, insight,
or discovery.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | |
| `kind` | `"Observation"` | yes | |
| `schema_version` | `1` | yes | |
| `text` | string | yes | Post-redaction body. |
| `summary` | string | yes | One-line summary. |
| `confidence` | float string | yes | `[0.0, 1.0]`. |
| `agent_id` | string | yes | See §3. |
| `agent_kind` | enum | yes | See §3. |
| `session_id` | string | yes | See §3. |
| `observed_at` | RFC 3339 | yes | See §3. |
| `ingested_at` | RFC 3339 | yes | See §3. |
| `source_handle` | string | when applicable | See §3. |
| `redaction_policy_version` | string | when redacted | See §3. |
| `superseded_by` | record ID | optional | ID of the record that supersedes this one. |
| `evidence_links` | `EvidenceLink[]` | required | Must contain at least one link. See §5. |

#### `Retraction` record shape

The auditable event `eg forget` writes when an operator logically retracts a
persisted record (issue #231). It is always accompanied by a tombstone (in the
retracted record's domain) whose `deleted_id` is the target; the event itself
is never tombstoned, so the act of forgetting stays citable. See
[`docs/cli/forget.md`](../cli/forget.md).

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | Deterministic per target: `["node", "retraction", target_record_id]`. |
| `kind` | `"Retraction"` | yes | |
| `schema_version` | `1` | yes | |
| `text` | string | yes | Post-redaction retraction reason. |
| `summary` | string | yes | `Retraction event for {target_record_id}` |
| `agent_id` | string | yes | Operator handle recorded as the retraction actor. |
| `transaction_time` | RFC 3339 | yes | When the retraction was committed (transaction-time axis). |
| `source_handle` | record ID | yes | The prior record handle (the retracted record's stable ID). |
| `valid_time` | RFC 3339 | yes | Mirrors `transaction_time`. |
| `valid_time_source` | `"inferred_from_transaction_time"` | yes | |
| `redaction_policy_version` | string | when redacted | See §3. |

#### `AgentRun` record shape

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | |
| `kind` | `"AgentRun"` | yes | |
| `schema_version` | `1` | yes | |
| `summary` | string | yes | `AgentRun outcome={outcome} exit_reason={exit_reason}` |
| `domain` | `"agent_memory"` | yes | |
| `importer_id` | string | yes | `"traj-importer"` |
| `importer_version` | string | yes | semver string |
| `source_artifact_path` | string | yes | Path to the `.traj` file. |
| `source_artifact_hash` | string | yes | BLAKE3 hex of the raw `.traj` bytes. |
| `observed_at` | RFC 3339 | optional | `info.started_at` from the trajectory. |
| `agent_kind` | `"rust-swe-agent"` | yes | |

#### `AgentTurn` record shape

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | |
| `kind` | `"AgentTurn"` | yes | |
| `schema_version` | `1` | yes | |
| `summary` | string | yes | `AgentTurn {turn_index}` |
| `turn_index` | u64 | yes | 0-based position in the AgentRun. |
| traj-importer provenance | see above | yes | `domain`, `importer_id`, etc. |
| `observed_at` | RFC 3339 | optional | Timestamp from `extra.timestamp`. |

#### Agent-action record shapes delegated to issue #13

`ToolCall` and `FileEdit` are agent-memory records, but their full field sets,
trust-class rules, stable ID inputs, and redaction field list belong to
[`docs/schema/agent-actions.md`](agent-actions.md). `PatchArtifact` is not an
agent-memory record; it lives in the `artifact` domain and is defined in the
same agent-actions schema. `CommandRun` runtime evidence is defined in
[`docs/schema/verification.md`](verification.md).

#### `Failure` record shape

A failed command, invalid patch, rejected assumption, or blocked workflow.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | |
| `kind` | `"Failure"` | yes | |
| `schema_version` | `1` | yes | |
| `failure_kind` | string | yes | `command_failure`, `patch_invalid`, `assumption_rejected`, `workflow_blocked`. |
| `exit_code` | i64 | optional | Exit code when `failure_kind` is `command_failure` or `patch_invalid`. |
| `text` | string | yes | Redacted output excerpt. |
| traj-importer provenance | see above | yes | |

#### `Decision` record shape

A durable project or implementation decision inferred from explicit context.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | `agent_memory:v1:{hash}` | yes | |
| `kind` | `"Decision"` | yes | |
| `schema_version` | `1` | yes | |
| `decision_text` | string | yes | The decision, stated directly. |
| `scope` | string | yes | Repository, module, or task scope. |
| `rationale_summary` | string | yes | Why this decision was made. |
| `confidence` | float string | yes | `[0.0, 1.0]`. |
| provenance fields | see §3 | yes | |
| `evidence_links` | `EvidenceLink[]` | required | |

### 4b — Agent-memory nodes (reserved, one-line definitions)

These `NodeKind` variants are reserved so future producers cannot invent
collisions. Full field specifications belong to the slice that ships each
producer.

| Kind | Reserved for |
|------|-------------|
| `Task` | Work item tracked by an agent; first-class project-domain shape is defined in [`docs/schema/project-graph.md`](project-graph.md). |
| `Artifact` | File, patch, report, or generated output linked to work. |
| `Verification` | Evidence for a claim, test, or check (emitted by traj importer for test commands). |
| `CommandEvidence` | Command output or terminal evidence. |
| `CostUsage` | Token, wall-clock, budget, or provider-cost metadata. |

Adding a new reserved kind requires updating this table and the compile-time
conformance test in `tests/daemon.rs`.

---

## 5 — EvidenceLink value type

An `EvidenceLink` is a typed citation from an agent-memory node to another
graph record. Links are stored in **two representations** that MUST agree at
write time:

1. **Denormalized array** on the source node (`evidence_links` field) — for
   fast single-record reads without traversal.
2. **Graph edge** — for traversal queries (e.g. `eg query observations
   --symbol X`).

The daemon write applier is the enforcement point. When a node with
`evidence_links` is ingested, the applier MUST:
- Verify that each `target_record_id` exists in the store (by read-back).
- Reject the write with `unresolved_evidence_target` (HTTP 422) if any target
  is missing.
- Emit a corresponding graph edge for each link.

### EvidenceLink fields

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `target_record_id` | string | yes | The code-graph or other-domain record being cited. |
| `target_domain` | string | yes | Domain of the target: `"codegraph"`, `"agent_memory"`, `"project"`, etc. |
| `relation` | string | yes | Cross-domain edge label. See §6. |
| `confidence` | float string | yes | `[0.0, 1.0]`. |
| `as_of_commit` | string (SHA) | optional | Git commit anchoring a time-specific citation. |

### Target ID resolution

A writer MAY specify the target either:
- By its **stable `codegraph:v1:`, `agent_memory:v1:`, or `project:v1:` ID** directly (no
  indirection required).
- By a `(repo_relative_path, span, git_commit)` triple when the writer cannot
  compute the stable hash. The daemon write applier resolves the triple to the
  canonical ID and rejects with `unresolved_evidence_target` if no matching
  record exists.

---

## 6 — Cross-domain edge label registry

All `EdgeLabel` enum variants must appear in this table or in the code-graph
schema. The exhaustive `match` in
`tests/daemon.rs::all_edge_labels_have_documented_schema` enforces this at
compile time.

Adding a new label is additive and requires updating this table. Renaming or
removing a label is a schema version bump.

### 6a — Cross-domain edges (this registry)

| Label | FROM domain(s) | TO domain(s) | FROM kind(s) | TO kind(s) | Cardinality | `confidence` required |
|-------|---------------|-------------|-------------|-----------|-------------|----------------------|
| `SESSION_OF` | `agent_memory` | `agent_memory` | `AgentSession` | `Agent` | many:1 | no |
| `AUTHORED_BY` | any | `agent_memory` | any | `AgentSession` | many:1 | no |
| `HAS_EVIDENCE` | any | `agent_memory` | any | `Verification`, `CommandEvidence` | many:many | no |
| `OBSERVES` | `agent_memory` | `codegraph` | `Observation` | any | many:many | yes |
| `MENTIONS_SYMBOL` | `agent_memory`, `verification` | `codegraph` | any | `Symbol` | many:many | yes |
| `MENTIONS_SYMBOL` | `project` | `codegraph` | `Task` | `Symbol` | many:many | yes |
| `TOUCHED_FILE` | `agent_memory`, `verification` | `codegraph` | `FileEdit`, `ToolCall`, `CommandRun`, `TestRun`, `CIStatus` | `File` | many:many | no |
| `PRODUCED_PATCH` | `agent_memory` | `artifact` | `FileEdit`, `AgentTurn` | `PatchArtifact` | many:1; FileEdit at most one | no |
| `PRODUCED_EVIDENCE` | `agent_memory` | `verification` | `ToolCall` | `CommandRun`, `TestRun` | many:1 | no |
| `VALIDATED_BY` | `agent_memory` | `verification` | `Observation`, `Decision` | any verification | many:many | no |
| `FAILED_ON` | `agent_memory`, `verification` | `codegraph` | `Failure`, `TestRun`, `CIStatus` | `Symbol`, `File` | many:many | no |
| `EXPLAINS_CHANGE` | `agent_memory` | `codegraph` | `Observation`, `Decision` | `Commit`, `Change` | many:many | yes |
| `REFERENCES_TASK` | `agent_memory` | `project` | `Observation`, `Decision`, `Failure`, `Lesson` | `Task` | many:many | no |
| `CLOSES_ACCEPTANCE_CRITERION` | `project` | `verification` | `AcceptanceCriterion` | `Verification`, `CommandRun`, `TestRun` | many:1 | no |
| `OWNED_BY_TASK` | `project` | `project` | `AcceptanceCriterion` | `Task` | many:1 | no |
| `EXTERNAL_HANDLE` | `project` | `project` | `Task`, `AcceptanceCriterion` | `ExternalLink` | many:1 | no |
| `TOUCHES_FILE` | `project` | `codegraph` | `Task` | `File` | many:many | no |
| `DRIFTS_FROM` | `semantic` | `codegraph` | `SemanticDrift` | `File`, `Symbol` | many:1 | no |
| `DRIFTS_PRIOR` | `semantic` | `codegraph` | `SemanticDrift` | `File`, `Symbol` | many:1 | no |
| `MEASURED_BY` | `semantic` | `semantic` | `SemanticDrift` | `EmbeddingModel` | many:1 | no |
| `CONTRADICTS` | `agent_memory`, `verification` | any | any | any | many:many | yes |
| `PROPOSED_BY` | `user_context` | `agent_memory` | `PromoteCandidate` | `Observation`, `AgentTurn`, `Decision` | many:many | yes |
| `PROMPTED_FOR` | `user_context` | `user_context` | `PromotionPrompt` | `PromoteCandidate` | many:1 | no |
| `DECIDED_ON` | `user_context` | `user_context` | `PromotionDecision` | `PromoteCandidate` | many:1 | no |
| `MATERIALIZED_AS` | `user_context` | `user_context` | `PromotionDecision` | `Preference`, `WorkflowRule`, `NamingDecision`, `Constraint` | many:1 | no |
| `REVOKED_BY` | `user_context` | `user_context` | `Preference`, `WorkflowRule`, `NamingDecision`, `Constraint` | `PromotionDecision` | many:1 | no |
| `CONTRADICTS` | `user_context` | `user_context` | `PromoteCandidate` | `Preference`, `WorkflowRule` | many:many | yes |
| `SCOPED_TO_REPO` | `user_context` | `codegraph` | `Preference`, `WorkflowRule`, `NamingDecision`, `Constraint` | `Repository` | many:1 | no |
| `SUPERSEDES` | `agent_memory`, `artifact` | `agent_memory`, `artifact` | any agent-memory, `PatchArtifact` | any agent-memory, `PatchArtifact` | many:1 | no |
| `RELATES_TO` | any | any | any | any | many:many | no |

For the `artifact` domain, `SUPERSEDES` is restricted to `PatchArtifact` ->
`PatchArtifact`; it represents a replacement patch preserving the pinned
validity result of the prior artifact.

### 6b — Code-graph-internal edges

The following labels are used exclusively within the code-graph domain and are
documented in `docs/prd/0001-codebase-knowledge-graph.md`:

`CONTAINS`, `DEFINES`, `IMPORTS`, `REFERENCES`, `CALLS`, `IMPLEMENTS`,
`MENTIONS`, `CHANGED_IN`, `PARENT_OF`.

Semantic drift edge labels are in the registry above and are specified in
[`docs/schema/semantic-drift.md`](semantic-drift.md). This is the issue #15
coordination point for #6's daemon edge validation.

---

## 7 — Stable ID composition

| Domain | Function | Format |
|--------|----------|--------|
| `codegraph` | `stable_id(parts)` | `codegraph:v{SCHEMA_VERSION}:{blake3_hex}` |
| `agent_memory` | `agent_memory_stable_id(parts)` | `agent_memory:v{AGENT_MEMORY_SCHEMA_VERSION}:{blake3_hex}` |

Both functions null-terminate each input part before hashing, so
`stable_id(&["a", "bc"])` ≠ `stable_id(&["ab", "c"])`.

### Agent, AgentSession, and traj-importer ID inputs

| Record | ID inputs |
|--------|-----------|
| `Agent` | `["node", "agent", agent_id]` |
| `AgentSession` | `["node", "agent_session", agent_id, session_id]` |
| `SESSION_OF` edge | `["edge", "SESSION_OF", session_node_id, agent_node_id]` |
| `AgentSession` (traj) | `["node", "agent_session", "traj-importer", importer_version, blake3_hex]` |
| `AgentRun` (traj) | `["node", "agent_run", session_id, "run-0"]` |
| `AgentTurn` (traj) | `["node", "agent_turn", run_id, turn_index]` |
| `ToolCall` | defined in [`docs/schema/agent-actions.md`](agent-actions.md) |
| `FileEdit` | defined in [`docs/schema/agent-actions.md`](agent-actions.md) |
| `Failure` (command) | `["node", "failure", "command_failure", turn_id, action_idx]` |
| `Failure` (patch) | `["node", "failure", "patch_invalid", turn_id, action_idx]` |

---

## 8 — Schema version policy

- `schema_version: 1` applies to all records defined in this document.
- Additive changes (new optional fields, new `NodeKind` reserved entries, new
  edge labels) do not bump the version.
- Breaking changes (field renames, semantic changes to existing fields, removal
  of fields) require `schema_version: 2` and a new schema document.
- The Rust constants `SCHEMA_VERSION` (code-graph) and
  `AGENT_MEMORY_SCHEMA_VERSION` (agent-memory) track the current version for
  each domain independently.

---

## 9 — Coordination notes

- **Issue #2 (`eg query`):** The `eg query` verbs MUST be able to follow
  `EvidenceLink` arrays from an `Observation` to a `Symbol` once cross-domain
  data exists. The JSON output schema MUST include `evidence_links` when
  present.
- **Issue #3 (domain field):** The `agent_memory` domain enum value is
  load-bearing and is exercised by `Agent`/`AgentSession` records that ship
  today.
- **Issue #4 (redaction):** The `redaction_policy_version` field name on
  agent-authored nodes is reserved by this schema. Issue #4 owns the pipeline
  that populates it.
- **Issue #13 (agent actions):** `PRODUCED_PATCH`, the `TOUCHED_FILE`
  extension, and `PRODUCED_EVIDENCE` are contributed by
  [`docs/schema/agent-actions.md`](agent-actions.md); this registry remains the
  canonical edge table.
- **Issue #5 (daemon wire):** `unresolved_evidence_target` is added to the
  daemon error-code enum. HTTP 422, non-retryable. See
  `docs/schema/daemon-api.md` §5.
- **Issue #14 (project graph):** `REFERENCES_TASK` is promoted from reserved
  to defined with `project.Task` as TO. `CLOSES_ACCEPTANCE_CRITERION`,
  `OWNED_BY_TASK`, `EXTERNAL_HANDLE`, `TOUCHES_FILE`, and the project-domain
  `MENTIONS_SYMBOL` row are contributed by
  [`docs/schema/project-graph.md`](project-graph.md); this table remains the
  canonical cross-domain edge registry.
- **Issue #17 (local JSONL):** A future `REFERENCES_TASK` edge whose target is
  a local-JSONL-sourced `Task` MUST resolve through the file path + local_id
  pair documented in [`docs/schema/local-project-jsonl.md`](local-project-jsonl.md),
  not by guessing the file format.
- **Issue #19 (user context):** `PROPOSED_BY`, `PROMPTED_FOR`, `DECIDED_ON`,
  `MATERIALIZED_AS`, `REVOKED_BY`, the user-context `CONTRADICTS` extension,
  and `SCOPED_TO_REPO` are contributed by
  [`docs/schema/user-context.md`](user-context.md); the `user_context` domain
  enum value is load-bearing.
