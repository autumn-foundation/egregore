# `POST /v1/query` — Verb-Dispatch Query Surface

**Schema version:** 1 (`DAEMON_QUERY_SCHEMA_VERSION`)
**Status:** Implemented. **Breaking** changes to the response shape for existing
verbs, verb renames, and verb removals require a new `schema_version`; purely
**additive** changes do not. See §9 for the exact split.

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
| `ambiguous_commit_prefix` | 400 | `symbol_at_commit` prefix matches > 1 commit |
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

| Verb                    | Status      | Params                        | Notes |
|-------------------------|-------------|-------------------------------|-------|
| `get_records`           | implemented | `record_ids: [string]`        | Batch read by stable ID |
| `symbol_by_name`        | implemented | `name: string`, `kind?: string`, `repo?: string` | Exact name match; honours `as_of.valid_time` |
| `symbol_at_commit`      | implemented | `name: string`, `commit: string`, `repo?: string` | Prefix-safe commit lookup |
| `file_defines`          | implemented | `repo_relative_path: string`, `repo?: string` | Symbols defined in a file |
| `drift_top_n`           | implemented | `limit?: u64` (default 10, max 100), `repo?: string` | SemanticDrift records ranked by score |
| `semantic_search`       | implemented | `query_vector: [f32]`, `limit?: u64` (default 10, max 100), `repo?: string` | Natural-language code search over the shared store's embedding index. Requires the `embeddings` feature. |
| `drift`                 | reserved    | same as `drift_top_n`         | Reserved for issue #10; returns `not_implemented` until wired. |
| `observations_for_symbol` | implemented | `name: string`, `supersession?: string` | Cross-domain symbol context (issue #86); parity with `eg query context`. Every record row carries `trust_class` + a derived `trust` label (issue #114, see below). |
| `agent_sessions_for_repo` | implemented | `repo: string` (alias `repository_id`), `limit?: u64` (default 20, max 200) | Repo-scoped recency digest of recent agent sessions (issue #112); daemon face of `eg query sessions`. |
| `criteria_for_task`      | implemented | `task_id: string`            | Task acceptance-criteria/evidence context over [`docs/schema/project-graph.md`](project-graph.md); daemon face of `eg query task`. Every record row carries `trust_class` + a derived `trust` label (issue #114). |

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

### `observations_for_symbol` — trust labels (issue #114)

Every **record row** in the `source_facts`, `observations`, `project_state`,
`artifacts`, `verification_evidence`, `drift_history`, and `excluded` sections
carries two labels:

| Field | Question | Vocabulary |
|-------|----------|------------|
| `trust_class` | *Where does this record live?* (provenance domain) | `source_fact`, `agent_authored`, `verification_evidence`, `project_state`, `artifact`, `runtime_observation`, `other` |
| `trust` | *How much uncorroborated agent judgement must I accept?* | `source_derived`, `verification_evidence`, `agent_verified`, `agent_unverified`, `agent_contradicted` |

Both are **additive** fields on the response payload — new keys inside
`result`, which §9 and [`daemon-api.md`](daemon-api.md) both classify as
non-breaking, so there is no `api_version` bump and no
`DAEMON_QUERY_SCHEMA_VERSION` break. Note they are **always present**, not
optional: a row never omits them.

`trust` is derived by the same function `eg query context` calls, over the same
record slice the verb resolved (including any `as_of.valid_time` narrowing), so
**the daemon and the CLI cannot disagree on a label**. Derivation rules, the
qualifying-link conditions, the passing-status table, and the determinism
guarantees are specified once in
[`docs/schema/trust-labels.md`](trust-labels.md).

The label is computed **before** the `supersession` partition, so it is a
property of the record and not of the rendering mode: under the default
`exclude` a contradicted observation moves into `excluded` and keeps its
`agent_contradicted` label; under `include-but-flag` the same record carries the
same label inside `observations`.

`topology_edges` and `unresolved` deliberately carry **no** `trust` field — an
edge is a relation rather than a record, and an unresolved row names a target
that is absent from the store, so there is nothing to classify.

The same two fields appear on `criteria_for_task` rows (including a nested
`verification_record`).

**Determinism:** for a fixed store, repeated calls return byte-identical labels.

---

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
  "repository_id": "codegraph:v8:...",
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

- Additive changes (new verbs, new response fields — whether optional or
  always-present) do not bump the version. Adding a key to an existing verb's
  rows is additive; changing or removing an existing key is not.
- Breaking changes (renamed verbs, removed fields, changed `record` shapes for
  existing verbs) require bumping `DAEMON_QUERY_SCHEMA_VERSION` and updating
  this document and `daemon-api.md`.
