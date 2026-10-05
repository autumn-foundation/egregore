# eg query suppressions

Inventory **`#[allow(...)]` / `#![allow(...)]` lint suppressions** as an advisory
debt lane — answer the operator-visible question *"where is this codebase
silencing lints, which lints are silenced, and did anyone leave a justification
next to the suppression?"*. Local-first; no network access, hosted indexing,
remote crawling, or remote embeddings.

> **Rows are advisory debt-triage leads, not verdicts.** Each row asserts only
> that an `allow` attribute exists at this span silencing these lints, derived
> solely from deterministic extractor facts. The lane never judges whether a
> suppression is warranted, never rewrites or re-scores a code fact, and
> introduces no agent-authored observation.

## What is detected

One `LintSuppression` record per `allow` attribute, detected over the
Tree-sitter **attribute AST** — never regex:

| Form | Matches | Never matches |
|------|---------|---------------|
| `#[allow(dead_code)]` | outer attributes on items | `#[allow(` inside a `//` comment, a `///` / `//!` / `/** */` doc comment, or a string literal (those are comment/literal nodes, never attribute nodes) |
| `#[allow(dead_code, clippy::too_many_arguments)]` | multi-lint forms: one entry per lint-path token, sorted and deduplicated | non-path tokens in the token tree |
| `#![allow(unused)]` at the crate root | crate scope | — |
| `#![allow(unused)]` at the top of a module body | module scope | — |
| `#[cfg_attr(test, allow(dead_code))]` | — | out of scope for this slice: only the attribute's *own* name is compared |
| bare `#[allow]` / `#[allow()]` | — | silences no lints, so it emits no suppression fact |

Lint names cover both rustc lints (`dead_code`, `unused_imports`) and tool
lints (`clippy::too_many_arguments`, `rustfmt::skip`-style paths are whatever
the token tree holds).

## Scope and justification signals

- **`scope`** — the closed attribute-application scope, computed at extraction:
  - `item` — an outer `#[allow(...)]` annotating an item, or a `#![allow(...)]`
    inside a non-module body (e.g. a function body);
  - `module` — a `#![allow(...)]` at the start of a module body;
  - `crate` — a `#![allow(...)]` at the crate root.
- **`has_justification`** — whether a line or block comment (doc comments
  included) sits immediately adjacent to the attribute: ending on the line
  directly above it, or on the same line after it. A comment separated by a
  blank line is *not* adjacent. This is a justification **signal**, never a
  verdict on whether the suppression is warranted.

## Enclosing-symbol resolution

`enclosing_symbol` names the symbol the suppression is attached to, resolved
deterministically from the attribute form:

- An **inner** `#![allow(...)]` applies to the *enclosing* item, so it
  resolves to the innermost symbol whose span contains the attribute (the
  same containment rule as the other structural lanes).
- An **outer** `#[allow(...)]` annotates the *following* item — Tree-sitter
  item spans start at the item keyword, excluding preceding outer attributes —
  so it resolves to the nearest following symbol in the same file version.
- When the nearest following item is not a symbol (a nearer `Module` or
  `Import` node starts between the attribute and the candidate — e.g.
  `#[allow(unused_imports)]` on a `use`), the row honestly reports
  `enclosing_symbol: null` rather than attributing the suppression to the
  wrong symbol. Crate-scope suppressions always report `null`.

## Synopsis

```text
eg query suppressions --graph <PATH>   [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
eg query suppressions --data-dir <DIR> [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). Strictly read-only: querying creates or modifies no records or
indexes, and re-running the identical query against an unchanged store yields
byte-identical output.

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# Inventory every lint suppression
eg query suppressions --graph graph.jsonl

# Scope to a subsystem (segment-aware: src/alpha never bleeds into src/alphabet)
eg query suppressions --graph graph.jsonl --path src/adapters

# Ask which suppressions existed at a commit (valid-time axis, needs scan-history)
eg scan-history . --out history.graph.jsonl
eg query suppressions --graph history.graph.jsonl --at <COMMIT_SHA>
```

## Response shape

