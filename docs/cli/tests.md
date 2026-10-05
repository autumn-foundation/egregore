# eg query tests

Map a symbol to the tests that exercise it before an edit — "which tests can
reach this symbol, and by which call path?" — for pre-edit verification triage
(issue #126).

## Synopsis

```text
eg query tests <HANDLE> --graph <PATH>   [--repo <SELECTOR>] [--max-depth N] [--at <COMMIT> | --as-of <RFC3339>] [--daemon --data-dir <DIR>] [--format json|text]
eg query tests <HANDLE> --data-dir <DIR> [--repo <SELECTOR>] [--max-depth N] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
```

Given a symbol record ID (`codegraph:vN:<hex>`) or exact symbol name, walks
inbound `CALLS` edges and reports every test symbol that can reach the target
— the tests most likely to exercise the code you are about to edit — with the
concrete connecting call path for each.

Rows are reachability **leads, not coverage proof**: a test with a `CALLS`
path to the target may exercise it, but the row never asserts that the test
covers the symbol, that the test will fail after the edit, or that the edit is
unsafe. Absence of a covering test is likewise not proof the symbol is
untested by other means (integration tests, manual QA, dynamic dispatch and
macro-generated calls are outside the extraction contract).

## Test identification

A row is reported only for symbols the extractor stamped with the test role
(`role == "test"`: a test-harness entry, a `#[cfg(test)]`-gated module
member, or an integration-test / bench-root file — see issue #238). The role
is never fabricated: a symbol with no stamped role is never reported as a
test, even if it calls the target directly.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<HANDLE>` | yes | Stable symbol record ID (`codegraph:vN:<hex>`) or exact symbol name. File paths, task handles, and source handles are rejected (exit `1`); so are handles that resolve to a non-symbol node. An ambiguous name is rejected (exit `1`) — walking the union would bleed an unrelated same-name symbol's tests into the result. |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. Read-only: the store is read from a throwaway copy (issue #424). |
| `--repo <SELECTOR>` | no | Restrict symbol resolution — and `--at`/`--as-of` commit resolution — to one repository (see [query.md](query.md#repository-scope---repo-issue-67)). |
| `--max-depth N` | no | Inbound walk bound in hops. Default `5`; must be ≥ 1. |
| `--at <COMMIT>` | no | Walk the graph state at this commit SHA or unique prefix (requires a history store). Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Walk the graph state at the most recent commit at or before this instant. Mutually exclusive with `--at`. |
| `--daemon` | no | Route through the running daemon's `tests_for_symbol` verb instead of reading the local store (requires `--data-dir`; see [daemon-query.md](../schema/daemon-query.md)). |
| `--format` | no | `json` (default) or `text`. |

## Traversal semantics

- **Edges walked:** inbound `CALLS` only. `REFERENCES` edges are deliberately
  excluded — a test that merely mentions a symbol (import, comment, string
  literal) does not exercise it, and reporting it would inflate the covering
  set with false positives. Containment (`DEFINES`/`CONTAINS`) is not
  traversed; rows carry repo-relative file/span citations instead.
- **Non-test callers are traversed but never reported:** a test that calls the
  target through a production helper surfaces as `transitive` with its full
  path (`test → helper → target`), so pre-edit triage sees the indirect
  coverage instead of silently dropping it.
- **`coverage` / `hop`:** hop 1 (the test calls the target itself) is
  `direct`; hop > 1 is `transitive`.
- **Shortest paths, deterministic:** the walk is a level-synchronized BFS.
  Each reachable test is reported **once**, at its shortest hop distance, with
  one concrete shortest connecting path chosen deterministically (minimum
  `(parent record ID, edge record ID)` at the discovering depth). Rows are
  ordered by `(hop, record ID)`. Output is byte-identical across repeated runs
  on an unchanged store.
- **Cycles terminate:** a visited set guarantees mutual recursion neither
  loops nor duplicates a test; the queried symbol is never reported as its own
  covering test.
- **Liveness:** tombstoned (deleted) symbols are excluded; the walk, like the
  transitive lanes, honors the latest-edge-version and liveness gates.
- **Resolution propagation:** every path step exposes the `resolution` label
  its `CALLS` edge carries (`resolved` / `ambiguous` / `unresolved`), and each
  row carries `path_resolution` — the weakest status along its chain. A test
  that reaches the target across an ambiguous edge is only as trustworthy as
  that edge, and the row says so.
- **Corpus scope:** the walk discloses `corpus_mode` / `corpus_mode_source` /
  `corpus_disclaimer` like every lane on this page; the head-anchored default
  excludes callers removed at HEAD (see
  [query.md](query.md#corpus-scope-issue-427) and
  [corpus-modes.md](corpus-modes.md)).

## Output

NDJSON: one summary header line, then one line per covering test.

The header carries the resolved `target` (record ID, name, kind,
repo-relative path, span), `direction: "inbound"`, `edge_labels: ["CALLS"]`,
`max_depth`, the `total_covering_tests` / `direct_tests` / `transitive_tests`
counts, the corpus-mode provenance fields, the reachability-lead `disclaimer`,
an optional `truncation` diagnostic (dropped frontier counts per depth when
the bound is reached — never silent omission), and `diagnostics`.

Each row carries the test's `record_id`, name, kind, repo-relative path,
span, `valid_time` / `git_commit` freshness, `hop`, `coverage`
(`direct`/`transitive`), `path_resolution`, the full `path` (each step with
source record ID, edge record ID, edge label, resolution, target record ID),
and `trust: "reachability_lead"`. `--format text` renders a one-line-per-test
human view; its exact format is unstable by contract.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Completed — including the explicit empty result (header only) when the symbol has no covering test in this view. |
| 1 | Malformed handle, invalid `--max-depth`, ambiguous symbol name, unsupported (non-symbol) handle, or unsupported combination; machine-readable JSON on stderr. |
| 2 | Handle resolves to no live record (`no_match` / `stale_handle`), or `--at`/`--as-of` names no resolvable commit; machine-readable envelope on stdout. |

## Daemon verb

The equivalent daemon verb is `tests_for_symbol`
(`POST /v1/query`, params `handle` (required), `max_depth` (default `5`),
`repo?`, `at?`, `as_of?`), documented in
[daemon-query.md](../schema/daemon-query.md). It returns the same header under
the `tests` key plus the row set as `records`, and reuses the CLI's shared
response computation, so both faces answer identically.
