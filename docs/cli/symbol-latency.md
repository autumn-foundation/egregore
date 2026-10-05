# Time-to-first-citable-answer product gate (`eg audit symbol-latency`)

Issue #57. The agent-critical structural path — `eg query symbol`,
`eg query file`, `eg query symbol --at <sha>`, and `eg query drift` — must
return **correct, citable** answers fast on a representative local Rust
corpus. This page defines the gate, the benchmark that enforces it, and the
measured numbers behind the targets.

Issue #255 (`eg audit query-latency`) gates cold p50 for `query symbol`
alone; issue #120 (`eg audit query-budget`) covers cold+warm latency for
symbol/file/drift with a ripgrep baseline and a relative scaling gate whose
absolute budgets are advisory. This gate is different: it checks every
measured answer against a fixture expectation (record ID + repo-relative
file/span or commit handle — **latency without correctness does not
count**), adds the symbol-at-commit lookup, compares against more boring
substitutes (`git grep`, `git show`), reports setup costs separately, and
its warm-p95 budget **gates** — it is not advisory.

## The gate

Targets are wall-clock **time-to-first-citable-answer**: process start →
first emitted result line on stdout, per query class, on the reference
corpus below. Warm means a fresh process per sample run immediately after
one unmeasured priming invocation of the identical query — the process
still cold-starts, so warm isolates hot page-cache / filesystem effects.

| Query class | Warm p95 budget | Correctness requirement |
|---|---|---|
| `query symbol <NAME>` | < 2s | every row names the fixture symbol, carries a record ID + repo-relative file/span |
| `query file <PATH>` | < 2s | every row cites the fixture file, carries a record ID + span |
| `query symbol <NAME> --at <SHA>` | < 2s | every row cites the requested commit SHA (+ record ID, file/span when present) |
| `query drift --limit <N>` | < 2s | every row carries a record ID + before/after commit range |

- **5 consecutive runs** per query class; the p95 is taken over the 5 warm
  samples. If any class misses the budget, the workflow fails (exit 1) with
  a stable diagnostic naming the query class and the observed p95, e.g.
  `{"code":"symbol_latency_budget_exceeded","query":"symbol-at-commit","observed_p95_ms":2341.5,"budget_p95_ms":2000.0}`.
- An answer that fails its fixture check fails the gate even if it was
  fast (`symbol_latency_answer_check_failed`); a non-deterministic answer
  across the 5 runs fails it too (`symbol_latency_answer_unstable`).
- **No optimization is performed or expected by this gate.** If p95 misses
  2s, the workflow reports the honest numbers and fails — the optimization
  slice is separate work that must satisfy this gate.

## The reference corpus

`corpus/symbol_latency_corpus.json` (manifest) reuses the issue #255/#120
fixture source (`corpus/query_latency_corpus/`: a pinned snapshot of
Egregore's own `src/query/` subsystem, 52 Rust files, 41,919 Rust LOC,
plus a minimal `Cargo.toml`), scanned with a pinned transaction time
(`2026-01-01T00:00:00Z`) and repository identity.

- Documented fixture scale: **41,919 Rust LOC** (≥ 25,000), **≥ 500
  symbols** (the workflow refuses to measure below 500 symbol records),
  **25 deterministic synthetic git commits** (commit 0 is the pristine
  fixture; each later commit appends one comment line to `churn.rs`, with
  pinned author/committer identity and timestamps so SHAs are stable
  across machines).
- A plain deterministic scan produces no `SemanticDrift` nodes, so the
  benchmark appends deterministic synthetic drift nodes (25, strictly
  descending scores) to give `query drift` a stable answer to time. They
  are fixture-only and clearly labeled as synthetic.
- The symbol-at-commit lookup targets commit index 12 (oldest-first) via
  `eg query symbol <NAME> --at <SHA>` against the `scan-history` store.
- The benchmark records the actual record counts in every report
  (reference: ~10,5xx scan-store records), so corpus size stays auditable
  even as the extractor evolves.

## Setup is reported separately

