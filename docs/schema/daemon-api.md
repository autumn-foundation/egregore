# egregored HTTP/JSON Wire Contract — v1

**Status:** Frozen. Changes that alter semantics of existing fields, rename
error codes, or remove response keys require a `/v2/` prefix.

**Source of truth:** This document. `src/daemon.rs` must conform to it; the
design plan (`docs/plans/2026-05-17-egregore-daemon-design.md`) describes
*how* it is implemented, this document owns *what* it exposes.
Daemon discovery before HTTP is specified separately in
[`daemon-runtime.md`](daemon-runtime.md).

---

## 1 — Versioning policy

All routes are prefixed `/v1/`. Within `v1`:

- **Additive changes are allowed**: new optional request fields, new keys in
  the `result` object, new error codes.
- **Breaking changes require `/v2/`**: field renames, semantic changes to
  existing fields, removal of fields, changes to error-code identifiers.

The current API version is surfaced as `"api_version": "v1"` in every
`GET /v1/health` and `GET /v1/status` response.

Record-level `GraphRecord.schema_version` values are governed separately by
[`schema-versioning.md`](schema-versioning.md). A v1 daemon can read and write
multiple record schema versions in one store, and must reject unknown
`(domain, kind, schema_version)` tuples with `unknown_schema_version`.

---

## 2 — Transport and auth

- **Bind**: loopback only (`127.0.0.1`) by default.
- **Protocol**: HTTP/1.1, `Connection: close`.
- **Auth**: every non-`/v1/health` request requires `Authorization: Bearer
  <token>`. The token is written to `egregored.json` in the daemon runtime
  directory when the daemon starts; clients find and validate that file through
  [`daemon-runtime.md`](daemon-runtime.md). Requests missing or with wrong
  tokens receive HTTP 401 + `unauthorized`.
- **Future transports** (Unix socket, Windows named pipe) are reserved but out
  of scope for v1.

---

## 3 — Request envelope

All write and query routes (`POST`) accept a JSON request body with these
top-level fields:

| Field            | Type         | Required for writes | Required for reads | Notes |
|------------------|--------------|---------------------|--------------------|-------|
| `request_id`     | string       | yes                 | yes                | Client-chosen correlation ID; echoed in every response |
| `agent_id`       | string       | yes                 | yes                | Identifies the calling agent |
| `session_id`     | string       | yes                 | yes                | Identifies the agent session |
| `idempotency_key`| string       | yes                 | n/a                | Unique per `(agent_id, route)` for ≥ 24 hours |
| `domain`         | string enum  | yes                 | optional           | `"codegraph"`, `"agent_memory"`, `"verification"`, `"artifact"`, `"project"`, or `"semantic"` |
| `created_at`     | RFC 3339     | yes                 | n/a                | Wall-clock time of the write |
| `payload`        | object       | yes                 | yes                | Route-specific content |

Missing required fields → HTTP 400 + `missing_field` with the `field` path.

Reads (`GET`) carry no request body.

---

## 4 — Response envelope

Every non-observability route emits exactly one of:

**Success:**
```json
{
  "ok": true,
  "request_id": "<echoed from request>",
  "result": { /* route-specific */ }
}
```

**Failure:**
```json
{
  "ok": false,
  "request_id": "<echoed, or null if not parsed>",
  "error": {
    "code": "<snake_case enum value>",
    "message": "<human-readable description>",
    "field": "<dotted path, only present for missing_field>",
    "retry_after_ms": <milliseconds, only present when meaningful>
  }
}
```

The `ok: true` and `ok: false` shapes are mutually exclusive — a success
response never has an `"error"` key; a failure response never has a `"result"`
key.

Observability endpoints (`GET /v1/health`, `GET /v1/status`) return flat JSON
with `"api_version": "v1"` rather than the `{ok, result}` envelope.

---

## 5 — Error-code enum

All error codes are snake\_case identifiers. String literals at call sites in
`src/daemon.rs` are forbidden; use the `ErrorCode` enum.

