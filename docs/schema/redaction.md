# Redaction Schema - v1

**Status:** Active at v1. This document owns the redaction policy names and
marker grammar used by agent-authored records. It is referenced by producer
schemas such as [`docs/schema/agent-actions.md`](agent-actions.md), which name
the concrete fields that must pass through this policy.

Code-graph records are explicitly out of scope for redaction. Source-derived
facts such as repository paths, symbol names, spans, commits, and topology edges
remain plaintext by construction.

Coordination: [`docs/schema/project-graph.md`](project-graph.md) reserves the
project fields that must pass through this policy:
`Task.title`, `Task.body_handle.inline`, `Task.labels`, `Task.assignees`,
`AcceptanceCriterion.text`, and `ExternalLink.url`.
[`docs/schema/local-project-jsonl.md`](local-project-jsonl.md) narrows the
local-file import-time targets to `task.title`, `task.body` inline form,
`task.labels`, `task.assignees`, `acceptance_criterion.text`, and
`external_link.url`; the on-disk local JSONL file does not redact at rest.

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
