# Embedded Store Concurrency Model

**Status:** v1 contract for the embedded write guard (issue #200). Extends the
runtime contract in [`docs/schema/daemon-runtime.md`](../schema/daemon-runtime.md)
and the decision in
[ADR-0003](../adr/0003-egregore-daemon-shared-store.md).

Egregore is a shared memory substrate for multi-agent workflows, so its local
concurrency rules are part of the operator contract. This document states when
the embedded adapter is safe, when the daemon is required, and exactly which
error a contended write receives.

## The model in one table

| Situation | Safe path | What happens on a violation |
|---|---|---|
| One process writing (`eg ingest --adapter embedded`, `eg refresh`, `eg watch`, `eg decide --data-dir`) | Embedded adapter | n/a — the writer holds the exclusive lease for its lifetime |
| Two or more concurrent writers against one data dir | Run `eg daemon start` and route every writer through `--adapter daemon` | The second embedded writer is **refused before any write** with the structured `store_contended` error below |
| Embedded write while a live daemon serves the data dir | Route the write through `--adapter daemon` | Refused with the same `store_contended` error, naming the daemon holder |
| Embedded write while **stale** non-stopped daemon metadata exists (crashed daemon, lease free) | `eg repair preflight` then `eg repair run --confirm`, then retry | Refused with the existing stale-metadata repair message (unchanged by this guard) |
| Read-only commands while a writer holds the store | Snapshot reads (below) | Reads are never blocked by the write lease |

## The write lease

Every embedded open for mutation acquires an OS-level exclusive advisory lock
on `egregored.lock` in the runtime sidecar directory
(`parent(D)/basename(D).egregore-runtime` for data dir `D`) — the same lock
file the daemon leases at startup. The kernel releases the lock on process
death, so a crashed writer never wedges the store.

Consequences:

- **Single writer, always.** Exactly one process — daemon or embedded — can
  mutate a data dir at any moment. A concurrent write can never be partial,
  interleaved, or silently lost: it either durably persists or is refused
  loudly before touching the store.
- **The daemon and embedded writers share one lock.** An embedded open while
  the daemon is live is refused; a daemon start while an embedded writer is
  live is refused.

## The `store_contended` error

A refused writer receives the stable machine code `store_contended`. On
`eg ingest --adapter embedded`, stdout carries the structured envelope and the
process exits non-zero:

```json
{
  "error": {
    "code": "store_contended",
    "message": "another live writer holds the exclusive embedded write lease for store .egregore; no write was performed. Remedy: route concurrent writers through the daemon (`eg daemon start --data-dir .egregore`, then re-run with `--adapter daemon`), or retry after the current writer releases the store",
    "data_dir": ".egregore",
    "remedy": "route concurrent writers through the daemon (`eg daemon start`, then re-run with `--adapter daemon`), or retry after the current writer releases the store"
  },
  "ok": false
}
```

The envelope fields are `code`, `message`, `data_dir`, and `remedy`; keys are
serialized in canonical (alphabetical) order.

When the holder is identifiable as a live daemon (running runtime metadata
plus a held lock), the message names it:
`a live egregored daemon (pid 4242, address 127.0.0.1:39041) holds the
exclusive embedded write lease ...`.

Other embedded write commands surface the same diagnosis on stderr, prefixed
with the stable `store_contended:` code.

The remedy is always one of:

1. **Use the daemon** — `eg daemon start --data-dir <dir>`, then send writes
   with `--adapter daemon`. This is the correct architecture for concurrent
   agents (ADR-0003).
2. **Retry** — wait for the current writer to release the store, then re-run.
   The refusal performed no write, so a retry is always safe.

A refusal is never a data-loss event: the contended writer's records were not
partially applied, and the holder's records are untouched.

## Reads are never blocked

Read-only commands do not take the write lease. Commands documented as
strictly read-only (`eg freshness`, `eg evidence-freshness`, `eg audit
citations --data-dir`, `eg audit memory --data-dir`) read a throwaway
**snapshot copy** of the store, so they succeed even while a live writer holds
the original's lease and never mutate the store they inspect. Reads of a store
under active write require this snapshot access; reading the live directory
in place is only guaranteed between writes.

## Guarantees under the concurrent-writer fixture

With N ≥ 8 concurrent provenance-complete embedded writers against one data
dir (see `tests/integration/store_contention.rs`):

- zero lost records, zero corrupted or partial records, zero silent
  overwrites — every attempt persists durably or returns `store_contended`;
- the post-race store stays queryable, passes referential-integrity checks,
  and its surviving records are byte-identical to a single-writer baseline.
