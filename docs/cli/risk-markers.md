# eg query risk-markers

List the **intentional completion-risk macro markers** — the unfinished stubs
(`todo!`, `unimplemented!`, `unreachable!`) and explicit panic points
(`panic!`) — that the Rust extractor already records as `Diagnostic` nodes,
as a citable pre-edit triage lane. Local-first; no scan, no recompute, no
network access, no embeddings.

This lane answers the operator-visible question *"what is unfinished or
runtime-fragile in this scope before I touch it?"* — the exact "is this
finished? / can this blow up?" signal an agent must check before editing a
subsystem, instead of discovering a `todo!()` at runtime.

## The closed risk-macro set

Classification is **exact and conservative** over a closed, documented set.
Only a `Diagnostic` node whose `name` is one of the macro names below (after
reducing path qualification to the final `::` segment — `std::panic!` is
still a `panic!`) AND whose `summary` names exactly that macro
(`unsupported macro invocation todo!` for name `todo!`) can classify.
Matching is exact: `mytodo!`, `panic_info!`, and `todo`/`panic` without the
macro `!` never match by substring, and a record whose name and summary
disagree classifies as nothing.

| Macro name(s) | Category | Meaning |
|---------------|----------|---------|
| `todo!` | `stub` | unfinished work the author marked for later |
| `unimplemented!` | `stub` | deliberately unimplemented branch |
| `unreachable!` | `stub` | branch the author believes is impossible |
| `panic!` | `panic` | explicit panic point |

Deliberately **outside** the closed set for this slice: `assert!` /
`assert_eq!` / `debug_assert!` (conditional panics — a different triage
question), `.unwrap()` / `.expect()` method calls (not macros; see
`eg query unwrap-expect`), TODO/FIXME/HACK comments (not extracted into the
graph; see `eg query debt-markers`), and all benign macros (`println!`,
`eprintln!`, `vec!`, `format!`, … — zero false-risk by construction: a benign
macro is never returned as a risk marker).

## Synopsis

```text
eg query risk-markers --graph <PATH>   [--repo <SELECTOR>] [--path <PREFIX>] [--at <COMMIT>] [--at-head | --all-history] [--format json|text]
eg query risk-markers --data-dir <DIR> [--repo <SELECTOR>] [--path <PREFIX>] [--at <COMMIT>] [--at-head | --all-history] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). Strictly read-only: querying creates or modifies no records
or indexes, and re-running the identical query against an unchanged store
yields byte-identical categories, handles, counts, and ordering.

## Response shape

Default `--format json` emits one JSON document — the risk-marker inventory
envelope (same envelope contract as `eg query debt-markers`):

```jsonc
{
  "ok": true,
  "lane": "risk-markers",
  "risk_macro_set": ["panic!", "todo!", "unimplemented!", "unreachable!"],
  "path_prefix": "src/parser",
  "at_commit": null,
  "disclaimer": "Rows are advisory triage leads derived solely from deterministic extractor facts. Each row asserts only that a stub or panic macro marker exists at this span — never that the surrounding code is correct or incorrect. Not a substitute for reading the source.",
  "markers": [
    {
      "record_id": "codegraph:v5:…",
      "kind": "Diagnostic",
      "schema_version": 5,
      "category": "stub",
      "macro_name": "todo!",
      "repo_relative_path": "src/parser/lexer.rs",
      "span": {"start_byte": 120, "end_byte": 132, "start_line": 9, "end_line": 9},
      "language": "rust",
      "git_commit": null,
      "enclosing_symbol": {
        "record_id": "codegraph:v5:…",
        "name": "lex_number",
        "symbol_kind": "function",
        "span": {"start_byte": 60, "end_byte": 300, "start_line": 5, "end_line": 21}
      },
      "repository_id": "codegraph:v5:…",
      "repository": "owner/name",
      "trust": "source_fact"
    }
  ],
  "counts": {"total": 3, "stub": 2, "panic": 1},
  "empty_reason": null,
  "diagnostics": [],
  "page": {"cursor": null, "has_more": false, "returned": 3},
  "corpus_mode": "head_anchored",
  "corpus_mode_source": "default",
  "corpus_disclaimer": "…"
}
```

Stable fields per marker:

| Field | Meaning |
|-------|---------|
| `record_id` | stable code-graph record ID of the `Diagnostic` node — the citable handle |
| `kind` | always `"Diagnostic"` |
| `schema_version` | record schema version |
| `category` | closed machine-readable category: `stub` or `panic` |
| `macro_name` | the macro name as recorded by the extractor (e.g. `todo!`; `std::panic!` stays qualified) |
| `repo_relative_path` | repo-relative file handle |
| `span` | `start_byte` / `end_byte` / `start_line` / `end_line` — lands on the macro site |
| `enclosing_symbol` | the innermost `Symbol` whose span encloses the marker in the same file version (record ID, name, kind, span); **explicit `null`** when the marker is module-top-level — never silently omitted |
| `trust` | always `"source_fact"` — every row is a deterministic extractor fact |

`--format text` prints one human-readable line per marker
(`<category> <macro_name> @ <path>:<line> in <symbol-or-(top-level)>`); an
empty scope prints `# no_markers_in_scope: …`.

