# `POST /v1/query` — Verb-Dispatch Query Surface

**Schema version:** 1 (`DAEMON_QUERY_SCHEMA_VERSION`)
**Status:** Implemented. Changes that alter the response shape for existing verbs,
rename verbs, or remove verbs require a new `schema_version`.

---

## 1 — Overview

`POST /v1/query` accepts a tagged-verb request envelope and dispatches to a
typed query handler. Each verb returns a uniform `result` object containing
`verb`, `snapshot`, `records`, and `page`.

This replaces the earlier batch-read-by-id stub. The batch read is preserved as
the `get_records` verb.

Full transport and authentication rules are in
[`docs/schema/daemon-api.md`](daemon-api.md).
Daemon clients MUST first follow the runtime discovery and stale-file checks in
[`docs/schema/daemon-runtime.md`](daemon-runtime.md); the query verb set runs
over the `address` and bearer token discovered through that contract.

Returned record-shaped rows include the record-level `schema_version` so mixed
stores and bi-temporal reads do not hide version boundaries. Record compatibility
is governed by [`schema-versioning.md`](schema-versioning.md), not by this query
envelope `schema_version`.

---

## 2 — Request envelope

```json
{
  "request_id": "<client-chosen correlation id>",
  "agent_id":   "<optional calling agent identity>",
  "verb":       "<verb name>",
  "params":     { /* verb-specific */ },
  "as_of":      { "valid_time": "<RFC 3339>" },
  "budget":     { "max_results": 5000, "timeout_ms": 5000 }
}
```

| Field        | Type    | Required | Notes |
|--------------|---------|----------|-------|
| `request_id` | string  | yes      | Echoed in every response |
| `agent_id`   | string  | no       | Optional for code-graph reads |
| `verb`       | string  | yes      | Missing or empty → HTTP 400 `missing_field` |
| `params`     | object  | no       | Defaults to `{}` if omitted |
| `as_of`      | object  | no       | See §4 |
| `budget`     | object  | no       | See §5 |

### `as_of` selector

```json
"as_of": {
  "valid_time":       "<RFC 3339>",
  "transaction_time": "<RFC 3339>",
  "since":            "<RFC 3339>"
}
```

All three fields are optional. Behaviour per field:

