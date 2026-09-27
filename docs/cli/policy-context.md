# Policy in context answers (issue #169)

Active approved user-context policy — `Preference`, `WorkflowRule`,
`NamingDecision`, and `Constraint` records with a validated audit chain —
folds into cross-domain context answers. A symbol under a governed path
arrives with the rules that govern it, each row citing the approval decision
that materialized it.

## What folds

`query::policy_for_anchor` starts from the same durable set as
`eg query policy` (`query::active_policy`): durable policy kinds only,
records with `active_to` set (superseded or revoked) excluded, records
carrying `superseded_by` excluded even when `active_to` is absent
(belt-and-braces: `active_policy` only checks `active_to`), and the full
audit chain validated (durable record → approving `PromotionDecision` →
prompt → candidate → supporting observations). Anything that fails that gate
never reaches a context answer.

On top of that, the fold applies a **scope gate**: a policy record's
[`UserContextScope`](../schema/user-context.md) must apply to the scope
auto-derived from the context anchor's own facts. There is no
caller-supplied policy filter — the caller cannot ask for, or suppress,
policy.

## Scope auto-derivation

`query::policy_scope_for_record` derives the target scope from the anchor
record:

| Dimension | Derived from | Notes |
|-----------|--------------|-------|
| `repo` | Owning repository record ID via the containment topology | Module / Cargo package containment is implicit in the path hierarchy |
| `path_glob` | The anchor's concrete repo-relative path | The policy's glob is interpreted against this path |
| `language` | The anchor's recorded language | |
| `lifecycle_phase` | Always unset | Code-graph facts carry no lifecycle phase, so lifecycle-scoped records never match a symbol/file anchor |

`query::policy_scope_applies` then matches: a record-scope dimension that is
set must match the target dimension; a dimension the record scope leaves
unset applies to every target. A record scoped to another repository never
applies; a record with a path glob never applies to a target with no known
path.

### Path glob syntax

`query::path_glob_matches` — segment-aware, no external glob crate:

- `*` matches any run of characters **within one path segment** (never
  crosses `/`);
- `**` matches any run of characters **across segments**, including `/`;
- `?` matches exactly one character that is not `/`;
- everything else matches literally.

`src/policy/*` matches `src/policy/mod.rs` but never `src/policy/sub/x.rs`
and never the sibling prefix `src/policy2/mod.rs`.

## Row shape

Each `policy` row carries:

| Field | Meaning |
|-------|---------|
| `record_id` | Durable policy record ID |
| `kind` | `Preference`, `WorkflowRule`, `NamingDecision`, or `Constraint` |
| `body` | Human-readable rule: `rule_text` (preferences, workflow rules), `constraint_text` (constraints), `canonical_name` (naming decisions) |
| `approval_decision_id` | Handle of the `PromotionDecision` that approved the record — the authorization basis, one hop away via the decision's prompt surface, chain, and supporting evidence |
| `active_from` | Activation timestamp (equals the approving decision's `decided_at`) |
| `status` | Always `active` in context answers; the fold never surfaces superseded or revoked rows |
| `trust` | `other` under the #114 closed vocabulary (see below) |
| `scope_*` | The record's own scope, echoed for auditability. The CLI flattens these to `scope_repo`, `scope_path_glob`, `scope_language`, `scope_lifecycle_phase`; the MCP `symbol_context` tool nests them under a `scope` object (`scope.repo`, `scope.path_glob`, …). |

## Trust

Policy rows classify as `other` under the #114 closed trust vocabulary —
the vocabulary's home for records that are neither code facts
(`source_derived`) nor observations (`agent_unverified`,
`agent_contradicted`). "Other" does not mean "unverified": the
authorization basis is the cited `approval_decision_id`, whose audit trail
the fold already validated. Do not relabel policy rows as
`source_derived`; do not treat them as agent claims either.

## Empty section, never missing

When no active policy applies — the anchor is governed by nothing, the
name is ambiguous, or nothing matched — the answer carries an explicit
empty `policy` section (`"policy": []`), never a missing field. Pending,
rejected, and deferred candidates never surface in `policy`; they remain
visible via `eg query policy`.

## Lane matrix

| Lane | Policy | Notes |
|------|--------|-------|
| `eg query context` (symbol / file) | **Included** | Single anchor; scope derives from the anchor's facts |
| MCP `symbol_context` | **Included** | Twin surface of `eg query context`; same contract |
| `eg query semantic-context` (semantic bridge) | **Deferred** | The bridge returns many leads per query; the fold is anchored to a single identity. Policy stays out of per-lead rows until a per-lead anchoring contract exists |
| `eg query subsystem` | **Deferred** | Subsystem context aggregates a whole subtree, not a single anchor; the #169 fold is defined per anchor |
| `eg query locate` | **Deferred** | Locate answers "where is X", not "what governs X"; the sections it renders are unchanged |

## Offline workflow: approve, then ask

Everything below works against a local JSONL graph — no network, no daemon.

1. A candidate exists in the store (written by `eg` candidate promotion or
   imported records). Inspect it:

   ```sh
   eg query policy --graph graph.jsonl
   ```

2. Approve the candidate (issue #50 write path). This writes the prompt
   record, the `PromotionDecision`, and the materialized durable policy
   record; the durable record's `active_from` equals the decision's
   `decided_at`:

   ```sh
   eg decide <CANDIDATE_ID> --outcome approved --graph graph.jsonl --out decided.jsonl
   cat decided.jsonl >> graph.jsonl
   ```

   Note the decision record's ID in the output — it is the
   `approval_decision_id` the context answer will cite.

3. Ask for context on a symbol under the governed path. One call returns
   the active rule **and** its approval-decision handle:

   ```sh
   eg query context governed_fn --graph graph.jsonl
   ```

   ```json
   {
     "ok": true,
     "symbol_name": "governed_fn",
     "policy": [
       {
         "record_id": "user_context:v1:...",
         "kind": "WorkflowRule",
         "body": "Always run `cargo fmt` before committing",
         "approval_decision_id": "user_context:v1:promotion_decision:...",
         "active_from": "2026-09-20T10:00:00Z",
         "status": "active",
         "trust": "other"
       }
     ]
   }
   ```

   No `--policy` flag exists and none is needed: the fold is automatic for
   every symbol context, governed by the anchor's own facts.

## Out of scope

Conformance checking (does the code obey the policy), the #50 write path
itself, a standalone policy query (`eg query policy` already exists),
scope-drift flags, token budgeting for the section (it rides the standard
record budget like every other section), and any redefinition of the #114
trust classes.
