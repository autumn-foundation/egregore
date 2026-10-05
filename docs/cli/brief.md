# `eg brief` — Working-Tree-Diff-Scoped Evidence Briefing

**Issue:** #214 — _Brief agents on graph evidence scoped to the working-tree diff_

---

## Overview

`eg brief` answers one question: **what does the graph already know about exactly
what I changed?** It computes the uncommitted working-tree diff at `repo_path`
(staged + unstaged, or staged-only with `--staged-only`), resolves the changed files
and the symbols whose recorded spans intersect the changed hunks into `File` /
`Symbol` handles, and returns one JSON object with trust-separated sections —
prior failures (runtime vs agent-authored), source facts, observations,
decisions, in-flight tasks, verification evidence, drift warnings — plus a
`store_coverage` map and the store-freshness staleness marker shared with
`eg freshness` (issue #220).

```
eg brief [repo_path] (--graph <path> | --data-dir <dir>) [--repo <id>] [--staged-only] [--format json|text]
```

* `repo_path` — the working tree to diff (defaults to `.`).
* `--graph` / `--data-dir` — the store to read (exactly one, required).
* `--repo` — restrict handle resolution to one stored repository by its identity
  ID (shared-store scoping, issue #67).
* `--staged-only` (alias `--staged`) — only consider staged (index vs `HEAD`) changes; unstaged
  working-tree changes and untracked files are excluded.
* `--format` — `json` (default) or `text`.

The command is **strictly read-only**: the diff is computed with read-only Git
plumbing (`GIT_OPTIONAL_LOCKS=0`, so Git never refreshes the index; no checkout,
stash, or index write), and the store is only read — never re-scanned or
mutated. A clean working tree exits `0` with `ok: true` and empty but
well-formed sections.

---

## Diff semantics

| Diff mode        | Git command            | Untracked files |
| ---------------- | ---------------------- | --------------- |
| default          | `git diff HEAD`        | included        |
| `--staged-only` | `git diff --cached HEAD` | excluded      |

* Only the repository **root** is accepted (`repo_path` must equal `git rev-parse
  --show-toplevel`); a non-repo path exits `2` with `{"ok": false, "error":
  {"code": "not_a_git_repo"}}`, and a repository with no commits exits `2` with
  `{"ok": false, "error": {"code": "unborn_head"}}`.
* Tracked changes come from `git diff --name-status -z --find-renames` (the
  authoritative path list) plus `git diff --unified=0 --no-ext-diff` for
  per-hunk line ranges. Renames resolve against the store by the new path first,
  falling back to the old path (the store indexes the `HEAD` version).
* Untracked (non-ignored) files are listed via `git ls-files --others
  --exclude-standard` with a whole-file changed range counted locally.
* Paths with no `File` record in the store are reported in the distinct
  `unindexed` list — never silently dropped.

### Old-side vs new-side ranges

The recorded graph spans describe the indexed `HEAD` version, so **symbol
intersection uses old-file (`HEAD`-side) ranges**: a symbol is in the working
set only when its recorded span intersects a changed hunk's old-side range.
Pure-deletion hunks contribute only old-side ranges (a deleted file's symbols
are still reported); insertion-only hunks use a bounded `(old_start,
old_start+1)` anchor around the insertion point so an insertion inside an
existing symbol still matches it. Symbols with no recorded span never match.

---

## JSON contract

One single-line JSON object: `{"ok": true, "result": {…}}` (the
one-JSON-object-per-line contract from `docs/cli/query.md`). The `result`
payload:

| Section                | Contents |
| ---------------------- | -------- |
| `repo_path`            | The diffed working tree (as given). |
| `staged_only`          | Whether `--staged-only` was passed. |
| `changed_files`        | One row per resolved file: `path`, `status` (`added`/`modified`/`deleted`/`renamed`/`untracked`), `renamed_from`, `record_id`, `hunk_count`, `changed_lines`. |
| `changed_symbols`      | One row per intersected symbol: `record_id`, `trust`, `name`, `path`, `span`. |
| `unindexed`            | Changed paths with no `File` record: `path`, `status`, `renamed_from`. |
| `source_facts`         | Code-graph source facts merged across the working set (identity, kind, path, span only — never source bodies). |
| `prior_failures`       | One aggregated failure-history traversal (`target_kind: "working_set"`): `runtime_failures` (verification domain) vs `agent_failures` (agent-authored `Failure` claims), plus machine-readable `diagnostics`. Redaction-safe rows identical to `eg failure-history`. |
| `observations`         | Agent-authored observations linked to the working set (provenance attached; never source truth). |
| `decisions`            | Agent-authored decisions, split out from observations. |
| `in_flight_tasks`      | Project/PR records touching the files, filtered to non-terminal status (`open`, `in_progress`, `blocked`, `unknown` for tasks; `open` for issues/PRs). Terminal records (`closed_completed`, `closed_dropped`, `closed`, `merged`) are excluded. |
| `verification_evidence`| Verification runs and test results touching the working set. |
| `drift_warnings`       | `SemanticDrift` records targeting the working set (DriftsFrom edge, falling back to `target_record_id`), ordered score descending then record ID. |
| `store_coverage`       | Domain presence map (see below). |
| `staleness`            | The `eg freshness` verdict (`fresh` / `stale_head` / `stale_dirty` / `unknown`) with its message. A briefing over a dirty tree carries `stale_dirty` rather than failing or silently returning partial data. |

Every section is ordered deterministically (by stable record ID; working-set
rows by path), so repeated runs on an unchanged tree and store produce
byte-identical output.

### `store_coverage` — domain-absent vs entity-absent (issue #196)

Each value states whether the store contains at least one record in that
trust-section domain:

```json
"store_coverage": {
  "code_graph": true,
  "agent_memory": false,
  "project": false,
  "artifact": false,
  "verification": false
}
```

Classification rule: an empty briefing section whose domain is **present** means
"no records for this working set" (*entity-absent*); an empty section whose
domain is **absent** means "domain not in this store" (*domain-absent*) — e.g.
empty `observations` with `agent_memory: false` must not be read as "no prior
lessons". No other command is needed to make this distinction. Presence never
asserts code is untested, unverified, or broken.

`--format text` spells the same distinction out per section (`entity-absent` /
`domain-absent` markers on empty sections).

---

## Examples

Brief the current working tree against a graph JSONL:

```
eg brief --graph code.graph.jsonl
```

Staged-only briefing of a repo at another path, as text:

```
eg brief /path/to/repo --graph code.graph.jsonl --staged-only --format text
```

Shared store holding several repos — scope handle resolution to one:

```
eg brief --graph shared.graph.jsonl --repo <repository-id>
```

---

## Exit codes

| Code | Meaning |
| ---- | ------- |
| 0    | Briefing produced (including clean trees and empty working sets). |
| 2    | Not a Git repository root (`not_a_git_repo`) or unborn `HEAD` (`unborn_head`); a JSON diagnostic is printed. |
| ≠0   | Store load failure or Git plumbing failure (message on stderr). |
