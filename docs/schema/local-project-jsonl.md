# Local Project/Task JSONL File Format - v1

**Status:** Active. This document is the single source of truth for the
on-disk local project/task JSONL file format that feeds
`source_kind: local_jsonl` records in
[`docs/schema/project-graph.md`](project-graph.md).

**File-format version:** `schema_version` = `1`. This is the local-JSONL
file-format version, not the project-graph in-graph schema version. The two
evolve independently.

This document intentionally ships before any writer or importer. The first
writer that appends to `.egregore/tasks/*.jsonl` must follow this file shape;
the M9 importer consumes it but is not implemented here.

Coordination: [`docs/schema/import-github.md`](import-github.md) owns the
GitHub-side fetch policy. The two importers share the same target record shapes
defined in [`docs/schema/project-graph.md`](project-graph.md); this document
owns only the local-JSONL file format.

## File Path Convention

Project/task JSONL files live under `.egregore/tasks/` relative to the
repository root. The day-one shape is one file per project:

```text
.egregore/tasks/<project-slug>.jsonl
```

Rule: **the path is the project identity**. The project slug in the path is the
project identity for local files, and the header `project_slug` must match the
filename stem. An importer that sees two canonical files for the same project
slug must reject the duplicate file with a `duplicate_project_slug` diagnostic.

Only the canonical `.jsonl` file participates in import. Tempfiles created by
the atomic-write protocol are ignored by importers.

## Line Shape

The file is newline-delimited JSON: one JSON object per line. Each line contains
exactly one of these records:

| `kind` | Meaning |
|--------|---------|
| `header` | File metadata and local file-format version. |
| `task` | One project task state row. |
| `acceptance_criterion` | One acceptance criterion attached to an earlier task row. |
| `external_link` | One source-system or local-file handle attached to a task or AC. |

The line discriminator is the top-level `kind` field.

Rule: an importer that encounters an unknown `kind` MUST emit a `Diagnostic` and skip the line, not fail the file.

Reserved future `kind` values:

| Reserved `kind` | Intended future owner |
|-----------------|-----------------------|
| `product` | Product-domain metadata once product records are importable from local files. |
| `project` | Project metadata beyond path identity. |
| `plan` | Plans or milestones attached to a project. |
| `review` | Review comments, findings, approvals, and requested changes. |
| `local_task` | A richer local-only task payload if `task` stops being sufficient. |

Writers must not use reserved values until the corresponding format slice
promotes them to defined values.

## Header Record

The first line of every file must be a header:

```json
{"kind": "header", "schema_version": 1, "project_slug": "<slug>", "created_at": "<rfc3339>"}
```

Fields:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `kind` | string | yes | Always `header`. |
| `schema_version` | integer | yes | Local-JSONL file-format version. Must be `1` for this document. |
| `project_slug` | string | yes | Must equal `<project-slug>` from `.egregore/tasks/<project-slug>.jsonl`. |
| `created_at` | RFC3339 string | yes | File creation timestamp from the writer/operator. |

Rule: the first line MUST be a header; importers MUST reject files without one
with a `missing_header` diagnostic.

## Task Record

Task lines define project work items:

```json
{
  "kind": "task",
  "local_id": "issue-17",
  "title": "Spec local project/task JSONL on-disk file format before any writer",
  "body": "Short inline task body",
  "status": "open",
  "priority": "high",
  "assignees": ["mark"],
  "labels": ["spec", "pm"],
  "created_at": "2026-05-18T08:18:02Z",
  "updated_at": "2026-05-18T08:41:56Z"
}
```

Field set:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `kind` | string | yes | Always `task`. |
| `local_id` | string | yes | Local-JSONL record identifier. Required and used in the local source handle. The first occurrence defines the record; later lines with the same `local_id` are revisions of that record. |
| `title` | string | yes | Redacted per issue #4 at import time before persistence into the graph. |
| `body` | string or body handle | no | Inline string for bodies up to 16 KiB, or `{"handle": "<repo-relative-path>", "hash": "<blake3>", "bytes": <u64>}` for larger bodies. In handle form, `bytes` is required and is the byte length of the referenced body content; a missing `bytes` value emits `missing_body_bytes` and skips the line. |
| `status` | enum | yes | `open`, `in_progress`, `blocked`, `closed_completed`, `closed_dropped`, `unknown`. Additive and aligned with `Task.status` in project-graph v1. |
| `priority` | enum | yes | `low`, `normal`, `high`, `urgent`, `unknown`. |
| `assignees` | string array | yes | Opaque human or agent identifiers. |
| `labels` | string array | yes | Operator labels. |
| `created_at` | RFC3339 string | yes | Original creation timestamp for this task. |
| `updated_at` | RFC3339 string | yes | Mutation timestamp for this line. |

