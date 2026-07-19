# eg query unwrap-expect

Inventory **`.unwrap()` / `.expect()` panic-risk call sites** as an advisory
triage lane — answer the operator-visible question *"where can this code panic
through the most common Rust panic vector, and is each site production or test
code?"*. Local-first; no network access, hosted indexing, remote crawling, or
remote embeddings.

> **Rows are advisory panic-risk triage leads, not verdicts.** Each row asserts
> only that an unwrap/expect call exists at this span in this context, derived
> solely from deterministic extractor facts. The lane never judges whether an
> unwrap is justified, never rewrites or re-scores a code fact, and introduces
> no agent-authored observation.

## The closed method set

This slice covers exactly two methods, detected as **method calls** over the
Tree-sitter AST:

| Category | Matches | Never matches |
|----------|---------|---------------|
| `unwrap` | `.unwrap()` method calls | `unwrap_or`, `unwrap_or_else`, `unwrap_or_default`, the unsafe `unwrap_unchecked`, plain `unwrap(..)` identifier calls |
| `expect` | `.expect(..)` method calls | `expect_err`, plain `expect(..)` identifier calls |

Detection is AST-shaped (a `call_expression` whose callee is a
`field_expression` with an exactly matching `field_identifier`), so the literal
text `.unwrap()` inside a `//` comment, a string literal, or a doc comment is
**never** returned, and unrelated `unwrap` identifiers never match.

## How this lane relates to #210 and #222

All three are deterministic, advisory risk lanes over the same extractor:

- **#210 (macro panics)** classifies `panic!` / `todo!` / `unimplemented!` /
  `unreachable!` macro invocations. It explicitly deferred method-call panic
  risk.
- **#222 (`unsafe`)** inventories the `unsafe` surface.
- **This lane (#223)** covers exactly the `.unwrap()` / `.expect()` method-call
  set that #210 deferred. The known-risk method set is documented above and is
  closed for this slice.

## Synopsis

```text
eg query unwrap-expect --graph <PATH>   [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
eg query unwrap-expect --data-dir <DIR> [--path <PREFIX>] [--at <COMMIT>] [--repo <SELECTOR>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). Strictly read-only: querying creates or modifies no records or
indexes, and re-running the identical query against an unchanged store yields
byte-identical output.

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# Inventory every unwrap/expect call site
eg query unwrap-expect --graph graph.jsonl

# Scope to a subsystem (segment-aware: src/alpha never bleeds into src/alphabet)
eg query unwrap-expect --graph graph.jsonl --path src/adapters

# Ask which sites existed at a commit (valid-time axis, needs scan-history)
eg scan-history . --out history.graph.jsonl
eg query unwrap-expect --graph history.graph.jsonl --at <COMMIT_SHA>
```

## Production vs test context

Each site carries a closed `context` class computed at extraction time:

- `test` — the site is in a file under a top-level `tests/` directory, inside a
  `#[cfg(test)]` module (inline, or out-of-line: a `#[cfg(test)] mod x;`
  declaration marks the module's own file `x.rs` / `x/mod.rs` and its
  transitive out-of-line submodules), or inside a function carrying a
  dedicated test attribute: `#[test]` or a path attribute ending in `::test`
  (e.g. `#[tokio::test]`), with or without arguments.
- `production` — everything else.

The classification set is closed for this slice. Configuration attributes that
merely mention `test` — `#[cfg(not(test))]`, `#[cfg_attr(test, ...)]`,
`#[cfg(all(test, ...))]` — never mark test context and classify as
`production`. Out-of-line module resolution honors a `#[path = "literal"]`
override with the Rust reference's semantics: relative to the declaring file's
directory for top-level declarations, and relative to the module directory
plus the inline components for declarations inside inline module blocks
(`mod parent { #[path = "child.rs"] mod child; }` in `src/lib.rs` resolves to
`src/parent/child.rs`). Relative components in the literal are lexically
normalized (`#[path = "../support.rs"]` inside `mod tests { ... }` in
`src/lib.rs` resolves to `src/support.rs`); a `..` chain that would escape the
repository root is unresolvable. **Production takes precedence for dual-use
module files**: a file loaded through both a non-test declaration and a
test-gated one (e.g. `mod support;` plus `#[cfg(test)] mod tests {
#[path = "../support.rs"] mod support; }`) still compiles into the production
build, so its sites stay `production` — transitively, a file reached via a
production chain stays production even when also reachable via a test chain,
and no duplicate both-context rows are emitted. Conventional Cargo crate
roots — `src/lib.rs`, `src/main.rs`, `src/bin/<name>.rs`,
`src/bin/<name>/main.rs`, and `build.rs` — always keep their production seed,
even when a test-gated `#[path]` declaration also targets them; files under a
top-level `tests/` directory are their own test crates and stay test context
regardless. Two known, documented gaps: non-literal `#[path]`
expressions are not resolved, and declarations nested under an inline module
that itself carries a `#[path]` attribute (which rebases everything inside it)
are not resolved rather than probed against the wrong directory.

## Response shape

```jsonc
{
  "ok": true,
  "lane": "unwrap_expect",
  "method_set": ["expect", "unwrap"],
  "path_prefix": "src",            // null when unscoped
  "at_commit": null,               // full SHA when --at was supplied
  "disclaimer": "Rows are advisory panic-risk triage leads ...",
  "sites": [
    {
      "record_id": "codegraph:v5:…",       // stable record/diagnostic ID
      "kind": "PanicRiskSite",
      "schema_version": 5,
      "category": "unwrap",                 // closed: unwrap | expect
      "context": "production",              // closed: production | test
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 210, "end_byte": 238, "start_line": 8, "end_line": 8 },
      "language": "rust",
      "valid_time": "2026-01-01T00:00:00Z",
      "git_commit": "…",                    // history-backed rows only
      "enclosing_symbol": {                 // explicit null when top-level
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
  "counts": { "total": 4, "unwrap": 2, "expect": 2, "production": 2, "test": 2 },
  "diagnostics": [],
  "page": { "cursor": null, "has_more": false, "returned": 4 }
}
```

Sites are ordered deterministically by
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
  A site removed by a later commit does not appear in a query pinned before its
  introduction, and vice versa. Without `--at`, the current view is returned:
  live current-tree records plus every history-backed version in the store —
  pin with `--at` when querying a history store.

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

"Scope contains zero unwrap/expect sites" and "scope not found" are distinct
machine-readable answers — the lane never conflates them.

## Record shape

Each site is a deterministic `PanicRiskSite` code-graph node emitted by
`eg scan` / `eg scan-history` (one record per call expression):

- `name` — the closed category (`unwrap` / `expect`);
- `call_context` — the closed context class (`production` / `test`);
- `repo_relative_path` + `span` — the citable file/span handle;
- a `CONTAINS` edge from the owning `File` node (repository attribution);
- standard temporal metadata on history-backed records.

Trust separation holds: `PanicRiskSite` records live in the code-graph domain
(`source_fact` trust class), separate from agent-authored observations by
construction.
## Corpus scope

This lane is designed as a current-state lane but today reads the **union of all
commit snapshots** over a `scan-history` store. It discloses this honestly —
`corpus_mode: "union"` (or `single_snapshot` over a snapshot-less store),
`corpus_mode_source`, and `corpus_disclaimer` — and does **not** yet accept the
`--at-head`/`--all-history` flags (an `--at`/`--as-of` selector, where offered,
still pins a single commit as `commit_pinned`). Flipping it to the HEAD-anchored
default and adding that flag pair, under the contract published in issue #427, is
tracked in **issue #456**. See [Corpus scope for query lanes](corpus-modes.md).
