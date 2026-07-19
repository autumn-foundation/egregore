# eg query ownership

Aggregate **Git authorship into a per-file ownership and bus-factor map**
(issue #245) — answer the software-archaeology risk question *"which files
depend on a single person's knowledge?"* — from a `scan-history` graph or an
embedded store. Local-first; no network access, hosted indexing, remote
crawling, or remote profile lookup.

> **Rows are empirical history-derived leads, not declared ownership,
> authority, or proven expertise.** The primary owner is only the author of
> the largest share of in-scope commits; the response never asserts that they
> are the maintainer, an approver, or the only competent editor. The query
> never parses CODEOWNERS, commit messages, or file names to infer authority.

## Synopsis

```text
eg query ownership [PATH] --graph <PATH>    [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--threshold <PERCENT>] [--limit <N>] [--format json|text]
eg query ownership [PATH] --data-dir <DIR>  [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--threshold <PERCENT>] [--limit <N>] [--format json|text]
```

`[PATH]` optionally filters the map to one repo-relative file. The query is
purely read-time: it reads only the supplied store, never Git state, so it
cannot mutate the working tree. (The store itself is produced by
`eg scan-history`, which reads Git objects only and leaves the checkout
byte-for-byte unchanged.)

## Shortest offline workflow

```sh
# Replay history into a temporal JSONL graph (reads Git objects only)
eg scan-history . --out history.graph.jsonl

# Full ownership / bus-factor map (most concentrated files first)
eg query ownership --graph history.graph.jsonl

# One file, as-of an earlier point in history
eg query ownership src/lib.rs --graph history.graph.jsonl --at 4f0c2b1
eg query ownership src/lib.rs --graph history.graph.jsonl --as-of 2026-01-02T00:00:00Z
```

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `[PATH]` | no | Exact repo-relative file path. An unknown path (including tracked files outside the indexed source set) exits `2` with `unknown_path`, never silent empty output. |
| `--graph <PATH>` | one of | History graph JSONL produced by `eg scan-history`. Mutually exclusive with `--data-dir`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated from a history graph. Mutually exclusive with `--graph`. |
| `--at <COMMIT>` | no | Report ownership as-of this commit SHA or unique prefix (valid-time axis). Exit `1` on an ambiguous prefix, `2` on a missing one. Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Report ownership at the most recent commit at or before this instant (valid-time axis). Exit `1` on a malformed timestamp, `2` when no commit exists at or before it. Mutually exclusive with `--at`. |
| `--repo <SELECTOR>` | no | Restrict aggregation to one repository (issue #67; see `eg query symbol --help`). |
| `--threshold <PERCENT>` | no | Cumulative ownership-share threshold percent for the bus factor. Integer `1..=100`, default `50`. Out-of-range values exit `1` with `invalid_threshold`. |
| `--limit <N>` | no | Maximum file rows. Default `100`, maximum `1000`; `0` or above-max values exit `1` with `invalid_limit`. The answer always states whether it was truncated. |
| `--format` | no | `json` (default, one envelope object per line) or `text`. |

## The metric, frozen

Aggregation runs over the commits reachable from the resolved **anchor**: the
repository head by default, the `--at` commit, or the most recent in-scope
commit at or before `--as-of`. Only files **present at the anchor commit**
with a resolvable `File` node are reported, so every row's handle resolves to
an existing record and untracked or `.gitignore`d paths never appear. Files
outside the indexed source set (for example `README.md`) carry no `File`
node and are outside this query's coverage.

Per file:

- **total_commits** — distinct in-scope commits carrying a recorded `Change`
  for the file's path. A merge commit that records one change per parent diff
  still counts once.
- **author identity** — the normalized identity recorded on `Commit` records
  by issue #116: the exact `author_name` + `author_email` pair from the
  commit's `%an` / `%ae` metadata. No `.mailmap` or cross-email
  reconciliation is applied (deferred exactly as #116 defers it): two emails
  are two identities.
- **ownership share** — `author_commits / total_file_commits` per author.
- **ranking** — authors sort by distinct commit count descending, ties by
  `(author_email, author_name)` ascending.
- **primary owner** — the first ranked author (max share; count ties break to
  the lexicographically smallest `(author_email, author_name)` identity).
- **bus factor** — the minimum number of top-ranked authors whose cumulative
  ownership share reaches the threshold (default 50%), computed in exact
  integer arithmetic (`cumulative_commits * 100 >= total_commits * percent`).
  A file authored 80% by one person reports bus-factor `1`. Lower means more
  concentrated knowledge.

File rows order by `(bus_factor asc, total_commits desc, repo_relative_path
asc, repository_id asc)` — most concentrated first — so `--limit N` keeps the
highest-concentration leads. Ranking, shares, and bus factor are
deterministic and byte-identical across repeated runs on unchanged history.

## Redaction

`author_email` is **redaction-eligible PII** and flows through the existing
policy exactly as issue #116 requires
([`docs/schema/redaction.md`](../schema/redaction.md)): a redaction-off local
store retains raw addresses and this query returns them as recorded, while a
redaction-on export (`eg bundle export`) carries only
`<REDACTED:email:hash_prefix>` markers — the marker is stable per raw value,
so aggregation over an exported store still ranks the same distinct
identities with zero raw email addresses. Output is otherwise bounded to
handles, counts, shares, the bus-factor integer, author identities, commit
handles, and redaction markers — never raw blob contents, patch hunks,
secrets, or tokens.

## Exit codes and diagnostics

Unknown paths and malformed/missing selectors fail with stable
machine-readable diagnostics (`{"ok":false,"error":{"error_type":...}}` on
stdout), never partial or silent output:

| Condition | `error_type` | Exit |
|-----------|--------------|------|
| Success (including an explicit `empty_surface` diagnostic) | — | `0` |
| `--at` prefix matches multiple commits | `ambiguous_commit_prefix` | `1` |
| `--as-of` is not valid RFC 3339 | `malformed_timestamp` | `1` |
| `--threshold` outside `1..=100` | `invalid_threshold` | `1` |
| `--limit` is `0` or above the documented max | `invalid_limit` | `1` |
| `[PATH]` is not an indexed source file at the anchor | `unknown_path` | `2` |
| `--at` prefix matches nothing | `missing_commit` | `2` |
| No in-scope commit at or before `--as-of` | `no_commits_at_time` | `2` |
| Store has no commit history (plain `eg scan` graph) | `empty_history` | `2` |

Non-fatal conditions surface as diagnostics inside a successful response:
`empty_surface` (no reportable files) and `no_recorded_changes` (a file
present at the anchor with no in-scope `Change` records).

## Response shape

```json
{
  "ok": true,
  "threshold_percent": 50,
  "disclaimer": "Rows are empirical history-derived leads ...",
  "anchors": [
    { "repository_id": "codegraph:v1:...", "commit_sha": "<full SHA>", "valid_time": "2026-01-06T00:00:00Z" }
  ],
  "total_file_count": 2,
  "returned_file_count": 2,
  "truncated": false,
  "files": [
    {
      "record_id": "codegraph:v5:...",
      "schema_version": 5,
      "repository_id": "codegraph:v1:...",
      "repo_relative_path": "src/hot.rs",
      "total_commits": 6,
      "bus_factor": 1,
      "primary_owner": { "author_name": "Alice Dev", "author_email": "alice@example.invalid", "commits": 4, "share": 0.6666666666666666 },
      "authors": [
        { "author_name": "Alice Dev", "author_email": "alice@example.invalid", "commits": 4, "share": 0.6666666666666666 },
        { "author_name": "Bob Dev", "author_email": "bob@example.invalid", "commits": 1, "share": 0.16666666666666666 },
        { "author_name": "Carol Dev", "author_email": "carol@example.invalid", "commits": 1, "share": 0.16666666666666666 }
      ]
    }
  ],
  "diagnostics": []
}
```

## When to use which tool

- **`eg query ownership`** — the aggregate concentration question: who owns
  what share of each file's recorded history, and how many people carry the
  knowledge.
- **`eg query who` (issue #116)** — the point question: which single author
  most recently changed one symbol, at or before a queried point in history.
- **`eg query deltas` (issue #118)** — what structurally changed between two
  commits, without an author dimension.
- **`git shortlog -sn -- <path>`** — fast and local, but plain text with no
  stable graph handles, no repository-identity scoping, no share or
  bus-factor metric, no `--as-of` time travel, and no redaction of author
  PII.
- **CODEOWNERS / declared-ownership systems** — a different question:
  *declared* authority rather than *empirical* history. This query
  deliberately never conflates the two.

And once more, because it is the sharp edge: **a low bus factor is a
knowledge-concentration lead to inspect, never proof that a file is
unmaintained or that other editors are incompetent.**
## Corpus scope

This is a history-analysis lane: it reads the **union of all commit snapshots**
by design and carries no `--at-head`/`--all-history` corpus flags. The summary
envelope discloses `corpus_mode: "union"` (or `single_snapshot` over a
snapshot-less store), `corpus_mode_source`, and `corpus_disclaimer` for
transparency. See [Corpus scope for query lanes](corpus-modes.md).
