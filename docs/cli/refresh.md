# `eg refresh` — Incremental Store Refresh

**Issue:** #98 — _Incrementally refresh an ingested store from working-tree edits_

---

## Overview

`eg refresh` updates an already-ingested embedded store from only the files that
changed since the last scan.  It combines the incremental file-cache scan (BLAKE3
per-file hashing) with the tombstone-aware embedded re-ingest in a single command,
so refresh cost tracks the edit, not the whole repository.

```
eg refresh <repo_path> --data-dir <dir> [--cache <path>] [--format json|text] [--embed]
```

---

## Shortest Workflow

```sh
# 1. First-time build (full scan + ingest).
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore

# 2. Edit source files …

# 3. Incremental refresh — re-extracts only changed/added/removed files.
eg refresh . --data-dir .egregore

# 4. Query the updated store.
eg query symbol my_function --data-dir .egregore
```

After step 3 the store reflects the current working tree.  Repeat steps 2–3 for
every subsequent edit cycle.

---

## How It Differs from `eg scan` + `eg ingest`

| | `eg scan` + `eg ingest` | `eg refresh` |
|---|---|---|
| **Files processed** | Every file in the repository | Only changed/added/removed files |
| **Cost** | O(repository size) | O(diff size) |
| **Use case** | First-time build or full rebuild | Subsequent updates from working-tree edits |
| **Cache required** | No | Built automatically on first refresh; fails with `no_prior_scan` if the embedded store does not yet exist |

---

## How It Relates to the Staleness Signal (#82)

Issue #82 adds a **read-only** freshness check — it tells you whether the store's
stored snapshot matches the current working tree.  `eg refresh` is the **write**
counterpart: it makes a stale store current.

Detect-then-refresh workflow:

```sh
# Detect staleness (issue #82 — when implemented):
eg query symbol my_fn --data-dir .egregore   # returns stale spans if tree changed

# Refresh:
eg refresh . --data-dir .egregore

# Re-query with fresh spans:
eg query symbol my_fn --data-dir .egregore
```

A successful `eg refresh` always reports `freshness_after_refresh: "fresh"` (visible
in `--format json`), confirming the store now matches the working tree.

---

## How It Differs from Runtime-State Repair (#49 / #72)

| | `eg refresh` | `eg repair` |
|---|---|---|
| **What it fixes** | Store is healthy but code graph is outdated | Store runtime state is broken (crashed daemon, stale metadata) |
| **Records touched** | Deterministic code-graph records and tombstones only | Runtime metadata files only; never graph records |
| **When to use** | After editing source files | After a daemon crash or ownership conflict |

Do not use `eg repair` to refresh source facts — it is read-only with respect to the
graph itself.  Do not use `eg refresh` after a crash — it operates on graph records
and will not fix broken runtime state.

---

## Machine-Readable JSON Report (`--format json`)

```json
{
  "rebuilt_files": ["src/foo.rs"],
  "rebuilt_count": 1,
  "reused_files": ["src/bar.rs", "src/baz.rs"],
  "reused_count": 2,
  "tombstoned_files": [],
  "tombstoned_count": 0,
  "ingest_attempted": 15,
  "ingest_succeeded": 15,
  "ingest_failed": 0,
  "embed_status": "not_requested",
  "freshness_after_refresh": "fresh"
}
```

The caller can verify that work was proportional to the diff: `rebuilt_count` equals
the number of changed or added files, and `tombstoned_count` equals the number of
deleted files.

---

## Embedding Behavior (`--embed`)

### Without `--embed` (default)

Structural records (Symbol, File, Module, Import, Repository, Tombstone) are
updated.  The semantic index is **not** updated.  If the store was built with
`eg ingest --embed`, the semantic index may serve embeddings for nodes whose source
has since changed.

`embed_status` in the JSON report: `"not_requested"`

To rebuild the full semantic index after a refresh, re-run:

```sh
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --embed --data-dir .egregore
```

### With `--embed`

Changed file and symbol nodes receive fresh embeddings.  Tombstoned nodes are
removed from the semantic index.

`embed_status` in the JSON report: `"refreshed"`

---

## Precondition Failures (Exit Code 2)

These are expected, documented conditions — not crashes.  Both produce a
machine-readable JSON object on stderr.

### `no_prior_scan`

```json
{"code":"no_prior_scan","message":"..."}
```

The `--data-dir` does not exist.  The store has not been built yet.  Run:

```sh
eg scan . --out graph.jsonl && eg ingest graph.jsonl --adapter embedded --data-dir .egregore
```

### `repository_identity_mismatch`

```json
{"code":"repository_identity_mismatch","cached_id":"...","current_id":"...","message":"..."}
```

The incremental cache (`<data-dir>/codegraph-cache.json`) was built for a different
repository.  This can happen if the `--data-dir` was moved, shared, or reused across
repositories.  Fix:

```sh
rm .egregore/codegraph-cache.json
eg scan . --out graph.jsonl && eg ingest graph.jsonl --adapter embedded --data-dir .egregore
```

---

## Caching and Cache Path

The incremental cache is stored at `<data-dir>/codegraph-cache.json` by default.
Override with `--cache <path>`.

The cache records a BLAKE3 hash and the extracted graph records for every previously
scanned source file.  On each refresh:

- Files whose content hash is unchanged are **reused** — their cached records are
  restamped with the current transaction time but not re-extracted.
- Files whose hash changed are **rebuilt** — re-extracted and their old records
  tombstoned if the stable IDs changed.
- Files that no longer exist are **tombstoned** — a `Tombstone` record is emitted
  so the embedded store can retire the deleted nodes.

The cache is updated atomically at the end of each refresh.

---

## Trust Preservation

`eg refresh` only writes deterministic code-graph records and tombstones.  It never
reads, modifies, or deletes agent-memory, project, artifact, or verification records
that share the same embedded store.

---

## Determinism

Starting from the same prior store state and applying the same working-tree change,
`eg refresh` produces byte-identical `rebuilt_files`, `reused_files`, and
`tombstoned_files` sets across consecutive runs (BLAKE3 hashing and BTreeMap ordering
are both deterministic).

---

## Options Reference

| Option | Default | Description |
|--------|---------|-------------|
| `<repo_path>` | required | Repository path to scan |
| `--data-dir <path>` | `.egregore` | Embedded AletheiaDB data directory |
| `--cache <path>` | `<data-dir>/codegraph-cache.json` | Incremental scan cache file |
| `--format json\|text` | `text` | Report format |
| `--embed` | off | Refresh semantic embeddings for changed nodes |
