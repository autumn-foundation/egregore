# eg query coupling

Rank the files that **historically changed in the same commits** as a target
file (issue #153) — answer the pre-edit question *"what else usually changes
together with this file?"* — from a `scan-history` graph or an embedded
store. This is the evolutionary-coupling signal static call/import edges
miss: a struct and its serialization fixture, a schema and its docs, an API
handler and its client can co-change for years with no syntactic edge between
them. Local-first; no network access, hosted indexing, remote crawling, or
mandatory remote embeddings.

> **Rows are historical co-change leads, not proof of dependency, breakage,
> behavior change, or verification.** No causality is inferred from commit
> messages, file names, or proximity — and absence of coupling is not proof
> of independence: two files can be tightly dependent yet never share a
> commit in the recorded history.

## Synopsis

```text
eg query coupling <PATH> --graph <PATH>    [--repo <SELECTOR>] [OPTIONS]
eg query coupling <PATH> --data-dir <DIR>  [--repo <SELECTOR>] [OPTIONS]

OPTIONS:
  --base <COMMIT> --head <COMMIT>   bound to the (base, head] commit range
  --at <COMMIT>                     bound to the ancestor closure of one commit
  --as-of <RFC3339>                 bound by valid time (committer date)
  --min-support <N>                 minimum shared commits (default 2, max 100)
  --limit <N>                       partner-row cap (default 20, max 500)
  --format json|text                output format (default json)
```

`<PATH>` is the repo-relative path of the target file and must resolve to an
existing `File` node. Commit handles are full SHAs or unique prefixes
resolved against the store's `Commit` nodes. The query is purely read-time:
it reads only the supplied store, never Git state, so it cannot mutate the
working tree. (The store itself is produced by `eg scan-history`, which reads
Git objects only and leaves the checkout byte-for-byte unchanged.)

## Shortest offline workflow

```sh
# Replay history into a temporal JSONL graph (reads Git objects only)
eg scan-history . --out history.graph.jsonl

# Who historically changes together with src/parser.rs?
eg query coupling src/parser.rs --graph history.graph.jsonl

# Only strong pairs, human-readable
eg query coupling src/parser.rs --graph history.graph.jsonl --min-support 5 --format text

# Coupling inside one release window
eg query coupling src/parser.rs --graph history.graph.jsonl --base 4f0c2b1 --head 9ad3e77
```

## The metric (`jaccard_v1`)

Let `A` be the set of distinct in-scope commits that modified the target and
`B` the set for a candidate partner (both from the `CHANGED_IN` edges
recorded by `scan-history`).

| Field | Definition | Meaning |
|-------|------------|---------|
| `co_change_count` | `\|A ∩ B\|` | distinct in-scope commits that modified both files |
| `target_change_count` | `\|A\|` | the target's total in-scope change count (repeated on every row) |
| `partner_change_count` | `\|B\|` | the partner's total in-scope change count |
| `coupling` | `\|A ∩ B\| / \|A ∪ B\|` | normalized symmetric strength (Jaccard); the ranking key |
| `confidence` | `\|A ∩ B\| / \|A\|` | directional: the fraction of the target's changes that also touched the partner |

Partners are ranked by `coupling` **descending**. The symmetric Jaccard
denominator grows with the partner's own churn, so a file that touches every
commit cannot dominate the ranking purely by volume. Ties break on a
documented stable key: `co_change_count` descending, then repo-relative path
ascending, then record ID ascending. The ranking comparison is computed with
integer cross-multiplication (never float rounding), so ordering is exact
and byte-stable.

Every row also carries the stable `record_id` + `schema_version` of the
partner's `File` node, the newest shared commit (`last_co_change_commit`,
chosen by valid time then SHA) with its `valid_time`, and the trust label
`historical_co_change_lead`.

## Noise suppression and completeness

- `--min-support <N>` (default **2**, bounds **1..=100**) suppresses pairs
  sharing fewer than `N` distinct commits. The threshold used is echoed back
  as `min_support` in every answer.
- `--limit <N>` (default **20**, bounds **1..=500**) caps partner rows. The
  answer always states `total_partners` (before the cap) and `truncated`, so
  a capped answer is never mistaken for a complete one.
- An empty partner set is an explicit machine-readable success (exit 0) with
  a stable diagnostic — `target_never_changed_in_scope` or
  `no_partner_at_or_above_min_support` — never silent empty output.

## Temporal scope

Consistent with the temporal-selector contract of the other history queries:

- **Default**: all commits recorded in the store (full history).
- **`--base B --head H`**: the `(base, head]` range — commits reachable from
  head but not from base — with exactly the issue #118 endpoint semantics
  and error taxonomy (identical endpoints, reversed range, no path).
