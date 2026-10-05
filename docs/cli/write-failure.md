# Recording a failed attempt: `eg write failure`

`eg write failure` records a failed attempt live as a typed, citable `Failure`
node in the agent-memory domain. It is the live counterpart of the traj
importer's post-hoc `Failure` nodes: it carries the `eg write` provenance
contract and is linked to its targets through denormalized `evidence_links`
(`FAILED_ON`, `REFERENCES_TASK`), which `eg query failures` reads directly.

```bash
eg write failure \
  --agent-id aide-1 \
  --agent-kind other \
  --session-id sess-9 \
  --observed-at 2026-09-14T12:00:00Z \
  --source-handle "/tmp/run-9.log" \
  --failure-kind command_failure \
  --text "cargo test --workspace failed: 2 failures in egregore-query" \
  --exit-code 101 \
  --failed-on codegraph:v9:deadbeef... \
  --references-task project:v1:cafef00d... \
  --out failure.jsonl
```

## Required fields

| Flag | Meaning |
|------|---------|
| `--agent-id` | Stable agent identity. |
| `--agent-kind` | `codex`, `claude-code`, `vantage`, `rust-swe-agent`, `human`, or `other`. Required — no silent default. |
| `--session-id` | Active session identifier. |
| `--observed-at` | RFC 3339 timestamp. |
| `--source-handle` | Citable source artifact path or hash. |
| `--failure-kind` | `command_failure`, `patch_invalid`, `assumption_rejected`, or `workflow_blocked`. |
| `--text` | Failure description; must be non-empty. |

A missing required field fails with a machine-readable JSON envelope on
stderr naming the field — `{"code":"missing_field","field":"agent_id"}` — and never
echoes the failure text. Exit code is 1.

## Failure kinds and exit codes

`failure_kind` is drawn from the reserved set in
[`docs/schema/agent-memory.md`](../schema/agent-memory.md):

- `command_failure`, `patch_invalid` — may carry `--exit-code`.
- `assumption_rejected`, `workflow_blocked` — carry no exit code; passing
  `--exit-code` with either is rejected as `{"code":"invalid_field","field":"exit_code"}`.

## Citable targets

At least one target is required — a targetless write is rejected with
`{"code":"missing_field","field":"failed_on"}`.

| Flag | Evidence link | Accepts |
|------|---------------|---------|
| `--failed-on` (repeatable) | `FAILED_ON` | canonical `codegraph:` / `artifact:` record IDs |
| `--references-task` (repeatable) | `REFERENCES_TASK` | canonical `project:` task record IDs |

Targets must be canonical `<domain>:v<N>:<64 hex>` record IDs — the hex segment
is exactly 64 characters, the length every stable ID the graph mints carries.
The write path carries no store handle, so a looser handle (file path, symbol
name, GitHub handle) would be silently orphaned at read time and is rejected as
`{"code":"invalid_field","field":"failed_on"}` (or `"references_task"`). Identical
targets are deduplicated before linking: a repeated `--failed-on` value mints
one link and never alters the record ID.

## Redaction and bounds

`--text` is redacted first, then bounded to a 500-byte excerpt with a `…`
marker — redact before truncate, so a partial secret cannot slip under a
detector's minimum-length threshold. Redaction is span-level: each detected
secret span becomes `<REDACTED:secret>` (the same placeholder the log-graph
domain uses for free text), so the excerpt keeps its surrounding context
instead of collapsing to a single marker. `redaction_policy_version` is set on
the node exactly when at least one span was redacted. No raw output or patch
hunks are stored beyond the excerpt.

## Determinism

The `Failure` node ID is content-addressed over the full identity: a
`"live"` discriminator plus `failure_kind`, provenance, `exit_code`, the
redacted-excerpt hash, and the canonical link hash. Re-running with identical
inputs produces byte-identical records (a re-write converges to a no-op), and
the `"live"` discriminator keeps the keyspace disjoint from the traj
importer's `["node", "failure", kind, turn_id, action_idx]` IDs.

## Reading it back

Live-authored failures appear in `eg query failures <handle>` under
`agent_failures` with trust class `agent_authored`, queryable by symbol ID,
file path, symbol name, task handle, or the failure's `source_handle`. See
[`failure-history.md`](failure-history.md).

## Scope

`eg write failure` is a live-authoring surface only. Decision/Lesson
authoring, an MCP failure write, supersession at write time, and changes to
`query failures` semantics are out of scope.
