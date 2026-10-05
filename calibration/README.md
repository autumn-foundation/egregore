# Semantic confidence calibration (issue #221)

This directory holds the recalibration procedure for the
`eg query semantic` confidence thresholds
(`SEMANTIC_CONFIDENT_THRESHOLD`, `SEMANTIC_WEAK_THRESHOLD` in
`src/semantic_confidence.rs`, pinned in
`corpus/semantic_confidence_fixture.json`).

## What was measured (2026-09-23; supersedes the flawed 2026-09-22 run)

The thresholds were set from real score distributions produced by the pinned
embedding model (`sentence-transformers/all-MiniLM-L6-v2`) against an
embedded checkout of the egregore repo itself:

- **Answerable class (27 queries):** every non-ambiguous query in
  `corpus/semantic_relevance_corpus.json` (architecture, concept_absent,
  synonym_heavy, persistence_query, error_handling).
- **Unanswerable class (20 queries):** the 5 ambiguous corpus queries
  (`q026`, `q027`, `q029`, `q031`, `q032`, which have no expected targets)
  plus the 15 held-out queries in
  `corpus/semantic_confidence_unanswerable.json`, each grep-reviewed as a
  concept absent from the codebase.

For each query, the max cosine similarity was recorded (via
`sentence-transformers` with the exact `candidate_text` format from
`src/embeddings.rs`). Acceptance: ≥90% of the unanswerable class lands in
`abstain`|`weak` (best score < confident threshold) and ≥85% of the
answerable class lands in `confident` (best score ≥ confident threshold).

**Measured 2026-09-23** (full-repo graph: 15,098 File/Symbol candidates —
the exact `candidate_from_record` population in `src/embeddings.rs` —
embedded with all-MiniLM-L6-v2 via `sentence-transformers` using the exact
`candidate_text` format):

- With confident ≥ 0.39, weak ≥ 0.34: 24/27 answerable (88.9%) confident,
  19/20 unanswerable (95.0%) weak/abstain — both bars met.
- The candidate 0.55/0.50 thresholds were rejected: only 3/27 answerable
  (11%) scored ≥ 0.55.

This supersedes the **flawed 2026-09-22 measurement**, which embedded every
node with non-empty text (4,501 nodes, including `Diagnostics` and
`PanicRiskSites` nodes that are never retrieval candidates) and omitted
`src/cli/mod.rs`; it claimed 23/27 answerable (85.2%) confident and 18/20
unanswerable (90%) weak/abstain. The thresholds (0.39/0.34) stand — the
fixed-population re-run confirms them.

The measured run's numbers are recorded in the `derivation` note of
`corpus/semantic_confidence_fixture.json`. The fixed-population calibration
script is `calibration/py_calibrate_fixed.py`; it writes
`py-fixed-scores.json` (per-query best scores) and `py-fixed-top3.json`
(top-3 hits per query).

## Re-running

```sh
EG=./target/debug/egregore
CALIB=/tmp/calib-221
mkdir -p "$CALIB"
$EG scan /path/to/egregore --out "$CALIB/graph.jsonl"
$EG ingest "$CALIB/graph.jsonl" --adapter embedded \
    --data-dir "$CALIB/store" --embed
cd "$CALIB"
python3 /path/to/egregore/calibration/calibrate.py \
    "$EG" "$CALIB/store" \
    /path/to/egregore/corpus/semantic_relevance_corpus.json \
    /path/to/egregore/corpus/semantic_confidence_unanswerable.json
```

Notes:

- Embedding the full repo takes a few minutes on an 8-core box; the model
  (~90MB) downloads once into the Hugging Face cache.
- If the re-run's distributions no longer meet the acceptance bars, the
  thresholds must move **and** `corpus/semantic_confidence_fixture.json`,
  `src/semantic_confidence.rs`, and the docs updated together — the
  integration tests pin the fixture values and will fail otherwise.
- Re-running against a later tree must re-verify the held-out set's absence
  review (see the `review_note` in
  `corpus/semantic_confidence_unanswerable.json`): a concept that lands in
  the codebase invalidates that query as a negative.
