# eg query debt-markers

Inventory **human-authored debt-comment markers** — `TODO`, `FIXME`, `HACK`,
`XXX` — as an advisory triage lane: answer the operator-visible question
*"what did a prior author flag as unfinished or fragile in this scope?"*
before touching a subsystem. Local-first; no network access, hosted indexing,
remote crawling, or remote embeddings.

> **Rows are advisory debt-triage leads, not verdicts.** Each row asserts only
> that a comment of category C with note text T exists at this span, derived
> solely from deterministic extractor facts — reproducible byte-for-byte from
> source. The lane never asserts the surrounding code is correct or
> incorrect, never re-scores a code fact, and introduces no agent-authored
> observation.

## The closed marker set

This slice recognizes exactly four marker categories, detected inside
**Tree-sitter comment nodes** only:

| Category | Matches | Never matches |
|----------|---------|---------------|
| `todo`   | the word `TODO` in a comment (any case) | `TODOIST`, `TODO2`, `TODO_LIST`, `"TODO"` in a string literal |
| `fixme`  | the word `FIXME` in a comment (any case) | `fixmeup` and other identifier substrings |
| `hack`   | the word `HACK` in a comment (any case) | `HACKATHON` and other longer words |
| `xxx`    | the word `XXX` in a comment (any case) | `XXXL` and other longer words |

Detection is conservative and false-positive-safe by construction:

- Comment ranges come exclusively from the Tree-sitter parse tree
  (`line_comment` / `block_comment` nodes, which include rustdoc `///`,
  `//!`, and `/** */` docs), so a marker token inside a **string or character
  literal** can never match.
- Within an identified comment's text, a candidate token is a maximal ASCII
  `[A-Za-z0-9_]` word run, so a marker substring inside an identifier or
  longer word never matches.
- Matching is **case-insensitive on the marker token only**; the note text
  keeps its original case.
- The marker set is closed for this slice — no user-configurable
  vocabularies.

One record is emitted per marker token occurrence, in source order. The
captured note is the raw text following the marker to the end of its line
(single-line), with a trailing `*/` terminator removed, one leading `:` or
`-` separator dropped, and surrounding whitespace trimmed. Structured
metadata such as `TODO(username):` assignees stays verbatim inside the note —
parsing it into typed fields is a later slice.

## How this lane relates to #210 and #87

- **#210 (stub/panic macro markers)** classifies `todo!()` /
  `unimplemented!()` / `panic!()` **macro invocations** — intentional
  completion-risk markers already present in the graph as `Diagnostic`
  nodes. It explicitly carved out TODO/FIXME/HACK comment markers as a
  distinct text-extraction concern.
- **#87 (diagnostics as parse-opacity)** treats all `Diagnostic` records as
  extraction-coverage signals.
