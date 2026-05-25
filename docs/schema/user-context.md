# User-Context Domain Schema - v1

**Status:** Active at v1. This document is the single source of truth for the
`user_context` domain: preference-promotion proposals, operator prompts,
operator decisions, and durable operator policy records.

| Property | Value |
|----------|-------|
| Domain name | `user_context` |
| `schema_version` | `1` |
| ID prefix | `user_context:v1:` |
| Rust constant | `USER_CONTEXT_SCHEMA_VERSION = 1` |
| Trust class | authorization-derived |

`schema_version` = `1` applies to every record in this document.

The user-context domain is authorization-derived. Agent observations can create
proposals, but a durable agent policy exists only when an explicit operator
approval authorizes it.

---

## 1 - Trust Class Rules

`PromoteCandidate` records MAY be created from agent observations, but they are
proposals, never policy. The agent-policy query helpers reserved by issues #2
and #10 MUST exclude `PromoteCandidate` records by default.

`Preference`, `WorkflowRule`, `NamingDecision`, and `Constraint` records MAY
exist only when an approved `PromotionDecision` references them. The daemon write
applier is the enforcement point and rejects direct writes to durable
user-context kinds without a referenced approval using
`unapproved_durable_user_context`.

`Preference`, `WorkflowRule`, `NamingDecision`, and `Constraint` records MAY exist only when an approved `PromotionDecision` references them.

`PromotionPrompt` and `PromotionDecision` are append-only audit records. A
rejection is not edited later when the user changes their mind. Instead, a new
`PromoteCandidate`, a new `PromotionPrompt`, and a new `PromotionDecision` are
written.

Repeated rejections of compatible candidates create a rejection-debounce window
so the same rejected proposal does not re-prompt until materially new evidence
appears.

---

## 2 - Common Fields

Every user-context node carries the base record fields:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | string | yes | Stable ID; see §10. |
| `record_type` | `"node"` | yes | Serde tag. |
| `kind` | `NodeKind` | yes | One of the record kinds below. |
| `schema_version` | integer | yes | `1`. |
| `domain` | `"user_context"` | yes | From issue #3. |
| `valid_time` | RFC3339 | yes | Per temporal-selector policy from issue #8. |
| `valid_time_source` | string | yes | The source for `valid_time`. |
| `summary` | string | yes | One-line operator-facing summary. |

Agent-created proposal records also carry the agent-memory provenance fields
from [`agent-memory.md`](agent-memory.md): `agent_id`, `agent_kind`,
`session_id`, `observed_at`, and `ingested_at`.

### Scope Struct

`scope` is shared by candidates and durable records:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `repo` | string | no | Repository ID from repository-identity. |
| `path_glob` | string | no | Repository-relative glob. |
| `language` | string | no | Example: `rust`. |
| `lifecycle_phase` | enum | no | `pre_commit`, `pre_pr`, `pre_merge`, `runtime`, `any`. |

All fields omitted means "global to this operator".

---

## 3 - PromoteCandidate Record Shape

PromoteCandidate record shape

`PromoteCandidate` is the canonical user-context proposal node.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `proposed_rule_text` | string | yes | Durable rule body that approval would create; post-redaction per issue #4. |
| `proposed_rule_kind` | enum | yes | `preference`, `workflow_rule`, `naming_decision`, `constraint`; additive. `revocation` is reserved for approved revocation flows. |
| `scope` | object | yes | See §2. |
| `confidence` | float `[0.0, 1.0]` | yes | Derived from evidence aggregation. |
| `supporting_evidence` | `EvidenceLink[]` | yes | Links to `Observation`/`AgentTurn`/`Decision` records that triggered the proposal. Minimum unique target count is the configured threshold. |
| `contradicting_evidence` | `EvidenceLink[]` | yes | Conflicting observations or prior `PromotionDecision` records; MAY be empty. |
| `superseded_by` | record ID | no | Later candidate or rejected candidate this candidate refines/replaces. |
| `evidence_quality` | enum | yes | `verbatim`, `summarized`, `referenced_only` from issue #11. |
| provenance fields | see §2 | yes | Agent-memory provenance fields. |

