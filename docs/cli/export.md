# `eg export` — Dump an Embedded Store to Canonical JSONL

**Issue:** #155 — _Export a persisted embedded store back to the record JSONL `eg ingest` reads_

---

## Overview

`eg export` answers one question: **what is actually persisted in this embedded
store, in a form I can read, diff, and re-ingest?** It reads an embedded
AletheiaDB store directly — no daemon, no network, no embeddings — and writes
every persisted graph record (nodes, edges, valid-time tombstones, diagnostics,
superseded versions, and unknown-version records) across every domain, in the
exact record shapes `eg scan` / `eg ingest` emit.

It is the **inverse of `eg ingest`**: `ingest` writes JSONL records into a store,
`export` reads them back out as the same canonical JSONL. It is **distinct from
the issue #68 evidence bundle** — a bundle is a record-closure-scoped,
manifest-wrapped artifact for sharing scoped evidence, while export is the whole
store as plain records with no header, manifest, or timestamp.

```
eg export --data-dir <dir> --out <file.jsonl>
```

Both flags are required: `--data-dir` names the embedded store to read, `--out`
names the JSONL file to create or overwrite.

## The daemon-free round-trip loop

The headline workflow is the store round-trip — ingest records, export them, and
re-ingest into a fresh store to confirm the two stores hold the same inventory:

```
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore

eg export --data-dir .egregore --out roundtrip.jsonl
eg ingest roundtrip.jsonl --adapter embedded --data-dir .egregore-copy

eg inspect --data-dir .egregore
eg inspect --data-dir .egregore-copy
```

For a store without retractions, the two `eg inspect --data-dir` reports match:
identical totals and identical per-domain, per-kind, and per-schema-version
counts. Export → re-ingest is a faithful copy of the store's record inventory.

Because export is the inverse of `ingest` and reuses the same read surface as
`eg inspect --data-dir`, the exported JSONL is exactly what `ingest` accepts —
no translation step. This is not the #68 evidence bundle: there is no manifest,
no scoping selector, and no wrapping envelope, just the store's records.

### Read-only guarantee

Export is strictly read-only. The embedded engine re-persists its index files on
open, so the command copies the store to a throwaway temporary directory (the
same `readonly_audit_store` path `eg inspect --data-dir` uses) and reads the
copy — zero graph records, indexes, idempotency receipts, or runtime files are
created, modified, or deleted in the exported store. A `dir_fingerprint`
before/after test proves the on-disk store is byte-for-byte unchanged.

The store is fully read into memory before the output file is written, so a
mid-read failure never leaves a partial `--out` file behind.

### The forget exception

There is exactly **one** deliberate departure from "every physical record":
a record hidden by an `eg forget` retraction (issue #231) is never re-emitted.
Re-exporting its body would resurface exactly the bytes `forget` was invoked to
hide — a redaction leak — so export drops it.

What is dropped and what is kept:

* **Dropped:** every physical version of the retracted stable ID (the original
  record and any superseded prior versions). Suppression is by stable ID and
  fails closed on privacy, so a forget-then-re-observe record is over-suppressed
  rather than leaked.
* **Kept:** the audit trail — the `Retraction` event node (which names the
  retracted record via its `source_handle`) and the retraction `Tombstone`. The
  fact that a retraction happened round-trips; only the retracted body does not.

A forget retraction is recognized by two independent signals, unioned so a
partially-written retraction still fails closed: the `Retraction` event node
whose `source_handle` names the record, and the deterministic
`retraction_tombstone_id` identity of the retraction tombstone. Valid-time
tombstones from history replay carry neither signal, so the legitimate
historical nodes they mark are preserved and round-trip normally.

This is why export is **not identical** to `eg inspect --data-dir`'s physical
inventory — export deliberately subtracts retracted bodies while inspect (a raw
physical count) still counts their versions. This is intentional export behavior
and changes nothing about `inspect`. `eg validate` on a re-ingested
forget-export is referentially clean (0 defects): the surviving Retraction and
Tombstone records leave no dangling edges.

The count-parity contract in the round-trip loop above therefore holds for
**stores without retractions**. A store carrying a `forget` retraction exports
fewer records than inspect counts, by design.

### Unknown schema versions

`eg ingest` rejects unknown record *kinds* at the write path, so the only unknown
`(domain, kind, schema_version)` tuples that can be persisted are bumped-version
records of otherwise-known kinds. Export reconstructs each such record's
canonical line from its stored properties and re-emits it **verbatim** — never
dropped, never folded into the known-version lines.

A record that genuinely cannot be reconstructed (a required property is absent) is
only reachable through artificial raw injection, never through `eg ingest`. Rather
than emit a malformed line, export surfaces it as an enumerated skip diagnostic on
stderr naming its `domain:kind:version` tuple — the single documented lossless
exception:

```
warning: skipped unknown-version record that could not be reconstructed from stored properties (<domain>:<kind>:<version>)
```

### Redaction

Records are redacted at persistence time (PR-10); export carries them as-is. No
raw transcript text, command output, patch hunks, or issue/comment bodies are
ever rehydrated. Protected raw payloads stay as `protected:v1:` handles and
hashes and are never resolved to bytes. Export moves redacted records; it never
un-redacts them.

### Determinism

The output carries no header, manifest, run id, or timestamp. The final
serialized lines are sorted and joined exactly as `Graph::to_jsonl` does (sort,
`\n` join, single trailing newline), independent of physical store iteration
order. Repeated exports of an unchanged store are byte-identical (verified across
five consecutive runs).

## Errors

A missing, empty, unreadable, or non-Egregore `--data-dir` fails with a stable
diagnostic that names the path, and **no output file is written** — the store is
validated before the engine is opened or `--out` is touched. It is never reported
as a valid-but-empty export.

```
error: embedded store not found at <path> - run `eg ingest --adapter embedded --data-dir <path>` first
error: embedded store at <path> is empty - run `eg ingest --adapter embedded --data-dir <path>` first
error: embedded store at <path> contains no Egregore records - run `eg ingest --adapter embedded --data-dir <path>` first
```

The third diagnostic covers a directory that is non-empty on disk (engine
index/runtime files exist) but holds zero Egregore records — e.g. after ingesting
an empty JSONL, or pointing `--data-dir` at a non-Egregore AletheiaDB directory.
This mirrors `eg inspect --data-dir` (issue #125).

Engine-open or read failures fail the whole export with a diagnostic naming the
path (`failed to open embedded store <path>`, `failed to read embedded store
<path> for export: <error>`); a write failure reports `failed to write export to
<path>`. Without the `embedded-aletheiadb` feature, export bails with `exporting
<path> requires the 'embedded-aletheiadb' feature`.

## Out of scope

Record-closure-scoped, manifest-wrapped sharing artifacts belong to the evidence
bundle (`eg bundle export`, #68). Store composition counts and summaries belong
to `eg inspect` (#125/#47). Daemon-transport export belongs to the daemon, not
this daemon-free path. Schema-migration transforms are out of scope — export
re-emits records in their stored shape, it does not upgrade them. Encryption,
compression, and store synchronization are out of scope.