- **`--at C`**: the ancestor closure of one commit (inclusive) — the history
  as it existed at `C`.
- **`--as-of T`**: commits whose recorded valid time (committer date) is at
  or before the RFC 3339 instant `T`.

The selectors are mutually exclusive; `--base`/`--head` come as a pair. The
resolved scope (selector kind, resolved SHAs, and in-scope commit count) is
echoed in the `scope` section of every answer.

## Scope and trust boundaries

- Both target and partners must resolve to `File` nodes recorded by the
  scanner, so scope automatically honors repository identity and the
  Git-tracked / `.gitignore` boundaries (#67, #99): untracked paths, ignored
  paths, and non-source files never appear as either target or partner, and
  a query for one fails with `unknown_file` rather than returning zeros.
- `--repo <SELECTOR>` gates commit resolution, file resolution, and counting
  to one repository in a shared store; a path present in two repositories
  without a scope fails with `ambiguous_file` listing every candidate.
- File granularity only: symbol-level co-change is a documented follow-up
  (mirroring how churn deferred symbol granularity).
- Output is bounded and redaction-safe: record IDs, paths, counts, the
  documented metrics, commit handles, and valid times only — never blob
  contents, patch hunks, secrets, or tokens.
- Repeating the same query on unchanged history yields byte-identical
  canonical output.

## Exit codes and diagnostics

Failures are stable machine-readable diagnostics
(`{"ok":false,"error":{"error_type":...}}` on stdout), never partial or
silent output:

| Condition | `error_type` | Exit |
|-----------|--------------|------|
| Success (including an explicit empty partner set) | — | `0` |
| Empty or `.`-only target path | `malformed_path` | `1` |
| Path resolves to multiple `File` nodes (no `--repo`) | `ambiguous_file` | `1` |
| `--min-support` outside `1..=100` | `invalid_min_support` | `1` |
| `--limit` outside `1..=500` | `invalid_limit` | `1` |
| `--as-of` not RFC 3339 | `invalid_as_of_timestamp` | `1` |
| Invalid selector combination | `malformed_selector` | `1` |
| Commit prefix matches multiple commits | `ambiguous_commit_prefix` | `1` |
| Both endpoints resolve to the same commit | `identical_endpoints` | `1` |
| Base is a descendant of head | `reversed_range` | `1` |
| No ancestor path connects the endpoints | `no_path` | `1` |
| No `File` node for the path (unknown/untracked/ignored) | `unknown_file` | `2` |
| Commit prefix matches nothing | `missing_commit` | `2` |
| No commit at or before the `--as-of` instant | `no_commit_at_or_before` | `2` |
| Store has no commit history | `empty_history` | `2` |

## Response shape

```json
{
  "ok": true,
  "target": {
    "record_id": "codegraph:v5:...",
    "schema_version": 5,
    "repo_relative_path": "src/alpha.rs",
    "change_count": 4
  },
  "scope": { "selector": "full_history", "commit_count": 5 },
  "min_support": 2,
  "limit": 20,
  "coupling_metric": "jaccard_v1",
  "disclaimer": "Rows are historical co-change leads ...",
  "total_partners": 2,
  "truncated": false,
  "partners": [
    {
      "record_id": "codegraph:v5:...",
      "schema_version": 5,
      "repo_relative_path": "src/beta.rs",
      "co_change_count": 3,
      "partner_change_count": 3,
      "target_change_count": 4,
      "coupling": 0.75,
      "confidence": 0.75,
      "last_co_change_commit": "<full SHA>",
      "last_co_change_valid_time": "2026-01-03T00:00:00Z",
      "trust": "historical_co_change_lead"
    }
  ],
  "diagnostics": []
}
```

## When to use which tool

- **`eg query coupling`** — the backward-looking correlation question: "what
  else has historically changed in the same commits as this file?" Catches
  partners with **no** syntactic edge.
- **`eg query change-impact` (issue #76)** — the forward, structure-derived
  question: "what calls/imports/references this now?" Static edges and
  historical co-change catch different partners; run both before a risky
  edit.
- **`eg query deltas` (issue #118)** — what actually changed between two
  specific commits, grouped by class; not a ranking of habitual partners.
- **`eg query lifeline` (issue #96)** — one symbol's lifecycle across
  history, not cross-file correlation.
- **`git log --name-only --pretty=format:` + shell counting** — fast and
  zero-setup, but plain text: no stable graph handles, no repository or
  `.gitignore` scoping, no normalized metric, no support threshold, and no
  join from a partner file to its symbols, agent memory, or verification
  evidence.

And once more, because it is the sharp edge: **co-change is correlation
recorded in history, never causation** — inspect the partner, don't assume
it must change.
