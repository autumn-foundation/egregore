# Issue #447 — Persistent sidecar index for `eg query … --graph` lanes

## SPEC

### Problem

Every `eg query … --graph <file>` lane loads the whole JSONL file
(`load_records_from_jsonl`) and deserializes every physical record — for a
299k-line export that is the dominant cost of a targeted lookup such as
`eg query deps <symbol>`, which only needs a handful of records. There is no
way to seek to the relevant records.

### Solution

A persistent, content-addressed sidecar index written next to the graph file at
`<graph>.idx`, built once by a new `eg index <graph>` command. Targeted query
lanes consult the index, hydrate only the closure of records the answer needs by
seeking to their byte offsets, and reproduce the EXACT records the cold path
produced. The index is a pure access-path optimization: results are
byte-identical with or without it.

### Index file format (v1)

`<graph>.idx`, adjacent to the graph (`foo.jsonl` → `foo.jsonl.idx`).

Layout: a raw little-endian binary header, a `\n`, then one JSON line body.

Header (fixed width):
- magic `b"EGIX"` (4 bytes)
- `format_version: u32` (= 1)
- `graph_len: u64` (byte length of the graph file at build time)
- `graph_blake3: [u8; 32]` (BLAKE3 of the graph file bytes at build time)

Body: a serde struct of `BTreeMap`s (deterministic, sorted serialization):
- `by_id: BTreeMap<String, Vec<u64>>` — every record's `id()` → sorted line
  START byte offsets (ALL physical versions).
- `by_deleted_id: BTreeMap<String, Vec<u64>>` — Tombstone `deleted_id` → offsets.
- `by_name: BTreeMap<String, Vec<u64>>` — Node `name` → offsets.
- `by_path: BTreeMap<String, Vec<u64>>` — Node `repo_relative_path` → offsets.
- `by_kind: BTreeMap<String, Vec<u64>>` — node-kind string → offsets.
- `adjacency: BTreeMap<String, Vec<u64>>` — node id → offsets of every incident
  Edge (source OR target).

All `Vec<u64>` offset lists are sorted for determinism.

### Validity

An index is valid for a graph only when the header magic matches,
`format_version == 1`, `graph_len == current file length`, and
`graph_blake3 == BLAKE3(current file bytes)`. Any mismatch (absent, corrupt,
truncated, version-mismatch, stale hash/len) makes the query lane transparently
fall back to the cold `load_records_from_jsonl` scan. The query path NEVER fails
because of the index and NEVER writes it.

`eg index` is the ONLY writer. It builds the index by streaming the whole file
once, parsing each non-blank line exactly as `records_from_jsonl` does (blank
lines skipped identically); if any line is an unknown schema version or fails to
parse, the build fails with the same error the cold path would raise and writes
no `.idx` (so a graph that cold-loads cleanly is the only graph that gets an
index, and hash-validated hydration never meets an unknown-version line). The
write is atomic: `<graph>.idx.tmp` then `fs::rename`. Auto-build on query is OFF
for v1.

### Selector + hydration closure

`Selector { Whole, ById(String), ByName(String), ByPath(String), ByKind(String) }`.

`load_records_selected(graph, data_dir, selector)`:
- `--data-dir`: unchanged embedded load (embedded store OUT of scope for #447).
- `--graph` + `Whole`: delegate to `load_records_from_jsonl` (byte-identical).
- `--graph` + non-Whole: load and validate `<graph>.idx`; if valid, hydrate the
  closure by seek; else cold-scan fallback.

Closure (a SUPERSET yielding byte-identical lane output):
- Always include ALL versions + tombstones of every id touched: for id X pull
  `by_id[X]` ∪ `by_deleted_id[X]`.
- `ById(id)`: X's records+tombstones, `adjacency[X]` edges, and for each incident
  edge the OTHER endpoint's records+tombstones + that endpoint's adjacency edges
  (one hop of neighbours), then a containment ANCESTRY climb to the repository
  root so `RepositoryIndex::owner_of`/`display_of` reproduce the cold answer
  (query at/locate/file emit `repository_id` even without `--repo`). Bounded.
- `ByName(name)`: the `ById` closure for each id in `by_name[name]`.
- `ByPath(path)`: all `by_path[path]` records + tombstones + ancestry climb.
- `ByKind(kind)`: all `by_kind[kind]` records + tombstones.

Liveness: lanes apply latest-write-wins liveness + tombstones over the records
they receive; hydrating ALL versions + tombstones of every id in the closure is
what makes the in-lane gate byte-identical. Records are never deduped or
pre-resolved. Hydrated records are returned in ascending byte-offset order, i.e.
file order, so the relative order of the included subset matches the cold path.

### Lanes migrated in v1

Fast path taken only for the plain current-state, unscoped invocation
(`--repo` absent, `--at`/`--as-of` absent — those need global topology / commit
timelines and stay cold):
- `deps <symbol>` → `ByName` (or `ById` for a `codegraph:` handle).
- `context <symbol>` / `symbol <name>` → `ByName`.
- `at <path>:<line>` / `locate <path>:<line>` → `ByPath`.
- `file <path>` (current, no `--at`/`--as-of`) → `ByPath`.
- `who-imports <module>` → `ByKind("Import")`.

Every other `--graph` lane stays `Whole` (identical to today).
transitive-callers/callees/path/cycles are intentionally deferred (unbounded
closures). Correctness over coverage: any lane whose differential test the
closure cannot satisfy reverts to `Whole`.
