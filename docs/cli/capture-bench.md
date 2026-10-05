# `eg capture-bench`

Capture criterion benchmark runs as citable, deterministic
**verification-domain** `BenchmarkRun` records.

This is CAPTURE-ONLY. `eg capture-bench` **never executes a benchmark
runner**. The caller runs the benchmarks; criterion's own machine-readable
`estimates.json` files are handed here via `--input` and parsed into one
redaction-safe `BenchmarkRun` node per benchmark. A captured run is a recorded
observation of one measurement; **"no captured regression" is not proof of
performance**.

It is the benchmark sibling of the test-run capture (`eg capture-tests`,
issue #165), the coverage capture (issue #230), and the proof capture
(issue #69) — but where those record a pass/fail boolean, `BenchmarkRun`
records a *numeric* measurement with a baseline-relative verdict.

## Shortest workflow

```bash
# 1. Run the benchmarks yourself (criterion writes target/criterion).
cargo bench
# Optionally keep a baseline for comparison (criterion compares `new` vs `base`).
cargo bench -- --save-baseline main

# 2. Capture the criterion output as citable BenchmarkRun records.
eg capture-bench \
  --input target/criterion \
  --out bench.graph.jsonl \
  --session-id sess-42 \
  --commit "$(git rev-parse HEAD)" \
  --suite benches \
  --executed-at 2026-07-19T12:00:00Z

# 3. (Optional) resolve each benchmark id to a Symbol/File by unioning a code graph.
eg scan . --out code.graph.jsonl
eg capture-bench --input target/criterion --out bench.graph.jsonl \
  --session-id sess-42 --commit "$(git rev-parse HEAD)" --suite benches \
  --executed-at 2026-07-19T12:00:00Z --graph code.graph.jsonl
```

## Flags

| Flag | Required | Meaning |
| --- | --- | --- |
| `--input <dir>` | yes | Criterion output directory (`target/criterion` shape: `<benchmark-id>/new/estimates.json`, optional `<benchmark-id>/base/estimates.json`). Stored, never executed. |
| `--out <path>` | yes | Output JSONL. |
| `--session-id <str>` | yes | Stable session identity; part of the record ID. |
| `--commit <str>` | yes | Commit handle / external identifier; part of the record ID. |
| `--suite <str>` | yes | Suite name; part of the record ID and the node `name`. |
| `--command <str>` | no | The exact command that produced the criterion output. Stored, never run. |
| `--executed-at <rfc3339>` | yes | Caller-supplied timestamp. Validated RFC 3339. |
| `--repo <str>` | no | Repository identity (reserved for scoping). |
| `--graph <path>` | no | A code graph (from `eg scan`) used to resolve benchmark ids to Symbol/File. |
| `--format <str>` | no | Input format. Only `criterion-estimates` is accepted (the default). |

## Input shape

The input directory is walked recursively and deterministically (sorted) for
files matching exactly `<benchmark-id>/new/estimates.json`. The benchmark id
is the `new/` directory's path relative to `--input`, so nested criterion
groups (`group/bench/new/estimates.json`) keep their full `group/bench` id.

Each `estimates.json` is criterion's own machine-readable output: every
estimate carries a `point_estimate`, a `confidence_interval`, and a
`standard_error`, all in **nanoseconds** (criterion's native unit). The
central estimate of record is the **`mean`** point estimate; `median` and the
confidence interval are carried alongside when present. An optional sibling
`<benchmark-id>/base/estimates.json` is treated as the saved baseline for the
verdict (see below).

## Records emitted (success)

- **One `BenchmarkRun` node per benchmark** — `domain: "verification"`,
  `verification_kind: "benchmark_run"`, stable ID
  `verification_stable_id(["benchmark_run", session_id, commit, suite,
  benchmark_id])`. Note the fifth tuple element: the schema's recommended
  4-tuple cannot address one record per benchmark, so the benchmark id is
  appended (documented in `docs/schema/verification.md` §2c). Carries
  `status: "pass"` (the run was captured; the numeric outcome is the verdict,
  not a boolean), `executed_at`, `source_artifact_path` (the
  `new/estimates.json` path) and `source_artifact_hash` (BLAKE3 of its bytes),
  a bounded normalized summary in `stdout_handle`, temporal provenance
  (`git_commit`, `valid_time`/`observed_at` from `--executed-at`), and a
  one-line human `summary` (e.g. `benchmark parse_scan: mean 1250 ns (suite
  benches), regression +25% vs baseline`).
- **The normalized summary** (canonical JSON in `stdout_handle.inline`) is
  the re-parseable record: `benchmark_id`, `suite`, `measurement_of_record:
  "mean"`, `unit: "ns"`, `mean_ns`, optional `median_ns` and
  `confidence_interval`, the `baseline` (`mean_ns` + artifact path, or
  `null`), `delta_pct`, and the `verdict`. `source_artifact_path` +
  `source_artifact_hash` pin the raw criterion artifact for later re-parsing.
- **Baseline verdict** — when a `base/estimates.json` exists, the record
  carries `regression` (new mean slower than baseline), `improvement`
  (faster), or `unchanged` (within the ±1% noise floor,
  `BENCH_NOISE_THRESHOLD_PCT`), plus `delta_pct` — the percentage change of
  the mean point estimates, rounded to four decimals. With no baseline the
  verdict is the explicit `no_baseline`, never a silent `unchanged`.
- **Cross-domain edges (only with `--graph`)** — for each benchmark whose
  final `/`-segment resolves to **exactly one** `Symbol` by name: a
  `MENTIONS_SYMBOL` edge (BenchmarkRun → Symbol) and a `TOUCHED_FILE` edge
  (BenchmarkRun → the symbol's `File`, when present). Resolution is
  conservative: **zero or two-plus** name matches emit **no edge** and instead
  a codegraph-domain `Diagnostic` (`bench_symbol_unresolved` /
  `bench_symbol_ambiguous`), and the benchmark id joins the envelope's
  `unresolved` section — never dropped silently (mirrors `eg query task`).
  The code graph's own nodes are not re-emitted; union the output with the
  code graph to resolve edge endpoints (mirrors `eg resolve-frames`).

Without `--graph` the batch is self-contained (no cross-domain edges) and
every benchmark is reported in `unresolved` — anchoring requires `--graph`.

## Determinism and idempotency

Record IDs carry no wall-clock. The producer stamped on every record uses the
caller-supplied `--executed-at` as its `producer_started_at` (never `now`),
the directory walk is sorted, and floats are rounded deterministically — so
given identical input bytes and identical `--executed-at` the entire output
JSONL is byte-identical across runs. Re-capturing the same criterion output
is therefore a no-op: the same `--out` file is overwritten with identical
bytes, and ingesting it twice converges on the same stable record IDs
(issue #130).

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. The envelope on stdout reports `ok: true`, per-benchmark `verdicts`, `record_ids`, and the `unresolved` section. |
| 1 | Usage/provenance error: unknown `--format`, invalid `--executed-at`, unreadable `--input`/`--graph`. A machine-readable `{"code":..,"field":..}` diagnostic goes to stderr. |
| 4 | Empty input: `--input` held no `*/new/estimates.json` files. A single `Diagnostic` (`empty_bench_output`) is written to `--out`; no `BenchmarkRun` is emitted. |
| 5 | Unparseable estimates: at least one `new/estimates.json` was not valid criterion estimates JSON (or carried no finite `mean` point estimate). A single `Diagnostic` (`unparseable_bench_output`) is written to `--out`; no `BenchmarkRun` is emitted. A corrupt *baseline* alone is not fatal — the run is still evidence and gets a `no_baseline` verdict. |
