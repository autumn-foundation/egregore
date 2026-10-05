# Symbol complexity (issue #162)

Deterministic per-symbol structural complexity for hotspot ranking.

## Field

`GraphRecord::Node` carries an optional field:

- `complexity: Option<u32>` — present on Rust callable `Symbol` nodes
  (`function` / `method` / `test` symbol kinds), omitted everywhere else.

Serialized as `"complexity": <u32>` inside the node object, `None` omitted via
`skip_serializing_if`. Additive per [schema-versioning §2](schema-versioning.md):
readers that predate the field ignore it, and old records simply carry no
score — no schema-version bump, no backfill.

## Metric

`complexity = 1 + (decision points in the item's own body)`, where each of
the following counts one:

- `if` / `else if`
- `for`
- `while`
- `loop`
- each `match` arm
- each `?`
- each `&&`
- each `||`

Scoping rules:

- Closure bodies count toward the enclosing callable.
- Nested `fn` items get their own symbol; their bodies do not count toward
  the enclosing callable.
- Signature-only trait methods score `1` (no body, no decision points).

The score is computed from the Tree-sitter parse of the source at extraction
time. It is a **source-derived code fact** (`TrustClass::SourceDerived`),
never agent confidence. It is deterministic: identical source (up to
line-ending style) always yields the identical score.

## Invariants

- Complexity never participates in stable IDs or schema versions; it is a
  derived measurement, not identity.
- It is stored as a plain non-interned integer property in the embedded
  `AletheiaDB` adapter (write and read paths handle it symmetrically, so
  repeated ingests cannot create phantom versions carrying stale scores).
- `eg query symbol` exposes the score on every callable row as `complexity`;
  the field is omitted for non-callables and for records that predate
  issue #162 — absence means *unknown/inapplicable*, never zero.
- `eg query complexity` ranks scored symbols: complexity descending, then
  qualified name ascending (documented tie-break), then symbol record ID
  ascending. See [docs/cli/complexity.md](../cli/complexity.md).
- Together with [`eg query churn`](../cli/churn.md) (change frequency), the
  two multiplicands of a CodeScene-style hotspot are both queryable. Fusing
  them into one churn×complexity score is out of scope for this slice.
