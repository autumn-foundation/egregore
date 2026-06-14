# Source Snapshot Identity

**Issue:** #82 — _Stamp source snapshot identity and report store staleness_

## Overview

A `scan`/`ingest` store is a snapshot of one working-tree state, but the stable
[repository identity](repository-identity.md) alone does not say *which* snapshot
the store was built from. The **source snapshot identity** records that: a
store-level stamp on the `Repository` node carrying the HEAD commit and the
working-tree dirty flag at scan time. A reader compares it against the live
working tree to classify store freshness (`fresh` / `stale_head` / `stale_dirty`
/ `unknown`) and avoid citing file/span handles the code has already
invalidated. See [`docs/cli/freshness.md`](../cli/freshness.md).

## Placement

The snapshot is a single optional field, `source_snapshot`, on the `Repository`
node — one per store, alongside the `repository_identity` payload. It is absent on
all other node kinds, and absent on stores produced before snapshot stamping
existed (those classify as `unknown`). It is **not** an identity input: two clones
of the same repository at different commits still share one stable `Repository`
ID; only their `source_snapshot` differs.

## Payload

```jsonc
"source_snapshot": {
  "head":   { "state": "commit", "sha": "<full 40-hex commit SHA>" },
  "dirty":  false,
  "repository_id": "codegraph:v3:…",
  "scanned_at": "2026-06-14T00:00:00Z"
}
```

| Field           | Meaning                                                                                           |
| --------------- | ------------------------------------------------------------------------------------------------- |
| `head`          | HEAD state at scan time (tagged union, see below).                                                |
| `dirty`         | `true` when the tree had uncommitted/untracked changes at scan time; always `false` for non-commit heads. |
| `repository_id` | The stable `Repository` ID this snapshot describes (the identity already used by the graph).      |
| `scanned_at`    | RFC 3339 scan time. Flows through the transaction-time override, so it never breaks determinism.  |

### `head` states

| `state`        | Condition                                                                 |
| -------------- | ------------------------------------------------------------------------- |
| `commit`       | The scanned path is a Git repository **root** with a resolvable `HEAD`; carries `sha`. |
| `no_git`       | The scanned path is not a Git repository root (a sub-directory of a repo, a non-Git directory, or Git unavailable). |
| `unborn_head`  | The scanned path is a Git repository root whose `HEAD` has no commits yet. |

The `commit`/`no_git` gate **mirrors the repository-identity module**: a commit
SHA is recorded only when the scanned path is the actual Git repository root.
Scanning an in-repo sub-directory (e.g. a fixture inside a larger repo) yields
`no_git`, so the surrounding repository's HEAD never leaks into the snapshot and
fixture scans stay byte-for-byte deterministic.

## Determinism

The deterministic portion of the snapshot — `head` (commit SHA) and `dirty` — is
a pure function of the scanned tree, so it is reproducible for an unchanged clean
tree at a fixed commit. The only wall-clock value, `scanned_at`, is set to the
scan's `transaction_time`; under the existing transaction-time override
(`scan_repository_at_with_override`) the entire `Repository` record — snapshot
included — is byte-stable.

## Freshness classification

Given the stored snapshot and the current working tree's head + dirty state:

1. No stored snapshot, or a non-`commit` head on either side → `unknown`.
2. Stored commit ≠ current HEAD → `stale_head`.
3. HEAD matches but the live tree is dirty, **or** the store was built from a
   dirty tree → `stale_dirty`.
4. Otherwise → `fresh`.

A store whose stored HEAD differs from the working tree, or whose tree is dirty,
is never reported `fresh`.

## Round-trip

The embedded `AletheiaDB` adapter persists the payload as the
`source_snapshot_json` node property and reads it back into `source_snapshot`, so
freshness can be reported from a `--data-dir` store as well as from a `--graph`
JSONL file.
