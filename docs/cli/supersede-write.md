# Superseding or contradicting a prior observation at write time: `eg write observation --supersedes / --contradicts`

When a live agent learns its earlier note was wrong, it can say so on the
write path it actually uses. `--supersedes <id>` authors a `SUPERSEDES` edge
from the new observation to a prior observation-class record;
`--contradicts <id>` authors a `CONTRADICTS` edge instead. Both are consumed
by the existing read lanes — the `eg query memory` audit (#64) surfaces the
supersession in `superseding_records`, and the belief timeline (#235) replays
author-written `SUPERSEDES` / `CONTRADICTS` edges through the shared temporal
resolver — with no schema change: only existing `SUPERSEDES` / `CONTRADICTS` edge labels on existing `agent_memory:` records.

The workflow is completely local: no network, no hosted indexing, no remote
embeddings, and nothing is inferred. The agent names the target explicitly,
the target is validated against a local store at write time, and the write is
purely additive — the prior record is never rewritten, so its provenance
fields (`agent_id`, `observed_at`, `source_handle`) stay exactly as authored.

## Accepted write

```bash
# 1. The prior observation lives in a local store (JSONL here; an embedded
#    store via --data-dir works the same).
eg write observation \
  --agent-id aide-1 --agent-kind other --session-id sess-9 \
  --observed-at 2026-09-26T12:00:00Z \
  --source-handle "/tmp/run-9.log" \
  --text "blocking IO here" \
  --confidence 0.9 \
  --evidence-target codegraph:v9:deadbeef... \
  --out prior.jsonl
# {"ok":true,"evidence_handle":"agent_memory:v1:9f2c...","records":5}
PRIOR=$(python3 -c '
import json
for line in open("prior.jsonl"):
    r = json.loads(line)
    if r.get("record_type") == "node" and r.get("kind") == "Observation":
        print(r["id"]); break
')
# (the evidence_handle is the Observation node's stable ID)

# 2. The correction names the prior record explicitly and points at the same
#    store so the target can be validated before anything is written.
eg write observation \
  --agent-id aide-1 --agent-kind other --session-id sess-9 \
  --observed-at 2026-09-26T13:00:00Z \
  --source-handle "/tmp/run-10.log" \
  --text "blocking IO deadlocked under load; switch this path to async" \
  --confidence 0.9 \
  --evidence-target codegraph:v9:deadbeef... \
  --supersedes "$PRIOR" \
  --graph prior.jsonl \
  --out correction.jsonl
# {"ok":true,"evidence_handle":"agent_memory:v1:41bd...","records":6,
#  "supersession_edge":"agent_memory:v1:77e1..."}
```

The output batch carries the usual five records plus one `SUPERSEDES` edge
from the new observation to the prior one; its handle is surfaced as
`supersession_edge`. Ordinary evidence links are still required — the
correction must be grounded like any other observation.

`--contradicts` is the same shape with a different relation:

```bash
eg write observation \
  --agent-id aide-1 --agent-kind other --session-id sess-9 \
  --observed-at 2026-09-26T13:00:00Z \
  --source-handle "/tmp/run-10.log" \
  --text "counter-evidence: blocking IO held up fine in the soak test" \
  --confidence 0.7 \
  --evidence-target codegraph:v9:deadbeef... \
  --contradicts "$PRIOR" \
  --graph prior.jsonl \
  --out dispute.jsonl
```

A `CONTRADICTS` edge is a mutual dispute, not a winner declaration: both
records stay live and citable, and the read side flags both as
`contradicted`, each pointing at the other. It is a temporal-authorship
lead — "this agent recorded a dispute at this time" — never a truth claim
about which record is correct.

## Rejected write

```bash
eg write observation \
  --agent-id aide-1 --agent-kind other --session-id sess-9 \
  --observed-at 2026-09-26T13:00:00Z \
  --source-handle "/tmp/run-10.log" \
  --text "blocking IO deadlocked under load; switch this path to async" \
  --confidence 0.9 \
  --evidence-target codegraph:v9:deadbeef... \
  --supersedes agent_memory:v1:doesnotexist \
  --graph prior.jsonl \
  --out correction.jsonl
# stderr: {"code":"unknown_supersession_target","field":"supersedes",
#          "target":"agent_memory:v1:doesnotexist"}
# exit code 2; correction.jsonl is not written.
```

Rejections are machine-readable JSON on stderr and never echo the
observation text or payload values — only the handles the caller supplied.

| Rejection | Code | Exit | Meaning |
|-----------|------|------|---------|
| missing target ID | `unknown_supersession_target` | 2 | The handle names no live record in the store (tombstoned records count as absent). |
| deterministic code fact | `supersede_codegraph_fact` | 1 | Target is a `codegraph:` node, edge, or tombstone (e.g. `Symbol`, `File`, `Import`, `Call`, `Commit`). Code facts are corrected by re-scan, never by agent supersession. |
| non-observation record | `supersede_non_observation_target` | 1 | Target is live but not observation-class (`Observation`, `Decision`, `Failure`) — e.g. an `Agent` node. |
| cycle | `supersession_cycle` | 1 | The target is the new observation itself — the only cycle a fresh write can introduce. |
| no store | `supersession_store_required` | 1 | `--supersedes` / `--contradicts` without `--graph` or `--data-dir`. The target is never inferred. |
| unreadable store | `supersession_store_unreadable` | 1 | The `--graph` file or `--data-dir` store could not be read. |
| idempotency conflict | `supersession_conflict` | 1 | The same observation identity already carries a live edge of the same relation to a *different* target. |
| bad label (library) | `invalid_field` | 1 | Direct `build_observation_records` callers passing a label other than `SUPERSEDES` / `CONTRADICTS` (reported on field `supersession`). |

`--supersedes` and `--contradicts` are mutually exclusive, as are `--graph`
and `--data-dir`.

## Read-back

The existing `eg query memory` lane consumes the result with no changes —
the prior record stays explicitly retrievable, and the audit flags the
supersession and supplies the new observation as the forward handle:

```bash
cat prior.jsonl correction.jsonl > combined.jsonl
eg query memory "$PRIOR" --graph combined.jsonl
# ... "superseding_records": [
#       {"record_id":"agent_memory:v1:41bd...","kind":"Observation",
#        "relation":"SUPERSEDES", ...} ]
```

## Idempotency and determinism

The observation ID is content-addressed, so it doubles as the idempotency
key: re-running the identical write (same text, provenance, evidence, and
target) returns the same record and edge handles byte-for-byte — five
identical runs produce byte-identical output. If the same key is re-used
with a *different* target, the write conflicts (`supersession_conflict`)
instead of duplicating or silently overwriting the edge.

## How this differs from retirement (#156) and preference promotion (#50)

- **Retirement (#156)** withdraws a record from current reads (a tombstone):
  the record stops appearing as live. **Supersession** keeps the prior
  record live and citable — it stays explicitly retrievable (the memory
  audit returns it with the supersession flagged) — but the read side flags
  it `superseded` and hands the reader the newer record.
- **Preference promotion (#50)** selects which candidate the agent should
  prefer going forward. **Contradiction** declares no winner: it records a
  mutual dispute and both sides stay visible, flagged `contradicted`.

In short: retirement hides, promotion ranks, supersession corrects, and
contradiction disputes.
