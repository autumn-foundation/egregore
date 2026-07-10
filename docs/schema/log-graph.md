# Log-Signature Domain Schema - v1

Runtime log observations (issues #319 / #320). `schema_version` = `1`. Domain
prefix `log:v1:`. Compatibility class **additive** per
[`schema-versioning.md`](schema-versioning.md).

The `scan-logs` command ([`docs/cli/scan-logs.md`](../cli/scan-logs.md)) is the
producer. Records are deterministic, filesystem-local, and redaction-safe. Raw
log text never enters the graph — record IDs and content hashes are one-way
BLAKE3 digests, and the only message-derived text stored is a bounded,
post-redaction excerpt.

## Trust class invariant

All four node kinds map to the trust class **`runtime_observation`**. A log
signature is the producing program's own claim about its execution,
deterministically parsed but **never verified**. A `runtime_observation` is
never `source_fact` (code-graph) and never `verification_evidence`. Deterministic
parsing does **not** upgrade the claim's trust. A zero count is not a proof of
correctness.

## Node kinds

| Kind | One-line definition |
|------|---------------------|
| `LogSource` | One captured log file: its repo-relative path, format, artifact hash, and total line count. |
| `ErrorSignature` | A deduplicated error fingerprint — one per distinct `template-v1` normalization — with severity, redacted template excerpt, and total occurrence count. |
| `LogEvent` | A bounded exemplar occurrence of a signature (capped, see below). |
| `LogOccurrenceBucket` | An hourly occurrence count for a signature. |

### Payload fields

Carried in one boxed `log` field on the node (mirrors the `dependency`
payload), serialized internally-tagged on `log_kind`.

- **`LogSource`**: `source_relative_path`, `source_format_version`
  (`plain-v1` | `jsonl-v1`), `source_artifact_hash` (BLAKE3 of the
  **newline-normalized** file bytes — the idempotency anchor), `line_count`
  (every logical line, including info/debug noise).
- **`ErrorSignature`**: `fingerprint_algorithm` (`template-v1`),
  `template_excerpt` (redacted, ≤200 chars), `severity`
  (`fatal` | `error` | `warn`), `occurrence_count`, `first_seen`, `last_seen`,
  and (issue #322) an optional `frames` array of structured, redaction-safe
  backtrace frames captured at scan time. Each frame carries `frame_index`
  (zero-based backtrace position) and optional `module_path`, `file_path`
  (repo-relative or generalized external-toolchain form), and `line`. `frames`
  is absent when no backtrace was parsed, capped at 64 frames, and each
  module/file text is bounded to 200 chars. **Frames are non-identity** — see
  the identity table. They are captured **because** `template-v1` normalization
  rewrites file paths to `<PATH>` and drops long backtraces past the excerpt
  bound, so the structured frames preserve the resolvable frame data the
  excerpt cannot.
- **`LogEvent`**: `event_excerpt` (redacted, ≤200 chars), `event_content_hash`,
  `source_line`, `severity`.
- **`LogOccurrenceBucket`**: `bucket_start` (RFC 3339 UTC, hour-floored),
  `bucket_width` (`1h`), `occurrence_count`.

## Edge labels

Structural (emitted by `scan-logs`):

| Label | Wire string | From → To |
|-------|-------------|-----------|
| `FingerprintedAs` | `FINGERPRINTED_AS` | `LogEvent` → `ErrorSignature` |
| `CapturedFrom` | `CAPTURED_FROM` | `ErrorSignature` / `LogEvent` → `LogSource` |
| `Aggregates` | `AGGREGATES` | `LogOccurrenceBucket` → `ErrorSignature` |

Evidence-link:

| Label | Wire string | Purpose |
|-------|-------------|---------|
| `FrameResolvesTo` | `FRAME_RESOLVES_TO` | A backtrace frame resolves to a code-graph `Symbol` / `File` / `Diagnostic` (issue #322, emitted by `eg resolve-frames`). |
| `EmittedDuring` | `EMITTED_DURING` | A signature was emitted during a verification/agent run (reserved for #323, not yet emitted). |

`FRAME_RESOLVES_TO` and `EMITTED_DURING` are valid evidence-link labels;
`FINGERPRINTED_AS`, `CAPTURED_FROM`, and `AGGREGATES` are structural. None of the
five is a code-graph topology label.

### `FRAME_RESOLVES_TO` edge fields (issue #322)

A `FRAME_RESOLVES_TO` edge runs `ErrorSignature` → target and carries two
optional fields (present only on this edge kind):

- `frame_index` — the zero-based backtrace position of the resolved frame.
- `frame_resolution` — a value from the **closed, stable** set:

  | Value | Meaning | Target |
  |-------|---------|--------|
  | `resolved` | The frame's `file:line` (or module-path name) matched exactly one in-repo `Symbol`. | that `Symbol` |
  | `ambiguous` | Two or more in-repo `Symbol`s matched; **every** candidate gets its own edge (no silent pick). | each candidate `Symbol` |
  | `path_only` | The frame's file exists but no enclosing symbol contains the line (optimized-out / macro-generated frame). | the `File` node |
  | `unresolved` | The frame names a repo-relative path absent from the resolved view (deleted / renamed). | a `Diagnostic` marker carrying the redacted frame text |

  A frame into the standard library or a dependency is classified **`external`**
  in a per-signature tally and mints **no** edge; `external` is therefore not a
  member of the on-edge enum. Modeled verbatim on `CallResolution` (issues
  #152/#134): a frame handle is never silently bound to an invented target.

Each edge is mirrored by an `EvidenceLink` (relation `FRAME_RESOLVES_TO`) on the
source `ErrorSignature` node; the two representations agree at write time.

## Stable-ID identity

IDs are `log:v1:<blake3>` over the NUL-joined identity parts below (content is
hashed verbatim — no lowercasing). The **producer envelope and its version
fields are never identity inputs**, so two binary versions over identical input
mint identical IDs. Line endings are normalized (`\r\n`/`\r` → `\n`) before
hashing, so CRLF and LF checkouts yield identical IDs.

| Kind | Identity parts | Non-identity inputs |
|------|----------------|---------------------|
| `LogSource` | `repository_id`, `source_relative_path`, `source_artifact_hash` | `line_count`, capture/transaction time, producer |
| `ErrorSignature` | `repository_id`, `fingerprint_algorithm`, `normalized_template`, `severity` | `occurrence_count`, `first_seen`, `last_seen`, **`frames`**, producer, capture time |
| `LogEvent` | `repository_id`, `signature_id`, `event_valid_time`, `event_content_hash` | `source_line`, byte offsets, producer, capture time |
| `LogOccurrenceBucket` | `repository_id`, `signature_id`, `bucket_start`, `bucket_width` | `occurrence_count`, producer, capture time |

`normalized_template` is the `template-v1` fingerprint **after** redaction, so a
secret never enters the fingerprint hash preimage.

## Aggregation storage design

Per-log-line nodes are a **non-goal**. Storage is signature-centric:

- One `ErrorSignature` per distinct fingerprint aggregates all its occurrences
  (`occurrence_count` = the raw count — 1000 identical panics → one signature).
- Volume over time is stored as hourly `LogOccurrenceBucket` counts, not
  per-line nodes.
- Concrete instances are represented by a capped set of **exemplar** `LogEvent`
  nodes — at most **5** per (signature, source), chosen in canonical order
  `(event_valid_time, event_content_hash, source_line)`. Exceeding the cap
  emits a machine-readable `exemplar_cap_reached` diagnostic with the dropped
  count — never a silent drop.

## Temporal

`valid_time` = the parsed event timestamp with
`valid_time_source = log_event_timestamp`; a timestamp-less line falls back to
the transaction time with `inferred_from_transaction_time`. A
`LogOccurrenceBucket`'s `valid_time` is its hour-floored `bucket_start`;
timestamp-less occurrences bucket under the transaction-time hour. No wall clock
enters IDs or canonical output. See
[`temporal-selectors.md`](temporal-selectors.md).

## Redaction

Redacted fields: `ErrorSignature.template_excerpt`, `LogEvent.event_excerpt`,
and (issue #322) each `ErrorSignature.frames[].module_path` / `file_path` plus
the redacted frame text on any `unresolved` `Diagnostic` marker. The v1
redaction policy ([`redaction.md`](redaction.md)) runs on the normalized
template and on every frame's module/file text; a frame `file_path` is
additionally normalized to a repository-relative form (or truncated from a
recognized external-toolchain anchor such as `/rustc/` or `/registry/`) so no
absolute host path or username enters the graph. A record whose excerpt carries
a `<REDACTED:…>` marker sets `redaction_policy_version`. Excerpts and frame
text are bounded to 200 characters. Output carries only IDs, hashes,
severities, counts, paths, bucket boundaries, frames, and markers — never raw
payload beyond the bounded post-redaction excerpts.

## Producer

`producer_kind = log_importer` with required `producer_components`:
`importer_schema_version`, `source_format_version`, and `fingerprint_algorithm`
(`template-v1`). See [`producer-version.md`](producer-version.md).