```jsonc
{
  "ok": true,
  "lane": "suppressions",
  "path_prefix": "src",            // null when unscoped
  "at_commit": null,               // full SHA when --at was supplied
  "disclaimer": "Rows are advisory lint-suppression triage leads ...",
  "suppressions": [
    {
      "record_id": "codegraph:v6:…",       // stable record/diagnostic ID
      "kind": "LintSuppression",
      "schema_version": 6,
      "lints": ["clippy::too_many_arguments", "dead_code"],  // sorted, deduped
      "scope": "item",                         // closed: item | module | crate
      "has_justification": true,
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 210, "end_byte": 245, "start_line": 8, "end_line": 8 },
      "language": "rust",
      "valid_time": "2026-01-01T00:00:00Z",
      "git_commit": "…",                    // history-backed rows only
      "enclosing_symbol": {                 // explicit null when no enclosing symbol
                                        // (crate scope; module scope resolves to null
                                        // because modules are Module records, not Symbols)
        "record_id": "codegraph:v6:…",
        "name": "parse_port",
        "symbol_kind": "function",
        "span": { "start_byte": 60, "end_byte": 260, "start_line": 2, "end_line": 10 }
      },
      "repository_id": "codegraph:v6:…",
      "repository": "owner/name",
      "trust": "source_fact"
    }
  ],
  "counts": { "total": 3, "item": 1, "module": 1, "crate": 1, "justified": 2, "unjustified": 1 },
  "diagnostics": [],
  "page": { "cursor": null, "has_more": false, "returned": 3 }
}
```

Suppressions are ordered deterministically by
`(repo_relative_path, span.start_byte, git_commit, record_id)`.

## Scoping and temporal selectors

- `--path <PREFIX>` — repo-relative directory/module prefix, matched
  segment-aware exactly like `eg query subsystem`.
- `--repo <SELECTOR>` — the standard repository selector (record ID, display
  name, basename/override, remote URL, root commit SHA, or canonical path). An
  unknown or ambiguous selector exits 1 with the standard machine-readable
  diagnostic; it never yields a silent empty result.
- `--at <COMMIT>` — pins the inventory to one commit on the valid-time axis
  (same selector contract as `eg query symbol --at`; unique prefixes accepted).
  A suppression removed by a later commit does not appear in a query pinned
  before its introduction, and vice versa. Without `--at`, the current view is
  returned: live current-tree records plus every history-backed version in the
  store — pin with `--at` when querying a history store.

## Empty vs not-found honesty (issue #196)

| Condition | Exit | Output |
|-----------|------|--------|
| Suppressions returned | `0` | Inventory JSON on stdout, `ok:true` |
| Scope exists but contains **zero** suppressions | `0` | `ok:true`, empty `suppressions`, `"empty_reason": "no_suppressions_in_scope"` |
| `--path` prefix matches nothing in the store slice | `2` | `{"ok":false,"error":{"code":"scope_not_found",...}}` |
| `--at` commit unknown to the store slice | `2` | `{"ok":false,"error":{"code":"unknown_commit",...}}` |
| Empty/malformed `--path` prefix | `1` | `{"ok":false,"error":{"code":"malformed_prefix",...}}` |
| Ambiguous `--at` commit prefix | `1` | `{"ok":false,"error":{"code":"ambiguous_commit",...}}` |
| Unknown / ambiguous `--repo` selector | `1` | standard selector diagnostic on stderr |

"Scope contains zero suppressions" and "scope not found" are distinct
machine-readable answers — the lane never conflates them.

## Record shape

Each suppression is a deterministic `LintSuppression` code-graph node emitted
by `eg scan` / `eg scan-history` (one record per `allow` attribute):

- `name` — the closed scope (`item` / `module` / `crate`);
- `lint_suppression` payload — the sorted, deduplicated `lints`, the `scope`,
  and the `has_justification` signal;
- `repo_relative_path` + `span` — the citable file/span handle;
- a `CONTAINS` edge from the owning `File` node (repository attribution);
- standard temporal metadata on history-backed records.

Trust separation holds: `LintSuppression` records live in the code-graph domain
(`source_fact` trust class), separate from agent-authored observations by
construction.

## Corpus scope

**Changed default (issue #456).** With **neither** a corpus flag nor (where
offered) an `--at`/`--as-of` selector, over a `scan-history` store this lane now
defaults to the **HEAD-anchored** corpus — records current at each repository's
stamped HEAD. Pass `--all-history` to read the union of all commit snapshots so
a suppression removed at a later commit still appears. See
`docs/cli/corpus-modes.md` for the full contract.