| Code                  | HTTP | Retryable | `retry_after_ms` | Routes |
|-----------------------|------|-----------|------------------|--------|
| `unauthorized`        | 401  | no        | no               | all    |
| `bad_request`         | 400  | no        | no               | all    |
| `missing_field`       | 400  | no        | no               | all writes/queries |
| `invalid_domain`      | 400  | no        | no               | ingest, jobs/ingest |
| `idempotency_conflict`| 409  | no        | no               | ingest, jobs/ingest |
| `not_found`           | 404  | no        | no               | records/{id}, jobs/{id} |
| `payload_too_large`   | 413  | no        | no               | ingest, jobs/ingest |
| `queue_full`          | 429  | yes       | yes              | ingest |
| `query_timeout`       | 408  | yes       | no               | query  |
| `internal_error`      | 500  | yes       | no               | all    |
| `not_implemented`     | 501  | no        | no               | reserved |
| `ambiguous_commit_prefix` | 400 | no     | no               | query (symbol_at_commit) |
| `invalid_limit`       | 400  | no        | no               | query (agent_sessions_for_repo) |
| `runtime_permissions_unsafe` | 500 | no | no               | daemon startup |
| `token_rotated`       | 401  | yes       | no               | reserved |
| `shutdown_in_progress`| 503  | yes       | yes              | all    |
| `redaction_required`  | 422  | no        | no               | ingest (future) |
| `unresolved_evidence_target` | 422 | no  | no               | ingest, jobs/ingest |
| `missing_evidence_handle` | 422 | no    | no               | ingest, jobs/ingest |
| `patch_status_pinned` | 422 | no       | no               | ingest, jobs/ingest |
| `inline_payload_exceeds_ceiling` | 400 | no | no             | ingest, jobs/ingest |
| `acceptance_criterion_missing_verification` | 422 | no | no | ingest, jobs/ingest |
| `drift_prior_target_mismatch` | 422 | no | no | ingest, jobs/ingest |
| `drift_record_immutable` | 422 | no | no | ingest, jobs/ingest |
| `unknown_schema_version` | 422 | no | no | ingest, jobs/ingest, query/read |
| `insufficient_promotion_evidence` | 422 | no | no | ingest, jobs/ingest |
| `unapproved_durable_user_context` | 422 | no | no | ingest, jobs/ingest |

Adding a new code is additive. Renaming or removing a code requires `/v2/`.

Coordination: issue #14 adds `acceptance_criterion_missing_verification` for
project-domain `AcceptanceCriterion` writes. See
[`docs/schema/project-graph.md`](project-graph.md).

Coordination: issue #15 adds `semantic` domain writes,
`drift_prior_target_mismatch`, and `drift_record_immutable`. See
[`docs/schema/semantic-drift.md`](semantic-drift.md).

Coordination: issue #16 reserves `unknown_schema_version` for record-level
version compatibility. See [`docs/schema/schema-versioning.md`](schema-versioning.md).

Coordination: issue #18 reserves `runtime_permissions_unsafe` and
`token_rotated`. Runtime-dir discovery, stale-file detection, and the
`egregored.json` schema live in [`daemon-runtime.md`](daemon-runtime.md).

Coordination: issue #19 reserves `insufficient_promotion_evidence` and
`unapproved_durable_user_context` for authorization-derived user-context writes.
See [`docs/schema/user-context.md`](user-context.md).

