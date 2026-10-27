# Structural query latency budget (`eg audit query-budget`)

Issue #120. The agent-critical structural path — `eg query symbol`, `eg query
file`, `query drift` — must answer fast on a representative single-crate
store, and a targeted lookup must not degrade linearly as the store grows.
This page defines the latency budget, the committed benchmark + scaling gate
that enforces it, and the measured numbers behind the targets.

Issue #255 (`eg audit query-latency`) remains the cold-p50 gate for
`query symbol` alone; this benchmark extends coverage to `query file` and
`query drift`, adds warm invocations, records a ripgrep baseline, and adds
the scaling assertion that #255 does not have.

## The budget

Targets are wall-clock **time-to-first-answer**: process start → first
emitted result line on stdout, per query, on the reference corpus below, on
the reference machine class.

| Query | Cold p50 | Cold p95 | Warm p50 | Warm p95 |
|---|---|---|---|---|
| `query symbol <NAME>` | < 2s | < 2s | < 2s | < 2s |
| `query file <PATH>` | < 2s | < 2s | < 2s | < 2s |
| `query drift` | < 2s | < 2s | < 2s | < 2s |

- *Cold* means a fresh process per sample: process start → first emitted
  result line on stdout. OS page cache is the only warm state.
- *Warm* means a fresh process per sample run immediately after one
  unmeasured priming invocation of the identical query — the process still
  cold-starts, so warm isolates hot page-cache / filesystem effects.
- **Scaling (the gate):** on the 10x store, cold `query symbol` p50 must be
  within **2x** of the 1x store p50. Sub-linear growth is the contract; a
  linear scan where an index belongs fails this gate.
- The absolute budgets above are **advisory**: the report flags every cell
  that exceeds them, but they never fail the gate — only the relative
  scaling assertion (machine-independent) gates, so hardware variance cannot
  red CI.

## The reference corpus

`corpus/query_budget_corpus.json` (manifest) reuses the issue #255 fixture
source (`corpus/query_latency_corpus/`: a pinned snapshot of Egregore's own
`src/query/` subsystem, 51 Rust files + minimal `Cargo.toml`), scanned with
a pinned transaction time (`2026-01-01T00:00:00Z`).

- The benchmark scans the corpus **N times** (N = `scale_factor`, 10) with
  disjoint repository identities (`<override>-sNN`) and concatenates the
  copies into the scaled store — so the 1x store is exactly the first slice
  of the 10x store, and record IDs never collide. Copies after the first also
  get disjoint symbol names (`__sNN` suffix) and paths (`sNN/` prefix). Then a
  by-name or by-path query has the same answer size at every store size, and
  the ratio measures lookup cost alone.
- Setup also builds the sidecar index (`eg index`) for each store, untimed.
  Without an index a lookup is a linear scan of the whole store.
- A plain deterministic scan produces no `SemanticDrift` nodes (drift is an
  embeddings-over-history product), so the benchmark appends deterministic
  synthetic drift nodes per copy (25 per repo; strictly descending scores, so
  the ranked answer is byte-stable) to give `query drift` a stable answer to
  time. They are fixture-only and clearly labeled as synthetic.
- The benchmark records the actual 1x/10x `record_count` in every report
  (reference: ~10,344 1x), so corpus size stays auditable even as the
  extractor evolves. The gate does **not** assert on the count — only on
  latency — but refuses to measure on a collapsed corpus (< 8,000 records).
- The benchmarked queries are `query symbol RepositoryIndex`,
  `query file symbols.rs`, and `query drift --limit 10`. A sample that emits
  no stdout line is a benchmark error (fail-closed), never a fast zero.

## The ripgrep baseline

Every run also times `rg -n --no-heading --no-messages RepositoryIndex
<corpus source>` with the same spawn-to-first-line harness, so the comparison
to the boring substitute is explicit in the output. If ripgrep is not on
`PATH`, the baseline is recorded as `skipped` with a reason — never silently
dropped.

## Running it

```bash
eg audit query-budget                          # full gate: 5 cold + 5 warm samples x 3 queries x 2 sizes
eg audit query-budget --samples 2 --warm-samples 1   # quicker local check
eg audit query-budget --max-ratio 1.5          # tighter scaling experiment
eg audit query-budget --budget-p95-ms 1000     # tighter advisory budget (still not gating)
eg audit query-budget --format text            # human-readable table instead of JSON
```

Exit codes: `0` — scaling assertion passed (`ok: true`); `1` — scaling
assertion violated (`ok: false`, full JSON report still printed);
`2` — usage/load error.

What it does, in order:

1. Scans the fixture corpus `scale_factor` times into temp `store-1x.jsonl`
   (copy 0) and `store-10x.jsonl` (all copies), appending synthetic drift
   nodes per copy (setup, not timed).
2. For each of `symbol` / `file` / `drift`, against each store size, takes
   `samples` cold and `warm_samples` warm measurements (fresh `eg query`
   processes; warm cells get one unmeasured priming run first).
3. Times the ripgrep baseline with the same harness.
4. Prints the JSON report: per-cell p50/p95, the ripgrep baseline, the
   scaling assertion (`ratio_p50`, `max_ratio`, `pass`), and the advisory
   p95 checks. Exits non-zero only if the scaling assertion is violated.

## CI wiring

The gate is enforced two ways:

1. `tests/integration/query_budget.rs`, which runs in the
   `cargo test --all-targets` matrix. It asserts the gate mechanics: an
   unmeetable `--max-ratio` genuinely fails the gate (exit 1, not a rubber
   stamp), an unmeetable `--budget-p95-ms` still passes (advisory, not
   gating), the report schema is complete, bad arguments fail fast with
   the JSON error envelope, and the end-state test
   (`scaling_gate_passes_on_reference_corpus`) holds the 2x ceiling.
2. The dedicated `query-budget` CI job (`.github/workflows/ci.yml`), which
   installs ripgrep, builds once, and runs the complete manifest-default
   gate — `./target/debug/egregore audit query-budget` — on the reference
   machine class (`ubuntu-latest`). This is the authoritative scaling
   enforcement on every push/PR.

## When the gate fails

1. Read the report: `scaling.ratio_p50` vs `max_ratio`; check whether the
   1x p50 moved (a general slowdown) or only the ratio (a
   size-dependent regression — the linear-scan signature).
2. `git stash` / bisect: the benchmark is deterministic in corpus, so a
   ratio move across commits is attributable.
3. Suspects, in order: store-load path (`load_records_from_jsonl`,
   `read_all_records`), per-record deserialization cost, new work in the
   symbol query's filter/format path that scales with record count.
4. Do **not** "fix" the gate by raising `--max-ratio` or shrinking the
   corpus without a product decision — the 2x scaling ceiling is the
   contract agents rely on. Out-of-scope mechanisms (indexing, caching, lazy
   load) are engineering's call; the budget itself is not.

## Measured (reference run)

Reference machine class: **GitHub Actions `ubuntu-latest` runner** (the CI
gate). Numbers below come from a 4-vCPU Linux sandbox with the debug binary
(the CI build), full manifest-default profile (`--samples 5 --warm-samples 5`,
scale factor 10).

| Query | Temp | 1x p50 | 1x p95 | 10x p50 | 10x p95 |
|---|---|---|---|---|---|
| symbol | cold | 49ms | 59ms | 47ms | 50ms |
| symbol | warm | 58ms | 62ms | 40ms | 41ms |
| file | cold | 26ms | 29ms | 25ms | 29ms |
| file | warm | 27ms | 37ms | 26ms | 30ms |
| drift | cold | 68ms | 80ms | 128ms | 131ms |
| drift | warm | 67ms | 69ms | 134ms | 180ms |
| ripgrep baseline | — | 5ms | 5ms | — | — |

- 1x store: **10,466 records**; 10x store: **104,660 records**.
- **Scaling assertion (cold symbol): ratio 0.95x vs 2.0x ceiling — PASS.**
- All twelve cells are within the advisory budgets.

### Before the fix (history)

Measured 2026-09-30 and re-measured on this sandbox before the fix: the query
path read the whole store per invocation, so latency grew linearly with record
count (symbol cold: 661ms at 1x, 6225ms at 10x; ratio 9.4x). Two causes: a
cold scan of every line, and a sidecar index (issue #447) that was not built
for the benchmark stores and still cost `O(n)` (whole-graph read, whole-graph
BLAKE3, JSON parse of the tables). The fix: index format 2 (binary tables read
in place, cheap validity check), the drift lane on the index, and the index
built in benchmark setup.

Do **not** "fix" a future regression by raising `--max-ratio` or shrinking the
corpus without a product decision.
