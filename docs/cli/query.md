# eg query

Query an existing graph JSONL for symbols, files, semantic drift records, or by natural-language similarity.

## Synopsis

```text
eg query symbol   <NAME>  --graph <PATH>    [--at <COMMIT>] [--format json|text]
eg query symbol   <NAME>  --data-dir <DIR>  [--at <COMMIT>] [--format json|text]
eg query file     <PATH>  --graph <PATH>    [--format json|text]
eg query file     <PATH>  --data-dir <DIR>  [--format json|text]
eg query drift            --graph <PATH>    [--limit N] [--format json|text]
eg query drift            --data-dir <DIR>  [--limit N] [--format json|text]
eg query semantic <QUERY> --data-dir <DIR>  [--limit N] [--format json|text]
eg query context  <NAME>  --graph <PATH>
eg query task     <HANDLE> --graph <PATH>
eg query memory   <HANDLE> --graph <PATH>   [--verified-only]
```

Evidence-backed audit subcommands have their own pages:

- `eg query context` — evidence-backed context for a **symbol** (issue #38).
- `eg query task` — evidence for a **task** ([task-queries.md](task-queries.md), issue #48).
- `eg query memory` — audit the evidence behind one **agent-authored memory
  claim** ([memory-audit.md](memory-audit.md), issue #64).

Most subcommands accept exactly one input source:

- `--graph <PATH>` — read from a JSONL file produced by `eg scan` or `eg scan-history`.
- `--data-dir <DIR>` — read from an embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. Requires the `embedded-aletheiadb` feature (enabled by default). Providing both `--graph` and `--data-dir` is an error.

`eg query semantic` accepts **only** `--data-dir`. The store must additionally have been populated with the `--embed` flag (`eg ingest --adapter embedded --data-dir <DIR> --embed`); a store without embeddings returns no results.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one result was found and printed. |
| `1` | An error occurred (missing file, malformed JSONL, ambiguous commit prefix). A single-line message is written to stderr. No partial JSON appears on stdout. |
| `2` | No match found. A single-line message is written to stderr. Stdout is empty. |

## Output format

### `--format json` (default)

One JSON object per line (JSONL). Field names are stable across releases. Machine consumers should depend only on the fields documented here; additional fields may be added later.

### `--format text`

One human-readable line per result for terminal use. The exact format is not stable and must not be parsed by scripts.

---

## eg query symbol

Find `Symbol` nodes by name.

```text
eg query symbol <NAME> --graph <PATH> [--at <COMMIT>] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<NAME>` | yes | Exact symbol name to look up. |
| `--graph <PATH>` | yes | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--at <COMMIT>` | no | Restrict to the single best record whose `git_commit` starts with this SHA prefix. Exit `1` with `error: ambiguous commit prefix` when the prefix matches more than one distinct commit SHA. Requires a history graph. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable BLAKE3-based record ID (`codegraph:v1:…`). |
| `schema_version` | number | yes | Record schema version for the returned `Symbol`; see [`docs/schema/schema-versioning.md`](../schema/schema-versioning.md). |
| `name` | string | yes | Symbol name. |
| `kind` | string | yes | Always `"Symbol"`. |
| `repo_relative_path` | string or null | yes | Repository-relative file path, e.g. `"src/lib.rs"`. |
| `span` | object or null | yes | Source span with `start_byte`, `end_byte`, `start_line`, `end_line`. |
| `git_commit` | string | only in history graphs | Full commit SHA for history-backed records. |

### Example

```sh
eg scan . --out g.jsonl
eg query symbol scan_repository --graph g.jsonl
```

```json
{"record_id":"codegraph:v1:abc...","schema_version":1,"name":"scan_repository","kind":"Symbol","repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":500,"start_line":51,"end_line":71}}
```

---

## eg query file

List all `Symbol` nodes defined in a file, resolved through `DEFINES` edges.

```text
eg query file <PATH> --graph <PATH> [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATH>` | yes | Repository-relative file path, e.g. `src/lib.rs`. |
| `--graph <PATH>` | yes | Graph JSONL to query. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

Same fields as `eg query symbol` (see above). Results are sorted by `span.start_line` ascending, then `record_id`.

---

## eg query drift

Find `SemanticDrift` nodes ranked by cosine-distance score descending.

```text
eg query drift --graph <PATH> [--limit N] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--graph <PATH>` | yes | Graph JSONL produced by `eg scan-history` after semantic drift computation. |
| `--limit N` | no | Maximum results (default `10`). |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable record ID for the `SemanticDrift` node. |
| `schema_version` | number | yes | Record schema version for the returned `SemanticDrift`; see [`docs/schema/schema-versioning.md`](../schema/schema-versioning.md). |
| `before_commit` | string | yes | Commit SHA for the earlier embedding. |
| `after_commit` | string | yes | Commit SHA for the later embedding. |
| `before_valid_time` | string | yes | Valid time for the earlier embedding. |
| `after_valid_time` | string | yes | Valid time for the later embedding. |
| `prior_record_id` | string | yes | Prior codegraph File/Symbol record ID. |
| `target_record_id` | string | yes | Later codegraph File/Symbol record ID. |
| `metric_kind` | string | yes | Drift metric, e.g. `cosine_distance`. |
| `score` | number | yes | Cosine distance as a JSON number. Higher = more drift. |
| `selection_threshold` | number | yes | Threshold that selected this drift record. |
| `selection_basis` | string | yes | Selection policy, e.g. `threshold_only`. |
| `embedding_model_provider` | string | yes | Provider or boundary that supplied the model. |
| `embedding_model_name` | string | yes | Embedding model name. |
| `embedding_model_version` | string | yes | Pinned model version. |
| `embedding_model_dim` | number | yes | Embedding dimension. |
| `embedding_model_content_hash` | string | yes | Model content hash or `unknown`. |
| `repo_relative_path` | string or null | when resolvable | Path of the drift target, resolved from `DRIFTS_FROM` edges. |
| `name` | string or null | when resolvable | Name of the drift target. |

Ties in `score` are broken by `record_id` ascending.

---

## eg query semantic

Find code nodes by natural-language similarity using dense vector embeddings.

```text
eg query semantic <QUERY> --data-dir <DIR> [--limit N] [--format json|text]
```

The query string is embedded with the same model used during ingest and compared against stored vectors using cosine similarity. Results are returned in descending similarity order.

The embedded store **must** have been populated with `eg ingest --embed`. A store created without `--embed` contains no embedding vectors and returns no results.

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<QUERY>` | yes | Natural-language search text, symbol name, or code snippet. |
| `--data-dir <DIR>` | yes | Embedded `AletheiaDB` store created by `eg ingest --adapter embedded --embed`. `--graph` is not accepted by this subcommand. |
| `--limit N` | no | Maximum number of results (default `10`). |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

One JSON object per line (JSONL). The default output format is `json`. Field names are stable across releases.

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable BLAKE3-based record ID (`codegraph:v1:…`). Safe to cite, log, and pass to other `eg` commands. |
| `score` | number | yes | Cosine similarity score (0.0–1.0). Higher = more similar to the query. |
| `name` | string | when available | Symbol or file name from the matched record. Absent when the record has no name field. |
| `repo_relative_path` | string | when available | Repository-relative file path, e.g. `"src/lib.rs"`. Absent when the record has no path field. |
| `span` | object | when available | Source span: `start_byte`, `end_byte`, `start_line`, `end_line` (all integers). Absent when the record has no span. |

Machine consumers must depend only on the fields listed above. Additional fields may be added in future releases; removing or renaming any of the fields above constitutes a breaking contract change and requires a version bump.

### No-result and missing-embedding behavior

| Condition | Exit code | Stderr message | Operator action |
|-----------|-----------|----------------|-----------------|
| Store directory does not exist | `1` | `embedded store not found … run ingest` | Create the store: `eg ingest --adapter embedded --data-dir <DIR> [--embed]`. |
| `semantic_search` returns no matches | `2` | `no results — store may not have embeddings (re-run ingest with --embed)` | Re-run ingest: `eg ingest --adapter embedded --data-dir <DIR> --embed`. |

Exit code `2` is also returned when the query produced no cosine-similar results above the search threshold. This is semantically equivalent to "no match" in other query subcommands.

### `--format text`

`--format text` emits one human-readable line per result for terminal use, for example:

```text
scan_repository score=0.9500 @ src/lib.rs:51
```

The exact format of `--format text` output is **not stable** and must not be parsed by scripts or agents. Use `--format json` for machine-readable output with stable field names.

### Example

```sh
eg ingest graph.jsonl --adapter embedded --data-dir .egregore-semantic --embed
eg query semantic "write nodes to database storage" --data-dir .egregore-semantic
```

```json
{"record_id":"codegraph:v1:abc123","name":"EmbeddedAletheiaSink::write_record","repo_relative_path":"src/sink/embedded.rs","score":0.9231,"span":{"start_byte":4096,"end_byte":5200,"start_line":142,"end_line":168}}
```

The `record_id` is stable across re-scans of the same commit and can be cited in agent-memory records. The `repo_relative_path` and `span` together give a file and line-range handle that agents can pass directly to editor tools or other `eg` commands.
