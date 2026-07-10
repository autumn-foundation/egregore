# eg scan-logs

Extract **runtime log signatures** from one captured log file into deterministic,
redaction-safe graph records (issues #319 / #320). A log becomes a `LogSource`,
one `ErrorSignature` per distinct `template-v1` fingerprint, up to five
`LogEvent` exemplars per signature, and hourly `LogOccurrenceBucket` counts.

> **A runtime observation, never source truth or verification evidence.** A
> signature is the producing program's own claim about its execution,
> deterministically parsed but **never verified**. Its trust class is
> `runtime_observation`. A signature does not prove a bug exists, and its
> absence does not prove correctness. Deterministic parsing does not upgrade the
> claim's trust.

Raw log text never enters the graph. Every stored excerpt is normalized
(`template-v1`), passed through the v1 redaction policy, and bounded to 200
characters; record IDs and content hashes are one-way BLAKE3 digests.

## Synopsis

```text
eg scan-logs <LOG_PATH> --repo-path <REPO> --out <OUT.jsonl> [--repo-id-override <ID>]
             [--protected-raw-artifacts --protected-store <DIR> --producer <ID> [--captured-at <RFC3339>]]
```

- `<LOG_PATH>` — the log file to scan.
- `--repo-path <REPO>` — repository root for repository attribution.
- `--out <OUT.jsonl>` — output JSONL path.
- `--repo-id-override <ID>` — force the repository identity (fixture-stable tests).
- `--protected-raw-artifacts` — also capture the log's **post-redaction** raw bytes into the
  protected artifact store (issue #321). Disabled by default; requires `--protected-store` and
  `--producer`. The graph JSONL never stores the protected handle.
- `--protected-store <DIR>` — protected store directory (required with `--protected-raw-artifacts`).
- `--producer <ID>` — stable operator identity recorded as authorised for the captured log blob
  (required with `--protected-raw-artifacts`).
- `--captured-at <RFC3339>` — override the blob capture timestamp for deterministic manifests;
  not part of the handle identity. Defaults to the scan's transaction time.

### Protected raw-log capture (issue #321)

With `--protected-raw-artifacts`, the scanned log's redaction-normalized bytes are stored as a
`log_payload` blob in the protected store, retrievable later with `eg protected get`. Redaction
runs **before** capture (a secret-bearing line is stored as its `<REDACTED:…>` marker; the
unredacted original is never persisted). The blob's `content_hash` (BLAKE3 over post-redaction
bytes) is independent of the graph's `LogSource.source_artifact_hash` (BLAKE3 over the
unredacted, newline-normalized bytes), and the graph never carries the protected handle. On
success `scan-logs` prints a one-line JSON capture summary (handle, hash, byte count, class —
never raw bytes); a capture I/O failure prints a `store_io_error` envelope to stderr and exits
`3` with no partial manifest. See [`docs/cli/protected-artifacts.md`](protected-artifacts.md).

## Workflow

```powershell
eg scan-logs app.log --repo-path . --out log.graph.jsonl
eg validate log.graph.jsonl
eg ingest log.graph.jsonl --adapter embedded --data-dir .egregore
eg inspect --data-dir .egregore   # log records appear under "Runtime Observations"
```

## Behavior

| Condition | Exit | Output |
|-----------|------|--------|
| A recognizable text log (`plain-v1` or `jsonl-v1`) | `0` | JSONL on `--out`; exemplar-cap diagnostics (if any) to stderr; capture summary JSON on stdout when `--protected-raw-artifacts` is set |
| Binary / non-UTF-8 input (unrecognized format) | `1` | `{"ok":false,"error":{"code":"unrecognized_format",...}}` on stdout, **no partial output** |
| `--protected-raw-artifacts` set without `--protected-store` or `--producer` | `1` | `{"ok":false,"error":{"code":"missing_field",...}}` on stderr |
| Protected-capture store I/O failure (issue #321) | `3` | `{"ok":false,"error":{"code":"store_io_error",...}}` on stderr, **no partial manifest** |
| The log file cannot be read | non-zero | Error on stderr |

### Format detection

`jsonl-v1` when every non-empty line parses as a JSON object; otherwise
`plain-v1`. `jsonl-v1` reads the well-known keys `timestamp`/`ts`/`time`,
`level`/`severity`/`lvl`, and `message`/`msg`/`error`/`err`.

### Severity (closed set)

`fatal` (a `panic`/`FATAL` line), `error` (`ERROR`), `warn` (`WARN`/`WARNING`).
`info`/`debug`/`trace` lines mint **no** signature but still count toward
`LogSource.line_count`.

### Multi-line events

In `plain-v1`, a Rust panic plus its indented/backtrace continuation lines
(leading whitespace, `stack backtrace:`, `note:`, `at …`, or `<n>: …` frames)
are one logical event.

### `template-v1` normalization

Applied before any hash or ID. Variable spans are replaced, most-specific-first,
with closed tokens: `<TS>` `<UUID>` `<IP>` `<DUR>` `<HEX>` `<PATH>` `<NUM>`. A
repeated error that varies only in timestamp, PID, UUID, and pointer normalizes
to one template — one `ErrorSignature` whose `occurrence_count` equals the raw
repetition count.

### Determinism

Byte-identical across runs for a fixed capture instant; `\r\n`/`\r` line endings
are normalized before hashing, so CRLF and LF checkouts of the same log produce
identical record IDs. Re-ingesting the same file adds zero duplicate records.

## When to use this

- **vs `rg` over the raw file** — `scan-logs` deduplicates thousands of noisy
  repetitions into one citable signature with a stable ID, an occurrence count,
  and hourly buckets, at a fraction of the token cost, and redacts secrets.
- **vs trajectory importers** (`import-traj`, `import-codex`) — those capture an
  agent's own actions; `scan-logs` captures a *program's* runtime output.
- **vs #165 test-run evidence** — verification evidence records a check that
  *ran and passed/failed*; a log signature is an unverified runtime claim.

See [`docs/schema/log-graph.md`](../schema/log-graph.md) for the full domain
contract.
