# eg query origin

Trace **a code symbol to the commit that introduced it, plus the
project-graph PR / issue / review records whose `merge_commit_sha` equals
that commit** (issue #159) — answer the provenance question *"which PR
introduced this symbol?"* — from a `scan-history` graph (plus a GitHub
import) or an embedded store. Local-first; no network access, no live GitHub
calls.

> **The project link is deterministic commit-SHA byte equality ONLY.** No
> fuzzy title or body matching: a PR whose `merge_commit_sha` merely shares a
> prefix with the introducing commit does not match, and an empty
> `pull_requests` section means "no imported PR merges as this commit", never
> "the symbol has no PR".

## Synopsis

```text
eg query origin <SYMBOL> --graph <PATH>   [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query origin <SYMBOL> --data-dir <DIR>  [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
```

`<SYMBOL>` is a stable symbol record ID or an exact symbol name. The query is
purely read-time: it reads only the supplied store, never Git state, so it
cannot mutate the working tree.

## Shortest offline workflow

```sh
# Replay history into a temporal JSONL graph (reads Git objects only)
eg scan-history . --out history.graph.jsonl

# Import GitHub state into the same store (offline afterwards)
eg import github --data-dir .egregore --repo acme/widget ...

# Which commit introduced this symbol, and which PR merged as that commit?
eg query origin parse_header --graph history.graph.jsonl
```

## Output contract

The answer keeps code facts and project facts in trust-separated sections,
mirroring `eg query context`'s contract:

- `code` (`source_derived`) — the symbol record ID, the introducing commit
  SHA, its valid time, the `Commit` record ID when the store carries one,
  and the symbol's path/span at introduction. The introducing commit is the
  first commit whose snapshot contains the symbol; reintroductions never move
  the origin.
- `project` (`project_state`) — `pull_requests`, `issues`, and `reviews`:
  the imported records whose flat `merge_commit_sha` field byte-equals the
  introducing commit, each echoing the SHA the link rests on. `link_rule` is
  stated on every answer.

When the store carries code history but no GitHub-imported records at all,
the answer degrades to a commit-only result: `project.github_import` is
`"absent"`, the sections are empty, and `project.note` carries the explicit
`github_import_absent` marker. That is not an error, and it does not mean no
PR introduced the symbol.

## Temporal selectors

`--at <COMMIT>` resolves the origin against the ancestry-closed code state
at that commit; `--as-of <RFC3339>` against commits whose valid time is at
or before the instant. A symbol introduced after the pinned state is
`no_match` (as of the pin, it did not exist). The pin scopes the *code*
history only: project records are store-level import state and match as-is.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| 0 | origin traced (possibly with empty project sections) |
| 1 | ambiguous symbol name, ambiguous commit prefix, malformed `--as-of` |
| 2 | `no_match` (unknown symbol, or not present as of the pin), `no_history` (symbol without commit-linked history), `missing_commit` |

## Boundaries

- Symbol granularity only: this lane does not do line-level blame.
- Introducing PR only: it never reports the last-touching PR.
- Squash/merge-commit case only: multi-commit PR heuristic attribution is
  future work.
