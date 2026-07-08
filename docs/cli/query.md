# eg query

Query an existing graph JSONL for symbols, files, who last changed a symbol, semantic drift records, or by natural-language similarity.

## Synopsis

```text
eg query symbol   <NAME>  --graph <PATH>    [--at <COMMIT>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query symbol   <NAME>  --data-dir <DIR>  [--at <COMMIT>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query symbols  <PATTERN> --graph <PATH>  [--case-insensitive] [--repo <SELECTOR>] [--format json|text]
eg query symbols  <PATTERN> --data-dir <DIR> [--case-insensitive] [--repo <SELECTOR>] [--format json|text]
eg query file     <PATH>  --graph <PATH>    [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query file     <PATH>  --data-dir <DIR>  [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query who      <NAME>  --graph <PATH>    [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query who      <NAME>  --data-dir <DIR>  [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query drift            --graph <PATH>    [--limit N] [--repo <SELECTOR>] [--format json|text]
eg query drift            --data-dir <DIR>  [--limit N] [--repo <SELECTOR>] [--format json|text]
eg query semantic <QUERY> --data-dir <DIR>  [--limit N] [--repo <SELECTOR>] [--format json|text]
eg query semantic-context <QUERY> --data-dir <DIR> [--limit N] [--min-score F] [--repo <SELECTOR>]
eg query semantic-memory <QUERY> --data-dir <DIR> [--limit N] [--repo <SELECTOR>] [--verified-only] [--format json|text]
eg query context  <NAME>  --graph <PATH>    [--repo-path <DIR>]
eg query task     <HANDLE> --graph <PATH>
eg query memory   <HANDLE> --graph <PATH>   [--verified-only]
eg query failures <HANDLE> --graph <PATH>   [--repo <SELECTOR>]
eg query change-impact <HANDLE> --graph <PATH> [--repo <SELECTOR>] [--depth N]
eg query transitive-callers <HANDLE> --graph <PATH> [--repo <SELECTOR>] [--max-depth N] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query deps     <HANDLE> --graph <PATH>   [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query deltas   <BASE> <HEAD> --graph <PATH> [--repo <SELECTOR>]
eg query coupling <PATH>  --graph <PATH>    [--repo <SELECTOR>] [--base <COMMIT> --head <COMMIT> | --at <COMMIT> | --as-of <RFC3339>] [--min-support N] [--limit N] [--format json|text]
eg query public-api       --graph <PATH>   [--repo <SELECTOR>]
eg query undocumented     --graph <PATH>   [--repo <SELECTOR>] [--limit N] [--include-private] [--format json|text]
eg query ownership [PATH] --graph <PATH>   [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--threshold <PERCENT>] [--limit N] [--format json|text]
eg query unreferenced     --graph <PATH>   [--repo <SELECTOR>]

eg query at       <PATH>:<LINE> --graph <PATH> [--at <COMMIT>] [--repo <SELECTOR>]
```

Evidence-backed audit subcommands have their own pages:

