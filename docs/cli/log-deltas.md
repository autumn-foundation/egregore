# eg query log-deltas

Classify **runtime error-signatures across a commit range** (issue #326) —
answer the regression question *"between commit A and commit B, did any new
runtime error signatures appear, and which of them touch code that changed in
the range?"* — from a `scan-history` graph (augmented with `scan-logs` records)
or an embedded store. Local-first; no network access, hosted indexing, remote
crawling, or mandatory remote embeddings.

> **A signature first observed in-range is a regression LEAD, not proof this
> range caused it.** A ceased signature is not proof of a fix. Occurrence data
> only reflects the log sources that were scanned — a sampling artifact, never
> the complete runtime behavior of the system.

This query composes three already-shipped mechanics rather than re-deriving any
of them: the issue #118 [`eg query deltas`](./deltas.md) range mechanics
(endpoint resolution, the commit valid-time window, and the symbol-delta join),
the issue #319/#320 `ErrorSignature` valid-time model
([`eg scan-logs`](./scan-logs.md)), and the issue #322 `FRAME_RESOLVES_TO`
frame-resolution edges ([`eg resolve-frames`](./resolve-frames.md)).

## Synopsis

```text
eg query log-deltas <BASE> <HEAD> --graph <PATH>    [--repo <SELECTOR>]
eg query log-deltas <BASE> <HEAD> --data-dir <DIR>  [--repo <SELECTOR>]
```

`<BASE>` and `<HEAD>` are commit handles — full SHAs or unique prefixes —
resolved against the store's `Commit` nodes with the exact issue #118 endpoint
resolution and error taxonomy. `BASE` must be an ancestor of `HEAD`. The query
is purely read-time: it reads only the supplied store, never Git state, so it
cannot mutate the working tree.

### `--repo` scope (code side only)

`--repo <SELECTOR>` scopes the **code side** of the query — commit/endpoint
resolution, the valid-time window, and the symbol-delta join — to one repository
in a shared multi-repo store. It does **not** filter the log signatures
themselves: `scan-logs` records (`ErrorSignature`, `LogOccurrenceBucket`, and
their `AGGREGATES` / `FRAME_RESOLVES_TO` edges) carry no retrievable repository
attribution — the repository identity is only hashed into their stable record
IDs, never stored as a queryable field — so there is no sound way to attribute a
signature to a repository at read time. Every in-window log signature in the
store is therefore classified against the scoped window regardless of `--repo`.
(An earlier revision applied the `--repo` predicate to signature IDs directly;
because `owner_of(<signature-id>)` is always `None`, that dropped **every**
signature and returned empty groups even for the correct repository.) To keep log
domains cleanly separated, keep each repository's logs in its own store.

**A single `Repository`-node count does NOT mean the run is repo-isolated for log
signatures.** Log records add no `Repository` node — their repository identity is
only hashed into their stable IDs — so a store that reports one distinct
`Repository` node can still hold a second repository's log graph (for example
repo-A `scan-history` plus a repo-B `scan-logs` graph), whose in-window signatures
are classified here regardless of `--repo`. The disclosure therefore **never**
guarantees repo-specificity for log signatures at any repository count.

Because a scoped run cannot be guaranteed repo-specific for log signatures, the
response **envelope discloses this in a machine-readable field** rather than only
in the docs. Whenever `--repo` is set, the response carries a `repo_scope_caveat`
object stating that log signatures are **not** repository-filtered and that a
scoped run cannot be guaranteed repo-specific for them (`repo_scope`,
`distinct_repository_count`, `multi_repository_store`, and a fixed `message`).
`distinct_repository_count` and `multi_repository_store` are **informational**
raw counts of `Repository` nodes, never an isolation verdict. When the store
holds more than one distinct `Repository` node the message ADDS a
higher-known-risk note (`multi_repository_store: true`) — multiple repositories
are demonstrably present — but the base disclosure is identical and a single
count is **never** downgraded to "safe". The field is omitted entirely for
unscoped queries and is deterministic (fixed strings, no wall clock). The
schema-level fix — persisting repository attribution on log records so `--repo`
can soundly filter log signatures and the caveat can be dropped — is tracked in
issue #362.

