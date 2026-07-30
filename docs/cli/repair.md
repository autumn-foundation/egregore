# `eg repair` — Offline-Exclusive Store Repair Mode

**Issue:** #72 — _Add offline-exclusive store repair mode_

---

## Overview

`eg repair` is the documented safe lane for recovering **stale or inconsistent
local daemon control-plane state** without corrupting the shared graph,
duplicating writes, leaking tokens, or training agents to bypass the one-owner
rule. It operates only on the runtime sidecar (`.egregore-runtime/`) — never on
graph records, never on the embedded store's index files.

```
eg repair preflight --data-dir <dir> [--format json|text]
eg repair run       --data-dir <dir> [--dry-run | --confirm]
                                     [--quarantine]
                                     [--format json|text]
                                     [--transaction-time <rfc3339>]
```

The workflow honors the `pg_resetwal` discipline: **dry-run is explicit, apply
is explicit, and neither happens by accident.** `eg repair run` with neither
`--dry-run` nor `--confirm` refuses with `confirmation_required` and mutates
nothing.

## Safety doctrine

1. **Dry-run by default in spirit.** Mutation happens only under the explicit
   `--confirm` flag. `preflight` and `--dry-run` are strictly zero-mutation —
   they never create, modify, or delete any runtime file (not even the lock
   file).
2. **Structural repairs only.** Repair removes or quarantines *stale runtime
   metadata* whose owner is provably gone. It never guesses record content,
   never edits graph payloads, never migrates schemas, and never performs
   storage-engine surgery. Anything not provably repairable is **reported, not
   touched**.
3. **Refuse against a live or ambiguous owner.** If a daemon actively holds the
   store lease (or holds it but is unresponsive), repair refuses before doing
   any work and names the `eg daemon` workflow to use next. The confirmed-apply
   path additionally re-acquires the exclusive store lease for the whole
   mutation window, closing the race where a daemon starts between the verdict
   and the mutation.
4. **Deterministic, redaction-safe output.** Output carries only IDs, handles,
   counts, hashes, paths, and stable string codes — **never** bearer tokens, raw
   transcript text, command output, patch hunks, issue bodies, or graph payload
   bodies. Pin `--transaction-time` for byte-identical output across runs.
5. **Before/after re-verification.** After a confirmed mutation the detection
   pass is re-run and the report states the post-repair ownership verdict and
   inspect summary honestly (clean or remaining), so the operator never has to
   trust that the repair "probably worked".

## Ownership verdicts

`preflight` classifies the data directory into one stable verdict:

| Verdict          | Meaning                                                       | Repair allowed? |
| ---------------- | ------------------------------------------------------------- | --------------- |
| `live`           | A daemon holds the lease and responds to health checks.       | Refused — run `eg daemon stop` first. |
| `ambiguous`      | The lock is held but the daemon is unresponsive, **or** the runtime metadata declares an unsupported daemon schema version. | Refused — run `eg daemon status`/`stop`. |
| `stopped`        | No metadata, or metadata with `stopped` state and no owner.   | Allowed, but **no repair is needed** — a healthy stopped store is left untouched. |
| `stale_no_owner` | Non-stopped metadata (crashed/stale) with no active owner.    | Allowed — the one metadata case apply mode repairs. |

## Corruption / recovery classes detected

| Class                          | On-disk signal                                            | Verdict / action |
| ------------------------------ | --------------------------------------------------------- | ---------------- |
| Live / active owner            | lock held + daemon responds                               | `live` → refuse (`live_daemon_active`) |
| Unresponsive-but-owned         | lock held + no health response                            | `ambiguous` → refuse (`ambiguous_ownership`) |
| Unknown runtime schema         | `schema_version` != this build's                          | `ambiguous` → refuse (`unsupported_runtime_schema`); never deletes a newer daemon's state |
| Stale metadata, no owner       | non-stopped metadata present, lock unheld/absent          | `stale_no_owner` → **remove or quarantine** `egregored.json` |
| Healthy stopped store          | `stopped` state with no owner, or no metadata at all      | `stopped` → **no repair needed**, zero mutation |

## Repair actions

| Action                     | Effect |
| -------------------------- | ------ |
| `stale_metadata_cleanup`   | Removes the stale `egregored.json`. |
| `stale_metadata_quarantine`| Moves `egregored.json` aside into `.egregore-runtime/quarantine/egregored.json.quarantined-<hash>` (recoverable) instead of deleting it. Selected with `--quarantine`. |
| `recovery_report_generation` | Writes `.egregore-runtime/repair-report.json` after a confirmed apply. |

Removal is the default. `--quarantine` swaps the destructive removal for a
recoverable move; the moved bytes are preserved verbatim (before/after content
hashes match).

## Repair manifest (apply mode)

A confirmed apply writes a redaction-safe manifest to
`.egregore-runtime/repair-manifest.json` and echoes it in the report's
`manifest` field. Each entry carries:

| Field                  | Meaning |
| ---------------------- | ------- |
| `action`               | The repair action taken (`stale_metadata_cleanup` / `stale_metadata_quarantine`). |
| `action_time`          | RFC 3339 instant the action was taken (pin with `--transaction-time` for deterministic output). |
| `result`               | `applied` / `planned` (dry-run) / `skipped` / `failed`. |
| `original_path`        | The runtime metadata path acted on. |
| `quarantine_path`      | The recoverable destination path (quarantine only; absent otherwise). |
| `before_metadata_hash` | BLAKE3 hex of the metadata bytes before the action. |
| `after_metadata_hash`  | BLAKE3 hex after the action — `null` after a removal, equal to `before` after a quarantine move. |
| `skipped_reason`       | Stable reason string when `result` is `skipped`; `null` otherwise. |

