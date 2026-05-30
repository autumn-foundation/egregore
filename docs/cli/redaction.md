# Redaction Policy — CLI Reference

**Status:** Active at v1. Enforces `docs/schema/redaction.md` at the write
boundary for agent-memory, artifact, verification, project, and user-context
graph records. Code-graph records are exempt by design.

## What `redaction_required` means

When Egregore rejects a write with `redaction_required`, the error names the
field path (e.g. `text`, `validation_summary`, `stdout_handle.inline`) that
contains raw secret material. The error never echoes the raw secret value.

The field must be replaced with a redaction marker before the record can be
persisted:

```text
<REDACTED:secret_class:hash_prefix>
```

where `secret_class` is one of: `api_token`, `ssh_private_key`,
`database_url`, `cloud_credential`, `webhook_secret`, `session_cookie`,
`env_secret`; and `hash_prefix` is the first 12 hex characters of the
BLAKE3 hash of the original value (safe to store, allows audit correlation).

After redacting, set `redaction_policy_version: "v1"` on the record.

## Commands that apply the redaction gate

### `import-traj`

Applies the v1 redaction policy to all free-text fields before emitting graph
records. The default `ImportOptions` uses `redaction::redact_value` as the
closure. Pass-through (no redaction) requires an explicit `ImportOptions::passthrough()`
— only permitted for dry-run or test invocations.

```powershell
cargo run -- import-traj trajectory.traj --out records.jsonl
```

### `import-codex`

Same as `import-traj`: the default `ImportOptions` applies the v1 policy to
command arguments, stdout/stderr excerpts, and assistant prose.

```powershell
cargo run -- import-codex session.jsonl --out records.jsonl
```

### Programmatic write path

Any code that constructs non-code-graph records and calls `ingest_records` or
writes to a `GraphSink` should call `redaction::validate_record` before
submission and call `redaction::redact_value` on sensitive fields:

```rust
use egregore::redaction::{redact_value, validate_record, REDACTION_POLICY_VERSION};

let clean_value = redact_value(raw_value);
// Set the redacted value on the record and stamp the policy version:
let record = record
    .with_redaction_policy_version(REDACTION_POLICY_VERSION);
// Now validate before persistence:
validate_record(&record)?;
```

## How to verify a store contains redacted rather than raw secrets

Run `inspect` on the JSONL output and search for your known secret values:

```powershell
# Verify no raw token appears in the stored graph
cargo run -- inspect graph.jsonl | grep -c "sk-prod-"
# Expect: 0

# Verify redaction markers are present
cargo run -- inspect graph.jsonl | grep -c "<REDACTED:"
# Expect: >0 for any import that encountered sensitive fields

# Verify policy version metadata is stored
cargo run -- inspect graph.jsonl | grep -c "redaction_policy_version"
# Expect: >0 for any redacted record
```

For shared daemon stores, use `jq` over the JSONL snapshot:

```powershell
jq 'select(.redaction_policy_version != null) | .redaction_policy_version' graph.jsonl
```

## Sensitive field index

| Domain | Field path | Notes |
|--------|-----------|-------|
| Agent-memory | `text` | Observation body text |
| Artifact | `validation_summary` | PatchArtifact validation reason |
| Artifact | `arguments_summary` | ToolCall one-line argument summary |
| Artifact | `arguments_handle.inline` | ToolCall raw arguments (≤16 KiB) |
| Artifact | `result_handle.inline` | ToolCall output (≤16 KiB) |
| Artifact | `patch_handle.inline` | Raw patch bytes (≤16 KiB) |
| Verification | `stdout_handle.inline` | CommandRun/TestRun standard output |
| Verification | `stderr_handle.inline` | CommandRun/TestRun standard error |
| Project | `title` | Task title |
| Project | `body_handle.inline` | Task body text |
| Project | `url` | ExternalLink canonical URL |
| Project | `assignees[*]` | Opaque assignee identifiers |
| Project | `labels[*]` | Project labels |
| User-context | `proposed_rule_text` | PromoteCandidate rule body |
| User-context | `prompt_text` | PromotionPrompt exact text |
| User-context | `decision_rationale` | PromotionDecision rationale |
| User-context | `edited_rule_text` | Edited approval rule body |
| User-context | `rule_text` | Preference/WorkflowRule body |
| User-context | `action_summary` | WorkflowRule action summary |
| User-context | `constraint_text` | Constraint body |

Code-graph fields (repository paths, symbol names, spans, commits, edges) are
**not** in the gate. Redacting source-derived facts would break determinism.

## Out of scope

- No encryption-at-rest: plaintext redaction is enforced first; encryption is
  defense-in-depth on top.
- No new secret taxonomy: see `docs/schema/redaction.md` for the canonical
  class and keyword lists.
- No remote scanning or SaaS dependency: the gate is local-first.