Rule: `local_id` is the stable identifier; duplicate `local_id` lines are valid only as revisions of the same record kind. The importer sets project-graph `valid_time` to this line's `updated_at` and sets `valid_time_source` to `local_jsonl_updated_at`.

Rule: omitted `task.body` projects as the canonical empty body: `inline = ""`,
hash = the BLAKE3 hash of the empty byte string, and `bytes` = 0.

## Acceptance Criterion Record

Acceptance criteria attach falsifiable requirements to a task:

```json
{
  "kind": "acceptance_criterion",
  "local_id": "issue-17-ac-1",
  "parent_task_local_id": "issue-17",
  "ordinal": 1,
  "text": "A new doc docs/schema/local-project-jsonl.md exists.",
  "status": "unverified",
  "verification_handle": {"system": "manual", "id": "future-import-test"},
  "updated_at": "2026-05-18T08:41:56Z"
}
```

Field set:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `kind` | string | yes | Always `acceptance_criterion`. |
| `local_id` | string | yes | Local-JSONL record identifier. Required. Later lines with the same `local_id` are revisions of the same AC; reuse across record kinds is invalid. |
| `parent_task_local_id` | string | yes | Must match a `task.local_id` earlier in the same file. |
| `ordinal` | u32 | yes | Position within the parent's AC list. |
| `text` | string | yes | Redacted per issue #4 at import time before persistence into the graph. |
| `status` | enum | yes | `unverified`, `verified`, `failed`, `superseded`, `unknown`. Additive and aligned with `AcceptanceCriterion.status` in project-graph v1. |
| `verification_handle` | object | no | `{"system": "<verifier>", "id": "<external-id>"}` for the verifier that closed the AC. Required when `status` is `verified`; optional otherwise. |
| `updated_at` | RFC3339 string | yes | Mutation timestamp for this AC line; this is `acceptance_criterion.updated_at`. |

When `verification_handle` is present, the importer is responsible for resolving
it to a verification record from issue #11 at import time.

Rule: verified `acceptance_criterion` lines without `verification_handle` are skipped with an
`acceptance_criterion_missing_verification` diagnostic. The importer MUST NOT
write a `verified` acceptance criterion without a non-null `verification_link_id`.

Rule: verified `acceptance_criterion` lines with unresolved `verification_handle` are skipped with an
`acceptance_criterion_missing_verification` diagnostic. The importer MUST NOT
write a `verified` acceptance criterion with a null `verification_link_id`.

Rule: non-verified `acceptance_criterion` lines with unresolved
`verification_handle` are imported with a null `verification_link_id` and emit
an `unresolved_verification_handle` diagnostic.

Rule: an `acceptance_criterion` line whose `parent_task_local_id` does not refer
to a `task` line earlier in the same file is rejected by the importer with
`unresolved_parent_task`. This is a `Diagnostic`, not a hard error; the line is
skipped and a `Diagnostic` record is emitted.

## External Link Record

External links attach source handles to a task or acceptance criterion:

```json
{
  "kind": "external_link",
  "local_id": "issue-17-github",
  "parent_local_id": "issue-17",
  "system": "github",
  "url": "https://github.com/madmax983/egregore/issues/17",
  "system_native_id": "madmax983/egregore#17",
  "discovered_at": "2026-05-18T08:41:56Z",
  "updated_at": "2026-05-18T08:41:56Z"
}
```

Field set:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `kind` | string | yes | Always `external_link`. |
| `local_id` | string | yes | Local-JSONL record identifier. Required. Later lines with the same `local_id` are revisions of the same external link; reuse across record kinds is invalid. |
| `parent_local_id` | string | yes | The `task.local_id` or `acceptance_criterion.local_id` this link belongs to. |
| `system` | enum | yes | `github`, `gitlab`, `local_file`, `harness_legacy`, `other`. Additive and aligned with `ExternalLink.system` in project-graph v1. |
| `url` | string | yes | Canonical URL, `file://` URL, or local path string when no URL exists. |
| `system_native_id` | string | yes | Source-system identifier. For the materialized local source link identity, this is `<encoded_file_path>:<encoded_local_id>` with `file_path` repo-relative. |
| `discovered_at` | RFC3339 string | yes | When the link was discovered or authored. |
| `updated_at` | RFC3339 string | yes | Mutation timestamp for this link line; this is `external_link.updated_at`. |

Rule: the importer MUST materialize one source `ExternalLink` for every `task`.
The materialized source link uses `system = "local_file"`, `parent_local_id`
equal to the task's `local_id`, `url = "file://<file_path>"`, and
`system_native_id = "<encoded_file_path>:<encoded_local_id>"`. This link is the
mandatory `Task.source_external_link_id` / `source_external_link_id` target in
the project graph.

