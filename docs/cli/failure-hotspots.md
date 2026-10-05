# eg query failure-hotspots

Rank **code targets by repeated agent-failure density** — answer the
operator-visible question *"where have agents repeatedly failed, across all
imported runs?"* — without needing to suspect a target first. The store-wide
complement to [`eg query failures`](failure-history.md): where that lane
answers "did anyone fail HERE?", this lane surfaces the quagmires. Local-first;
no network access.

> **A hotspot is a lead, not a diagnosis.** A high rank means many distinct
> agent runs failed against the target; it never states WHY they failed.
> Root-causing stays with the agent — this lane ranks and cites.

## Synopsis

```text
eg query failure-hotspots --graph <PATH> [--limit N] [--as-of <TS>] [--since <TS>]
eg query failure-hotspots --data-dir <DIR> [--limit N] [--as-of <TS>] [--since <TS>]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`).

## Rank semantics

Targets are resolved `Symbol`/`File` records. Every live agent-authored
`Failure` record resolves to its code targets through the failure-link
relations (`FAILED_ON`, `TOUCHED_FILE`, `MENTIONS_SYMBOL`, `OBSERVES`,
`REFERENCES_TASK`, `PRODUCED_EVIDENCE`, `HAS_EVIDENCE`, `VALIDATED_BY`) and
denormalized evidence links — the same edge semantics as
`eg query failures`, including the `Failure → PatchArtifact → File` relay.

Targets are then ranked by:

1. **`distinct_run_count` descending** — the number of distinct agent runs
   that failed against the target. The run key is the failure's `session_id`
   provenance; a failure with no `session_id` counts as its own run.
2. **`failure_count` descending** — total contributing `Failure` records
   (the documented tie-break).
3. **`target_record_id` ascending** — the final tie-break, making the ranking
   byte-identical across repeated runs on an unchanged store (issue #199).

Tombstoned (deleted) `Failure` records are excluded entirely (issue #421):
they appear neither in the ranking nor in `unresolved`.

## Output

NDJSON: a header line, then one line per ranked hotspot.

```jsonc
// header
{
  "ok": true,
  "lane": "failure-hotspots",
  "as_of": null, "since": null,          // echoed selectors
  "total_hotspots": 3,                   // before truncation
  "returned_hotspots": 3,                // after truncation
  "truncated": false,                    // completeness signal
  "limit": 20,
  "unresolved": [ ... ],                 // failures with no code target
  "disclaimer": "..."
}
// one row per hotspot
{
  "rank": 1,
  "target_record_id": "codegraph:v9:<hex>",
  "target_kind": "symbol",               // or "file"
  "repo_relative_path": "src/lib.rs",
  "span": { "start_line": 10, ... },
  "distinct_run_count": 2,
  "failure_count": 3,
  "failures": [
    {
      "record_id": "agent_memory:v1:<hex>",
      "run": "run-1",                   // session_id, or the failure's own ID
      "session_id": "run-1",
      "observed_at": "2026-02-01T00:00:00Z",
      "failure_kind": "command_failure"
    }
  ],
  "trust": "agent_authored"              // issue #114: agent claims, not source truth
}
```

`--format text` renders a human-readable list (unstable by contract).

## The `unresolved` section

A live `Failure` that resolves to no `Symbol`/`File` target is reported in
the header's `unresolved` array — never silently dropped:

- `reason: "task_only"` — linked only to task/project records (code-target
  ranking is out of scope for it);
- `reason: "no_code_target"` — linked to nothing live.

## Temporal selectors

`--as-of <TS>` aggregates only failures observed at or before the RFC 3339
instant; `--since <TS>` only those observed at or after. The two combine into
a window (`since..=as_of`), so an operator can ask "where were agents stuck
last week?". The failure instant is `executed_at` preferred, else
`observed_at`. **Undated failures are excluded when a temporal selector is
active** — the lane cannot prove they were observed in the window; without
selectors they participate (the conservative default).

## Exit codes

| Condition | Exit | Output |
|-----------|------|--------|
| Success (ranked rows; rows may be empty when every failure is `unresolved`) | `0` | NDJSON on stdout, `ok:true` |
| Invalid `--limit` (0 or > 200), or malformed `--as-of`/`--since` | `1` | `{"code":"invalid_limit"/"malformed_timestamp",...}` on stderr |
| Zero live `Failure` records in scope | `2` | `{"ok":false,"error":{"code":"no_match",...}}` on stdout |

`--limit N` caps the rows; the header signals truncation (`truncated: true`,
`total_hotspots` vs `returned_hotspots`) so a capped list is never mistaken
for "only N hotspots exist".

## Out of scope

Diagnosing *why* a target fails, new `Failure` capture, per-handle lookup
(`eg query failures`, issue #178), cross-store aggregation, and predictive
"this will fail" scoring. The lane consumes records the existing importers
already produce.