The enclosing symbol is resolved by innermost span containment in the same
file version — the `DEFINES` owner of the region. The extractor emits no
edge from a symbol to a macro diagnostic, so there is no `DEFINES` edge to
cite; containment is the structural equivalent the lane (like
`eg query debt-markers`) uses, and it is deterministic. When the query is
repository-scoped, a candidate symbol positively owned by a *different*
repository is excluded, so a colliding path in another repository can never
be cited as the owner.

## Scoping

- `--repo <SELECTOR>` — the standard repository selector (record ID, display
  name, basename/override, remote URL, root commit SHA, or canonical path). An
  unknown or ambiguous selector exits 1 with the standard machine-readable
  diagnostic on stderr; it never yields a silent empty result. Repository
  attribution is **file-mediated**: the scanner wires no containment edges
  for `Diagnostic` nodes, so a marker belongs to the selected repository
  when its `File` does — the marker textually lives in that file. Under
  `--repo`, every returned row carries the selected repository's stable ID in
  `repository_id`.
- `--path <PREFIX>` — restrict to a repo-relative directory/module path
  prefix (segment-aware, same contract as `eg query subsystem`: `src/parser`
  matches `src/parser/lexer.rs`, never `src/parser2/x.rs`). A prefix that
  matches no file in the selected store slice exits 2 with a
  machine-readable `scope_not_found` envelope — a typo is never a silent
  "clean".
- `--at <COMMIT>` — pin the inventory to a commit SHA or unique prefix on
  the valid-time axis (same selector contract as `eg query symbol --at`), so
  an agent can ask "which stub/panic markers existed at commit X". A marker
  retired by a later commit does not appear in a query pinned before its
  introduction. An unknown commit exits 2 (`unknown_commit`); an ambiguous
  prefix exits 1 (`ambiguous_commit`).

## Empty vs not-found honesty

| Condition | Exit | Output |
|-----------|------|--------|
| Markers returned | `0` | inventory envelope on stdout |
| Scope exists but contains **zero** risk markers | `0` | envelope with `markers: []`, `counts` zeroed, and `empty_reason: "no_markers_in_scope"` (`# no_markers_in_scope: …` in text mode) |
| `--path` names nothing in the store slice | `2` | `{"ok":false,"error":{"code":"scope_not_found",…}}` |
| Unknown `--at` commit | `2` | `{"ok":false,"error":{"code":"unknown_commit",…}}` |
| Ambiguous `--at` prefix / malformed prefix / unknown `--repo` | `1` | standard machine-readable diagnostic |

"Scope contains zero risk markers" and "scope not found" are distinct
machine-readable answers — the lane never conflates them. A zero-marker scope
is a citable positive fact: the extractor recorded no stub or panic macro in
scope.

## Corpus scope

With **neither** a corpus flag nor a temporal selector, over a `scan-history`
store this lane defaults to the **HEAD-anchored** corpus — records current at
each repository's stamped HEAD commit — so a marker removed before HEAD no
longer appears. Pass `--all-history` to read the union of all commit
snapshots (the same marker then appears once per commit, distinguished by
`git_commit`); pass `--at-head` to force the HEAD-anchored view explicitly;
pass `--at <COMMIT>` to pin one commit's valid-time view. Over a
snapshot-less (plain `scan`) store the disclosure is `single_snapshot`. Every
response carries `corpus_mode` / `corpus_mode_source` /
`corpus_disclaimer`. `--at-head` and `--all-history` are mutually exclusive;
a conflict exits `1` with `unsupported_combination`. See
[Corpus scope for query lanes](corpus-modes.md).

## Trust separation

Rows derive solely from deterministic extractor `Diagnostic` facts: the lane
never rewrites or re-scores a code fact and introduces no agent-authored
observation. Each row asserts only that *a marker of category C exists at
this span* — never that the surrounding code is correct or incorrect, and
never that a `stub` will or will not be implemented. `Diagnostic` rows live
in the code-graph domain (`source_fact` trust class), separate from
agent-authored observations by construction.

## How this lane differs from the extraction-completeness signal (#87)

Both lanes read the same `Diagnostic` nodes, but they answer different
questions:

- **`eg query diagnostics`** (#87) treats *all* diagnostics as
  parse-opacity: it reports every coverage gap the extractor flagged
  (unsupported macros, unresolved calls, unresolved dispatch) as one
  undifferentiated blind-spot list. It never classifies; a `todo!()` and a
  `println!()` sit in the same bucket.
- **`eg query risk-markers`** (this lane, #210) classifies a **closed set of
  intentional completion-risk markers** — exactly `todo!`,
  `unimplemented!`, `unreachable!`, and `panic!` — and returns *only* those,
  each carrying its category, enclosing symbol, and citable file/span handle.

Use `diagnostics` when asking "where might the extractor be blind";
use `risk-markers` when asking "what is unfinished or runtime-fragile before
I edit this subsystem". Neither lane is a correctness claim and neither is
a substitute for reading the source.

## Shortest workflow

```text
# Seed: eg scan / eg scan-history writes the graph; macro diagnostics are automatic.
eg scan . --out graph.jsonl

# Triage one subsystem before editing it:
eg query risk-markers --graph graph.jsonl --path src/parser

# Ask what was risky at an older commit:
eg query risk-markers --graph history.graph.jsonl --path src/parser --at <commit-sha>
```
