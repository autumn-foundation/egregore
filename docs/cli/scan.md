# `eg scan` — Extract a Code Graph, with File-Level Coverage

**Issues:** graph extraction (MVP); #135 — _scan coverage as a stated,
deterministic, queryable fact_

---

## Overview

`eg scan <repo> --out <graph.jsonl>` walks a repository and emits deterministic
code-graph records (`Repository`, `File`, `Module`, `Symbol`, `Import`, …) for
every **indexed source file**. The indexed languages are **Rust, Python,
TypeScript, and Go** (`.rs`, `.py`, `.ts`/`.tsx`, `.go`). Every other file is
walked but not indexed.

```
eg scan . --out graph.jsonl
```

## Scan coverage (issue #135)

Before #135, files that were not indexed were silently dropped and nothing said
how much of the repository was actually covered. `eg scan` now makes coverage a
**stated, deterministic, machine-readable fact**:

* A single `ScanCoverage` node rides in the emitted JSONL, attached to its
  `Repository` by a `CONTAINS` edge (so it is citable and never an orphan, and
  `eg validate` accepts it).
* When the repository is a Git working tree, `eg scan` also prints a
  human-readable summary to **stderr**:

```
scan coverage: 61 files walked, 41 indexed, 20 skipped
  skipped by extension: md: 12, toml: 6, (no-ext): 2
indexed languages: Rust, Python, TypeScript, Go
```

### The `ScanCoverage` node shape

```json
{
  "record_type": "node",
  "kind": "ScanCoverage",
  "id": "codegraph:v6:<hash>",
  "schema_version": 6,
  "scan_coverage": {
    "files_walked": 61,
    "files_indexed": 41,
    "skipped_by_extension": { "": 2, "md": 12, "toml": 6 },
    "indexed_languages": ["Rust", "Python", "TypeScript", "Go"],
    "coverage_complete": true
  }
}
```

Field notes:

* `files_walked` — files the walk visited (see the excluded-directory contract
  below; excluded files are never counted).
* `files_indexed` — every walked file that received a graph `File` node, whether
  from source-symbol extraction (`.rs`/`.py`/`.ts`/`.go`) **or** from a
  non-source File producer such as manifest/dependency extraction (issue #180): a
  tracked `Cargo.toml` that declares dependencies gets a `File` node plus
  queryable `DependencyDeclaration` facts (`eg query manifest-deps`), so it is
  counted here — never mislabeled as skipped when the graph in fact holds a node
  for it (issue #135). Finalized after all File-producing extractors run.
* `skipped_by_extension` — per-extension count of walked files that received
  **no** graph `File` node, keyed on the **lowercased final extension** (`""` for
  a file with no extension). A `Cargo.toml` with no declared dependencies mints
  no `File` node and so is honestly counted here under `toml`. A sorted map for
  byte-stable output.
* `indexed_languages` — the human-facing names of the parsed **source** languages
  `eg scan` indexes, derived from the extractor's real capability so the named
  scope can never drift from what is actually parsed (#135 AC6). The issue text
  said "Rust only"; that was stale — the extractor indexes
  Rust/Python/TypeScript/Go. This names the source-language scope specifically; a
  manifest counted under `files_indexed` is not a parsed "language" and adds no
  entry here.
* `coverage_complete` — `true` on the Git-tracked-files walk, which yields a
  complete walked/skipped accounting. `false` on the non-Git filesystem-walk
  fallback, which enumerates only matching source files and therefore has no
  walked/skipped denominator; there `files_walked == files_indexed`,
  `skipped_by_extension` is empty, and the stderr summary is **suppressed** rather
  than print a misleading partial tally. A denominator is never fabricated.

### Complete accounting

Whenever `coverage_complete` is `true`,

```
files_indexed + sum(skipped_by_extension.values()) == files_walked
```

Zero files are silently unaccounted (#135 AC4).

### Excluded-directory contract (#135 AC7)

Directories that are structurally not part of the indexed repository are
**excluded from the walk entirely** — they are never counted as walked and never
appear in `skipped_by_extension`, so coverage percentages are not diluted:

* `.git`
* `target`
* nested Git worktrees / submodules / vendored clones (a directory containing its
  own `.git`)

A file tracked under `target/` is excluded exactly like an untracked one: it
never enters `files_walked` or the skip tally, and never becomes a graph node.

### Determinism

The `ScanCoverage` node (with a fixed transaction time) and the stderr summary
are byte-identical across repeated scans of an unchanged repository. The
per-extension skip groups use a sorted map, so their order is stable (#135 AC5).

The auto-stamped scan `valid_time`/`transaction_time` is at **seconds**
precision. When an exported JSONL (`eg export`) holds multiple `ScanCoverage`
versions for the same repository, `eg inspect --graph` picks the current one by
`valid_time` instant, then — on a same-UTC-second tie — by the record's
full-precision `coverage_generation` instant (a nanosecond RFC 3339 timestamp
captured at scan time; non-identity, additive per schema-versioning §2), so the
**newer** of two same-second scans wins deterministically regardless of physical
line order (issue #406). The residual tie — two scans within the same
**nanosecond**, or a legacy record produced before `coverage_generation` existed
— falls through to a content tiebreak; `eg inspect --data-dir` (authoritative,
ordered by store write-order) resolves those.

## Reading coverage back

* `eg inspect <graph.jsonl>` and `eg inspect --data-dir <store>` both surface a
  `coverage` block — see `docs/cli/inspect.md`. This lets an agent tell "0
  results because absent" from "0 results because that language was never
  indexed" (#135 AC3).
* The node survives an embedded-store round-trip (`parse_node_kind`) and
  validates cleanly under `eg validate`.
* `eg refresh` (the incremental path) maintains the same `ScanCoverage` node
  (issue #403): every refresh recomputes coverage against the current tree and
  re-emits the repo-keyed node, superseding the prior full-scan or refresh
  version in the store. So after adding or removing files and running
  `eg refresh`, `eg inspect --data-dir` reports the up-to-date
  `files_walked`/`files_indexed`, never the stale pre-refresh numbers. Refresh
  runs manifest dependency extraction before finalizing coverage (mirroring the
  full-scan path), so a dependency-declaring `Cargo.toml` stays counted under
  `files_indexed`, never misclassified as skipped `toml`.

## Out of scope for this slice

Adding non-Rust/Python/TypeScript/Go parsers, `.gitignore`-semantics changes, and
per-file partial-parse coverage are out of scope: this is file-level
indexed/skipped accounting only.
