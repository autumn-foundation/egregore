# eg query deltas

Summarize **symbol- and file-level changes across a commit range** (issue
#118) — answer the review question *"between commit A and commit B, what
symbols and files were added, removed, or modified, with citable handles?"* —
from a `scan-history` graph or an embedded store. Local-first; no network
access, hosted indexing, remote crawling, or mandatory remote embeddings.

> **Rows are observed structural/semantic deltas, not proof of behavior
> change, breakage, test failure, or verification.** Absence of a delta is
> not proof a behavior was preserved: unchanged code can still change behavior
> through its dependencies, inputs, or environment.

## Synopsis

```text
eg query deltas <BASE> <HEAD> --graph <PATH>    [--repo <SELECTOR>]
eg query deltas <BASE> <HEAD> --data-dir <DIR>  [--repo <SELECTOR>]
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

# Symbol- and file-level deltas between two commits
eg query deltas 4f0c2b1 9ad3e77 --graph history.graph.jsonl

# Scope to one repository in a shared store
eg query deltas 4f0c2b1 9ad3e77 --graph history.graph.jsonl --repo acme/widget
```

## Change classes

Deltas compare the recorded `File`/`Symbol` snapshots at the base endpoint
against the head endpoint. The stable change-class labels are:

| Group | Class label | Meaning |
|-------|-------------|---------|
| `added_symbols` | `added_symbol` | Symbol present at head, absent at base |
| `removed_symbols` | `removed_symbol` | Symbol present at base, absent at head |
| `modified_symbols` | `modified_symbol` | Symbol present at both endpoints with a changed recorded body |
| `added_files` | `added_file` | File present at head, absent at base |
| `removed_files` | `removed_file` | File present at base, absent at head |
| `modified_files` | `modified_file` | File present at both endpoints with changed recorded content |
| `unresolved` | — | Stable diagnostics for rows that could not be fully resolved (`unresolved_introducing_commit`, `missing_repo_relative_path`) |

Notes on semantics:

- **Renames surface as a pair**: symbol identity is path- and name-based, so a
  rename appears as a `removed_symbol` row for the old name plus an
  `added_symbol` row for the new name (likewise for file renames).
- **Endpoint semantics**: a fact that appears and disappears strictly inside
  the range (added in one commit, removed in a later one) is not an endpoint
  delta and is not reported.
- Every row carries a stable `record_id`, `schema_version`, `change_class`,
  `repo_relative_path`, `span` when available (with a documented
  `absent_span_reason` for module-level symbols), and the `commit` +
  `valid_time` of the range commit that introduced the head-visible state of
  the delta (the last such commit in deterministic topological order).
- Output is bounded and redaction-safe: record IDs, paths, spans, names,
  commit handles, and valid times only — never raw blob contents, patch
  hunks, snapshot bodies, or protected raw-artifact payloads.
- Repeating the same query yields byte-equivalent canonical output: every
  group is always present (empty arrays, never omitted) and canonically
  ordered by `(repo_relative_path, name, record_id)`.

## Semantic drift

Semantic drift falling inside the range is folded into a `semantic_drift`
section, always labeled `semantic_movement_not_structural_change` — drift is
embedding-measured semantic movement, never a structural claim. When the
store carries no drift records (embeddings absent or drift never computed),
the structural deltas are still returned and the section reports
`"status": "unavailable"` with `"reason": "no_drift_records_in_store"` rather
than an indistinguishable empty list.

## Exit codes and diagnostics

Ambiguous prefixes, unknown commits, identical endpoints, and reversed ranges
fail with stable machine-readable diagnostics (`{"ok":false,"error":
{"error_type":...}}` on stdout), never partial or silent output:

| Condition | `error_type` | Exit |
|-----------|--------------|------|
| Success (including a resolved range with no deltas) | — | `0` |
| Commit prefix matches multiple commits | `ambiguous_commit_prefix` | `1` |
| Both endpoints resolve to the same commit | `identical_endpoints` | `1` |
| Base is a descendant of head | `reversed_range` | `1` |
| No ancestor path connects the endpoints | `no_path` | `1` |
| Commit prefix matches nothing | `missing_commit` | `2` |
| Store has no commit history | `empty_history` | `2` |

## Response shape

```json
{
  "ok": true,
  "base": "<full base SHA>",
  "head": "<full head SHA>",
  "range_commit_count": 2,
  "disclaimer": "Rows are observed structural and semantic deltas ...",
  "added_symbols": [
    {
      "record_id": "codegraph:v5:...",
      "schema_version": 5,
      "change_class": "added_symbol",
      "name": "fresh::fresh_fn",
      "symbol_kind": "function",
      "repo_relative_path": "src/fresh.rs",
      "span": { "start_byte": 0, "end_byte": 30, "start_line": 1, "end_line": 1 },
      "commit": "<introducing commit SHA>",
      "valid_time": "2026-01-03T00:00:00Z"
    }
  ],
  "removed_symbols": [],
  "modified_symbols": [],
  "added_files": [],
  "removed_files": [],
  "modified_files": [],
  "unresolved": [],
  "semantic_drift": {
    "status": "unavailable",
    "reason": "no_drift_records_in_store",
    "label": "semantic_movement_not_structural_change",
    "rows": []
  }
}
```

## When to use which tool

- **`eg query deltas`** — the historical A..B answer: which symbols and files
  structurally changed between two commits, grouped by class, with citable
  record IDs and file/span handles.
- **`eg query change-impact` (issue #76)** — the *forward*, pre-edit
  blast-radius question: "if I edit this handle now, what nearby code should
  I inspect?" A different job with different input (a symbol/file handle, not
  a commit range).
- **`eg query symbol <NAME> --at <COMMIT>`** — a point-in-time lookup of one
  symbol at one commit, not a range summary.
- **`eg query lifeline` (issue #96)** — one symbol's full lifecycle across
  all history (introduced/modified/removed/reintroduced), not the delta set
  of a bounded range.
- **`git diff --stat` / `git log --name-status A..B`** — fast and local, but
  they report files and line churn, not which functions, structs, traits, or
  impls changed, and they emit no stable graph handles that join to agent
  memory or verification evidence.
- **`eg query semantic` / semantic search** — meaning-based retrieval over
  the current store, not a structural comparison of two commits.

And once more, because it is the sharp edge: **absence of a delta is not
proof a behavior was preserved** — this query reports observed structural and
semantic movement, nothing else.
