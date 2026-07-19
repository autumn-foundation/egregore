# eg query deps

List the direct outbound dependencies of a symbol — "what does this thing
call, implement, reference, and import?" — as the minimal reading list to
build before touching it (issue #123).

## Synopsis

```text
eg query deps <HANDLE> --graph <PATH>   [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query deps <HANDLE> --data-dir <DIR> [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
```

The inbound direction — who calls/references this symbol — is owned by
`eg query change-impact` (issue #76). This verb owns the outbound mirror: the
typed direct neighbors the extractor already recorded for the symbol, so an
agent about to read or edit it gets a citable dependency list without
re-opening the whole file or grepping.

Rows are dependency **leads, not runtime proof**: an outbound edge existing in
the graph never asserts the dependency is exercised at runtime, and absence of
an edge is not proof of independence (dynamic dispatch, macro-generated calls,
and cross-crate targets are outside the extraction contract).

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<HANDLE>` | yes | Stable symbol record ID (`codegraph:vN:<hex>`) or exact symbol name. File paths and task/source handles are rejected (exit `1`). |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. |
| `--repo <SELECTOR>` | no | Restrict symbol resolution — and `--at`/`--as-of` commit resolution — to one repository (see [query.md](query.md#repository-scope---repo-issue-67)). |
| `--at <COMMIT>` | no | Return the dependency set at this commit SHA or unique prefix (requires a history store). Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Return the dependency set at the most recent commit at or before this instant. Mutually exclusive with `--at`. |
| `--format` | no | `json` (default) or `text`. |

## Semantics

- **Edges returned:** direct outbound `CALLS`, `IMPLEMENTS`, `IMPORTS`, and
  `REFERENCES` edges whose source is the queried symbol. Weak `MENTIONS`
  edges and containment topology are excluded. Each row carries the
  `relation` that produced it (AC: results are labeled with their edge type).
- **Direct neighbors only:** the transitive closure ("how a route reaches
  storage" end-to-end) is out of scope for this slice; walk it by re-running
  the verb on returned record IDs.
- **The anchor is never its own dependency:** a recursive self-call edge
  produces no row.
- **Resolution labels propagate (issues #152/#134):** a `CALLS` edge inside
  the resolution contract carries its `resolution` (`resolved` /
  `ambiguous`); an ambiguous call site fans out to every candidate, each
  labeled `ambiguous`. Legacy edges without the field omit it rather than
  inventing a status.
- **Unresolved targets are an explicit category, never dropped:** an edge
  whose target is an unresolved-call `Diagnostic` marker (no in-repo
  definition matched, issue #152) or whose target record is missing from the
  graph is emitted as a `category: "unresolved"` row with a stable `reason`
  (`unresolved_call` / `missing_target`), the raw `target_record_id`, and —
  for markers — the callee display name plus the call-site path/span.
- **Deterministic:** rows are canonically ordered (dependencies by
  `(relation, record_id, edge_record_id)`, then unresolved rows by
  `(relation, target_record_id, edge_record_id)`) and output is
  byte-identical across repeated runs on an unchanged store.

## Handle resolution

Same structural contract as the other code-handle verbs, with one deliberate
tightening: an exact name that matches **more than one live symbol** is
ambiguous for this verb (exit `1`, all candidate record IDs listed), because
merging unrelated same-name symbols' outbound edges would blend their
dependency sets. Re-run with one of the reported record IDs, or scope with
`--repo`.

## Output format

### `--format json` (default) — newline-delimited JSON

Line 1 is a summary envelope; every following line is one row — all
`category: "dependency"` rows, then all `category: "unresolved"` rows. An
empty dependency set is an explicit success: the envelope reports
`total_dependencies: 0` and exit `0`.

Summary envelope fields:

| Field | Type | Description |
|-------|------|-------------|
| `ok` | bool | `true` on success. |
| `handle` | string | The handle as supplied. |
| `target` | object | The resolved symbol: `record_id`, `schema_version`, `name`, `kind`, `repo_relative_path`, `span`. |
| `direction` | string | Always `"outbound"` (the inbound mirror is `change-impact`, issue #76). |
| `edge_labels` | array | Always `["CALLS","IMPLEMENTS","IMPORTS","REFERENCES"]`. |
| `at_commit` | string | Present with `--at`/`--as-of`: the resolved commit SHA. |
| `as_of` | string | Present with `--as-of`: the instant as supplied. |
| `total_dependencies` | number | Dependency row count (matches the emitted rows). |
| `total_unresolved` | number | Unresolved row count (matches the emitted rows). |
| `disclaimer` | string | Leads-not-proof statement, always present. |
| `diagnostics` | array | Stable machine-readable diagnostics (e.g. `protected_payload`). |

Dependency row fields (`category: "dependency"`):

| Field | Type | Description |
|-------|------|-------------|
| `relation` | string | Edge label that produced the row: `CALLS` / `IMPLEMENTS` / `IMPORTS` / `REFERENCES`. |
| `record_id` | string | Stable record ID of the dependency node. |
| `schema_version` | number | Record schema version. |
| `name` / `kind` | string | Dependency name and node kind (`Symbol`, `Import`, `Module`, …). |
| `repo_relative_path` / `span` | string / object | Citable file/span handle. |
| `valid_time` / `git_commit` | string | Temporal provenance, when the record carries it (history views). |
| `resolution` | string | `CALLS` resolution status (`resolved` / `ambiguous`), when the edge carries one. |
| `edge_record_id` | string | Stable record ID of the producing edge. |
| `trust` | string | Always `"dependency_lead"`. |

Unresolved row fields (`category: "unresolved"`):

| Field | Type | Description |
|-------|------|-------------|
| `relation` | string | Edge label that produced the row. |
| `reason` | string | `unresolved_call` (the edge targets a Diagnostic marker or carries `resolution: "unresolved"` — even when that marker record is absent from the graph) or `missing_target` (the target record is not in the graph and the edge carries no unresolved signal). |
| `record_id` | string | The Diagnostic marker's record ID, when the marker is in-graph. |
| `name` / `kind` | string | Callee display name and marker kind, when the marker is in-graph. |
| `repo_relative_path` / `span` | string / object | The call site recorded by the marker, when present. |
| `target_record_id` | string | The raw target record ID carried by the edge, always present. |
| `resolution` | string | Edge resolution status, when carried. |
| `edge_record_id` | string | Stable record ID of the producing edge. |
| `trust` | string | Always `"dependency_lead"`. |

The output never includes raw source text, patch hunks, transcript text, or
protected-artifact payloads — record IDs, names, paths, spans, and counts
only.

### `--format text`

One human-readable line per row (relation, name, location). The exact format
is not stable and must not be parsed by scripts.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Query completed — including an explicit empty dependency set. |
| `1` | Malformed or unsupported handle (empty, malformed canonical ID, file/task/source handle, non-symbol record), ambiguous symbol name (candidates listed on stderr), unknown/ambiguous `--repo` selector, ambiguous `--at` prefix, or an invalid `--as-of` timestamp. Machine-readable JSON on stderr. |
| `2` | Handle resolves to no live record (`no_match` / `stale_handle`), `--at` names no commit (`missing_commit`), `--as-of` predates all commits (`no_commit_at_or_before`), or the store has no history (`empty_history`). Machine-readable envelope on stdout. |

## Temporal views (`--at` / `--as-of`)

Over a `scan-history` graph or an ingested history store, the query runs
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
corpus — records current at each repository's stamped HEAD commit — so a
dependency or edge removed before HEAD no longer appears. This flips the pre-#427
default, which read the **union** of all commit snapshots. Pass `--all-history`
to opt back into that union; pass `--at-head` to force the HEAD-anchored view
explicitly. The summary envelope discloses `corpus_mode` / `corpus_mode_source` /
`corpus_disclaimer`. `--at-head` and `--all-history` are mutually exclusive with
each other and with `--at`/`--as-of` (exit `1`, `unsupported_combination`). See
[Corpus scope for query lanes](corpus-modes.md).

## Example

```sh
eg scan . --out graph.jsonl
eg query deps handle_query --graph graph.jsonl
```

```json
{"ok":true,"handle":"handle_query","target":{"record_id":"codegraph:v5:ab…","schema_version":5,"name":"handle_query","kind":"Symbol","repo_relative_path":"src/query.rs","span":{"start_byte":0,"end_byte":100,"start_line":10,"end_line":30}},"direction":"outbound","edge_labels":["CALLS","IMPLEMENTS","IMPORTS","REFERENCES"],"total_dependencies":2,"total_unresolved":1,"disclaimer":"Rows are graph-derived dependency LEADS: …","diagnostics":[]}
{"category":"dependency","relation":"CALLS","record_id":"codegraph:v5:cd…","schema_version":5,"name":"load_records","kind":"Symbol","repo_relative_path":"src/store.rs","span":{…},"resolution":"resolved","edge_record_id":"codegraph:v5:ef…","trust":"dependency_lead"}
{"category":"dependency","relation":"IMPORTS","record_id":"codegraph:v5:12…","schema_version":5,"name":"serde_json","kind":"Import","repo_relative_path":"src/query.rs","span":{…},"edge_record_id":"codegraph:v5:34…","trust":"dependency_lead"}
{"category":"unresolved","relation":"CALLS","reason":"unresolved_call","record_id":"codegraph:v5:56…","name":"external_call","kind":"Diagnostic","repo_relative_path":"src/query.rs","span":{…},"target_record_id":"codegraph:v5:56…","resolution":"unresolved","edge_record_id":"codegraph:v5:78…","trust":"dependency_lead"}
```

## Out of scope (this slice)

- Transitive/multi-hop dependency closure — this slice ships direct outbound
  neighbors only.
- The inbound callers/blast-radius direction — owned by `change-impact`
  (issue #76).
- New extraction: no cross-crate resolution, macro expansion, or trait-method
  dispatch beyond the edges `scan` already emits.
- Joining dependencies to verification or agent-memory records.
