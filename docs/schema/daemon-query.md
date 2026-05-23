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
| `transaction_time` | reserved    | Always returns HTTP 501 `not_implemented` |
| `since`            | reserved    | Always returns HTTP 501 `not_implemented` |

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
| `missing_field`         | 400  | `verb` is absent or blank |
| `bad_request`           | 400  | Unknown verb, or required `params` field missing |
| `ambiguous_commit_prefix` | 400 | `symbol_at_commit` prefix matches > 1 commit |
| `not_implemented`       | 501  | Reserved verb, or `as_of.transaction_time` / `as_of.since` set |
| `query_timeout`         | 408  | Budget `timeout_ms` elapsed |
| `internal_error`        | 500  | Store read failed |

---

## 5 — Verb table

| Verb                    | Status      | Params                        | Notes |
|-------------------------|-------------|-------------------------------|-------|
| `get_records`           | implemented | `record_ids: [string]`        | Batch read by stable ID |
| `symbol_by_name`        | implemented | `name: string`, `kind?: string` | Exact name match; honours `as_of.valid_time` |
| `symbol_at_commit`      | implemented | `name: string`, `commit: string` | Prefix-safe commit lookup |
| `file_defines`          | implemented | `repo_relative_path: string`  | Symbols defined in a file |
| `drift_top_n`           | implemented | `limit?: u64` (default 10, max 100) | SemanticDrift records ranked by score |
| `drift`                 | reserved    | same as `drift_top_n`         | Reserved for issue #10; returns `not_implemented` until wired. |
| `observations_for_symbol` | reserved  | —                             | Returns `not_implemented` |
| `agent_sessions_for_repo` | reserved  | —                             | Returns `not_implemented` |
| `criteria_for_task`      | reserved  | `task_id: string`              | Future project-graph query over [`docs/schema/project-graph.md`](project-graph.md); returns `not_implemented` until wired. |

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

When `as_of.valid_time` is set, returns the single record whose valid-time is
closest to and not after the instant.

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

Returns at most one record. If the prefix is ambiguous (matches > 1 commit),
returns HTTP 400 `ambiguous_commit_prefix`.

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

## 7 — CLI mapping

`eg query symbol`, `eg query file`, and `eg query drift` dispatch through the
daemon when `--daemon` is given:

```sh
eg query symbol nested::Widget --daemon --data-dir .egregore
eg query file src/lib.rs       --daemon --data-dir .egregore
eg query drift                 --daemon --data-dir .egregore
eg query symbol nested::Widget --daemon --data-dir .egregore --at abc1234
eg query symbol nested::Widget --daemon --data-dir .egregore --as-of 2026-05-01T00:00:00Z
```

`--daemon` requires `--data-dir` (clap enforces this). Output format matches
the non-daemon path: one JSON object per line, sorted by `(span.start_line,
record_id)`.

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

- Additive changes (new verbs, new optional response fields) do not bump the version.
- Breaking changes (renamed verbs, removed fields, changed `record` shapes for
  existing verbs) require bumping `DAEMON_QUERY_SCHEMA_VERSION` and updating
  this document and `daemon-api.md`.
