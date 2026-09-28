# Footprint overlap — `eg query task-overlap` (issue #150)

Answers one question: **which in-flight tasks touch the same code?** The lane
resolves each in-flight task's deterministic code footprint — the set of live
`Symbol` / `File` handles it names — and reports every unordered pair of
in-flight tasks that share at least one handle. An overlap row is an
**inspection lead**, never proof of an edit conflict: it tells you where to
look before dispatching parallel work, not what will collide.

## Shortest local workflow

```bash
# 1. Import tasks into a project-graph JSONL file
eg import-local-tasks .egregore/tasks --out project.jsonl
# 2. Scan the repository so Symbol/File handles exist (any eg scan output
#    merged into the same graph), and link sessions to tasks/symbols/files
#    via the usual agent-memory edges (REFERENCES_TASK, TOUCHED_FILE,
#    MENTIONS_SYMBOL)
# 3. Flag overlapping in-flight tasks
eg query task-overlap --graph project.jsonl
```

No network, hosted indexing, crawling, or remote embeddings are involved:
the lane reads the records you hand it and computes.

## Command

```text
eg query task-overlap --graph <PATH>    [--format json|text] [--status <LIST>]
eg query task-overlap --data-dir <DIR>  [--format json|text] [--status <LIST>]
```

`--format json` (default) is the stable agent contract. `--format text` is a
human-readable rendering of the same report. `--status` is a comma-separated
list of in-flight statuses to compare; the default is
`open,in_progress,blocked`.

## What counts as in-flight

Only tasks whose **latest** `transaction_time` status is in the active
filter are compared. The filter defaults to `{open, in_progress, blocked}`
(the #119 vocabulary's in-flight set). `closed_completed`, `closed_dropped`,
`unknown`, and `superseded` never appear in a pair — passing any of them in
`--status` is a malformed filter (exit 3), as is an unknown name, an empty
entry, or a duplicate.

## What a footprint is

A task's footprint is the set of live code-graph handles it reaches through
two legs:

- **direct** — the task's own live `MENTIONS_SYMBOL` (task → symbol) and
  `TOUCHES_FILE` (task → file) edges;
- **session** — any live node with a live `REFERENCES_TASK` edge to the
  task (an agent-memory session working on it), followed forward through
  that node's live `TOUCHED_FILE` / `MENTIONS_SYMBOL` edges.

Only the latest-`transaction_time` version of each node and edge
participates: tombstoned or superseded records resolve to nothing, and a
handle is resolvable only when its `Symbol` / `File` node is live. A task
with no resolvable footprint is reported with a `no_footprint` diagnostic —
it is never silently ignored and never fabricated into a pair.

## Overlap rows

Each row names the two tasks (`record_id`, `title`, `status`, and
`source_handle` — the GitHub issue/PR number and URL via `ExternalLink`, or
the local-JSONL handle) and the shared handles that caused the overlap:
each handle's stable record ID, kind (`Symbol`/`File`), repo-relative path,
span for symbols (rendered `start_line:start_col-end_line:end_col`), and the
footprint legs that resolved it (`direct`, `session`, or both).

Pairs are in canonical ascending `(task_a, task_b)` record-ID order; shared
handles are ascending by record ID. Repeated runs over the same records are
byte-identical.

## Trust separation

Footprints derive **only** from deterministic code-graph handles and
existing project/agent-memory edges. No agent observation is promoted to a
code fact, and the lane emits no raw bodies, transcripts, command output,
patch hunks, env values, or raw artifact payloads — only IDs, handles,
paths, spans, counts, and redaction markers. `trust: "inspection_lead"`
follows the single-domain lane convention in `query.md` (cf.
`eligibility_lead`, `impact_lead`): the rows are leads to inspect, not
facts.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Valid report. Zero overlaps (`pairs: []`, `zero_overlaps: true`) is valid and distinct from an error. |
| `1` | Malformed input / read failure (unreadable graph, bad JSONL). |
| `2` | No `Task` records in the project domain: import tasks first (`no_project_data`). |
| `3` | Malformed `--status` filter (`invalid_status_filter`). |

## JSON shape

```jsonc
{
  "ok": true,
  "lane": "task-overlap",
  "trust": "inspection_lead",
  "disclaimer": "An overlap row is an inspection lead ...",
  "status_filter": ["blocked", "in_progress", "open"],
  "counts": {
    "tasks": 8, "in_flight_tasks": 5, "pairs": 3,
    "no_footprint_tasks": 1, "diagnostics": 1
  },
  // "source_handle" is the task's ExternalLink rendered as
  // "{system_native_id} <{url}>" (local-file tasks look like
  // "tasks%2Fproj.jsonl:t2 <file://tasks/proj.jsonl>").
  "pairs": [
    {
      "task_a": {
        "record_id": "project:v1:...",
        "title": "Task t1",
        "status": "open",
        "source_handle": "#42 <https://github.com/acme/repo/issues/42>"
      },
      "task_b": {
        "record_id": "project:v1:...",
        "title": "Task t2",
        "status": "in_progress",
        "source_handle": "tasks%2Fproj.jsonl:t2 <file://tasks/proj.jsonl>"
      },
      "shared_handles": [
        {
          "record_id": "codegraph:v1:...",
          "kind": "File",
          "repo_relative_path": "src/wire.rs",
          "span": null,
          "via": ["session"]
        },
        {
          "record_id": "codegraph:v1:...",
          "kind": "Symbol",
          "repo_relative_path": "src/wire.rs",
          "span": "10:4-14:1",
          "via": ["direct"]
        }
      ]
    }
  ],
  "diagnostics": [
    {
      "code": "no_footprint",
      "message": "task 'project:v1:...' is in-flight but has no resolvable footprint: ...",
      "task_record_id": "project:v1:..."
    }
  ],
  "zero_overlaps": false
}
```

## Determinism and read-only operation

The lane is pure: it reads records, computes, and prints. It creates,
modifies, and deletes no files, records, indexes, or runtime state — safe to
run against a live store directory. Repeated runs over the same records are
byte-identical (sorted traversal, stable tie-breaks, no timestamps in
output).

## When to use which lane

Three lanes answer three different dispatch questions; they compose into the
dispatch gate:

- **`eg query task-ready`** (issue #161) — *eligibility*: which tasks have
  every declared dependency `closed_completed` and nothing blocking them.
  Use it to find what *can* start. It says nothing about whether two
  eligible tasks step on each other.
- **`eg query task-overlap`** (issue #150, this lane) — *non-collision*:
  which in-flight tasks name the same code handles. Use it right before
  dispatching parallel work, on the eligible set: an overlap row is the
  lead to inspect (same file, same symbol) before two agents edit at once.
  Dispatch now requires eligible **and** non-overlapping.
- **`eg query conflicts`** — *recorded disagreement*: where beliefs,
  observations, or verification records about the same code entity
  contradict each other. Use it when you need the adjudication lead on what
  is true about a piece of code, not which tasks touch it.

In short: `task-ready` says what may start, `task-overlap` says what may
start *together*, `conflicts` says what is disputed about the code they
would touch.
