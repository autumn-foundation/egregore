# `eg audit evidence-links`

Audit whether every cross-domain evidence link in a store still points at a live
record. Local-first, read-only, no network, no embeddings, no daemon.

> A clean gate proves every cross-domain evidence edge resolves to a **live**
> record; it is NOT proof the cited code still matches current source (that is
> drift — see `eg query verification-freshness`, issue #111, and the on-demand
> single-handle verdict tracked in #160), and NOT proof the store is otherwise
> correct. "Broken" here means the target is **absent** or **tombstoned**,
> never "drifted".

Egregore's value is that evidence links connect code, agent memory, tasks,
artifacts, and verification *without letting guesses masquerade as source truth*.
A cross-domain evidence edge (`OBSERVES`, `VALIDATED_BY`, `HAS_EVIDENCE`,
`CLOSES_ACCEPTANCE_CRITERION`, `OWNED_BY_TASK`, `REFERENCES_TASK`, …) whose target
record is missing or tombstoned is a citation that resolves to nothing, presented
as if it were backed. This command is the post-ingest, all-domains health gate
that sweeps a whole loaded store and reports every such broken edge.

## Synopsis

```powershell
eg audit evidence-links --graph <PATH> [--format json|text]
eg audit evidence-links --data-dir <DIR> [--format json|text]
```

`--graph` and `--data-dir` are mutually exclusive; exactly one is required.

## Shortest local workflow

```powershell
eg scan . --out graph.jsonl
eg audit evidence-links --graph graph.jsonl
echo "exit: $LASTEXITCODE"
```

Or over an ingested embedded store:

```powershell
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
eg audit evidence-links --data-dir .egregore
```

## Interpreting pass/fail

| Exit | Meaning |
|---|---|
| **0** | Clean — `broken_edge_count == 0`, `ok: true`; a `no_broken_evidence_links` diagnostic. Full report on stdout. |
| **1** | Broken links found — `ok: false`; the full report (with `broken_edges`) is still printed to stdout so `eg repair` (#72) and CI can consume it. |
| **2** | Usage/load error (both/neither input flag, unreadable/empty store or graph). A redaction-safe JSON error is printed to stderr. |

The gate is binary: any broken evidence link fails it, so the command is usable
as a CI or pre-swarm trust gate.

## Report shape

The JSON envelope carries:

- `ok` — `true` iff zero broken evidence edges were found.
- `checked_edge_count` — total CHECKED evidence edges swept (standalone edge
  records plus inline evidence links that carry a target ID).
- `broken_edge_count` — `broken_edges.len()`.
- `by_source_domain` / `by_edge_label` / `by_case` — broken-edge counts keyed by
  source domain, edge label, and case (`absent` / `tombstoned`).
- `broken_edges` — one row per broken edge (see below), canonically ordered.
- `checked_edge_labels` / `excluded_edge_labels` — the sorted edge-label
  partition, disclosed so a clean verdict is never mistaken for proof that no
  grounding exists.
- `diagnostics` — stable machine-readable markers
  (`no_broken_evidence_links`, `unresolvable_link_no_target_id`).

Each `broken_edges` row carries:

| Field | Meaning |
|---|---|
| `source_record_id` | Stable ID of the record making the claim. |
| `source_domain` | Source domain (`agent_memory`, `project`, `artifact`, `verification`, …). |
| `source_kind` | Source node kind (`Observation`, `AcceptanceCriterion`, …). |
| `edge_label` | Cross-domain evidence edge label wire name (`OBSERVES`, …). |
| `representation` | `edge_record` (standalone edge) or `inline_evidence_link`. |
| `edge_record_id` | Stable edge-record ID, present only for `edge_record` rows. |
| `target_record_id` | The unresolved target ID, verbatim. |
| `case` | `absent` (no record with that ID exists) or `tombstoned` (exists but not live). |
| `target_domain` | Target domain declared by an inline evidence link, when present. |
| `target_repo_relative_path` / `target_span` | Recovered when the target is (was) a code node and a node version survives. |

`absent` means no record with that ID exists anywhere in the loaded set.
`tombstoned` means the target exists but is no longer live (retracted/deleted, or
only a tombstone survives). A node re-ingested *after* its own tombstone is live
again on both `--graph` and `--data-dir` transports (latest-write-wins liveness)
and is therefore not reported.

## What is and isn't checked

**Checked** (their target must resolve to a live record): the cross-domain
evidence / provenance / grounding edges — `HAS_EVIDENCE`, `OBSERVES`,
`MENTIONS_SYMBOL`, `TOUCHED_FILE`, `PRODUCED_PATCH`, `PRODUCED_EVIDENCE`,
`VALIDATED_BY`, `CLOSES_ACCEPTANCE_CRITERION`, `OWNED_BY_TASK`, `EXTERNAL_HANDLE`,
`TOUCHES_FILE`, `MERGED_AS`, `REVIEWS_COMMIT`, `REVIEWED_BY`,
`REQUESTED_REVIEW_FROM`, `TRANSITIONS_REVIEW`, `FAILED_ON`, `EXPLAINS_CHANGE`,
`REFERENCES_TASK`, `CONTRADICTS`, `SUPERSEDES`, `RELATES_TO`, `FRAME_RESOLVES_TO`,
`EMITTED_DURING`, `MATERIALIZED_AS`, `PROPOSED_BY`, `PROMPTED_FOR`, `DECIDED_ON`,
`REVOKED_BY`, `SCOPED_TO_REPO`, `FINGERPRINTED_AS`, `CAPTURED_FROM`, `AGGREGATES`.

**Not checked** (code-graph structural topology — that is `eg validate`'s job,
#103 — and intra-agent-memory scaffolding): `CONTAINS`, `DEFINES`, `IMPORTS`,
`REFERENCES`, `CALLS`, `IMPLEMENTS`, `MENTIONS`, `CHANGED_IN`, `PARENT_OF`,
`DRIFTS_FROM`, `DRIFTS_PRIOR`, `MEASURED_BY`, `SESSION_OF`, `AUTHORED_BY`.

The partition is an exhaustive, compile-time-checked classification of every edge
label — a new label cannot be added without consciously classifying it here.

Both **representations** of an evidence link are swept: standalone
`GraphRecord::Edge` records and inline evidence links carried on nodes (including
the user-context `supporting_evidence` / `contradicting_evidence` lists). An
inline link that is **triple-only** (no `target_record_id`) is not an ID
reference, so it is out of the "absent/tombstoned target" scope; it is tallied in
the `unresolvable_link_no_target_id` diagnostic, never reported as a broken edge.

## Safety: no raw payloads

Output is allow-list only: record IDs, domain/kind strings, edge-label wire
names, repo-relative paths, spans, counts, and closed-enum markers. No raw source
text, transcript text, command output, patch hunk, or issue/PR body ever appears
in the report.

## Determinism

Re-running the identical audit against an unchanged graph or store produces
byte-identical output across consecutive runs. All aggregate maps are sorted, and
`broken_edges` uses a total-order tie-break of
`(source_domain, edge_label, source_record_id, target_record_id, case,
representation, edge_record_id)`. The command reads no wall clock and no
environment. Over `--data-dir` the store is read through a throwaway copy, so the
original is left byte-for-byte untouched.

## When to use this versus other tools

| Reach for | When |
|---|---|
| **`eg audit evidence-links`** (this) | Post-ingest, all-domains health gate: "is this store safe to query — does every evidence link still resolve to a live record?" before agents query or a swarm runs. |
| `eg validate` (#103) | **Pre-ingest** referential closure of a **code-graph** JSONL. |
| `eg audit memory-health` (#185/#94) | Agent-memory **recall-time** staleness of observations. |
| `eg query evidence-path` / handle resolution (#160) | Resolve **one** cited handle / trace one witness chain on demand. |
| `eg audit citations` (#65) | Completeness of **one answer's** citations across query workflows. |
| `eg repair` (#72) | **Fix** the broken edges this audit reports (consumes this output). |
