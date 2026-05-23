# ADR 0003: Use an Egregore Daemon for Shared Store Access

## Status

Accepted

## Wire Contract

The HTTP/JSON wire contract for `egregored` is frozen in
[docs/schema/daemon-api.md](../schema/daemon-api.md). That document is the
single source of truth for request envelopes, response envelopes, error-code
identifiers, idempotency semantics, and versioning policy.

The `POST /v1/query` verb set and payload contract are specified in
[docs/schema/daemon-query.md](../schema/daemon-query.md). That document is the
read-side complement to the write-side wire contract above.

Persisted graph records use their own record-level `schema_version` policy in
[docs/schema/schema-versioning.md](../schema/schema-versioning.md). That policy
is separate from daemon `api_version` and query-envelope `schema_version`; the
daemon rejects unknown record tuples with `unknown_schema_version`.

## Context

Egregore stores code facts, agent memory, project/task state, artifacts, and
verification evidence in one AletheiaDB-backed graph. The current embedded
adapter is useful for tests, one-off ingestion, and local experiments, but a
multi-agent workflow changes the concurrency model.

If every agent shells out to the CLI and each CLI process opens the same
embedded AletheiaDB data directory, Egregore has multiple OS processes
competing to own the same local store. AletheiaDB has in-process concurrency
primitives for transactions, snapshot reads, WAL, and internal indexes, but
that does not give Egregore a clear multi-process coordination boundary.

Egregore also owns concerns above raw storage:

- schema validation and domain namespaces
- redaction before persistence
- provenance and evidence links
- idempotent agent writes
- request admission, backpressure, and operational health
- CLI, SDK, and MCP compatibility surfaces

A generic database pool in front of embedded handles would not solve the real
problem. It would multiply store owners instead of defining one owner.

## Decision

Introduce `egregored`, an Egregore-owned local daemon that is the primary shared
writer and query service for multi-agent workflows.

`egregored` opens exactly one embedded `AletheiaDB` handle for a configured data
directory and acquires an OS-level exclusive lease for that directory before
serving requests. The CLI, future SDK, and future MCP server send requests to
the daemon instead of independently opening the same store.

Keep the existing embedded adapter as:

- the fast unit/integration test path
- the single-process import path
- the fallback for deliberately exclusive local jobs

Do not build a pool of embedded AletheiaDB handles against the same data
directory. If Egregore uses the term "pool", it means bounded request
concurrency inside `egregored`, not multiple database owners.

## Consequences

Positive:

- Multi-agent writes have one serialization and recovery boundary.
- AletheiaDB remains the storage substrate while Egregore owns agent workflow
  semantics.
- The CLI stays ergonomic while becoming a thin client for shared use.
- Read traffic can use snapshot/read transactions without each agent reopening
  the store.
- Idempotency and the v1 `codegraph` schema have one mandatory persistence
  gate. Redaction, richer provenance, and non-codegraph workflow schemas remain
  explicit daemon-layer responsibilities for follow-up slices.

Negative:

- Egregore now has a local service lifecycle to manage.
- The daemon protocol becomes a compatibility surface and needs tests.
- Embedded-only CLI behavior must detect or respect the daemon lease to avoid
  accidental bypasses.
- Large ingestion jobs need explicit backpressure and job status instead of
  blindly holding a CLI process open.

## Alternatives Considered

### Keep CLI-Only Embedded Access

This is the simplest short-term shape, but it is unsafe as the default
multi-agent architecture. It relies on every caller behaving well and gives no
central place for request ordering, idempotency, redaction, or admission
control.

### Add a Local File Lock Around CLI Writes

This is a useful emergency guard and should still be added for the embedded
fallback. It prevents the worst concurrent writers, but it does not create a
good shared read/write service. Agents would still repeatedly start processes,
rebuild local state, and miss daemon-level observability.

### Use the AletheiaDB Daemon Directly

This may become a valid storage transport if the AletheiaDB daemon exposes the
needed primitives. Egregore still needs its own workflow layer for schemas,
redaction, provenance, idempotency, and agent/session semantics. Direct
AletheiaDB daemon access should be an adapter target, not the default agent
surface.
