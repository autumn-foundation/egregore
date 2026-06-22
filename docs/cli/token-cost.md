# eg audit token-cost

Measure the **token cost of an Egregore query answer against the ripgrep
baseline** for a fixed question set — answer the maintainer question *"is `eg`
actually cheaper in tokens than dumping `rg` output into an agent, or is the
README's saving a slogan?"* — over a pinned, representative Rust corpus.
Local-first and offline; the grep baseline is computed in-process (no ripgrep
dependency), with no network access, hosted indexing, remote crawling, or
mandatory remote embeddings (issue #84).

> **A passing gate measures answer *size*, not answer *truth*.** Correctness is
> held constant: a measured Egregore answer counts only if it carries the
> expected record ID plus a repo-relative file/span or commit handle. A cheap
> answer that omits the citable handle is reported as a **miss**, never a win.
> Relevance calibration (#58) and citation completeness (#65) are separate gates.

This is a **measurement gate** layered over the existing `eg query symbol`,
`eg query file`, and `eg query semantic` answer shapes. It introduces no new
graph domain, node kind, edge label, query semantics, importer, language
coverage, or output-schema change.

## Synopsis

```text
eg audit token-cost [--corpus <PATH>] [--min-ratio <F>] [--format json]
```

* `--corpus <PATH>` — token-cost corpus manifest JSON. Default:
  `corpus/token_cost_corpus.json`.
* `--min-ratio <F>` — minimum baseline-to-Egregore savings ratio each question
  class (and the aggregate) must meet. Defaults to the manifest's `min_ratio`
  (3.0). When supplied, overrides it.
* `--format json` — output format (JSON only; `text` aliases to JSON).

## Shortest local workflow

```sh
eg audit token-cost
echo "exit: $?"   # 0 = gate passed, 1 = a class/aggregate regressed, 2 = load error
```

Exit codes: `0` gate passed (`ok: true`), `1` gate failed (`ok: false`; the full
JSON report is still printed to stdout so it is consumable), `2` usage/load
error (bad manifest path, unparseable corpus, unsupported token-count method, or
scan error).

## How it works

For each question in the manifest the gate:

1. **Builds the Egregore answer** by scanning the pinned corpus into a
   deterministic in-memory graph (a fixed repository-id override and scan time
   keep record IDs byte-stable across checkouts) and serializing the rows
   exactly as `eg query symbol` / `eg query file` / `eg query semantic` print
   them. The measured cost **is** the returned answer.
2. **Builds the grep-shaped baseline** in-process — the matching lines (or, for
   a file-defines question, the file's lines) a model would have to ingest to
   answer the same question via `rg`/`git grep` — and records the exact
   equivalent `rg` command. The gate never shells out, so it does not depend on
   ripgrep being installed and stays deterministic.
3. **Counts tokens on both sides with one pinned method** (`word-punct-v1`,
   below) and reports the `baseline ÷ egregore` ratio per question and in
   aggregate, alongside the raw counts.
4. **Gates on correctness and savings**: a question passes only when its answer
   carries the expected record ID plus a file/span (or commit) handle **and**
   the ratio meets the threshold.

### The pinned token-count method (`word-punct-v1`)

Each maximal run of `[A-Za-z0-9_]` counts as one token; each other non-whitespace
character counts as one token; whitespace separates tokens and is not itself
counted. The method is dependency-free, deterministic, and an approximation of
subword tokenization. The absolute count is approximate, but the **same method
is applied to the Egregore answer and the grep baseline**, so the ratio is
reproducible. The method name and description are emitted in every report.

## Question classes

The default corpus covers the three classes the issue requires:

| Class | Egregore answer | Grep-shaped baseline |
|-------|-----------------|----------------------|
| `exact_symbol` | `eg query symbol <name>` | `rg -n --word-regexp <name> <corpus>` — every line containing the identifier, including comment and string-literal false positives. |
| `file_defines` | `eg query file <path>` | `rg -n . <file>` — the file's lines, since reading the file is the boring substitute for "what does this file define". |
| `semantic` | `eg query semantic <text>` | `rg -n --word-regexp <keyword> <corpus>` — the lines a concept search by keyword drags in. |

The `semantic` answer's float `score` is embedding-derived; to keep the gate
deterministic and offline it is reported at a fixed representative value. The
token cost of a semantic answer is dominated by its citable handle, not the
score, so this does not move the ratio.

## Report shape

```json
{
  "ok": true,
  "corpus": { "name": "...", "source_file_count": 4, "record_count": 61, "store_kind": "jsonl" },
  "egregore_version": "0.1.0",
  "token_count_method": "word-punct-v1",
  "min_ratio": 3.0,
  "questions": [
    {
      "id": "q1-exact-symbol",
      "class": "exact_symbol",
      "baseline_command": "rg -n --word-regexp 'parse_config' corpus/token_cost_corpus",
      "baseline_match_count": 25,
      "baseline_tokens": 459,
      "egregore_row_count": 1,
      "egregore_tokens": 104,
      "ratio": 4.75,
      "expected_record_id": "codegraph:v4:...",
      "answer_has_expected_record_id": true,
      "answer_has_file_span_or_commit_handle": true,
      "correctness": "cited",
      "meets_threshold": true,
      "pass": true
    }
  ],
  "aggregate": { "baseline_tokens": 0, "egregore_tokens": 0, "ratio": 4.04, "meets_threshold": true, "all_answers_cited": true },
  "diagnostics": []
}
```

Every report records enough non-sensitive context to interpret the result: the
corpus name, source-file and record counts, store kind, token-count method and
description, Egregore version, the minimum ratio in effect, and the exact
baseline command per question. Output is redaction-safe — it carries record IDs,
handles, commands, and counts only, never raw payloads.

### Diagnostics

| Code | Meaning |
|------|---------|
| `uncited_answer_miss` | An answer lacked the expected record ID plus a file/span or commit handle; it is a miss, not a win. |
| `below_token_savings_threshold` | A question class's measured ratio fell below the threshold; the diagnostic names the class and the observed ratio. |

A regression below threshold is never silently reported as success: the gate
exits `1` and the failing class is named in both the per-question `pass: false`
and a `below_token_savings_threshold` diagnostic.

## Determinism

Re-running the gate against the unchanged corpus produces byte-identical token
counts, ratios, ordering, and diagnostics across runs: the scan is pinned to a
fixed repository-id override and scan time, the baseline is computed in-process,
and the report is canonically ordered (questions in manifest order, diagnostics
by question id).

## The pinned corpus

`corpus/token_cost_corpus/` is a four-file Rust settings stack whose public
entry point is `parse_config`. The modules mention `parse_config` and `settings`
in doc comments and string literals on purpose, so a text search returns many
false-positive lines that a structural `eg query` answer does not — the exact
gap this gate measures. The manifest (`corpus/token_cost_corpus.json`) pins the
question set, the expected record ID per question, the baseline pattern, the
token-count method, and the minimum ratio. It is the documented-equivalent
representative fixture in the absence of #57's shared latency corpus.

## Scope

This slice measures answer **size**, assuming the answer is already correct and
cited. It does not measure latency (#57), relevance/threshold calibration (#58),
or citation completeness (#65); it prescribes no optimization (caching, index
layout, output trimming); and it adds no hosted dashboard, telemetry service,
new graph domain, query semantics, importer, language, or output schema.
