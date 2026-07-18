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

### `--repo` scope (code side **and** log signatures, schema v3)

`--repo <SELECTOR>` scopes the **code side** of the query — commit/endpoint
resolution, the valid-time window, and the symbol-delta join — to one repository
in a shared multi-repo store. Since **schema v3** (issue #362) it **also filters
the log signatures**: every `scan-logs` record now persists a retrievable
`repository_id` field (byte-identical to the `Repository` record ID the
selector resolves to), so `RepositoryIndex::owner_of(<signature-id>)` resolves
and a signature attributed to a **different** repository is soundly **excluded**
from a scoped run rather than bled into it. This closes the cross-repository
false-regression lead that the earlier disclosure could only warn about: an
`ErrorSignature` belonging to repository B whose `first_seen` lands inside
repository A's derived window is no longer reported under `--repo A`.

**Legacy `log:v2:` records — conservative exclusion.** A pre-v3 log record
carries **no** persisted `repository_id` (it deserializes to the empty string
via `#[serde(default)]`), so it cannot be *proven* to belong to the scoped
repository. Rather than bleed it in (a possible cross-repository false lead), a
scoped run **excludes** it — a conservative choice that may **under**-report for
those legacy records until they are re-scanned, never a cross-repository bleed.
Re-run `eg scan-logs` to regenerate every record under schema v3 with
retrievable attribution.

**Residual caveat (shrunk).** The former "logs are never repository-filtered"
`repo_scope_caveat` is **gone**. A `repo_scope_caveat` object is now emitted
**only** when `--repo` is set **and** at least one legacy unattributed signature
was actually excluded, carrying `repo_scope`, an
`excluded_unattributed_signature_count` (always `> 0` when present), and a fixed
`message` disclosing the conservative exclusion and the re-scan remedy. A fully
schema-v3 scoped store — every log signature attributed — carries **no** caveat,
because the filtering is sound. The field is omitted for unscoped queries and is
deterministic (fixed strings + a determined count, no wall clock). The former
`distinct_repository_count` / `multi_repository_store` disclosure fields are
removed; they were `Repository`-node counts, never an isolation verdict, and the
attribution field makes them unnecessary.

Separately, whenever the query runs over the embedded (`--data-dir`) read path
**and** the store holds at least one `ErrorSignature`, the response carries an
`embedded_log_retention_caveat` object (a fixed `message`) disclosing that
embedded stores now retain **every distinct scan observation** (enrichment-only
rewrites from `resolve-frames` / `link-logs` are not double-counted), so
cross-scan coalescing **is** reconstructed there for differing-content scans, and
that the one residual divergence is that byte-identical re-ingests are deduped
(not multiplied) — see
[embedded coalescing](#embedded---data-dir-coalescing-issue-363) below. The field
is omitted for `--graph` queries and for embedded stores with no log records, and
is deterministic (fixed string, no wall clock).

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
- **occurrence buckets** = every **distinct** linked bucket node, **deduped by
  bucket record ID** then summed across the group (see below);
- **aggregate `occurrence_count`** = the sum of the group's per-scan counts.

Since **issue #361** a `LogOccurrenceBucket` record ID is **source-aware**:
`(repository_id, signature_id, bucket_start, bucket_width, source_id)`. Two
**distinct** sources observing the same signature in the same hour therefore mint
**distinct** bucket record IDs whose per-source counts each sum in, while a
genuine **rescan** of identical bytes mints the **same** bucket ID and is
**deduped** (collapsed) — so concatenating the identical `scan-logs` output no
longer double-counts buckets. Per-window bucket counts stay consistent with the
aggregate `occurrence_count`, which sums the coalesced signatures.

Without this coalescing a single stable signature could split — an earlier scan
that observed it before the range landing in `ceased_signatures` while a later
scan that first observed it in-range lands in `new_signatures`. In a store built
from a single `scan-logs` output this is moot (each signature ID appears once).

### Embedded (`--data-dir`) coalescing (issue #363)

Cross-scan coalescing works on **both** read paths. `ErrorSignature` /
`LogOccurrenceBucket` are **non-temporal** nodes, so ingesting multiple
`scan-logs` outputs of the **same** stable signature/bucket ID with differing
captured content appends a **superseded** physical version per scan. The embedded
(`--data-dir`) lane loads records through the **log-retained** read surface
(`read_all_records_log_retained`), which surfaces every one of those versions —
so the same duplicate slice the coalescer needs is present, and `first_seen` /
`last_seen` / occurrence counts are reconstructed **exactly** as on the
concatenated `--graph` JSONL. The split-signature case above therefore classifies
identically on `--data-dir` and `--graph`.

Only **distinct scan observations** are retained, where "distinct" is decided on
the **full scan payload** (window, occurrence count, and captured `frames`) —
not just `first_seen` / `last_seen` / `occurrence_count` — so two observations
that share an identical occurrence window but differ in their scan-time captured
frames (#322) are both kept. The standard pipeline
`scan-logs -> resolve-frames -> link-logs` re-emits the same `ErrorSignature`
node enriched with node-level evidence links (`FRAME_RESOLVES_TO` /
`EMITTED_DURING` / `REFERENCES_TASK`) while leaving its log payload unchanged.
That enrichment-only rewrite is the **same** observation (identical payload, only
`evidence_links` added), not a new scan, so it is retained as a **single**
observation and never double-counts occurrences. The retained current version is
always the enriched one, so resolved frames and evidence links are preserved.

Since **issue #361** made `LogOccurrenceBucket` identity source-aware, the former
same-hour/same-count bucket divergence is **gone**: distinct sources mint distinct
bucket IDs (summed on both paths) and a genuine rescan mints the same bucket ID
(deduped on both paths — `--graph` dedups by record ID before summing,
`--data-dir` dedups at write time), so **per-window occurrence counts converge**.

**One residual divergence remains**, rooted purely in the idempotent-write dedup
of byte-identical non-temporal records — **not** a bucket-identity gap: a
**byte-identical** re-ingest of the *entire* same `scan-logs` output (the same
signature record appended twice) is an idempotent no-op on `--data-dir` (deduped to
one physical record) but is **summed** on `--graph` (which groups duplicate
signatures by stable ID and sums their aggregate `occurrence_count`). So
concatenating identical JSONL inflates the `--graph` aggregate count while an
identical re-scan does not inflate `--data-dir`.

A **single** `scan-logs` ingest is exact either way. When the query runs over
`--data-dir` **and** the store holds at least one `ErrorSignature`, the response
envelope carries an `embedded_log_retention_caveat` object (a fixed `message`)
disclosing this residual divergence. The adapter-level retention fix landed in
**issue #363**.

The retention preserves the **`forget` retraction boundary**: if an
`ErrorSignature` was retracted with `eg forget` and a **later** `scan-logs`
re-observes the same stable ID, the log-retained read surfaces only the
post-retraction observation — the pre-retraction observation is **not**
resurrected into the coalesced sum (a re-scan after `forget` never revives a
forgotten observation).

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

Since **schema v3** (issue #364) each `LogOccurrenceBucket` carries an
`occurrence_timestamps` list — the sorted RFC 3339 UTC per-occurrence instants
that fell in its hour, preserved at **full sub-second precision** (fixed-width
nanoseconds, e.g. `…T12:30:00.900000000Z`) so the endpoint-exact claim holds even
against a whole-second commit endpoint (issue #364, Codex P2) — so per-window
counts are now **endpoint-exact**:

- `base_window_occurrences` = for each linked v3 bucket, the count of its
  `occurrence_timestamps` at or before `commit_valid_time[BASE]` (by parsed UTC
  instant);
- `head_window_occurrences` = the same at or before `commit_valid_time[HEAD]`.

A bucket that straddles the base/head commit instant is **sub-divided at the
instant**: only the occurrences at or before the endpoint are counted, even when
the endpoint falls **mid-hour**. The over-count the earlier hour-bucket rule
disclosed is gone for v3 records.

**Legacy `log:v2:` fallback (per-bucket, honest degradation).** A pre-v3 bucket
has an **empty** `occurrence_timestamps` and cannot be sub-divided, so it falls
back to the hour-bucket-granular rule for **that** bucket: it is counted whole
whenever `bucket_start <= endpoint`, and a mid-hour endpoint may include
occurrences up to one bucket width (**1 hour**) past the exact instant. The
"fully-before" predicate (`bucket_start + width <= endpoint`) is **not** used —
it would under-count by dropping pre-endpoint occurrences in the same partial
bucket. Re-scan to regenerate buckets under v3 for endpoint-exactness.

**The `occurrence_count_granularity` marker is now per-response and
conditional** (issue #364):

- `"endpoint_exact"` when **every** bucket contributing to a per-window count in
  the response carried per-occurrence timestamps (or no bucket contributed at
  all — vacuously exact). The `disclaimer` uses the endpoint-exact wording.
- `"hourly_bucket"` when **at least one** contributing bucket was a legacy
  `log:v2:` record with no timestamps, so the response degraded to the whole-hour
  rule for that bucket. The `disclaimer` retains the legacy hour-bucket-granular
  wording (a count may include occurrences up to one bucket width past the exact
  endpoint). This is disclosed, never silently absorbed.

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
  "occurrence_count_granularity": "endpoint_exact",
  "new_signatures": [
    {
      "record_id": "log:v3:...",
      "schema_version": 3,
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
