# eg query producer-drift

Audit stored **producer identity** against the currently running `egregore`
binary (issue #234). Every persisted record carries a `producer` envelope
(`docs/schema/producer-version.md`) naming the binary version and the grammar
component versions that wrote it. A grammar or extractor bump can change which
spans and edges are emitted for the same source without any source change —
this verb reports exactly which records that has already happened to.

> **Read-only reproducibility report, never a truth claim.** Drift means
> re-extraction with this binary *could* produce different records; it is
> never proof the recorded facts are wrong. Nothing is re-extracted,
> re-embedded, or mutated (re-scan is `eg refresh`, issue #167), and no trust
> policy is enforced — whether a producer revision is trustworthy stays an
> operator decision.

The realistic trigger: `cargo install --force egregore` upgrades
`tree-sitter-rust`, then only changed files are re-extracted. Untouched files
keep old-grammar spans, the store silently mixes producers, and nothing flags
it — `git status` answers "did the *source* change?" but is blind to "did the
*extractor* change?". This verb makes the mixed-producer state auditable.

## Synopsis

```text
eg query producer-drift --graph <PATH>    [--repo <SELECTOR>] [--format json|text]
eg query producer-drift --data-dir <DIR>  [--repo <SELECTOR>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` scopes a shared multi-repo store to one
repository (nodes attribute by ID, edges by source node, tombstones by
deleted ID; records unattributable to any repository are excluded from a
scoped run). An unknown or ambiguous selector is rejected with a
machine-readable stderr diagnostic (exit 1), never resolved implicitly.

| Condition | Exit | Output |
|-----------|------|--------|
| Report computed — **including zero drift** | `0` | Drift JSON on stdout, `ok:true` |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing store input | `1` | Error message on stderr |

Drift presence never changes the exit code: this slice reports drift, it does
not gate on it.

## Classification

Records group by the distinct producer signature
`(producer_kind, egregore_version, producer_components)` and each group lands
in exactly one bucket:

- **`drifted`** — a code-graph-extraction producer (`code_graph_extractor`,
  `history_replay`, `incremental_cache`) whose `egregore_version` and/or any
  recorded `producer_components` entry (e.g. `tree_sitter_rust`) differs from
  the running binary. Per-field mismatches are listed, plus every affected
  record ID with its repo-relative file/span handle where available.
- **`current`** — a code-graph-extraction producer matching the running
  binary exactly. Counted, not listed.
- **`non_code_producer`** — agent-memory, importer, drift-engine, and other
  producers. Their output is not a function of grammar versions, so they are
  **never** compared against grammar/binary identity and never labeled stale.
- **`legacy_pre_v1`** — records persisted before the producer envelope
  existed. Reported in their own bucket, never merged into the current or
  drifted buckets (`producer-version.md` §6); re-extraction backfill is
  forever out of scope.

Comparison details:

- A recorded component matches when the running binary knows the key and the
  versions are equal; a key unknown to this binary is a mismatch with
  `current: null`. A record whose components are a *subset* of the binary's
  set (e.g. a Rust-only graph without the Python grammar key) matches — only
  recorded keys are compared.
- `producer_started_at` (wall clock) and `egregore_git` (build provenance
  subsumed by the version string) are **not** compared. This holds the
  zero-false-positive bar: a store written entirely by one binary version
  always yields an empty drift result with an explicit `no_drift` diagnostic.
- The `incremental_cache` envelope is trustworthy because the incremental
  cache invalidates on producer-signature change: a scan by a binary whose
  `egregore_version` or any producer-component version differs from the
  binary that wrote the cache rebuilds every file instead of reusing cached
  records. Records stamped `incremental_cache` were therefore extracted by
  the binary named in their envelope, never laundered from an older grammar.

## Output shape

Deterministic and byte-identical across repeated runs: groups sort by bucket
(`drifted`, `current`, `non_code_producer`, `legacy_pre_v1`), then producer
kind, version, and component set; affected records sort by record ID. All
values derive from record content and compile-time constants — never
wall-clock time or map iteration order. Redaction-safe: record IDs, versions,
paths, spans, and counts only, never payload.

```json
{
  "ok": true,
  "disclaimer": "Read-only audit of stored producer identity against the running binary. ...",
  "current_producer": {
    "egregore_version": "0.1.0",
    "producer_components": {
      "cache_format_version": "6",
      "tree_sitter": "0.26.8",
      "tree_sitter_go": "0.25.0",
      "tree_sitter_python": "0.25.0",
      "tree_sitter_rust": "0.24.2",
      "tree_sitter_typescript": "0.23.2"
    }
  },
  "groups": [
    {
      "bucket": "drifted",
      "producer_kind": "code_graph_extractor",
      "egregore_version": "0.0.9",
      "producer_components": { "tree_sitter": "0.26.8", "tree_sitter_rust": "0.23.0" },
      "mismatches": [
        { "field": "egregore_version", "recorded": "0.0.9", "current": "0.1.0" },
        { "field": "tree_sitter_rust", "recorded": "0.23.0", "current": "0.24.2" }
      ],
      "record_count": 2,
      "records": [
        {
          "record_id": "codegraph:v4:...",
          "record_type": "node",
          "repo_relative_path": "src/old.rs",
          "span": { "start_byte": 0, "end_byte": 10, "start_line": 1, "end_line": 1 }
        }
      ]
    },
    {
      "bucket": "current",
      "producer_kind": "code_graph_extractor",
      "egregore_version": "0.1.0",
      "producer_components": { "tree_sitter": "0.26.8", "tree_sitter_rust": "0.24.2" },
      "record_count": 150
    },
    { "bucket": "non_code_producer", "producer_kind": "observation_writer", "egregore_version": "0.1.0", "producer_components": { "writer_schema": "1" }, "record_count": 3 },
    { "bucket": "legacy_pre_v1", "producer_kind": "legacy_pre_v1", "record_count": 1 }
  ],
  "counts": { "total": 156, "drifted": 2, "current": 150, "non_code_producer": 3, "legacy_pre_v1": 1 },
  "diagnostics": []
}
```

- `repo_scope` appears at the top level when `--repo` was given.
- `mismatches` and `records` appear **only** on `drifted` groups — non-code
  and legacy buckets are never compared, so no mismatch list may appear there.
- Diagnostics: `no_drift` (every code-graph record matches — an explicit
  answer, not an error) and `empty_store` (no records in scope).
- `--format text` renders the same report as deterministic one-line-per-fact
  text for terminal use; the exact text format is not stable for scripts.

## Shortest offline workflow

```sh
# A store built by an older binary + a partial re-scan with a newer one
eg scan . --out graph.jsonl

# Which records would change if I re-extracted with this binary?
eg query producer-drift --graph graph.jsonl

# Same audit over an embedded store, scoped to one repository
eg query producer-drift --data-dir .egregore --repo acme/widget
```

## Relationship to neighboring surfaces

- **`eg inspect`** — prints the flat per-producer-kind / per-version
  histogram; it shows *that* producers mix but not *which* signature differs
  from the installed binary or which records carry it.
- **`eg refresh` (issue #167)** — the write-side answer: re-extract. This verb
  is the read-side audit that tells you whether you need to.
- **SCIP/LSIF `toolInfo`, CodeQL database headers** — record producer
  identity as a static header; this verb turns the per-record envelope into a
  drift query against the currently installed tool.
- **`docs/schema/producer-version.md`** — the envelope contract this verb
  consumes, including the §9 success metric it answers.
