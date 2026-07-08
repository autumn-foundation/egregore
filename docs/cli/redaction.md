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

### At-import redaction report (issue #266)

`import-traj` and `import-codex` accept `--redaction-report <path>` (`-` for
stdout) to emit a verifiable, secret-free JSON summary of exactly what the
import redacted, alongside the normal record output:

```powershell
cargo run -- import-traj trajectory.traj --out records.jsonl --redaction-report report.json
cargo run -- import-codex session.jsonl --out records.jsonl --redaction-report -
```

The report is built from the `<REDACTED:secret_class:hash_prefix>` markers
stored on the emitted records, so every entry provably ties to a persisted
marker. Each entry carries the owning record ID, the redacted field path
(e.g. `stdout_handle.inline`), the `secret_class`, and the marker's BLAKE3
`hash_prefix` — never the raw secret value. The envelope carries the source
artifact path and hash, per-class counts, and a total count:

```json
{
  "schema_version": 1,
  "redaction": "enabled",
  "policy_version": "v1",
  "source_artifact_path": "trajectory.traj",
  "source_artifact_hash": "<blake3-hex>",
  "total": 2,
  "counts_by_class": {"cloud_credential": 1, "env_secret": 1},
  "entries": [
    {"record_id": "agent:v1:...", "field_path": "text", "secret_class": "env_secret", "hash_prefix": "abc123def456"}
  ]
}
```

Guarantees:

- The report is emitted even when zero redactions occur (`total: 0`), so a
  silent clean pass is distinguishable from redaction being disabled.
- A field that was absent is never reported as redacted; a field that carries
  a marker is always reported.
- Re-running the same import produces a byte-identical report (canonical
  entry ordering, sorted class counts).
- A passthrough (no-redaction) import marks the report
  `"redaction": "disabled"` with `"policy_version": null` and no entries, so
  an empty report cannot be mistaken for a clean redacted pass. The CLI
  importers always apply the v1 policy; passthrough reports arise only on
  programmatic (dry-run/test) paths.
- When the report goes to stdout (`-`), the human status line moves to stderr
  so stdout is exactly the JSON report.
- Only markers whose hash prefix is exactly 12 lowercase-hex characters — the
  length `redact_value` always emits — are counted; a marker-shaped
  placeholder merely mentioned in a transcript (e.g. `<REDACTED:api_token:f>`)
  never inflates the report.
- `--redaction-report` pointing at the same path as `--out` is rejected
  before either artifact is written, so the report can never overwrite the
  records JSONL.

The report covers the two local transcript importers' at-import write
boundary only — it is not a resting-store leak audit, retraction tool, or
export bundle.

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
