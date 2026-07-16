# eg query recency

Rank indexed symbols by least-recent last change (issue #219).

Over a temporal store produced by `eg scan-history`, returns symbols ranked by
**least-recent last change — most dormant first** — the dormancy-triage signal.
Every row is a stable, citable graph handle an agent can pivot on with
`eg query lifeline`, `eg query change-impact`, `eg query locate`, or
`eg query subsystem` — something raw `git log` shell-mining cannot offer.

> **Dormancy is measured against the newest indexed commit, never wall-clock
> "now".** The reference point is the newest indexed commit per repository
> (highest topological rank, SHA-ascending tie-break) — the same anchor
> `eg query churn` uses for its `last_commit`. A symbol last changed *at* the
> anchor commit has dormancy `0`. This keeps the ranking deterministic and
> reproducible across replays: two runs over identical history are byte-for-byte
> identical, regardless of when they run.

## Synopsis

```text
eg query recency --graph <PATH>    [--repo <SELECTOR>] [--limit N] [--format json|text]
eg query recency --data-dir <DIR>  [--repo <SELECTOR>] [--limit N] [--format json|text]
```

```sh
eg scan-history . --out history.graph.jsonl
eg query recency --graph history.graph.jsonl
eg query recency --graph history.graph.jsonl --limit 10 --format text
```

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded` from a `scan-history` graph. Providing both `--graph` and `--data-dir` is an error. |
| `--repo <SELECTOR>` | no | Restrict the ranking to one repository (see [Repository scope](query.md#repository-scope---repo-issue-67)). Unknown or ambiguous selectors exit `1` with the standard machine-readable stderr diagnostic. Each repository is anchored on its own newest indexed commit — no cross-repository bleed. |
| `--limit N` | no | Maximum ranked symbols returned. **Default `50`, maximum `500`.** Values outside `1..=500` are rejected with an `invalid_limit` diagnostic on stderr and exit `1`. |
| `--format` | no | `json` (default) or `text`. |

## What counts as a last change

- A symbol's last change is the **highest-topological-rank commit** at which its
  body differs from its parent snapshot, or at which it was introduced — reusing
  the `eg query lifeline` (#96/#215) change-detection mechanics. Commits where a
  snapshot exists but the body is unchanged are not "changes" and do not reset
  dormancy.
- Every derivation is keyed on the `Symbol` node's stable record ID (ADR-0004
  identity), **never by name**, so same-name symbols never collapse into one row.
- Only live (non-tombstoned) symbols can rank. A symbol deleted from the current
  tree is **gone, not dormant**, and never appears.
- Only symbols carrying commit provenance (history-replay snapshots) can rank.
  A current-tree-only `eg scan` graph carries no `Commit` nodes and no per-commit
  snapshots, so recency is **unavailable** there (see [Exit codes](#exit-codes)).
- This reads committed history only — live working-tree edits and uncommitted
  changes are invisible to the ranking.

## Dormancy

For each ranked symbol:

```text
dormancy_seconds = anchor_commit.valid_time − last_change_commit.valid_time
dormancy_days    = dormancy_seconds / 86_400   (whole days)
```

Both endpoints are parsed as UTC instants (`chrono` RFC 3339), never compared as
raw strings, and the anchor is the owning repository's newest indexed commit.

## Ordering (deterministic)

Rows are sorted by:

1. `dormancy_seconds` descending — most dormant first;
2. the last-change commit's **topological rank** ascending;
3. `repo_relative_path` ascending;
4. `record_id` ascending.

The full ranking is byte-identical across repeated runs on unchanged history.

## Output

`--format json` (default) prints **one JSON envelope on one line**, keeping
the one-JSON-object-per-line contract of [`query.md`](query.md):

```json
{"ok":true,"result":{
  "ranking_basis":"least_recent_last_change",
  "tie_break":"dormancy_seconds_desc,last_change_commit_rank_asc,repo_relative_path_asc,record_id_asc",
  "dormancy_basis":"measured against the newest indexed commit per repository, never wall-clock",
  "limit":50,
  "total_symbol_count":2,
  "returned_symbol_count":2,
  "truncated":false,
  "anchors":[{"repository_id":"codegraph:v4:…","repository":"acme/widget","commit_sha":"be4a01…","valid_time":"2026-01-03T00:00:00Z"}],
  "symbols":[
    {"rank":1,"record_id":"codegraph:v1:…","symbol_name":"dormant::dormant","schema_version":1,"repo_relative_path":"src/dormant.rs","span":{"start_byte":0,"end_byte":29,"start_line":1,"end_line":1},"last_change_commit":"83fa99…","last_change_valid_time":"2026-01-01T00:00:00Z","dormancy_seconds":172800,"dormancy_days":2,"repository_id":"codegraph:v4:…","repository":"acme/widget"}
  ]
}}
```

### Result fields

| Field | Type | Description |
|-------|------|-------------|
| `ranking_basis` | string | Always `"least_recent_last_change"`. |
| `tie_break` | string | The documented stable tie-break chain. |
| `dormancy_basis` | string | States dormancy is measured against the newest indexed commit per repository, never wall-clock. |
| `limit` | number | The limit applied to the ranking. |
| `total_symbol_count` | number | Ranked symbols before truncation. |
| `returned_symbol_count` | number | Symbols returned after truncation. |
| `truncated` | boolean | Completeness signal: whether `--limit` cut the ranking. Never silent. |
| `anchors` | array | The newest-indexed-commit anchor(s) dormancy was measured against — one per repository scope, sorted by repository record ID, each with `commit_sha` and `valid_time`. |
| `symbols[]` | array | Ranked rows, most dormant first. |

### Row fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `rank` | number | yes | 1-based rank after the documented ordering. |
| `record_id` | string | yes | Stable record ID of the `Symbol` node (ADR-0004 identity). |
| `symbol_name` | string | yes | The symbol's name at its last-change commit. |
| `schema_version` | number | yes | Record schema version of the cited `Symbol` node. |
| `repo_relative_path` | string | yes | Repository-relative file handle at the last-change commit. |
| `span` | object | when recorded | Syntax source span at the last-change commit. Absent for symbols with no recorded span. |
| `absent_span_reason` | string | when span absent | The documented reason the span is absent (e.g. `no_span_module_level`), rather than silently omitting it. |
| `last_change_commit` | string | yes | The commit SHA at which the symbol last changed. |
| `last_change_valid_time` | string | yes | The valid time of the last-change commit. |
| `dormancy_seconds` | number | yes | Anchor valid time − last-change valid time, in whole seconds. |
| `dormancy_days` | number | yes | `dormancy_seconds / 86_400` (whole days). |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the row. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

In an unscoped multi-repository store, rows from different repositories are
returned side by side — never merged — and each carries its own repository
identity and anchor (consistent with the unscoped list-query contract in
[`query.md`](query.md)).

`--format text` prints a human-readable ranking (`1. src/dormant.rs:1-1
dormant::dormant dormant=2d last_change=83fa99… (codegraph:v1:…)`) plus the
anchor header and, when applicable, an explicit `truncated: showing K of M
symbols` line. The text format is not stable and must not be parsed by scripts.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Ranking returned. |
| `1` | Load error, invalid `--limit` (`invalid_limit` on stderr), or unknown/ambiguous `--repo` selector. |
| `2` | Nothing to rank: `{"ok":false,"error":{"code":"no_history",…}}` when the store holds no `Commit` nodes in scope — **the honesty case: a current-tree-only `eg scan` graph reports recency UNAVAILABLE rather than implying every symbol is brand-new (AC5)** — or `{"code":"no_match",…}` when commits exist but no live symbol carries an attributable last change. |

## Compared to the boring alternative

`git log -1 --format=%cI -- <file>` yields a file's last-touched time cheaply,
but produces plain text with no stable graph handles, no symbol-level
granularity, no repository-identity scoping, no tombstone awareness, and no path
from a dormant symbol to its lifeline, agent memory, or verification evidence.
`eg query recency` returns deterministic `Symbol` record IDs that join to every
other Egregore domain in the same store, and its dormancy is reproducible
(anchored on indexed history, not the wall clock).

## Out of scope (per issue #219)

- File-level recency (symbol-level granularity in this slice; use `eg query
  churn` for file change frequency).
- Author/ownership recency (`eg query ownership`, #116/#245).
- Complexity or risk weighting (dormancy only).
- Semantic/embedding drift ranking (`eg query drift`, #55).
- Uncommitted working-tree changes.
