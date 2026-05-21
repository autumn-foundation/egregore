# ADR 0004: Stable Symbol Identity

## Status

Accepted

## Context

Egregore stores code facts in a temporal graph. A symbol ID must name the same
logical symbol across repeated scans and across Git history replay when the
symbol survives unchanged. The previous Rust extractor used `span.start_byte` as
part of `Symbol` stable ID construction. That made the ID sensitive to unrelated
edits above the declaration: adding a comment, reordering imports, or formatting
earlier code shifted byte offsets and produced a new symbol record.

That is the wrong identity model for a persisted graph. `span` is still useful
for reads and citations, but it is a coordinate, not an identity component.

Prior art points the same way:

- SCIP descriptors identify symbols with `<scheme> <package> <descriptor>...`;
  source coordinates are attached data, not the descriptor.
- LSIF monikers identify cross-tool symbols by scheme, identifier, and kind;
  byte offsets are not moniker components.
- Glean-style facts key symbols by file, name, kind, and a structured collision
  key rather than by line or byte position.

## Decision

Within a repository namespace, a Rust `Symbol` identity is:

```text
(repo_relative_path, symbol_kind, qualified_name, disambiguator)
```

The concrete codegraph stable ID is namespaced by the `Repository` node ID so
two different repositories can contain the same file path and symbol name
without colliding:

```text
stable_id([
  "node",
  "symbol",
  symbol_kind,
  repository_id,
  repo_relative_path,
  qualified_name,
  disambiguator,
])
```

`span.start_byte`, `span.end_byte`, `span.start_line`, and `span.end_line` are
explicitly rejected as identity inputs. They remain on every `Symbol` record as
non-identity fields so queries can return file coordinates.

## Disambiguator Algorithm

The `disambiguator` is a deterministic ordinal scoped to:

```text
(repo_relative_path, symbol_kind, qualified_name)
```

The Rust extractor walks the Tree-sitter AST in source order. When it emits a
symbol, it increments the ordinal for that tuple:

- first occurrence: `disambiguator = 0`
- second occurrence: `disambiguator = 1`
- subsequent occurrences: `2`, `3`, ...

The tie-breaking rule is declaration order in the AST traversal. This keeps
single occurrences stable at `0` while allowing real collisions, such as two
`impl Widget` blocks in one file, to remain distinguishable. If the colliding
declarations are reordered, their ordinals follow source order; rename and move
tracking are separate future graph facts.

## Diagnostics

Macro-invocation diagnostics also used `span.start_byte` historically. A
diagnostic is not a symbol, but the same byte-shift failure mode applies to an
unsupported macro invocation that survives a header-only edit. The Rust extractor
therefore uses:

```text
(repo_relative_path, diagnostic_kind_or_invocation_name, disambiguator)
```

with the same source-order ordinal rule. The diagnostic `span` remains attached
as a non-identity field.

## Consequences

- Prepending comments or whitespace to a file changes symbol spans but not
  symbol IDs.
- `scan-history` emits the same symbol ID for an unchanged symbol at commit A
  and commit B, so temporal observations share one logical handle.
- Edges such as `DEFINES`, `CALLS`, `MENTIONS`, `OBSERVES`,
  `MENTIONS_SYMBOL`, `EXPLAINS`, and `VALIDATED_BY` can target symbol IDs that
  do not rot after unrelated edits.
- Cross-crate canonical identity remains out of scope. Cargo package/version
  descriptors can refine the repository namespace later without reintroducing
  source coordinates into symbol identity.
- Symbol rename and move tracking remain out of scope. A rename or move across
  files produces a new symbol ID; a future semantic-drift or rename edge can
  stitch the history.

## Migration Note

No persisted ingest workflow has been frozen for downstream consumers yet. The
daemon and embedded adapter paths are still pre-contract for production stores,
so this is a non-breaking change for production data. Test fixtures that
implicitly relied on byte-offset-derived IDs are updated with this ADR. Future
consumers may rely on the post-ADR scheme as the stable symbol identity contract.