Rule: Explicit `external_link` rows are optional only for additional, non-source links;
their absence does not block import because the source `ExternalLink` is
materialized deterministically. Their presence produces one
`project.ExternalLink` row per line in the graph unless the line has the same
`system` and `system_native_id` as the materialized source link, in which case
it refines that source link rather than creating a duplicate identity.

Rule: explicit source-link refinement field precedence is deterministic. When
an explicit `external_link` row refines the materialized local source link, the
explicit `external_link` row wins for `url`, `discovered_at`, and `updated_at`.
The materialized source link retains identity and task parent wiring:
`system`, `system_native_id`, and the owning task linkage remain the source link
identity for `Task.source_external_link_id`.

Rule: an `external_link` line whose `parent_local_id` does not refer to a
`task` or `acceptance_criterion` line earlier in the same file is rejected by
the importer with `unresolved_parent_local_id`. This is a `Diagnostic`, not a
hard error; the line is skipped and a `Diagnostic` record is emitted.

## Source Handle Encoding

The project-graph `source_handle` uses `<encoded_file_path>:<encoded_local_id>:<record_hash>`.
The `record_hash` is the BLAKE3 hash of the canonical source line bytes. The
hash component is provenance for the concrete source row, not the stable task
identity.

The materialized local source link identity uses the hashless
`<encoded_file_path>:<encoded_local_id>` form. The historical
`<file_path>:<local_id>` spelling is display shorthand only when neither
component needs escaping.

Encoding rules:

1. Normalize `file_path` to the repo-relative path used for import.
2. Percent-encode each UTF-8 component with uppercase hex escapes before
   joining encoded components with the single `:` separator.
3. ASCII alphanumeric plus `-`, `.`, `_`, and `~` are the only unescaped bytes.
   Every other UTF-8 byte MUST be percent-encoded.
4. A literal `:` MUST be percent-encoded as `%3A`; a literal `%` MUST be
   percent-encoded as `%25`; a literal `/` MUST be percent-encoded as `%2F`.

Importers MUST reject malformed percent escapes or non-canonical lowercase
escapes with `source_handle_encoding_error`. This keeps the mapping from
`(file_path, local_id)` to source handle injective.

## Mutation Model

Local JSONL files are append-oriented. Edits to an existing task,
acceptance_criterion, or external_link append a new line with the same
`local_id` and a later `updated_at`. In other words, edits to a task,
acceptance_criterion, or external_link append a revision line.

Rule: task, acceptance_criterion, or external_link records all use the same
append-revision mutation model.

Rule: identity fields MUST NOT change across revisions with the same `local_id`.
`AcceptanceCriterion` identity fields are `parent_task_local_id` and `ordinal`.
`ExternalLink` identity fields are `system` and `system_native_id`. A same-kind,
same-`local_id` revision that changes any identity field is rejected with a
`revision_identity_mismatch` diagnostic and skipped. Non-identity field changes
are valid revisions.

Rule: latest line for a given `local_id` wins by file order. file order is the only current-state winner rule; `updated_at` sets graph `valid_time` but does not choose the winner. An importer that sees `updated_at` move backward for the same `local_id` SHOULD emit a `non_monotonic_updated_at` diagnostic while still using file order for current state.

Rule: this is the on-disk equivalent of issue #14's append-with-same-entity-id
model; the importer produces one new project-graph row per appended line, with
`transaction_time` set to import time and `valid_time` set to the line's `updated_at`.

Moving a task between projects is not a rewrite. Write a `closed_dropped` task
line in the old file and a new `open` task line in the new file. Because path
identity changes, the in-graph ID changes by design.

## Atomic Write Protocol

Writers MUST write a tempfile sibling of the target, fsync it, rename it over
the target, then fsync the containing directory:

```text
.egregore/tasks/<project>.jsonl.tmp-<uuid>
```

Required sequence:

1. Write the complete candidate file to the sibling tempfile.
2. `fsync` the tempfile.
3. Rename the tempfile over `.egregore/tasks/<project>.jsonl`.
4. `fsync` the containing directory so the rename's directory entry is durable.

Rule: an importer encountering a `.tmp-*` leftover from a crashed writer SHOULD
ignore it and import the canonical file. A future operator-tooling slice may
implement a `.tmp-*` reaper; that is not part of v1.

## Redaction Posture

Local files are operator-authored and are NOT redacted at rest by Egregore.

The redaction rule from issue #4 applies at import time. These local fields pass
through the redaction policy before becoming queryable graph fields:

| Local field | Project-graph projection |
|-------------|--------------------------|
| `task.title` | `Task.title` |
| `task.body` inline form | `Task.body_handle.inline` |
| `task.labels` | `Task.labels` |
| `task.assignees` | `Task.assignees` |
| `acceptance_criterion.text` | `AcceptanceCriterion.text` |
| `external_link.url` | `ExternalLink.url` |

