# eg link-logs

Link **runtime error signatures** to the **agent runs and commands** that
preceded them, emitting `EMITTED_DURING` evidence-link edges (issue #323).

Given a union graph of log records (from [`scan-logs`](scan-logs.md)),
agent-memory & verification records (`AgentRun` / `AgentTurn` / `CommandRun`),
and project `Task` records, this command binds each `ErrorSignature` to the
runs / commands that plausibly produced it and mints one `EMITTED_DURING` edge
per correlation — each carrying a closed-set correlation **basis**, and each
mirrored by an `EvidenceLink` on the `ErrorSignature`. Task/issue linkage reuses
the existing `REFERENCES_TASK` evidence-link surface (no new project-facing
label).

> **An `EMITTED_DURING` edge is a correlation lead — never causation.**
> `content_hash_join` proves the log artifact *is* the command's captured output
> (exact byte equality). `temporal_correlation` proves only that the signature's
> time falls **inside** a run's execution window — *overlapping in time is never
> proof that the run produced the error, caused it, or is at fault.*

Fully offline and filesystem-local: no network, no embeddings, no daemon. Output
is deterministic and byte-identical across runs; raw log / transcript / command
text never enters the graph.

## Synopsis

```text
eg link-logs --graph <GRAPH.jsonl> [--graph <MORE.jsonl> ...] --out <OUT.jsonl> \
    [--tolerance <SECS>] [--at <SHA> | --as-of <RFC3339>]
eg link-logs --data-dir <DIR> --out <OUT.jsonl> \
    [--tolerance <SECS>] [--at <SHA> | --as-of <RFC3339>]
```

- `--graph <GRAPH.jsonl>` — a graph JSONL to union. **Repeat** the flag to union
  several files (e.g. a `scan` code graph + a `scan-logs` log graph + imported
  agent records). Mutually exclusive with `--data-dir`.
- `--data-dir <DIR>` — an embedded store holding the union of records.
- `--out <OUT.jsonl>` — output JSONL for the enriched records.
- `--tolerance <SECS>` — symmetric window tolerance in seconds for
  `temporal_correlation` (default `0` = strict). A run window `[start, end]`
  matches a signature time `t` when `start - tolerance <= t <= end + tolerance`.
  Must be `>= 0`; a negative value prints an `invalid_tolerance` diagnostic and
  exits 1.
- `--at <SHA>` / `--as-of <RFC3339>` — record a commit view on emitted evidence
  links (requires a history graph). Mutually exclusive; passing both prints an
  `unsupported_combination` diagnostic and exits 1.

## Correlation bases

Every `EMITTED_DURING` edge carries **exactly one** basis from this closed,
stable set — **no edge is ever emitted without a basis**:

| Basis | Wire string | Rule | Confidence | Trustworthy when… |
|-------|-------------|------|------------|-------------------|
| Content-hash join | `content_hash_join` | The `LogSource` the signature was `CAPTURED_FROM` carries a `source_artifact_hash` equal to a `CommandRun`'s captured stdout/stderr `OutputHandle.hash`. | `1.0` | Always — exact BLAKE3 byte equality means the log artifact *is* that command's output. Deterministic and inherently within one repository. |
| Temporal correlation | `temporal_correlation` | The signature's representative valid time falls inside an `AgentRun` / `AgentTurn` window `[started_at, finished_at]` (± `--tolerance`) for the **same repository**. | `0.5` | Only as a *lead*. Overlap in time is suggestive, never proof; unrelated errors can share a window, and clock skew or coarse timestamps can mislead. |

Both bases may apply to one signature (different targets) — that is honest and
allowed. The signature's representative valid time is `last_seen`, falling back
to `first_seen`.

**Overlapping runs each mint their own edge** — when several run windows contain
the signature's time, every candidate run gets an edge; no single winner is ever
silently chosen.

## Repository boundary

- **`content_hash_join`** is inherently within-repository: identical BLAKE3 bytes
  cannot span two repositories by construction, so no repository check is
  applied.
- **`temporal_correlation`** is repository-guarded. The linker resolves a single
  **repository anchor** — the sole `Repository` node's stable ID. Each
  signature's repository is *verified* by recomputing its `LogSource` stable ID
  against that anchor
  (`log:v3:blake3("log_source", anchor, source_relative_path,
  source_artifact_hash)`) and checking it equals the stored `LogSource` ID; this
  reads the repository identity the ID hash already encoded. (Schema v3 also
  persists that identity as a retrievable `repository_id` field per issue #362,
  but link-logs still verifies by recompute — it needs no field read.) A
  signature whose `LogSource` does not recompute to the anchor
  is *foreign*: its in-window candidates are suppressed and counted in
  `cross_repo_rejected`.
- With **zero or multiple** `Repository` anchors the linker cannot attribute
  agent runs to a repository (the agent-memory domain carries no finer
  repository binding), so `temporal_correlation` is **disabled fail-closed** and
  every in-window candidate is tallied in `cross_repo_rejected`.
  `content_hash_join` is unaffected.

**Limitation:** temporal correlation requires exactly one `Repository` anchor in
the input (normally supplied by unioning a `scan` code graph). Foreign-log
rejection is per-signature via `LogSource` ID recomputation; agent runs are
attributed to the single anchor.

## Task / issue linkage

For each `EMITTED_DURING` edge `signature → run`, if that run already carries an
existing `REFERENCES_TASK` or `OWNED_BY_TASK` edge to a `Task` / `GitHubIssue` /
`LocalTask` node, the linker mints a `REFERENCES_TASK` edge `signature → task`,
propagating the discovering edge's basis confidence (strongest basis wins per
`(signature, task)`). No new project-facing edge label is introduced; a
`REFERENCES_TASK` edge carries **no** correlation basis.

