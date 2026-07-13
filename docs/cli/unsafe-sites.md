# eg query unsafe-sites

Inventory a crate's **`unsafe`-code surface** — every `unsafe { .. }` block,
`unsafe fn` declaration, and `unsafe impl` block in the scanned repo's own
source — as a citable risk query lane, answering the operator-visible question
*"where does this code reach for `unsafe`, and how concentrated is it?"*.
Local-first; no network access, hosted indexing, remote crawling, or remote
embeddings.

> **This lane is an inventory, not a correctness or soundness audit.** Each
> row asserts only that an `unsafe` site of a given kind exists at a span,
> derived solely from deterministic extractor facts. The lane never judges
> whether the code is sound or unsound, never rewrites or re-scores a code
> fact, and introduces no agent-authored observation. **A zero count is not a
> safety guarantee**: `unsafe` introduced by macro expansion, build scripts,
> or dependencies is out of this slice (a documented blind spot).

## The closed kind set

This slice covers exactly three site kinds, detected over the Tree-sitter AST:

| Kind | Matches | Never matches |
|-------|---------|---------------|
| `block` | `unsafe { .. }` block expressions | the word `unsafe` in comments, doc comments, or string literals |
| `fn` | `unsafe fn` declarations (including trait-method signatures) | ordinary `fn` items; `unsafe_`-prefixed identifiers |
| `impl` | `unsafe impl` blocks | ordinary `impl` blocks |

Detection is AST-shaped — an `unsafe_block` node, or an `unsafe` keyword token
on a `function_item` / `function_signature_item` / `impl_item` — so the
literal text `unsafe` inside a `//` comment, a string literal, a doc comment,
or an identifier such as `unsafe_flag_count` is **never** returned. `unsafe
trait` declarations are outside the closed kind set for this slice.

## How this lane relates to #210, #218, and #223

All are deterministic, advisory risk lanes over the same extractor:

- **#210 (macro panics)** classifies stub/panic macro invocations and
  explicitly scoped `unsafe` out.
- **#218 (debt markers)** extracts TODO/FIXME comment markers — finishedness,
  not memory safety.
