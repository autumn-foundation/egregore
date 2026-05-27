# ADR 0005: Codex JSONL Field-Stability Tiers

## Status

Accepted

## Context

Codex CLI (codex-cli ≥ 0.1.2450) produces session and rollout JSONL files whose
schema is not formally versioned. Field presence and payload shapes can change
between CLI releases. The importer (`src/codex.rs`) must survive field drift
without silent data loss or panics.

This ADR establishes four tiers of field reliability, documents which fields
fall into each tier, and specifies how the importer handles each tier.

## Decision

### Tier 1 — Required

Fields that **must** be present for a record to be parseable. Missing required
fields cause the enclosing event to be treated as `CodexEvent::Unknown` and
emitted as a `Diagnostic` node rather than silently discarded.

| Event type             | Required fields              |
|------------------------|------------------------------|
| `session`              | `type`                       |
| `rollout`              | `type`                       |
| `message`              | `type`, `role`               |
| `function_call`        | `type`, `call_id`, `name`    |
| `function_call_output` | `type`, `call_id`            |
| `interrupted`          | `type`                       |

### Tier 2 — Expected

Fields that are present in all known Codex CLI releases but may be absent in
future versions or hand-crafted files. Absent expected fields degrade gracefully
to a sentinel value (`None`, `""`, or `"unknown"`); the record is still emitted.

| Field                              | Sentinel |
|------------------------------------|----------|
| `message.content`                  | empty    |
| `message.status`                   | `None`   |
| `function_call.arguments`          | `""`     |
| `function_call_output.output`      | `None`   |
| `session.model`                    | `None`   |
| `session.created_at`               | epoch    |
| `rollout.model`                    | `None`   |
| `rollout.started_at`               | epoch    |

### Tier 3 — Best-Effort

Fields emitted by the CLI when available but deliberately omitted in some runs.
The importer emits dependent records (e.g., `CostUsage`) only when these fields
are present and non-zero.

| Field                        | Dependent record |
|------------------------------|------------------|
| `message.usage.input_tokens` | `CostUsage`      |
| `message.usage.output_tokens`| `CostUsage`      |
| `message.usage.total_tokens` | `CostUsage`      |
| `interrupted.reason`         | `Diagnostic`     |
| `interrupted.at`             | `Diagnostic`     |

### Tier 4 — Opaque

Fields whose payload shape can change without notice. Stored verbatim as a
compact JSON string inside the `text` field of the corresponding record.
Downstream consumers must not parse these fields structurally.

| Field             | Storage location        |
|-------------------|-------------------------|
| `message.usage.*` | `CostUsage.text` (JSON) |

## Importer Behavior Rules

1. **Unknown event kinds** (`#[serde(other)]`): any `type` value not in the
   known set is deserialized as `CodexEvent::Unknown` and emitted as a
   `Diagnostic` node. The importer never panics on unrecognized event kinds.

2. **Session vs rollout auto-detection**: flavor is determined from the `type`
   field of the first event. If neither `session` nor `rollout` appears first,
   `SessionFlavor::None` is used and timestamps default to the Unix epoch.

3. **Verification trust rule**: a `Verification` node is emitted **only** when
   the tool name is `"shell"`, the command matches a known test-command prefix
   (e.g., `pytest`, `cargo test`), and `exit_code == 0`. A non-zero exit code
   produces a `Failure` node instead.

4. **Patch status rule**: `PatchArtifact.patch_status` is set to `"invalid"`
   when `exit_code != 0`; otherwise `"unverified"`.

5. **Idempotency**: the `AgentSession` ID is derived as
   `BLAKE3(["node","agent_session",IMPORTER_ID,IMPORTER_VERSION,artifact_hash])`
   so that re-importing the same file always produces the same session ID.

6. **Redaction hook**: all free-text fields (command strings, stdout, stderr,
   summaries) are passed through the caller-supplied `ImportOptions::redact`
   closure before storage. The hook is applied before any artifact handle is
   computed to avoid leaking pre-redaction content through hashes.

## Consequences

- New Codex event kinds are automatically demoted to `Diagnostic` nodes without
  requiring a code change.
- Token-usage records (`CostUsage`) are stored as opaque JSON; consumers that
  need structured token counts should re-parse the `text` field after confirming
  the format version.
- The field-stability tier of any new field added to the importer must be
  documented by updating this ADR.
