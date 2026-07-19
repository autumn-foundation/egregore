# eg query transitive-callers

Walk the transitive inbound callers/referencers of a symbol — "what can reach
this, and by which call path?" — for pre-edit blast-radius triage (issue #139).

## Synopsis

```text
eg query transitive-callers <HANDLE> --graph <PATH>   [--repo <SELECTOR>] [--max-depth N] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query transitive-callers <HANDLE> --data-dir <DIR> [--repo <SELECTOR>] [--max-depth N] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
```

The depth-1 verbs answer "who calls this directly" (`eg query change-impact`,
issue #76). This verb owns the multi-hop closure: it returns every symbol that
can reach the target through a chain of inbound `CALLS`/`REFERENCES` edges,
**with the connecting call path for each**, so an agent can tell when a
public entry point or test reaches its edit target through a 2–3-hop chain the
one-hop query never surfaces — without hand-looping the direct query and
losing the path.

Rows are reachability **leads, not breakage proof**: a call path existing in
the graph never asserts that a reachable symbol will break, that a test will
fail, or that the edit is unsafe. Absence of a path is likewise not proof of
unreachability (dynamic dispatch, macro-generated calls, and cross-crate
callers are outside the extraction contract).

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<HANDLE>` | yes | Stable symbol record ID (`codegraph:vN:<hex>`) or exact symbol name. File paths and task/source handles are rejected (exit `1`). |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. |
| `--repo <SELECTOR>` | no | Restrict symbol resolution — and `--at`/`--as-of` commit resolution — to one repository (see [query.md](query.md#repository-scope---repo-issue-67)). |
| `--max-depth N` | no | Inbound walk bound in hops. Default `5`; must be ≥ 1. |
| `--at <COMMIT>` | no | Walk the graph state at this commit SHA or unique prefix (requires a history store). Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Walk the graph state at the most recent commit at or before this instant. Mutually exclusive with `--at`. |
| `--format` | no | `json` (default) or `text`. |

## Traversal semantics

- **Edges walked:** inbound `CALLS` and `REFERENCES` only. Weak `MENTIONS`
  edges and containment topology are excluded, so the `--max-depth=1` result
  is exactly the direct inbound caller/referencer set of the depth-1 verbs
  (no double-counting, no drift).
- **Shortest paths, deterministic:** the walk is a level-synchronized BFS.
  Each reachable node is reported **once**, at its shortest hop distance, with
  one concrete shortest connecting path chosen deterministically (minimum
  `(parent record ID, edge record ID)` at the discovering depth). Output is
  byte-identical across repeated runs on an unchanged store.
- **Cycles terminate:** a visited set guarantees mutual recursion neither
  loops nor duplicates a node; the queried symbol is never reported as its own
  transitive caller.
- **Resolution propagation (issues #152/#134):** every path step exposes the
  `resolution` label its `CALLS` edge carries (`resolved` / `ambiguous` /
  `unresolved`), and each row carries `path_resolution` — the **weakest**
  status along its chain (`unresolved` > `ambiguous` > `resolved`). A path
  that crosses an ambiguous edge is only as trustworthy as that edge, and the
  row says so. `path_resolution` is omitted when no step on the path is inside
  the resolution contract (e.g. a pure `REFERENCES` chain or legacy edges).
- **Truncation is explicit:** reaching `--max-depth` with reachable nodes
  still undiscovered emits a `truncation` diagnostic counting the dropped
  frontier per depth — never a silent omission.
- **Dangling edges are diagnostics:** an inbound edge whose source record is
  missing from the store surfaces as an `unresolved_edge_source` diagnostic,
  never as a row and never silently dropped.

## Handle resolution

Same structural contract as the other code-handle verbs, with one deliberate
tightening: an exact name that matches **more than one live symbol** is
ambiguous for this verb (exit `1`, all candidate record IDs listed), because
walking the union would bleed an unrelated same-name symbol's edges into the
target's reachability. Re-run with one of the reported record IDs, or scope
with `--repo`.

## Output format

### `--format json` (default) — newline-delimited JSON

Line 1 is a summary envelope; every following line is one reachable row,
canonically ordered by `(hop, record_id)` ascending. An empty reachable set is
an explicit success: the envelope reports `total_reachable: 0` and exit `0`.

Summary envelope fields:

| Field | Type | Description |
|-------|------|-------------|
| `ok` | bool | `true` on success. |
| `handle` | string | The handle as supplied. |
| `target` | object | The resolved symbol: `record_id`, `schema_version`, `name`, `kind`, `repo_relative_path`, `span`. |
| `direction` | string | Always `"inbound"` (the outbound mirror is [`eg query transitive-callees`](transitive-callees.md), issue #253). |
| `edge_labels` | array | Always `["CALLS","REFERENCES"]`. |
| `max_depth` | number | Bound in effect. |
| `at_commit` | string | Present with `--at`/`--as-of`: the resolved commit SHA. |
| `as_of` | string | Present with `--as-of`: the instant as supplied. |
| `total_reachable` | number | Row count (matches the emitted row lines). |
| `disclaimer` | string | Leads-not-proof statement, always present. |
| `truncation` | object | Present only when the bound dropped reachable nodes: `code` (`max_depth_truncated`), `max_depth`, `dropped_frontier` (array of `{depth, count}`), `dropped_total`. |
| `diagnostics` | array | Stable machine-readable diagnostics (e.g. `unresolved_edge_source`). |

Row fields:

| Field | Type | Description |
|-------|------|-------------|
| `record_id` | string | Stable record ID of the reachable node. |
| `schema_version` | number | Record schema version. |
| `name` / `kind` | string | Symbol name and node kind. |
| `repo_relative_path` / `span` | string / object | Citable file/span handle. |
| `valid_time` / `git_commit` | string | Temporal provenance, when the record carries it (history views). |
| `hop` | number | Shortest hop distance from the target (≥ 1). |
| `path_resolution` | string | Weakest `CALLS` resolution along the path; omitted when no step carries one. |
| `path` | array | Ordered connecting chain from this row down to the target. Each step: `source_record_id`, `edge_record_id`, `edge_label`, `resolution` (when carried), `target_record_id`. The first step starts at the row's own record, the last ends at the target, and consecutive steps share their middle record ID. |
| `trust` | string | Always `"reachability_lead"`. |

The output never includes raw source text, patch hunks, transcript text, or
protected-artifact payloads — record IDs, names, paths, spans, and counts
only.

### `--format text`

One human-readable line per row (name, location, hop, chain). The exact format
is not stable and must not be parsed by scripts.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Walk completed — including an explicit empty reachable set. |
| `1` | Malformed or unsupported handle (empty, malformed canonical ID, file/task/source handle, non-symbol record), ambiguous symbol name (candidates listed on stderr), unknown/ambiguous `--repo` selector, `--max-depth 0`, ambiguous `--at` prefix, or an invalid `--as-of` timestamp. Machine-readable JSON on stderr. |
| `2` | Handle resolves to no live record (`no_match` / `stale_handle`), `--at` names no commit (`missing_commit`), `--as-of` predates all commits (`no_commit_at_or_before`), or the store has no history (`empty_history`). Machine-readable envelope on stdout. |

## Temporal views (`--at` / `--as-of`)

Over a `scan-history` graph or an ingested history store, the walk runs
against the single-commit snapshot the selector names, reusing recorded
history output without touching Git state or the working tree. `--as-of`
resolves to the most recent commit whose valid time is at or before the
instant; the envelope's `at_commit` reports which commit answered. When
`--repo` is set, commit resolution happens within the selected repository
(matching `eg query deltas`): a shared multi-repository store never answers
from another repository's commits, and an `--at` prefix is never ambiguous
because of commits outside the selected repository.

## Corpus scope

**Changed default (issue #427).** With **neither** `--at`/`--as-of` nor a corpus
flag, over a `scan-history` store this lane now defaults to the **HEAD-anchored**
corpus — records current at each repository's stamped HEAD commit — so a caller
or edge removed before HEAD no longer appears. This flips the pre-#427 default,
which read the **union** of all commit snapshots. Pass `--all-history` to opt
back into that union; pass `--at-head` to force the HEAD-anchored view
explicitly. The summary envelope discloses `corpus_mode` / `corpus_mode_source` /
`corpus_disclaimer`. `--at-head` and `--all-history` are mutually exclusive with
each other and with `--at`/`--as-of` (exit `1`, `unsupported_combination`). See
[Corpus scope for query lanes](corpus-modes.md).

## Example

```sh
eg scan . --out graph.jsonl
eg query transitive-callers handle_query --graph graph.jsonl --max-depth 3
```

```json
{"ok":true,"handle":"handle_query","target":{"record_id":"codegraph:v5:ab…","schema_version":5,"name":"handle_query","kind":"Symbol","repo_relative_path":"src/query.rs","span":{"start_byte":0,"end_byte":100,"start_line":10,"end_line":30}},"direction":"inbound","edge_labels":["CALLS","REFERENCES"],"max_depth":3,"total_reachable":2,"disclaimer":"Rows are reachability LEADS: …","diagnostics":[]}
{"record_id":"codegraph:v5:cd…","schema_version":5,"name":"dispatch","kind":"Symbol","repo_relative_path":"src/cli.rs","span":{…},"hop":1,"path_resolution":"resolved","path":[{"source_record_id":"codegraph:v5:cd…","edge_record_id":"codegraph:v5:ef…","edge_label":"CALLS","resolution":"resolved","target_record_id":"codegraph:v5:ab…"}],"trust":"reachability_lead"}
{"record_id":"codegraph:v5:12…","schema_version":5,"name":"main","kind":"Symbol","repo_relative_path":"src/main.rs","span":{…},"hop":2,"path_resolution":"resolved","path":[{"source_record_id":"codegraph:v5:12…","edge_record_id":"codegraph:v5:34…","edge_label":"CALLS","resolution":"resolved","target_record_id":"codegraph:v5:cd…"},{"source_record_id":"codegraph:v5:cd…","edge_record_id":"codegraph:v5:ef…","edge_label":"CALLS","resolution":"resolved","target_record_id":"codegraph:v5:ab…"}],"trust":"reachability_lead"}
```

## Out of scope (this slice)

- The outbound direction — transitive callees / "how a route reaches storage"
  — is the mirror slice owned by [`eg query transitive-callees`](transitive-callees.md)
  (issue #253).
- Joining reachable nodes to public-API/visibility (#124) or test (#126) sets.
- Risk scoring, ranking, test selection, or root-cause inference.
- New extraction: no cross-crate resolution, macro expansion, or trait
  dispatch beyond the edges `scan` already emits.
