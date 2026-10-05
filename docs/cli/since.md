# eg query since

The transaction-time delta feed for resuming and swarm agents (issue #197).

A returning agent — or an operator coordinating several agents against one
shared store — needs to ask *"what entered the knowledge store since I last
looked?"* Transaction-time *as-of* snapshots (`eg query symbol --tx-as-of`,
#66) answer point-in-time state; `eg query since` answers the *delta between
two points*. It spans every domain on the one axis they share —
`transaction_time` — so one cursor reconciles new code facts, agent memory,
tasks, and verification evidence without re-reading the whole store.

Each call returns the records whose `transaction_time` is **strictly after**
the cursor, ordered by `(transaction_time, record_id)`, plus a `next_cursor`.
Feeding `next_cursor` back returns only records added after the prior call:
no gaps, no overlap. Any partition of a transaction-time window replays to the
single-window delta as a set.

Retraction and supersession records appear in the feed as records (classified
by `event`), so a consumer learns a prior record was *retired* — not only
that records were added. Edges and tombstones carry no transaction-time
handle by schema and cannot be positioned on the feed; their exclusion is
counted in `diagnostics`, never silent.

## Synopsis

```text
eg query since --tx-after <RFC3339> --graph <PATH> [--repo <SELECTOR>]
    [--domain <NAME> ...] [--limit N] [--format json|text]
eg query since --tx-after <RFC3339> --data-dir <DIR> [--repo <SELECTOR>]
    [--domain <NAME> ...] [--limit N] [--format json|text]
```

```sh
eg scan . --out graph.jsonl
eg query since --graph graph.jsonl --tx-after 2026-01-01T00:00:00Z
# ... later, resume where the last call left off:
eg query since --graph graph.jsonl --tx-after <next_cursor from above>
eg query since --graph graph.jsonl --tx-after 2026-01-01T00:00:00Z \
    --domain agent_memory --domain verification --format text
```

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--tx-after <RFC3339>` | yes | Cursor: only records with `transaction_time` strictly after this instant are returned. An unparseable cursor exits `1` with an `invalid_cursor` error envelope. |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store: history-inclusive view (every superseded version is a feed event), read from a throwaway copy — never the live store — per the read-only lane contract. Providing both `--graph` and `--data-dir` is an error. |
| `--repo <SELECTOR>` | no | Restrict records to one repository (see [Repository scope](query.md#repository-scope---repo-issue-67)). Unknown or ambiguous selectors exit `1` with the standard machine-readable stderr diagnostic. |
| `--domain <NAME>` | no | Restrict the feed to one or more domains (repeatable). One of `codegraph`, `agent_memory`, `verification`, `artifact`, `project`, `semantic`, `user_context`, `log`. Unknown names exit `1` with an `unknown_domain` diagnostic naming the valid domains. |
| `--limit N` | no | Maximum records returned. **Default `500`, maximum `5000`.** Values outside `1..=5000` exit `1` with an `invalid_limit` diagnostic. Truncation only ever cuts *between* transaction-time groups (a partial group is never delivered), so `next_cursor` always lands on a fully-delivered instant and resumption stays gap-free. |
| `--format` | no | `json` (default) or `text`. |

## The cursor contract

- A cursor is an RFC 3339 instant on the transaction-time axis. It is
  **exclusive**: a record stamped exactly at the cursor was already delivered
  by the previous call.
- `next_cursor` is the maximum delivered transaction time (RFC 3339). When the
  delta is empty it echoes the input cursor and the answer reports
  `up_to_date: true` with `empty_reason: "up_to_date"` — a well-formed empty
  answer, distinct from an invalid cursor (which errors).
- Rows order by `(transaction_time, record_id)` ascending: byte-deterministic
  for a fixed store.
- Each row carries at minimum `domain`, `kind`, `record_id`, and
  `transaction_time`, plus the `event` class (`added` | `superseded` |
  `retracted`), `superseded_by` when superseded, and citable source handles
  (`name`, `repo_relative_path`, span) when present. **No raw record bodies
  are emitted.**

## JSON envelope

```jsonc
{
  "ok": true,
  "verb": "since",
  "tx_after": "2026-01-01T00:00:00Z",
  "domains": ["agent_memory"],
  "next_cursor": "2026-01-03T00:00:00Z",
  "up_to_date": false,
  "total_matching": 42,
  "returned": 42,
  "truncated": false,
  "records": [
    {
      "record_id": "agent_memory:v1:obs-1",
      "domain": "agent_memory",
      "kind": "Observation",
      "event": "superseded",
      "transaction_time": "2026-01-03T00:00:00Z",
      "superseded_by": "agent_memory:v1:obs-2",
      "name": "noted widget shape"
    }
  ],
  "diagnostics": []
}
```

## Diagnostics

| Code | Meaning |
|------|---------|
| `missing_transaction_metadata` | N node records carry no transaction-time handle; excluded (no current-state fallback). |
| `invalid_record_transaction_time` | N records carry an unparseable `transaction_time`; excluded. |
| `non_positionable_records` | N edge/tombstone records carry no transaction-time handle by schema; they cannot appear on the feed. |
| `no_transaction_stamps` | No record in scope carries a parseable transaction-time handle; the feed is vacuously up-to-date. |

## Out of scope

Push, subscribe, polling, or webhooks — this is a pull-only query. Valid-time
deltas, conflict resolution between concurrent writers, and cross-store
aggregation are separate slices.
