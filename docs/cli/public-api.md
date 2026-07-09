# eg query public-api

Enumerate the crate's **externally-reachable public API surface** — the set of
items this crate actually exports to the outside world — from recorded
per-symbol visibility (issue #124) and module containment. Local-first, no
network, no build.

> **Parse-derived enumeration, not a build-verified or semver claim.** The
> surface is computed from what the code graph recorded at scan time.
> Breaking-change classification across a commit range is
> `eg query public-api-deltas` (issue #157), which consumes this snapshot —
> see `docs/cli/public-api-deltas.md`.

A `pub` token grep is wrong, not just noisy: a `pub fn` inside a private `mod`
is **not** externally reachable, and a `pub use private::Foo;` re-export
**is**. This query computes true module-chain reachability instead:

- An item is externally reachable when its own recorded visibility is
  `public` **and** every module on its containment chain is recorded `public`.
- A `pub use` re-export at a reachable site adds its leaves to the surface,
  **attributed to the re-export site** (the `pub use` line's file and span),
  with the resolved in-graph target cited by record ID when it resolves.
- `pub(crate)`, `pub(super)`, and `pub(in path)` items are crate-internal:
  excluded from the surface and tallied in `counts.crate_internal`.
- `pub` items trapped inside non-`pub` modules are excluded and tallied in
  `counts.trapped_public`.

## Synopsis

```text
eg query public-api --graph <PATH>    [--repo <SELECTOR>]
eg query public-api --data-dir <DIR>  [--repo <SELECTOR>]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the surface to one repository in
a multi-repo store; an unknown or ambiguous selector is rejected with a
machine-readable stderr diagnostic (exit 1), never resolved implicitly.

| Condition | Exit | Output |
|-----------|------|--------|
| Surface computed — including an **empty** surface | `0` | Public-api JSON on stdout, `ok:true` |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input | `1` | Error message on stderr |

An empty or absent surface is a **real, successful, explicit answer**: exit 0
with `ok:true`, an empty `items` array, and an `empty_surface` diagnostic —
never a silent empty payload, never synthesized items (AC6).

## Scope and bounds

- **Rust only**, at the **current** graph state. Records from other languages
  never contribute; when a stable ID appears more than once (history graphs)
  the latest record wins deterministically.
- The surface is the **library crate rooted at `src/`**. `src/bin/**` holds
  separate binary crates; `tests/`, `examples/`, and `benches/` are separate
  target crates — `pub` items there are never part of the library contract.
- Enumerated item kinds: `function`, `struct`, `enum`, `trait`, `type_alias`,
  `const`, `static`, `module`, plus re-export rows. Methods, trait members,
  and `impl` blocks are declaration details of their owning items, not
  surface items.
- Glob re-exports (`pub use m::*;`) cannot be enumerated without name
  resolution: they yield a `glob_reexport_unresolved` diagnostic naming the
  re-export record — never guessed item rows.
- A module with no recorded visibility (dead file, pre-#213 scan) makes items
  beneath it non-provable: they are excluded and a
  `module_visibility_unknown` diagnostic names the module. Re-scan with a
  current build to record module visibility.

## Output shape

Deterministic, byte-identical across repeated runs on an unchanged graph.
Items are sorted by (`path`, `kind`, `record_id`).

```json
{
  "ok": true,
  "language": "rust",
  "disclaimer": "Parse-derived enumeration of the externally-reachable public API surface ...",
  "items": [
    {
      "record_id": "codegraph:v4:...",
      "kind": "struct",
      "path": "api::Widget",
      "visibility": "public",
      "repo_relative_path": "src/api.rs",
      "span": { "start_byte": 0, "end_byte": 42, "start_line": 3, "end_line": 5 },
      "signature": "struct Widget"
    },
    {
      "record_id": "codegraph:v4:...",
      "kind": "struct",
      "path": "Secret",
      "visibility": "public",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 120, "end_byte": 144, "start_line": 9, "end_line": 9 },
      "via_reexport": true,
      "target": "internal::Secret",
      "target_record_id": "codegraph:v4:..."
    }
  ],
  "counts": {
    "externally_reachable": 2,
    "reexports": 1,
    "crate_internal": 3,
    "private": 12,
    "trapped_public": 4
  },
  "diagnostics": []
}
```

- `path` is the externally visible crate-relative fully-qualified path; for a
  re-export it is the path created **by** the re-export (alias-aware), not the
  target's original path.
- `signature` is the declaration header persisted by issue #124, joined when
  present; module and re-export rows carry none.
- Every row carries a stable `record_id` plus a repo-relative `file` + `span`
  citation — the enumeration is checkable, not just readable.

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# Enumerate the externally-reachable public API surface
eg query public-api --graph graph.jsonl

# Scope to one repository in a multi-repo store
eg query public-api --graph graph.jsonl --repo acme/widget
```

## Relationship to neighboring tools

- **`rg "pub "`** — counts `pub` tokens with zero module-reachability
  awareness; over-reports trapped items, misses re-exports.
- **`cargo public-api` / `cargo-semver-checks`** — build-backed and precise,
  but require a successful build per revision. This query is parse-only and
  works on code that does not currently compile.
- **`eg query public-api-deltas` (issue #157)** — the breaking-change
  classification layer across a commit range; consumes this surface rule.
  See `docs/cli/public-api-deltas.md`.
