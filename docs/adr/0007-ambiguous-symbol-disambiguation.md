# ADR 0007: Ambiguous Symbol Recall Is Disambiguated, Never Merged

## Status

Accepted

## Context

Issue #192: `symbol_context` recall on a name shared by two or more distinct
symbols (e.g. `Foo::new` and `Bar::new`) merged every matching identity's
evidence into one blended bundle — one symbol's observations, decisions, and
verification history attributed to another. The recall was wrong, and silently
so: nothing in the envelope distinguished a true single-symbol answer from a
merge.

The identity primitive already exists: ADR 0004 gives each symbol a stable
record ID, and the query layer already separates current-state recall from
history (tombstones exclude identities; temporal versions of one ID are one
identity). The missing rule is what a name recall does when the current-state
identity set has cardinality ≥ 2.

## Decision

A name recall that resolves to **two or more distinct current-state symbol
identities is ambiguous, not merged**:

- The `SymbolContext` carries empty sections and a `candidates` list — one
  entry per distinct identity (stable record ID, repo-relative `file:span`,
  deterministic order: path, span start line, record ID).
- The CLI (`eg query context`) and the MCP `symbol_context` tool surface a
  stable typed error, `ambiguous_symbol` (exit `1` on the CLI), listing the
  candidates with enough identity to re-query.
- Re-query is identity-pure: pass a candidate's `record_id` or `file:span`
  handle (`path:start_line-end_line`) via `--candidate` (CLI) or the
  `candidate` MCP argument, and the recall is anchored on exactly that record
  — zero sibling records in any section. No auto-ranking, no implicit
  selection; an unresolvable selector is `no_match`, never a guess.
- Single-match output shape is unchanged. Tombstone and history semantics are
  unchanged: only the identities the recall would otherwise have merged count
  toward ambiguity.

## Consequences

- Consumers must handle `ambiguous_symbol` explicitly (the MCP contract
  schema already permits object candidates alongside `ambiguous_handle`'s
  string candidates).
- `file:span` handles resolve against live symbols and fail closed when one
  handle matches two distinct identities.
- The candidate list is deterministic but carries no relevance ordering —
  that is deliberate; ranking would be a second, separate decision.
