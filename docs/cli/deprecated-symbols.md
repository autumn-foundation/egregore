# eg query deprecated-symbols

Inventory the code symbols carrying a `#[deprecated]` attribute mark —
with their still-resolvable call sites as a migration worklist. Code facts
only: no replacement suggestions, no synthesized prose.

> **Inventory, not advice.** Every row asserts only that a `#[deprecated]`
> attribute mark exists on a symbol at a citable file/span handle and that
> the listed call sites resolve to it over the recorded `CALLS` edges. What
> to use instead is your call — the lane never suggests replacements.

## Synopsis

```text
eg query deprecated-symbols --graph <PATH>    [--repo <SELECTOR>] [--file <PATH>] [--format json|text]
eg query deprecated-symbols --data-dir <DIR>  [--repo <SELECTOR>] [--file <PATH>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the inventory to one
repository in a multi-repo store; an unknown or ambiguous selector is
rejected with a machine-readable stderr diagnostic (exit 1), never resolved
implicitly. `--file <PATH>` restricts the inventory (and the
`symbols_considered` tally) to symbols declared in that repo-relative file —
`src/lib.rs`, not an absolute path. The call-site worklists of the selected
symbols stay complete: they cite callers wherever they are, because the
worklist answers "who still calls this symbol", not "what is declared in
this file". Strictly read-only: no records, indexes, or runtime files are
created, modified, or deleted.

The default JSON output is **newline-delimited JSON**: one compact JSON
document on a single line (no pretty-printing), like the sibling query
lanes — byte-identical across runs on an unchanged store. `--format text`
prints one human-readable line per symbol and call site.

| Condition | Exit | Output |
|-----------|------|--------|
| At least one deprecated symbol inventoried | `0` | JSON or text on stdout, `ok:true` (JSON) |
| Live code symbols exist, **none** marked deprecated | `3` | `{"ok":true, "deprecated_symbols":[], "diagnostics":[{"code":"no_deprecated_symbols",...}]}` on stdout |
| Input resolves to **zero** code-graph nodes | `4` | `{"ok":true, "deprecated_symbols":[], "diagnostics":[{"code":"no_code_symbols",...}]}` on stdout |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input, or both/neither of `--graph`/`--data-dir` | `1` | Error message on stderr |

Exits `3` and `4` are **never** an empty success conflated with "no
deprecated code found": each carries a stable machine-readable diagnostic
and a distinct exit code, and neither fabricates a row.

## Response fields (JSON)

Top-level envelope, one compact JSON document per line:

| Field | Meaning |
|-------|---------|
| `ok` | `true` on every stdout response (exits 0, 3, 4) |
| `lane` | `"deprecated_symbols"` |
| `repo_scope` | The resolved `--repo` selector, when one was given |
| `file_filter` | The `--file` selector, when one was given |
| `disclaimer` | One-line soundness boundary: inventory only, never advice |
| `deprecated_symbols[]` | Inventory rows, sorted by (`repo_relative_path`, span start line, `record_id`) |
| `counts` | `symbols_considered`, `deprecated_symbols`, `resolved_call_sites`, `unresolved_call_edges` |
| `coverage.unresolved_call_edges` | `{count, call_edge_record_ids[]}` — every unresolvable `CALLS` edge cited by record ID, sorted |
| `diagnostics[]` | Stable diagnostics (`no_deprecated_symbols`, `no_code_symbols`, `unresolved_call_edges_present`), sorted by (code, record ID, detail) |
| `corpus_mode` / `corpus_mode_source` / `corpus_disclaimer` | Which corpus the current-state view read (issue #427) |

Each inventory row:

| Field | Meaning |
|-------|---------|
| `record_id` | Stable record ID of the `Symbol` node — the citable handle |
| `schema_version` | Record schema version |
| `name` | Symbol name (qualified where the extractor qualifies it) |
| `kind` | Language-specific kind (`function`, …); `"symbol"` when unrecorded |
| `repo_relative_path` | Declaring file, repo-relative (omitted when unrecorded) |
| `span` | `{start_byte, end_byte, start_line, end_line}` (+ columns when recorded) |
| `git_commit` | Introducing commit for temporal (history-backed) records |
| `deprecated_since` / `deprecated_note` | Verbatim bounded payloads; `null` when the attribute did not carry them — never fabricated |
| `call_sites[]` | Migration worklist, sorted by (caller path, caller span start line, edge ID): `call_edge_record_id`, `caller_record_id`, `caller_name`, `caller_kind`, `caller_repo_relative_path`, `caller_span`, `resolution` (`resolved` / `ambiguous` / `unrecorded`) |

The `--format text` rendering prints one line per symbol (`- <name> [<kind>]
deprecated_since=<since> deprecated_note=<note> @ <path>:<start>-<end> (<record_id>)`), one
indented line per call site, a `counts:` tally line, the diagnostics, and
the corpus mode. Absent `deprecated_since`/`deprecated_note` render as `-`.

## What counts as deprecated

A symbol is inventoried when its `Symbol` node carries a deprecation mark —
the mark's *presence* is the fact. The extractor (issue #249) captures the
mark from the attribute items immediately preceding the item declaration
(Tree-sitter node walking, never regex) in three forms:

- Bare: `#[deprecated]`
- Note shorthand: `#[deprecated = "..."]`
- Meta form: `#[deprecated(since = "...", note = "...")]` (key order is free;
  unknown keys and non-literal values are ignored — the item is still marked)

