# eg query task

Retrieve evidence-backed context for a task starting from a task ID or source handle.

## Synopsis

```text
eg query task <ID_OR_HANDLE> --graph <PATH>
eg query task <ID_OR_HANDLE> --data-dir <DIR> [--daemon]
```

This command accepts either:
- `--graph <PATH>` — read from a JSONL file.
- `--data-dir <DIR>` — read from an embedded AletheiaDB store.

Additionally, when using `--data-dir`, you can specify `--daemon` to query through a running local Egregore daemon.

## Handle Resolution

The `<ID_OR_HANDLE>` argument can be in any of the following supported formats:

1. **Canonical ID**: e.g., `project:v1:72c88f...` (prefix `project:` followed by `v` + version + 64-hex SHA)
2. **GitHub Issue/PR URL**: e.g., `https://github.com/madmax983/egregore/issues/48`
3. **GitHub Issue/PR Short Handle**: e.g., `madmax983/egregore#48`
4. **Local JSONL task handle**: e.g., `path/to/tasks.jsonl:1`

If the handle resolves to multiple distinct task IDs in the database, the command fails with stable exit code `1` and prints an `Ambiguous` JSON error.

If the handle format is unrecognized, the command fails with stable exit code `1` and prints an `Unsupported` JSON error.

## Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Success. Structured JSON printed to stdout. |
| `1` | Unsupported or Ambiguous handle format. Diagnostic JSON printed to stderr. |
| `2` | Task not found / no match. Standard no_match envelope printed to stdout. |

## JSON Output Structure

The output is a structured JSON envelope containing 8 context sections, sorted deterministically by stable ID:

* **`tasks`**: The queried `Task` node(s), including historical versions.
* **`acceptance_criteria`**: `AcceptanceCriterion` nodes owned by the Task. Verified acceptance criteria (status `verified`) will carry/inline their closing verification record under the `verification_record` field.
* **`source_facts`**: Code-graph files or symbols linked to the task.
* **`observations`**: Subjective agent-authored claims, decisions, and failure records referencing the task.
* **`artifacts`**: `Artifact`, `PatchArtifact`, or `FileEdit` nodes linked to the task.
* **`verification_evidence`**: `Verification` or `CommandRun` nodes validating the task or closing its criteria.
* **`external_links`**: `ExternalLink` nodes referencing source issues.
* **`unresolved`**: Missing evidence link targets.

Every **record row** above (including a nested `verification_record`) carries a
`trust_class` (its provenance domain) and a derived `trust` label (issue #114) —
one of `source_derived`, `verification_evidence`, `agent_verified`,
`agent_unverified`, `agent_contradicted`. `unresolved` rows carry neither: they
name a target absent from the store, so there is no record to classify. See
[`docs/schema/trust-labels.md`](../schema/trust-labels.md).

### Example Success Output

```json
{
  "ok": true,
  "task_id": "project:v1:10a45b6...",
  "tasks": [
    {
      "record_id": "project:v1:10a45b6...",
      "kind": "Task",
      "summary": "Task #48 implementation",
      "title": "Implement task evidence query"
    }
  ],
  "acceptance_criteria": [
    {
      "record_id": "project:v1:30bcf9e...",
      "kind": "AcceptanceCriterion",
      "summary": "AC 1: JSON output",
      "status": "verified",
      "verification_record": {
        "record_id": "verification:v1:40fed8a...",
        "kind": "Verification",
        "status": "pass",
        "verification_kind": "command_run",
        "summary": "Verification pass"
      }
    }
  ],
  "source_facts": [
    {
      "record_id": "codegraph:v1:50ef8ad...",
      "kind": "File",
      "repo_relative_path": "src/query.rs"
    }
  ],
  "observations": [],
  "artifacts": [],
  "verification_evidence": [],
  "external_links": [
    {
      "record_id": "codegraph:v1:98abc12...",
      "kind": "ExternalLink",
      "summary": "GitHub Issue #48 Link"
    }
  ],
  "unresolved": []
}
```

### Example Ambiguous Handle Error (exit code 1)

```json
{"Ambiguous":{"handle":"madmax983/egregore#48","candidates":["project:v1:abc...","project:v1:def..."]}}
```

### Example Unsupported Handle Error (exit code 1)

```json
{"Unsupported":{"handle":"some_bad_format","message":"handle format is not recognized. Supported formats: canonical ID, GitHub URL, GitHub short handle (owner/repo#num), local JSONL handle (path.jsonl:local_id)"}}
```

### Example No Match (exit code 2)

```json
{
  "ok": false,
  "error": {
    "code": "no_match",
    "task_id": "https://github.com/madmax983/egregore/issues/999"
  }
}
```