- **#223 (unwrap/expect)** covers `.unwrap()` / `.expect()` panic risk.
- **This lane (#222)** inventories the memory-safety `unsafe` surface — a
  distinct risk axis from all three. The kind set is closed for this slice.

## Synopsis

```text
eg query unsafe-sites --graph <PATH>   [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
eg query unsafe-sites --data-dir <DIR> [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). Strictly read-only: querying creates or modifies no records or
indexes, and re-running the identical query against an unchanged store yields
byte-identical output.

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# Inventory the whole unsafe surface (aggregate count included)
eg query unsafe-sites --graph graph.jsonl

# Scope to a subsystem (segment-aware: src/alpha never bleeds into src/alphabet)
eg query unsafe-sites --graph graph.jsonl --path src/ffi

# Ask what the unsafe surface was at a commit (valid-time axis, needs scan-history)
eg scan-history . --out history.graph.jsonl
eg query unsafe-sites --graph history.graph.jsonl --at <COMMIT_SHA>
```

## Response shape

```jsonc
{
  "ok": true,
  "lane": "unsafe_sites",
  "kind_set": ["block", "fn", "impl"],
  "path_prefix": "src",            // null when unscoped
  "at_commit": null,               // full SHA when --at was supplied
  "disclaimer": "Rows are an advisory unsafe-surface inventory ...",
  "sites": [
    {
      "record_id": "codegraph:v5:…",       // stable record ID
      "kind": "UnsafeSite",
      "schema_version": 5,
      "site_kind": "block",                 // closed: block | fn | impl
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 310, "end_byte": 348, "start_line": 8, "end_line": 8 },
      "language": "rust",
      "valid_time": "2026-01-01T00:00:00Z",
      "git_commit": "…",                    // history-backed rows only
      "enclosing_symbol": {                 // explicit null when top-level
        "record_id": "codegraph:v5:…",
        "name": "read_flag",
        "symbol_kind": "function",
        "span": { "start_byte": 60, "end_byte": 360, "start_line": 2, "end_line": 9 }
      },
      "repository_id": "codegraph:v5:…",
      "repository": "owner/name",
      "trust": "source_fact"
    }
  ],
  "counts": { "total": 4, "block": 1, "fn": 2, "impl": 1 },
  "diagnostics": [],
  "page": { "cursor": null, "has_more": false, "returned": 4 }
}
```

Sites are ordered deterministically by
`(repo_relative_path, span.start_byte, git_commit, record_id)`. The aggregate
`counts.total` always equals the number of returned sites, so one number
compares crates or subsystems.

The enclosing symbol is the innermost `DEFINES`-owned `Symbol` whose span
covers the site. For an `unsafe fn` item or an `unsafe impl` block that is
the declared symbol itself; for an `unsafe { .. }` block it is the containing
function or item. For a signature-only trait method (`trait T { unsafe fn
f(); }`) it is the **method itself**: issue #342 makes signature-only trait
method declarations first-class `Symbol` records (mirroring default-bodied
trait methods), so the innermost covering symbol is the method's own `Symbol`,
whose span is smaller than the containing trait's. A site with no covering
symbol span carries an explicit `null`. (Foreign function declarations inside
an `extern` block are still symbol-less, so an `unsafe fn` there attributes to
its nearest covering symbol, or `null` at file top level.)

## Scoping and temporal selectors

- `--path <PREFIX>` — repo-relative directory/module prefix, matched
  segment-aware exactly like `eg query subsystem`.
- `--repo <SELECTOR>` — the standard repository selector (record ID, display
  name, basename/override, remote URL, root commit SHA, or canonical path). An
  unknown or ambiguous selector exits 1 with the standard machine-readable
  diagnostic; it never yields a silent empty result.
- `--at <COMMIT>` — pins the inventory to one commit on the valid-time axis
  (same selector contract as `eg query symbol --at`; unique prefixes accepted).
  A site retired by a later commit does not appear in a query pinned before
  its introduction, and vice versa — so the `unsafe` footprint can be compared
  over history. Without `--at`, the current view is returned: live
  current-tree records plus every history-backed version in the store — pin
  with `--at` when querying a history store.

## Empty vs not-found honesty (issue #196)

| Condition | Exit | Output |
|-----------|------|--------|
| Sites returned | `0` | Inventory JSON on stdout, `ok:true` |
| Scope exists but contains **zero** sites | `0` | `ok:true`, empty `sites`, `"empty_reason": "no_sites_in_scope"` |
| `--path` prefix matches nothing in the store slice | `2` | `{"ok":false,"error":{"code":"scope_not_found",...}}` |
| `--at` commit unknown to the store slice | `2` | `{"ok":false,"error":{"code":"unknown_commit",...}}` |
| Empty/malformed `--path` prefix | `1` | `{"ok":false,"error":{"code":"malformed_prefix",...}}` |
| Ambiguous `--at` commit prefix | `1` | `{"ok":false,"error":{"code":"ambiguous_commit",...}}` |
| Unknown / ambiguous `--repo` selector | `1` | standard selector diagnostic on stderr |

"Scope contains zero `unsafe` sites" and "scope not found" are distinct
machine-readable answers — the lane never conflates them.

## Record shape

Each site is a deterministic `UnsafeSite` code-graph node emitted by
`eg scan` / `eg scan-history` (one record per site):

- `name` — the closed site kind (`block` / `fn` / `impl`);
- `repo_relative_path` + `span` — the citable file/span handle;
- a `CONTAINS` edge from the owning `File` node (repository attribution);
- standard temporal metadata on history-backed records.

Trust separation holds: `UnsafeSite` records live in the code-graph domain
(`source_fact` trust class), separate from agent-authored observations by
construction.

## Known blind spots (out of scope for this slice)

- `unsafe` reached transitively through **dependencies** (cargo-geiger's
  whole-tree view) — this lane inventories the scanned repo's own source only.
- `unsafe` introduced by **macro expansion** or **build scripts** — not
  visible without expansion.
- Severity scoring, ranking, or hotspot ordering; `#[allow(unsafe_code)]` /
  `unsafe_op_in_unsafe_fn` policy interpretation.
- Languages beyond Rust.