When the attribute carries `since` and/or `note`, those values are surfaced
**verbatim** as bounded strings: at most 256 characters (`ir::MAX_DEPRECATION_STRING_LEN`),
a verbatim prefix with no truncation marker synthesized, passed through
redaction policy v1 exactly like issue #124 doc facts. Absent fields use the
documented absent-value rule — `null` in JSON, `-` in text — **never**
fabricated text.

## The migration worklist

For each deprecated symbol, the lane reports its still-resolvable call
sites over the **existing `CALLS` edge vocabulary** — no new edge kinds:

- `resolved` / `ambiguous` edges targeting the symbol join the worklist. An
  ambiguous call *might* target the symbol, so it is never dropped.
- Each call site carries the citable `CALLS` edge record ID plus the caller
  symbol's record ID, name, kind, and repo-relative file/span.
- Edges carrying no resolution status are attributed but labeled
  `unrecorded` — honestly, not guessed.

Call edges the graph **cannot** resolve (`Unresolved` /
`UnresolvedDispatch`, issue #152) target a `Diagnostic` marker, not a
symbol, so they cannot join any worklist. They are reported under
`coverage.unresolved_call_edges` — an honest count plus every edge's stable
record ID, sorted — **never silently dropped and never counted as zero**.
Any of them may call a deprecated symbol, so every worklist is a lower
bound.

## Determinism

Output ordering is deterministic and depends only on record content:

- Symbols: by (`repo_relative_path`, span start line, `record_id`).
- Call sites: by (caller path, caller span start line, call-edge record ID).
- Diagnostics: by (code, record ID, detail).

Re-running the identical command against an unchanged store/graph produces
byte-identical rows and ordering.

## Current-state view

The lane reads the same current-state view as the sibling triage lanes:
tombstoned symbols are excluded; over history graphs a stable ID keeps its
latest version and a deprecation mark or call edge absent at the
repository's stamped HEAD does not shape the answer. `impl`-block symbols
can never carry the mark (`#[deprecated]` on an `impl` is not valid Rust),
so no impl exclusion is needed.

## When to use it

Use `eg query deprecated-symbols` for **migration / maintenance / debt
triage**: "which deprecated APIs do we still call, and where, so we can
schedule the migration work."

Prefer something else when:

- **`cargo build` warnings** — the compile-path-only view: it shows
  deprecation warnings reachable from *your current build's* crate graph,
  not the repo-wide inventory, and it says nothing about call sites in
  files the build never compiles.
- **`rg '#\[deprecated'`** — the text view: finds the attribute spellings
  but cannot join call sites, distinguish resolved from unresolvable calls,
  or give you citable record handles.
- **Dead-code pruning** (`eg query unreferenced`, issues #113/#240) —
  answers "what does nothing reference", not "what is marked deprecated".
  A deprecated symbol with callers is the opposite of dead code.
- **Breaking-API-change detection** (`eg query public-api-deltas`, issue
  #157) — answers "what changed between commits", not "what is deprecated
  right now".

## Contracts consumed

The slice reuses the existing code-graph, repository-identity,
schema-versioning (additive optional `deprecated` field, never an identity
input), and redaction contracts. It introduces **no** new graph domain,
importer, trust class, edge vocabulary, or drift algorithm. Agent
observations, project/task state, and verification records are never mixed
into the inventory.
