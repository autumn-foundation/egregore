# eg query path

Trace a directed **shortest call path** between two named symbols — "does `A`
reach `B` through the call graph, and if so by what concrete path?" — with a
single citable witness (issue #225).

## Synopsis

```text
eg query path <FROM> <TO> --graph <PATH>   [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query path <FROM> <TO> --data-dir <DIR> [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
```

`eg query transitive-callers` (#139) returns the upstream reachability *set* and
`eg query deps` (#123) returns the one-hop callees. This verb owns the directed
A-to-B reachability *witness* neither provides: it walks a BFS from `FROM`
toward `TO` over the call graph and returns one concrete, deterministic path,
so an agent reasoning about control flow, blast radius, or input-to-sink
reachability gets the chain with one call instead of stitching `callers` /
`deps` hop by hop.

The witness is a reachability **lead, not proof**: a call path in the graph
never asserts that control flow reaches `TO` at runtime, and a `no_path`
verdict is never proof of non-reachability (dynamic dispatch, macro-generated
calls, and cross-crate calls are outside the extraction contract).

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<FROM>` | yes | Source symbol record ID (`codegraph:vN:<hex>`) or exact symbol name. File paths and task/source handles are rejected (exit `1`). |
| `<TO>` | yes | Target symbol record ID or exact symbol name, same contract as `<FROM>`. |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. |
| `--repo <SELECTOR>` | no | Restrict symbol resolution — and `--at`/`--as-of` commit resolution — to one repository (see [query.md](query.md#repository-scope---repo-issue-67)). |
| `--at <COMMIT>` | no | Trace against the graph state at this commit SHA or unique prefix (requires a history store). Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Trace against the graph state at the most recent commit at or before this instant. Mutually exclusive with `--at`. |
| `--format` | no | `json` (default) or `text`. |

## Traversal semantics

- **Edges walked:** outbound `CALLS` edges carrying `resolution == resolved`
  (issues #152/#134) **only**. Ambiguous and unresolved `CALLS` edges, `CALLS`
  edges outside the resolution contract (no `resolution` field), and every
  other label (`REFERENCES`, `MENTIONS`, `IMPORTS`, `IMPLEMENTS`, containment
  topology) are excluded. Because ambiguity never silently picks a target, a
  path is only ever asserted over edges the extractor resolved to exactly one
  in-repo definition.
- **Direction is honored:** the walk follows outbound `CALLS`, so `path A B`
  and `path B A` are distinct queries; a one-way edge `A -> B` produces a
  witness for `path A B` and none for `path B A`.
- **Shortest path, deterministic (tie-break):** the walk is a
  level-synchronized BFS from `FROM`. Among all minimum-hop directed paths, the
  witness is selected by retaining, for each node first reached at its shortest
  depth, the discovery edge with the **lexicographically smallest
  `(source_record_id, edge_record_id)` pair** as that node's parent pointer;
  the witness is reconstructed by following parent pointers from `TO` back to
  `FROM`. This yields exactly one path, byte-identical across repeated runs on
  an unchanged store.
- **Cycles terminate:** a visited set guarantees mutual recursion neither loops
  nor duplicates a node.
- **Trivial path:** `FROM == TO` (the same symbol, whether named twice or by
  matching record IDs) is a `path_found` verdict with `hops: 0` and no hop
  lines.
- **Tombstone-aware:** symbols and edges retracted by an unsuperseded tombstone
  are excluded, matching the sibling reachability lanes.

## Handle resolution

Each endpoint resolves by the same structural contract as the other
code-handle verbs, with one deliberate tightening carried over from
`transitive-callers`: an exact name that matches **more than one live symbol**
is ambiguous (exit `1`, all candidate record IDs listed on stderr), because a
witness over the union would silently pick one same-name symbol. Re-run with
one of the reported record IDs, or scope with `--repo`. Both endpoints are
resolved independently; the `endpoint` field on an error envelope names which
side (`from` / `to`) failed.

## Output format

### `--format json` (default) — newline-delimited JSON

Line 1 is a summary envelope; every following line is one hop of the witness
path in `FROM -> TO` order. A `no_path` verdict emits only the envelope line
(no hop lines) and exits `2`.

Summary envelope fields:

| Field | Type | Description |
|-------|------|-------------|
| `ok` | bool | `true` — the query answered definitively (including a `no_path` verdict). |
| `from_handle` / `to_handle` | string | The handles as supplied. |
| `from` / `to` | object | The resolved endpoints: `record_id`, `schema_version`, `name`, `kind`, `repo_relative_path`, `span` (plus `valid_time` / `git_commit` in history views). |
| `direction` | string | Always `"outbound"`. |
| `edge_labels` | array | Always `["CALLS"]`. |
| `resolution_scope` | string | Always `"resolved"` — the walk traverses only resolved `CALLS` edges. |
| `at_commit` | string | Present with `--at`/`--as-of`: the resolved commit SHA. |
| `as_of` | string | Present with `--as-of`: the instant as supplied. |
| `verdict` | string | `"path_found"` or `"no_path"`. |
| `path_found` | bool | `true` when a directed path (including the trivial `A == B` path) exists. |
| `hops` | number | Number of hops (`0` for the trivial path; `0` for `no_path`). |
| `disclaimer` | string | Leads-not-proof + resolved-only statement, always present. |

Hop fields (one per line, in `FROM -> TO` order):

| Field | Type | Description |
|-------|------|-------------|
| `index` | number | 1-based hop position. |
| `from` / `to` | object | The caller / callee node of this hop: `record_id`, `schema_version`, `name`, `kind`, `repo_relative_path`, `span`. |
| `edge_record_id` | string | Stable record ID of the connecting `CALLS` edge. |
| `edge_label` | string | Always `"CALLS"`. |
| `resolution` | string | Always `"resolved"`. |
| `confidence` | string | Edge extraction confidence, when carried. |
| `trust` | string | Always `"reachability_lead"`. |

The output never includes raw source text, patch hunks, transcript text, or
protected-artifact payloads — record IDs, names, paths, spans, and counts only.

### `--format text`

A human-readable header line plus one indented line per hop, or a single
`no path` line for a `no_path` verdict. The exact format is not stable and must
not be parsed by scripts.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | A directed path was found (including the trivial `FROM == TO` zero-hop path). |
| `1` | Malformed / ambiguous / unsupported handle for either endpoint (empty, malformed canonical ID, file/task/source handle, non-symbol record, ambiguous symbol name with candidates listed on stderr), or an unknown/ambiguous `--repo` selector, ambiguous `--at` prefix, or invalid `--as-of` timestamp. Machine-readable JSON on stderr. Supplying both `--at` and `--as-of` is a clap usage error. |
| `2` | An endpoint resolves to no live record (`no_match` / `stale_handle`), both endpoints resolve but **no directed path exists** (`no_path`), `--at` names no commit (`missing_commit`), `--as-of` predates all commits (`no_commit_at_or_before`), or the store has no history (`empty_history`). Machine-readable envelope on stdout. |

## Temporal views (`--at` / `--as-of`)

Over a `scan-history` graph or an ingested history store, the walk runs against
the single-commit snapshot the selector names, reusing recorded history output
without touching Git state or the working tree. `--as-of` resolves to the most
recent commit whose valid time is at or before the instant; the envelope's
`at_commit` reports which commit answered. When `--repo` is set, commit
resolution happens within the selected repository.

### Temporal scope of an unpinned read

With **neither** `--at` nor `--as-of`, a `scan-history` graph or store is read
as the **union of all commit snapshots** — `scan-history` emits no edge
tombstones, so a resolved `CALLS` edge that was removed at a later commit is
still present in the union. A directed path over such an edge therefore still
yields `path_found`. This is deliberate and matches `deps`,
`transitive-callers`, and `transitive-callees`, which read the same union.
Pass `--at <HEAD_SHA>` for HEAD-only (single-snapshot) semantics — the walk
then sees only the edges live at that commit, so an edge deleted before `HEAD`
no longer contributes a path.

## Example

```sh
eg scan . --out graph.jsonl
eg query path handle_query write_record --graph graph.jsonl
```

```json
{"ok":true,"from_handle":"handle_query","to_handle":"write_record","from":{"record_id":"codegraph:v6:ab…","schema_version":6,"name":"handle_query","kind":"Symbol","repo_relative_path":"src/query.rs","span":{…}},"to":{"record_id":"codegraph:v6:cd…","schema_version":6,"name":"write_record","kind":"Symbol","repo_relative_path":"src/store.rs","span":{…}},"direction":"outbound","edge_labels":["CALLS"],"resolution_scope":"resolved","verdict":"path_found","path_found":true,"hops":2,"disclaimer":"The witness path uses only `resolved` CALLS edges: …"}
{"index":1,"from":{"record_id":"codegraph:v6:ab…","name":"handle_query",…},"to":{"record_id":"codegraph:v6:ef…","name":"dispatch",…},"edge_record_id":"codegraph:v6:11…","edge_label":"CALLS","resolution":"resolved","confidence":"1.0","trust":"reachability_lead"}
{"index":2,"from":{"record_id":"codegraph:v6:ef…","name":"dispatch",…},"to":{"record_id":"codegraph:v6:cd…","name":"write_record",…},"edge_record_id":"codegraph:v6:22…","edge_label":"CALLS","resolution":"resolved","confidence":"1.0","trust":"reachability_lead"}
```

## Out of scope (this slice)

- Enumerating *all* paths (or k-shortest paths) between two symbols; this slice
  returns one deterministic witness path plus existence. A bounded all-paths
  mode can be a follow-up.
- Semantic dataflow or taint precision (value flow, aliasing, field
  sensitivity). This is syntactic call-graph reachability over best-effort
  resolved edges, not a dataflow engine.
- Improving cross-file edge resolution itself (tracked in #152); this lane
  consumes whatever resolved edges exist and reports the gap honestly.
- Path queries over non-call edges such as `CONTAINS`, `IMPORTS`, or temporal
  `PARENT_OF`.
