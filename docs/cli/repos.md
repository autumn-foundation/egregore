# `eg repos` — Repository Catalog with Query Selectors

**Issue:** #193 — _Catalog a store's repositories with selectors to scope queries_

---

## Overview

`eg repos` answers one question: **which repositories does this store hold,
and what handle do I pass to scope a query to each one?** It lists every live
repository with its stable record ID, its `identity_source`, the human-usable
scope selector the `--repo` query lanes accept verbatim, a node count, and a
recency hint.

```
eg repos --data-dir <dir>   [--format json|text]   # embedded store, no daemon (issue #193)
eg repos <graph.jsonl>      [--format json|text]   # pre-ingest JSONL file
```

This closes the chicken-and-egg gap left by #67: repository-scoped queries
require the caller to name a repository, but nothing in the public surface
told the caller which repositories a shared store contains. The catalog is
that roster — the repository picker for the `repo:` scoping #67 shipped.

## The daemon-free embedded workflow (issue #193)

```
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
eg repos --data-dir .egregore
```

`eg repos --data-dir` reads the embedded AletheiaDB store **directly** — no
running daemon, no network, no embeddings required — through the same
embedded read path the `eg query ... --data-dir` surface uses.

### Read-only guarantee

The catalog is strictly read-only. The embedded engine re-persists its index
files on open, so the command copies the store to a throwaway temporary
directory and reads the copy — zero graph records, indexes, idempotency
receipts, or runtime files are created, modified, or deleted in the
cataloged store. Re-running the command on an unchanged store produces
byte-identical output (canonical ascending-ID ordering, no catalog-time
timestamps).

### Round-trip contract

Every `selector` the catalog emits is accepted **verbatim** by the #67
repository-scoped query path (`RepositoryIndex::resolve_selector`, the same
resolver every `--repo` lane uses):

```
selector=$(eg repos --data-dir .egregore | head -1 | jq -r .selector)
eg query symbols --data-dir .egregore --repo "$selector" ...
```

When two repositories would share a human selector (e.g. the same basename
under different remotes), the collision is **surfaced, not de-duplicated**:
each entry keeps its distinct stable ID, `selector_collision` is `true`,
`colliding_selector` names the shared handle, and the emitted `selector`
falls back to the stable record ID — which the scoping path always accepts.

## JSON contract (newline-delimited, one repository per line)

```jsonc
{
  "repository_id": "codegraph:v5:repo-9f2c…",   // stable repository record ID
  "identity_source": "remote",                  // remote | local_root_commit | local_path | operator_override
                                                // (null for legacy nodes without an identity payload)
  "selector": "acme/alpha",                     // verbatim #67-accepted scope selector;
                                                // the stable ID itself when selector_collision is true
  "selector_collision": false,                  // true when the human handle is shared
  "colliding_selector": null,                   // the shared human handle (present iff selector_collision)
  "node_count": 128,                            // node records attributed to this repository
                                                // (all domains; includes the repository node itself)
  "recency_kind": "commit",                     // commit | ingest_timestamp (null when unknown)
  "recency_handle": "aaa111…"                   // HEAD commit SHA or RFC 3339 ingest timestamp
}
```

Field shape is stable: every field is present on every line (`null` where a
value is unknown). The allow-list is IDs, identity handles/hashes, selectors,
counts, and a recency handle — never raw transcript text, command output,
patch hunks, issue/PR bodies, environment values, or tokens.

### Empty store

A store with no live repositories reports an explicit empty roster with a
distinct, stable diagnostic (exit 0 — this is a report, not silent success):

```json
{"repositories": [], "diagnostic": {"code": "empty_repository_roster", "message": "the store contains no live repositories"}}
```

### Missing or unreadable `--data-dir`

Fails with an operator-facing diagnostic that names the path, e.g.:

```
Error: error: embedded store not found at ./nope - run `eg ingest --adapter embedded --data-dir <path>` first
```

## Text format (`--format text`)

```text
repository: codegraph:v5:repo-9f2c…
  identity_source: remote
  selector: acme/alpha
  nodes: 128
  recency: commit aaa111…
```

A colliding entry adds a `selector_collision: true` row naming the shared
human handle; an empty store prints `repositories: none` plus the
`empty_repository_roster` diagnostic line.

## Scope notes

- The catalog lists each repository **once**, at its highest-version
  (current) view: superseded schema versions and tombstoned repositories are
  excluded, so `--graph` and embedded `--data-dir` reads agree.
- Per-domain / per-kind / per-schema-version composition counts stay with
  `eg inspect` (#125); store-vs-working-tree staleness analysis stays with
  `eg freshness` (#82/#186); the recency hint here is a hint, not a verdict.
