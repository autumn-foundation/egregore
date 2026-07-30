# Upgrading an embedded store to `AletheiaDB` 0.2.0

Egregore's embedded store (`--data-dir`) is an `AletheiaDB` data directory.
Egregore now links `AletheiaDB` **0.2.0**, up from 0.1.1. This page is for
operators who have an existing `.egregore` directory written by an older `eg`
build.

**Short version:** if your old `eg` process exited cleanly, there is nothing to
do. Every `eg` command that opens a store writes its indexes and shuts the store
down through the normal drop path, so a directory left behind by a completed
command opens under 0.2.0 with full integrity. The one failure mode below is
reachable only when a writer was killed mid-run.

## The one refusal you can hit

`AletheiaDB` 0.2.0 **refuses to open** a data directory that still holds an
unreplayed **pre-v13 write-ahead-log tail** — the shape left behind when an
`eg` build linked against 0.1.x was hard-killed (SIGKILL, container OOM, power
loss) before it drained its WAL.

This is a refusal, not corruption, and it is deliberately fail-closed. 0.1.x
wrote WAL labels as *process-local string-interner ids* rather than as strings.
0.2.0 rebuilds its interner in a different order, so replaying such a tail would
resolve those ids to unrelated strings — silently mislabelling every record
recovered from the tail, with no way to recover the original (the string was
never on disk). Upstream refuses rather than corrupt. **The failed open modifies
nothing**; the directory is exactly as it was.

Egregore surfaces this as an open failure naming the data directory, stating
that nothing was modified, and carrying the remedy plus the upstream detail.

### Remedy

1. **Re-open the directory once with the previous `eg` build.** It drains its
   own WAL and shuts down cleanly. Then re-run under the current build. This is
   the lossless path and the one to prefer.
2. **If the old build is unavailable**, re-ingest from JSONL into a **fresh**
   `--data-dir`. Note that `eg export` reads through the same refused open, so
   exporting the stranded store also requires the old build — recover the JSONL
   from wherever `eg scan` wrote it, not from the store.

There is no in-place repair, and no backup/restore off-ramp: `AletheiaDB`'s
`.albk` backup format did not exist in 0.1.x, so a 0.1.x-era store cannot be
backed up on the old version and restored on the new one. In-place open of a
drained directory is the only migration path.

Going forward this cannot recur: 0.2.0 writes WAL segments that carry labels as
strings, so a hard-killed 0.2.0 writer replays correctly.

## String-interner headroom

The other operator-visible change is a **100× relaxation**. 0.1.1 capped its
process-global string interner at a hardcoded 100 000 entries; 0.2.0 makes it
configurable and Egregore sets it explicitly to **10 000 000**. Graphs that
0.1.1 refused to ingest outright now ingest normally. 0.2.0 also removed the
background-persistence retry loop that turned an overflow into a hang.

Full contract — the preflight estimate, `--force`, and the exit-2 capacity
class — is in [`ingest.md`](ingest.md#capacity-preflight-and-fatal-capacity-classification-issue-439).

## What did not change

Record IDs, schema versions, the JSONL contract, query output, and determinism
are all unaffected: this is a storage-substrate upgrade, not a schema change. A
graph exported before the upgrade re-ingests byte-identically after it.
`AletheiaDB` 0.2.0's new opt-in subsystems (namespaces, schema constraints,
property indexes, changefeed, encryption, replication, multi-tenancy) are inert
— Egregore does not enable any of them in this slice.
