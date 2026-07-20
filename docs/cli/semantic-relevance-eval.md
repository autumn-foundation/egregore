# `eg audit semantic-relevance` — labeled relevance gate (issue #106)

Makes the "find code by **meaning**, not by name" claim falsifiable and
gate-able. Runs a labeled query corpus against an `--embed` embedded store,
computes standard IR retrieval metrics, and fails the build when relevance drops
below an agreed floor — so regressions are caught in CI, not in agent sessions.

This extends the issue #58 relevance harness (`src/semantic_eval.rs`,
`corpus/semantic_relevance_corpus.json`). The #58 metric functions (top-1/top-3
accuracy) are retained; this lane adds hit-rate@k (k = 1, 5, 10), recall, the
snapshot pin, and the enforced floor gate.

## Command

```powershell
eg audit semantic-relevance --data-dir .egregore
eg audit semantic-relevance --corpus corpus/semantic_relevance_corpus.json --data-dir .egregore
eg audit semantic-relevance --data-dir .egregore --min-hit-rate-5 0.95 --min-mrr 0.80
```

The store must have been ingested with embeddings (`--embed`). Only `File` and
`Symbol` nodes are scored; agent-memory nodes that share the vector index
(issue #91) are filtered out before scoring, exactly like `eg query semantic`.

### Flags

- `--corpus <path>` — labeled corpus JSON (default
  `corpus/semantic_relevance_corpus.json`).
- `--data-dir <dir>` — the embedded store to score against.
- `--min-hit-rate-5 <f>` — override the hit-rate@5 floor (default `0.90`).
- `--min-mrr <f>` — override the MRR floor (default `0.70`).
- `--fp-threshold <f>` — ambiguous-query false-positive score threshold
  (default `0.5`).
- `--top-k <n>` — retrieval depth kept per query (default `10`; must be `>= 10`
  to measure hit-rate@10).

### Exit codes

Mirrors `eg audit token-cost`:

- `0` — both floors met (`ok: true`).
- `1` — a floor was missed (`ok: false`); the full JSON report is still printed,
  with a machine-readable `breaches` list naming each failing metric and its
  observed value.
- `2` — usage/capability error: a bad `--min-*` value (non-finite or outside
  `[0.0, 1.0]`), an unreadable corpus or store, or the `embeddings` feature is
  unavailable. When the feature is off the command emits an honest
  `capability: "requires_embeddings_feature"` report and **never silently
  passes**.

## Metrics

Computed over the **labeled** (non-ambiguous) queries only:

- **hit-rate@k** — fraction of labeled queries whose expected target appears
  within the top `k` returned results. Reported for k = 1, 5, 10.
- **MRR** (mean reciprocal rank) — mean of `1 / rank` of the first correct hit
  (0 when the target is not returned).
- **recall** — fraction of labeled queries whose expected target appears
  anywhere in the returned candidate list (hit rate at the full retrieval
  depth).

Ambiguous queries never count as hits; they are used only to count
`false_positive_count` — an ambiguous query whose top result meets
`--fp-threshold`.

Every missed labeled query (expected target not in the top 5) is listed in
`misses` with its query text, expected handle(s), and the actual returned top
hits, so a failure is debuggable without re-running by hand.

## The floors, and how they were chosen

The gate is:

> **hit-rate@5 ≥ 0.90 AND MRR ≥ 0.70**

These are the issue #106 success metric verbatim. hit-rate@5 is the primary
"the right answer is on the first screen" signal; the MRR floor additionally
requires that correct answers rank *high*, not merely somewhere in the top 5.
Both are conventional floors for vector-RAG / BEIR-style qrel evaluation and are
deliberately set at the *current* achievable level: this slice only *measures*
the existing `all-MiniLM-L6-v2` store — raising the floor is a follow-up once the
floor exists. Record the baseline numbers on first landing so future drift is
measurable.

Overriding `--min-hit-rate-5` / `--min-mrr` changes the gate for one run; the
committed defaults are the CI contract.

## Determinism

The report is deterministic. The pure metric + gate harness produces
**byte-identical** JSON across runs on the same rankings. Where f32 embedding
scores are involved (the store-backed path), the same store reproduces metric
values within a tolerance of **`1e-5`** (`RELEVANCE_DETERMINISM_TOLERANCE`),
consistent with the semantic-drift replay tolerance. Results are compared with
that tolerance; `relevance_metrics_within_tolerance` is the shared comparison.

## Corpus format

`corpus/semantic_relevance_corpus.json`:

```json
{
  "corpus_version": "1",
  "description": "…",
  "source_snapshot": {
    "commit": "68fdf380850e3c352af9c1d30f88c58dbbf4c552",
    "note": "trunk HEAD the expected targets were authored against"
  },
  "queries": [
    {
      "id": "q001",
      "text": "where are graph records written to the embedded store",
      "class": "concept_absent",
      "expected": [
        {
          "repo_relative_path": "src/adapters/aletheiadb.rs",
          "symbol_name": null,
          "note": "why this is the correct target"
        }
      ],
      "rg_substitute": {
        "keyword": "write_node",
        "note": "what rg can and cannot answer",
        "rg_can_answer_semantic_intent": false
      }
    }
  ]
}
```

Fields:

- `corpus_version` — corpus schema version.
- `description` — human-readable scope.
- `source_snapshot` — **the pinned source snapshot** (issue #106). `commit` is
  the SHA the reviewed `expected` targets were authored against (or a marker such
  as `"fixture"` for a synthetic fixture not tied to a checkout). This keeps the
  corpus valid and deterministic: the paths/spans/symbol names in `expected` are
  only guaranteed to resolve against that snapshot.
- `queries[]`:
  - `id` — stable unique identifier.
  - `text` — the natural-language query embedded and searched.
  - `class` — one of `concept_absent`, `synonym_heavy`, `architecture`,
    `error_handling`, `persistence_query`, `ambiguous` (for stratified
    reporting; `ambiguous` queries have no correct answer and count only false
    positives).
  - `expected[]` — reviewed correct handles: `repo_relative_path` plus optional
    `symbol_name` (a file-level target with `symbol_name: null` matches any hit
    from that file). Empty for `ambiguous` queries.
  - `rg_substitute` — the "boring substitute" `rg`/`git grep` formulation and
    whether it can answer the semantic intent (documentation of the precision
    win over grep). Required on labeled queries.

## How to add a query

1. Pick the **query text** an agent would actually type — natural language, not
   a symbol name. Prefer classes where grep is weak (`concept_absent`,
   `synonym_heavy`).
2. Find the **reviewed correct target(s)** in the current tree. Record each as
   `repo_relative_path` (+ `symbol_name` when a specific symbol is meant). Add a
   `note` explaining why it is correct.
3. Add an `rg_substitute` with the keyword you would grep for and an honest note
   on whether grep answers the intent.
4. Give the query a unique `id` and the right `class`.
5. If the target's file/symbol was added since the pinned `source_snapshot.commit`,
   **re-pin the snapshot** to a commit where every expected target exists and
   **re-validate the floors** (`eg audit semantic-relevance --data-dir <store>`
   against a store built at that snapshot). Re-pinning invalidates the reviewed
   targets, so the floors must be re-confirmed.

Because the corpus is version-pinned and the metrics are deterministic, the
committed corpus is byte-stable and regression-gated.

## What this is and is not

- Measures **retrieval relevance** — distinct from issue #84 (query token cost
  vs ripgrep) and issue #93 (parser extraction accuracy).
- A semantic hit is a **retrieval lead**, never verification evidence. Semantic
  similarity is not proof of correctness.
- Rust-only, local-first, offline. No model tuning, no remote corpora (out of
  scope per the issue).
