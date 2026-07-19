# eg query churn

Rank Git-tracked files by change frequency across commit history (issue #128).

Over a temporal store produced by `eg scan-history`, returns files ranked by
descending count of **distinct commits that modified them** — the
software-archaeology hotspot signal (change frequency is the frequency half of
CodeScene-style hotspots; complexity weighting is out of scope for this
slice). Every row is a stable, citable graph handle an agent can pivot on with
`eg query file`, `eg query change-impact`, `eg query failures`, or
`eg query subsystem` — something raw `git log` shell-mining cannot offer.

## Synopsis

```text
eg query churn --graph <PATH>    [--repo <SELECTOR>] [--limit N] [--format json|text]
eg query churn --data-dir <DIR>  [--repo <SELECTOR>] [--limit N] [--format json|text]
```

```sh
eg scan-history . --out history.graph.jsonl
eg query churn --graph history.graph.jsonl
eg query churn --graph history.graph.jsonl --limit 10 --format text
```

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded` from a `scan-history` graph. Providing both `--graph` and `--data-dir` is an error. |
| `--repo <SELECTOR>` | no | Restrict the ranking to one repository (see [Repository scope](query.md#repository-scope---repo-issue-67)). Unknown or ambiguous selectors exit `1` with the standard machine-readable stderr diagnostic. |
| `--limit N` | no | Maximum ranked files returned. **Default `50`, maximum `500`.** Values outside `1..=500` are rejected with an `invalid_limit` diagnostic on stderr and exit `1`. |
| `--format` | no | `json` (default) or `text`. |

## What counts as churn

- A file's churn is the number of **distinct commits** in scope carrying a
  `CHANGED_IN` edge from the file's `File` node to a `Commit` node — exactly
  the commits whose Git diff modified (added, changed) that file, as recorded
  by history replay.
- Only paths that resolve to a live (non-tombstoned) `File` node can rank.
  History replay records only committed, Git-tracked, indexed source files, so
  **untracked or `.gitignore`d paths never appear** (consistent with #67 and
  #99), and non-source files (e.g. `README.md`) do not rank.
- A source file deleted mid-history still ranks on the commits that touched it
  while it existed: its temporal `File` records remain citable.
- Retraction tombstones on the change edge itself are honored: a retracted
  (tombstoned, non-temporal) `CHANGED_IN` edge never counts, and a file whose
  only change marker was retracted does not rank. An edge carrying commit
  provenance (temporal metadata) is a historical fact exempt from
  current-state tombstone suppression — the same convention as
  `eg query changes` and the `eg forget` retraction contract.
- This reads committed history only — live working-tree edits and uncommitted
  churn are invisible to the ranking.

## Ordering (deterministic)

Rows are sorted by:

1. `commit_count` descending;
2. `repo_relative_path` ascending — the documented stable tie-break;
3. `file_record_id` ascending (only reachable for cross-repository path
   collisions in an unscoped multi-repository store).

The full ranking is byte-identical across repeated runs on unchanged history.

## Output

`--format json` (default) prints **one JSON envelope on one line**, keeping
the one-JSON-object-per-line contract of [`query.md`](query.md):

```json
{"ok":true,"result":{
  "ranking_basis":"distinct_commit_count",
  "tie_break":"repo_relative_path",
  "limit":50,
  "total_file_count":3,
  "returned_file_count":3,
  "truncated":false,
  "commit_ranges":[{"repository_id":"codegraph:v4:…","repository":"acme/widget","first_commit":"83fa99…","last_commit":"be4a01…","commit_count":4}],
  "files":[
    {"rank":1,"repo_relative_path":"src/hot.rs","file_record_id":"codegraph:v1:…","schema_version":1,"commit_count":4,"first_commit":"83fa99…","last_commit":"be4a01…","repository_id":"codegraph:v4:…","repository":"acme/widget"}
  ]
}}
```

### Result fields

| Field | Type | Description |
|-------|------|-------------|
| `ranking_basis` | string | Always `"distinct_commit_count"`: count of distinct commits in scope that modified the file. |
| `tie_break` | string | Always `"repo_relative_path"`: the documented stable tie-break key. |
| `limit` | number | The limit applied to the ranking. |
| `total_file_count` | number | Ranked files before truncation. |
| `returned_file_count` | number | Files returned after truncation. |
| `truncated` | boolean | Completeness signal: whether `--limit` cut the ranking. Never silent. |
| `commit_ranges` | array | Inclusive commit range(s) the ranking was measured over — one entry per repository scope in the store, sorted by repository record ID, each with `first_commit`, `last_commit` (deterministic topological order, SHA tie-break), and `commit_count`. |
| `files[]` | array | Ranked rows, highest churn first. |

### Row fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `rank` | number | yes | 1-based rank after the documented ordering. |
| `repo_relative_path` | string | yes | Repository-relative file handle. |
| `file_record_id` | string | yes | Stable record ID of the `File` node the handle resolves to. |
| `schema_version` | number | yes | Record schema version of the cited `File` node. |
| `commit_count` | number | yes | Distinct commits in scope that modified the file. |
| `first_commit` | string | yes | First commit (inclusive) of the range the frequency was measured over. |
| `last_commit` | string | yes | Last commit (inclusive) of the range the frequency was measured over. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the row. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

In an unscoped multi-repository store, rows from different repositories are
returned side by side — never merged — and each carries its own repository
identity and range (consistent with the unscoped list-query contract in
[`query.md`](query.md)).

`--format text` prints a human-readable ranking (`1. src/hot.rs commits=4
(codegraph:v1:…)`) plus the range header and, when applicable, an explicit
`truncated: showing K of M files` line. The text format is not stable and
must not be parsed by scripts.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Ranking returned. |
| `1` | Load error, invalid `--limit` (`invalid_limit` on stderr), or unknown/ambiguous `--repo` selector. |
| `2` | Nothing to rank: `{"ok":false,"error":{"code":"no_history",…}}` when the store holds no `Commit` nodes in scope (not a `scan-history` store), or `{"code":"no_match",…}` when commits exist but no file carries a commit-backed change edge. |

## Compared to the boring alternative

`git log --name-only --pretty=format: | sort | uniq -c | sort -rn` yields raw
file churn cheaply, but produces plain text with no stable graph handles, no
repository-identity scoping, no tombstone awareness, and no path from a
hotspot to its symbols, agent memory, or verification evidence. `eg query
churn` returns deterministic `File` record IDs that join to every other
Egregore domain in the same store.

## Out of scope (per issue #128)

- Symbol-level churn (#118 consumes symbol-change detection).
- Author-diversity / ownership churn (#116).
- Complexity weighting (frequency only in this slice).
- Semantic/embedding drift ranking (`eg query drift`, #55).
- Uncommitted working-tree churn.
## Corpus scope

This is a history-analysis lane: it reads the **union of all commit snapshots**
by design and carries no `--at-head`/`--all-history` corpus flags. The summary
envelope discloses `corpus_mode: "union"` (or `single_snapshot` over a
snapshot-less store), `corpus_mode_source`, and `corpus_disclaimer` for
transparency. See [Corpus scope for query lanes](corpus-modes.md).
