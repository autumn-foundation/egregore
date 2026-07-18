# eg query who-imports

List the files that import a module path — "who depends on this module?" — as
a precise, citable importer inventory over the `Import` nodes the language
extractors already mint (issue #444).

## Synopsis

```text
eg query who-imports <MODULE-PATH> --graph <PATH>   [--crate <NAME>] [--repo <SELECTOR>] [--format json|text]
eg query who-imports <MODULE-PATH> --data-dir <DIR> [--crate <NAME>] [--repo <SELECTOR>] [--format json|text]
```

This is a read-only lookup: it never re-parses source and never scans comment
or string text. Because only extractor-minted `Import` nodes are considered, a
`///` doc-comment or a string literal that mentions the module path produces
**no** match — the precision win over `grep`, which cannot tell a real `use`
declaration from a mention of the same characters.

Rows are import-site **leads, not proof of use**: an `Import` node records that
a file's `use` declaration names the module path, never that the imported item
is exercised at runtime.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<MODULE-PATH>` | yes | A `::`-separated module path (`serde`, `foo::bar`, `crate::query::liveness`). Empty, leading/trailing `::`, an empty interior segment (`a::::b`), or a whitespace-bearing segment is rejected (exit `1`). |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. Read from a throwaway copy — the store is never mutated. |
| `--crate <NAME>` | no | Unify a leading `crate::` with this crate name (see [crate-unification boundary](#crate-unification-boundary)). |
| `--repo <SELECTOR>` | no | Restrict the importer set to one repository in a multi-repo store (see [query.md](query.md#repository-scope---repo-issue-67)). |
| `--format` | no | `json` (default) or `text`. |

## Semantics

- **Segment-aware prefix matching.** Each live `Import` node's path text is
  reduced to a module-path segment list — a trailing ` as <alias>` rename is
  stripped, a group import `a::b::{C, D}` is reduced to its common module
  prefix `a::b`, a glob `a::b::*` is reduced to `a::b` — then split on `::`.
  An import **matches** iff the query segment sequence equals the import's
  first *N* segments. So `foo::bar` matches `foo::bar::Baz` and `foo::bar`,
  but **never** `foo::barbell`: segment boundaries are respected, so no
  sibling-path bleed.
- **Alias imports match on the path, not the alias.** `use serde::Serialize as
  S` matches `serde` and `serde::Serialize`, never `serde::Serialize::S` — the
  bound alias is not part of the module path.
- **Group and glob imports match their common module prefix.**
  `use foo::bar::{Qux, Quux}` and `use foo::bar::*` each match `foo::bar`; the
  individual leaves are not reachable as module segments.
- **Liveness is latest-write-wins.** An `Import` tombstoned and not re-added is
  excluded; one re-added after its tombstone is included, via the shared
  `Liveness` gate (issue #421), so `--graph` and `--data-dir` agree on
  tombstoned / revived records.
- **Deterministic.** Rows are ordered by `(repo_relative_path,
  span.start_line, record_id)` and output is byte-identical across repeated
  runs and across `--graph` vs `--data-dir` on an unchanged store.

### crate-unification boundary

The graph carries **no per-source-file owning-crate name**: a `File` node
records a repo-relative path and a `Repository` node records a repo identity,
but neither maps a source file to the Cargo crate that compiles it. This lane
therefore does **not** guess a `crate::` ↔ `<crate_name>::` unification.

- **By default**, segments are matched literally. A `crate`-relative import
  (`crate::foo::Bar`) and an absolute external import (`mycrate::foo::Bar`) are
  **distinct**, and a leading `self` / `super` is matched literally (the graph
  has no module anchor to resolve a relative prefix soundly).
- **With `--crate <name>`**, a leading `crate` segment in **both** the query
  and each import is rewritten to `<name>` before matching, so the two forms
  unify: `--crate mycrate` makes `mycrate::foo` match both `crate::foo::Bar`
  and `mycrate::foo::Bar`. This is caller-supplied ground truth, never guessed.

## Output format

### `--format json` (default) — newline-delimited JSON

Line 1 is a summary envelope; every following line is one importer row. A
well-formed query with **zero** importers is not a success: it emits a
`no_match` envelope and exits `2` (see [exit codes](#exit-codes)).

Summary envelope fields:

| Field | Type | Description |
|-------|------|-------------|
| `ok` | bool | `true` on a match. |
| `query_path` | string | The normalized query module path (segments re-joined with `::`). |
| `crate_name` | string | Present only with `--crate`: the unification name in effect. |
| `total_importers` | number | Importer row count (matches the emitted rows). |
| `disclaimer` | string | Matching-contract + leads-not-proof statement, always present. |

Importer row fields:

| Field | Type | Description |
|-------|------|-------------|
| `record_id` | string | Stable record ID of the `Import` node. |
| `schema_version` | number | Record schema version. |
| `repo_relative_path` | string | Repo-relative path of the importing file (the file handle). |
| `span` | object | Source span of the `use` declaration, when recorded. |
| `import_path` | string | The raw import path text the node carries. |
| `trust` | string | Always `"source_fact"`. |

The output never includes raw source text — record IDs, paths, spans, the
import path text, and counts only.

### `--format text`

One human-readable line per importer (`path:line  import_path`). The exact
format is not stable and must not be parsed by scripts.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one importer found. |
| `1` | Malformed module path — empty, leading/trailing `::`, empty interior segment, or a whitespace-bearing segment. Machine-readable `malformed_module_path` JSON on stderr. Also unknown/ambiguous `--repo` selector. |
| `2` | Well-formed query with zero importers (`no_match`). Machine-readable envelope on stdout. |

## Example

```sh
eg scan . --out graph.jsonl
eg query who-imports serde::Serialize --graph graph.jsonl
```

```json
{"ok":true,"query_path":"serde::Serialize","total_importers":2,"disclaimer":"Rows are the files whose recorded `use` declarations name a module path with the query as a segment-aware prefix …"}
{"record_id":"codegraph:v6:ab…","schema_version":6,"repo_relative_path":"src/ir.rs","span":{"start_byte":0,"end_byte":40,"start_line":3,"end_line":3},"import_path":"serde::Serialize","trust":"source_fact"}
{"record_id":"codegraph:v6:cd…","schema_version":6,"repo_relative_path":"src/query/repo.rs","span":{"start_byte":0,"end_byte":40,"start_line":5,"end_line":5},"import_path":"serde::{Serialize, Deserialize}","trust":"source_fact"}
```

## Out of scope (this slice)

- **No new schema.** This is a read-only query lane over existing `Import`
  nodes; it adds no inbound `IMPORTS` edge, no node kind, and bumps no schema
  or cache version.
- **No crate-name inference.** `crate::` ↔ `<crate>::` unification is available
  only via the explicit `--crate` flag (see the
  [crate-unification boundary](#crate-unification-boundary)); relative `self` /
  `super` prefixes are matched literally.
- **No temporal views.** `--at` / `--as-of` are not offered — this slice
  answers over the current state only.
- **No usage proof.** A match means a `use` declaration names the module path,
  never that the imported item is referenced or exercised.
