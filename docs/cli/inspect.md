# `eg inspect` — Store and Graph Composition Summaries

**Issue:** #125 — _Inspect an embedded store's contents without starting the daemon_

---

## Overview

`eg inspect` answers one question: **what landed in this graph or store, and is
source truth visibly separated from agent-authored memory?** It reports totals
(records, nodes, edges, tombstones, diagnostics, repository summaries) plus
per-`domain`, per-`kind`, and per-`schema_version` counts grouped into the
vision's trust classes.

```
eg inspect <graph.jsonl>            [--format text|json]   # pre-ingest JSONL file
eg inspect --data-dir <dir>         [--format json|text]   # embedded store, no daemon (issue #125)
eg inspect --daemon --data-dir <dir> [--format text|json]  # running daemon (issue #47)
```

The three modes are mutually exclusive: a `graph.jsonl` path conflicts with
`--data-dir`, and `--daemon` requires `--data-dir`.

## The daemon-free embedded workflow (issue #125)

The README's headline local-first workflow is embedded and daemon-free. The
shortest end-to-end path is:

```
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
eg inspect --data-dir .egregore
```

`eg inspect --data-dir` reads the embedded AletheiaDB store **directly** — no
running daemon, no network, no embeddings required — using the same embedded
read path the `eg query ... --data-dir` surface already uses. It is the
daemon-free analog of `eg inspect --daemon` (#47): the same counts, without
standing up infrastructure the embedded path was designed to avoid.

### Read-only guarantee

Inspection is strictly read-only. The embedded engine re-persists its index
files on open, so the command copies the store to a throwaway temporary
directory and reads the copy — zero graph records, indexes, idempotency
receipts, or runtime files are created, modified, or deleted in the inspected
store. Re-running the command on an unchanged store produces byte-identical
output.

### Trust classes

Domains are grouped so deterministic source facts stay distinguishable from
subjective memory:

| Domain         | Trust class                 |
| -------------- | --------------------------- |
| `codegraph`    | Deterministic Source Facts  |
| `semantic`     | Derived Measurements        |
| `agent_memory` | Agent-Authored Claims       |
| `project`      | Project/Work State          |
| `artifact`     | Artifacts                   |
| `verification` | Verification Evidence       |
| `user_context` | User Context                |

### Unknown schema versions

A record whose `(domain, kind, schema_version)` tuple is not known to this
binary is counted and labeled distinctly under `unknown_schema_versions` —
never silently folded into the known-version counts. This matches the
mixed-store behavior of JSONL and daemon inspection.

### Redaction

Output never includes raw transcript text, command output, patch hunks,
issue/comment bodies, summaries, environment values, or tokens — only counts,
domains, kinds, schema versions, and repository handles.

## JSON contract (`--data-dir`, agent/SDK-facing)

For `eg inspect --data-dir`, output defaults to newline-delimited JSON: a
single-line JSON object terminated by a newline. `--format text` produces the
same human-readable layout as JSONL inspection. The object is deterministic
(keys serialize in stable order, repository summaries sort by record ID, and
there is deliberately no timestamp), so it is byte-identical across runs on an
unchanged store.

```json
{
  "source": { "mode": "embedded", "data_dir": "<path as given>" },
  "records": 1204,
  "nodes": 1204,
  "edges": 0,
  "tombstones": 0,
  "diagnostics": 0,
  "domain_counts": {
    "Deterministic Source Facts": { "File v5": 401, "Repository v5": 1 },
    "Agent-Authored Claims": { "Observation v1": 401 },
    "Project/Work State": { "Task v1": 401 }
  },
  "schema_versions": { "codegraph:File:5": 401, "codegraph:Repository:5": 1 },
  "unknown_schema_versions": { "codegraph:Repository:6": 1 },
  "repositories": [ { "id": "codegraph:v6:...", "identity_summary": "remote: ..." } ],
  "coverage": [
    {
      "id": "codegraph:v6:...",
      "files_walked": 61,
      "files_indexed": 41,
      "skipped_by_extension": { "": 2, "md": 12, "toml": 6 },
      "indexed_languages": ["Rust", "Python", "TypeScript", "Go"],
      "coverage_complete": true
    }
  ],
  "producer_kinds": { "legacy_pre_v1": 1204 },
  "egregore_versions": { "legacy_pre_v1": 1204 }
}
```

Field notes:

* `source` — embedded-mode descriptor; `data_dir` echoes the operator-supplied path.
* `records` — every physical record seen, including unknown-schema-version records.
* `nodes` / `edges` / `tombstones` / `diagnostics` — totals over known-version records.
* `domain_counts` — per-trust-class `"<kind> v<version>": count` breakdown.
* `schema_versions` / `unknown_schema_versions` — `"<domain>:<kind>:<version>": count`.
* `repositories` — stable record ID plus a redaction-safe identity summary, sorted by ID.
* `coverage` — one entry per `ScanCoverage` node (issue #135), sorted by record
  ID: file-level indexing coverage for the repository the scan visited. Lets an
  agent tell "0 results because absent" from "0 results because that file was
  never indexed". `files_indexed` counts every walked file that received a graph
  `File` node — source symbols **or** manifest/dependency facts (a dependency-
  declaring `Cargo.toml` is indexed, not skipped); `skipped_by_extension` counts
  only walked files with no `File` node. See the field-level shape and the
  excluded-directory contract in `docs/cli/scan.md`. `coverage_complete`
  is `true` only for the Git-tracked-files walk; the non-Git fallback reports
  best-effort counts with `coverage_complete: false`. The text layout prints one
  `coverage: <walked> files walked, <indexed> indexed, <skipped> skipped
  (complete: <bool>)` line plus one indented line per skipped extension and the
  indexed-language scope.
* `producer_kinds` / `egregore_versions` — provenance breakdown; records that
  predate producer stamping count under `legacy_pre_v1`.

* `semantic_index` — **`--data-dir` only** (issue #104), the store's vector-index
  identity: `index_present`, `index_dimensions`, `identity_recorded`, and
  `indexed_models` (an array of `{provider, name, version, dim, content_hash}`
  identities, deduplicated and sorted). This is the documented `eg` workflow for
  reading which embedding model produced a store's queryable index, and therefore
  for understanding why `eg query semantic` refused. `identity_recorded: false`
  with `index_present: true` is a legacy store whose compatibility is
  unverifiable; `indexed_models` with more than one entry means the index was
  written by several models. A `--graph` JSONL carries no vector index, so the
  block is absent there, as it is on a binary built without the `embeddings`
  feature (the vector index is that feature's surface). Allow-list only — identity
  fields, dimensions, and flags; never vectors or model bytes, and every identity
  value is control-character-sanitized and length-capped before rendering, since
  values read back from a store are operator-controlled. See
  [`docs/cli/semantic-index-identity.md`](semantic-index-identity.md).

JSONL-file and daemon inspection keep their existing pretty-printed JSON
envelope, which additionally carries a `snapshot_timestamp`.

### ScanCoverage freshness on exported JSONL (issue #406)

When reading an **exported** JSONL (`eg export`) that contains multiple physical
`ScanCoverage` versions for the same repository, `eg inspect --graph` resolves
the current version by `valid_time` at RFC 3339 **instant** precision, THEN — on
a tie — by the record's full-precision `coverage_generation` instant. The
full-scan path stamps `valid_time` at **seconds** precision, so two full scans of
the same repository within the **same UTC second** share a `valid_time`; the
`coverage_generation` field (a nanosecond RFC 3339 instant captured at scan time,
non-identity, additive) breaks that tie so the **newer** version wins
deterministically, regardless of physical line order (immune to `eg export`'s
lexicographic `lines.sort_unstable()`).

Residual ties fall through to the byte-deterministic content tiebreak: two scans
within the **same nanosecond**, or a legacy `ScanCoverage` record produced before
`coverage_generation` existed (only seconds `valid_time`, field absent). For
those cases — and any sub-second re-scan on the store side — use
`eg inspect --data-dir`, which is **authoritative**: it orders by store physical
write-order and is immune to same-second ties.

Count semantics after a retraction (`eg forget`, issue #231): daemon-free
`--data-dir` inspection is a physical inventory, so its counts include a
retracted record's physical versions (inspect never shows content). Daemon
inspection (`--daemon`) counts the daemon's transaction-time-current serving
view (`GET /v1/records`), which excludes actively retracted records while
still counting their tombstones and retraction events. That serving view is
also version-collapsed — one (latest) version per stable ID — so daemon
counts exclude superseded prior versions and stale tombstones that the
physical inventory still counts.

## Errors

A missing store, wrong `--data-dir`, or unreadable store fails with a non-zero
exit and an operator-facing diagnostic that names the path — it is never
reported as a valid-but-empty store. This includes a directory that is
non-empty on disk (engine index/runtime files exist) but holds zero Egregore
records, e.g. after ingesting an empty JSONL or pointing `--data-dir` at a
non-Egregore AletheiaDB directory:

```
error: embedded store not found at <path> - run `eg ingest --adapter embedded --data-dir <path>` first
error: embedded store at <path> is empty - run `eg ingest --adapter embedded --data-dir <path>` first
error: embedded store at <path> contains no Egregore records - run `eg ingest --adapter embedded --data-dir <path>` first
```

A corrupt record with a *known* schema version fails the whole inspection (it
indicates store damage, not version skew).

## Out of scope

Graph traversal questions belong to the `eg query` verbs; daemon-transport
inspection belongs to #47; store repair/compaction belongs to #72/#49;
store-vs-working-tree staleness belongs to `eg freshness` (#82).