Rule: the local file is the source of truth; Egregore's graph view is a
redacted, indexed projection of it. The operator is responsible for not
committing secrets to local task files, the same posture as `.env`.

The local-file source identity handle `<encoded_file_path>:<encoded_local_id>`
and project-graph `source_handle`
`<encoded_file_path>:<encoded_local_id>:<record_hash>` are not redacted by
default. In examples without escaped characters the identity handle is rendered
as `<file_path>:<local_id>`.

## Stable ID Composition

Local-JSONL-sourced project-graph rows use the canonical project-graph stable ID
composition from [`docs/schema/project-graph.md`](project-graph.md):

```text
id = project:v<schema_version>:<blake3(domain || kind || source_kind || source_native_id || entity_kind_identity)>
```

Inputs:

| Input | Value |
|-------|-------|
| `domain` | `project` |
| `kind` | `Task`, `AcceptanceCriterion`, or `ExternalLink` |
| `source_kind` | `local_jsonl` |
| `source_native_id` | Repo-relative `.egregore/tasks/<project-slug>.jsonl` |

Kind-specific `entity_kind_identity` values:

| Kind | `entity_kind_identity` |
|------|------------------------|
| `Task` | `<encoded_file_path>:<encoded_local_id>` |
| `AcceptanceCriterion` | `(parent_task_id, ordinal)` |
| `ExternalLink` | `(system, system_native_id)` |

Rule: renaming a local JSONL file produces new in-graph IDs; moving a task to a
new project is a deliberate identity break. The operator mechanism is a
`closed_dropped` line in the old file plus a new `open` line in the new file.

For `ExternalLink.system_native_id` that represents the local JSONL source
itself, use `<encoded_file_path>:<encoded_local_id>`. For the shared
project-graph `source_handle` field on local-JSONL rows, use
`<encoded_file_path>:<encoded_local_id>:<record_hash>`.

## Idempotent Re-Import Contract

Rule: idempotent re-import is required for every unchanged local JSONL file.

Re-running the importer over an unchanged set of local JSONL files produces the
same project-graph IDs and zero net new graph rows. Re-running after an
append-only edit produces exactly one new row per appended line.

The named test fixture for the importer slice is `.egregore/tasks/sample.jsonl`.
It contains a header, one task, two acceptance criteria, and one external link.
The fixture is imported twice, asserting:

1. byte-equal graph output on the second run.
2. One new row per appended line after an edit.

Minimal fixture:

```jsonl
{"kind": "header", "schema_version": 1, "project_slug": "sample", "created_at": "2026-05-18T00:00:00Z"}
{"kind": "task", "local_id": "sample-task", "title": "Wire local JSONL import", "body": "Make project state editable offline.", "status": "open", "priority": "normal", "assignees": [], "labels": ["local-jsonl"], "created_at": "2026-05-18T00:00:00Z", "updated_at": "2026-05-18T00:00:00Z"}
{"kind": "acceptance_criterion", "local_id": "sample-task-ac-1", "parent_task_local_id": "sample-task", "ordinal": 1, "text": "The importer reads the header.", "status": "unverified", "updated_at": "2026-05-18T00:00:00Z"}
{"kind": "acceptance_criterion", "local_id": "sample-task-ac-2", "parent_task_local_id": "sample-task", "ordinal": 2, "text": "Re-import is idempotent.", "status": "unverified", "updated_at": "2026-05-18T00:00:00Z"}
{"kind": "external_link", "local_id": "sample-task-source", "parent_local_id": "sample-task", "system": "local_file", "url": "file://.egregore/tasks/sample.jsonl", "system_native_id": ".egregore%2Ftasks%2Fsample.jsonl:sample-task", "discovered_at": "2026-05-18T00:00:00Z", "updated_at": "2026-05-18T00:00:00Z"}
```

## Inspect Surface Contract

A future `eg inspect-tasks .egregore/tasks/`, or an extension of `eg inspect`,
reports per-file totals:

| Count | Meaning |
|-------|---------|
| `task` lines | Number of task records parsed. |
| `acceptance_criterion` lines | Number of AC records parsed. |
| `external_link` lines | Number of external-link records parsed. |
| `Diagnostic` records | Parse-time diagnostics emitted for skipped lines/files. |

This document names the surface contract. The implementation lives in the M9
importer slice.

## Versioning rules

`schema_version` = `1` is reserved for the format defined in this document.

Additive changes keep `schema_version` = `1`:

- New optional fields on existing kinds.
- New status enum values.
- New `system` enum values.
- Promoting a reserved `kind` to defined.

Breaking changes require `schema_version` = `2` and an importer that accepts
both versions for one release cycle:

- Renaming or removing fields.
- Changing the header-required rule.
- Changing the mutation model.
- Changing the stable-ID composition.

Unknown fields on known `kind` values are additive and should be ignored by v1
importers unless a future field explicitly changes validation behavior.
