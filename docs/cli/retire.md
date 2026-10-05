# eg retire / reinstate / query retirement

`eg retire <handle>` retires an agent-memory record **from recall** — the
record stops appearing in default `eg query semantic-memory` results — while
keeping its history fully queryable (issue #156). Retirement is event-sourced
and additive: nothing is mutated, deleted, or tombstoned; the target record
and its provenance bytes are untouched.

## Retirement vs the other lifecycle operations

| Operation | Effect | History |
|-----------|--------|---------|
| **`eg retire`** (#156) | Record excluded from default recall until reinstated. | Receipt appended; target untouched. |
| **`eg forget`** (#231) | Record logically retracted from every transaction-time-current read surface. | Retraction event + tombstone; record still recoverable. |
| **Supersession** (#114) | A newer record supersedes an older one; superseded records are excluded from recall by default (`--supersession` controls this). | Normal `SUPERSEDES` edge between records; no lifecycle node. |

Retirement is the *recall-state* switch. `forget` is the *retraction* switch.
Supersession is a *provenance relationship* that recall also filters on.
`eg retire --reason superseded` writes both: the retirement receipt *and* a
normal `SUPERSEDES` edge.

Retirement differs from the adjacent read/repair lanes on purpose, and it
curates *recall*, not *truth* — the past stays queryable:

- **#92 (recall-time supersession flagging)** only *flags* superseded or
  contradicted records at recall time; the records remain in default recall
  unless filtered. Retirement *excludes* the record from default recall
  until reinstated — it is the operator's actuator on top of #92's signal.
- **#94 (composition-health report)** *measures* store-wide provenance
  health (unverified / superseded / dangling-evidence shares); it never
  changes recall. Retirement *acts* on those measurements, one record at a
  time, with a cited reason and receipt.
- **#49 / #72 (store repair)** rewrites or heals the store itself
  (re-embedding, index rebuilds, corruption repair). Retirement never
  rewrites history: it appends receipts and leaves every original byte —
  including the retired record — exactly where it was.

A retired record is therefore still retrievable via `--include-retired` and
via `eg query retirement --as-of` pinned inside its original valid window.

## Synopsis

```powershell
# Retire one observation as superseded by a newer one.
eg retire agent_memory:v1:<hex> --reason superseded --superseded-by agent_memory:v1:<hex> --retired-by op-1

# Retire on drifted evidence (the cited handle no longer resolves).
eg retire agent_memory:v1:<hex> --reason drifted --evidence-handle codegraph:v1:repo:src/deleted.rs --retired-by op-1

# Operator judgment call, no evidence needed.
eg retire agent_memory:v1:<hex> --reason operator-decision --retired-by op-1

# Bring it back.
eg reinstate agent_memory:v1:<hex> --reason "re-checked, still holds" --reinstated-by op-1

# Query the receipt trail and the state at a past instant.
eg query retirement agent_memory:v1:<hex>
eg query retirement agent_memory:v1:<hex> --as-of 2026-09-28T11:59:59Z

# Offline: append receipts to a graph JSONL instead of an embedded store.
eg retire agent_memory:v1:<hex> --reason operator-decision --graph graph.jsonl
```

| Option | Meaning |
|--------|---------|
| `<handle>` | Stable record ID of the record to retire / reinstate. |
| `--graph` | Graph JSONL file to append the receipt to (offline mode; mutually exclusive with `--data-dir`). |
| `--data-dir` | Embedded `AletheiaDB` data directory (default `.egregore`). |
| `--reason` | Required for `retire`: `superseded`, `drifted`, `contradicted`, or `operator-decision` (the issue's `operator_decision` spelling is also accepted). |
| `--superseded-by` | Superseding record (required for `superseded`). |
| `--evidence-handle` | Evidence handle (required for `drifted`; optional for `contradicted` and `operator-decision`). |
| `--retired-by` / `--reinstated-by` | Operator handle recorded as the actor (default `operator`). |
| `--transaction-time` | Fixed RFC 3339 instant for deterministic output; defaults to now. |

## Typed reasons and their gates

- **`superseded`** — requires `--superseded-by` naming a live, active,
  observation-class record. Also writes a normal `SUPERSEDES` evidence edge
  from the target to the superseder, so existing supersession views stay
  consistent; recall exclusion is controlled by the receipt.
- **`drifted`** — requires `--evidence-handle` naming an evidence handle the
  target actually cited that no longer resolves to a live record (a deleted
  file, a retracted claim).
- **`contradicted`** — no requirement, but an optional `--evidence-handle`
  may name a live record as the contradicting evidence. Dangling optional
  evidence is refused.
- **`operator-decision`** — no evidence required.

## What is retireable

Only observation-class agent-memory records (`Observation`, `Decision`,
`Failure`) can be retired. Deterministic code-graph facts are refused with a
machine-readable error (`retire_codegraph_fact`, exit 1) and zero writes —
they are source truth, never recall state.

## Receipts

A retirement appends exactly one `RetirementReceipt` node; `superseded`
appends one additional `SUPERSEDES` evidence edge. A reinstatement appends one
`ReinstatementReceipt` node. Receipt fields:

- `source_handle` — the target record ID.
- `agent_id` — the retiring/reinstating actor.
- `text` — the typed reason code (`superseded`, …) for retirements; free-text
  for reinstatements.
- `transaction_time` — the receipt's event time (the transaction-time axis
  that `--as-of` pins).
- Evidence links on the receipt naming the superseder (`SUPERSEDED_BY`) or
  the evidence handle (`DRIFTED_EVIDENCE`, `CONTRADICTED_BY`,
  `CITED_EVIDENCE`). The normal `SUPERSEDES` edge is a separate graph edge,
  not a receipt link.

Receipt IDs are stable per (target, transaction time, reason, actor,
superseder, evidence handle): re-running `eg retire` with identical inputs
produces a byte-identical receipt, while distinct events (e.g. two
retirements of one target at one instant with different reasons) never share
an ID, so a later ingest cannot overwrite an earlier receipt's audit trail.
Retiring an already-retired record is an idempotent success (`action:
already_retired`) returning the existing receipt; reinstating a record that
is not retired is a refusal (exit 1).

## State resolution

The latest receipt on the transaction-time axis wins; file order breaks ties
at identical timestamps. A `ReinstatementReceipt` newer than the latest
retirement returns the record to active recall — the retirement receipt stays
in history, so the full trail remains queryable. `--as-of <instant>` pins the
axis: receipts after the pin are invisible, so a pin before a retirement sees
the record as `active`.

## Recall behavior

Retired records are excluded from default `eg query semantic-memory` recall
with an exclusion diagnostic, on both the vector path and the
normalized-text degraded collapse path. `--include-retired` keeps them and
labels every returned row with `retirement_state`:

```json
"retirement_state": { "state": "retired", "reason": "superseded", "retired_by": "op-1", "retired_at": "2026-09-28T12:00:00Z" }
```

Active records label as `{ "state": "active" }` when the flag is passed.

## Machine-readable contract

Success (stdout, exit 0):

```json
{ "ok": true, "action": "retired", "receipt": { "receipt_id": "…", "retired_record_id": "…", "retired_by": "op-1", "retired_at": "…", "reason": "superseded", "superseded_by": "…" } }
```

`action` is `retired` / `already_retired` / `reinstated` / `already_active`.
`eg query retirement` returns `{ "ok": true, "handle": "…",
"retirement_state": { "state": "retired", … }, "receipts": [ { "action":
"retired", … }, { "action": "reinstated", … } ], "target": { …original
record… } }`.

Failures (stderr): exit 1 for refusals and malformed input, exit 2 when a
handle does not resolve:

| `error.code` | Meaning |
|---|---|
| `retire_not_found` / `reinstate_not_found` | Target handle resolves to nothing (exit 2). |
| `retire_dangling_superseding_record` | `--superseded-by` handle resolves to nothing (exit 2). |
| `retire_codegraph_fact` | Target is a deterministic code-graph fact; zero writes (exit 1). |
| `retire_superseding_record_not_observation` | Superseder is outside the observation class (exit 1). |
| `retire_superseding_record_retired` | Superseder is itself retired (exit 1). |
| `retire_missing_evidence_handle` | `drifted` without `--evidence-handle` (exit 1). |
| `retire_evidence_still_resolves` | `drifted` handle still resolves (exit 1). |
| `retire_unknown_evidence_handle` | `drifted` handle was never cited by the target (exit 1). |
| `retire_invalid_reason` | Reason is not one of the four typed codes (exit 1). |
| `retire_invalid_transaction_time` | `--transaction-time` is not RFC 3339 (exit 1). |
| `reinstate_not_retired` | Record is not currently retired (exit 1). |
