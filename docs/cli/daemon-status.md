# `eg daemon status` — operational status (issue #61)

`GET /v1/status` (rendered by `eg daemon status`) is the daemon's operational
dashboard: what work is queued or in flight, how long the oldest job has waited,
whether writes are being shed under backpressure, and how often the four
operator-relevant error classes have fired. It is distinct from the liveness
probe.

## `/v1/health` vs `eg daemon status`

| | `GET /v1/health` | `eg daemon status` / `GET /v1/status` |
|---|---|---|
| Question | "Is the process up and reachable?" | "Is it healthy, keeping up, and what has gone wrong?" |
| Auth | none required | bearer token required |
| Use for | load balancers, liveness/uptime checks, `is it running?` | triage, capacity/backpressure decisions, incident reports |
| Cost | trivial, no locks | reads the job map + counters (read-only) |

Use `/v1/health` for a fast, unauthenticated up/down signal. Use
`eg daemon status` when you need to know *how* the daemon is doing:
job backlog, oldest-job age, write pressure, and error rates.

`status` is strictly read-only. It never mutates a job, a counter, or any
daemon state, so it is safe to poll as often as you like.

## Fields (operational blocks)

The status payload keeps every pre-existing field (`jobs`, `agents`,
`idempotency_store_size`, `pressure`, plus `api_version`/`status`/`data_dir`) and
adds three operational blocks. The full JSON shape is the contract in
[`docs/schema/daemon-api.md`](../schema/daemon-api.md) § 8.

### `jobs_by_state`

Job counts by the closed lifecycle set `{queued, running, completed, failed}`.
These always sum to the scalar `jobs` field.

A job's internal status is a free string; it is canonicalized into exactly one
bucket for the counts:

- `running` → `running`
- `completed` → `completed`
- `failed` → `failed`
- **anything else** — the initial `queued`, plus any unknown or legacy value —
  → `queued`, the conservative "not yet running, not yet terminal" bucket.

Mapping unknown values to `queued` guarantees no job is ever silently dropped
from the totals.

### `oldest_active_job`

Over the *active* jobs (canonical state `queued` or `running`), the one with the
earliest creation stamp:

```json
{ "start_time_unix_ms": 1700000000000, "age_ms": 1234 }
```

- `start_time_unix_ms` — wall-clock creation instant (epoch milliseconds).
- `age_ms` — `now − start_time`, floored at 0.

`oldest_active_job` is `null` when no job is active (empty map, or only
`completed`/`failed` jobs). A steadily climbing `age_ms` with a non-empty
`jobs_by_state.queued`/`running` is the signal that the write worker is not
keeping up — cross-check the `pressure` block.

### `error_counts`

Process-lifetime **monotonic** counters (they only ever increase; a status read
never resets them) for the four operator-relevant error classes:

| field | error code | meaning |
|---|---|---|
| `retryable_overload` | `queue_full` | writes shed because the write-admission queue was full (issue #45 backpressure). Counted at the single admission reject site, so it includes background (job-path) rejections. |
| `timeout` | `query_timeout` | a read exceeded its budget. |
| `auth` | `unauthorized` | a request presented a missing/invalid bearer token. |
| `schema_validation` | `unknown_schema_version` | a write carried a record schema version this binary does not support. |

Rising `retryable_overload` alongside a `saturated` pressure state means the
daemon is shedding load — back off and retry using the pressure block's
`retry_after_ms` guidance. Rising `auth` points at a misconfigured client token.
Rising `schema_validation` points at a producer/daemon version skew.

## Interpreting pressure + retry guidance

The `pressure` block (issue #45) reports write-admission state
(`idle`/`busy`/`saturated`), the bounded queue depth/capacity, rejection totals,
and, while `saturated`, a `retry_after_ms`. A `queue_full` (429) error envelope
always carries `retry_after_ms` regardless of the current state. Treat
`retry_after_ms` as the minimum backoff before re-submitting a shed write. See
[`docs/schema/daemon-api.md`](../schema/daemon-api.md) § 10 for the full pressure
contract.

## Safe to share

Every value on this endpoint is a count, an age, a timestamp, or a stable error
code. None of it is a payload, record body, transcript, command output, patch
hunk, issue body, environment value, bearer token, or protected-store byte. The
entire status payload is safe to paste verbatim into a bug report or issue.
