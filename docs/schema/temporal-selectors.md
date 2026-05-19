# Temporal Selectors: Bi-Temporal Query Grammar

This document is the **single source of truth** for temporal query semantics in Egregore
and AletheiaDB-backed knowledge graphs.  CLI flags, daemon-API fields, and SDK query
builders must all derive their behaviour from this specification.

---

## Two Time Axes

Every observable fact in the graph lives on two independent time axes.

| Axis | Field name | Meaning |
|------|-----------|---------|
| Valid time | `valid_time` | When the fact was true in the real world (the domain timeline) |
| Transaction time | `transaction_time` | When the system first recorded the fact (the store timeline) |

Both times are **RFC 3339** strings (e.g. `2026-05-19T00:00:00Z`).

### Per-Domain Semantics

| Domain | `valid_time` source | Notes |
|--------|--------------------|----|
| `codegraph` | Commit committer date (`git_commit_committer_date`) for history records; inferred from transaction time (`inferred_from_transaction_time`) for current-tree scans | See `valid_time_source` field |
| `agent_memory` | Author-provided; falls back to ingestion time | Agent sets the logical observation instant |
| `project` / `task` | Author-provided | Event time of the task or milestone |
| `artifact` | Build or release timestamp | When the artifact was produced |
| `verification` | When the check ran | Test run or audit time |
| `user_context` | When the context window was created | Session-scoped |

---

## `valid_time_source` Field

Every record that carries `valid_time` should also carry a `valid_time_source` string that
describes how the value was derived.  Current values:

| Value | Meaning |
|-------|---------|
| `git_commit_committer_date` | Set from the Git commit's committer date during `scan-history` |
| `inferred_from_transaction_time` | Set by the scan process from the wall-clock time of the scan |
| `author_provided` | Explicitly set by the agent or tool that created the record |

---

## Selector Grammar

Selectors are expressed as a JSON object with the following optional keys.  An absent key
means "do not filter on this axis."

```json
{
  "as_of": "<RFC 3339 instant>",
  "since": "<RFC 3339 instant>",
  "tx_as_of": "<RFC 3339 instant>",
  "tx_since": "<RFC 3339 instant>"
}
```

| Key | Axis | Semantics |
|-----|------|-----------|
| `as_of` | Valid time | Return the most-recent record whose `valid_time ≤ as_of` |
| `since` | Valid time | Return all records with `valid_time ≥ since` |
| `tx_as_of` | Transaction time | **Reserved — not yet implemented** |
| `tx_since` | Transaction time | **Reserved — not yet implemented** |

`as_of` and `since` are mutually exclusive with each other.  `tx_as_of` / `tx_since` are
similarly exclusive.

---

## CLI Surface

The `egregore query symbol` subcommand exposes the following temporal flags:

| Flag | Shorthand | Axis | Behaviour |
|------|-----------|------|-----------|
| `--as-of <RFC3339>` | | Valid time | Point-in-time look-up |
| `--at <commit-sha>` | | Valid time | Point-in-time by commit (existing flag) |
| `--tx-as-of <RFC3339>` | | Transaction time | Returns `not_implemented` envelope |

`--as-of` and `--at` are **mutually exclusive** (enforced by clap `conflicts_with`).

### Exit Codes

| Exit code | Meaning |
|-----------|---------|
| `0` | Match found; one JSONL line printed per record |
| `2` | No match / invalid query (includes conflicting flags) |
| `1` | `not_implemented` envelope printed to stdout |

### `not_implemented` Envelope

When `--tx-as-of` is supplied the CLI exits with code 1 and prints the following JSON
to stdout:

```json
{
  "ok": false,
  "error": {
    "code": "not_implemented",
    "message": "--tx-as-of: transaction-time queries are reserved and not yet implemented for JSONL queries; see docs/schema/temporal-selectors.md"
  }
}
```

---

## Defaults and Trust Rules

1. **Current-tree scans** (`scan`): `valid_time` is set to the wall-clock instant of the
   scan with `valid_time_source: "inferred_from_transaction_time"`.  This is a best-effort
   approximation; it is not cryptographically anchored to a commit.

2. **History scans** (`scan-history`): `valid_time` is set from the Git commit's committer
   date with `valid_time_source: "git_commit_committer_date"`.  This is reproducible and
   deterministic.

3. **Agent-authored records**: `valid_time` should be set by the agent to the logical
   observation time.  If absent, the store may infer it from the ingestion timestamp.

4. **Trust hierarchy**: `git_commit_committer_date` > `inferred_from_transaction_time` >
   absent.  Higher-trust sources should be preferred when reasoning about fact age.

---

## Per-Domain Mapping Table

| Domain | `as_of` resolves against | `tx_as_of` resolves against | Notes |
|--------|-------------------------|-----------------------------|-------|
| `code_graph` | `valid_time` on node | transaction log (reserved) | Use `scan-history` to build the history |
| `agent_memory` | `valid_time` on node | transaction log (reserved) | Agent must set `valid_time` |
| `project` | `valid_time` on node | transaction log (reserved) | |
| `artifact` | `valid_time` on node | transaction log (reserved) | |
| `verification` | `valid_time` on node | transaction log (reserved) | |
| `user_context` | `valid_time` on node | transaction log (reserved) | Session-scoped; usually point-in-time |

---

## Schema Version

This specification was introduced at **`SCHEMA_VERSION = 2`** (bumped from 1).  The
`valid_time` and `valid_time_source` fields are new in v2.  Records produced by older
scanners carry `SCHEMA_VERSION = 1` and may omit these fields.

---

## Relationship to daemon-api.md

The daemon query API (`POST /v1/query`) will accept a `selector` object following this
grammar in a future release.  Until `tx_as_of` / `tx_since` are implemented, any request
including those fields must receive a `not_implemented` error response.  See
[daemon-api.md](daemon-api.md) for the envelope format.