The daemon rejects a `PromoteCandidate` with fewer than the configured evidence
threshold using `insufficient_promotion_evidence`.

---

## 4 - PromotionPrompt Record Shape

PromotionPrompt record shape

`PromotionPrompt` records what was shown to the operator.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `candidate_id` | record ID | yes | The `PromoteCandidate` being presented. |
| `prompt_surface` | enum | yes | `cli`, `mcp`, `web`, `other`; additive. |
| `prompt_text` | string | yes | Exact rendered text shown to the operator; post-redaction. |
| `prompted_at` | RFC3339 | yes | Prompt timestamp. |
| `prompted_to` | string | yes | Operator handle if known, opaque otherwise. |
| `expires_at` | RFC3339 | no | If no response arrives by this time, write a `PromotionDecision` with `outcome = expired` and no durable record. |

Append-only: prompts are never edited. A re-prompt is a new record.

---

## 5 - PromotionDecision Record Shape

PromotionDecision record shape

`PromotionDecision` records the operator response.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `candidate_id` | record ID | yes | Candidate being decided. |
| `prompt_id` | record ID | yes | Prompt this responds to. |
| `outcome` | enum | yes | `approved`, `rejected`, `deferred`, `expired`, `edited_then_approved`; additive. |
| `decided_at` | RFC3339 | yes | Decision timestamp. |
| `decided_by` | string | yes | Operator handle if known. |
| `decision_rationale` | string | no | Post-redaction. |
| `materialized_record_id` | record ID or null | conditional | Required when `outcome` is `approved` or `edited_then_approved`; MUST be null otherwise. |
| `edited_rule_text` | string | conditional | Required when `outcome = edited_then_approved`. |

An edited approval produces a durable record with `edited_rule_text`, not the
candidate's original `proposed_rule_text`. Decisions are append-only.

---

## 6 - Durable User-Context Records

The four durable kinds are the only records returned by `agent_policy_for`.

### Preference

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `rule_text` | string | yes | Approved preference body. |
| `proposed_rule_kind` | `"preference"` | yes | Must match the durable kind. |
| `scope` | object | yes | Same struct as `PromoteCandidate`. |
| `approval_decision_id` | record ID | yes | Approved `PromotionDecision` that created this record. |
| `active_from` | RFC3339 | yes | Equal to the decision's `decided_at`. |
| `active_to` | RFC3339 | no | Populated when superseded or revoked. |
| `superseded_by` | record ID | no | Replacement durable record. |

### WorkflowRule

Same shape as `Preference`, plus:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `triggers` | enum array | yes | `pre_commit`, `pre_pr`, `pre_merge`, `pre_command`, `post_command`; additive. |
| `action_summary` | string | yes | What the rule asks the agent to do. |

### NamingDecision

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `entity_kind` | enum | yes | `crate`, `module`, `type`, `function`, `field`, `feature`, `other`; additive. |
| `canonical_name` | string | yes | Approved name. |
| `alternatives_rejected` | string array | yes | MAY be empty. |
| `approval_decision_id` | record ID | yes | Approved `PromotionDecision`. |
| `active_from` | RFC3339 | yes | Equal to `decided_at`. |
| `active_to` | RFC3339 | no | Populated when superseded or revoked. |
| `superseded_by` | record ID | no | Replacement durable record. |

### Constraint

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `constraint_text` | string | yes | Approved constraint body. |
| `enforcement_level` | enum | yes | `advisory` or `blocking`. |
| approval/lifecycle fields | see `Preference` | yes | Same approval and lifecycle fields. |

