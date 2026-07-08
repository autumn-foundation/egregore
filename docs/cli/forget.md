# eg forget

`eg forget <handle>` retracts one persisted record — identified by its stable
record ID — from every transaction-time-current read surface of an embedded
store, without destroying the bytes (issue #231).

Retraction exists for the records redaction cannot catch after the fact: a
novel secret shape, a customer name, or a flatly wrong agent-authored claim
that slipped through at write time. It is **logical, auditable removal**, not
revision (supersede/contradict) and not physical erasure.

## Synopsis

```powershell
# Retract one agent-authored record by stable handle.
eg forget agent_memory:v1:<hex> --data-dir .egregore --reason "leaked customer name" --retracted-by op-1

# Re-running is a no-op success returning the original event.
eg forget agent_memory:v1:<hex> --data-dir .egregore --reason "anything"   # action: already_retracted

# Deterministic code-graph facts are refused (exit 1).
eg forget codegraph:v5:<hex> --data-dir .egregore --reason "wrong"         # deterministic_code_fact

# Derived semantic measurements are refused (exit 1).
eg forget semantic:v1:<hex> --data-dir .egregore --reason "noisy"          # derived_semantic_record

# Unknown handles exit 2.
eg forget agent_memory:v1:doesnotexist --data-dir .egregore --reason "x"   # not_found
```

| Option | Meaning |
|--------|---------|
| `<handle>` | Stable record ID of the record to retract. |
| `--data-dir` | Embedded `AletheiaDB` data directory (default `.egregore`). |
| `--reason` | Required retraction reason, recorded on the event (redaction policy v1 applies). |
| `--retracted-by` | Operator handle recorded as the actor (default `operator`). |
| `--transaction-time` | Fixed RFC 3339 instant for deterministic output; defaults to now. |

The command needs exclusive store access (the ordinary embedded write lease);
stop a running daemon first.

## What a retraction writes

Exactly two records, through the ordinary adapter boundary:

1. **A retraction event node** (`Retraction`, agent-memory domain) — the
   citable audit record. It carries who retracted the target (`agent_id`),
   when on the transaction-time axis (`transaction_time`), why (`text`, passed
   through redaction policy v1 so a reason quoting the leaked secret cannot
   re-leak it), and the prior record handle (`source_handle`). Its record ID
   is deterministic per target, so one target can only ever have one event.
2. **A tombstone** whose `deleted_id` is the target, in the target's domain.
   Every current-state read lane already excludes actively tombstoned records
   — the same machinery the deterministic refresh path uses.

The act of forgetting is therefore itself auditable, never a silent hole.

Re-running `eg forget` on an already-retracted handle never writes a second
event. When an active tombstone still suppresses the target, the re-run is a
no-op success returning the original event (`action: already_retracted`). When
the event exists but no active tombstone suppresses the target — a crash
between the two writes, or a later write superseding the tombstone — the
re-run repairs the retraction by re-issuing the tombstone
(`action: retracted`) while preserving the original event verbatim.

## What stops returning the record

After retraction, no transaction-time-current read surface returns the
record's content: `query symbol`, `file`, `semantic`, `context`, `task`,
`memory`, `audit`, `failures`, `changes`, `inspect`, and the MCP tools
(`inspect_store`, `symbol_context`, `task_evidence`) all exclude it, including
the semantic/vector index — a retracted record is filtered out of similarity
results even though its embedding bytes remain at rest. The daemon's
record-serving surfaces suppress it too: `GET /v1/records/{id}` and the
`get_records` query verb resolve a retracted handle to no record, and the bulk
`GET /v1/records` endpoint (which backs `DaemonClient::get_all_records`,
`eg inspect --daemon`, and the MCP tools) never serializes the retracted
record, so knowing the handle is not enough to fetch the content back. The
retraction event and the tombstone themselves stay fetchable — they are the
audit trail.

One deliberate nuance: daemon-free `eg inspect --data-dir` reports a physical
inventory, so its *counts* still include a retracted record's physical
versions (content is never shown by inspect). Daemon-backed
`eg inspect --daemon` counts the daemon's current serving view instead, which
excludes actively retracted records while still counting their tombstones and
retraction events.

`eg query memory <handle>` on a retracted handle reports `stale_handle`
(exit 2) rather than the content.

## What is refused

Deterministic code-graph facts (`codegraph:` domain — File / Symbol / Import /
CALLS edges / Commit / Change and every other scan-derived record) are
reproducible from source; hand-deleting them would silently desynchronize the
graph. `eg forget` refuses them with exit 1 and a machine-readable envelope
naming the correction path:

```json
{"ok":false,"error":{"code":"deterministic_code_fact","detail":{
  "record_id":"codegraph:v5:…","kind":"Symbol","remedy":"eg refresh",
  "message":"… correct them with `eg refresh` or a re-scan"}}}
```

Derived semantic measurements (`semantic:` domain — `SemanticDrift` nodes and
their `DRIFTS_FROM` / `DRIFTS_PRIOR` edges) are refused for the same reason
(`derived_semantic_record`, exit 1): they are re-derived deterministically
from history with a pinned embedding model and threshold, and the semantic
schema freezes them as immutable at a stable ID
([`docs/schema/semantic-drift.md`](../schema/semantic-drift.md)). They are
also temporal records the current-state read deliberately re-emits for
`--at` views, so a retraction tombstone would never actually suppress them —
accepting the handle would report success while `eg query drift` kept
returning the record. Correct them with a re-scan and re-ingest.

Any other commit-anchored (temporal) node — e.g. a manually ingested
observation or verification record carrying temporal metadata — is refused
for that same mechanical reason (`temporal_record`, exit 1): the embedded
current-state read re-emits every per-commit snapshot for `--at` history
views regardless of tombstones, so a retraction tombstone could never
actually suppress the record and success would be a silent lie. Correct such
a record by re-ingesting without it. Commit-anchored **edges** stay
retractable: evidence-link edges legitimately carry routing-only commit
anchors, and the edge read path honors tombstones for them.

Tombstones and retraction events themselves are also refused
(`unsupported_target`): forgetting the audit trail would turn retraction back
into a silent hole.

## Citing records

Retracting a record never deletes records that merely cite it. A surviving
claim whose evidence link targets the retracted handle keeps the link, and
`eg query memory <citing-id>` reports it as a `stale_evidence_target`
diagnostic — consistent with existing stale-evidence behavior — rather than
silently dropping it.

## Bi-temporal honesty

Retraction is scoped to the transaction-time axis. The physical record stays
in the store, so a historical transaction-time view predating the retraction
still reflects that the record existed then; only transaction-time-current
reads exclude it. This is deliberately **not** `git filter-repo`-style history
rewriting.

## Exit codes and envelope

| Exit | Meaning |
|------|---------|
| 0 | Retracted, or already retracted (idempotent no-op; `action` distinguishes). |
| 1 | Refused (`deterministic_code_fact`, `derived_semantic_record`, `temporal_record`, `unsupported_target`) or malformed (`missing_reason`, `missing_actor`, `invalid_transaction_time`). |
| 2 | `not_found` — the handle resolves to no record. |

Success prints one JSON envelope on stdout:

```json
{"ok":true,"action":"retracted","retraction":{
  "retraction_id":"agent_memory:v1:…","tombstone_id":"agent_memory:v1:…",
  "retracted_record_id":"agent_memory:v1:…","retracted_by":"op-1",
  "retracted_at":"2026-07-01T00:00:00Z","reason":"leaked customer name"}}
```

Failures print a machine-readable envelope on stderr. With a pinned
`--transaction-time`, output is deterministic and byte-identical across runs.

## Out of scope

- Bulk/glob retraction, time-window purges, and policy-driven auto-expiry.
- Physical erasure / cryptographic shredding of bytes at rest.
- Editing or correcting a record in place (that is supersede/contradict).
- Retracting deterministic code-graph facts, derived semantic measurements,
  or commit-anchored temporal nodes (explicitly refused above).
- Re-deriving or rewriting Git history.
