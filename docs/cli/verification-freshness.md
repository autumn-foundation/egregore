# `eg query verification-freshness` — Verification-Evidence Freshness Verdicts

**Issue:** #111 — _Flag verification evidence whose cited code has drifted since the run_

---

## Overview

`eg query verification-freshness` flags each **verification record**
(`TestRun`, `CIStatus`, `BenchmarkRun`, `CoverageReport`, `ProofResult`) by
whether the code it cites has **drifted** since the run's anchor. For every
citation via `FAILED_ON` / `MENTIONS_SYMBOL` / `TOUCHED_FILE`, it returns a
per-citation **freshness verdict** — so a `TestRun status=pass` recorded 40
commits ago is never handed back as authoritative green without a signal that
the code it covered has moved.

> **A `stale` / `unresolved` verdict is a freshness lead, never a
> re-judgment of pass/fail.** It states only that the *verified basis
> moved* — never that the recorded `status` is now wrong, that the code is
> broken, or that it is correct/safe. A `stale` verdict is a reason to
> **re-verify**, not proof the recorded result is wrong.

Strictly **read-only**, **local-first**, and **deterministic**: no network
access, no hosted indexing, no remote crawling, no mandatory remote
embeddings. The workflow creates, modifies, and deletes **zero** records —
re-running it against an unchanged store (and an unchanged `--repo-path`, when
given) produces byte-identical output.

```
eg query verification-freshness [scope] --graph <PATH> [--repo <selector>] [--repo-path <DIR>] [--stale-only] [--limit <n>] [--format text|json]
eg query verification-freshness [scope] --data-dir <DIR> [--repo <selector>] [--repo-path <DIR>] [--stale-only] [--limit <n>] [--format text|json]
```

