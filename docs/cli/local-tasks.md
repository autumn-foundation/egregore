# Local Task Import — `eg import-local-tasks`

Import local project/task JSONL files into project-graph JSONL so that task
intent and acceptance criteria become queryable evidence without requiring
GitHub, Harness, or a daemon.

## Shortest Local Workflow

```sh
# 1. Write or edit tasks in .egregore/tasks/sample.jsonl
# 2. Import into a project-graph JSONL file
eg import-local-tasks .egregore/tasks --out project.jsonl

# 3. Verify the imported record counts
eg inspect project.jsonl
```

`eg inspect` reports `records:`, `nodes:`, and per-kind counts. Look for
`Task`, `AcceptanceCriterion`, and `ExternalLink` in the output to verify
the import succeeded.

## Command

```
eg import-local-tasks <tasks-path> --out <output.jsonl> [options]
```

| Option | Default | Purpose |
|--------|---------|---------|
| `--out <path>` | required | Output JSONL path |
| `--repo-root <path>` | current directory | Anchors repo-relative source handles |
| `--transaction-time <rfc3339>` | current wall-clock instant | Pin for deterministic/test output |

`<tasks-path>` can be a directory (all `*.jsonl` files are imported in sorted
order) or a single `.jsonl` file.

## Task File Format

Task files live under `.egregore/tasks/<project-slug>.jsonl`. The slug in
the path must match the `project_slug` field in the file header. See
`docs/schema/local-project-jsonl.md` for the full format specification.

Minimal file:

```jsonl
{"kind":"header","schema_version":1,"project_slug":"my-project","created_at":"2026-05-18T00:00:00Z"}
{"kind":"task","local_id":"my-task","title":"Implement feature X","body":"Details here.","status":"open","priority":"normal","assignees":[],"labels":[],"created_at":"2026-05-18T00:00:00Z","updated_at":"2026-05-18T00:00:00Z"}
{"kind":"acceptance_criterion","local_id":"my-task-ac-1","parent_task_local_id":"my-task","ordinal":1,"text":"Feature X passes all unit tests.","status":"unverified","updated_at":"2026-05-18T00:00:00Z"}
```

## Idempotency

Re-importing an unchanged set of task files produces byte-for-byte identical
output after canonical ordering. Appending a revision line to a task
produces exactly one additional graph record for that revision.

## Diagnostics

Invalid lines produce `Diagnostic` records in the output rather than hard
errors. Valid lines in a partially invalid file are still imported. Diagnostic
summaries include a `[code]` prefix:

| Code | Cause |
|------|-------|
| `missing_header` | First line is not a header |
| `project_slug_mismatch` | `project_slug` does not match the filename stem |
| `duplicate_project_slug` | Two files resolve to the same project slug |
| `invalid_json` | A line is not valid JSON |
| `unresolved_parent_task` | AC references a task not found in the same file |
| `unresolved_parent_local_id` | ExternalLink references an unknown parent |
| `duplicate_local_id_kind_mismatch` | Same `local_id` used for different record kinds |
| `unknown_kind` | A line has an unrecognized `kind` value |
| `non_monotonic_updated_at` | A revision's `updated_at` is earlier than the previous revision's; record is still imported |
| `acceptance_criterion_missing_verification` | Verified AC has no resolvable verification handle; imported with status downgraded to `unverified` |

## Ingest into the Embedded Store

After importing, ingest the output into the embedded `AletheiaDB` store for
persistent, queryable storage:

```sh
eg import-local-tasks .egregore/tasks --out project.jsonl
eg ingest project.jsonl --adapter embedded --data-dir .egregore
```

## Verifying Imported Task Counts

Use `eg inspect` immediately after import:

```sh
eg inspect project.jsonl
```

The output lines `schema_version 1: N` count all project-schema v1 records.
Each task produces one `Task` node, one materialized source `ExternalLink`,
and an `ExternalHandle` edge. Each acceptance criterion produces one
`AcceptanceCriterion` node and an `OwnedByTask` edge. Explicit
`external_link` lines produce additional `ExternalLink` nodes.