- `eg query context` — evidence-backed context for a **symbol** (issue #38).
- `eg query semantic-context` — **natural-language query → evidence-backed
  context** for the top-N semantic matches in one call
  ([semantic-search-guidance.md](semantic-search-guidance.md), issue #90).
- `eg query task` — evidence for a **task** ([task-queries.md](task-queries.md), issue #48).
- `eg query memory` — audit the evidence behind one **agent-authored memory
  claim** ([memory-audit.md](memory-audit.md), issue #64).
- `eg query semantic-memory` — recall prior **agent memory by meaning** with
  provenance, trust-separated from code
  ([semantic-memory-recall.md](semantic-memory-recall.md), issue #91). Supports
  recall-time filtering of superseded and contradicted memories
  ([recall-supersession.md](recall-supersession.md), issue #92).
- `eg query failures` — **prior failed attempts** linked to a code or task
  handle ([failure-history.md](failure-history.md), issue #63).
- `eg query change-impact` — **graph-derived impact leads** grouped by relation
  for a symbol or file handle, for blast-radius triage before editing
  ([change-impact.md](change-impact.md), issue #76).
- `eg query transitive-callers` — the **transitive inbound-reachable set** of a
  symbol with one concrete connecting call path per row, bounded by
  `--max-depth`, deterministic under cycles, honoring `--at`/`--as-of`
  temporal views, with `CALLS` resolution labels propagated along each path
  ([transitive-callers.md](transitive-callers.md), issue #139).
- `eg query deps` — the **direct outbound dependencies** of a symbol — what it
  calls, implements, imports, and references — labeled by edge type, with
  unresolved targets as an explicit category, honoring `--at`/`--as-of`
  temporal views ([deps.md](deps.md), issue #123).
- `eg query deltas` — **symbol- and file-level changes between two commits**,
  grouped by stable change class with citable handles
  ([deltas.md](deltas.md), issue #118).
- `eg query coupling` — the files that **historically changed in the same
  commits** as a target file, ranked by a documented normalized coupling
  strength with a minimum-support threshold; historical co-change leads,
  never dependency proof ([coupling.md](coupling.md), issue #153).
- `eg query public-api` — the crate's **externally-reachable public API
  surface** from recorded visibility and module containment, re-exports
  included, trapped `pub` items excluded
  ([public-api.md](public-api.md), issue #213).
- `eg query undocumented` — **externally-reachable public symbols with no
  recorded doc comment**, each row carrying citable evidence;
  presence/absence only, never doc quality
  ([undocumented.md](undocumented.md), issue #257).
- `eg query ownership` — **per-file authorship aggregates with a primary
  owner and bus-factor signal** from a history store: ranked ownership
  shares, deterministic tie-breaks, `--at`/`--as-of` time travel, and an
  explicit empirical-not-declared boundary
  ([ownership.md](ownership.md), issue #245).
- `eg query unreferenced` — symbols with **no recorded inbound reference
  edges**, as prune-triage leads with citable handles — never proof of dead
  code ([unreferenced.md](unreferenced.md), issue #113).

- `eg query at` — resolve a **`file:line` location to its smallest enclosing
  code symbol**, with the enclosing chain reported outermost → innermost
  ([below](#eg-query-at), issue #151).

Most subcommands accept exactly one input source:

- `--graph <PATH>` — read from a JSONL file produced by `eg scan` or `eg scan-history`.
- `--data-dir <DIR>` — read from an embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. Requires the `embedded-aletheiadb` feature (enabled by default). Providing both `--graph` and `--data-dir` is an error.

`eg query semantic`, `eg query semantic-context`, and `eg query semantic-memory` accept **only** `--data-dir`. The store must additionally have been populated with the `--embed` flag (`eg ingest --adapter embedded --data-dir <DIR> --embed`); a store without embeddings returns no results. `eg query semantic` returns only deterministic **code** hits; `eg query semantic-memory` returns only **agent-authored** memory hits — the two are never blended (issue #91). `eg query semantic-context` follows the `eg query context` no-match convention: on no semantic hit clearing `--min-score` it prints `{"ok":false,"error":{"code":"no_match",...}}` to **stdout** and exits `2`.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one result was found and printed. |
| `1` | An error occurred (missing file, malformed JSONL, ambiguous commit prefix, unknown/ambiguous repository selector, ambiguous unscoped repository collision). A single-line message is written to stderr. No partial JSON appears on stdout. |
| `2` | No match found. A single-line message is written to stderr. Stdout is empty. |

## Output format

### `--format json` (default)

One JSON object per line (JSONL). Field names are stable across releases. Machine consumers should depend only on the fields documented here; additional fields may be added later.

### `--format text`

One human-readable line per result for terminal use. The exact format is not stable and must not be parsed by scripts.

---

## Store freshness (`--repo-path`, issue #82)

`eg query symbol`, `eg query file`, and `eg query context` accept an optional
`--repo-path <DIR>` pointing at a working tree. When set, each result carries a
non-fatal `freshness` field with a stable code (`fresh` / `stale_head` /
`stale_dirty` / `unknown`) computed by comparing the store's stamped
[source snapshot](../schema/source-snapshot.md) against that working tree, so an
agent can downgrade trust in a cited `repo_relative_path` + `span` handle. The
result is **never suppressed** on a non-`fresh` verdict.

Without `--repo-path` the `freshness` field is absent and output is unchanged.
See [`freshness.md`](freshness.md) for the standalone store-level report.

---

## Extraction completeness (`extraction_completeness`, issue #87)

`eg query symbol` and `eg query file` return an `extraction_completeness` signal (`complete` / `partial`) in their JSON output format.
- `complete`: The queried file or symbol scope contains zero `Diagnostic` markers (unparsed macro blocks or syntax errors).
- `partial`: The scope contains at least one active `Diagnostic` marker, indicating that code extraction may have been incomplete, and some symbols or elements within this scope might be missing from the index.

For `eg query file <file>`, if the completeness is `partial`, the output also includes a `diagnostics` array enumerating the active diagnostic markers within that file (each containing `record_id`, `repo_relative_path`, and `span`).

The completeness signal is advisory and states only that extraction *may* be incomplete in the scope; it does not make direct truth claims about symbol presence or absence.


---

## Cross-Producer Determinism & Portable Spans

Egregore guarantees cross-platform and cross-producer byte-for-byte stable scans for identical Git commits:

1. **Path Normalization**: All `repo_relative_path` and path-based fields are normalized to use forward slashes (`/`) as separators, ensuring that scans run on Windows do not leak backslashes (`\`) compared to scans on POSIX systems.
2. **Line-Ending Normalization (Portable Spans)**: Source files are normalized to use LF (`\n`) line endings before syntax parsing and span generation. As a result, all `SourceSpan` byte offsets (`start_byte`, `end_byte`) are completely portable and stable regardless of whether the local checkout has LF or CRLF (`\r\n`) line endings (e.g. from Git's `core.autocrlf` setting).
3. **Identity Stability**: Node and edge stable record IDs are computed on normalized inputs and match perfectly across platforms.

### Distinctions from other axes

- **VS. Freshness/Staleness (issue #82)**: Line-ending normalization ensures that identical content produces identical facts. Working-tree staleness (dirty files or HEAD changes) is an orthogonal axis tracked by the `freshness` signals.
- **VS. Extraction Accuracy (issue #93)**: Portable spans guarantee structural identity and portability of spans across platforms; they do not impact the linguistic precision or syntax coverage of the language-specific extractors.

---

## Repository scope (`--repo`, issue #67)

A shared local store can hold more than one repository, and two repositories
routinely contain the same repo-relative path (`src/lib.rs`) or the same
symbol name (`Widget`). `name` and `repo_relative_path` alone are therefore
not unique handles: a citation that names the right-looking path in the wrong
repository is worse than a clean no-match. Repository scope keeps the
repository boundary explicit on every public code query.

### Shortest local workflow

```sh
# Two local repositories scanned into one store — no network, no git remote
# required (operator overrides force distinct identities for plain dirs):
eg scan ./widget-a --repo-id-override widget-a --out a.jsonl
eg scan ./widget-b --repo-id-override widget-b --out b.jsonl
cat a.jsonl b.jsonl > store.jsonl

# Scope a query to exactly one repository:
eg query symbol widget --graph store.jsonl --repo widget-a
eg query file src/lib.rs --graph store.jsonl --repo widget-a
```

The same `--repo` flag works against an embedded store (`--data-dir`) and
through the daemon (`--daemon`), where it maps to the `repo` query-verb param
(see [`docs/schema/daemon-query.md` §5.1](../schema/daemon-query.md)).

### Selector forms

`--repo` accepts the stable `Repository` record ID or any human-usable handle
from the repository identity payload
([`docs/schema/repository-identity.md`](../schema/repository-identity.md)):

| Selector | Example |
|----------|---------|
| Stable repository record ID | `codegraph:v4:e2aa0c…` |
| Display name (remote-derived `owner/name`) | `acme/widget` |
| Short name (final path segment of the display name) | `widget` |
| Basename / operator override | `widget-a` |
| Normalized remote URL | `https://github.com/acme/widget` |
| Root commit SHA (`local_root_commit` identities) | `83fa99…` |
| Canonical path (`local_path` identities) | `/home/me/src/widget` |

Tombstoned `Repository` records (e.g. after an identity change in an
incremental scan) are not selectable and never make a live repository's
selector ambiguous.

Failures are stable, machine-readable JSON on stderr with exit code `1`:

- `{"code":"unknown_repository_selector","selector":"…"}` — nothing matches.
- `{"code":"ambiguous_repository_selector","selector":"…","candidates":[…]}` —
  more than one repository matches (e.g. two repos sharing a basename). The
  candidates list every matching repository record ID; Egregore never picks
  one implicitly.

### When repository scope is required

- **Unscoped list queries** (`eg query symbol`, `eg query file`, `eg query
  drift`, `eg query semantic`) stay usable in a multi-repo store: every row
  carries `repository_id` and `repository`, and colliding rows from different
  repositories are returned side by side — never merged across the boundary.
- **Unscoped single-answer time views** (`eg query symbol --as-of`, `--at`)
  fail closed on a collision with
  `{"code":"ambiguous_repository","repositories":[…]}` and exit `1`, because a
  single row cannot represent two repositories. Re-run with `--repo`. Rows the
  store topology cannot attribute (legacy records) count as their own
  candidate group; when one contributes to the collision the diagnostic adds
  `"includes_unattributed_rows":true`.
- **Scoped `eg query file`** excludes a matching path in another repository
  from the result set and reports it only through a stderr diagnostic:
  `{"code":"excluded_other_repositories","repo_relative_path":"src/lib.rs",
  "excluded_repository_count":1,"excluded_row_count":2}`.

Repository scope composes with the temporal selectors: `--repo` answers
"which repository?", `--at`/`--as-of`/`--tx-as-of` answer "which time view?"
([`docs/schema/temporal-selectors.md`](../schema/temporal-selectors.md));
neither dimension silently widens the other.

### Compared to the boring alternatives

| Tool | How it scopes | What it lacks here |
|------|---------------|--------------------|
| `rg` / `git grep` / `git log -S` from one checkout | Implicitly: you run it inside a single working tree | No shared store, no temporal graph, no drift/semantic/evidence joins; scoping disappears the moment results from several checkouts are merged into one report |
| GitHub Code Search `repo:owner/name` ([syntax](https://docs.github.com/en/search-github/github-code-search/understanding-github-code-search-syntax)) | Explicit `repo:` qualifier | Hosted-only; no local store, no AletheiaDB trust separation, no valid/transaction-time views |
| Sourcegraph `repo:^github\.com/acme/widget$` ([docs](https://sourcegraph.com/docs/code-search/queries)) | Explicit `repo:` filter with revision scoping | Server deployment; not a local-first evidence-citable graph that joins to agent memory and verification records |
| rust-analyzer / SCIP workspace navigation | Project-root-relative documents inside one workspace | Single-workspace by construction; cannot answer for a shared store where two repositories carry the same project-root-relative path |

Egregore's slice is narrower than any of these on raw search, and that is the
point: a repository-scoped, locally citable row (`record_id` +
`repository_id` + path + span) that later joins to memory, task, artifact, and
verification domains without leaving the shared store.

---

## eg query symbol

Find `Symbol` nodes by name.

```text
eg query symbol <NAME> --graph <PATH> [--at <COMMIT>] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<NAME>` | yes | Exact symbol name to look up. |
| `--graph <PATH>` | yes | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--at <COMMIT>` | no | Restrict to the single best record whose `git_commit` starts with this SHA prefix. Exit `1` with `error: ambiguous commit prefix` when the prefix matches more than one distinct commit SHA. Requires a history graph. |
| `--repo <SELECTOR>` | no | Restrict results to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable BLAKE3-based record ID (`codegraph:v1:…`). |
| `schema_version` | number | yes | Record schema version for the returned `Symbol`; see [`docs/schema/schema-versioning.md`](../schema/schema-versioning.md). |
| `name` | string | yes | Symbol name. |
| `kind` | string | yes | Always `"Symbol"`. |
| `repo_relative_path` | string or null | yes | Repository-relative file path, e.g. `"src/lib.rs"`. |
| `span` | object or null | yes | Source span with `start_byte`, `end_byte`, `start_line`, `end_line`. |
| `visibility` | string | Rust declaration-surface symbols | Declaration visibility class from the closed set `public` / `crate` / `restricted` / `private`, derived from the source `pub` modifier (`pub(in path)` and `pub(super)` map to `restricted`; `pub(self)` and no modifier map to `private`). Present on Rust `fn` / method / `struct` / `enum` / `trait` / type-alias / `const` / `static` symbols extracted at issue #124 or later; absent on `impl` symbols and records from older graphs. |
| `signature` | string | Rust declaration-surface symbols | Normalized declaration header: item keyword through the end of the parameter list / return type / where-clause for callables (or the item header for type-defining items), body excluded, interior whitespace collapsed deterministically. Same presence rules as `visibility`. |
| `doc` | string | when the item has a doc comment | Doc-comment text (`///` or `/** */`) after redaction policy v1 (see [`docs/schema/redaction.md`](../schema/redaction.md)); a secret-shaped value is replaced by a `<REDACTED:class:hash>` marker. Omitted entirely when the item has no doc comment — never an empty string. |
| `git_commit` | string | only in history graphs | Full commit SHA for history-backed records. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the row. Absent only for legacy graphs without repository topology. |
| `repository` | string | when attributable | Human-usable repository identity handle, e.g. `acme/widget`. |

### Example

```sh
eg scan . --out g.jsonl
eg query symbol scan_repository --graph g.jsonl
```

```json
{"record_id":"codegraph:v1:abc...","schema_version":1,"name":"scan_repository","kind":"Symbol","repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":500,"start_line":51,"end_line":71},"visibility":"public","signature":"fn scan_repository(repo_path:impl AsRef<Path>)->Result<Graph>","doc":"Scans a repository working tree into a code graph."}
```

---

## eg query symbols

Find `Symbol` nodes by **partial name** — a literal substring or an anchored
`*` glob — against the structural store, with no embedding model or `--embed`
store required (issue #102).

```text
eg query symbols <PATTERN> --graph <PATH> [--case-insensitive] [--repo <SELECTOR>] [--format json|text]
```

### Pattern semantics

| Pattern | Meaning | Example matches |
|---------|---------|-----------------|
| no `*` | Literal substring anywhere in the name. | `handle_` matches `handle_input`, `try_handle_x`. |
| contains `*` | Anchored glob over the **whole** name: each `*` matches any (possibly empty) run of characters; every other character is literal. | `handle_*` (prefix), `*_sink` (suffix), `Embedded*Adapter`. |

No other metacharacters are supported: this slice is literal substring plus
`*` glob only — full regular expressions and fuzzy/typo-tolerant ranking are
out of scope. An empty pattern is rejected as malformed (exit `1`).

Matching is **case-sensitive by default**; pass `--case-insensitive` to
compare both sides Unicode-lowercased.

Only `Symbol` node names are searched. Comments, string literals, and doc
text never produce a match — the false-positive class a raw `rg` search
cannot avoid. Tombstoned (deleted) symbols are excluded from current-state
results, in parity with `eg query file`.

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATTERN>` | yes | Substring or anchored `*`-glob name pattern (must be non-empty). |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store. Structural records only — the store does **not** need `--embed`. |
| `--case-insensitive` | no | Lowercase both pattern and names before matching. |
| `--repo <SELECTOR>` | no | Restrict results to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--format` | no | `json` (default) or `text`. |

### Output

Rows have the same shape and fields as [`eg query symbol`](#eg-query-symbol):
`record_id`, `schema_version`, `name`, `kind`, `repo_relative_path`, `span`,
the declaration-surface fields when present, `git_commit` for temporal
(history-graph) records, repository identity, and
`extraction_completeness`.

Output is deterministic and byte-stable: identical inputs produce identical
bytes, with rows sorted by `(repo_relative_path, span.start_line,
record_id)` ascending.

### Exit codes

Same contract as the other query subcommands, so a no-match is never
conflated with a store-absent or malformed-input condition:

| Code | Meaning |
|------|---------|
| `0` | At least one symbol matched and was printed. |
| `1` | Error: missing/malformed store, both or neither of `--graph`/`--data-dir`, empty pattern, unknown/ambiguous `--repo` selector. Stderr carries the message; stdout stays empty. |
| `2` | No symbol name matched the pattern. Stderr says `no match found for pattern ...`; stdout stays empty. |

### Example

```sh
eg scan . --out g.jsonl
eg query symbols 'handle_*' --graph g.jsonl
```

```json
{"record_id":"codegraph:v1:abc...","schema_version":1,"name":"handle_input","kind":"Symbol","repo_relative_path":"src/alpha.rs","span":{"start_byte":0,"end_byte":100,"start_line":10,"end_line":20},"extraction_completeness":"complete"}
{"record_id":"codegraph:v1:def...","schema_version":1,"name":"handle_request","kind":"Symbol","repo_relative_path":"src/beta.rs","span":{"start_byte":0,"end_byte":100,"start_line":30,"end_line":40},"extraction_completeness":"complete"}
```

This subcommand is **CLI-only** in this slice: the daemon exposes no
partial-name verb (see
[`docs/schema/daemon-query.md`](../schema/daemon-query.md)), and it does not
accept `--at`/`--as-of`/`--tx-as-of` temporal selectors or `--daemon`
routing.

---

## eg query file

List all `Symbol` nodes defined in a file, resolved through `DEFINES` edges.

```text
eg query file <PATH> --graph <PATH> [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATH>` | yes | Repository-relative file path, e.g. `src/lib.rs`. |
| `--graph <PATH>` | yes | Graph JSONL to query. |
| `--at <COMMIT>` | no | Pin the listing to the file's recorded state at this commit SHA or unique prefix (issue #158). Requires a `scan-history` store. Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Pin the listing to the file's recorded state at the most recent commit at or before this instant (valid-time axis). Mutually exclusive with `--at`. |
| `--tx-as-of <RFC3339>` | no | Reserved for `query file`: always returns a `not_implemented` error envelope and exit `1`, never a silently coerced result. Transaction time currently covers `query symbol` only (issue #66). |
| `--repo <SELECTOR>` | no | Restrict results to one repository. A matching path in another repository is excluded and reported only through the `excluded_other_repositories` stderr diagnostic. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

Same fields as `eg query symbol` (see above), except the declaration-surface fields `visibility`, `signature`, and `doc`, which are omitted from file listing rows to keep the per-file answer lean — use `eg query symbol <NAME>` for a symbol's contract. Results are sorted by `span.start_line` ascending, then `record_id`.

### File snapshot at a point in time (`--at` / `--as-of`, issue #158)

`--at <COMMIT>` / `--as-of <RFC3339>` reconstruct the deterministic set of
symbols the file defined **at that point**, from a `scan-history` graph or an
embedded store — no network, no re-parsing of `git show` output, no working-tree
reads. `scan-history` records a full snapshot at every commit, so a symbol
tombstoned at or before the selected point simply has no snapshot there and can
never leak into the result; spans and names are the recorded state as-of the
point, not the current tree. The answer is code-facts only (`File`/`Symbol`/
`Commit` history records) — agent observations, project/task, artifact, and
verification records are never mixed in.

```sh
# Replay history first (reads Git objects only, never mutates the checkout)
eg scan-history . --out history.graph.jsonl

# What symbols did src/parser.rs define at commit 4f0c2b1?
eg query file src/parser.rs --graph history.graph.jsonl --at 4f0c2b1

# ... and as of an instant (most recent commit at or before it)?
eg query file src/parser.rs --graph history.graph.jsonl --as-of 2026-01-02T00:00:00Z
```

Unlike the current-state listing, the point-in-time answer is a **single JSON
envelope** (not JSONL rows), byte-identical across repeated runs:

```json
{
  "ok": true,
  "path": "src/parser.rs",
  "at": "4f0c2b1",
  "as_of": null,
  "resolved_commit": "<full SHA the answer was computed against>",
  "resolved_valid_time": "2026-01-01T00:00:00Z",
  "file_record_id": "codegraph:v5:...",
  "file_schema_version": 5,
  "symbols": [
    {
      "record_id": "codegraph:v5:...",
      "schema_version": 5,
      "name": "parse",
      "kind": "Symbol",
      "symbol_kind": "function",
      "repo_relative_path": "src/parser.rs",
      "span": { "start_byte": 0, "end_byte": 30, "start_line": 1, "end_line": 1 },
      "commit": "<resolved full SHA>",
      "valid_time": "2026-01-01T00:00:00Z"
    }
  ],
  "returned": 1,
  "diagnostics": []
}
```

Every row carries a stable record ID plus a repo-relative file/span handle
resolved as-of the point (module-level symbols without a span carry a
documented `absent_span_reason`), and the envelope records the resolved
commit/instant it was computed against. Output is bounded and redaction-safe:
record IDs, commit handles, paths, spans, and counts only — never raw source
text, patch hunks, or commit-message bodies.

Not-found is never conflated with empty: a file that existed at the point but
defined zero symbols returns `ok: true` with an explicit `empty_symbol_set`
diagnostic (exit `0`), while an unknown path or a path that did not exist at
the point fails with a machine-readable error and exit `2`.

In a shared multi-repository store, `--as-of` resolves the instant on the
queried path's **own repository timeline**: an unrelated repository's newer
commit never wins the at-or-before race, so it cannot make the file look
absent at a commit its repository never had. When more than one repository
records the same path, an unscoped `--as-of` query fails closed with
`ambiguous_repository` (rerun with `--repo <SELECTOR>`), matching the
issue #67 contract for single-answer time views.

#### Exit codes for `--at` / `--as-of`

Failures print `{"ok":false,"error":{"error_type":...}}` to stdout — never a
fabricated symbol, never an empty success:

| Condition | `error_type` | Exit |
|-----------|--------------|------|
| Resolved point (including an `empty_symbol_set` result) | — | `0` |
| Path matches nothing at any recorded commit | `unknown_path` | `2` |
| Path known to history but absent at the resolved point | `file_absent_at_point` | `2` |
| Commit prefix matches nothing | `missing_commit` | `2` |
| Store has no commit history (plain `scan` graph) | `empty_history` | `2` |
| `--as-of` instant is not valid RFC 3339 | `invalid_instant` | `2` |
| No commit at or before the `--as-of` instant | `no_commit_at_or_before_instant` | `2` |
| `--at` combined with `--as-of` | — (flag conflict, clap) | `2` |
| Commit prefix matches multiple commits | `ambiguous_commit_prefix` | `1` |
| Unscoped path+commit collision across repositories | `ambiguous_repository` | `1` |
| `--tx-as-of` supplied | `not_implemented` (as `error.code`) | `1` |

#### When to use which temporal tool

- **`eg query file <PATH> --at/--as-of`** — you have a *path* and want its
  past symbol set; you do not yet know the symbol names.
- **`eg query symbol <NAME> --at/--as-of`** — you already know the *name* and
  want that one symbol's state at a point.
- **`eg query lifeline` (issue #96)** — one symbol's full lifecycle across all
  history, not a per-file snapshot.
- **`eg query deltas <BASE> <HEAD>` (issue #118)** — what changed *between* two
  commits, not what existed *at* one.
- **`eg query symbol --tx-as-of` (issue #66)** — what the *store* knew at a
  transaction instant; the file listing has no transaction-time view yet.
- **`git show <COMMIT>:<PATH>`** — raw file bytes at a commit; you must
  re-parse them and re-filter same-name/comment/string noise yourself, with no
  stable record IDs to join against the graph.

---

## eg query who

Find who last changed a symbol (issue #116).

```text
eg query who <NAME> --graph <PATH> [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--format json|text]
```

Answers "who last changed `<NAME>`" by returning the Git author of the most
recent commit at or before the queried point in history that actually changed
the symbol (following file renames), together with the commit SHA and the
symbol's repo-relative file handle. Requires a history graph produced by
`eg scan-history`: a plain `eg scan` graph carries no `Commit` records and
yields a no-match exit `2`.

Authorship is a **deterministic VCS-derived fact**, not an agent-authored
observation: `eg scan-history` records the normalized Git author identity
(`author_name` + `author_email`, from the commit's `%an` / `%ae` author
metadata) on every `Commit` record, on its own fields distinct from the
`author_time` timestamp in the temporal metadata. It states who empirically
made a change; it is **not an ownership claim** — it asserts nothing about
declared ownership, responsibility, or review authority (CODEOWNERS-style
declarations answer a different question). Repeated `eg scan-history` runs of
an unchanged repository reproduce the author fields byte-for-byte.

`author_email` is **redaction-eligible** PII: a local store retains the raw
address at rest, while evidence-bundle export always scrubs it to a
`<REDACTED:email:hash_prefix>` marker
(see [`docs/schema/redaction.md`](../schema/redaction.md) and
[`bundle.md`](bundle.md)).

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<NAME>` | yes | Exact symbol name to look up. |
| `--graph <PATH>` | one of | History graph JSONL produced by `eg scan-history`. Mutually exclusive with `--data-dir`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated from a history graph. Mutually exclusive with `--graph`. |
| `--at <COMMIT>` | no | Report authorship as of this commit SHA or unique prefix (valid-time axis). Exit `1` on an ambiguous prefix. Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Report authorship at the most recent commit at or before this instant (valid-time axis). Mutually exclusive with `--at`. |
| `--tx-as-of <RFC3339>` | no | Reserved for `eg query who`; exits `1` with an error rather than silently ignoring the flag. |
| `--repo <SELECTOR>` | no | Restrict results to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--repo-path <DIR>` | no | Working-tree path for the non-fatal `freshness` code (see [Store freshness](#store-freshness---repo-path-issue-82)). |
| `--format` | no | `json` (default) or `text`. |

Without a temporal selector the answer is computed at HEAD. With `--at` /
`--as-of` authorship is reported **as-of the queried point in history**, scoped
to the HEAD lineage — the same valid-time semantics as `eg query symbol`
([`docs/schema/temporal-selectors.md`](../schema/temporal-selectors.md)).

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `symbol_name` | string | yes | The queried symbol name. |
| `commit_sha` | string | yes | Full SHA of the most recent commit at or before the queried point that changed the symbol — the citable commit handle. |
| `author_name` | string | when recorded | Git author display name from that commit. |
| `author_email` | string | when recorded | Git author email address from that commit. Redaction-eligible PII (see above). |
| `valid_time` | string | yes | Committer timestamp of that commit (valid-time axis). |
| `repo_relative_path` | string or null | yes | Repository-relative file path of the symbol — the citable file handle. |
| `freshness` | string | with `--repo-path` | Non-fatal store-freshness code (`fresh` / `stale_head` / `stale_dirty` / `unknown`). |

### Example

```sh
eg scan-history . --out history.graph.jsonl
eg query who scan_repository --graph history.graph.jsonl
eg query who scan_repository --graph history.graph.jsonl --as-of 2026-01-02T00:00:00Z
```

```json
{"symbol_name":"scan_repository","commit_sha":"83fa99...","author_name":"Jane Dev","author_email":"jane@example.com","valid_time":"2026-01-01T12:00:00Z","repo_relative_path":"src/lib.rs"}
```

---

## eg query drift

Find `SemanticDrift` nodes ranked by cosine-distance score descending.

```text
eg query drift --graph <PATH> [--limit N] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--graph <PATH>` | yes | Graph JSONL produced by `eg scan-history` after semantic drift computation. |
| `--limit N` | no | Maximum results (default `10`). Bounds the scoped result set when `--repo` is supplied. |
| `--repo <SELECTOR>` | no | Restrict results to drift records whose target belongs to one repository. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable record ID for the `SemanticDrift` node. |
| `schema_version` | number | yes | Record schema version for the returned `SemanticDrift`; see [`docs/schema/schema-versioning.md`](../schema/schema-versioning.md). |
| `before_commit` | string | yes | Commit SHA for the earlier embedding. |
| `after_commit` | string | yes | Commit SHA for the later embedding. |
| `before_valid_time` | string | yes | Valid time for the earlier embedding. |
| `after_valid_time` | string | yes | Valid time for the later embedding. |
| `prior_record_id` | string | yes | Prior codegraph File/Symbol record ID. |
| `target_record_id` | string | yes | Later codegraph File/Symbol record ID. |
| `metric_kind` | string | yes | Drift metric, e.g. `cosine_distance`. |
| `score` | number | yes | Cosine distance as a JSON number. Higher = more drift. |
| `selection_threshold` | number | yes | Threshold that selected this drift record. |
| `selection_basis` | string | yes | Selection policy, e.g. `threshold_only`. |
| `embedding_model_provider` | string | yes | Provider or boundary that supplied the model. |
| `embedding_model_name` | string | yes | Embedding model name. |
| `embedding_model_version` | string | yes | Pinned model version. |
| `embedding_model_dim` | number | yes | Embedding dimension. |
| `embedding_model_content_hash` | string | yes | Model content hash or `unknown`. |
| `repo_relative_path` | string or null | when resolvable | Path of the drift target, resolved from `DRIFTS_FROM` edges. |
| `name` | string or null | when resolvable | Name of the drift target. |
| `repository_id` | string | when attributable | Stable `Repository` record ID of the drift target's repository. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

Ties in `score` are broken by `record_id` ascending. Drift records from
different repositories are never merged: each row carries its own repository
identity.

---

## eg query semantic

Find code nodes by natural-language similarity using dense vector embeddings.

```text
eg query semantic <QUERY> --data-dir <DIR> [--limit N] [--format json|text]
```

The query string is embedded with the same model used during ingest and compared against stored vectors using cosine similarity. Results are returned in descending similarity order.

The embedded store **must** have been populated with `eg ingest --embed`. A store created without `--embed` contains no embedding vectors and returns no results.

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<QUERY>` | yes | Natural-language search text, symbol name, or code snippet. |
| `--data-dir <DIR>` | yes | Embedded `AletheiaDB` store created by `eg ingest --adapter embedded --embed`. `--graph` is not accepted by this subcommand. |
| `--limit N` | no | Maximum number of results (default `10`). Bounds the scoped result set when `--repo` is supplied. |
| `--repo <SELECTOR>` | no | Restrict retrieval leads to one repository. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

One JSON object per line (JSONL). The default output format is `json`. Field names are stable across releases.

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable BLAKE3-based record ID (`codegraph:v1:…`). Safe to cite, log, and pass to other `eg` commands. |
| `score` | number | yes | Cosine similarity score (0.0–1.0). Higher = more similar to the query. |
| `name` | string | when available | Symbol or file name from the matched record. Absent when the record has no name field. |
| `repo_relative_path` | string | when available | Repository-relative file path, e.g. `"src/lib.rs"`. Absent when the record has no path field. |
| `span` | object | when available | Source span: `start_byte`, `end_byte`, `start_line`, `end_line` (all integers). Absent when the record has no span. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the row. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

Machine consumers must depend only on the fields listed above. Additional fields may be added in future releases; removing or renaming any of the fields above constitutes a breaking contract change and requires a version bump.

### No-result and missing-embedding behavior

| Condition | Exit code | Stderr message | Operator action |
|-----------|-----------|----------------|-----------------|
| Store directory does not exist | `1` | `embedded store not found … run ingest` | Create the store: `eg ingest --adapter embedded --data-dir <DIR> [--embed]`. |
| `semantic_search` returns no matches | `2` | `no results — store may not have embeddings (re-run ingest with --embed)` | Re-run ingest: `eg ingest --adapter embedded --data-dir <DIR> --embed`. |

Exit code `2` is also returned when the query produced no cosine-similar results above the search threshold. This is semantically equivalent to "no match" in other query subcommands.

### `--format text`

`--format text` emits one human-readable line per result for terminal use, for example:

```text
scan_repository score=0.9500 @ src/lib.rs:51
```

The exact format of `--format text` output is **not stable** and must not be parsed by scripts or agents. Use `--format json` for machine-readable output with stable field names.

### Example

```sh
eg ingest graph.jsonl --adapter embedded --data-dir .egregore-semantic --embed
eg query semantic "write nodes to database storage" --data-dir .egregore-semantic
```

```json
{"record_id":"codegraph:v1:abc123","name":"EmbeddedAletheiaSink::write_record","repo_relative_path":"src/sink/embedded.rs","score":0.9231,"span":{"start_byte":4096,"end_byte":5200,"start_line":142,"end_line":168}}
```

The `record_id` is stable across re-scans of the same commit and can be cited in agent-memory records. The `repo_relative_path` and `span` together give a file and line-range handle that agents can pass directly to editor tools or other `eg` commands.

---

## eg query at

Resolve a `file:line` location — a compiler diagnostic (`src/daemon.rs:412`),
a panic backtrace frame, a PR diff hunk, or `git blame -L` output — to the
**smallest enclosing `Symbol` node** whose recorded span contains that line
(issue #151). Purely a span-containment lookup over already-stored
`SourceSpan` data: no daemon, no embeddings, no `--embed` store required.

```text
eg query at <PATH>:<LINE> --graph <PATH>   [--at <COMMIT>] [--repo <SELECTOR>]
eg query at <PATH>:<LINE> --data-dir <DIR> [--at <COMMIT>] [--repo <SELECTOR>]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATH>:<LINE>` | yes | Repo-relative path and 1-based line, e.g. `src/lib.rs:42`. The line is taken after the **last** `:`. |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store. Structural records only — the store does **not** need `--embed`. |
| `--at <COMMIT>` | no | Temporal pin: resolve the location against symbol spans **as they existed at this commit** (full SHA or unique prefix). Requires a history-bearing store. |
| `--repo <SELECTOR>` | no | Restrict resolution to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--format` | no | Accepted for CLI-surface consistency; the envelope is JSON. |

### Resolution semantics

- **Innermost wins.** When nested symbols contain the line (a method inside an
  `impl` inside a module), the primary `symbol` is the one with the narrowest
  containing span (line width, then byte width, then record ID — all
  ascending). The full enclosing chain of containing `Module` and `Symbol`
  nodes is also reported, ordered **outermost → innermost**; the last chain
  entry is always the primary symbol.
- **Absence is the answer.** A line outside every symbol span (blank line,
  file-level `use`/attribute, inter-item whitespace — even inside a module)
  yields the typed `no_enclosing_symbol` error, never a nearest-neighbor
  guess.
- **Current state by default.** Without `--at`, tombstoned records are
  excluded, and a history graph is anchored to its stamped HEAD snapshot
  (issue #82): a path deleted or renamed at HEAD is a `no_match`, never a
  stale pre-deletion symbol. Legacy stores without a stamped snapshot fall
  back to each stable ID's newest recorded version (by valid time,
  independent of record emission order). With `--at`, only records observed
  at that commit participate, so the same line can resolve to different
  symbols (or to none) at different commits.
- **Repository boundary stays explicit.** An unscoped location whose path
  exists in more than one repository fails closed with the
  `ambiguous_repository` stderr diagnostic (exit `1`); re-run with `--repo`.

### JSON output fields

A single deterministic JSON envelope on stdout, byte-identical across runs
for identical inputs and store state:

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `ok` | boolean | yes | `true` on success. |
| `path` | string | yes | Queried repo-relative path, echoed back. |
| `line` | number | yes | Queried 1-based line, echoed back. |
| `symbol` | object | yes | The smallest enclosing `Symbol` node. |
| `enclosing_chain` | array | yes | Containing `Module`/`Symbol` nodes, outermost → innermost. The innermost entry is the primary `symbol`. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the answer. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

`symbol` and each `enclosing_chain` entry carry: `record_id`,
`kind` (`"Symbol"` or `"Module"` for chain entries), `schema_version`, `name`,
`symbol_kind` (e.g. `function` / `method` / `struct` / `impl`; absent on
modules), `repo_relative_path`, `span` (`start_byte`, `end_byte`,
`start_line`, `end_line`), and — when recorded — `language`, `visibility`,
`signature`, `git_commit` (history-backed records), and `valid_time`.

### Exit codes and error envelopes

Errors are one-line `{"ok":false,"error":{...}}` envelopes on stdout:

| Code | Error `code` | Meaning |
|------|--------------|---------|
| `0` | — | An enclosing symbol was found and printed. |
| `1` | `malformed_location` | Input is not `<path>:<line>` with a positive 1-based line. |
| `1` | `ambiguous_commit_prefix` | `--at` prefix matches more than one commit; `candidates` lists them. |
| `1` | `ambiguous_repository` / `unknown_repository_selector` / `ambiguous_repository_selector` | Repository selection failed (stderr diagnostic, as elsewhere). |
| `2` | `no_match` | No record carries the path in the selected view (unknown file, or file absent at `--at <COMMIT>`). |
| `2` | `missing_commit` | `--at` commit is absent from the store's history. |
| `2` | `no_enclosing_symbol` | The path is known but no symbol span contains the line. Carries `file_record_id` when the `File` node resolved. |

### Example

```sh
eg scan . --out g.jsonl
eg query at src/lib.rs:412 --graph g.jsonl
```

```json
{
  "ok": true,
  "path": "src/lib.rs",
  "line": 412,
  "symbol": {
    "record_id": "codegraph:v4:abc...",
    "kind": "Symbol",
    "schema_version": 4,
    "name": "outer::Gadget::method_one",
    "symbol_kind": "method",
    "repo_relative_path": "src/lib.rs",
    "span": {"start_byte": 4096, "end_byte": 5200, "start_line": 409, "end_line": 431},
    "language": "rust",
    "visibility": "public",
    "signature": "pub fn method_one(&self) -> usize"
  },
  "enclosing_chain": [
    {"record_id": "codegraph:v4:m01...", "kind": "Module", "schema_version": 4, "name": "outer", "repo_relative_path": "src/lib.rs", "span": {"start_byte": 100, "end_byte": 6000, "start_line": 3, "end_line": 480}, "language": "rust"},
    {"record_id": "codegraph:v4:i01...", "kind": "Symbol", "schema_version": 4, "name": "outer::impl Gadget", "symbol_kind": "impl", "repo_relative_path": "src/lib.rs", "span": {"start_byte": 3900, "end_byte": 5300, "start_line": 405, "end_line": 433}, "language": "rust"},
    {"record_id": "codegraph:v4:abc...", "kind": "Symbol", "schema_version": 4, "name": "outer::Gadget::method_one", "symbol_kind": "method", "repo_relative_path": "src/lib.rs", "span": {"start_byte": 4096, "end_byte": 5200, "start_line": 409, "end_line": 431}, "language": "rust"}
  ]
}
```

The returned `record_id` is a stable handle: feed it directly to
`eg query change-impact`, `eg query failures`, or `eg query context` to pivot
from a raw location into callers, prior failures, and evidence without ever
scanning the file's full symbol list.
