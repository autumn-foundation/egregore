# eg query undocumented

List **externally-reachable public symbols with no doc comment** — the crate's
documentation debt on its outward contract — as a citable, deterministic,
read-only query lane. Local-first, no network, no build.

> **Presence/absence only, never quality.** The lane asserts whether a
> recorded doc-comment fact exists for each symbol (`///`, `/** */`, or
> `#[doc = "..."]`). It never asserts doc quality, accuracy, or completeness —
> broken intra-doc links, stale examples, and missing `# Errors` sections are
> out of scope.

The lane is a graph-native join of two recorded facts:

- the **externally-reachable public surface** from issue #213
  (`eg query public-api`) — top-level `pub` reachability plus
  visibility-widening `pub use` re-exports, reused verbatim, never re-derived;
- the **captured doc-comment fact** from issue #124 — a symbol carrying any
  doc form is excluded; a plain `//` comment above a symbol is *not*
  documentation and the symbol is still reported.

The boring substitute, rustdoc's `missing_docs` lint, needs a compiling crate
and emits stderr warnings; this lane works on code that does not build and
returns a machine-readable, citable set instead.

## Synopsis

```text
eg query undocumented --graph <PATH>    [--repo <SELECTOR>] [--limit N] [--include-private] [--format json|text]
eg query undocumented --data-dir <DIR>  [--repo <SELECTOR>] [--limit N] [--include-private] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the audit to one repository in a
multi-repo store; an unknown or ambiguous selector is rejected with a
machine-readable stderr diagnostic (exit 1), never resolved implicitly.
`--limit N` truncates the deterministically sorted rows and adds a
`results_truncated` diagnostic. `--include-private` widens the audit to every
doc-auditable symbol (adding `method` declarations) regardless of visibility,
for whole-crate doc audits.

| Condition | Exit | Output |
|-----------|------|--------|
| Audit computed — including **zero** undocumented symbols | `0` | Undocumented JSON on stdout, `ok:true` |
| Store predates issue #124 doc capture | `0` | Explicit `capability: "doc_facts_unavailable"` verdict, empty `items` |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input | `1` | Error message on stderr |

An empty result is a **real, successful, explicit answer**: exit 0 with
`ok:true`, an empty `items` array, and a `no_undocumented_items` diagnostic.

## Capability-absent verdict

A store scanned before issue #124 carries no doc-comment facts at all. The
lane detects this (no doc-auditable symbol carries the recorded declaration
surface) and reports `capability: "doc_facts_unavailable"` with a
`doc_capture_unavailable` diagnostic and an **empty** `items` array — it never
silently treats every symbol as undocumented. Re-scan with a current build to
audit documentation.

## Scope and bounds

- **Rust only**, at the **current** graph state, over the **library crate
  rooted at `src/`** (excluding `src/bin/**`, tests, examples, benches) —
  the same scope as `eg query public-api`.
- Default audited kinds: `function`, `struct`, `enum`, `trait`, `type_alias`,
  `const`, `static`, plus `pub use` re-export rows whose target resolves
  in-graph. `--include-private` adds `method` declarations.
- **Modules** carry no doc-comment fact and are excluded from the audit
  (tallied in `counts.modules_excluded`), never guessed.
- A re-export whose target does **not** resolve in-graph cannot have its doc
  presence asserted: it yields a `reexport_target_unresolved` diagnostic and
  is excluded, never guessed.
- Surface diagnostics (`glob_reexport_unresolved`,
  `module_visibility_unknown`, `symbol_visibility_missing`) pass through so
  known blind spots stay visible.
- Out of scope: doc quality assessment, auto-generating documentation,
  ranking by importance, and non-Rust languages.

## Output shape

Deterministic, byte-identical across repeated runs on an unchanged graph.
Items are sorted by (`path`, `kind`, `record_id`); `--limit` truncates after
sorting and never reorders.

```json
{
  "ok": true,
  "language": "rust",
  "capability": "doc_facts_recorded",
  "disclaimer": "Asserts the presence or absence of a recorded doc comment ...",
  "items": [
    {
      "record_id": "codegraph:v5:...",
      "kind": "struct",
      "path": "BareStruct",
      "visibility": "public",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 120, "end_byte": 142, "start_line": 12, "end_line": 12 },
      "signature": "struct BareStruct;",
      "evidence": ["externally_reachable", "doc_comment_absent"]
    },
    {
      "record_id": "codegraph:v5:...",
      "kind": "struct",
      "path": "Hidden",
      "visibility": "public",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 300, "end_byte": 325, "start_line": 19, "end_line": 19 },
      "evidence": ["externally_reachable", "doc_comment_absent"],
      "via_reexport": true,
      "target": "internal::Hidden",
      "target_record_id": "codegraph:v5:..."
    }
  ],
  "counts": {
    "considered": 9,
    "documented": 5,
    "undocumented": 4,
    "reexports": 1,
    "modules_excluded": 2,
    "reexports_unresolved": 0,
    "doc_capture_missing": 0
  },
  "diagnostics": []
}
```

- `evidence` names exactly what the row asserts: `doc_comment_absent` always,
  plus `externally_reachable` on public-surface rows. `--include-private`
  rows omit the reachability claim and carry their declared `visibility`
  class instead.
- Re-export rows are attributed to the **re-export site** (the `pub use`
  line's file and span), with the resolved declaration whose doc fact was
  checked cited in `target_record_id`.
- Every row carries a stable `record_id` plus a repo-relative `file` + `span`
  citation — the debt list is checkable, not just readable.

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# List externally-reachable public symbols with no doc comment
eg query undocumented --graph graph.jsonl

# Cap the triage list; widen to a whole-crate doc audit
eg query undocumented --graph graph.jsonl --limit 20
eg query undocumented --graph graph.jsonl --include-private

# Scope to one repository in a multi-repo store
eg query undocumented --graph graph.jsonl --repo acme/widget
```

## Relationship to neighboring tools

- **rustdoc `missing_docs` lint** — requires a successful build, fires only
  on its own visibility view, and emits stderr warnings rather than a
  deterministic, citable, queryable set; cannot be scoped per-repo in a
  shared store.
- **`rg "pub fn"` + eyeballing for `///`** — no reachability awareness, no
  re-export handling, no `#[doc]` recognition, no citable handle.
- **`eg query public-api`** — the surface this lane consumes; use it to see
  the full contract, documented or not.
