# `eg ingest`

Ingest a graph JSONL (from `eg scan` / `eg scan-history`) into a sink with
read-back verification.

```powershell
eg ingest graph.jsonl --adapter dry-run
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
eg ingest graph.jsonl --adapter daemon --data-dir .egregore --idempotency-key <key>
```

`--adapter` selects the destination: `dry-run` (verify only, no store),
`embedded` (open the local `AletheiaDB` store directly), or `daemon` (route
through a running daemon). Concurrency rules for the embedded/daemon writers are
in [`embedded-concurrency.md`](embedded-concurrency.md).

## Capacity preflight and fatal capacity classification (issue #439)

`AletheiaDB` 0.1.1 caps its **process-global string interner** at a
non-overridable `MAX_STRING_COUNT` of **100 000** entries
(`src/storage/index_persistence/mod.rs`, a DoS-protection limit on a
monotonic/append-only interner). At write time only node/edge labels and
property *keys* are interned, so tens of thousands of records write without
complaint. At index-**persist** time the serializer interns every per-record
property *value* string (record id, path, name, summary, signature, doc,
boxed-payload JSON, ...). A large graph mints far more than 100 000 distinct
value strings, overflows the cap, and the store's background persistence thread
then hot-loops on the resulting `CapacityExceeded` error forever — the observed
"ingest hangs" symptom. The published crate cannot be patched (no fork, no path
dependency), so Egregore defends at the CLI boundary.

### Preflight refusal (primary defense)

For `--adapter embedded`, before the store is opened, `eg ingest` estimates the
distinct value strings the graph would intern and **refuses fast** when that
estimate reaches the cap — so a doomed writer is never spawned. The estimate is
a deterministic count over the graph's string-bearing property values plus one
per-record store-side sequence string; integer properties (spans,
`schema_version`) are not counted because they are never interned as strings.

On refusal, `eg ingest` prints a machine-readable envelope on stdout, a
one-line human summary on stderr, and exits with code **2**:

```json
{"ok":false,"error":{"code":"ingest_capacity_exceeded","estimated_distinct_strings":123456,"limit":100000,"record_count":41000,"message":"...","workaround":"...","data_dir":".egregore"}}
```

### Runtime refusal (fatal backstop)

If a capacity overflow instead surfaces during the write or the synchronous
`persist_indexes` (for example against a store that was already partway to the
cap before this ingest), that error is classified as the fatal
`CapacityExceeded` class — never a generic per-record failure — and the same
envelope is printed with `records_written` (how far the ingest got) in place of
`estimated_distinct_strings`, again exiting with code **2**.

### `--force`

`--force` bypasses the **preflight estimate** only. It does not make a real
capacity overflow non-fatal: a genuine interner overflow during write/persist
still classifies as fatal and exits 2 even under `--force`. Use `--force` for
the rare false refusal (see the pre-existing-store gap below).

### Known gap: pre-existing store contents

The preflight is a **graph-only** estimate. It does not account for strings
already interned in a store this graph is being appended to, so a store already
near the cap can still overflow on a graph the preflight passes. That gap is on
the safe side — the preflight refuses eagerly on what it can see, the runtime
`CapacityExceeded` classification is the backstop for the pre-existing-store
case, and `--force` is the escape hatch for a false refusal.

### Workarounds

- Split the graph into smaller per-crate / per-subsystem ingests.
- Query the JSONL directly with the `--graph` query path, which needs no
  embedded store at all.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Every record ingested (and, for `embedded`, indexes persisted). |
| 1 | A generic per-record ingest failure (surfaced through `anyhow`). |
| 2 | Fatal capacity refusal — the preflight estimate reached, or a real write/persist overflow hit, `AletheiaDB`'s non-overridable 100 000 string-interner cap (issue #439). |