Dry-run returns the same manifest with `result: planned` and performs no writes,
so `eg repair run --dry-run` previews exactly what apply would record.

## Determinism (issue #72 AC)

`preflight` output carries no timestamp and is byte-identical across runs on an
unchanged store. `eg repair run` embeds action timestamps; pin them with
`--transaction-time <rfc3339>` so repeated dry-runs of the same fixture return
byte-identical status, ordering, diagnostics, and manifest preview content.

## Output formats

`--format json` (default) emits a single pretty-printed JSON object. `--format
text` emits a deterministic human-readable summary of the same facts (verdict,
allow/refuse, refusal reasons, actions, manifest). Both carry identical
information; neither includes secrets.

## Shortest local recovery workflow

```
# 1. Detect the problem (embedded ingest reported stale metadata).
eg repair preflight --data-dir .egregore

# 2. If the verdict is `live`, stop the daemon first.
eg daemon stop --data-dir .egregore

# 3. Preview the repair with zero mutations.
eg repair run --data-dir .egregore --dry-run

# 4. Confirm no active owner, then apply one repair.
eg repair run --data-dir .egregore --confirm

# 5. Inspect the manifest.
cat .egregore-runtime/repair-manifest.json

# 6. Restart / retry the daemon-backed workflow.
eg daemon start --data-dir .egregore
```

To keep the moved metadata recoverable rather than deleted, add `--quarantine`
to step 4.

## Write-receipt repair (issue #460 / #72 AC5/AC6)

`eg repair run --receipts` classifies and repairs the daemon's write-receipt
(`idempotency.json`) state — the dangling receipts a crashed daemon leaves that
public diagnostics flag as "manual repair required". It is a distinct phase from
the runtime-metadata repair above: it gates on active store ownership itself and
does not run the ownership-verdict branches.

```
eg repair run --data-dir <dir> --receipts               # AC5: enumerate (read-only)
eg repair run --data-dir <dir> --receipts --confirm     # AC6: apply the safe subset
```

### Anomaly classes

| Class                   | On-disk signal                                                        | Auto-repairable? |
| ----------------------- | -------------------------------------------------------------------- | ---------------- |
| `duplicate_record_ids`  | Two records in a pending receipt collapse to one recovery key.        | Only when the colliding records are **byte-identical** — the redundant copy is dropped (`dropped_redundant_duplicate`). Differing content is reported manual. |
| `conflicting_committed` | A pending receipt's record is committed in the store with **different** content. | Never — always `reported_manual`. |
| `partial_committed`     | A pending receipt's records are (some or all) committed but the receipt was never finalized. | Only when **all** target records are already durably committed — a pure receipt flip `pending → committed` (`finalized_partial`). A genuinely partial write (a record still missing) is reported manual; a commit is never fabricated. |

Detection reuses the exact primitives the daemon's own crash-recovery uses
(recovery-key multiplicity and per-record store-state comparison), so the
offline verdict matches the daemon's runtime verdict.

### Safety

- **AC5 enumerate is strictly read-only**: no lease is taken, the store is
  inspected through a throwaway copy, and the receipt file is never touched.
- **AC6 apply is lease-aware**: it refuses (result `refused`,
  `live_daemon_active`) before any mutation when a live daemon/embedded peer
  holds the lease or a crashed holder left stale metadata, holds the exclusive
  lease for the whole mutate window, and applies **only** the provably-safe
  structural subset — never guessing content, never fabricating a commit, never
  dropping committed bytes.
- The repair is **idempotent-convergent**: exactly one provably-safe action per
  anomaly. De-duplicating a byte-identical duplicate can leave a now-finalizable
  partial receipt, which a subsequent pass finalizes; repeated passes converge to
  a clean store.

### Output

The report carries a `write_receipt_report` object:

| Field                       | Meaning |
| --------------------------- | ------- |
| `scan.total_receipts`       | Number of receipts in the file. |
| `scan.anomalies[]`          | Per-anomaly rows: `class`, `idempotency_key`, `receipt_state`, sorted `record_ids`, `payload_hash`, `structurally_repairable`, `recommended_action`. |
| `outcomes[]` (apply only)   | Per-action rows: `idempotency_key`, `class`, `action`, `applied`, `before_hash`, `after_hash`, `skipped_reason`. |
| `mutated`                   | True when the receipt file was rewritten. |
| `post_scan`                 | After-state re-verification scan (present only when `mutated`). |

The generic `manifest` also carries one `write_receipt_repair_enumerate`
(dry-run) or `write_receipt_repair_apply` row per action, with before/after
hashes. Output is redaction-safe — only idempotency keys, record IDs, payload
**hashes**, closed-vocabulary labels, counts, and paths escape; the private
`records`/`response`/payload bytes never do. Scans are byte-identical across runs
on an unchanged store.

## Out of scope

No AletheiaDB page/WAL repair, compaction, vacuuming, or storage surgery; no
graph-record editing, deletion, or schema migration; no bypass of daemon auth,
lease ownership, or redaction policy; no backup/restore, remote repair service,
or MCP tool. Store inspection belongs to `eg inspect` (#125/#47);
store-vs-working-tree staleness belongs to `eg freshness` (#82).