## Dual representation

Every emitted edge is mirrored by an `EvidenceLink` on the source
`ErrorSignature` node (`evidence_links` array). The node-field and graph-edge
representations agree at write time (relation, source, target). See
[`docs/schema/agent-memory.md`](../schema/agent-memory.md) §5.

## Output

Enriched log-domain records (each `ErrorSignature` carrying its new evidence
links, the other log records unchanged) followed by the new `EMITTED_DURING` /
`REFERENCES_TASK` edges, in canonical order. Non-log records (agent /
verification / project targets) are referenced by stable ID and never
re-emitted. The enriched `ErrorSignature` re-emits its log payload unchanged
except for the added evidence links, so the schema-v3 `repository_id` field
(issue #362) is **preserved** through enrichment and no log record ID changes. A
deterministic JSON envelope is printed to stdout:

```json
{
  "ok": true,
  "command": "link-logs",
  "at_commit": null,
  "as_of": null,
  "totals": {
    "signatures": 3,
    "content_hash_join_edges": 1,
    "temporal_correlation_edges": 2,
    "task_link_edges": 1,
    "uncorrelated": 2,
    "cross_repo_rejected": 1,
    "tolerance_seconds": 0,
    "temporal_correlation_enabled": true
  },
  "signatures": [ { "signature_id": "log:v3:…", "content_hash_join": 1,
                    "temporal_correlation": 2, "task_links": 1,
                    "uncorrelated": false } ],
  "disclaimer": "An EMITTED_DURING edge is a correlation lead, never causation. …"
}
```

- `uncorrelated` — signatures that received **zero** `EMITTED_DURING` edges of
  any basis. Absence is always reported, never hidden.
- `cross_repo_rejected` — signatures whose in-window temporal candidates were
  suppressed by the repository guard.

## Redaction / safety

Output carries only record IDs, bases, confidences, counts, and time bounds —
never raw transcript text, raw command output, or log payload text. Every edge
carries a stable record ID, `schema_version`, relation, basis (for
`EMITTED_DURING`), confidence, and both endpoint IDs.

## Scope

This command introduces only the linker and the `basis` edge field. Rendering
the links into a query surface is out of scope (issues #324 / #325). Root-cause
or blame inference is a non-goal: a binding proves a signature was *emitted
during* a run, never that the run is responsible.

## See also

- [`scan-logs`](scan-logs.md) — produces the `LogSource` / `ErrorSignature`
  records.
- [`resolve-frames`](resolve-frames.md) — the sibling backtrace-frame resolver
  (issue #322).
- [`docs/schema/log-graph.md`](../schema/log-graph.md) — the `EMITTED_DURING`
  edge and correlation-basis schema.
