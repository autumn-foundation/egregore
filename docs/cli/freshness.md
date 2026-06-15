# `eg query freshness` — Evidence-Link Freshness Verdicts

**Issue:** #85 — _Flag agent observations whose cited code has drifted since recording_

---

## Overview

`eg query freshness` flags each **agent observation** by whether the code it
cites has **drifted** since the observation was recorded. For every
`Observation` / `Decision` that cites a code handle (file/symbol at a recorded
commit or valid-time), it returns a per-evidence-link **freshness verdict** so
you can downgrade trust in stale notes instead of acting on a span the live code
has already invalidated.

> **A `drifted` / `unresolved` verdict is a freshness lead, never a truth
> claim.** It states only that the *evidence basis moved* — not that the
> observation is now false, superseded, or correct. A `drifted` verdict is a
> reason to **re-verify**, not proof the note is wrong.

Strictly **read-only**, **local-first**, and **deterministic**: no network
access, no hosted indexing, no remote crawling, no mandatory remote embeddings.
The workflow creates, modifies, and deletes **zero** records — re-running it
against an unchanged store produces byte-identical output.

```
eg query freshness --graph <PATH> [--stale-only]
eg query freshness --data-dir <DIR> [--stale-only]
```

Reads from either a JSONL graph (`--graph`) or an embedded AletheiaDB store
(`--data-dir`).

---

## Shortest Workflow

```sh
# 1. Build a code graph (with history, so drift signals exist).
eg scan-history . --out history.graph.jsonl

# 2. … agents write observations citing code handles over time …

# 3. Which of my notes now stand on moved ground?
eg query freshness --graph history.graph.jsonl --stale-only
```

`--stale-only` returns only `drifted` + `unresolved` observations. When nothing
is stale, the result is reported with the stable diagnostic
`no_stale_observations` — never silently as success-with-nothing.

---

## Verdicts

| Verdict | Meaning |
|---------|---------|
| `current` | The cited code is unchanged since the observation's anchor. |
| `drifted` | The cited symbol/file changed after the observation's anchor. |
| `unresolved` | The cited handle no longer resolves (symbol removed/renamed or file deleted). |
| `untemporal` | The observation carries no commit, valid-time, or recording time to anchor a comparison. |

Verdicts are mutually exclusive and decided in this precedence: `unresolved`
(anchor-independent) → `untemporal` (no anchor) → `drifted` → `current`.

A retracted (tombstoned) observation is omitted entirely — a deleted note is no
longer current memory, matching the current-state reads of the other query
paths. Evidence links to non-handle codegraph records (a `Commit`/`Change` cited
by `EXPLAINS_CHANGE`) are valid links, not code handles, and are not classified.

### The anchor

The comparison anchor is, in order:

1. the evidence link's `as_of_commit`,
2. the evidence link's `target_git_commit`,
3. the observation's `valid_time`,
4. the observation's recording time (`observed_at` / `ingested_at`).

If none is present the verdict is `untemporal`. Most notes carry a recording
time even when they omit a commit and a valid-time, so "drifted since recording"
remains answerable rather than collapsing to `untemporal`.

A triple citation `(path, span, target_git_commit)` is resolved **at its anchor
commit**, scanning historical and tombstoned versions, and matches any code
handle — `Symbol`, `Module`, or `Import` — by span before falling back to the
whole file. If the cited symbol was later removed or renamed and a different live
symbol reused the same path/span, the verdict is `unresolved` for the original
identity rather than a silent re-point at the new occupant.

### Liveness in a history graph

A handle is resolved against the **frontier** — the latest snapshot in the graph.
`scan-history` re-emits a full snapshot at every commit but writes no `Tombstone`
when a symbol is removed or renamed, so a handle present only at older commits is
treated as **gone**, not live: a citation to it is `unresolved`, never a false
`current`. A current-tree `scan` (no temporal versions) has no frontier and every
present handle stays live.

### Trigger sources (reused, never re-derived)

Drift is read from signals the graph already stores, anchored to the **cited
handle** — a sibling symbol changing under the same file never flags a neighbor:

