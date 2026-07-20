# `eg capture-tests`

Capture one `cargo test` (or any libtest-JSON-emitting) run as a citable,
deterministic **verification-domain** `TestRun` record.

This is CAPTURE-ONLY. `eg capture-tests` **never executes a test runner**. The
caller runs the tests, captures the machine-readable JSON event stream to a
file, and hands that file plus the run metadata to `eg capture-tests`, which
parses it into a redaction-safe graph record. A captured pass is a recorded
observation of one run; **"no captured failure" is not proof of correctness**.

It is the test-runner sibling of the Verus proof-capture workflow (issue #69)
and is distinct from the typed-write evidence API (`eg write verification`,
issue #107): `capture-tests` parses a runner's own structured output into a
`TestRun`, whereas `eg write verification` records a caller-asserted verdict.

## Shortest workflow

```bash
# 1. Run the tests yourself, capturing libtest's JSON event stream.
cargo test -p mycrate -- -Z unstable-options --format json > run.json
echo "exit=$?"

# 2. Capture that stream as a citable TestRun record.
eg capture-tests \
  --input run.json \
  --out testrun.graph.jsonl \
  --session-id sess-42 \
  --commit "$(git rev-parse HEAD)" \
  --suite mycrate-unit \
  --command "cargo test -p mycrate -- --format json" \
  --exit-code 0 \
  --executed-at 2026-07-19T12:00:00Z

# 3. (Optional) resolve each captured test to a Symbol/File by unioning a code graph.
eg scan . --out code.graph.jsonl
eg capture-tests --input run.json --out testrun.graph.jsonl \
  --session-id sess-42 --commit "$(git rev-parse HEAD)" --suite mycrate-unit \
  --command "cargo test -p mycrate -- --format json" --exit-code 0 \
  --executed-at 2026-07-19T12:00:00Z --graph code.graph.jsonl
```

## Flags

| Flag | Required | Meaning |
| --- | --- | --- |
| `--input <path>` | yes | File holding the libtest JSON event stream (stored, never executed). |
| `--out <path>` | yes | Output JSONL. |
| `--session-id <str>` | yes | Stable session identity; part of the record ID. |
| `--commit <str>` | yes | Commit handle / external identifier; part of the record ID. |
| `--suite <str>` | yes | Suite name; part of the record ID and the node `name`. |
| `--command <str>` | yes | The exact command that produced the stream. Stored, never run. |
| `--exit-code <i64>` | yes | The runner's exit status. |
| `--executed-at <rfc3339>` | yes | Caller-supplied timestamp. Validated RFC 3339. |
| `--runner <str>` | no | Runner name (e.g. `libtest`, `cargo-nextest`). |
| `--runner-version <str>` | no | Runner version. |
| `--repo <str>` | no | Repository identity (reserved for scoping). |
| `--graph <path>` | no | A code graph (from `eg scan`) used to resolve test names to Symbol/File. |
| `--format <str>` | no | Input format. Only `libtest-json` is accepted (the default). |
| `--protected-raw-artifacts` | no | Capture raw input bytes into the protected store. Requires the two flags below. |
| `--protected-store <dir>` | no | Protected store directory. |
| `--producer <id>` | no | Authorised producer identity for the captured blob. |

## Records emitted (success)

- **One `TestRun` node** — `domain: "verification"`, `verification_kind:
  "test_run"`, stable ID `verification_stable_id(["test_run", session_id,
  commit, suite])`. Carries `status` (`pass`/`fail`), `exit_code`,
  `executed_at`, `source_artifact_path` (the `--input` path),
  `source_artifact_hash` (BLAKE3 of the raw input bytes), a bounded normalized
  summary in `stdout_handle`, and a one-line human `summary`. The normalized
  summary is canonical JSON with the per-test `{name, outcome}` list sorted by
  name plus the command and runner name/version — never raw failing-test output.
- **Cross-domain edges (only with `--graph`)** — for each parsed test whose
  final `::`-segment resolves to **exactly one** `Symbol` by name: a
  `MENTIONS_SYMBOL` edge (TestRun → Symbol), a `TOUCHED_FILE` edge (TestRun →
  the symbol's `File`, when present), and — for a failing/timed-out test — a
  `FAILED_ON` edge (TestRun → Symbol). Resolution is conservative: **zero or
  two-plus** name matches emit **no edge** and instead a codegraph-domain
  `Diagnostic` (`test_symbol_unresolved` / `test_symbol_ambiguous`). The code
  graph's own nodes are not re-emitted; union the output with the code graph
  to resolve edge endpoints (mirrors `eg resolve-frames`).
- **`partial_test_output` diagnostic** — when tests parsed but the stream
  carried no terminal `suite` event: the suite status is derived from the test
  outcomes and this diagnostic is emitted (still exit 0 with the `TestRun`).

Without `--graph` the `TestRun` node is emitted self-contained (no cross-domain
edges, no anchoring diagnostic — anchoring requires `--graph`).

## Determinism

Record IDs carry no wall-clock. The producer stamped on every record uses the
caller-supplied `--executed-at` as its `producer_started_at` (never `now`), so
given identical input bytes and identical `--executed-at` the entire output
JSONL is byte-identical across runs.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Capture succeeded (a `TestRun` was written). A partial stream still exits 0. |
| 1 | Usage / provenance error (bad `--executed-at`, unknown `--format`, incomplete `--protected-*` group). Machine-readable `{"code":..,"field":..}` diagnostic. |
| 3 | Protected-store I/O failure (no partial manifest). |
| 4 | Empty input file. A `Diagnostic` (`empty_test_output`) is written; no `TestRun`. |
| 5 | Unparseable input (a non-empty non-JSON line, or zero recognized test/suite events). A `Diagnostic` (`unparseable_test_output`) is written; no `TestRun`. |

## Redaction / safety

Output JSONL carries only record IDs, handles, hashes, counts, statuses, test
names, spans, redaction markers, and the bounded normalized summary. Raw
stdout/stderr, tokens, and environment values never appear inline. Raw input
bytes are retrievable only through the protected store handle (reported to
stdout, never written into the graph).