Reads from either a JSONL graph (`--graph`) or an embedded AletheiaDB store
(`--data-dir`; read through a throwaway copy — strictly read-only). The
comparison needs the code version current **at the anchor**, which can be a
superseded (pre-drift) version, so this lane always reads the
history-inclusive store view — like `eg query evidence-freshness` (issue #85)
and unlike the corpus-mode-flipped lanes (`deps`, `path`, …). It does not
participate in the `--all-history`/`--at-head` corpus-mode contract
documented in `docs/cli/corpus-modes.md`.

### Distinct from `eg query evidence-freshness` (issue #85)

The two commands look similar and share verdict machinery, but age
**different domains**:

| | `eg query evidence-freshness` (#85) | `eg query verification-freshness` (#111) |
|---|---|---|
| Ages | Agent-memory `Observation` / `Decision` citations | Verification-domain `TestRun`/`CIStatus`/`BenchmarkRun`/`CoverageReport`/`ProofResult` citations |
| Trust class | `agent_authored` | `deterministic-but-runtime-derived` |
| Anchor | Evidence-link `as_of_commit`/`target_git_commit`, else the observation's `valid_time`/`observed_at`/`ingested_at` | The record's own `temporal.git_commit`, else `executed_at` |
| Verdict names | `current` / `drifted` / `unresolved` / `untemporal` | `current` / `stale` / `unresolved` / `unanchored` |

The two never share a verdict row — an agent observation is never counted as
verification evidence, and a verification record is never counted as an
agent observation.

---

## Shortest Workflow

```sh
# 1. Build a code graph (with history, so drift signals exist).
eg scan-history . --out history.graph.jsonl

# 2. Capture test runs as citable TestRun records (issue #165).
eg capture-tests --graph history.graph.jsonl --out history.graph.jsonl ...

# 3. Which of my recorded passes now stand on moved ground?
eg query verification-freshness --graph history.graph.jsonl --stale-only
```

`--stale-only` returns only `stale` + `unresolved` records. When nothing is
stale, the result carries the stable diagnostic `no_stale_verification_records`
— never silently as success-with-nothing.

---

## Verdicts

| Verdict | Meaning |
|---------|---------|
| `current` | Every cited symbol/file is unchanged since the record's anchor. |
| `stale` | The cited symbol/file changed after the anchor, or the record's `source_artifact_hash` no longer matches the artefact's current content. |
| `unresolved` | The cited handle no longer resolves (symbol removed/renamed or file deleted). |
| `unanchored` | The record carries neither `temporal.git_commit` nor `executed_at` to compare against. |

Verdicts are decided per **citation** (one row per verification record ×
cited handle), in this precedence: `unresolved` (anchor-independent) →
`unanchored` (resolved, but no anchor) → `stale` → `current`. A record with
multiple citations (e.g. both `MENTIONS_SYMBOL` and `TOUCHED_FILE`) gets one
row per citation — never collapsed, so one drifted citation cannot hide
alongside a current one (AC5).

A retracted (tombstoned) verification record is omitted entirely — a
retracted run is no longer current evidence, matching the current-state reads
of the other query paths.

---

## Anchor precedence

1. `temporal.git_commit` (paired with `temporal.valid_time`), when present.
2. Otherwise `executed_at`, when it parses as an RFC 3339 instant.
3. Otherwise `unanchored`.

This is a deliberate design decision for this issue: the verification schema
(`docs/schema/verification.md` §5) documents `git_commit` as the primary
anchor, but the one real trunk writer (`eg capture-tests`, issue #165)
populates only `executed_at` today. `executed_at` must therefore be a usable
anchor on its own, or every real-world `TestRun` would be `unanchored`.

---

## Triggers (reused, never re-derived)

- **drift record** — a `SemanticDrift` record whose `prior_record_id` equals
  the cited record ID and whose `after_valid_time` post-dates the anchor. The
  triggering handle is the drift record ID.
- **content change** — a later code-graph version of the *same* cited record
  ID whose content hash differs from the version current at the anchor
  (reusing the same `content_hash`/`content_differs` signal `eg query
  evidence-freshness` uses). The triggering handle is the later commit plus
  the content hash.
- **artifact hash change** — when the record's `source_artifact_hash` is
  recorded and `--repo-path <DIR>` is supplied, the artefact at
  `source_artifact_path` is re-hashed (BLAKE3) and compared against the
  recorded hash. A mismatch is `stale` **regardless of anchor** — a direct
  hash comparison needs no historical ordering (AC2). **Without
  `--repo-path` this trigger is simply not evaluated** — never assumed to
  match. This is the one trigger that reads outside the loaded store (a
  local, read-only file read, mirroring the `--repo-path` convention used by
  `eg freshness`/`eg query symbol`).

No false staleness from neighbors (AC5): every trigger is matched against the
exact cited record ID, never the containing file blob. A `MENTIONS_SYMBOL`
citation of symbol `A` is never flagged because a sibling symbol `B` in the
same file changed — only `A`'s own drift record or content-version history
can trigger it.

---

## Output shape

```json
{
  "ok": true,
  "stale_only": false,
  "counts": { "current": 3, "stale": 1, "unresolved": 1, "unanchored": 0 },
  "truncated": false,
  "diagnostics": [
    { "code": "freshness_verdicts", "detail": "5 citation(s) evaluated" }
  ],
  "verdicts": [
    {
      "verification_record_id": "verification:v1:...",
      "verification_kind": "test_run",
      "status": "pass",
      "verdict": "stale",
      "freshness_lead": "evidence basis moved since the record's anchor; a reason to re-verify, not proof the recorded result is now wrong",
      "anchor": { "git_commit": "c1", "executed_at": "2026-01-02T00:00:00Z" },
      "cited_handle": {
        "target_record_id": "codegraph:v8:...",
        "repo_relative_path": "src/a.rs",
        "span": { "start_line": 10, "end_line": 20, "start_byte": 0, "end_byte": 100 },
        "anchor_commit": "c1",
        "relation": "MENTIONS_SYMBOL",
        "target_domain": "codegraph"
      },
      "triggering_handle": {
        "kind": "content_change",
        "after_git_commit": "c2",
        "after_valid_time": "2026-01-05T00:00:00Z",
        "content_hash": "blake3:..."
      }
    }
  ]
}
```

`status` is the verification record's own recorded `status`, echoed
verbatim — never rewritten, reinterpreted, or re-stamped by a freshness
verdict (AC4). `freshness_lead` and `triggering_handle` are present only on
`stale`/`unresolved` rows.

Allow-listed output only: record IDs, kinds, statuses, hashes, edge labels,
repo-relative paths, spans, and closed diagnostic codes. Never raw
`stdout_handle`/`stderr_handle`/`summary` text, patch hunks, or protected
raw-artifact payloads (AC10).

---

## Scope, repo, and limit

- `[scope]` (optional positional): a record ID, exact `Symbol`/`File` name,
  or a segment-aware repo-relative path prefix. Filters verdicts to citations
  whose cited handle matches. An unresolved name/id exits 2 (`no_match`); an
  unresolved path exits 2 (`scope_not_found`); a name matching more than one
  live code item exits 1 (`ambiguous_scope`, candidates listed).
- `--repo <selector>`: restrict to one repository in a multi-repo store.
  Unknown/ambiguous selectors exit 1 with candidates listed.
- `--limit <n>` (default 500, max 1000): caps the (already sorted) verdict
  set; exceeding it sets `truncated: true` and pushes a `results_truncated`
  diagnostic naming the true count. `--limit 0` or `--limit` above the max
  exits 1 before touching the store.

---

## Diagnostics (AC7 / AC8)

| Code | Meaning |
|------|---------|
| `no_verification_records_in_store` | The store carries none of the five verification-domain kinds this lane covers. |
| `no_verification_code_citations` | Verification records exist but none cite a code handle (and no `--repo-path` artifact citation applied). |
| `freshness_verdicts` | The default (non-`--stale-only`) result. |
| `no_stale_verification_records` | `--stale-only` found nothing stale. |
| `stale_verification_records_present` | `--stale-only` found at least one stale/unresolved row. |
| `results_truncated` | `--limit` truncated the verdict set (carries the true count). |

Every one of these is reported at **exit 0** — an empty or capability-absent
result is never a silent success and never an error.

---

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Verdicts returned (including the diagnostics above). |
| 1 | Invalid `--limit`, ambiguous `--repo`/scope selector, or `--graph`/`--data-dir` usage error. |
| 2 | Scope handle matches no in-store code item. |

---

## Known limitations

- Liveness (for the `unresolved` verdict) uses a **store-wide** tip-commit
  frontier, not the per-repository scoping `eg query evidence-freshness`
  built up over several follow-up rounds (issues #203/#204/#454/#559). In a
  shared multi-repository store where two repositories' histories happen to
  share a commit SHA, this can under/over-prune; scope with `--repo` to a
  single repository to avoid it.
- Staleness ordering compares recorded valid-time instants, not a
  commit-ancestry DAG walk — a rebase that backdates a descendant commit is
  not detected. A documented gap, not a silent wrong answer.
- `content_hash` is `unknown` when the underlying model/tool did not record
  one; comparisons still work off the symbol's normalized summary/content
  signature, matching `eg query evidence-freshness`'s own content signal.
- The `source_artifact_hash` trigger only runs with `--repo-path`; without
  it, an artifact-only citation contributes nothing rather than a guessed
  `current`.

---

## When to use this vs. the alternatives

- **Re-running `cargo test`/Verus/CI** is the only way to know a pass holds
  *right now* — authoritative, but slow, and impossible for a historical
  snapshot. `verification-freshness` is the cheap triage step that tells you
  *which* recorded passes are worth re-running first.
- **`eg freshness` (issue #82)** compares a whole *store* snapshot against
  the live working tree (fresh/stale_head/stale_dirty) — coarse,
  store-vs-tree, not per-record.
- **`eg query evidence-freshness` (issue #85)** ages *agent-memory*
  observations, a different domain and trust class.
- **`eg query verification-coverage` (issue #109)** answers "is this symbol
  covered by *any* verification evidence at all?" — presence/absence, never
  aging what it finds. `verification-freshness` ages the evidence
  `verification-coverage` finds.
- **`git log -L <range>:<file>`** answers "did these lines change?" with no
  link back to a specific recorded verification run, no trust-class framing,
  and no batch "which of my passes are stale?" answer.

---

## See also

- `docs/schema/verification.md` — the verification-domain schema (node
  kinds, anchor fields, cross-domain edges).
- `docs/cli/evidence-freshness.md` — the agent-memory sibling (issue #85).
- `docs/cli/verification-coverage.md` — presence/absence partition (issue
  #109).
- `docs/cli/freshness.md` — store-vs-working-tree freshness (issue #82).
- `docs/cli/corpus-modes.md` — the corpus-mode contract this lane
  deliberately opts out of, and why.