- **This lane (#218)** covers human-authored **comment** markers, which the
  extractor previously stripped before the graph was built and which required
  new extraction. It is comment text only; macro markers stay with #210.

## Synopsis

```text
eg query debt-markers --graph <PATH>   [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
eg query debt-markers --data-dir <DIR> [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). Strictly read-only: querying creates or modifies no records or
indexes, and re-running the identical query against an unchanged store yields
byte-identical output.

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# Inventory every debt-comment marker
eg query debt-markers --graph graph.jsonl

# Scope to a subsystem (segment-aware: src/alpha never bleeds into src/alphabet)
eg query debt-markers --graph graph.jsonl --path src/adapters

# Ask which markers existed at a commit (valid-time axis, needs scan-history)
eg scan-history . --out history.graph.jsonl
eg query debt-markers --graph history.graph.jsonl --at <COMMIT_SHA>
```

## Response shape

```jsonc
{
  "ok": true,
  "lane": "debt_markers",
  "marker_set": ["fixme", "hack", "todo", "xxx"],
  "path_prefix": "src",            // null when unscoped
  "at_commit": null,               // full SHA when --at was supplied
  "disclaimer": "Rows are advisory debt-triage leads ...",
  "markers": [
    {
      "record_id": "codegraph:v5:…",       // stable record ID
      "kind": "DebtMarker",
      "schema_version": 5,
      "category": "todo",                   // closed: todo | fixme | hack | xxx
      "note": "wire retry logic",           // trimmed, single-line
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 82, "end_byte": 104, "start_line": 3, "end_line": 3 },
      "language": "rust",
      "valid_time": "2026-01-01T00:00:00Z",
      "git_commit": "…",                    // history-backed rows only
      "enclosing_symbol": {                 // explicit null at module top level
        "record_id": "codegraph:v5:…",
        "name": "parse_port",
        "symbol_kind": "function",
        "span": { "start_byte": 60, "end_byte": 260, "start_line": 2, "end_line": 10 }
      },
      "repository_id": "codegraph:v5:…",
      "repository": "owner/name",
      "trust": "source_fact"
    }
  ],
  "counts": { "total": 4, "todo": 1, "fixme": 1, "hack": 1, "xxx": 1 },
  "diagnostics": [],
  "page": { "cursor": null, "has_more": false, "returned": 4 }
}
```

Markers are ordered deterministically by
`(repo_relative_path, span.start_byte, git_commit, record_id)`.

## Scoping and temporal selectors

- `--path <PREFIX>` — repo-relative directory/module prefix, matched
  segment-aware exactly like `eg query subsystem`.
- `--repo <SELECTOR>` — the standard repository selector (record ID, display
  name, basename/override, remote URL, root commit SHA, or canonical path). An
  unknown or ambiguous selector exits 1 with the standard machine-readable
  diagnostic; it never yields a silent empty result.
- `--at <COMMIT>` — pins the inventory to one commit on the valid-time axis
  (same selector contract as `eg query symbol --at`; unique prefixes
  accepted). A marker introduced by a later commit does not appear in a query
  pinned before its introduction, and a marker deleted by a later commit does
  not appear in a query pinned after its removal. Without `--at`, the current
  view is returned: live current-tree records plus every history-backed
  version in the store — pin with `--at` when querying a history store.

## Empty vs not-found honesty (issue #196)

| Condition | Exit | Output |
|-----------|------|--------|
| Markers returned | `0` | Inventory JSON on stdout, `ok:true` |
| Scope exists but contains **zero** markers | `0` | `ok:true`, empty `markers`, `"empty_reason": "no_markers_in_scope"` |
| `--path` prefix matches nothing in the store slice | `2` | `{"ok":false,"error":{"code":"scope_not_found",...}}` |
| `--at` commit unknown to the store slice | `2` | `{"ok":false,"error":{"code":"unknown_commit",...}}` |
| Empty/malformed `--path` prefix | `1` | `{"ok":false,"error":{"code":"malformed_prefix",...}}` |
| Ambiguous `--at` commit prefix | `1` | `{"ok":false,"error":{"code":"ambiguous_commit",...}}` |
| Unknown / ambiguous `--repo` selector | `1` | standard selector diagnostic on stderr |

"Scope contains zero debt markers" and "scope not found" are distinct
machine-readable answers — the lane never conflates them.

## Record shape

Each marker is a deterministic `DebtMarker` code-graph node emitted by
`eg scan` / `eg scan-history` (one record per marker token occurrence):

- `name` — the closed lowercase category (`todo` / `fixme` / `hack` / `xxx`);
- `note` — the trimmed single-line note text, passed through redaction
  policy v1 at extraction time;
- `repo_relative_path` + `span` — the citable file/span handle covering the
  marker token through the end of its note;
- a `CONTAINS` edge from the owning `File` node (repository attribution);
- standard temporal metadata on history-backed records.

The enclosing symbol returned by the lane is the innermost same-file-version
`Symbol` in the marker's own repository whose span contains the marker — in a
shared multi-repository store, a symbol from another repository that happens
to define the same repo-relative path is never cited. A marker at module top
level (for example inside a file-leading doc comment) carries an explicit
`enclosing_symbol: null`.

Trust separation holds: `DebtMarker` records live in the code-graph domain
(`source_fact` trust class), separate from agent-authored observations by
construction. Language coverage in this slice is Rust only.