Coordination: issue #45 freezes the write-admission pressure contract carried by
`GET /v1/status` and the `queue_full` retry envelope. See
[§ 10 — Write-admission pressure](#10--write-admission-pressure).

---

## 6 — Idempotency contract

Scope: `(agent_id, route, idempotency_key)`.

- **Retention**: at least 24 hours after first commit.
- **Replay, same payload**: returns the original `result` with HTTP 200.
- **Replay, different payload**: returns `idempotency_conflict` (HTTP 409).
- **Persistence**: state survives daemon restart via `idempotency.json` in the
  daemon runtime directory.
- **Health reporting**: `GET /v1/status` reports `"idempotency_store_size"` so
  operators can size retention.

---

## 7 — Read budgets

Every read (`POST /v1/query`) accepts an optional `payload.budget` object:

```json
"payload": {
  "budget": { "max_results": 1000, "timeout_ms": 3000 },
  "record_ids": ["..."]
}
```

Server-enforced defaults (when budget is omitted): `max_results = 5000`,
`timeout_ms = 5000`. Exceeding the timeout returns `query_timeout` (HTTP 408)
with `"partial_result": false`; no partial JSON is emitted.

Valid-time and transaction-time selectors are **reserved** (named, not yet
honored) and will be added additively in a future slice.  The selector grammar
is specified in [`docs/schema/temporal-selectors.md`](temporal-selectors.md).
Any request including `tx_as_of` or `tx_since` fields must receive a
`not_implemented` error response until the transaction-time axis is wired up.

---

## 8 — Route reference

### `GET /v1/health` — no auth required

```json
{
  "api_version": "v1",
  "status": "ok",
  "version": "<crate version>",
  "data_dir": "<canonical store path>"
}
```

### `GET /v1/status` — auth required

```json
{
  "api_version": "v1",
  "status": "running",
  "data_dir": "<canonical store path>",
  "jobs": <integer>,
  "agents": <integer>,
  "idempotency_store_size": <integer>,
  "jobs_by_state": {
    "queued": <integer>,
    "running": <integer>,
    "completed": <integer>,
    "failed": <integer>
  },
  "oldest_active_job": {
    "start_time_unix_ms": <integer>,
    "age_ms": <integer>
  },
  "error_counts": {
    "retryable_overload": <integer>,
    "timeout": <integer>,
    "auth": <integer>,
    "schema_validation": <integer>
  },
  "pressure": {
    "state": "idle",
    "alive": true,
    "queue_capacity": <integer>,
    "queue_depth": <integer>,
    "total_rejections": <integer>,
    "saturation_transitions": <integer>,
    "last_saturated_at_unix_ms": <integer or null>,
    "last_recovered_at_unix_ms": <integer or null>,
    "recent_events": [ <PressureEvent...> ],
    "retry_after_ms": <integer, only present while saturated>
  }
}
```

The `pressure` block is the machine-readable write-admission pressure contract.
It is the stable surface that clients and SDKs read; the `eg daemon status` CLI
renders the same data for humans but is not a contract. See
[§ 10 — Write-admission pressure](#10--write-admission-pressure).

The `jobs_by_state`, `oldest_active_job`, and `error_counts` blocks (issue #61)
are the operational-status surface. Their fields, canonicalization rules, and the
liveness-vs-operational distinction are documented for operators in
[`docs/cli/daemon-status.md`](../cli/daemon-status.md):

- `jobs_by_state` — job counts by the closed lifecycle set
  `{queued, running, completed, failed}`. A job's free-string status is
  canonicalized: `running`/`completed`/`failed` map to themselves, and every
  other value (the initial `queued`, plus any unknown/legacy string) maps to
  `queued`, so no job is dropped from the totals. The counts always sum to the
  scalar `jobs`.
- `oldest_active_job` — over active (queued or running) jobs, the earliest
  creation stamp: `start_time_unix_ms` and its `age_ms`
  (`now − start_time`, floored at 0). `null` when no job is active.
- `error_counts` — process-lifetime **monotonic** counters for four error
  classes: `retryable_overload` (`queue_full` write-admission rejections, #45),
  `timeout` (`query_timeout`), `auth` (`unauthorized`), and `schema_validation`
  (`unknown_schema_version`). Counters only ever increase; a status read never
  resets or mutates them.

Every field on this endpoint is a count, age, timestamp, or stable code — no
payload, record body, transcript, command output, or secret. All of it is safe
to paste into a bug report.

### `POST /v1/records/ingest` — synchronous write

`payload`: `{ "records": [ <GraphRecord...> ] }`

Success result:

```json
{
  "attempted": 3,
  "succeeded": 3,
  "failed": 0,
  "failures": [],
  "record_ids": ["..."],
  "idempotent": false
}
```

### `GET /v1/records/{record_id}` — read-back

Success result: `{ "record": <GraphRecord or null> }`

### `POST /v1/query`

Verb-dispatched query surface. Full spec: [`docs/schema/daemon-query.md`](daemon-query.md).

Top-level fields: `request_id`, `agent_id` (optional), `verb` (required),
`params` (verb-specific), `as_of` (optional bi-temporal selector), `budget`
(optional read limits).

Success result:

```json
{
  "verb":     "<verb>",
  "snapshot": "<RFC 3339>",
  "records":  [ /* verb-specific objects */ ],
  "page":     { "cursor": null, "has_more": false, "returned": N }
}
```

Verb set: `get_records`, `symbol_by_name`, `symbol_at_commit`, `file_defines`,
`drift_top_n`, `semantic_search`, `observations_for_symbol`,
`criteria_for_task`, `agent_sessions_for_repo` (issue #112; see
[`daemon-query.md`](daemon-query.md) for the full verb table); reserved:
`drift`.

### `POST /v1/agents/register`

Top-level fields: `request_id`, `agent_id`, `session_id`, `agent_kind`,
`project_scope`, `created_at` (RFC 3339, required — used as the registration
timestamp; must be identical across retries for the same `(agent_id, session_id)`
pair so that registration records hash-stabilise and hit the idempotency cache).
Success result:

```json
{ "status": "registered", "record_ids": ["..."], "node_kinds": ["Agent", "AgentSession"] }
```

### `POST /v1/agents/heartbeat`

Top-level fields: `request_id`, `agent_id`, `session_id`.
Success result: `{ "status": "ok" }`

### `POST /v1/jobs/ingest` — async write (returns 202)

Same envelope as `/v1/records/ingest`. Success result (202 Accepted):

```json
{ "job_id": "...", "status": "queued" }
```

### `GET /v1/jobs/{job_id}`

Success result: `{ "job_id": "...", "status": "...", "report": <IngestReport or null>, "events": ["..."] }`

### `GET /v1/jobs/{job_id}/events`

Success result: `{ "job_id": "...", "events": ["..."] }`

### `POST /v1/admin/checkpoint` — force index flush

Success result: `{ "status": "checkpointed" }`

### `POST /v1/admin/shutdown` — graceful shutdown

Success result: `{ "status": "stopping" }`

---

## 9 — Worked example: `POST /v1/records/ingest`

### Success

```http
POST /v1/records/ingest HTTP/1.1
Authorization: Bearer <token>
Content-Type: application/json

{
  "request_id": "req-abc123",
  "agent_id":   "codex-agent-1",
  "session_id": "session-42",
  "idempotency_key": "scan-2026-05-18-001",
  "domain":     "codegraph",
  "created_at": "2026-05-18T10:00:00Z",
  "payload": {
    "records": [
      {
        "record_type": "node",
        "id": "codegraph:v1:abc",
        "schema_version": 1,
        "kind": "File",
        "repo_relative_path": "src/lib.rs",
        "summary": "Library root"
      }
    ]
  }
}
```

```json
HTTP/1.1 200 OK

{
  "ok": true,
  "request_id": "req-abc123",
  "result": {
    "attempted": 1,
    "succeeded": 1,
    "failed": 0,
    "failures": [],
    "record_ids": ["codegraph:v1:abc"],
    "idempotent": false
  }
}
```

### Idempotent replay (same `idempotency_key`, same payload)

```json
HTTP/1.1 200 OK

{
  "ok": true,
  "request_id": "req-abc124",
  "result": {
    "attempted": 1,
    "succeeded": 1,
    "failed": 0,
    "failures": [],
    "record_ids": ["codegraph:v1:abc"],
    "idempotent": true
  }
}
```

### Missing field

```json
HTTP/1.1 400 Bad Request

{
  "ok": false,
  "request_id": "req-abc125",
  "error": {
    "code": "missing_field",
    "message": "required field is missing: idempotency_key",
    "field": "idempotency_key"
  }
}
```

### Queue full

```json
HTTP/1.1 429 Too Many Requests

{
  "ok": false,
  "request_id": "req-abc126",
  "error": {
    "code": "queue_full",
    "message": "write queue is full",
    "retry_after_ms": 500
  }
}
```

Every `queue_full` rejection echoes the caller's `request_id`, carries a
positive `retry_after_ms`, and never echoes graph records, command output,
secrets, or any other submitted payload text. The message is a fixed string.

---

## 10 — Write-admission pressure

The daemon is the single owner of a local store, so when several agents write
at once a transiently overloaded daemon must be distinguishable from a broken
one. `GET /v1/status` exposes a bounded, redacted `pressure` block for this.

### Pressure states

| `state`     | Meaning | Operator action |
|-------------|---------|-----------------|
| `idle`      | No writes queued or in flight. | None. |
| `busy`      | Writes are queued or in flight; the queue is admitting them. | None; the daemon is healthy. |
| `saturated` | The bounded write queue rejected at least one write and is shedding load via `queue_full`. | Back off and retry per `retry_after_ms`; the daemon is **alive**, not failed. |

Saturation is *sticky*: it is raised on the first `queue_full` rejection and
cleared only once the backlog drains to the recovery watermark. A status sample
taken any time after a rejection classifies the daemon as `saturated` until it
actually recovers, so a saturated daemon never looks idle.

### Pressure fields

- `state` — one of `idle`, `busy`, `saturated`.
- `alive` — always `true`; pressure is backpressure, never a liveness signal.
- `queue_capacity` — the bounded write-queue capacity.
- `queue_depth` — writes currently admitted but not yet completed.
- `total_rejections` — monotonic count of `queue_full` rejections.
- `saturation_transitions` — count of distinct entries into saturation.
- `last_saturated_at_unix_ms` / `last_recovered_at_unix_ms` — transition
  timestamps, or `null` if the transition has not occurred.
- `recent_events` — a bounded ring buffer (most recent first-in-first-out) of
  pressure-transition diagnostics. Each `PressureEvent` is:

  ```json
  {
    "at_unix_ms": 1748649600000,
    "transition": "entered_saturation",
    "operation": "records/ingest",
    "code": "queue_full",
    "request_id": "req-abc126"
  }
  ```

  `transition` is `entered_saturation` or `exited_saturation`. Events carry only
  bounded metadata — never payload bodies, graph records, command output, or
  secrets. The `request_id` is the caller's correlation id truncated to a
  bounded length for the diagnostic copy (the full id is still echoed in the
  `queue_full` error envelope). The same event is also emitted as a one-line
  structured `stderr` log (`"egregore_event": "daemon_pressure"`) for operators
  tailing daemon output.

- `retry_after_ms` — present only while `state` is `saturated`; the minimum
  back-off before retrying, matching the `queue_full` error field.

### How an agent should respond to `queue_full`

1. **Wait at least `retry_after_ms`** before retrying. The value is positive on
   every rejection.
2. **Retry with the same `idempotency_key`** when the write is otherwise
   unchanged. Idempotency semantics are unchanged by pressure: a replayed write
   returns the original result, and a different payload under the same key still
   returns `idempotency_conflict`.
3. **Do not bypass the daemon with direct embedded writes.** The daemon is the
   single owner of the store (ADR 0003); falling back to embedded writes under
   load reintroduces the contention the daemon exists to remove. Embedded
   fallback rules are unchanged — fall back only when the daemon is *absent*,
   not when it is `saturated`.

A `saturated` daemon that keeps returning `queue_full` with `alive: true` is
healthy and backpressuring; escalate to operator inspection only if status
calls themselves fail or the daemon stops responding.
