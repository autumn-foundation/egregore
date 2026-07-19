# eg index

Build a **persistent sidecar index** next to a graph JSONL so targeted
`eg query … --graph <file>` lanes seek to the records their answer needs instead
of deserializing every physical line of the file (issue #447).

```sh
eg scan . --out graph.jsonl
eg index graph.jsonl            # writes graph.jsonl.idx
eg query deps my_symbol --graph graph.jsonl   # now seeks, not full-scans
```

> **Pure access-path optimization.** The index never changes an answer. Every
> migrated lane returns byte-identical stdout and the same exit code with or
> without the index — it only changes how fast the records are found. A stale,
> corrupt, missing, or version-mismatched index is silently ignored and the
> lane falls back to the cold full-file scan.

## Where it fits

For a 299k-line export, `eg query deps <symbol>` needs only a handful of
records, yet the cold loader deserializes the whole file. `eg index` records,
once, where each record lives so a lane can seek to the bounded closure of
records it needs.

```
eg scan . --out graph.jsonl      # produce the graph
eg index graph.jsonl             # build graph.jsonl.idx (idempotent, atomic)
eg query deps <symbol> --graph graph.jsonl        # fast path
eg query context <symbol> --graph graph.jsonl
eg query at <path>:<line> --graph graph.jsonl
eg query locate <path>:<line> --graph graph.jsonl
eg query file <path> --graph graph.jsonl
eg query who-imports <module> --graph graph.jsonl
```

`eg index` is the **only** writer of the sidecar index. Query lanes never write
it; auto-build on query is off for v1. Re-run `eg index` after re-scanning to
refresh the index.

## Sidecar location and format

The index is written to `<graph>.idx` adjacent to the graph (`foo.jsonl` →
`foo.jsonl.idx`). It is a raw little-endian binary header, a `\n`, then one JSON
line:

- **Header:** magic `EGIX`, `format_version` (`u32`), the graph file's byte
  length (`u64`), and the graph file's BLAKE3 (`[u8; 32]`).
- **Body:** deterministic sorted maps from record ids, node names,
  repo-relative paths, node-kind strings, tombstone `deleted_id`s, and edge
  adjacency to line-start byte offsets, plus a `has_temporal_history` boolean
  (see below).

The `has_temporal_history` flag (still format v1, an additive body field) is
`true` when the indexed graph is a **history / corpus store** — any record
carries temporal provenance, or a `Commit` node is present. It is recorded at
build time so the loader can decide, before hydrating, whether the fast path is
sound (see the next section).

The write is atomic (`.idx.tmp` then rename), so a reader never observes a torn
index. Repeated builds of an unchanged graph produce a byte-identical `.idx`.

## Content-addressed validity + transparent fallback

An index is used for a query only when it is present, the magic matches, the
`format_version` equals the version this build understands, and the stored graph
length **and** BLAKE3 match the graph file's current bytes. Any mismatch —
absent, corrupt, truncated, an unknown future version, or a stale hash because
the graph changed — makes the query lane transparently cold-scan and return the
current answer. The query path never fails because of the index and never writes
it.

`eg index` refuses (exit 2, machine-readable `index_build_error`) any graph a
cold load would also reject (a parse error or an unknown schema version) and
writes no `.idx`, so a graph that cold-loads cleanly is the only graph that gets
an index.

## Which lanes use it

Migrated in v1 (fast path taken only for the plain current-state, unscoped
invocation — `--repo`, `--at`, `--as-of`, and `--repo-path` keep the cold scan
because they need global topology or the commit timeline):

- `eg query deps <symbol>`
- `eg query context <symbol>` and `eg query symbol <name>`
- `eg query at <path>:<line>` and `eg query locate <path>:<line>`
- `eg query file <path>` (current state)
- `eg query who-imports <module-path>`

Every other `--graph` lane is unchanged (it reads the whole file exactly as
before). The embedded `--data-dir` store has its own indexes and is out of scope
for this command.

### History / corpus stores fall back to the cold scan

Over a `scan-history` (or otherwise temporal / corpus) graph, several migrated
lanes changed semantics in issue #457: `deps` and `who-imports` **HEAD-anchor by
default**, dropping records that are not current at the repository's stamped
HEAD, and other history-view lanes read across commit snapshots. Deciding what
is current at HEAD needs global commit topology and **every** version of every
record — a set a targeted index closure (a bounded neighbourhood of one symbol,
path, or kind) cannot soundly supply. Hydrating only the closure would silently
omit off-HEAD versions or the `Repository` snapshot the gate reads, diverging
from the cold answer.

So the loader composes with #457 by **falling back to the cold whole-file scan
for any migrated lane whenever the index's `has_temporal_history` flag is set**.
A plain current-tree `eg scan` graph (no temporal records, no `Commit` nodes)
keeps the #447 fast path — there head-anchoring drops nothing and the closure is
byte-identical. A history graph still gets a valid `.idx` (and the freshness and
build guarantees above), it simply cold-scans on query; the index answer stays
byte-identical to the cold answer for every lane and every corpus mode.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Index built and written to `<graph>.idx`. |
| 2 | The graph could not be read or a line is not an indexable record (no `.idx` written). |