Cold scan, history replay, ingest, and embedding setup are timed as their
own report rows — never folded into query latency:

| Phase | What it times |
|---|---|
| `cold-scan` | deterministic `scan` of the fixture source into the JSONL store |
| `history-replay` | `scan-history` over the 25-commit synthetic git history |
| `ingest` | `eg ingest --adapter embedded` of the scan store (skipped with a reason when the feature is off or ingest fails) |
| `embedding-setup` | resolving the embedding model (skipped with a reason — no measured query needs embeddings; materializing the model would download it, which is network-dependent and out of this benchmark's local-first contract) |

## The boring substitutes

Every run also times the boring substitutes with the same
spawn-to-first-line harness, so the comparison is explicit in the output.
Each row carries a comparability note; a substitute that cannot return
citable graph handles is marked `not-comparable` with the reason stated —
never silently compared.

| Query class | Substitute | Verdict |
|---|---|---|
| symbol | `rg -n … <symbol> <source>` | comparable timing, but returns file:line text hits — not citable graph record IDs |
| symbol | `git grep -n <symbol>` | comparable timing, same caveat: no record IDs, no DEFINES edges |
| file | `rg` for top-level `fn`/`struct`/`enum`/… lines | **not comparable** — a regex cannot return DEFINES edges or record IDs |
| symbol-at-commit | `git show <sha>:<file>` | **not comparable** — returns the file's bytes at the commit (no symbol record, no record ID); retrieval-only lower bound |
| drift | — | **not comparable** — semantic drift has no text-search equivalent |

## When Egregore should beat the boring substitute, when it is
intentionally slower, and when to just use `rg`

- **Egregore should win** on any question whose answer is a *handle* rather
  than a *hit*: "the record for this symbol" (stable ID an agent can cite
  across turns), "everything this file defines" (DEFINES edges, not a
  regex), "this symbol at commit X" (the graph's temporal axis), and ranked
  semantic drift (no text tool can produce it). If `rg` ever beats Egregore
  on time-to-first-*citable*-answer for these, that is the regression this
  gate exists to catch.
- **Egregore is intentionally slower** at raw text throughput: every query
  loads the store and returns evidence-linked rows with record IDs, spans,
  and provenance. That fixed cost is the price of citable answers — the
  2s budget prices it in, and the substitutes' rows in the report keep the
  gap honest.
- **Just use `rg`** when you need text hits, not handles: "where is this
  string mentioned", "which files contain this literal", quick
  edit-loop greps. If the answer will not be cited as a graph record,
  Egregore's evidence machinery is pure overhead.

## Running it

```bash
eg audit symbol-latency                              # full gate: 5 warm samples x 4 query classes
eg audit symbol-latency --warm-samples 2             # quicker local check (relaxes the 5-run rule)
eg audit symbol-latency --budget-p95-ms 1000        # tighter hard budget
eg audit symbol-latency --history-commits 5         # smaller synthetic history (faster setup)
eg audit symbol-latency --format text               # human-readable table instead of JSON
```

Exit codes: `0` — gate passed (`ok: true`); `1` — gate failed
(`ok: false`, full JSON report still printed, stable diagnostic on
stderr); `2` — usage/load error.

What it does, in order:

1. Builds the fixture in a temp dir: deterministic 25-commit git history
   from the fixture source, cold scan → `store-scan.jsonl` (+ synthetic
   drift), `scan-history` → `store-history.jsonl`, embedded ingest, and
   the embedding-setup record (setup, timed separately, not gated).
2. For each of `symbol` / `file` / `symbol-at-commit` / `drift`, takes
   `warm_samples` warm measurements (fresh `eg query` processes, one
   unmeasured priming run per sample), capturing the full answer each
   time.
3. Checks every answer against its fixture expectation (record ID +
   file/span or commit handle), checks the 5 answers are byte-identical
   after canonical ordering, and gates warm p95 against the hard budget.
4. Times the boring substitutes (`rg`, `git grep`, `git show`) with
   explicit comparability notes.
5. Prints the JSON report: environment context (OS, CPU class, Rust
   profile, Egregore version, corpus name, record counts, store kind),
   setup phases, per-class warm cells with the checked answers, the
   substitutes, and the gate result. Exits non-zero unless every class
   met its budget with a correct, deterministic, citable answer.

## The documented `eg` workflow (no hosted indexing)

Everything the gate measures can be reproduced by hand, locally, with no
hosted indexing, crawling, or mandatory remote embeddings:

```bash
eg scan corpus/query_latency_corpus --out graph.jsonl
eg scan-history /path/to/repo --out history.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
eg query symbol RepositoryIndex --graph graph.jsonl
eg query file symbols.rs --graph graph.jsonl
eg query symbol RepositoryIndex --at <sha> --graph history.jsonl
eg query drift --limit 10 --graph graph.jsonl
```

## CI wiring

The gate is enforced two ways:

1. `tests/integration/symbol_latency.rs`, which runs in the
   `cargo test --all-targets` matrix. It uses a cheap profile
   (`--warm-samples 2 --history-commits 3`) and asserts gate mechanics:
   an unmeetable `--budget-p95-ms` genuinely fails the gate (exit 1 with
   the stable `symbol_latency_budget_exceeded` diagnostic, not a rubber
   stamp), the report schema is complete (environment, setup phases,
   substitutes, per-class cells), and bad arguments fail fast with the
   JSON error envelope.
2. The dedicated `symbol-latency` CI job (`.github/workflows/ci.yml`),
   which builds once and runs the complete manifest-default gate —
   `./target/debug/egregore audit symbol-latency` — on the reference
   machine class. This is the authoritative product-gate enforcement on
   every push/PR.

## When the gate fails

1. Read the diagnostic on stderr: it names the query class and the
   observed p95 (`symbol_latency_budget_exceeded`), the answer error
   (`symbol_latency_answer_check_failed`), or the determinism failure
   (`symbol_latency_answer_unstable`).
2. Check whether the p95 moved for all classes (a general slowdown —
   suspect the store-load path) or one class (suspect that query's
   filter/format path).
3. `git stash` / bisect: the corpus is deterministic, so a p95 move across
   commits is attributable.
4. Do **not** "fix" the gate by raising `--budget-p95-ms` without a
   product decision — the 2s warm-p95 budget is the contract agents rely
   on. This gate is the measurement slice; the optimization slice
   (indexing, lazy load, caching) is explicitly out of scope for #57 and
   must satisfy this gate when it lands.

## Measured (reference run)

First full manifest-default run, 2026-09-30, dev profile on the sandbox
VM (not the reference GitHub Actions runner — hardware differs, so treat
absolute numbers as indicative, not canonical). The gate **failed**:
`symbol-at-commit` missed the 2s hard budget.

| Query class | Warm p50 | Warm p95 | Answer rows | Deterministic | Within budget |
|---|---|---|---|---|---|
| symbol | 585ms | 786ms | 1 | yes | yes |
| file | 689ms | 882ms | 5 | yes | yes |
| symbol-at-commit | 14,036ms | 16,118ms | 1 | yes | **no** |
| drift | 434ms | 448ms | 10 | yes | yes |

The `symbol-at-commit` miss is a product finding, not a gate bug: the
query loads the entire 25-commit history store (264,022 records, 290MB
JSONL) per invocation, and debug-mode deserialization alone costs
15–20s. The answer was correct and deterministic — only the latency
missed. Setup phases from the same run: cold-scan 3.1s, history-replay
116s (25 commits), ingest skipped (embedded-adapter WAL flush I/O error —
a pre-existing product gap, honestly reported), embedding-setup skipped
(no measured query needs embeddings). Boring substitutes: `rg` 6ms,
`git grep` 2ms, `git show <sha>:<file>` 2ms — all far faster than the
graph query, but none returns citable record IDs.

Setup phases and boring-substitute timings from the same run are recorded
in the report JSON; notable rows are summarized above.
