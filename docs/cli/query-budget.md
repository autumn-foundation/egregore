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
  of the 10x store, and record IDs never collide.
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
   gating), the report schema is complete, and bad arguments fail fast with
   the JSON error envelope. The end-state test
   (`scaling_gate_passes_on_reference_corpus`) is `#[ignore]`d as the
   tracked RED — see the Measured section; remove the attribute when the
   optimization slice turns the gate green.
2. The dedicated `query-budget` CI job (`.github/workflows/ci.yml`), which
   installs ripgrep, builds once, and runs the complete manifest-default
   gate — `./target/debug/egregore audit query-budget` — on the reference
   machine class (`ubuntu-latest`). This is the authoritative scaling
   enforcement on every push/PR, and it is **expected-red** until the
   optimization slice lands (the RED is the signal, not a breakage).

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

Store size, machine class, and measured numbers for the committed benchmark,
so future regressions are detectable. Reference machine class: **GitHub
Actions `ubuntu-latest` runner** (the CI gate).

*Measured 2026-09-30 on a Fly Sprite (8 vCPU, 8 GiB RAM, Ubuntu) — full
manifest-default profile (`--samples 5 --warm-samples 5`, scale factor 10).
`eg` built from this branch at the commit recorded below; ripgrep 14.1.1.*

| Query | Temp | 1x p50 | 1x p95 | 10x p50 | 10x p95 |
|---|---|---|---|---|---|
| symbol | cold | 285ms | 287ms | 2879ms | 2913ms |
| symbol | warm | 289ms | 305ms | 2789ms | 2913ms |
| file | cold | 322ms | 1457ms | 2685ms | 2727ms |
| file | warm | 266ms | 269ms | 2750ms | 2782ms |
| drift | cold | 289ms | 324ms | 2695ms | 2697ms |
| drift | warm | 284ms | 291ms | 2713ms | 2768ms |
| ripgrep baseline (`rg … RepositoryIndex` on the corpus source) | — | 4ms (p50) | 4ms (p95) | — | — |

- 1x store: **10,466 records**; 10x store: **104,660 records**.
- **Scaling assertion (cold symbol): ratio 10.11x vs 2.0x ceiling — FAIL.**
  The query path loads the whole store per invocation (see issue #255's
  notes), so time-to-first-answer scales linearly with record count
  (~27µs/record in both sizes — no pathology, just O(n) load). This is the
  genuine RED this slice was built to capture: the measurement, budget, and
  guard are committed; the optimization slice (indexing / lazy load /
  caching — explicitly out of scope for #120) is required to turn the gate
  green. Do **not** "fix" it by raising `--max-ratio`.
- Advisory budgets (p50 < 2s, p95 < 2s): all six 1x cells within budget;
  all six 10x cells exceed the p95 budget (flagged, not gating). The
  file/cold 1x p95 (1457ms) is a single-sample outlier against a 322ms p50 —
  system noise during the run, not a trend.
- Success-metric status: p95 time-to-first-symbol-answer on the 1x
  representative store is **287ms < 2s** ✓; sub-linear scaling is **not met**
  on current trunk (10.11x) — pending the optimization slice.