`blocking` constraints MUST be evaluable by the daemon at write time. For
example, "no `unwrap()` in production code" cannot be a blocking constraint
enforced at observation-write time; it can be advisory. The evaluator contract
is reserved for a later slice.

All four durable kinds MAY be revoked. Revocation is a new `PromoteCandidate`
of kind `revocation`, a new `PromotionPrompt`, and a new `PromotionDecision`
whose `outcome = approved` and whose `materialized_record_id` points back to
the original durable record being revoked. The original record's `active_to` is
set and a `REVOKED_BY` edge connects them. Direct deletion is not supported; the
audit trail is the product.

---

## 7 - Candidate-Aggregation Rule

Evidence threshold: a `PromoteCandidate` requires at least `N` unique supporting
observations from at least `K` distinct sessions. Defaults: `N = 3`, `K = 2`.
These thresholds are configuration knobs, not schema contracts. Changing them
does not bump `schema_version`.

Compatibility test: two observations are compatible when their normalized
`proposed_rule_text` values have Jaccard token similarity >= 0.6 and their
`scope` structs are compatible. Text normalization is lowercase plus whitespace
collapse. Scopes are compatible when one is a subset of the other or they are
equal. Embedding-based similarity is a future refinement.

Rejection-debounce window: when a `PromotionDecision` has `outcome = rejected`,
future candidates whose `(proposed_rule_text, scope)` is compatible with the
rejected candidate are suppressed until at least `M` additional observations
arrive and at least `T` time has passed since the rejection. Defaults:
`M = 5`, `T = 30 days`. These are defaults, not contracts. A candidate written
inside the debounce window carries `superseded_by` pointing to the rejected
candidate and should not create a new `PromotionPrompt`.

Contradiction handling: when a new observation contradicts an active
`Preference` or `WorkflowRule`, it creates a `PromoteCandidate` of kind
`revocation`, not a new conflicting durable record. Agent-policy query helpers
MUST surface active durable records, not the in-flight revocation candidate,
until approval.

---

## 8 - Redaction Call-Out

The following fields MUST pass through the redaction policy from issue #4 before
persistence:

- `PromoteCandidate.proposed_rule_text`
- `PromotionPrompt.prompt_text`
- `PromotionDecision.decision_rationale`
- `PromotionDecision.edited_rule_text`
- `Preference.rule_text`
- `WorkflowRule.rule_text`
- `WorkflowRule.action_summary`
- `Constraint.constraint_text`

Never store raw correction transcripts as a durable rule body.

---

## 9 - Query Semantics

`agent_policy_for(scope)` returns ONLY durable records (`Preference`,
`WorkflowRule`, `NamingDecision`, `Constraint`) whose `scope` matches and whose
`active_to` is null. `PromoteCandidate` records are excluded by default.

`pending_candidates(scope)` returns `PromoteCandidate` records with no
`PromotionDecision` yet, or a deferred decision, so an operator can review the
queue.

`audit_trail_for(durable_record_id)` returns:

`Preference` -> `PromotionDecision` -> `PromotionPrompt` -> `PromoteCandidate`
-> `Observation[]`

These three verbs are reserved for the daemon `/v1/query` set from issue #10.
They are not implemented by this schema slice.

---

## 10 - Stable ID Composition

IDs are unique within `(domain, schema_version)` per issue #3. The
`user_context:` prefix is distinct from `codegraph:`, `agent_memory:`,
`verification:`, `artifact:`, `project:`, and `semantic:`.