- **`drift_record`** — a semantic-drift record (issue #55) whose
  `prior_record_id` (or `DRIFTS_PRIOR` edge target) equals the cited record ID
  and whose later measurement post-dates the anchor — or whose `before` commit is
  exactly the anchor commit. Triggering handle: the drift record ID + later
  commit.
- **`content_change`** — a later code-graph version of the *same* cited record ID
  (same handle, distinct `temporal.git_commit`) whose content hash differs from
  the anchor version. Triggering handle: the later commit + content hash.
- **`handle_removed`** — the cited handle survives only as a tombstone.
  Triggering handle: the tombstone record ID.
- **`handle_absent`** — the cited handle resolves to nothing in the store.

---

## Response shape

```json
{
  "ok": true,
  "stale_only": false,
  "counts": { "current": 2, "drifted": 2, "unresolved": 1, "untemporal": 1 },
  "diagnostic": "freshness_verdicts",
  "verdicts": [
    {
      "observation_id": "agent_memory:v1:…",
      "kind": "Observation",
      "verdict": "drifted",
      "freshness_lead": "evidence basis moved since the observation's anchor; a reason to re-verify, not proof the note is wrong",
      "provenance": {
        "agent_id": "agent_1",
        "session_id": "sess_1",
        "observed_at": "2026-02-01T00:00:00Z",
        "confidence": "0.9"
      },
      "cited_handle": {
        "target_record_id": "codegraph:v…",
        "repo_relative_path": "src/query.rs",
        "span": { "start_byte": 0, "end_byte": 100, "start_line": 30, "end_line": 40 },
        "anchor_commit": "commit_a",
        "anchor_valid_time": "2026-01-01T00:00:00Z",
        "relation": "OBSERVES",
        "target_domain": "codegraph"
      },
      "triggering_handle": {
        "kind": "drift_record",
        "drift_record_id": "semantic:v1:…",
        "after_git_commit": "commit_b",
        "after_valid_time": "2026-01-02T00:00:00Z"
      }
    }
  ]
}
```

### Trust separation

The verdict attaches to the observation's **evidence link**, never to the
deterministic code fact. A code fact is never rewritten, hidden, or marked stale
by this workflow, and an observation is never promoted to source truth.

### Safety: no raw payloads

Output never includes raw transcript text, command output, patch hunks,
issue/PR bodies, environment values, or tokens. Only record IDs, hashes,
handles, spans, confidence, and redaction markers are emitted. The observation's
raw `text` body is never surfaced.

---

## When to use this versus other tools

| Reach for | When you want |
|-----------|---------------|
| **`eg query freshness`** (this) | Whether an **agent note's cited code** has moved since the note was recorded — batch "which of my notes are now stale?" |
| `eg query memory` (#64) | The **evidence behind a memory claim**: provenance, support, author-written contradictions/supersessions. It surfaces *existing* supersession records; it does not *compute* drift-based link staleness. |
| `eg refresh` (#82) | Store-vs-working-tree freshness for **code facts** (does the store match the tree?). It excludes memory and per-symbol re-resolution. |
| `eg query drift` (#55) | The largest **code-only** semantic drifts, ranked by score. It says nothing about which memory cited that code. |
| `git blame` / `git log -L` | Manual, per-file archaeology of when specific lines changed — no link from a note to the code, no trust separation, no batch answer. |
| `rg` | Fast recursive **text** search when you know where to look. |

`git blame` + manual date comparison is the honest substitute and the real bar:
a maintainer *can* check whether the lines a note cited have changed since a
date, but it is per-file manual work with no link from the note to the code and
no batch "which of my notes are now stale?" answer. Egregore already stores both
the observation's anchor and the drift; this workflow is the small deterministic
join that closes the loop — and it remains a freshness lead, not a truth verdict.

---

## Scope

This slice consumes existing agent-memory, evidence-link, semantic-drift,
temporal-selector, repository-identity, redaction, and schema-version contracts.
It introduces no new graph domain, drift algorithm, importer, edge vocabulary,
trust class, hosted service, LLM-generated answer, or memory-mutation workflow.
Detecting and labelling is the whole job — auto-invalidating, deleting,
rewriting, superseding, or re-pointing stale memory is explicitly out of scope
(supersession authoring stays with #50/#64).
