# eg query public-api-deltas

Classify **changes to the externally-reachable public API surface across a
commit range** (issue #157) — answer the release-review question *"did this
range change the crate's public contract in a way worth flagging?"* — from a
`scan-history` graph or an embedded store. Local-first; no network access,
hosted indexing, build step, or mandatory remote embeddings.

> **Rows are observed structural surface changes, not proof of semver
> breakage, downstream build failure, or behavior change.** The
> `potentially_breaking` flag marks a change class worth review — removal,
> signature change, visibility narrowing — it never asserts a required
> version bump or an actual downstream break. Proving breakage needs a real
> build (`cargo-semver-checks`); this query is the honest, build-free,
> any-commit-range signal that tells you where to look first.

## Prerequisites it composes

- **Issue #124** — per-symbol `visibility` and `signature` captured at
  extraction time supply the before/after surface text.
- **Issue #118 (`eg query deltas`)** — the commit-range mechanics: endpoint
  resolution, introducing commits, and the error taxonomy are identical.
- **Issue #213 (`eg query public-api`)** — the reachability rule: an item is
  exported only when its own visibility is `public` **and** every module on
  its containment chain is recorded `public` at that endpoint.

## Synopsis

```text
eg query public-api-deltas <BASE> <HEAD> --graph <PATH>    [--repo <SELECTOR>] [--include-internal] [--callers] [--format json|text]
eg query public-api-deltas <BASE> <HEAD> --data-dir <DIR>  [--repo <SELECTOR>] [--include-internal] [--callers] [--format json|text]
```

`<BASE>` and `<HEAD>` are commit handles — full SHAs or unique prefixes —
resolved against the store's `Commit` nodes. `BASE` must be an ancestor of
`HEAD`. The query is purely read-time: it reads only the supplied store,
never Git state, so it cannot mutate the working tree. (The store itself is
produced by `eg scan-history`, which reads Git objects only and leaves the
checkout byte-for-byte unchanged.)

## Shortest offline workflow

```sh
# Replay history into a temporal JSONL graph (reads Git objects only)
eg scan-history . --out history.graph.jsonl

# Classify public-API surface changes between two commits
eg query public-api-deltas 4f0c2b1 9ad3e77 --graph history.graph.jsonl

# Also list crate-internal deltas, and join caller leads onto removals
eg query public-api-deltas 4f0c2b1 9ad3e77 --graph history.graph.jsonl --include-internal --callers
```

## Change classes

The closed, stable change-class set for exported symbols:

| Group | Class label | `potentially_breaking` | Meaning |
|-------|-------------|------------------------|---------|
| `added` | `added` | no | Exported at head; no snapshot existed at base |
| `removed` | `removed` | **yes** | Exported at base; no snapshot exists at head |
| `signature_changed` | `signature_changed` | **yes** | Exported at both endpoints; the recorded signature header (params / return type / generics / where-clause) changed |
| `visibility_narrowed` | `visibility_narrowed` | **yes** | Exported at base, still present at head but off the surface (e.g. `pub` → `pub(crate)`, or a containing module went non-`pub` — flagged `via_module_chain`) |
| `visibility_widened` | `visibility_widened` | no | Present at base off the surface, exported at head |
| `internal` (opt-in) | `internal_added` / `internal_removed` / `internal_modified` | no | Deltas to symbols not on the external surface at either endpoint |

Notes on semantics:

- **A private-symbol delta never appears as a public-API change.** Private,
  `pub(crate)`, `pub(super)`, and `pub(in path)` items, `pub` items trapped
  in non-`pub` modules, methods/tests/`impl` blocks, and non-library-crate
  paths (`src/bin/**`, `tests/`, …) classify into the internal lane. They
  are always tallied in `counts.internal_changes` and listed only with
  `--include-internal`, inside a group labeled
  `internal_not_public_surface`.
- **Renames surface as a pair** (`removed` + `added`): symbol identity is
  path- and name-based, exactly as in `eg query deltas`.
- **Removals carry a tombstone handle**: the row's `record_id`, path, and
  span cite the base-endpoint snapshot and the row says so with
  `"handle_side": "base_tombstone"`; every other class cites the head
  snapshot (`"handle_side": "head"`).
- **Body-only changes to exported items are not surface changes**: when the
  recorded visibility and signature are identical at both endpoints, the row
  is suppressed and tallied in `counts.exported_body_only_modified`.
- Every row carries a stable `record_id`, `schema_version`, `change_class`,
  `potentially_breaking`, `name`, `repo_relative_path`, `span` when
  available (with a documented `absent_span_reason` otherwise), the
  before/after visibility class and signature header where each endpoint has
  a snapshot, and the `commit` + `valid_time` of the range commit that
  introduced the head-visible state.
- `--callers` attaches `internal_callers` — base-endpoint `CALLS`-edge
  sources with record IDs and paths (cf. issue #139 / `eg query
  change-impact`) — to `removed` and `signature_changed` rows so the agent
  sees who relied on the item. The join is never required for
  classification.
- A surface-eligible symbol with **no recorded visibility** (pre-#124 scan)
  is excluded from every group and reported through a
  `symbol_visibility_missing` diagnostic — never guessed. A module with no
  recorded visibility at an endpoint yields `module_visibility_unknown` and
  its items are treated as off-surface.
- `pub use` re-export sites are not classified by this query; enumerate them
  point-in-time with `eg query public-api`.
- Output is bounded and redaction-safe: record IDs, paths, spans, names,
  commit handles, valid times, visibility classes, and normalized signature
  headers only — never raw blob contents, snapshot bodies, patch hunks,
  secrets, or tokens.
- Repeating the same query yields byte-identical canonical output: every
  surface group is always present (empty arrays, never omitted) and
  canonically ordered by `(repo_relative_path, name, record_id)`.

## Exit codes and diagnostics

Identical to `eg query deltas`: ambiguous prefixes, unknown commits,
identical endpoints, and reversed ranges fail with stable machine-readable
diagnostics (`{"ok":false,"error":{"error_type":...}}` on stdout), never
partial or silent output:

| Condition | `error_type` | Exit |
|-----------|--------------|------|
| Success (including a range with no surface changes) | — | `0` |
| Commit prefix matches multiple commits | `ambiguous_commit_prefix` | `1` |
| Both endpoints resolve to the same commit | `identical_endpoints` | `1` |
| Base is a descendant of head | `reversed_range` | `1` |
| No ancestor path connects the endpoints | `no_path` | `1` |
| Unscoped multi-repository store, endpoints ambiguous across repositories | `repo_scope_required` | `1` |
| Commit prefix matches nothing | `missing_commit` | `2` |
| Store has no commit history | `empty_history` | `2` |

### `repo_scope_required` (issue #341)

Shared verbatim with `eg query deltas`: both range lanes resolve endpoints
through the same gate. In an unscoped multi-repository store, when a commit SHA
is present in more than one repository (a fork, a mirrored history, the same
upstream ingested twice) or the two endpoints resolve into different
repositories, the endpoint resolution is ambiguous across repositories. Rather
than silently collapse the shared SHA to one match or splice two repositories'
topologies, the query refuses with a stable diagnostic (exit `1`, the same
malformed/ambiguous-input class as `ambiguous_commit_prefix`), naming every
candidate repository and telling the caller to pass `--repo <SELECTOR>`:

```json
{"ok":false,"error":{"error_type":"repo_scope_required","candidate_repositories":[{"repository_id":"codegraph:v5:...","repository_name":"widget-a"},{"repository_id":"codegraph:v5:...","repository_name":"widget-b"}]}}
```

Ambiguity is never resolved by picking a repository implicitly. When both
endpoints unambiguously belong to a single common repository the query succeeds,
gated to that repository so a foreign repository's same-SHA commits cannot leak
into the surface diff. Single-repository stores and `--repo`-scoped calls are
unaffected. This mirrors the repository-gating precedent established for the
co-change coupling lane in PR #312 (issue #153).

## Response shape

```json
{
  "ok": true,
  "base": "<full base SHA>",
  "head": "<full head SHA>",
  "range_commit_count": 1,
  "disclaimer": "Rows are observed structural changes to the parse-derived public API surface ...",
  "added": [],
  "removed": [
    {
      "record_id": "codegraph:v5:...",
      "schema_version": 5,
      "change_class": "removed",
      "potentially_breaking": true,
      "name": "doomed",
      "symbol_kind": "function",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 27, "end_byte": 53, "start_line": 2, "end_line": 2 },
      "handle_side": "base_tombstone",
      "commit": "<introducing commit SHA>",
      "valid_time": "2026-01-02T00:00:00Z",
      "before_visibility": "public",
      "before_signature": "fn doomed() -> u32"
    }
  ],
  "signature_changed": [],
  "visibility_narrowed": [],
  "visibility_widened": [],
  "counts": {
    "added": 0,
    "removed": 1,
    "signature_changed": 0,
    "visibility_narrowed": 0,
    "visibility_widened": 0,
    "exported_body_only_modified": 0,
    "internal_changes": 1
  },
  "diagnostics": []
}
```

`--format text` renders the same report as deterministic human-readable
text; `--format json` (the default) is the agent contract.

## When to use which tool

- **`eg query public-api-deltas`** — the range question about the *exported*
  contract: which public items were added, removed, signature-changed, or
  visibility-shifted between two commits, with citable handles and honest
  flags. Build-free, works on any recorded range, joins to callers and agent
  memory.
- **`cargo-semver-checks`** — the authoritative semver lint, but it needs a
  successful build/rustdoc of both versions and is release-oriented. Use it
  to *prove* what this query *flags*.
- **`cargo public-api`** — diffs the textual public API between two builds;
  same build requirement, no spans, no commit-range axis beyond what you
  check out and rebuild.
- **`eg query public-api` (issue #213)** — the point-in-time enumeration of
  the current surface; this query is its classification layer across a
  range.
- **`eg query deltas` (issue #118)** — the generic, visibility-agnostic
  symbol/file range delta: it treats a `pub fn` removal identically to a
  private helper rename. Use it for "what changed at all"; use this query
  for "what changed on the public contract".
- **`cargo doc` / rustdoc** — renders the current surface only; no diff, no
  history.
- **`git diff` / `git log -S`** — textual deltas with no visibility
  awareness and no symbol-level classification.

And once more, because it is the sharp edge: **a flagged row is an observed
surface change, not proof of breakage — and an unflagged range is not proof
of compatibility** (trait coherence, type inference, and macro-expanded
surface are out of scope; see `cargo-semver-checks` for build-verified
claims).
