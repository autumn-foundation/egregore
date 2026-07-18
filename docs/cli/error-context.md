# eg query error-context

Assemble **one trust-separated cross-domain bundle for a runtime error
signature** (issue #324) — answer *"what does the graph know about this error?"*
in a single cited envelope instead of four separate tool round-trips. From a
`scan-logs` / `resolve-frames` / `link-logs` graph (optionally augmented with a
`scan-history` timeline and code facts) or an embedded store. Local-first; no
network access, hosted indexing, remote crawling, or mandatory remote
embeddings.

> **Every row is a CORRELATION LEAD, never proof of cause.** A resolved frame
> proves the backtrace *names* a symbol, not that it is at fault. An
> `EMITTED_DURING` edge is a content-hash or temporal correlation, never
> causation. The absence of a lead is not proof of unrelatedness. Occurrence
> data only reflects the log sources that were scanned.

This query is a **read-time join** of three already-shipped cores — it mints no
edge and introduces no new node kind, edge label, or trust class:

- the [`eg query context`](./query.md) cross-domain trust-separated bundle
  (reused verbatim for the code half via each resolved frame target);
- the [`eg link-logs`](./link-logs.md) / [`eg resolve-frames`](./resolve-frames.md)
  log-edge topology (`CAPTURED_FROM`, `FRAME_RESOLVES_TO`, `AGGREGATES`,
  `EMITTED_DURING`, `REFERENCES_TASK`) for the runtime half;
- the issue #118 [`eg query deltas`](./deltas.md) range mechanics for the
  `first_seen_range` symbol-delta overlap.

## Synopsis

```text
eg query error-context <HANDLE> --graph <PATH>    [--repo <SELECTOR>] [--as-of <RFC3339> | --at <COMMIT>] [--supersession exclude|include-but-flag] [--protected-store <DIR>]
eg query error-context <HANDLE> --data-dir <DIR>  [--repo <SELECTOR>] [--as-of <RFC3339> | --at <COMMIT>] [--supersession exclude|include-but-flag] [--protected-store <DIR>]
```

`<HANDLE>` resolves an `ErrorSignature` three ways, in precedence order:

1. **Exact record ID** — a `log:v3:<hex>` `ErrorSignature` stable ID.
2. **Fingerprint / template-hash prefix** — a hex prefix of the signature's
   stable-ID hex tail. A unique prefix resolves; a prefix matching two or more
   signatures is **ambiguous** (exit 1, all candidate IDs listed). A bare hex
   prefix that matches no fingerprint falls through to the symbol-name mode.
3. **Exact symbol name** — a `Symbol` name whose backtrace frames resolved to
   it (`FRAME_RESOLVES_TO`). A symbol named by *many* signatures returns **all**
   of them (exit 0) — this is not ambiguity.

A well-formed `log:v3:` handle that is not an `ErrorSignature` (absent, or a
`LogSource` / `LogOccurrenceBucket` ID) is `no_match` (exit 2), never silently
prefix-matched.

The query is purely read-time: it reads only the supplied store, never Git
state, so it cannot mutate the working tree. Raw log/transcript/command/patch
text never enters the response beyond the signature's bounded, post-redaction
`template_excerpt`.

### `--repo` scope (code side + log side, issue #362)

`--repo <SELECTOR>` scopes **both** the code side and the runtime/log side in a
shared multi-repo store. On the code side it restricts the `first_seen_range`:
both the commit timeline that brackets the signature's `first_seen`
(base/head/window commits) and the reused symbol-delta join are limited to
commits and code owned by that repository (via the `CONTAINS` topology, like
[`eg query log-deltas`](./log-deltas.md)); without this a foreign repository's
commits could bracket a foreign window.

Since **schema v3** (issue #362) every log payload persists a retrievable
`repository_id` (byte-equal to the code `Repository` node ID), so `--repo` now
**soundly filters the runtime/log sections too**: an `ErrorSignature` (and its
frames, buckets, and `EMITTED_DURING` observations) attributed to a **different**
repository is **excluded** via `RepositoryIndex::owner_of`, closing the
cross-repository false lead the old disclosure could only warn about. A legacy
`log:v2:` signature whose `repository_id` deserializes empty **cannot be proven
in-repo**, so it is conservatively **excluded** (a possible under-report, never a
cross-repository bleed) and tallied. When at least one such legacy signature is
excluded, the envelope carries a shrunken residual `repo_scope_caveat` (the same
`LogRepoScopeCaveat` shape as `eg query log-deltas`, carrying `repo_scope`,
`excluded_unattributed_signature_count`, and a fixed re-scan-remedy `message`); a
fully schema-v3 scoped store carries **no** caveat. Re-run `eg scan-logs` to
regenerate legacy records under schema v3 with retrievable attribution.

### Temporal selectors

- `--at <COMMIT>` re-resolves the backtrace frames against that commit view
  (spans as they existed at that commit), reusing the
  [`eg resolve-frames`](./resolve-frames.md) resolver.
- `--as-of <RFC3339>` bounds the occurrence-bucket view on the valid axis
  **endpoint-exactly** (issue #364). Each bucket contributes only its
  per-occurrence `occurrence_timestamps` at or before the instant: a schema-v3
  bucket straddling the cutoff is **sub-divided precisely** (its reported
  `occurrence_count` is the count at or before the cutoff, not the whole hour),
  and a bucket whose every occurrence falls after the cutoff drops out. A legacy
  `log:v2:` bucket carries no timestamps, so it **falls back** to the whole-hour
  predicate (its full count survives whenever `bucket_start` is at or before the
  cutoff). The envelope reports a per-response `occurrence_count_granularity`
  marker — `endpoint_exact` when every listed bucket carried timestamps, else
  `hourly_bucket` when at least one legacy bucket fell back — present **only**
  under `--as-of`. This reuses the same `bucket_occurrences_at_or_before` counter
  as [`eg query log-deltas`](./log-deltas.md), so the two lanes count identically.
  A malformed `--as-of` (not a full RFC 3339 instant) exits 1 with a
  machine-readable `invalid_as_of_timestamp` envelope — never silently ignored.

`--at` and `--as-of` are mutually exclusive; combining them exits 1 with a
machine-readable `unsupported_combination` envelope (mirroring
`eg resolve-frames`).

Over `--data-dir`, the `--at`/`--as-of` lane loads through the same log-retained
read surface as the current-state lane, so an enrichment-only `ErrorSignature`
rewrite is collapsed to a **single** observation here too (never double-counted),
while every non-log superseded/temporal version stays intact for valid-time
reconstruction (issue #363).

### `--supersession`

`--supersession exclude` (default) removes superseded/contradicted agent rows
from their section and reports them in `excluded`. `--supersession
include-but-flag` keeps them in place, each carrying `supersession_status` and
its forward handles. Deterministic code facts are never subject to supersession.

### `--protected-store <DIR>` (opt-in)

When set, the signature's captured `LogSource` `source_artifact_hash` is matched
at read time against the protected store's manifest (issue #60/#321). A hit
emits a `protected:v1:<hex>` handle with its class and byte length in
`protected_payloads`. **Raw bytes are never read.** The graph must carry zero
protected handles (they are resolved only at read time); a graph carrying one
while the flag is set exits 1 with `protected_handle_in_graph`.

### Embedded-store log-retention caveat (`--data-dir`)

When the query runs over an embedded (`--data-dir`) store that holds at least one
`ErrorSignature`, the response carries an `embedded_log_retention_caveat`
disclosing that the embedded lane now loads through the **log-retained** read
surface, which surfaces every **superseded** non-temporal `ErrorSignature` /
`LogOccurrenceBucket` version that differing-content `scan-logs` ingests append.
The cross-scan coalescing the `--graph` path performs (earliest `first_seen`,
latest `last_seen`, summed occurrence counts, all buckets) is therefore
reconstructed **exactly** on `--data-dir` for differing-content scans. Only
**distinct scan observations** are retained, where "distinct" is decided on the
**full scan payload** (window, occurrence count, and captured `frames` #322) —
not just `first_seen` / `last_seen` / `occurrence_count` — so two observations
sharing one occurrence window but differing in captured frames are both kept. The
standard `scan-logs -> resolve-frames -> link-logs` pipeline re-emits the same
`ErrorSignature` node enriched with node-level evidence links but with an
unchanged log payload, and that enrichment-only rewrite is retained as a
**single** observation — never double-counted. Since **issue #361** made
`LogOccurrenceBucket` identity source-aware, the former same-hour/same-count bucket
divergence is **gone**: distinct sources mint distinct bucket IDs (summed on both
paths) and a rescan mints the same bucket ID (deduped on both paths), so the
occurrence-bucket block **converges** across `--graph` and `--data-dir`. The one
residual divergence stems purely from idempotent-write dedup of byte-identical
non-temporal records (not bucket identity): a byte-identical re-ingest of the whole
`scan-logs` output is deduped to one physical record on `--data-dir` but summed on
`--graph`. The `--graph` path preserves every ingested line and
never carries the caveat; a pure `scan` store with no log records never carries
it either. This mirrors the identical disclosure on
[`eg query log-deltas`](./log-deltas.md); the adapter-level retention fix landed
in issue #363. This is a disclosure only — it never changes handle resolution or
section contents.

The retained read preserves the **`forget` retraction boundary**: a
`forget`-retracted `ErrorSignature` re-observed by a **later** `scan-logs` is not
resurrected — only the post-retraction observation reaches the coalesced sum, so a
re-scan after `forget` never revives a forgotten observation.

## Shortest offline workflow

```powershell
# Capture runtime error signatures from a log file
eg scan-logs app.log --repo-path . --out log.graph.jsonl

# Resolve backtrace frames onto code-graph targets (issue #322)
eg resolve-frames log.graph.jsonl --graph graph.jsonl --out resolved.graph.jsonl

# Link signatures to the agent runs/commands that produced them (issue #323)
eg link-logs --graph resolved.graph.jsonl --graph agent.graph.jsonl --out linked.graph.jsonl

# Concatenate code, history, and linked-log records into one graph
Get-Content graph.jsonl, history.graph.jsonl, linked.graph.jsonl | Set-Content combined.graph.jsonl

# Assemble the cross-domain error-context bundle
eg query error-context log:v3:<hex> --graph combined.graph.jsonl
```

## Response shape

```json
{
  "ok": true,
  "handle": "log:v3:<hex>",
  "signature_ids": ["log:v3:<hex>"],
  "signatures": [
    {
      "record_id": "log:v3:<hex>",
      "schema_version": 1,
      "trust_class": "runtime_observation",
      "severity": "error",
      "fingerprint_algorithm": "template-v1",
      "template_excerpt": "<bounded, post-redaction>",
      "first_seen": "2026-01-02T12:00:00Z",
      "last_seen": "2026-01-02T13:00:00Z",
      "occurrence_count": 5,
      "source_handles": [{ "record_id": "log:v3:<hex>", "source_relative_path": "app.log", "source_artifact_hash": "<blake3-hex>" }],
      "buckets": [{ "record_id": "log:v3:<hex>", "bucket_start": "2026-01-02T12:00:00Z", "bucket_width": "1h", "occurrence_count": 5 }],
      "frames": [{ "frame_index": 0, "frame_resolution": "resolved", "target_record_id": "codegraph:v5:<hex>" }]
    }
  ],
  "first_seen_range": {
    "status": "history",
    "anchor_signature_id": "log:v3:<hex>",
    "first_seen": "2026-01-02T12:00:00Z",
    "base_commit": "<sha>",
    "head_commit": "<sha>",
    "window_start": "2026-01-02T00:00:00Z",
    "window_end": "2026-01-03T00:00:00Z",
    "overlapping_symbol_deltas": [{ "record_id": "codegraph:v5:<hex>", "change_class": "modified_symbol" }]
  },
  "source_facts": [{ "record_id": "codegraph:v5:<hex>", "trust_class": "source_fact", "kind": "Symbol", "repo_relative_path": "src/lib.rs", "span": { "start_line": 1, "end_line": 10 } }],
  "observations": [{ "record_id": "agent_memory:v1:<id>", "trust_class": "agent_observation", "kind": "AgentRun", "correlation_basis": "temporal_correlation" }],
  "project_state": [{ "record_id": "project:v1:<id>", "trust_class": "project_state", "kind": "Task" }],
  "artifacts": [],
  "verification_evidence": [{ "record_id": "verification:v1:<id>", "trust_class": "verification", "kind": "CommandRun", "correlation_basis": "content_hash_join" }],
  "unresolved": [],
  "excluded": [],
  "disclaimer": "Rows are CORRELATION LEADS, never proof of cause: ..."
}
```

- `first_seen_range` is `{"status":"unavailable","diagnostic":"history_unavailable"}`
  on a plain `scan` graph — never a fabricated window.
- `protected_payloads` is present only under `--protected-store`.
- `embedded_log_retention_caveat` is present only on the embedded (`--data-dir`)
  read path when the store holds at least one `ErrorSignature`; omitted for
  `--graph` queries and pure `scan` stores (issue #363, see above).
- Every row carries a `trust_class`; runtime observations live only in
  `signatures`, never in `source_facts` (zero cross-class leakage).
- Output is deterministic and byte-identical across runs: every section is
  sorted by record ID (frames by `(frame_index, frame_resolution, target)`,
  buckets by `(bucket_start, record_id)`), and all timestamp ordering is by
  parsed UTC instant (never raw RFC 3339 string order).

## Exit codes and diagnostics

| Exit | Condition | Envelope |
|---|---|---|
| 0 | Handle resolved to ≥1 signature; bundle emitted (some sections may be empty) | `{ "ok": true, ... }` |
| 1 | Fingerprint prefix ambiguous (≥2 candidates) | `{ "ok": false, "error": { "code": "ambiguous", "candidates": [...] } }` |
| 1 | `--at` and `--as-of` both set | `{ "ok": false, "error": { "code": "unsupported_combination" } }` |
| 1 | `--as-of` is not a valid RFC 3339 instant | `{ "ok": false, "error": { "code": "invalid_as_of_timestamp" } }` |
| 1 | `--protected-store` set and the graph carries a protected handle | `{ "ok": false, "error": { "code": "protected_handle_in_graph" } }` |
| 2 | No signature ID, fingerprint prefix, or symbol frame target matched | `{ "ok": false, "error": { "code": "no_match", "handle": ... } }` on stdout |
| 2 | Load error (missing/empty store, unreadable graph) | anyhow diagnostic |

## When to use which tool

- **`eg query error-context`** — you have one error signature (from a stack
  trace, an alert, or `eg scan-logs`) and want *everything the graph knows* about
  it in one cited envelope.
- [`eg query log-deltas`](./log-deltas.md) — you have a commit range and want to
  know which signatures are new/ceased/continuing across it.
- [`eg resolve-frames`](./resolve-frames.md) / [`eg link-logs`](./link-logs.md) —
  you are *building* the frame-resolution and run-correlation edges this query
  reads.
