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
```

- `<LOG_PATH>` — the log file to scan.
- `--repo-path <REPO>` — repository root for repository attribution.
- `--out <OUT.jsonl>` — output JSONL path.
- `--repo-id-override <ID>` — force the repository identity (fixture-stable tests).

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
| A recognizable text log (`plain-v1` or `jsonl-v1`) | `0` | JSONL on `--out`; exemplar-cap diagnostics (if any) to stderr |
| Binary / non-UTF-8 input (unrecognized format) | `1` | `{"ok":false,"error":{"code":"unrecognized_format",...}}` on stdout, **no partial output** |
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

### Structured frame capture (issue #322)

Because `template-v1` normalization rewrites file paths to `<PATH>` and the
200-char excerpt bound drops long backtraces, `scan-logs` additionally captures
each event's backtrace as a structured, redaction-safe `frames` array on the
`ErrorSignature` (`frame_index`, and optional `module_path`, `file_path`,
`line`). File paths are normalized to a repository-relative form (or truncated
from a recognized external-toolchain anchor such as `/rustc/` or `/registry/`),
and every module/file text passes through the v1 redaction policy, so no
absolute host path or username enters the graph. Frames are **non-identity** —
they never change a signature's record ID — capped at 64 per signature, and
absent when no backtrace parsed. They are the input `eg resolve-frames` binds to
code-graph symbols (see [`resolve-frames.md`](resolve-frames.md)).

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
