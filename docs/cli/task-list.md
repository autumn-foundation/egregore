# Task listing — `eg query task-list` (issue #119)

Answers one question: **which tasks are in a given status right now?** The
lane resolves every project `Task` to its latest-`transaction_time` version
per stable entity, filters to the requested statuses, and groups the rows by
status. A status row is an **imported-intent lead** — it tells you which work
the source system says is in flight, never proof the code changed, the
acceptance criteria hold, or any verification passed.

## Shortest local workflow

```bash
# 1. Import tasks into a project-graph JSONL file (no GitHub, no daemon,
#    no embeddings required)
eg import-local-tasks .egregore/tasks --out project.jsonl
# 2. List the in-flight work
eg query task-list --graph project.jsonl
# 3. Narrow to one status
eg query task-list --graph project.jsonl --status blocked
```

No network, hosted indexing, crawling, or remote embeddings are involved:
the lane reads the records you hand it and computes. Both `--graph`
(JSONL) and `--data-dir` (embedded store) work; the lane is structural.

## Command

```text
eg query task-list --graph <PATH>    [--format json|text] [--status <FILTER>]
eg query task-list --data-dir <DIR>  [--format json|text] [--status <FILTER>]
```

`--format json` (default) is the stable agent contract. `--format text` is a
human-readable rendering of the same report.

## The status filter

`--status` is a comma-separated list of task statuses, or the `active`
convenience filter. The default (when `--status` is omitted) is `active`.

- `active` (default) = `{open, in_progress, blocked}` — the in-flight set.
- Any member of the closed project task-status vocabulary is eligible:
  `open`, `in_progress`, `blocked`, `closed_completed`, `closed_dropped`,
  `unknown`.
- `active` must stand alone: `active,open` is malformed.
- An unknown name, an empty entry, or a duplicate fails with exit code `3`
  and a stable machine-readable diagnostic (`invalid_status_filter`) — the
  lane never silently returns everything or nothing on a bad filter.

## Bi-temporal status resolution

A task's status can change over time: the store holds multiple rows for one
entity (same `entity_id`, different `transaction_time`). The listing
reflects **only the latest-`transaction_time` row per entity** — the older
row is excluded, never double-counted. Rows that share a record ID resolve
to the latest write (same rule as the other task lanes); tombstoned tasks
resolve to nothing.

## Rows

Every returned task carries, at minimum:

- `record_id` — the stable record ID of the winning row;
- `schema_version` — the schema version that produced the record;
- `status` — the current (latest-`transaction_time`) status;
- `source_kind` — the importer source kind (`github_issue`,
  `local_jsonl`, …);
- `source_handle` — the source-system handle: the GitHub issue/PR number
  plus URL resolved through the task's `ExternalLink` (rendered
  `{system_native_id} <{url}>`, e.g. `#42
  <https://github.com/acme/repo/issues/42>`), or the task's own local-JSONL
  record handle (`<encoded_path>:<local_id>:<blake3>`);
- `title` / `summary` — the human-readable title and agent-facing summary.

Groups follow the closed-vocabulary order
(`open`, `in_progress`, `blocked`, `closed_completed`, `closed_dropped`,
`unknown`); rows within a group are in ascending record-ID order. Repeated
runs over an unchanged store are byte-identical.

## Trust separation

Rows carry **only** importer-produced, redaction-passed fields: titles,
summaries, record IDs, hashes, enums, handles, and counts. The lane never
emits raw issue/PR bodies, comment text, transcript text, command output,
env values, bearer tokens, or protected raw-artifact payloads — bodies live
behind `body_handle` on the node and are never read. `trust:
"status_lead"` follows the single-domain lane convention in `query.md`
(cf. `eligibility_lead`, `inspection_lead`): the rows are leads to
inspect, not facts.

A task's status reflects **imported intent, not proof the code or the
verification matches**: a `closed_completed` row means the source system
recorded it closed — it says nothing about whether the acceptance criteria
were ever verified. For the proof gap, see `criteria-coverage.md`; for
gating a single task on live verification evidence, see
`task-evidence-gate.md`.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Valid report. Zero matches (`groups: []`, `zero_matches: true`) is valid and distinct from an error and from "no project data". |
| `1` | Malformed input / read failure (unreadable graph, bad JSONL). |
| `2` | No `Task` records in the project domain: import tasks first (`no_project_data`). |
| `3` | Malformed `--status` filter (`invalid_status_filter`). |

## JSON shape

```jsonc
{
  "ok": true,
  "lane": "task-list",
  "trust": "status_lead",
  "disclaimer": "A task's status is imported intent ...",
  "status_filter": ["blocked", "in_progress", "open"],
  "counts": { "tasks": 12, "matched": 7 },
  "groups": [
    {
      "status": "open",
      "tasks": [
        {
          "record_id": "project:v1:...",
          "schema_version": 1,
          "status": "open",
          "source_kind": "github_issue",
          "source_handle": "#42 <https://github.com/acme/repo/issues/42>",
          "title": "Implement task listing",
          "summary": "github_issue #42"
        },
        {
          "record_id": "project:v1:...",
          "schema_version": 1,
          "status": "open",
          "source_kind": "local_jsonl",
          "source_handle": "tasks%2Facme.jsonl:t2:9f2c…",
          "title": "Write the docs",
          "summary": "Task: Write the docs"
        }
      ]
    }
  ],
  "zero_matches": false
}
```

A malformed filter prints a stable diagnostic and exits `3`:

```jsonc
{ "ok": false, "error": { "code": "invalid_status_filter", "message": "unknown task status 'bogus': ..." } }
```

## Determinism and read-only operation

The lane is pure: it reads records, computes, and prints. It creates,
modifies, and deletes no files, records, indexes, or task state — safe to
run against a live store directory. Repeated runs over the same records are
byte-identical (sorted traversal, vocabulary-ordered groups, stable
tie-breaks, no timestamps in output).

## When to use task listing vs the alternatives

- **`eg query task-list`** (this lane, issue #119) — *what is in flight*:
  the status-grouped roll call of imported work. Use it at the start of a
  session, in a standup digest, or when triaging: "what is open, what is
  blocked, what just closed."
- **`eg query task <HANDLE>`** (issue #48) — *single-task evidence*: the
  deep, evidence-backed context for one task you already named
  (acceptance criteria, source facts, observations, verification
  evidence). Use it once `task-list` told you *which* task to look at.
- **AC-coverage reporting** (`criteria-coverage.md`, issue #115) — *the
  proof gap*: which acceptance criteria across all imported work are
  actually closed by passing evidence, and which tasks are marked done
  while owning unproven criteria. Use it when a status row says
  `closed_completed` and you need to know whether the closure is earned.
- **Session resume digest** (`agent-sessions.md`, issue #112) — *where did
  I leave off*: the agent-memory view of recent sessions. Use it to resume
  *your* work; use `task-list` to see *the project's* work.
- **`gh issue list`** — the live tracker, not the imported snapshot. Use
  it when you need the tracker's current truth (labels changed a minute
  ago, brand-new issues). Use `task-list` when you need the imported,
  queryable, redacted project graph — offline, deterministic, and joined
  with code evidence.
- **`jq` over local JSONL** — the raw import files. Use it for ad-hoc
  poking at one file's rows. Use `task-list` when you need bi-temporal
  resolution (latest-`transaction_time` wins), closed-vocabulary filter
  validation, canonical ordering, and the GitHub/local handle join —
  none of which a one-liner gets right.
