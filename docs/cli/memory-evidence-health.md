# `eg audit memory-evidence-health`

Sweep every agent-memory observation's cited evidence and classify each link as
`resolves_live`, `drifted`, or `dangling` — plus flag nodes whose denormalized
`evidence_links` array disagrees with their stored edge representation.
Local-first, read-only, no network, no embeddings, no daemon.

> A citation that silently rotted still *looks* authoritative. This command is
> the maintenance primitive that turns "which stored agent memories now cite
> evidence that no longer resolves to live, current source?" into a
> deterministic, machine-readable report — so rotted memory can be triaged
> before it flows back into agent context as if it were still verified.

The agent-memory schema makes `evidence_links` required on every `Observation`
at write time (`docs/schema/agent-memory.md` §3/§5), but write-time checks
never re-run as the code moves underneath: a cited target gets renamed or
deleted (its content-addressed `record_id` changes), or it drifts (a
`SemanticDrift` record already exists for it, #55). Per-record audit (#64)
needs an id you already suspect; single-handle deref (#160) checks one handle.
This sweep covers the whole store.

**Issue:** #185 — *Flag agent memories whose cited evidence is now stale or
dangling*.

## Synopsis

```powershell
eg audit memory-evidence-health --graph <PATH> [--format json|text]
eg audit memory-evidence-health --data-dir <DIR> [--format json|text]
```

`--graph` and `--data-dir` are mutually exclusive; exactly one is required
(the `egregore.toml` `data_dir` config applies when neither is passed, per the
#261 convention shared by all audit lanes).

## Shortest local workflow

```powershell
eg scan-history . --out history.graph.jsonl
# … agents write observations citing code handles over time …
eg audit memory-evidence-health --graph history.graph.jsonl
echo "exit: $LASTEXITCODE"
```

Or over an ingested embedded store (read through a throwaway copy — the
original is never re-persisted or mutated):

```powershell
eg ingest history.graph.jsonl --adapter embedded --data-dir .egregore
eg audit memory-evidence-health --data-dir .egregore --format text
```

## What is swept

Every **live** agent-authored observation-class node carrying `evidence_links`:

| Node kind | Covered | Notes |
|---|---|---|
| `Observation` | yes | The canonical agent-memory claim node. |
| `Decision` | yes | Carries required `evidence_links` per the schema. |
| `Failure` | yes | Live-authored failures cite evidence links. |
| `Lesson` / `Hypothesis` | when present | No `NodeKind` variants exist yet; the sweep picks them up automatically when a future slice adds them (`is_observation_class_kind` is the single extension point). |

Tombstoned source nodes are skipped (a deleted memory is not stored memory).
All other node kinds are out of scope, even when they carry `evidence_links`.

## Buckets

Each evidence link lands in **exactly one** bucket:

| Bucket | Meaning |
|---|---|
| `resolves_live` | A live node with the target `record_id` exists, and no live `SemanticDrift` record applies to it. |
| `drifted` | The target exists, but a live `SemanticDrift` record whose `prior_record_id` is the target applies — the cited span moved after it was cited (the "changed-span verdict"). The earliest such record (by `after_valid_time`, tie-broken by drift record id) is reported as `drift_record_id`. A `drifted` verdict is a **freshness lead, never a truth claim**: a reason to re-verify, not proof the note is wrong. |
| `dangling` | No live node carries the target `record_id`. The `tombstoned` flag distinguishes `true` (the target existed and was tombstoned — deliberately removed) from `false` (the id was never present — wrong or fabricated handle). A link with no `target_record_id` at all is `dangling`: it resolves to nothing. |

Drift records are *read*, never minted: a tombstoned `SemanticDrift` record
does not apply, and a re-ingested drift record collapses to its latest version.

## Integrity violations (AC3)

The schema requires the denormalized `evidence_links` array and the stored
edge representation to agree at write time (§5). The sweep diffs, per source
node, the multiset of `(target_record_id, relation)` pairs in the array
against the multiset of `(target, label)` pairs among the node's live,
latest-version, integrity-checked evidence edges (the #217 edge partition —
code-graph structural topology and intra-agent-memory scaffolding are never
grounding evidence):

| Violation | Meaning |
|---|---|
| `array_without_edge` | The array cites a `(target, relation)` the stored edges lack (with `array_count` / `edge_count`). |
| `edge_without_array` | A stored evidence edge has no matching array entry. |

## Interpreting pass/fail

| Exit | Meaning |
|---|---|
| **0** | Clean — `ok: true`: zero dangling links and zero integrity violations. The full report is still printed. |
| **1** | Findings — `ok: false`; the full report is still printed to stdout so triage tooling can consume it. |
| **2** | Usage/load error (both or neither input flag, unreadable/empty store or graph). A redaction-safe JSON diagnostic (`empty_memory_evidence_input`) is printed to stderr on empty input — a false "clean" on an empty store would be dangerous for a trust gate. |

`drifted` links do **not** fail the gate: drift is a freshness lead, not rot.
Only `dangling` citations (resolving to nothing) and representation corruption
fail it.

## Report shape

Default output is **newline-delimited JSON**: one JSON object per line —
`link` rows, then `integrity_violation` rows, then exactly one `summary` line.
Re-running against an unchanged store is byte-for-byte identical (all maps are
sorted; row order is canonical by `(source_record_id, target_record_id,
relation)`).

Link row (`"type": "link"`):

```json
{"type":"link","source_record_id":"agent_memory:v1:obs1","source_kind":"Observation","target_record_id":"codegraph:v1:sym9f","target_domain":"codegraph","relation":"OBSERVES","bucket":"drifted","drift_record_id":"semantic:v1:drift3"}
{"type":"link","source_record_id":"agent_memory:v1:obs1","source_kind":"Observation","target_record_id":"codegraph:v1:gone","target_domain":"codegraph","relation":"OBSERVES","bucket":"dangling","tombstoned":false}
```

- `drift_record_id` appears only on `drifted` rows; `tombstoned` only on
  `dangling` rows. Every row carries the source observation's citable
  `record_id` and the cited target handle, so output is actionable input to
  other `eg` commands (e.g. `eg query memory`, `eg audit citations`).

Integrity-violation row (`"type": "integrity_violation"`):

```json
{"type":"integrity_violation","source_record_id":"agent_memory:v1:obs2","source_kind":"Observation","violation":"array_without_edge","target_record_id":"codegraph:v1:sym9f","relation":"OBSERVES","array_count":1,"edge_count":0}
```

Summary line (`"type": "summary"`, always last):

```json
{"type":"summary","ok":false,"sources_checked":4,"links_checked":8,"resolves_live":5,"drifted":1,"dangling":2,"integrity_violations":2}
```

`--format text` renders the human view: per-source link rows grouped under
each source record, integrity violations, and the bucket-count summary.

## Redaction safety

Rows carry only record IDs, domain/kind/relation wire strings, closed-enum
buckets, and counts — never raw source, transcript, command, patch, or text
payloads (mirrors #217 and the #188 failure-history serializer discipline).

## Related

- `eg query evidence-freshness` (#85) — anchor-based per-link freshness
  verdicts (`current` / `drifted` / `unresolved` / `untemporal`) for
  `Observation` / `Decision` citing code at a recorded commit or valid-time.
  #185 is the store-wide, anchor-free complement: every link of every
  observation-class node classified into the three health buckets, plus the
  array-vs-edge integrity check #85 does not perform.
- `eg audit evidence-links` (#217) — all-domains broken-edge sweep (absent or
  tombstoned targets); #185 is agent-memory-scoped and adds the drift bucket.
- `eg audit memory-health` (#94) — aggregate agent-memory composition health
  (provenance coverage, thresholds); #185 is per-link evidence resolution.
- `eg audit citations` — the write-time citation gate (#183); #185 audits
  already-stored records as the code moves underneath.