Separately, whenever the query runs over the embedded (`--data-dir`) read path
**and** the store holds at least one `ErrorSignature`, the response carries an
`embedded_log_retention_caveat` object (a fixed `message`) disclosing that
embedded stores retain one record per stable non-temporal log ID
(last-write-wins), so cross-scan coalescing is **not** reconstructable there — see
[`--graph` only](#--graph-only-coalescing-is-not-reconstructable-on---data-dir-issue-363)
above. The field is omitted for `--graph` queries and for embedded stores with no
log records, and is deterministic (fixed string, no wall clock).

## Shortest offline workflow

```sh
# Replay history into a temporal JSONL graph (reads Git objects only)
eg scan-history . --out history.graph.jsonl

# Capture runtime error signatures from a log file
eg scan-logs app.log --repo-path . --out log.graph.jsonl

# Resolve backtrace frames onto code-graph targets (issue #322)
eg resolve-frames log.graph.jsonl --graph history.graph.jsonl --out resolved.graph.jsonl

# Concatenate the code-history and resolved-log records into one graph
cat history.graph.jsonl resolved.graph.jsonl > combined.graph.jsonl

# Classify error-signatures across a commit range
eg query log-deltas 4f0c2b1 9ad3e77 --graph combined.graph.jsonl

# Scope to one repository in a shared store
eg query log-deltas 4f0c2b1 9ad3e77 --graph combined.graph.jsonl --repo acme/widget
```

## Window derivation

The valid-time window the classification runs against is derived from the
committer dates of the commits in the resolved range (the commits reachable
from `HEAD` but not from `BASE`):

- `window_start = min(commit_valid_time[sha])` over the range commits;
- `window_end   = max(commit_valid_time[sha])` over the range commits.

All timestamp comparisons — window derivation, signature classification, and
occurrence-bucket cutoffs — are made on **parsed UTC instants**, never on raw
RFC 3339 string order. Commit committer dates carry local UTC offsets (Git
`%cI`, e.g. `2026-01-01T00:30:00-05:00`), while `scan-logs` normalizes signature
`first_seen`/`last_seen` and bucket starts to UTC `Z`. A lexical string
comparison is wrong across offsets (`"...05:00:00Z"` sorts after
`"...00:30:00-05:00"` even though its instant precedes it), which would drop an
in-window signature as out-of-range; parsing to an instant first avoids this.
The emitted `window_start` / `window_end` fields keep the original RFC 3339
text — only the ordering is by instant. A range commit whose committer date
carries no parseable timestamp cannot bound the window and is dropped from the
derivation; if no range commit carries a parseable valid time the window is
empty and every signature is excluded (never fabricated bounds).

## Signature coalescing across scan-logs outputs

`LogSource` is a **non-identity** input for signatures: a signature's stable
record ID is derived from `(repository_id, fingerprint_algorithm, template,
severity)` only. A graph that combines **multiple `scan-logs` outputs for the
same repo** (e.g. logs captured on different days, each producing its own
`ErrorSignature` for a recurring fingerprint) therefore carries the same
signature record ID more than once, each copy with its own scan-local
`first_seen` / `last_seen` / `occurrence_count`.

`log-deltas` groups these records by stable signature ID and **merges them
before classifying**, emitting exactly **one row per signature ID** — never
split across conflicting classes. The merge is:

- **`first_seen`** = the earliest across the group (by parsed instant);
- **`last_seen`** = the latest across the group (by parsed instant);
- **occurrence buckets** = **every** linked bucket node, **summed** across the
  group with **no dedup by bucket record ID** (see below);
- **aggregate `occurrence_count`** = the sum of the group's per-scan counts.

Both the per-window bucket counts and the aggregate `occurrence_count` **sum**
across every scanned source. A `LogOccurrenceBucket` record ID is
`(repository_id, signature_id, bucket_start, bucket_width)` and **omits
`LogSource`**, so two **distinct** sources observing the same signature in the
same hour mint the **same** bucket record ID with their own per-source counts.
Summing (rather than deduping by bucket ID) preserves both sources and keeps the
window counts consistent with the aggregate. The symmetric cost is that
concatenating the **identical** `scan-logs` output multiplies counts (a
degenerate, user-error input) — so **scan each source once**, or use
per-source / per-repository stores. Fully source-attributed counts require
source-aware bucket identity, a log-graph (#320) schema change out of this
command's scope, tracked in **issue #361**.

Without this coalescing a single stable signature could split — an earlier scan
that observed it before the range landing in `ceased_signatures` while a later
scan that first observed it in-range lands in `new_signatures`. In a store built
from a single `scan-logs` output this is moot (each signature ID appears once).

### `--graph` only: coalescing is not reconstructable on `--data-dir` (issue #363)

Cross-scan coalescing is a **`--graph`** capability. The embedded (`--data-dir`)
current-state read surface returns exactly **one record per stable ID**, and
`ErrorSignature` / `LogOccurrenceBucket` are **non-temporal** nodes, so ingesting
multiple `scan-logs` outputs of the **same** stable signature/bucket ID retains a
single record (**last-write-wins**) — the duplicate records the coalescing needs
are gone before `log-deltas` runs. On the `--data-dir` path, therefore,
`first_seen` / `last_seen` and occurrence counts reflect only the **retained**
record, and the split-signature case above can **misclassify**.

A **single** `scan-logs` ingest is unaffected and correct — this only bites
multi-scan aggregation on the embedded path. When the query runs over
`--data-dir` **and** the store holds at least one `ErrorSignature`, the response
envelope carries an `embedded_log_retention_caveat` object (a fixed `message`)
disclosing this. To aggregate across scans, combine `scan-logs` outputs at the
**`--graph`** level (concatenated JSONL) or use **per-source stores**. The
store/adapter-layer fix — a log-domain-aware embedded read path that retains
duplicate non-temporal log records — is out of this command's scope and tracked
in **issue #363**.

## Change classes

Every in-scope `ErrorSignature`, after coalescing, is classified against the
window from its merged `first_seen` (`fs`) and `last_seen` (`ls`) valid times,
into a **closed, mutually exclusive** set evaluated in this precedence:

| Group | Class label | Condition | Meaning |
|-------|-------------|-----------|---------|
| `new_signatures` | `new_signature` | `window_start <= fs <= window_end` | First observed inside the window — the primary regression signal. A signature that appeared **and** ceased within the window still classifies as new. |
| `ceased_signatures` | `ceased_signature` | not new, `fs < window_start` and `ls < window_end` | Existed before the range and went silent by/within it. A signature last seen before the range (`ls < window_start`) trivially satisfies this. |
| `continuing_signatures` | `continuing_signature` | `fs < window_start` and `ls >= window_end` | Existed before the range and still occurring through its end. |

**After-window exclusion.** A signature whose first observation falls strictly
after the window (`fs > window_end`) is **out of range** and is excluded from
all three classes — it belongs to a future range, not this one. This exclusion
is deliberate and documented so it never silently disappears into "ceased".

### Symbol-delta join (`overlapping_symbol_deltas`)

For each `new_signatures` row only, the query follows the signature's
`FRAME_RESOLVES_TO` edges (issue #322) to their code-graph target record IDs.
Each target that also appears in the reused issue #118 `range_deltas`
`added_symbols` / `modified_symbols` / `removed_symbols` groups is added to that
row's `overlapping_symbol_deltas` as `{record_id, change_class}`. The
intersection is computed entirely from the existing delta mechanics — never
re-derived ad hoc. The list is empty when there is no overlap and is sorted
deterministically by `(change_class, record_id)`. `ceased_signatures` and
`continuing_signatures` rows always carry an empty join.

A frame binding proves only that the frame **names** the symbol; an overlap is
a review lead correlating a new failure with code that changed in the same
range, never proof the change caused the failure.

## Occurrence counts

Per-window occurrence figures are computed from the signature's own hourly
`LogOccurrenceBucket` records (issue #320), discovered through the `AGGREGATES`
(bucket → signature) edges and **summed** across the coalesced group with **no
dedup by bucket record ID** — so distinct sources sharing a bucket ID are
preserved and the window counts stay consistent with the aggregate
`occurrence_count` (see
[Signature coalescing](#signature-coalescing-across-scan-logs-outputs) for the
identical-rescan caveat and issue #361):

- `base_window_occurrences` = sum of linked bucket counts whose `bucket_start`
  is `<= commit_valid_time[BASE]`;
- `head_window_occurrences` = sum of linked bucket counts whose `bucket_start`
  is `<= commit_valid_time[HEAD]`.

These per-window counts are **hour-bucket-granular, not endpoint-exact**. A
`LogOccurrenceBucket` carries only an hour-aligned `bucket_start` and an
aggregate count — **no per-occurrence timestamps** (issue #320) — so a bucket
that straddles the base/head commit instant **cannot be sub-divided** at that
instant. Because a bucket is counted whenever `bucket_start <= endpoint`, when
the endpoint falls **mid-hour** the **whole** hour is counted: a window count
may include occurrences up to one bucket width (**1 hour**) past the exact
commit instant. This is disclosed, never silently absorbed — every response
carries `"occurrence_count_granularity": "hourly_bucket"` and the always-present
`disclaimer` states it. Endpoint-exact counts would require sub-hour
per-occurrence timestamps the bucket model does not retain; the alternative
"fully-before" predicate (`bucket_start + width <= endpoint`) is **not** used
because it would under-count by dropping pre-endpoint occurrences in the same
partial bucket — trading over-count for under-count with no honesty gain.
Endpoint-exact occurrence counts require a #319/#320 log-graph schema change
(sub-hour per-occurrence timestamps) and are tracked in issue #364.

When a signature has at least one linked bucket, `occurrence_source` is
`occurrence_buckets` and both window fields are present. When a signature
carries **no** linked buckets (e.g. a log graph ingested without buckets),
per-window bucketization is unavailable: the two window fields are omitted and
`occurrence_source` is `aggregate_only`, exposing the signature's aggregate
`occurrence_count` as the only honest count. Counts are never fabricated. The
aggregate `occurrence_count` is always present on every row.

## Exit codes and diagnostics

Ambiguous prefixes, unknown commits, identical endpoints, and reversed ranges
fail with the same stable machine-readable diagnostics as `eg query deltas`
(`{"ok":false,"error":{"error_type":...}}` on stdout), never partial or silent
output:

| Condition | `error_type` | Exit |
|-----------|--------------|------|
| Success (including a resolved range with no signatures in any class) | — | `0` |
| Commit prefix matches multiple commits | `ambiguous_commit_prefix` | `1` |
| Both endpoints resolve to the same commit | `identical_endpoints` | `1` |
| Base is a descendant of head | `reversed_range` | `1` |
| No ancestor path connects the endpoints | `no_path` | `1` |
| Commit prefix matches nothing | `missing_commit` | `2` |
| Store has no commit history | `empty_history` | `2` |

An empty result in all three classes is an explicit success (exit `0`), not an
error.

## Response shape

Every group is always present (empty arrays, never omitted) and canonically
ordered by `(first_seen, record_id)`, so repeating the same query yields
byte-equivalent canonical output. No raw log payload text ever appears — only
bounded template excerpts, record IDs, severities, counts, commit handles, and
valid times.

```json
{
  "ok": true,
  "base": "<full base SHA>",
  "head": "<full head SHA>",
  "window": {
    "window_start": "2026-01-02T00:00:00Z",
    "window_end": "2026-01-03T00:00:00Z"
  },
  "range_commit_count": 2,
  "disclaimer": "Rows are runtime error-signature observations ...",
  "occurrence_count_granularity": "hourly_bucket",
  "new_signatures": [
    {
      "record_id": "log:v1:...",
      "schema_version": 1,
      "change_class": "new_signature",
      "severity": "error",
      "first_seen": "2026-01-02T12:00:00Z",
      "last_seen": "2026-01-02T13:00:00Z",
      "occurrence_count": 5,
      "occurrence_source": "occurrence_buckets",
      "base_window_occurrences": 0,
      "head_window_occurrences": 5,
      "resolved_frames": [
        {
          "frame_index": 0,
          "frame_resolution": "resolved",
          "target_record_id": "codegraph:v5:..."
        }
      ],
      "overlapping_symbol_deltas": [
        { "record_id": "codegraph:v5:...", "change_class": "modified_symbol" }
      ]
    }
  ],
  "ceased_signatures": [],
  "continuing_signatures": []
}
```

## When to use which tool

- **`eg query log-deltas`** — the runtime-regression A..B answer: which error
  signatures newly appeared, ceased, or continued across a commit range, with
  the new ones joined to the code deltas of the same range. Its subject is the
  *observed runtime behavior* recorded from scanned logs.
- **`eg query deltas` (issue #118)** — the structural A..B answer: which
  symbols and files were added, removed, or modified. `log-deltas` reuses these
  mechanics for its window and symbol-delta join, but its rows are error
  signatures, not code facts.
- **`eg query public-api-deltas` (issue #157)** — the public-surface
  classification on top of the range mechanics (`removed`, `signature_changed`,
  `visibility_narrowed`, …). Use it when the question is about the exported
  *contract*, not runtime failures.
- **Diffing grep snapshots of logs** (`grep ERROR old.log > a; grep ERROR
  new.log > b; diff a b`) — fast and local, but it compares raw log text line
  by line: it re-counts volatile fields (timestamps, PIDs, UUIDs, pointers) as
  distinct, never fingerprints by `template-v1`, never anchors a signature to a
  commit valid-time window, and emits no stable graph handles that join to code
  deltas, agent memory, or verification evidence.

And once more, because it is the sharp edge: **occurrence data only reflects the
log sources that were scanned** — this query reports observed, sampled runtime
signatures, never the complete runtime behavior of the system, and a signature
first observed in-range is a regression lead, not proof this range caused it.
