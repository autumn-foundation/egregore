# Semantic Code Search Guidance

## When to Use `eg query semantic`

Use semantic search when you need to find code by **meaning** rather than by exact names — when you know the concept but not the symbol or file name.

### Good use cases

- **Concept terms absent from symbol names**: "where does the tool measure similarity between code versions" finds `cosine_distance` even though "measure similarity" appears nowhere in the function name.
- **Synonym-heavy queries**: "storage backend write path" finds the adapter module even though the code says "ingest" and "sink".
- **File-level architecture questions**: "which module defines the graph intermediate representation" finds `src/ir.rs` without knowing to look for `GraphRecord`.
- **Error-handling paths**: "what error is returned when the embedded store does not exist" finds the validation function without knowing its exact name.
- **Persistence and query paths**: "how to open the embedded store without a daemon lease" finds `open_unleased` even when you don't know the method name.

### When `rg` or `eg query symbol` is the better tool

Use `rg` (ripgrep) or `git grep` when:

- You **know the exact identifier** — `rg find_similar_by_embedding` is faster and more precise than semantic search for an exact method name.
- You need **all call sites** — semantic search returns conceptually similar records, not every occurrence.
- You are **verifying correctness** — `rg` gives you the literal text, not a ranked approximation.
- The **keyword is unambiguous** — if `rg tombstone` returns exactly what you need, use it.

Use `eg query symbol <name>` when:

- You know the symbol name and want the graph record handle and provenance.
- You need the span (line range) for a specific function or struct.

## How to Run the Relevance Corpus

### Prerequisites

1. Build the embedded store with embeddings:
   ```
   cargo run -- scan . --out graph.jsonl
   cargo run -- ingest graph.jsonl --adapter embedded --data-dir .egregore --embed
   ```
   A pre-primed local model cache for `sentence-transformers/all-MiniLM-L6-v2` is required. The model is downloaded from Hugging Face on first use and cached locally. No remote repository crawling or background embedding workers are used during evaluation.

2. Run the evaluation against the checked-in corpus:
   ```
   cargo run -- eval-semantic corpus/semantic_relevance_corpus.json --data-dir .egregore
   ```

### Interpreting results

The report shows per-query results (HIT@1, HIT@3, or MISS) and aggregate metrics:

- **Top-1 accuracy**: fraction of labeled queries where the expected target appeared at rank 1.
- **Top-3 recall**: fraction of labeled queries where the expected target appeared in the top 3 results. The success threshold is **80%**.
- **Mean reciprocal rank (MRR)**: average of 1/rank for each hit (higher = better).
- **False-positive count**: number of ambiguous queries that returned any result (these have no correct answer; results are retrieval risk, not retrieval success).

If top-3 recall drops below 80%, the command exits 1 with a diagnostic listing the missed query IDs and observed top results.

### Boring substitute comparison

The corpus records an `rg_substitute` formulation for every labeled query. The `rg_can_answer_semantic_intent` field states whether `rg` can retrieve the expected target for that query. Most concept-absent and synonym-heavy queries cannot be answered by `rg` without prior knowledge of the identifier — that is where semantic search provides value.

To run the `rg` baseline manually:
```sh
rg '<keyword>' --type rust   # compare against the semantic result
```

## Important Caveats

**Semantic similarity is a retrieval lead, not verification evidence.**

A high similarity score means the embedding model judged the query text and the candidate text to be semantically close. It does **not** prove:

- That the retrieved file or symbol is correct for your task.
- That the code does what you expect.
- That the behaviour is unchanged since the last scan.

Always confirm retrieved handles using `eg query context <symbol>` or by reading the source directly. Use `eg query symbol` for exact navigation and verification evidence from `eg query context` for trust-separated facts.

## Corpus File

The relevance corpus lives at `corpus/semantic_relevance_corpus.json`. It contains 30 natural-language queries across six classes:

| Class | Description |
|---|---|
| `concept_absent` | Concept terms absent from the symbol name |
| `synonym_heavy` | Queries using different vocabulary than the code |
| `architecture` | File-level architectural questions |
| `error_handling` | Error-handling and failure-path queries |
| `persistence_query` | Database, ingest, and query-path questions |
| `ambiguous` | No clear correct answer (false-positive risk) |

The 24 labeled queries each have one or more reviewed expected targets as repo-relative file paths and optional symbol names. The 6 ambiguous queries represent concepts not present in the codebase (no authentication, no HTTP router, no GUI) — they are used to measure the false-positive rate.
