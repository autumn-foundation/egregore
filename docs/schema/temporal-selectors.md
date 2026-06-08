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
| `tx_as_of` | Transaction time | Return, per stable record ID, the version the store knew at or before `tx_as_of` (issue #66) |
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
| `--tx-as-of <RFC3339>` | | Transaction time | Prior store view (issue #66); combinable with `--as-of` |

`--as-of` and `--at` are **mutually exclusive** (enforced by clap `conflicts_with`).
`--tx-as-of` may be combined with `--as-of` (two-axis query) but **not** with
`--at` (the latter pins the valid axis to a commit; combining them is an
`unsupported_combination` error).

### Exit Codes

| Exit code | Meaning |
|-----------|---------|
| `0` | Query ran; results in the envelope (an empty `records` list with diagnostics is still exit 0) |
| `2` | No match for the plain (non-`--tx-as-of`) `query symbol` path |
| `1` | `--tx-as-of` query rejected before running (malformed timestamp / unsupported flag combination) |

### Transaction-time workflow (shortest local `eg` form)

Seed or export a local store as JSONL (no daemon, no network), then ask what the
graph knew at a chosen transaction instant:

```sh
# What did the store know about `widget` before the 2026-01-03 re-import?
eg query symbol widget --graph store.jsonl --tx-as-of 2026-01-02T00:00:00Z

# Two-axis: what was TRUE at valid-time V, as KNOWN BY transaction-time T?
eg query symbol widget --graph store.jsonl \
  --tx-as-of 2026-01-04T00:00:00Z --as-of 2026-01-02T00:00:00Z
```

The same query runs against a running daemon with `--daemon --data-dir <dir>`,
and against the embedded store with `--data-dir <dir>`.

#### Which prior versions the embedded store / daemon can see

A `--tx-as-of` query can only return versions the store actually **retained**:

- **History-replay (`scan-history`) records** keep every commit snapshot, so the
  embedded store and daemon return the full prior view across the commit
  timeline. This is the supported path for code-graph corrections over time.
- **Project/task records** are append-with-same-entity-id; every physical
  mutation is retained and visible to `--tx-as-of`.
- **Non-temporal current-state records** (a plain `scan` symbol or an
  agent-memory observation re-ingested under the same stable ID) are kept as a
  single current-state version by the embedded store's contract — re-ingesting a
  changed version overwrites the prior one at *ingest* time, so no read path can
  recover it. To time-travel over such corrections, query the exported JSONL with
  `--graph` (which preserves every line you wrote) or model the history through
  `scan-history`. Retaining superseded non-temporal versions in the embedded
  store would be a store-versioning change outside this query slice.

### Response envelope

`--tx-as-of` returns a single JSON object (not bare JSONL rows):

```json
{
  "ok": true,
  "verb": "symbol",
  "name": "widget",
  "tx_as_of": "2026-01-02T00:00:00Z",
  "as_of": null,
  "snapshot": "2026-01-02T00:00:00Z",
  "records": [
    {
      "record_id": "codegraph:v4:...",
      "schema_version": 4,
      "name": "widget",
      "kind": "Symbol",
      "domain": "codegraph",
      "trust_class": "source_fact",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_line": 1, "end_line": 5, "start_byte": 0, "end_byte": 100 },
      "valid_time": "2026-01-01T00:00:00Z",
      "valid_time_source": "author_provided",
      "transaction_time": "2026-01-01T00:00:00Z"
    }
  ],
  "diagnostics": [],
  "page": { "cursor": null, "has_more": false, "returned": 1 }
}
```

Every row carries a stable `record_id`, a `schema_version`, a `domain` and
`trust_class`, valid-time fields when present, a `transaction_time` handle, and a
citable source handle (`repo_relative_path` + `span`). Rows never contain raw
record bodies, summaries, transcript text, command output, patch hunks, issue or
PR bodies, environment values, tokens, or protected payloads — only bounded
handles, hashes, IDs, and counts.

### Diagnostics (stable, machine-readable)

A `--tx-as-of` query **never silently falls back to current state.** Edge cases
are reported as `diagnostics[]` entries (the query still succeeds with exit 0) or,
for malformed input, as a top-level `error` (exit 1):

| Code | Kind | Meaning |
|------|------|---------|
| `invalid_timestamp` | error (exit 1) | `--tx-as-of` or `--as-of` is not RFC 3339 |
| `unsupported_combination` | error (exit 1) | `--tx-as-of` combined with `--at` |
| `before_first_transaction` | diagnostic | instant precedes the earliest known transaction; empty view |
| `after_latest_transaction` | diagnostic | instant at/after the latest known transaction; view reflects all known history |
| `superseded` | diagnostic | a matched record is `superseded_by` another record that is also known by the instant; excluded |
| `missing_transaction_metadata` | diagnostic | a matched record has no transaction-time stamp; excluded (no current-state fallback) |
| `invalid_record_transaction_time` | diagnostic | a matched record's `transaction_time` is unparseable; excluded |
| `no_named_symbol` | diagnostic | no Symbol with the queried name exists in the store |

### When to use `--tx-as-of` vs. the boring substitutes

- **`--tx-as-of`** — "what did *Egregore* know at time T?" Use it to audit the
  graph's own memory before a re-import, correction, redaction-policy change, or
  schema migration changed the view.
- **`--as-of` / `--at`** — "what was *true in the domain* at valid time V / at
  commit C?" Use these for the real-world/code timeline, not the store timeline.
- **`git log -S` / `git blame`** — valid-time code history (when a line/symbol
  changed in the source). They cannot say what the graph had ingested, redacted,
  corrected, or linked at a prior store-observation time.
- **`rg` / `jq` over a JSONL export** — fine when you already have the right file,
  but they offer no first-class transaction-time view and will happily read
  current-state corrections as if they were known earlier.
- **Existing evidence-query workflows** (`eg query context`, memory evidence
  audits, prior-failure queries) — answer "what evidence supports X," not "pin
  the view to a store instant."

> **Transaction-time answers mean "known by Egregore *then*," not "objectively
> true forever."** A row returned for `--tx-as-of T` reflects what the store had
> recorded by T; a later correction may have superseded it. The transaction axis
> is an audit lens on the store's memory, not a truth oracle.

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

`tx_as_of` resolves a record's transaction-time handle from its body fields, in
priority order: explicit `transaction_time` → `ingested_at` → `valid_time` when
`valid_time_source == "inferred_from_transaction_time"` (current-tree scans set
`valid_time` to the scan's wall-clock instant, which *is* the transaction time) →
`temporal.observed_at` for history-replay (`scan-history`) records. Replayed
history has only the commit timeline as a store-observation timeline, so the
transaction axis collapses onto it for that data. A record with no resolvable
transaction-time handle is **excluded** with a `missing_transaction_metadata`
diagnostic — never treated as current state.

| Domain | `as_of` resolves against | `tx_as_of` resolves against | Notes |
|--------|-------------------------|-----------------------------|-------|
| `code_graph` | `valid_time` on node | `transaction_time`; else inferred-from-tx `valid_time` (current scan); else `temporal.observed_at` (history replay) | Use `scan-history` to build the history |
| `agent_memory` | `valid_time` on node | `ingested_at` | Agent must set `valid_time` |
| `project` | `valid_time` on node | `transaction_time` | |
| `artifact` | `valid_time` on node | `transaction_time` / `ingested_at` | |
| `verification` | `valid_time` on node | `transaction_time` / `ingested_at` | |
| `user_context` | `valid_time` on node | `transaction_time` / `ingested_at` | Session-scoped; usually point-in-time |

---

## Schema Version

This specification was introduced at **`SCHEMA_VERSION = 2`** (bumped from 1).  The
`valid_time` and `valid_time_source` fields are new in v2.  Records produced by older
scanners carry `SCHEMA_VERSION = 1` and may omit these fields.

---

## Relationship to daemon-api.md

The daemon query API (`POST /v1/query`) accepts an `as_of` selector object following
this grammar. `as_of.valid_time` and `as_of.transaction_time` are implemented for the
`symbol_by_name` verb (issue #66); `as_of.transaction_time` on any other verb, and
`as_of.since` on every verb, still return a `not_implemented` (HTTP 501) response. See
[daemon-query.md](daemon-query.md) for the verb table and [daemon-api.md](daemon-api.md)
for the envelope format.
