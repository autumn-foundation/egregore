# egregored HTTP/JSON Wire Contract — v1

**Status:** Frozen. Changes that alter semantics of existing fields, rename
error codes, or remove response keys require a `/v2/` prefix.

**Source of truth:** This document. `src/daemon.rs` must conform to it; the
design plan (`docs/plans/2026-05-17-egregore-daemon-design.md`) describes
*how* it is implemented, this document owns *what* it exposes.

---

## 1 — Versioning policy

All routes are prefixed `/v1/`. Within `v1`:

- **Additive changes are allowed**: new optional request fields, new keys in
  the `result` object, new error codes.
- **Breaking changes require `/v2/`**: field renames, semantic changes to
  existing fields, removal of fields, changes to error-code identifiers.

The current API version is surfaced as `"api_version": "v1"` in every
`GET /v1/health` and `GET /v1/status` response.

---

## 2 — Transport and auth

- **Bind**: loopback only (`127.0.0.1`) by default.
- **Protocol**: HTTP/1.1, `Connection: close`.
- **Auth**: every non-`/v1/health` request requires `Authorization: Bearer
  <token>`. The token is written to `egregored.json` in the daemon runtime
  directory when the daemon starts. Requests missing or with wrong tokens
  receive HTTP 401 + `unauthorized`.
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
| `domain`         | string enum  | yes                 | optional           | Must be `"codegraph"` for v1 |
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
| `shutdown_in_progress`| 503  | yes       | yes              | all    |
| `redaction_required`  | 422  | no        | no               | ingest (future) |
| `unresolved_evidence_target` | 422 | no  | no               | ingest, jobs/ingest |

Adding a new code is additive. Renaming or removing a code requires `/v2/`.

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
  "jobs": <integer>,
  "agents": <integer>,
  "idempotency_store_size": <integer>
}
```

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
`drift_top_n`; reserved: `observations_for_symbol`, `agent_sessions_for_repo`.

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