| Field              | Status      | Effect |
|--------------------|-------------|--------|
| `valid_time`       | implemented | Restricts symbol results to records whose valid-time is ≤ the given instant |
| `transaction_time` | implemented for `symbol_by_name` (issue #66) | Returns, per stable record ID, the version the store knew at or before the instant. On any other verb → HTTP 501 `not_implemented` |
| `since`            | reserved    | Always returns HTTP 501 `not_implemented` |

When `transaction_time` is set on `symbol_by_name`, the result adds a `tx_as_of`
echo and a `diagnostics` array (see [`temporal-selectors.md`](temporal-selectors.md)
for the diagnostic codes), and each record carries `domain`, `trust_class`,
`valid_time`, `valid_time_source`, and `transaction_time` handles in addition to
the base symbol shape. Setting both `valid_time` and `transaction_time` applies
both axes independently. A malformed `transaction_time` returns HTTP 400
`bad_request` (field `as_of.transaction_time`), never a silent current-state read.

### `budget` object

```json
"budget": { "max_results": 1000, "timeout_ms": 3000 }
```

Server-enforced defaults: `max_results = 5000`, `timeout_ms = 5000`. Exceeding
the timeout returns HTTP 408 `query_timeout`.

---

## 3 — Response envelope

Every successful query returns HTTP 200 with:

```json
{
  "ok": true,
  "request_id": "<echoed>",
  "result": {
    "verb":     "<verb name>",
    "snapshot": "<RFC 3339 instant when the read lock was acquired>",
    "records":  [ /* verb-specific record objects */ ],
    "page": {
      "cursor":   null,
      "has_more": false,
      "returned": 3
    }
  }
}
```

`page.cursor` is always `null` and `page.has_more` is always `false` in v1
(full-page results only). `page.returned` is the count of items in `records`.

Error responses follow the standard envelope in
[`docs/schema/daemon-api.md §4`](daemon-api.md).

---

## 4 — Error codes specific to query

| Code                    | HTTP | When |
|-------------------------|------|------|
| `missing_field`         | 400  | `verb` is absent or blank; or `semantic_search` called without `params.query_vector` |
| `bad_request`           | 400  | Unknown verb, or required `params` field missing or malformed |
| `ambiguous_commit_prefix` | 400 | `symbol_at_commit` prefix matches > 1 commit; `resolve_record` `at` prefix matches > 1 commit |
| `dangling_handle`       | 404  | `resolve_record`: no record matches the handle in the requested view (issue #160) |
| `malformed_handle`      | 400  | `resolve_record`: `record_id` is not a `codegraph:vN:<suffix>` handle (issue #160) |
| `unsupported_handle_domain` | 400 | `resolve_record`: well-formed handle from another id domain (issue #160) |
| `unknown_repository_selector` | 400 | `params.repo` matches no repository identity in the store (issue #67) |
| `ambiguous_repository_selector` | 400 | `params.repo` matches more than one repository identity; ambiguity is never resolved implicitly. The error object carries a `candidates` array with every matching repository record ID so callers can retry with an exact selector (issue #67) |
| `missing_semantic_index` | 422 | `semantic_search` against a store with no embedding index (re-ingest with `--embed`) |
| `semantic_index_unreadable` | 422 | `semantic_search` against a store whose embedding index EXISTS on disk but was skipped at load as corrupted or unreadable (issue #489). Distinct from `missing_semantic_index`: the store WAS embedded, so reporting it as never-embedded would answer a data-loss condition with a configuration one. The message names the index files found and points at a fresh `--data-dir` — re-embedding in place would overwrite them |
| `incompatible_embedding_dimension` | 422 | `semantic_search` query vector width disagrees with the store's index |
| `not_implemented`       | 501  | Reserved verb; `as_of.transaction_time` set on a verb other than `symbol_by_name`; `as_of.since` set; or `semantic_search` on a daemon built without the `embeddings` feature |
| `query_timeout`         | 408  | Budget `timeout_ms` elapsed |
| `internal_error`        | 500  | Store read failed |

---

## 5 — Verb table

The runtime source of truth for this table is `GET /v1/capabilities` (§10):
it is built from the same `QUERY_VERB_TABLE` registry that `POST /v1/query`
dispatches through, so the two cannot drift. The table below is the
human-readable copy — if it ever disagrees with the endpoint, the endpoint
wins and this document needs a fix.

| Verb                    | Status      | Params                        | Notes |
|-------------------------|-------------|-------------------------------|-------|
| `get_records`           | implemented | `record_ids: [string]`        | Batch read by stable ID |
| `symbol_by_name`        | implemented | `name: string`, `kind?: string`, `repo?: string` | Exact name match; honours `as_of.valid_time` |
| `symbol_at_commit`      | implemented | `name: string`, `commit: string`, `repo?: string` | Prefix-safe commit lookup |
| `resolve_record`        | implemented | `record_id: string`, `at?: string`, `as_of?: string` | Record-id handle resolution with drift verdict (issue #160) |
| `file_defines`          | implemented | `repo_relative_path: string`, `repo?: string` | Symbols defined in a file |
| `locate`                | implemented | `repo_relative_path: string`, `line: u64`, `repo?: string`, `at?: string`, `as_of?: string`, `supersession?: string` | Positional query: innermost symbol at `file:line` plus the full trust-separated context bundle (issue #212); daemon face of `eg query locate` |
| `drift_top_n`           | implemented | `limit?: u64` (default 10, max 100), `repo?: string` | SemanticDrift records ranked by score |
| `clone_classes`         | implemented | `limit?: u64` (default 50, max 500), `min_size?: u64` (default 2, min 2), `repo?: string` | Exact-duplicate Rust symbol bodies grouped into clone classes (issue #216); honours `as_of.valid_time` |
| `semantic_search`       | implemented | `query_vector: [f32]`, `limit?: u64` (default 10, max 100), `repo?: string` | Natural-language code search over the shared store's embedding index. Requires the `embeddings` feature. |
| `drift`                 | reserved    | same as `drift_top_n`         | Reserved for issue #10; returns `not_implemented` until wired. |
| `observations_for_symbol` | implemented | `name: string`, `supersession?: string` | Cross-domain symbol context (issue #86); parity with `eg query context`. |
| `agent_sessions_for_repo` | implemented | `repo: string` (alias `repository_id`), `limit?: u64` (default 20, max 200) | Repo-scoped recency digest of recent agent sessions (issue #112); daemon face of `eg query sessions`. |
| `criteria_for_task`      | implemented | `task_id: string`            | Task acceptance-criteria/evidence context over [`docs/schema/project-graph.md`](project-graph.md); daemon face of `eg query task`. |
| `tests_for_symbol`       | implemented | `handle: string`, `max_depth?: u64` (default 5, min 1), `repo?: string`, `at?: string`, `as_of?: string` | Tests that exercise a symbol over inbound `CALLS` edges (issue #126); daemon face of `eg query tests`. |

Partial-name symbol matching (`eg query symbols <PATTERN>`, issue #102) is
**CLI-only in this slice**: the daemon exposes no substring/glob symbol verb,
and `symbol_by_name` remains exact-match. A future daemon exposure would be a
purely additive verb (no change to existing verbs and no
`DAEMON_QUERY_SCHEMA_VERSION` break). See
[`docs/cli/query.md`](../cli/query.md#eg-query-symbols).

---

## 5.1 — Repository scope (`params.repo`, issue #67)

The code-oriented verbs (`symbol_by_name`, `symbol_at_commit`, `file_defines`,
`drift_top_n`, `semantic_search`) accept an optional `repo` param that
restricts the result set to exactly one repository in a shared multi-repo
store. The selector accepts:

- the stable `Repository` record ID, or
- a human-usable handle from the repository identity payload: the display
  name (`owner/name` for remote-derived identities), its final path segment
  (`name`), the basename / operator override, the normalized remote URL, the
  root commit SHA, or the canonical path.

Tombstoned `Repository` records are not selectable and never make a live
repository's selector ambiguous.

Unknown selectors fail with `unknown_repository_selector`; selectors matching
more than one repository fail with `ambiguous_repository_selector` (the
message lists every candidate). The daemon never picks a repository
implicitly.

Every returned row additionally carries the repository identity when the store
topology can attribute it:

```json
{ "repository_id": "codegraph:v4:...", "repository": "acme/widget" }
```

In an unscoped multi-repo collision the row set contains every matching
repository, each row disambiguated by these fields. Rows the topology cannot
attribute (legacy stores without `Repository` records) omit the fields rather
than guessing.

`repo` composes with the temporal selectors: `as_of.valid_time` and
`as_of.transaction_time` answer "which time view?", `repo` answers "which
repository?" — neither dimension widens the other. For
`as_of.transaction_time` the scope is applied to the record set *before*
temporal resolution (store-wide bounds stay global), so a forked repository's
descendant commits never drive the selected repository's removal or
supersession logic.

A scoped `file_defines` whose path also matches another repository reports the
exclusion through a result-level `diagnostics` array instead of mixing rows:

```json
"diagnostics": [{
  "code": "excluded_other_repositories",
  "repo_relative_path": "src/lib.rs",
  "excluded_repository_count": 1,
  "excluded_row_count": 2
}]
```

---

## 6 — Verb details

### `get_records`

Fetch graph records by stable ID. Returns one record object per resolved ID;
IDs not present in the store are silently omitted.

**Params:**
```json
{ "record_ids": ["codegraph:v3:abc123...", "codegraph:v3:def456..."] }
```

**Record shape:** full `GraphRecord` JSON as stored.

**Example:**
```json
// Request
{
  "request_id": "r1",
  "verb": "get_records",
  "params": { "record_ids": ["codegraph:v3:abc"] }
}

// Response result
{
  "verb": "get_records",
  "snapshot": "2026-05-19T10:00:00Z",
  "records": [{ "record_type": "node", "id": "codegraph:v3:abc", ... }],
  "page": { "cursor": null, "has_more": false, "returned": 1 }
}
```

---

### `symbol_by_name`

Look up Symbol nodes whose `name` field exactly matches the given value.
Results are sorted by `(span.start_line, record_id)`.

Optional `kind` filter (only `"Symbol"` is valid in v1).

When `as_of.valid_time` is set, returns — per repository — the record whose
valid-time is closest to and not after the instant (a single record in a
single-repository store; one per repository on a multi-repo collision, each
row carrying its repository identity).

When `as_of.transaction_time` is set (issue #66), returns — per stable record ID —
the version the store knew at or before the instant, excluding later corrections,
supersessions, and re-imports. Combine with `as_of.valid_time` to ask "what was
true at valid time V, as known by transaction time T." The row set is byte-equal
(after canonical ordering) to `eg query symbol <name> --tx-as-of <T> --graph
<same JSONL>`.

**Params:**
```json
{ "name": "nested::Widget", "kind": "Symbol" }
```

**Record shape** (parity with `eg query symbol`):
```json
{
  "record_id":          "codegraph:v3:...",
  "schema_version":     3,
  "name":               "nested::Widget",
  "kind":               "Symbol",
  "repo_relative_path": "src/lib.rs",
  "span":               { "start_line": 5, "end_line": 12 },
  "git_commit":         "abc1234..."
}
```

`git_commit` is present only for temporal (history-replay) records.
`span` is present only when the extractor produced line numbers.

**Invariant:** the `record_id` set returned by `symbol_by_name` is byte-equal
(after sorting) to the `record_id` set returned by `eg query symbol <name>
--graph <same JSONL>`.

---

### `symbol_at_commit`

Look up a Symbol node at a specific Git commit SHA or unique prefix.

Returns at most one record per repository (forked clones can share a commit
under distinct repository identities; each repository's best match is returned
with its identity attached rather than picking one implicitly). If the prefix
is ambiguous (matches > 1 commit), returns HTTP 400 `ambiguous_commit_prefix`.

**Params:**
```json
{ "name": "nested::Widget", "commit": "abc1234" }
```

**Record shape:** same as `symbol_by_name`.

---

### `resolve_record`

Resolves a cited `codegraph:vN:<suffix>` record-id handle to its live source
record with a drift verdict (issue #160) — the daemon face of
`eg query resolve --daemon`.

**Params:**
```json
{ "record_id": "codegraph:v1:…", "at": "abc1234", "as_of": "2026-03-02T00:00:00Z" }
```

`at` (commit SHA prefix) and `as_of` (RFC 3339 valid-time instant) are
optional and mutually exclusive. The selection and tombstone rules match the
local lanes exactly: forget tombstones keep the issue #231 temporal
exemption, repository-eviction tombstones suppress everywhere (issue #472),
and an ambiguous `at` prefix is rejected, never guessed.

**Record shape:** a single object with the citation fields
(`record_id`, `kind`, `name`, `repo_relative_path`, `span`, `git_commit`,
`valid_time`) plus `verdict` (`valid` | `drifted`; `drifted` adds `detail`,
`current_repo_relative_path`, and `current_span`).

**Errors:**

| Code | HTTP | When |
|------|------|------|
| `dangling_handle` | 404 | No record matches the handle in the requested view |
| `malformed_handle` | 400 | `record_id` is not a `codegraph:vN:<suffix>` handle |
| `unsupported_handle_domain` | 400 | Well-formed handle from another id domain |
| `ambiguous_commit_prefix` | 400 | `at` matches more than one commit |

---

### `file_defines`

List Symbol nodes whose `repo_relative_path` matches the given path, sorted by
`(span.start_line, record_id)`. Tombstoned (deleted) symbols are excluded from
current-state results.

**Params:**
```json
{ "repo_relative_path": "src/lib.rs" }
```

**Record shape:** same as `symbol_by_name`.

**Example:**
```json
// Request
{
  "request_id": "r2",
  "verb": "file_defines",
  "params": { "repo_relative_path": "src/lib.rs" }
}

// Response result (partial)
{
  "verb": "file_defines",
  "snapshot": "2026-05-19T10:00:00Z",
  "records": [
    { "record_id": "...", "schema_version": 3, "name": "nested::Widget", "kind": "Symbol", ... },
    { "record_id": "...", "schema_version": 3, "name": "nested::Runner", "kind": "Symbol", ... }
  ],
  "page": { "cursor": null, "has_more": false, "returned": 2 }
}
```

---

### `locate`

Positional query (issue #212): resolve the innermost `Symbol` containing a
1-based `file:line` and return the same full JSON envelope `eg query locate
--graph` emits — the located `symbol`, its outermost → innermost
`enclosing_chain`, and the trust-separated cross-domain bundle
(`source_facts`, `topology_edges`, `observations`, `project_state`,
`artifacts`, `verification_evidence`, `unresolved`). The answer is never a
nearest-symbol guess. The row `limit` is inapplicable (the answer is a single
envelope) and is ignored.

**Params:**
```json
{
  "repo_relative_path": "src/lib.rs",
  "line": 13,
  "repo": "optional repository selector (same contract as every other verb)",
  "at": "optional commit prefix (mutually exclusive with as_of)",
  "as_of": "optional RFC 3339 instant (mutually exclusive with at)",
  "supersession": "optional \"exclude\" (default) or \"include-but-flag\""
}
```

`at` / `as_of` reuse the same `file_symbols_at_point` machinery as the CLI
`--at` / `--as-of` flags, so the temporal + repository-collision contract is
identical. The request-level `as_of` selector is not applied: temporal pins
are verb params here. `line` must be a positive integer.

**Result shape:** `result.locate` is the `eg query locate` envelope verbatim.

**Typed errors:** positional failures are `ok:false` bodies whose `error.code`
matches the CLI's typed codes, so clients can re-emit the cold path's
machine-readable envelope:

| HTTP | `error.code` | Meaning |
|------|--------------|---------|
| 404 | `no_match` | No record carries the path in the selected view. |
| 404 | `no_enclosing_symbol` | The path is known but no symbol span contains the line. |
| 404 | `line_out_of_range` | The line is beyond the file's actual last line (`max_known_line` cited). |
| 404 | `missing_commit` / `no_commit_at_or_before` | The `at` / `as_of` pin resolved to nothing. |
| 400 | `ambiguous_repository` | Unscoped path exists in more than one repository; retry with `repo`. |
| 400 | `ambiguous_commit_prefix` | `at` matches more than one commit (`candidates` listed). |
| 400 | `malformed_timestamp` | `as_of` is not a valid RFC 3339 instant. |
| 400 | `empty_history` | Temporal pin on a history-less store. |

---

### `tests_for_symbol`

Map a symbol to the tests that exercise it before an edit (issue #126): the
daemon face of `eg query tests`. Walks inbound `CALLS` edges from the target
symbol and reports every extractor-stamped test symbol (`role == "test"`) that
can reach it, with the concrete connecting call path for each row.

**Params:**
```json
{
  "handle": "process_order",
  "max_depth": 5,
  "repo": "optional repository selector (same contract as every other verb)",
  "at": "optional commit prefix (mutually exclusive with as_of)",
  "as_of": "optional RFC 3339 instant (mutually exclusive with at)"
}
```

`handle` accepts a stable symbol record ID (`codegraph:vN:<hex>`) or an exact
symbol name; `max_depth` must be a positive integer (default 5). `at` /
`as_of` reuse the same commit-pin machinery as the CLI `--at` / `--as-of`
flags, so the temporal + repository-collision contract is identical. The
request-level `as_of` selector is not applied: temporal pins are verb params
here. The request-level row `limit` truncates the returned row set (the
`tests` summary always reports the full counts).

**Result shape:** `result.tests` is the `eg query tests` header verbatim
(resolved `target`, `direction: "inbound"`, `edge_labels: ["CALLS"]`,
`max_depth`, `total_covering_tests` / `direct_tests` / `transitive_tests`
counts, `truncation`, `diagnostics`, corpus-mode provenance, and the
reachability-lead disclaimer); the rows are returned as `result.records` with
the standard `page` envelope.

Traversal semantics (inbound `CALLS` only; `REFERENCES` excluded; non-test
callers traversed but never reported; hop 1 = `direct`, hop > 1 =
`transitive`; level-synchronized BFS with deterministic shortest paths;
`trust: "reachability_lead"` on every row) are identical to the CLI lane —
both faces run the same shared response computation. Rows are reachability
leads, not coverage proof; see [`docs/cli/tests.md`](../cli/tests.md).

**Typed errors:** failures are `ok:false` bodies whose `error.code` matches
the CLI's typed codes:

| HTTP | `error.code` | Meaning |
|------|--------------|---------|
| 404 | `no_match` | Handle resolves to no live record. |
| 404 | `stale_handle` | Handle names a tombstoned record. |
| 404 | `missing_commit` / `no_commit_at_or_before` | The `at` / `as_of` pin resolved to nothing. |
| 404 | `empty_history` | Temporal pin on a history-less store. |
| 400 | `ambiguous_handle` | Name matches more than one live symbol (`candidates` listed). |
| 400 | `ambiguous_commit_prefix` | `at` matches more than one commit. |
| 400 | `invalid_as_of_timestamp` | `as_of` is not a valid RFC 3339 instant. |
| 400 | `unsupported_handle` | Handle resolved to a non-symbol node. |
| 400 | `bad_request` | `max_depth` is not a positive integer, `at` and `as_of` are both set, or a param has the wrong JSON type. |

---

### `drift_top_n`

Return the top-N `SemanticDrift` nodes ranked by `score` descending.
`SemanticDrift` records are defined by
[`docs/schema/semantic-drift.md`](semantic-drift.md); `score` and
`selection_threshold` are JSON numbers.

**Params:**
```json
{ "limit": 10 }
```

`limit` defaults to 10 and is capped at 100.

**Record shape** (parity with `eg query drift`):
```json
{
  "record_id":                    "semantic:v1:...",
  "schema_version":               1,
  "before_commit":                "abc1234",
  "after_commit":                 "def5678",
  "before_valid_time":            "2026-05-21T00:00:00Z",
  "after_valid_time":             "2026-05-22T00:00:00Z",
  "prior_record_id":              "codegraph:v4:...",
  "target_record_id":             "codegraph:v4:...",
  "metric_kind":                  "cosine_distance",
  "score":                        0.92,
  "selection_threshold":          0.7,
  "selection_basis":              "threshold_only",
  "embedding_model_provider":     "aletheiadb_re_export",
  "embedding_model_name":         "sentence-transformers/all-MiniLM-L6-v2",
  "embedding_model_version":      "0.1.0",
  "embedding_model_dim":          384,
  "embedding_model_content_hash": "unknown",
  "repo_relative_path":           "src/lib.rs",
  "name":                         "nested::Widget"
}
```

`repo_relative_path` and `name` are omitted when not resolved (present only
when a `DriftsFrom` edge or `target_record_id` resolves to a Symbol or File
node with those fields set).

Coordination: issue #15 updates this response shape. Issue #10's future
`drift` verb should return the same record shape.

---

### `clone_classes`

Group exact-duplicate Rust symbol bodies into citable clone classes (issue
#216): the daemon face of `eg query clones`. Computed by the shared query
layer over the store's current-state codegraph records, so the daemon and the
local `--graph` / `--data-dir` transports agree.

**Params:**
```json
{ "limit": 50, "min_size": 2, "repo": "acme/widget" }
```

`limit` defaults to 50 and is capped at 500; `min_size` defaults to 2 and must
be at least 2 (both rejected as 400 otherwise). Honours `as_of.valid_time`
like `drift_top_n`: records newer than the instant are excluded from the
class computation.

**Record shape** — one record per clone class:

```json
{
  "content_hash":  "blake3:9f2c…",
  "size":          3,
  "members": [
    {
      "record_id":          "codegraph:v1:…",
      "qualified_name":     "a::compute",
      "repo_relative_path": "src/a.rs",
      "span":               {"start_byte":0,"end_byte":42,"start_line":1,"end_line":4}
    }
  ]
}
```

No raw source body is emitted — only the shared normalized-content hash plus
bounded citable handles. Classes order by `size` descending, then
`content_hash` ascending; members by `record_id` ascending. The full report
(counts, truncation signal, empty reason) also rides along under
`result.report`, and `page.returned` counts the returned classes; `truncated`
in the report says whether `limit` cut the class list (issue #121). A store
with no clone classes yields a well-formed empty `records` array with
`report.empty_reason == "no_clone_classes"`, not an error (issue #196).

---

### `semantic_search`

Natural-language code search over the shared store's embedding index, routed
through the daemon (issue #59). The daemon performs the same vector similarity
search the embedded `eg query semantic` path uses; the **client supplies the
query embedding vector**. No embedding model is loaded daemon-side, no remote
embedding service is contacted, and there is no background indexing — the verb
reuses the existing semantic ingest/query behavior. The embedding provider,
vector model, and ranking algorithm are unchanged (issues #58, #15 own those).

**Params:**
```json
{ "query_vector": [0.0123, -0.0481, ...], "limit": 10 }
```

| Field          | Type    | Required | Notes |
|----------------|---------|----------|-------|
| `query_vector` | [number]| yes      | Dense query embedding. Non-empty; all entries finite. Width must equal the store's index dimensionality. |
| `limit`        | u64     | no       | Result ceiling. Default 10, capped at 100; further bounded by `budget.max_results`. |

To obtain `query_vector`, embed the query text with the same default local
model used by ingest (`sentence-transformers/all-MiniLM-L6-v2`). The
`eg query semantic --daemon` CLI does this for you.

**Record shape** (parity with `eg query semantic`):
```json
{
  "record_id":          "codegraph:v4:...",
  "name":               "nested::Widget::new",
  "repo_relative_path": "src/lib.rs",
  "score":              0.83,
  "span":               { "start_line": 5, "end_line": 12, "start_byte": 80, "end_byte": 240 }
}
```

`record_id` and `score` are always present. `name`, `repo_relative_path`, and
`span` are omitted when the underlying node lacks them — the same documented
absent-span rule as the embedded semantic CLI contract. Rows are bounded
**retrieval leads**, not proof: they carry record IDs, scores, repo-relative
paths, and spans only. They are never raw transcript text, command output,
patch hunks, issue/comment bodies, environment values, tokens, or protected
artifact payloads, and they are not classified as verification evidence, task
completion, or agent memory. Confirm a lead with `eg query symbol`,
`eg query context`, or by reading the source.

**Determinism:** for a fixed store and fixed `query_vector`, results are
order-stable across repeated runs and match the embedded `eg query semantic`
top-k for the same store, after canonical ordering. Daemon and embedded read
the same persisted index, so scores agree within a tight tolerance (≤ 1e-4).

**Confidence verdict (issue #221):** every non-empty result carries a
top-level `confidence` object stamped by the daemon itself, so MCP consumers
inherit the same contract as the CLI without client-side derivation:

```json
"confidence": {"verdict":"confident","best_score":0.83,"total_candidates":42,"confident_threshold":0.39,"weak_threshold":0.34,"selection_basis":"corpus_calibrated_confidence_floor"}
```

`verdict` is one of `confident` (best score `>= 0.39`), `weak` (best score in
`[0.34, 0.39)`), `abstain` (best score `< 0.34`). `best_score` is the highest
score among the ranked candidates; `total_candidates` covers the
pre-`limit` pool. Rows below the confident threshold are still returned —
flagged per-row via the embedded lane's `confidence_band` — never silently
dropped. An empty pool carries no `confidence` object: the empty answer is
the CLI's exit-2 no-match path, not an abstention. The verdict is a pure
function of the ranked score distribution, so repeated identical verbs yield
byte-identical verdicts. The CLI's `--daemon` path forwards the daemon's
verdict verbatim (re-serialized through the shared struct for canonical field
order); against a pre-#221 daemon it derives the verdict client-side from the
returned rows. Full semantics in [`docs/cli/query.md`](../cli/query.md)
("Confidence verdicts").

**Diagnostics:**

| Condition | Code | HTTP |
|-----------|------|------|
| `params.query_vector` absent | `missing_field` | 400 |
| `query_vector` not an array / empty / non-finite; `limit` not an integer | `bad_request` | 400 |
| Store has no embedding index | `missing_semantic_index` | 422 |
| Embedding index present on disk but skipped at load | `semantic_index_unreadable` | 422 |
| `query_vector` width ≠ index dimensionality | `incompatible_embedding_dimension` | 422 |
| Budget `timeout_ms` elapsed | `query_timeout` | 408 |
| Missing/invalid bearer token | `unauthorized` | 401 |
| Daemon built without the `embeddings` feature | `not_implemented` | 501 |

A **no-match** is a successful empty result (`ok:true`, `records: []`,
`page.returned: 0`), never a silent fallback to direct embedded reads or raw
store internals. Missing-daemon and stale-runtime-metadata conditions are
surfaced by the shared discovery contract in
[`daemon-runtime.md`](daemon-runtime.md) before the verb is dispatched.

When to reach for this verb vs. structural reads is covered in
[`docs/cli/semantic-search-guidance.md`](../cli/semantic-search-guidance.md),
including how this slice relates to issue #58's relevance gate.

### `agent_sessions_for_repo`

Repo-scoped, recency-ordered digest of the recent agent sessions recorded for
one repository (issue #112) — the daemon face of `eg query sessions <REPO>`.
The formerly reserved verb, now implemented. The full behavioral contract
(membership, scope derivation, ordering, run-outcome honesty, redaction) lives
in [`docs/cli/agent-sessions.md`](../cli/agent-sessions.md); the daemon result
serializes the identical core digest value the CLI envelope carries, so the
two transports cannot drift.

**Params:**
```json
{ "repo": "repo-a", "limit": 20 }
```

| Field   | Type   | Required | Notes |
|---------|--------|----------|-------|
| `repo`  | string | yes      | Repository selector (record ID, basename, remote URL, root commit SHA, canonical path, or final-segment shorthand). `repository_id` is accepted as an alias; `repo` wins when both are present. A non-string value is rejected naming the key the caller actually sent, never the canonical key it would be rewrapped under. |
| `limit` | integer | no      | Session-row ceiling. Default 20, capped at 200. A well-formed integer outside `1..=200` — negative included — is `invalid_limit`, mirroring the CLI. A shape that is not an integer at all (a fractional float such as `1.5`, a string, ...) is `bad_request`. KNOWN GAP: an integer literal so large it overflows both `i64` and `u64` (e.g. `18446744073709551616`) is, once parsed, indistinguishable from an ordinary whole-valued float (`1.0`, `2e0`) without the `arbitrary_precision` serde_json feature this crate does not enable, so both land in `bad_request` rather than the former being reclassified as `invalid_limit` — an honest limitation, not a guess. Further bounded by the request's `budget.max_results`: the effective cap is the minimum of the two, and `results_truncated` reports the cap that actually applied — including `limit: 0` (an empty row set is still `200`, never conflated with `no_sessions`, which means the repository has none at all). The traversal deadline is re-checked after the digest is built, so a tight `budget.timeout_ms` yields `query_timeout`, never a late 200. |

This lane has **no temporal selectors** in this slice (matching the CLI, which
carries neither `--at` nor `--as-of`): a request naming `as_of.valid_time` is
rejected outright with `not_implemented` rather than silently answering the
current-state digest for an unrequested — or malformed — point in time.
`as_of.transaction_time` and `as_of.since` are already rejected globally for
every verb but `symbol_by_name` (see §2).

**Result shape** (parity with the `eg query sessions` envelope):
```json
{
  "verb": "agent_sessions_for_repo",
  "repository_id": "codegraph:v9:...",
  "repository": "repo-a",
  "disclaimer": "rows are recorded agent-authored memory and project-state facts; ...",
  "unsupported_count_kinds": ["lesson"],
  "sessions": [ /* SessionRow objects, ordered by last_activity desc */ ],
  "diagnostics": [ /* sorted machine-readable diagnostics */ ]
}
```

Every session row is trust-labeled `agent_authored` (an agent claim, never
verification); referenced tasks carry `project_state`. Rows are allow-list
only — record IDs, handles, time fields, status/outcome enums, bounded counts,
structured summary labels, and BLAKE3 hashes; never raw transcript text,
command output, patch hunks, tool-call arguments, or task titles.

**Determinism:** for a fixed store, repeated calls return byte-identical
`sessions` and `diagnostics`, matching the CLI answer over the same records.

**Diagnostics:**

| Condition | Code | HTTP |
|-----------|------|------|
| Both `params.repo` and `params.repository_id` absent | `missing_field` | 400 |
| Selector present but not a string | `bad_request` | 400 |
| `limit` not a well-formed integer (a float, a string, ...) | `bad_request` | 400 |
| `limit` a well-formed integer outside 1..=200 (negative included; NOT a literal wider than `u64`, which is indistinguishable from a whole-valued float and stays `bad_request` — see the params table above) | `invalid_limit` | 400 |
| `as_of.valid_time` present | `not_implemented` | 501 |
| Selector matches no repository | `unknown_repository_selector` | 400 |
| Selector matches several repositories | `ambiguous_repository_selector` | 400 |
| Budget `timeout_ms` elapsed (checked before AND after building the digest) | `query_timeout` | 408 |
| Missing/invalid bearer token | `unauthorized` | 401 |

A repository that resolves but has **zero** scoped sessions is a successful
`200` carrying `sessions: []` plus a `no_sessions` diagnostic — an empty digest
is an answer, never a `404` and never a fabricated row. Sessions with no
derivable repository scope surface under `unresolved_repository_scope`.

### `observations_for_symbol` / `criteria_for_task` — the `trust` field (issue #114)

The two cross-domain verbs return code facts, agent observations, project
state, and verification evidence side by side. Every record in every section of
those results carries a single `trust` field drawn from a closed vocabulary, so
a consuming agent can weight a verified fact above an unverified hypothesis
without re-traversing the graph.

The vocabulary and the full derivation rules are documented once in
[`docs/cli/query.md` § Trust class](../cli/query.md#trust-class-trust-issue-114).
The summary:

| `trust` | Meaning |
|---|---|
| `source_derived` | Deterministic code-graph or semantic-derived fact |
| `verification_evidence` | A recorded verification execution |
| `agent_verified` | Agent claim citing a live **passing** verification record |
| `agent_unverified` | Agent claim with no such citation |
| `agent_contradicted` | Agent claim displaced by a live `CONTRADICTS`/`SUPERSEDES` |
| `project_state` | Imported external work state |
| `artifact` | Produced bytes |
| `runtime_observation` | A program's own claim about its execution |
| `other` | No derivation rule applies |

`domain` says *where a record lives*; `trust` says *how much weight it has
earned*. The label is a deterministic function of the record's node kind plus
its evidence and contradiction edges at the queried snapshot, so repeating a
query over an unchanged store yields byte-identical labels.

**Parity:** the daemon and `eg query context` / `eg query task` call the same
derivation, so the *rule* cannot drift between transports. Caveat: the CLI lanes
derive over their corpus-filtered slice (`--at-head` / `--all-history`) while
this surface has no corpus selector — same rule, potentially a different
snapshot.

**Versioning:** adding `trust` is an **additive** change under §9 — a new
response field, with no existing field renamed, removed, or reshaped — so
`DAEMON_QUERY_SCHEMA_VERSION` stays `1`. This matches the precedent set when
issue #108 added the whole `drift_history` section to `observations_for_symbol`
without a bump.

The `unresolved` section carries no `trust`: its entries describe an evidence
link whose target record is **absent**, and there is no record to classify. The
`excluded` diagnostics section does carry `trust`, since those entries name real
records the answer withheld.

---

## 7 — CLI mapping

`eg query symbol`, `eg query file`, `eg query drift`, and `eg query semantic`
dispatch through the daemon when `--daemon` is given:

```sh
eg query symbol nested::Widget       --daemon --data-dir .egregore
eg query file src/lib.rs             --daemon --data-dir .egregore
eg query drift                       --daemon --data-dir .egregore
eg query semantic "where is the parser" --daemon --data-dir .egregore
eg query symbol nested::Widget       --daemon --data-dir .egregore --at abc1234
eg query symbol nested::Widget       --daemon --data-dir .egregore --as-of 2026-05-01T00:00:00Z
```

For `symbol`/`file`/`drift`, `--daemon` requires `--data-dir` (clap enforces
this). `eg query semantic` always takes `--data-dir`; adding `--daemon` routes
the query through the daemon. `eg query semantic --daemon` embeds the query
text locally, then sends only the resulting vector to the daemon. Output format
matches the non-daemon path: one JSON object per line.

---

## 8 — Pagination rules (v1)

v1 returns full-page results only:
- `page.cursor` is always `null`
- `page.has_more` is always `false`
- `page.returned` equals `len(records)`

Cursor-based pagination is reserved for a future slice.

---

## 9 — Versioning rules

`DAEMON_QUERY_SCHEMA_VERSION = 1` is a Rust constant in `src/daemon.rs`.

- Additive changes (new verbs, new response fields — whether optional or always
  present) do not bump the version. A consumer reading version 1 still finds
  every version-1 field, unrenamed and unreshaped; JSON consumers tolerate keys
  they do not know. The looser summary at the top of this document ("changes
  that alter the response shape") is subordinate to this rule.
- Breaking changes (renamed verbs, removed fields, changed `record` shapes for
  existing verbs) require bumping `DAEMON_QUERY_SCHEMA_VERSION` and updating
  this document and `daemon-api.md`.

---

## 10 — Capability discovery (`GET /v1/capabilities`, issue #166)

`GET /v1/capabilities` is the daemon's self-description endpoint: one request
tells a client the full live verb set and the query schema version, with zero
`not_implemented` round-trips. It requires no knowledge of the verb set to
call (plain GET route, Bearer <redacted> like `GET /v1/status`), and exists so
integration bridges (the MCP stdio bridge, SDKs) can derive their tool list
from the running daemon instead of hard-coding it from this document.

### Contract

Flat JSON, HTTP 200, no `ok`/`result` envelope (same shape as the
observability endpoints in `daemon-api.md` §8):

```json
{
  "api_version": "v1",
  "daemon_query_schema_version": 1,
  "verbs": [
    { "name": "agent_sessions_for_repo", "status": "implemented" },
    { "name": "clone_classes",           "status": "implemented" },
    { "name": "criteria_for_task",       "status": "implemented" },
    { "name": "drift",                   "status": "reserved" },
    { "name": "drift_top_n",             "status": "implemented" },
    { "name": "file_defines",            "status": "implemented" },
    { "name": "get_records",             "status": "implemented" },
    { "name": "locate",                  "status": "implemented" },
    { "name": "observations_for_symbol", "status": "implemented" },
    { "name": "semantic_search",         "status": "implemented" },
    { "name": "symbol_at_commit",        "status": "implemented" },
    { "name": "symbol_by_name",          "status": "implemented" }
  ],
  "accepted_record_tuples": [
    { "domain": "agent_memory", "kind": "*", "schema_version": 1 },
    { "domain": "artifact",     "kind": "*", "schema_version": 1 },
    { "domain": "codegraph",    "kind": "*", "schema_version": 1 },
    { "domain": "codegraph",    "kind": "*", "schema_version": 2 }
  ]
}
```

| Field | Type | Stability |
|-------|------|-----------|
| `api_version` | string | Always `"v1"` while this route lives under `/v1/` |
| `daemon_query_schema_version` | integer | The value of the `DAEMON_QUERY_SCHEMA_VERSION` Rust constant in `src/daemon.rs` |
| `verbs` | array of `{name: string, status: string}` | Sorted by `name`. `status` is exactly `implemented` or `reserved` — the closed set never gains a third value without a `/v2/` API prefix change |
| `accepted_record_tuples` | array of `{domain: string, kind: string, schema_version: integer}` | Sorted by `(domain, schema_version)`. Every tuple the daemon accepts on ingest/read |

### Semantics

- **Verbs.** The `verbs` array is built from the same
  `QUERY_VERB_TABLE` registry in `src/daemon.rs` that `POST /v1/query`
  dispatches through — adding a verb to the daemon means adding a registry
  row, so the manifest cannot drift from reality. `implemented` means the
  verb is callable in *this* daemon build; `reserved` means it answers HTTP
  501 `not_implemented`. `semantic_search` is `implemented` only in builds
  with the `embeddings` feature, `reserved` otherwise.
- **Record tuples.** `accepted_record_tuples` enumerates the
  `(domain, kind, schema_version)` tuples the daemon's reader-side gate
  (`is_known_record_version` in `src/schema_version.rs`) accepts — the same
  gate that produces `unknown_schema_version` on ingest/read. A client
  holding a record whose `(domain, schema_version)` pair is absent from this
  list would be rejected, and can detect that before sending.
- **`kind: "*"`** is a wildcard, not a literal kind. The gate checks domain +
  schema_version only, never the record kind, so the wildcard is the honest
  contract: any kind under a listed domain + version is accepted. Match your
  tuple by domain + schema_version and treat `"*"` as any-kind.
- **Not store inspection.** This endpoint reports what the daemon
  *supports/accepts*, never which tuples are *present* in the store (store
  inspection is tracked by issue #125).
