# Test vs. Production Symbol Roles (issue #238)

**Status:** Active. This document is the single source of truth for the
`role` field's classification rule — the deterministic `test` / `production`
label stamped on every `Symbol` and `File` record the Rust extractor emits,
plus every `Module` record: a `#[cfg(test)] mod` declaration is itself the
lexical gate, so the `Module` record carries the same role its member symbols
get (trunk repair, 2026-09-18).

## The rule

`role` is a total, deterministic function of exactly three source signals —
never guessed, never inferred from names, comments, or heuristics:

| # | Signal | Closed definition |
|---|--------|-------------------|
| (a) | Test-family attribute on the item | The item carries an attribute whose own name is `test`, ends in `::test` (e.g. `#[tokio::test]`), is `bench`, or ends in `::bench` — with or without arguments. This is the same closed vocabulary as issue #240's `EntryPointKind::Test`; the extractor reuses `entry_point_kind_from_attribute`, it does not define a second vocabulary. Configuration attributes that merely mention these tokens (`#[cfg(test)]`, `#[cfg_attr(test, ...)]`) never match: only the attribute's own name is compared. |
| (b) | Lexical membership in a `#[cfg(test)]`-gated module | The item is lexically nested inside a `mod` (inline or out-of-line declaration) annotated with exactly `#[cfg(test)]`, directly or through an enclosing gated `mod`. The gate is inherited down the whole subtree: a `#[cfg(test)] mod tests { mod helpers { … } }` marks every symbol in `helpers` as test. The ancestry composition rule is conjunction-by-nesting — one gated ancestor anywhere on the module path suffices. The check consumes the literal `#[cfg(test)]` attribute text already captured by the extractor (issue #223's `has_cfg_test_attribute`); it does not parse general `#[cfg(...)]` predicates. When issue #190's gate fact lands, this check may consume its predicate instead of the local exact-match; the classified set is unchanged. |
| (c) | Integration-test / bench root location | The file's repo-relative path has `tests` or `benches` as its first segment (Cargo's integration-test and bench roots). Segment comparison is separator-agnostic (`/` and `\`), so CRLF/`\`-separator checkouts classify identically. |

Anything matching none of the three signals is `production`.

### Out-of-line modules

A `#[cfg(test)] mod helpers;` declaration gates a module whose body lives in
another file (`src/helpers.rs`), extracted with no view of the gating
attribute. The repo-wide pass (`apply_out_of_line_test_roles`, sibling of
issue #223's `apply_out_of_line_test_scope`) resolves these declarations to
their target files and marks the target `File` record — and transitively the
files those test-only files declare — as `test`. Because issue #238's signal
(b) explicitly covers out-of-line modules, the pass also marks the `Symbol`
records those files define as `test`: a symbol in a file that only compiles
under `cfg(test)` is test code even when its own file carries no lexical
signal.

**Production takes precedence for dual-use files**, exactly as in issue #223:
a module file also loaded by a non-test declaration still compiles into the
production build, so it — and the symbols it defines — keeps `production`.
Marking it `test` would hide production risk behind a test label.

### What `role` is not

- **Not** a confidence: `role` is a `TrustClass::SourceDerived` code-graph
  fact, drawn from the AST and the file path. It carries no agent-authored
  confidence and never overwrites or impersonates an agent observation (the
  PRD two-truths model).
- **Not** a test→target link: classifying a symbol's *own* role is this
  slice; mapping a production symbol to the tests that exercise it via call
  edges is issue #126 (the inverse direction).
- **Not** doctests or examples: `///` runnable examples and `examples/`
  binaries are `production` in this slice; a later slice may add
  `doctest` / `example` roles.
- **Not** an identity input: stamping `role` never moves a record ID
  (additive per `docs/schema/schema-versioning.md` §2).

## Query surface

`eg query symbol <name>` and `eg query file <path>` surface `role` on every
result row (`"role": "test"` / `"role": "production"` in JSON, `role: test`
in `--format text`; see `docs/cli/query.md`).

`--role <production|test|all>` (default `all`) scopes the answer:
`--role production` returns zero `test`-classified rows and vice versa. `all`
preserves the pre-#238 output exactly. Rows from graphs produced before issue
#238 carry no `role` (absent, never fabricated) and are excluded by either
concrete scope — absent is *unknown*, not *production*.

## Determinism proof sketch

Classification is a pure function of (attribute text, module-nesting path,
repo-relative path):

- attribute matching compares closed string sets — no ordering dependence;
- module ancestry is a source-order walk over a `Vec` stack, pushed/popped
  symmetrically;
- path segmentation splits on both separators and compares the first segment
  against a two-element closed set;
- the out-of-line pass resolves declarations against a `BTreeSet` of known
  paths and expands its fixpoints over sorted sets.

No wall-clock reads, no filesystem I/O, no hash-map iteration order leaks
into the verdict. Re-scanning unchanged source yields byte-identical `role`
values and record ordering across runs and across OS line-ending /
path-separator checkouts (the extractor normalizes line endings at the parse
boundary, issue #242, before Tree-sitter sees the source).

## Success metric (how to check)

On Egregore's own repository every symbol carries a `role` matching a
hand-labeled fixture for ≥99% of symbols; `eg query symbol --role production`
and `eg query file --role production` return zero symbols that are
`#[test]`-annotated, inside a `#[cfg(test)]` module, or under
`tests/`/`benches/`; classification is byte-identical across 5 consecutive
scans.
