# eg query who-constructs

List the symbols that construct a type via a `Type { … }` struct/enum literal —
"who builds this type?" — as a precise, citable construction-site inventory over
the `CONSTRUCTS` edges the Rust extractor mints (issue #471, over PR #467 /
issue #443).

This is the type-anchored, first-class form of the `construction_sites` group
that [`eg query change-impact <type>`](change-impact.md) already surfaces (the
interim answer shipped in #467): both read the same `CONSTRUCTS` edges and
expose the same `e0063_risk` signal, but this lane makes "who constructs this
type?" a dedicated question rather than a slice of a broader blast-radius
envelope. It is the inbound, type-anchored mirror of [`eg query deps`](deps.md)
and the symmetric partner to [`eg query who-imports`](who-imports.md).

## Synopsis

```text
eg query who-constructs <TYPE> --graph <PATH>   [--repo <SELECTOR>] [--at <SHA> | --as-of <RFC3339>] [--at-head | --all-history] [--format json|text]
eg query who-constructs <TYPE> --data-dir <DIR> [--repo <SELECTOR>] [--at <SHA> | --as-of <RFC3339>] [--at-head | --all-history] [--format json|text]
```

This is a read-only lookup: it never re-parses source and never scans comment or
string text. Only extractor-minted `CONSTRUCTS` edges are consulted — a
provably-resolved `Type { … }` construction site (PR #467 emits an edge **only**
on a unique provable resolution; ambiguous / external / unresolved sites stay
unbound, the no-wrong-edge doctrine). A doc-comment or string mention of the type
name produces **no** row — the precision win over `grep`.

Rows are construction-site **leads, not proof a field addition breaks**: a
`CONSTRUCTS` edge records that a symbol's body writes a literal of the type; the
`e0063_risk` flag is the actionable signal, never a compile-tested verdict.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<TYPE>` | yes | The type to look up: an exact type **name** (`Deal`) or a canonical **record ID** (`codegraph:v7:…`) naming the struct/enum definition symbol. An empty/blank handle is rejected (exit `1`, `malformed_type_handle`). A name matching more than one live type is ambiguous (exit `1`). A handle resolving to a file, task, or other non-symbol node is unsupported (exit `1`). |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. Read from a throwaway copy — the store is never mutated. |
| `--repo <SELECTOR>` | no | Restrict the constructor set to one repository in a multi-repo store (see [query.md](query.md#repository-scope---repo-issue-67)). |
| `--at <SHA>` | no | Resolve construction sites at a single commit (full SHA or unique prefix). Mutually exclusive with `--as-of` and the corpus flags. |
| `--as-of <RFC3339>` | no | Resolve construction sites as of a valid-time instant. Mutually exclusive with `--at` and the corpus flags. |
| `--at-head` | no | Force the HEAD-anchored corpus (the default when a snapshot exists; makes it explicit). |
| `--all-history` | no | Read the union of all commit snapshots. |
| `--format` | no | `json` (default) or `text`. |

## Semantics

- **Handle resolution.** The `<TYPE>` handle resolves through the shared symbol
  resolver, so a record ID or an exact name resolves identically to `deps` /
  `change-impact`. `CONSTRUCTS` edges target the type's **definition symbol**, so
  a file path or task handle can never be the anchor and is rejected as
  unsupported.
- **What a row is.** For the resolved type, every live inbound `CONSTRUCTS` edge
  yields one row citing the **constructing** symbol's stable `record_id`, name,
  node `kind`, and repo-relative `repo_relative_path` / `span` handle, plus the
  producing `edge_record_id`. Construction is a caller-granularity relation:
  per-site spans collapse to the constructing symbol, exactly as `CALLS` does. A
  constructor that builds the type more than once appears **once** (the sites
  collapse to one edge).
- **`e0063_risk` — the actionable flag.** `true` when at least one collapsed
  construction site for this (constructor, type) pair uses the **exhaustive,
  non-`..base`** literal form — the form that fails to compile (rustc **E0063**)
  when a required field is added to the type. `false` when every collapsed site
  used the struct-update `..base` (functional-update, FRU) form, which stays
  valid across a field addition. Derived as `is_exhaustive.unwrap_or(true)` — a
  legacy edge with no marker is conservatively treated as risky.
- **`is_exhaustive` — the raw marker.** The edge's recorded exhaustiveness
  marker (OR-aggregated across sites that collapse to one edge), present as
  `is_exhaustive`; omitted on a legacy edge that predates the marker.
- **Liveness is latest-write-wins.** A constructing symbol tombstoned and not
  re-added is excluded; one re-added after its tombstone is included, and only
  the latest write of a stable `CONSTRUCTS` edge id supplies a row — via the
  shared `Liveness` gate (issue #421), so `--graph` and `--data-dir` agree on
  tombstoned / revived construction sites.
- **Corpus modes (issue #427).** A `scan-history` store **defaults to the
  HEAD-anchored corpus** (sites current at each repository's stamped
  `source_snapshot` HEAD), so a constructor (or the edge, or the type) removed at
  HEAD does not appear. `--all-history` reads the union of all commit snapshots;
  `--at-head` makes the default explicit; `--at` / `--as-of` pin a single commit.
  The flags are mutually exclusive with each other and with `--at` / `--as-of`
  (exit `1`, `unsupported_combination`). A plain `scan` store is a single
  snapshot (`single_snapshot`). The envelope discloses `corpus_mode`,
  `corpus_mode_source`, and `corpus_disclaimer` — see
  [corpus-modes.md](corpus-modes.md).
- **Deterministic output.** Rows are sorted by `repo_relative_path`, then
  `span.start_line`, then `record_id`, and are byte-identical across runs and
  across `--graph` / `--data-dir`.

## Output

Newline-delimited JSON: a summary envelope line, then one row per constructing
symbol.

### Envelope (first line)

| Field | Description |
|-------|-------------|
| `ok` | `true` on a match. |
| `handle` | The `<TYPE>` handle as supplied. |
| `target` | The resolved type's citable handle: `record_id`, `schema_version`, `name`, `kind`, `repo_relative_path`, `span`. |
| `direction` | Always `inbound`. |
| `edge_label` | Always `CONSTRUCTS`. |
| `at_commit` | The resolved commit SHA under `--at` / `--as-of`; omitted otherwise. |
| `as_of` | The `--as-of` instant, when supplied. |
| `total_constructors` | Number of rows that follow. |
| `corpus_mode` / `corpus_mode_source` / `corpus_disclaimer` | Corpus disclosure (issue #427). |
| `disclaimer` | The leads-not-proof contract. |

### Row (one per line)

| Field | Description |
|-------|-------------|
| `record_id` | Stable record ID of the constructing symbol. |
| `schema_version` | Schema version of the constructing symbol. |
| `name` | Name of the constructing symbol. |
| `kind` | Node kind (e.g. `Symbol`). |
| `repo_relative_path` | Repo-relative path of the constructing symbol's file. |
| `span` | Source span of the constructing symbol. |
| `edge_record_id` | Stable record ID of the `CONSTRUCTS` edge. |
| `e0063_risk` | `true` when a new required field would break this site (exhaustive form); `false` for the FRU `..base` form. |
| `is_exhaustive` | The raw edge marker; omitted on a legacy edge. |
| `trust` | Always `source_fact`. |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one constructor found. |
| `1` | Malformed handle (`malformed_type_handle`), ambiguous name (all candidate record IDs reported), an unsupported handle (file/task/other non-symbol), or a conflicting corpus/selector combination (`unsupported_combination`). Machine-readable JSON on stderr (handle errors) or stdout (corpus conflicts). |
| `2` | A well-formed type that resolved but has **zero** live constructors, or an unknown type (`no_match`, JSON on stdout). |

## Examples

```powershell
# Who builds the Deal struct?
eg query who-constructs Deal --graph graph.jsonl

# Only the exhaustive (E0063-breakable) sites are worth reviewing before adding a field:
eg query who-constructs Deal --graph graph.jsonl --format text   # each row flags [e0063_risk] / [fru_safe]

# Resolve by canonical record ID, scoped to one repo, at a past commit:
eg query who-constructs codegraph:v7:<hex> --graph history.graph.jsonl --repo acme/widget --at <sha>

# The union of all commit snapshots (a site removed at HEAD still appears):
eg query who-constructs Deal --graph history.graph.jsonl --all-history
```

## Relationship to `change-impact`

`eg query change-impact <type>` returns a `construction_sites` inbound group as
part of a broader blast-radius envelope (direct callers/callees, referencing
files, implementations, containing context). `who-constructs` is the dedicated,
type-anchored lane for the same underlying `CONSTRUCTS` edges and the same
`e0063_risk` signal — use it when the question is specifically "who builds this
type?" and you want a focused, sorted, paginated construction-site inventory
rather than a slice of the impact envelope.

## Scope (carried over from #443 / #467)

Out of scope for this lane, mirroring the extraction scope of #467: tuple-struct
construction `T(...)` (already `CALLS`-shaped), plain enum unit/tuple variants,
and module-level literals outside any symbol body. Only provably-resolved
struct-literal / enum-struct-variant sites within symbol bodies mint a
`CONSTRUCTS` edge and therefore appear here.
