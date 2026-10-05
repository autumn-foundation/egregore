# `eg freshness` — Store Staleness Reporting

**Issue:** #82 — _Stamp source snapshot identity and report store staleness_

---

## Overview

`eg freshness` answers one question: **does this store still match my working
tree?** Every `eg scan` / `eg ingest` stamps a store-level *source snapshot
identity* on the `Repository` node (the HEAD commit it was built from plus a
working-tree dirty flag — see
[`docs/schema/source-snapshot.md`](../schema/source-snapshot.md)). `eg freshness`
reads that stamp and compares it against the live working tree, so an agent never
cites `repo_relative_path` + `span` handles the code has already invalidated.

```
eg freshness [repo_path] (--graph <path> | --data-dir <dir>) [--repo-id-override <id>] [--format text|json]
```

* `repo_path` — the working tree to compare against (defaults to `.`).
* `--graph` / `--data-dir` — the store whose stamped snapshot to read (exactly one).
* `--format` — `text` (default, human-readable) or `json` (machine-readable, stable codes).

The command is **strictly read-only** and **fully offline**: it loads the store
through the same read-only path the query commands use, probes the working tree
with `git rev-parse HEAD` and `git status --porcelain`, and never creates,
modifies, or deletes any graph record, runtime file, index, or idempotency
receipt. It always exits `0` once a verdict is produced — the verdict (including
`unknown`) is the payload, not an error.

---

## Freshness states

| Code          | Meaning                                                                                  |
| ------------- | ---------------------------------------------------------------------------------------- |
| `fresh`       | Stored commit equals the current HEAD **and** the tree is clean.                         |
| `stale_head`  | The current HEAD differs from the stored commit.                                         |
| `stale_dirty` | HEAD matches but the tree has uncommitted/untracked changes (or the store itself was built from a dirty tree). |
| `unknown`     | The store predates snapshot stamping, or no Git context exists on either side.           |

A store whose stored HEAD differs from the working tree, or whose tree is dirty,
is **never** reported `fresh`. A failed dirty probe (e.g. a locked index) is
treated conservatively as dirty rather than silently reported `fresh`.

### What counts as "dirty"

The dirty probe (`git status`, run read-only with `GIT_OPTIONAL_LOCKS=0`) reports
any uncommitted change in the working tree, with two refinements:

- **The checked store artifact is excluded.** When the `--graph` file or
  `--data-dir` directory lives under the working tree, it is excluded from the
  probe, so the documented in-tree workflow (`eg scan . --out graph.jsonl`) is
  not reported `stale_dirty` merely because the store it just wrote is itself an
  untracked file.
- **Git-ignored and untracked files are excluded — and so are they from the graph.** The
  scanner scopes the scan to Git-tracked files only, skipping any untracked or
  git-ignored files for all supported languages (Rust, Python, TypeScript, Go).
  This keeps the indexed set aligned with what `git status` reports.


---

## Workflow

```sh
# 1. Build a store.
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore

# 2. … work, commit, rebase, or switch branches …

# 3. Before trusting cited file/span handles, check freshness.
eg freshness . --data-dir .egregore --format json
#   -> {"freshness":"stale_head", ...}  re-scan before citing handles
eg freshness . --graph graph.jsonl
```

When not `fresh`, refresh the store (`eg refresh` or a full `eg scan`+`eg ingest`)
before relying on its handles. `eg freshness` only **detects and reports**
staleness; it never re-scans, watches files, or repairs the store.

---

## JSON output

```json
{
  "freshness": "stale_dirty",
  "fresh": false,
  "repository_id": "codegraph:v3:…",
  "store_kind": "data_dir",
  "current_head": { "state": "commit", "sha": "…" },
  "current_dirty": true,
  "stored_snapshot": {
    "head": { "state": "commit", "sha": "…" },
    "dirty": false,
    "repository_id": "codegraph:v3:…",
    "scanned_at": "2026-06-14T00:00:00Z"
  },
  "message": "working tree has uncommitted changes relative to the stored snapshot; …"
}
```

The `freshness` code is stable. `stored_snapshot` is omitted for pre-stamping
stores (which classify as `unknown`).

---

## Freshness on the structural query commands

`eg query symbol`, `eg query file`, and `eg query context` accept `--repo-path`.
When set, each result carries a non-fatal `freshness` field with the same stable
code, so an agent can downgrade trust in the cited handle **without the answer
being suppressed**:

```sh
eg query symbol my_fn --graph graph.jsonl --repo-path . --format json
#   {"record_id":"…","name":"my_fn","repo_relative_path":"src/lib.rs", … ,"freshness":"stale_dirty"}
```

Without `--repo-path` the field is absent and output is byte-identical to before
this feature.

---

## Freshness on the MCP tools (issue #220)

The MCP tools stamp the same verdict onto their successful JSON responses as a
non-fatal, machine-readable `freshness` **object** — a trust signal, never
suppression: a non-`fresh` verdict still returns the full answer payload.

- `inspect_store`, `symbol_context`, and `task_evidence` each accept an optional
  `repo_path` (default: the MCP server's current directory, mirroring
  `eg freshness`'s default `.`) and carry the `freshness` object on every
  `ok: true` response. Error responses (`ok: false`) are untouched.
- `store_freshness` returns the same verdict for the whole store without
  requiring a symbol or task argument.

The object reuses the #186 contract — no new vocabulary:

```json
{
  "verdict": "fresh",
  "fresh": true,
  "repository_id": "codegraph:v3:repo:…",
  "repo_path": "/path/to/working/tree",
  "stored_snapshot": {
    "head": {"state": "commit", "sha": "abc…"},
    "dirty": false,
    "repository_id": "codegraph:v3:repo:…",
    "scanned_at": "2026-05-19T00:00:00Z"
  },
  "current_head": {"state": "commit", "sha": "abc…"},
  "current_dirty": false,
  "message": "store matches the current working tree (HEAD unchanged, tree clean)"
}
```

- `verdict` / `fresh`: the stable code (`fresh` / `stale_head` / `stale_dirty` /
  `unknown`), computed by the exact code path `eg freshness` uses, so the MCP
  verdict always agrees with the CLI verdict for the same store and tree.
- `stored_snapshot`: the source-snapshot identity the answer was derived from.
  A store that predates snapshot stamping carries the explicit
  `{"state": "pre_stamping"}` marker (never a silent absence, never a false
  `fresh`); `no_git` / `unborn_head` reuse the on-disk head serialization.
- `current_head` / `current_dirty`: the working-tree state at `repo_path` the
  stored snapshot was compared against.

The freshness path is strictly read-only and offline: the working-tree probe
runs `git rev-parse` / `git status` with `GIT_OPTIONAL_LOCKS=0` and the stamp is
a pure function of the loaded records plus that probe — no graph records,
indexes, or runtime files are created, modified, or deleted. For a fixed store +
working-tree state the object is byte-identical across calls.

---

## Out of scope

Auto-rescanning, file watching, incremental refresh (owned by `eg refresh`),
per-symbol line-level re-resolution, daemon-specific surfaces, and remote/hosted
freshness services are out of scope for this command.
