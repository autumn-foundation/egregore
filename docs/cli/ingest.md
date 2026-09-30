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

## `--embed` and the vector-index model identity (issue #104)

`--embed` (embedded adapter only) generates dense embeddings and enables the
store's queryable vector index. It also writes **one extra record**: a
semantic-domain `EmbeddingModel` node recording the identity of the model that
produced the index (provider, name, version, dimension, content hash). That
record is what lets `eg query semantic` prove the query embedder shares the
index's vector space instead of silently ranking across incompatible ones — two
different models can share a dimension, so a dimension check alone is not enough.

Because the record ID is derived from the identity tuple, re-running the same
`--embed` ingest is idempotent (one record), while embedding the same store with
a *different* model leaves a second record and makes the mixed vector space
detectable at query time rather than silently ranked. The identity therefore adds
`+1` to `attempted`/`succeeded` counts on an `--embed` ingest.

Read the identity back with `eg inspect --data-dir <DIR>`; the full contract and
the re-ingest workflow are in
[`semantic-index-identity.md`](semantic-index-identity.md).

## Re-ingest convergence: an unchanged source is a no-op (issue #130)

Record IDs are stable content hashes (blake3, issue #12), so re-ingesting the
same `graph.jsonl` bytes addresses the same records. Before writing, the
embedded sink compares each record byte-for-byte against the stored state
(`ExpectedRecordState::Matched`); a record that matches is **skipped** — no
duplicate physical version is written, no transaction-time history inflates the
census, and every code-graph node/edge identity keeps resolving to exactly one
current record. The one exception is deliberate: a byte-identical re-emit of a
record whose stable ID is *actively tombstoned* writes a fresh version so the
newer write supersedes the tombstone and the record is revived (issues #231,
#333); tombstones themselves are likewise re-issued only once they go stale.

Every ingest therefore reports an explicit upsert summary:

```powershell
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
# attempted: 1234
# inserted: 1234
# unchanged: 0
# succeeded: 1234
# failed: 0
```

- `inserted` — records newly written (or superseding a prior version).
- `unchanged` — records skipped because the store already held
  byte-identical state.
- `succeeded` — the `inserted + unchanged` total, kept for back-compat.

Re-ingesting the identical graph converges to a visible no-op:

```powershell
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
# attempted: 1234
# inserted: 0
# unchanged: 1234
# succeeded: 1234
# failed: 0
```

`inserted: 0` / `unchanged: N` is the convergence signal — it replaces what
used to look like silent `+N` growth (`succeeded: N` on every run gave no way
to tell a fresh ingest from a no-op). The daemon adapter reports the same
breakdown in its response (`inserted` / `unchanged` alongside `idempotent`).

After a no-op re-ingest, `eg inspect --data-dir <DIR>` produces a
byte-identical census (records, nodes, edges, tombstones), and
`eg query symbol <NAME>` / `eg query file <PATH>` return the same id set with
the same count as after the first ingest — no duplicate current nodes or
edges. Net new *current* (queryable) logical records added by a no-op
re-ingest is 0. This holds for structural-only stores and for `--embed`
stores alike, and for `eg import-traj` artifacts: the importer derives stable
IDs from the trajectory bytes, so ingesting the same trajectory twice does
not duplicate `AgentSession` / `AgentRun` / `Observation` / `Failure` records.

Out of scope: incremental refresh from working-tree *edits* (issue #98) and
daemon request-level idempotency keys (already in `src/daemon.rs`) are
separate mechanisms with their own contracts.

## Capacity preflight and fatal capacity classification (issue #439)

`AletheiaDB` bounds its **process-global string interner** — a DoS-protection
limit on a monotonic/append-only structure. At write time only node/edge labels
and property *keys* are interned, so tens of thousands of records write without
complaint. At index-**persist** time the serializer interns every per-record
property *value* string (record id, path, name, summary, signature, doc,
boxed-payload JSON, ...). The interner, not the record count, is therefore the
binding limit on how large a graph one embedded store can hold.

Since the `AletheiaDB` 0.2.0 upgrade the cap is **10 000 000** entries. Egregore
sets it **explicitly** on every embedded store open via
`PersistenceConfig.max_interned_strings`, from the same
`adapters::preflight::MAX_INTERNED_STRINGS` constant that bounds the preflight
estimate below — so the number Egregore refuses at and the number the store
enforces are the same by construction, and an upstream default change cannot
silently desync them. The interner is process-global and read once at open, so
this is a per-**process** budget shared by every store the process opens, not a
per-store one. Roughly 100 bytes of resident memory per interned string (~1 GB
at the cap).

**What this changed.** On `AletheiaDB` 0.1.1 the cap was a hardcoded
`MAX_STRING_COUNT` of **100 000**, overridable only by an environment variable,
and an overflow made the store's background persistence thread hot-loop on
`CapacityExceeded` forever — the observed "ingest hangs" symptom, and the reason
a pre-open refusal was the only safe defense. 0.2.0 raised the cap 100× *and*
removed that retry loop. Graphs in the 100 000–10 000 000 band that 0.1.1
refused outright now ingest normally, with no `--force`.

### Preflight refusal

The preflight is retained, but it is no longer load-bearing against a hang: it
is now an early, well-diagnosed refusal in place of a long write that would fail
at persist time.

For `--adapter embedded`, before the store is opened, `eg ingest` estimates the
distinct value strings the graph would intern and **refuses fast** when that
estimate reaches the cap — so a doomed writer is never spawned. The estimate is
a deterministic count over the graph's string-bearing property values plus one
per-record store-side sequence string; integer properties (spans,
`schema_version`) are not counted because they are never interned as strings.

On refusal, `eg ingest` prints a machine-readable envelope on stdout, a
one-line human summary on stderr, and exits with code **2**:

```json
{"ok":false,"error":{"code":"ingest_capacity_exceeded","estimated_distinct_strings":12345678,"limit":10000000,"record_count":4100000,"message":"...","workaround":"...","data_dir":".egregore"}}
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

## Dangling cross-domain evidence citations (issue #241)

At ingest — dry-run, embedded, and daemon adapters — every cross-domain
evidence citation (`OBSERVES`, `VALIDATED_BY`, and the other
agent/verification→target edges in the closed evidence vocabulary shared with
the `evidence-link-audit`) is resolved against the union of the destination
store and the batch itself, order-independently: a forward reference to a
target defined later in the same batch resolves. A citation whose target
resolves to nothing (never existed, or only a tombstone — tombstones are
non-resolving, so ingest and the audit agree on what "dangling" means) never
enters the store as a live, backed claim.

`--dangling-citation-policy` selects what happens to the citing record:

- `quarantine` (default): the citing record is skipped with a machine-readable
  diagnostic and the rest of the batch ingests. Quarantine cascades: a record
  citing a quarantined record is itself quarantined, so no dangling edge can
  enter through a chain of in-batch citations. The default never corrupts an
  otherwise-valid batch.
- `reject-batch`: the whole batch fails and nothing is written.

The diagnostic is JSON under the stable code `dangling_evidence_citation` and
names exactly the `(citing_record_id, target_record_id, relation,
target_domain)` tuple — it never echoes observation text or payload values:

```json
{"code":"dangling_evidence_citation","citing_record_id":"agent_memory:v1:obs1","target_record_id":"codegraph:v1:ghost","relation":"OBSERVES","target_domain":"codegraph"}
```

For `--adapter daemon` the flag is forwarded to the daemon in the ingest
request (`dangling_citation_policy` payload field; unknown values are a 400).
The offline `eg write` step is unchanged: it cannot see the store, so
enforcement lives at the ingest boundary, which can see both batch and store.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Every record ingested (and, for `embedded`, indexes persisted). |
| 1 | A generic per-record ingest failure (surfaced through `anyhow`). |
| 2 | Fatal capacity refusal — the preflight estimate reached, or a real write/persist overflow hit, the configured `AletheiaDB` string-interner cap of 10 000 000 (issue #439). |
