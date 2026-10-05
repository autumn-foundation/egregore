# Task readiness — `eg query task-ready` (issue #161)

Answers one question: **which tasks are ready to dispatch right now?** A task
is *ready* when every task it declares a dependency on is `closed_completed`.
The lane resolves the `DEPENDS_ON` graph declared by `depends_on` in the
local task JSONL and partitions the open/in-flight tasks into READY and
BLOCKED, naming each unmet dependency with its stable record ID and source
handle.

## Shortest local workflow

```bash
# 1. Declare dependencies in .egregore/tasks/<name>.jsonl
#    {"kind":"task","local_id":"t2","title":"...","status":"open",
#     "depends_on":["t1"], ...}
# 2. Import into a project-graph JSONL file
eg import-local-tasks .egregore/tasks --out project.jsonl
# 3. Resolve readiness
eg query task-ready --graph project.jsonl
```

## Command

```text
eg query task-ready --graph <PATH>    [--format json|text]
eg query task-ready --data-dir <DIR>  [--format json|text]
```

`--format json` (default) is the stable agent contract. `--format text` is a
human-readable rendering of the same report.

## What "ready" means

Readiness derives **only** from author-written `status` and author-declared
`depends_on` edges:

- Candidates are tasks with status `open` or `in_progress`. Terminal tasks
  (`closed_completed`, `closed_dropped`) are never candidates; any other
  status (including author-written `blocked`) is reported separately, never
  as a readiness verdict.
- A dependency is satisfied **only** by `closed_completed`. A
  `closed_dropped` dependency does **not** unblock the dependent — the
  prerequisite was cancelled, not completed — so the dependent stays blocked
  and names the dropped task as its blocker.
- Dependency chains resolve transitively through any number of hops:
  completing a task can make its dependents ready, which in turn unblocks
  theirs. Re-import and re-query after each status change.
- A task with no dependencies is ready as soon as its status is a candidate.

A READY verdict is an **eligibility lead**, not a judgment: "no declared
prerequisite blocks this task." It never claims the task is correct, safe, or
non-colliding. Dispatching parallel work needs eligibility **and**
non-collision — pair this lane with footprint-overlap analysis (issue #150,
`eg query task-overlap`, documented in `task-overlap.md`) before dispatching.
Readiness complements status listing (issue #119, *what* each task's status is)
with dependency resolution (*which* tasks nothing blocks): status +
dependencies establish eligibility; overlap analysis establishes
non-collision; dispatch now requires both. For the full when-to-use
comparison (task-ready vs task-overlap vs conflicts), see
`task-overlap.md`'s "When to use which lane".

## Blocked rows

Every blocked task names its specific unmet dependencies:

- **Resolved blocker**: the target is a live task whose status is not
  `closed_completed`. The row carries the blocker's `record_id`, `title`,
  `status`, and `source_handle`.
- **Unresolved declaration**: the declared local ID matched no task known to
  the importer. The task is still blocked (fail closed), the row carries
  `resolution: "unresolved"` and the declared local ID, and a
  `[unresolved_dependency]` diagnostic names the declaring task, file, line,
  and unknown target. Fix the reference or add the missing task, then
  re-import.
- **Absent target** (defensive): a `DEPENDS_ON` edge whose target has no live
  task record. The local importer never emits this shape.

## Cycles

A dependency cycle (including a self-dependency) is a structural diagnostic,
not a silent exclusion:

- Every member of the cycle is reported blocked with `in_cycle: true` and is
  never ready.
- One `dependency_cycle` diagnostic names the exact member record IDs in
  ascending order (Tarjan SCC — downstream-only nodes are never misreported
  as members).
- The command exits with code **3** and prints a one-line summary to stderr.
  Fix the declared dependencies and re-import.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Valid report. An empty ready set with live tasks is still valid. |
| `1` | Malformed input / read failure (unreadable graph, bad JSONL). |
| `2` | No Task records in the project domain: import tasks first. |
| `3` | Dependency cycle diagnosed (report still printed). |

## JSON shape

```jsonc
{
  "ok": true,
  "lane": "task-ready",
  "trust": "eligibility_lead",
  "disclaimer": "Readiness is prerequisite eligibility only ...",
  "counts": {
    "tasks": 8, "dependency_edges": 5,
    "ready": 3, "blocked": 3, "author_blocked": 1, "diagnostics": 0
  },
  // "source_handle" is the task's ExternalLink rendered as
  // "{system_native_id} <{url}>" (local-file tasks look like
  // "tasks%2Fproj.jsonl:t2 <file://tasks/proj.jsonl>").
  "ready": [
    {
      "record_id": "project:v1:...",
      "title": "Task t1",
      "status": "open",
      "source_handle": "tasks%2Fproj.jsonl:t1 <file://tasks/proj.jsonl>"
    }
  ],
  "blocked": [
    {
      "record_id": "project:v1:...",
      "title": "Task t2",
      "status": "open",
      "source_handle": "tasks%2Fproj.jsonl:t2 <file://tasks/proj.jsonl>",
      "unmet_dependencies": [
        {
          "record_id": "project:v1:...",
          "declared_local_id": null,
          "title": "Task t1",
          "status": "open",
          "source_handle": "tasks%2Fproj.jsonl:t1 <file://tasks/proj.jsonl>",
          "resolution": "resolved"
        }
      ],
      "in_cycle": false
    }
  ],
  "author_blocked": [ { "record_id": "...", "title": "...", "status": "blocked", "source_handle": "..." } ],
  "diagnostics": [
    {
      "code": "dependency_cycle",
      "message": "dependency cycle among tasks: project:v1:..., project:v1:...",
      "members": ["project:v1:...", "project:v1:..."],
      "task_record_id": null,
      "unknown_dep": null
    }
  ]
}
```

`trust: "eligibility_lead"` follows the single-domain lane convention in
`query.md` (cf. `dependency_lead`, `impact_lead`): the rows are a
dispatch-eligibility lead, not a fact. Key order is stable: structs serialize
field-declaration order, rows sort by ascending record ID, and repeated runs
are byte-identical.

For GitHub-backed tasks, `source_handle` resolves through the task's
`EXTERNAL_HANDLE` link to the `ExternalLink` system handle (issue/PR native
ID and URL); for local tasks it is the local-JSONL handle.

## Determinism and read-only operation

The lane is pure: it reads records, computes, and prints. It creates,
modifies, and deletes no files, records, indexes, or runtime state — safe to
run against a live store directory. Repeated runs over the same records are
byte-identical (sorted traversal, stable tie-breaks, no timestamps in
output).

## Declaring dependencies

In the local task JSONL, add the optional `depends_on` array of task
`local_id`s:

```json
{"kind":"task","local_id":"t2","title":"Migrate schema","status":"open",
 "depends_on":["t1"], "priority":"normal","assignees":[],"labels":[],
 "created_at":"2026-09-27T00:00:00Z","updated_at":"2026-09-27T00:00:00Z"}
```

Rules (see `../schema/local-project-jsonl.md`):

- Resolution is file-wide: forward references are allowed.
- Duplicate entries collapse to a single `DEPENDS_ON` edge.
- Only the latest revision's declarations are emitted; a later revision that
  omits a dependency removes it.
- An unknown target keeps the task importable but blocked: an
  `[unresolved_dependency]` diagnostic is emitted and the lane reports the
  task blocked with `resolution: "unresolved"`.
- A self-dependency is imported and surfaces as a cycle diagnostic at query
  time.
