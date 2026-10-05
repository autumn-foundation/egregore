# `eg query bench`

Return the latest captured criterion benchmark run for a benchmark id,
symbol name, or file path — with a citable handle joining the run to the
commit, the raw criterion artifact, and (when anchored) the measured code.

The lane reads `BenchmarkRun` records produced by `eg capture-bench`
(issue #237); it never runs benchmarks.

## Shortest workflow

```bash
# Capture, then ask for the latest run of a benchmark:
eg capture-bench --input target/criterion --out bench.graph.jsonl \
  --session-id sess-42 --commit "$(git rev-parse HEAD)" --suite benches \
  --executed-at 2026-07-19T12:00:00Z --graph code.graph.jsonl

eg query bench parse_scan --graph bench.graph.jsonl
# {
#   "ok": true,
#   "target": "parse_scan",
#   "benchmark_id": "parse_scan",
#   "record_id": "verification:v1:…",
#   "commit": "abc123…",
#   "artifact_path": "target/criterion/parse_scan/new/estimates.json",
#   "artifact_hash": "…",
#   "executed_at": "2026-07-19T12:00:00Z",
#   "estimate_ns": 1250.0,
#   "unit": "ns",
#   "measurement": "mean",
#   "verdict": "regression",
#   "delta_pct": 25.0,
#   "symbol": { "id": "…", "name": "parse_scan", "file": "src/lib.rs" },
#   "run_count": 1
# }

# The run as of a commit or an instant:
eg query bench parse_scan --graph history.graph.jsonl --at abc123
eg query bench parse_scan --graph history.graph.jsonl --as-of 2026-07-19T12:00:00Z
```

## Flags

| Flag | Required | Meaning |
| --- | --- | --- |
| `target` | yes | Benchmark id, symbol name, or file path to look up. |
| `--graph <path>` \| `--data-dir <path>` | yes (one) | Structural store: a JSONL graph or an embedded store. |
| `--at <commit-prefix>` | no | Keep only runs whose captured commit starts with the prefix. Mutually exclusive with `--as-of`. |
| `--as-of <rfc3339>` | no | Keep only runs at or before the instant (valid-time axis). Mutually exclusive with `--at`. |
| `--format <json\|text>` | no | Output format (default `json`). |

## Target resolution

The target matches a `BenchmarkRun` when any of these hold:

1. The run's captured `benchmark_id` equals the target (read from the
   normalized summary in `stdout_handle`, falling back to the node `name`
   for records ingested from foreign producers).
2. A `MENTIONS_SYMBOL` edge runs from the `BenchmarkRun` to a `Symbol`
   whose name equals the target.
3. A `TOUCHED_FILE` edge runs from the `BenchmarkRun` to a `File` whose
   path equals the target.

Candidates are filtered by the temporal selector, then the latest run wins
(greatest `executed_at`, tie-broken by record id — deterministic).

## The citable handle

The answer is one JSON object (or one text line) carrying everything needed
to cite the run: `record_id` (stable, content-derived), `commit` (the
captured commit SHA), `artifact_path` + `artifact_hash` (the raw criterion
`estimates.json`), plus the measurement itself (`estimate_ns`, `unit`,
`measurement`, `verdict`, `delta_pct`) and, when the capture was anchored
with `--graph`, the linked `symbol` (`id`, `name`, `file`). The `symbol` key is
omitted entirely — not `null` — for runs with no anchoring edge. `run_count`
reports how many runs matched the target and selectors, so a caller can tell
"the only run" from "the latest of N".

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success. The answer object is printed to stdout. |
| 1 | Usage error: malformed `--as-of`, or `--graph`/`--data-dir` problems. A machine-readable diagnostic goes to stderr. |
| 2 | No match: no `BenchmarkRun` matched the target (and selectors). The standard no-match envelope `{"ok": false, "error": {"code": "no_match", "benchmark": <target>}}` goes to stdout — not an error, just an empty answer. |
