# Task completion evidence gate — `eg query task-evidence-gate` (issue #147)

Answers one question: **does this task have live passing verification
evidence for every acceptance criterion?** The lane resolves one task,
walks its linked `AcceptanceCriterion` records, and checks each one against
the verification records that close it. A criterion is satisfied only by a
**passing** verification record whose cited code has **not drifted** since
the run. The task is `ready` when every criterion is satisfied — a
completion-gating lead, never a claim the task is correct, safe, or
complete.

## Shortest local workflow

```bash
# 1. Import tasks (with acceptance criteria) and verification records
eg import-local-tasks .egregore/tasks --out project.jsonl
# 2. Link verification records to criteria (CLOSES_ACCEPTANCE_CRITERION or
#    the criterion's verification_link_id), and cite code handles from the
#    verification records (TOUCHED_FILE / MENTIONS_SYMBOL), in the usual way
# 3. Gate one task on its evidence
eg query task-evidence-gate --graph project.jsonl <TASK_ID_OR_HANDLE>
```

No network, hosted indexing, crawling, or remote embeddings are involved:
the lane reads the records you hand it and computes.

## Command

```text
eg query task-evidence-gate --graph <PATH>    <TASK_ID_OR_HANDLE> [--format json|text]
eg query task-evidence-gate --data-dir <DIR>  <TASK_ID_OR_HANDLE> [--format json|text]
```

`--format json` (default) is the stable agent contract. `--format text` is a
human-readable rendering of the same report.

## What counts as evidence for a criterion

Only the **latest live version** of each record participates: tombstoned or
superseded records resolve to nothing.

A verification record counts as evidence for a criterion when it is linked
through either of the two supported shapes:

- a live `CLOSES_ACCEPTANCE_CRITERION` edge (criterion → verification), or
- the criterion's own `verification_link_id` field.

Any other relation that merely happens to point at a verification record
(e.g. `RELATES_TO`) does not count.

## The gate rule

For each linked `AcceptanceCriterion` (ordered by `ordinal`, then record
ID):

1. **Missing evidence** — no live verification record is linked to the
   criterion → `satisfied: false`, `not_ready_reason: "missing_evidence"`.
2. **Failing evidence** — at least one linked record's recorded outcome is
   not passing (the shared `verification_outcome` rule: `pass`/`passed`/
   `success`, or exit code `0`) → `satisfied: false`,
   `not_ready_reason: "failing_evidence"`. A recorded failure dominates:
   it is hard evidence against completion even when other linked records
   pass.
3. **Passing but drifted** — every linked record passes, but none has a
   confirmed-current code basis: its cited code moved since the run
   (`stale`), was removed or renamed (`unresolved`), or cannot be anchored
   (`unanchored`, issue #111 fail-closed) → `satisfied: false`,
   `not_ready_reason: "stale_evidence"`.
4. **Satisfied** — at least one linked record passes **and** its cited code
   has not drifted (`current`) → `satisfied: true`.

The task is `ready: true` only when **every** criterion is satisfied. A
task with no acceptance criteria is vacuously `ready: true`.

## Drift, unanchored citations, and the freshness lead

The lane reuses the issue #111 freshness signal computed over the same
records: per citation the lane reports the `#111` verdict (`current` /
`stale` / `unresolved` / `unanchored`) plus the target record ID,
repo-relative path, and span. A record's freshness is the worst of its
citations; a record citing no code is `current` by vacuity. Stale and
unresolved verdicts carry the standard freshness-lead text pointing at the
handle that moved.

Fail-closed: a passing record whose cited code cannot be confirmed current
— drifted, removed/renamed, or citing no live code handle at all — does
**not** satisfy a criterion. Unanchored passing evidence therefore reads
`stale_evidence`, not `ready`.

## Exit codes

- `0` — a verdict was produced (both `ready: true` and `ready: false`).
  `ready: false` is a verdict, not an error.
- `2` — `no_match`: no live `Task` record for the given ID or handle.

## Output contract

`--format json` emits a stable envelope: `ok`, `lane: "task-evidence-gate"`,
`trust: "evidence_gate"`, `disclaimer`, `task` (record ID, title, status),
`ready`, per-criterion rows (record ID, ordinal, text, `satisfied`,
`not_ready_reason`, and the evidence items with record ID, kind, recorded
status, `pass`, freshness, citations), and `counts`. `--format text` is a
human rendering of the same verdict — parse the JSON, not the text.

## When to use which lane

Four lanes answer four different dispatch questions; they compose into the
dispatch gate:

- **`eg query task-ready`** (issue #161) — *eligibility*: which tasks have
  every declared dependency `closed_completed` and nothing blocking them.
  Use it to find what *can* start. It says nothing about whether the work
  is evidenced.
- **`eg query task-overlap`** (issue #150) — *non-collision*: which
  in-flight tasks name the same code handles. Use it right before
  dispatching parallel work: an overlap row is the lead to inspect before
  two agents edit at once.
- **`eg query task-evidence-gate`** (issue #147, this lane) — *completion*:
  which tasks have live passing verification evidence for every acceptance
  criterion. Use it when deciding whether a task may **close** or its
  completion claim may be trusted. It says nothing about whether the work
  is correct — only that the recorded evidence for it is live.
- **`eg query conflicts`** — *recorded disagreement*: where beliefs,
  observations, or verification records about the same code entity
  contradict each other. Use it when you need the adjudication lead on what
  is true about a piece of code.

In short: `task-ready` says what may start, `task-overlap` says what may
start *together*, `task-evidence-gate` says what may *close*, `conflicts`
says what is disputed about the code they would touch.
