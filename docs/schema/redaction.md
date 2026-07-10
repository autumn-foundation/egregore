# Redaction Schema - v1

**Status:** Active at v1. This document owns the redaction policy names and
marker grammar used by agent-authored records. It is referenced by producer
schemas such as [`docs/schema/agent-actions.md`](agent-actions.md), which name
the concrete fields that must pass through this policy.

Structural facts of code-graph records are reproducible and remain plaintext by
construction (e.g. repository paths, symbol names, spans, commits, and topology edges).
However, literal payloads captured into node descriptions (such as the `summary` of
File and Symbol nodes) and vector embeddings are subject to this secret-masking policy.

One code-graph exception exists for PII (issue #116): `Commit` records carry
the Git author identity as deterministic VCS-derived facts on the
`author_name` and `author_email` fields. `author_email` is redaction-eligible:
a redaction-off local store retains the raw author email address at rest,
while evidence-bundle export (`eg bundle export`,
[`docs/cli/bundle.md`](../cli/bundle.md)) always replaces it with a
`<REDACTED:email:hash_prefix>` marker and passes `author_name` through the
detection policy, so an exported bundle contains
zero raw author email addresses.

Coordination: [`docs/schema/project-graph.md`](project-graph.md) reserves the
project fields that must pass through this policy:
`Task.title`, `Task.body_handle.inline`, `Task.labels`, `Task.assignees`,
`AcceptanceCriterion.text`, and `ExternalLink.url`.
[`docs/schema/local-project-jsonl.md`](local-project-jsonl.md) narrows the
local-file import-time targets to `task.title`, `task.body` inline form,
`task.labels`, `task.assignees`, `acceptance_criterion.text`, and
`external_link.url`; the on-disk local JSONL file does not redact at rest.
Coordination: [`docs/schema/user-context.md`](user-context.md) reserves
`PromoteCandidate.proposed_rule_text`, `PromotionPrompt.prompt_text`,
`PromotionDecision.decision_rationale`, `PromotionDecision.edited_rule_text`,
`Preference.rule_text`, `WorkflowRule.rule_text`, `WorkflowRule.action_summary`,
and `Constraint.constraint_text` as user-context redaction fields.

## Secret Classes

The default policy must redact at least these named classes before persistence:

| Class | Examples |
|-------|----------|
| `api_token` | API keys, bearer tokens, OAuth access/refresh tokens. |
| `ssh_private_key` | PEM/OpenSSH private key material. |
| `database_url` | Database URLs with embedded credentials. |
| `cloud_credential` | Cloud access keys, secret keys, service-account secrets. |
| `webhook_secret` | Webhook signing secrets and shared callback tokens. |
| `session_cookie` | Auth/session cookies. |
| `env_secret` | `.env`-style `KEY=VALUE` assignments whose key matches the documented secret-name allowlist. |
| `email` | Email addresses (PII), including the Git author email on exported `Commit` records. |

The initial secret-name allowlist includes keys containing:

`TOKEN`, `SECRET`, `PASSWORD`, `PASS`, `PWD`, `PRIVATE_KEY`, `ACCESS_KEY`,
`API_KEY`, `AUTH`, `COOKIE`, `DATABASE_URL`, `DB_URL`, `WEBHOOK_SECRET`.

## Marker Grammar

Redacted values are replaced with:

```text
<REDACTED:secret_class:hash_prefix>
```

`secret_class` is one of the named classes above. `hash_prefix` is a non-secret
prefix of a BLAKE3 hash of the raw value, long enough for audit correlation but
not sufficient to recover the secret.

## Metadata Fields

Agent-authored records that carry redacted fields must preserve:

| Field | Meaning |
|-------|---------|
| `redaction_policy_version` | The policy version applied to the record. |
| redaction marker | The marker replacing the raw secret. |
| source handle/hash | Provenance for the upstream transcript or artifact. |

Producer schemas own their concrete redacted-field lists. For example,
[`docs/schema/agent-actions.md`](agent-actions.md) reserves
`PatchArtifact.validation_summary`, `PatchArtifact.patch_handle.inline`,
`ToolCall.arguments_summary`, `ToolCall.arguments_handle.inline`, and
`ToolCall.result_handle.inline`.

## Code Facts Redaction Workflow

By default, egregore scans detect and mask secrets in code facts (such as node summaries/descriptions).

To run the workflow offline against a repository:

1. Scan the repository:
   ```powershell
   eg scan <repo-path> --out graph.jsonl
   ```
2. Ingest the graph into the AletheiaDB store:
   ```powershell
   eg ingest graph.jsonl --adapter embedded --embed
   ```

To retain raw literals and skip redaction, use the `--raw-literals` command-line flag during the scan:
```powershell
eg scan <repo-path> --out graph.jsonl --raw-literals
```
