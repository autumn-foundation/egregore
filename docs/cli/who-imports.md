# eg query who-imports

List the files that import a module path — "who depends on this module?". The
scan pipelines additionally mint inbound `IMPORTS` edges from each importing
file to the resolved imported Module/File target (issue #444, second half),
so the same importer relationship is traversable as graph edges — e.g. via
`change-impact`'s `referencing_files` or a direct inbound-`IMPORTS` walk —
without `jq` over the raw JSONL. This lane itself keeps matching the `Import`
nodes the language extractors mint, which also covers imports the scan-time
pass could not resolve to a module.

## Synopsis

```text
eg query who-imports <MODULE-PATH> --graph <PATH>   [--crate <NAME>] [--repo <SELECTOR>] [--at-head | --all-history] [--format json|text]
eg query who-imports <MODULE-PATH> --data-dir <DIR> [--crate <NAME>] [--repo <SELECTOR>] [--at-head | --all-history] [--format json|text]
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
| `--crate <NAME>` | no | Override the owning-crate resolution: unify a leading `crate::` with this crate name for the query and every import (see [crate-unification](#crate-unification)). |
| `--repo <SELECTOR>` | no | Restrict the importer set to one repository in a multi-repo store (see [query.md](query.md#repository-scope---repo-issue-67)). |
| `--at-head` | no | Corpus selector (issue #427): head-anchor the current-state view to each repository's stamped HEAD, excluding imports removed at HEAD. The default over a `scan-history` store carrying a `source_snapshot`; mutually exclusive with `--all-history`. |
| `--all-history` | no | Corpus selector (issue #427): read the union of all commit snapshots, so an import present only in an earlier commit still appears. Mutually exclusive with `--at-head`. |
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
- **A `scan-history` graph defaults to the HEAD-anchored corpus (breaking change,
  issue #427).** By default an unpinned query now returns only imports current at
  each repository's stamped HEAD commit, so an import removed before HEAD is
  excluded. This **flips the previous default**, which read the **union of all
  commit snapshots**: history replay stamps a per-commit `Import` record but does
  not tombstone an import removed in a later commit, so under the old default an
  import that existed only in an early commit was still returned. Pass
  `--all-history` to restore that union; pass `--at-head` to force the
  HEAD-anchored view explicitly. This lane offers no `--at` / `--as-of`. The
  summary envelope discloses `corpus_mode` / `corpus_mode_source` /
  `corpus_disclaimer`; `--at-head` and `--all-history` are mutually exclusive
  (exit `1`, `unsupported_combination`). This matches the flipped defaults of
  `deps`, `path`, `transitive-callers`, and `transitive-callees`. Over a `scan`
  (current-tree) graph there is exactly one snapshot, so the answer is the current
  state directly. See [Corpus scope for query lanes](corpus-modes.md).
- **Deterministic.** Rows are ordered by `(repo_relative_path,
  span.start_line, record_id)` and output is byte-identical across repeated
  runs and across `--graph` vs `--data-dir` on an unchanged store.

### crate-unification

A leading `crate::` in an import denotes the importing file's **own** crate, so
this lane resolves it to that crate's absolute `<crate_name>::…` form from
facts the graph already carries — no flag needed (issue #450, building on the
issue #440 crate-root partitioning):

1. **Auxiliary-target files** (`src/bin/<t>.rs`, `examples/<t>.rs`,
   `tests/<t>.rs`, `benches/<t>.rs`) compile as their own crate named for the
   **target** — `crate::` inside `examples/demo.rs` denotes the `demo` crate,
   not the owning package.
2. Otherwise the record's manifest-stamped **package attribution** (issue
   #117), validated per the #104 doctrine: only a resolver-producible value
   whose cited manifest encloses the record's path counts, so a crafted record
   cannot forge another crate's identity.
3. Otherwise the #440 **workspace-prefix-derived name**: the last component of
   the workspace-crate directory prefix, cargo-normalized (`-` → `_`). This
   covers graphs whose records predate attribution.

A leading `crate` in the **query** resolves per import to that import's owning
crate ("the importer's own crate"), so `crate::foo` finds every crate's own
`foo`. Concretely: `mycrate::foo` matches both `crate::foo::Bar` and
`mycrate::foo::Baz` written in `mycrate`. When no fact covers a record (e.g. a
hand-built graph with no attribution), segments match literally, as before.

**`--crate <name>`** remains as an explicit **override**: caller-supplied
ground truth that wins over the facts for both the query and every import —
for graphs the facts cannot cover, or when the caller wants to force a name.

Residual bounds (documented, not hidden): a `[lib] name` override makes the
true crate name differ from the package name; a helper module under an
auxiliary target's directory (e.g. `tests/common/mod.rs`) inherits the target
name although its `crate::` resolves to whichever target includes it; and a
leading `self` / `super` is still matched literally (the graph has no module
anchor to resolve a relative prefix soundly).

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

- **No new schema.** `IMPORTS` edges already existed; the second half of issue
  #444 adds the *target* shape (`File —IMPORTS→ Module|File`, "this file
  imports this module") alongside the extractor's containment shape
  (`File —IMPORTS→ Import`, "this file declares this `use`"). Edge IDs are
  stable (`edge/IMPORTS/source/target`) and the pass is deterministic, so no
  schema or cache version bump was needed.
- **No crate-name guessing.** `crate::` ↔ `<crate_name>::` unification resolves
  only from owning-crate facts (auxiliary-target name, manifest-stamped
  attribution, workspace directory name — see
  [crate-unification](#crate-unification)); when no fact covers a record, the
  forms stay distinct, and `--crate` overrides. Relative `self` / `super`
  prefixes are matched literally.
- **No `--at` / `--as-of` temporal pin.** A single-commit view is not offered.
  Corpus scope is instead controlled by `--at-head` / `--all-history`: over a
  `scan-history` graph the lane now defaults to the **HEAD-anchored** corpus and
  `--all-history` opts into the **union of all commit snapshots** (the old
  default), mirroring `deps` / `path`. See [Semantics](#semantics) and
  [Corpus scope for query lanes](corpus-modes.md).
- **No usage proof.** A match means a `use` declaration names the module path,
  never that the imported item is referenced or exercised.