| Record | Stable ID |
|--------|-----------|
| `PromoteCandidate` | `user_context:v<schema_version>:candidate:<blake3(normalized_proposed_rule_text || canonical_scope || earliest_supporting_observation_id)>` |
| `PromotionPrompt` | `user_context:v<schema_version>:prompt:<blake3(candidate_id || prompted_at || prompt_surface)>` |
| `PromotionDecision` | `user_context:v<schema_version>:decision:<blake3(candidate_id || prompt_id || decided_at || outcome)>` |
| `Preference` | `user_context:v<schema_version>:preference:<blake3(rule_text || canonical_scope || approval_decision_id)>` |
| `WorkflowRule` | `user_context:v<schema_version>:workflow_rule:<blake3(rule_text || canonical_scope || approval_decision_id)>` |
| `NamingDecision` | `user_context:v<schema_version>:naming_decision:<blake3(entity_kind || canonical_name || canonical_scope || approval_decision_id)>` |
| `Constraint` | `user_context:v<schema_version>:constraint:<blake3(constraint_text || canonical_scope || approval_decision_id)>` |

Same normalized rule + same scope + same earliest evidence = same candidate ID.
This makes idempotency natural and lets re-imports of the same `.traj` not
create duplicate candidates.

---

## 11 - Cross-Domain Edge Rows

The canonical registry lives in [`agent-memory.md`](agent-memory.md) §6. This
schema contributes these rows:

| Label | FROM domain(s) | TO domain(s) | FROM kind(s) | TO kind(s) | Cardinality | `confidence` required |
|-------|---------------|-------------|-------------|-----------|-------------|----------------------|
| `PROPOSED_BY` | `user_context` | `agent_memory` | `PromoteCandidate` | `Observation`, `AgentTurn`, `Decision` | many:many | yes |
| `PROMPTED_FOR` | `user_context` | `user_context` | `PromotionPrompt` | `PromoteCandidate` | many:1 | no |
| `DECIDED_ON` | `user_context` | `user_context` | `PromotionDecision` | `PromoteCandidate` | many:1 | no |
| `MATERIALIZED_AS` | `user_context` | `user_context` | `PromotionDecision` | `Preference`, `WorkflowRule`, `NamingDecision`, `Constraint` | many:1 | no |
| `REVOKED_BY` | `user_context` | `user_context` | `Preference`, `WorkflowRule`, `NamingDecision`, `Constraint` | `PromotionDecision` | many:1 | no |
| `CONTRADICTS` | `user_context` | `user_context` | `PromoteCandidate` | `Preference`, `WorkflowRule` | many:many | yes |
| `SCOPED_TO_REPO` | `user_context` | `codegraph` | `Preference`, `WorkflowRule`, `NamingDecision`, `Constraint` | `Repository` | many:1 | no |

`PROPOSED_BY` is redundant with the `supporting_evidence` array and exists for
fast traversal queries.

---

## 12 - Versioning Rules

Additive without a version bump:

- new `proposed_rule_kind` enum values
- new `outcome` enum values
- new edge rows
- new optional `scope` fields with documented defaults
- new durable record optional fields

Requires `/v2/`:

- renaming or removing existing kinds or fields
- changing the candidate-aggregation rule from defaults to a contract
- removing the approval-gates-durable-records invariant
- allowing durable user-context records without a resolvable approved
  `PromotionDecision`

---

## 13 - Coordination Notes

- **Issue #2 (`eg query`):** agent-policy query verbs MUST exclude
  `PromoteCandidate` records by default. JSON output must include
  `agent_policy_for`, `pending_candidates`, and `audit_trail_for` when those
  verbs ship.
- **Issue #4 (redaction):** the user-context field names in §8 are reserved by
  this schema.
- **Issue #5 (daemon wire):** `insufficient_promotion_evidence` and
  `unapproved_durable_user_context` are daemon error-code enum values.
- **Issue #6 (agent memory):** the edge rows in §11 extend the cross-domain
  registry; the `user_context` domain enum value is now load-bearing.
- **Issue #9 (`.traj` importer):** the importer MAY emit `Observation` records
  with preference-shaped text but MUST NOT directly emit `PromoteCandidate`.
  Candidate creation is a daemon-write-applier concern.
- **Issue #10 (`/v1/query`):** `agent_policy_for`, `pending_candidates`, and
  `audit_trail_for` are reserved against this schema for a later slice.
