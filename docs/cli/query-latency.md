# Cold query-latency budget (`eg audit query-latency`)

Issue #255. Every structural query loads the whole store on each cold
invocation, so time-to-first-answer scales with total store size. This page
defines the measured latency budget that keeps Egregore usable inside an
agent edit loop, and the committed benchmark + gate that enforces it.

## The budget

**p50 < 2s** for cold time-to-first-symbol-answer of
`eg query symbol <NAME>`, measured per input source on the reference corpus
below, on the reference machine class.

- *Cold* means a fresh process per sample: process start → first emitted
  result line on stdout. OS page cache is the only warm state.
- The gate reports **p50 and p95** (linear interpolation over sorted samples);
  the gate fails when **p50** exceeds the budget. p95 is recorded for
  diagnosis, not gating.
- Reference machine class: **GitHub Actions `ubuntu-latest` runner** (the CI
  gate). Calibration on a 2-CPU sandbox VM: p50 ~0.5s (`--graph`) unloaded,
  ~1.0–1.5s under heavy load — wide margin on the reference class.

## The reference corpus

`corpus/query_latency_corpus.json` (manifest) + `corpus/query_latency_corpus/`
(source). A pinned snapshot of Egregore's own `src/query/` subsystem
(51 Rust files + minimal `Cargo.toml`), scanned with a pinned transaction
time (`2026-01-01T00:00:00Z`) and repository identity
(`query-latency-fixture`) — representative of a small crate's code graph.

- The benchmark records the actual `record_count` in every report (reference:
  10,344), so corpus size stays auditable even as the extractor evolves. The
  gate does **not** assert on the count — only on latency — but refuses to
  pass on a collapsed corpus (< 8,000 records).
- The benchmarked query is `query symbol RepositoryIndex`, which matches a
  stable handful of records in the fixture. A sample that emits no stdout
  line is a benchmark error (fail-closed), never a fast zero.

## Running it

```bash
eg audit query-latency                          # full gate: 11 samples × 2 sources
eg audit query-latency --samples 5              # quicker local check
eg audit query-latency --source graph           # measure only --graph (no ingest)
eg audit query-latency --source data-dir        # measure only --data-dir
eg audit query-latency --budget-p50-ms 1000     # tighter local experiment
```

Exit codes: `0` — gate passed (`ok: true`); `1` — gate failed (`ok: false`,
full JSON report still printed); `2` — usage/load error.

What it does, in order:

1. Scans the fixture corpus into a temp `graph.jsonl` (setup, not timed).
2. Ingests it into a temp embedded data dir via `eg ingest` (setup, not
   timed; skipped without the `embedded-aletheiadb` feature, marked
   `"skipped": true` with a reason — never silently dropped).
3. Spawns 11 fresh `eg query symbol RepositoryIndex` processes per source
   (`--graph` and `--data-dir`), timing spawn → first stdout line.
4. Prints the JSON report and exits non-zero if any measured source's p50
   exceeds the budget.

## CI wiring

The gate is enforced two ways:

1. `tests/integration/query_latency_budget.rs`, which runs in all four CI
   legs (`cargo test --all-targets` matrix). It asserts the gate passes on
   the reference corpus, that the report schema is complete, and — with an
   unmeetable `--budget-p50-ms 1` — that the gate genuinely fails instead of
   rubber-stamping. Only the pass/fail test runs the full benchmark; the
   others use cheap `--source graph --samples 1` profiles or the
   pre-measurement validation path.
2. The dedicated `query-latency` CI job (`.github/workflows/ci.yml`), which
   builds once and runs the complete two-source gate —
   `./target/debug/egregore audit query-latency` — on the reference machine
   class (`ubuntu-latest`). This is the authoritative budget enforcement:
   the full 11-samples-per-source measurement on every push/PR.

## When the gate fails

1. Read the report: which source regressed, p50 vs p95 (a p95-only spike is
   environment noise; a p50 shift is a real regression).
2. `git stash` / bisect: the benchmark is deterministic in corpus, so a p50
   move across commits is attributable.
3. Suspects, in order: store-load path (`load_records_from_jsonl`,
   `read_all_records`), per-record deserialization cost, new work in the
   symbol query's filter/format path.
4. Do **not** "fix" the gate by raising the budget or shrinking the corpus
   without a product decision — the 2s budget is the contract agents rely
   on. Out-of-scope mechanisms (indexing, caching, lazy load) are
   engineering's call; the budget itself is not.
