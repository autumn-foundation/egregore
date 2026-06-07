# Semantic Code Search Guidance

## When to Use `eg query semantic`

Use semantic search when you need to find code by **meaning** rather than by exact names — when you know the concept but not the symbol or file name.

## Embedded vs. Daemon-Backed (`--daemon`)

`eg query semantic` has two transports over the **same** ranking behavior:

- **Embedded (default):** `eg query semantic "<query>" --data-dir .egregore` opens the store directly. Best for one-off, exclusive work where no daemon is running.
- **Daemon-backed:** `eg query semantic "<query>" --daemon --data-dir .egregore` routes the query through the running Egregore daemon. Prefer this in **multi-agent** operation: the daemon is the one shared local owner of the store, so the query honors daemon discovery, token checks, and a consistent snapshot instead of bypassing them with a direct read.

Both embed the query text locally with the same model and run the same vector search, so for a fixed store and query they return the **same top-k record IDs in the same order** (scores agree within a tight tolerance). The daemon path sends only the resulting query vector to the daemon — no embedding model is loaded daemon-side, no remote service is contacted, and there is no background indexing.

Daemon-backed results are still **retrieval leads, not proof**: each row carries a `record_id`, `score`, `repo_relative_path`, and `span` (omitted when the node has none), and nothing else. They are not verification evidence, task completion, source truth beyond deterministic code facts, or agent memory.

Stable diagnostics make failures actionable rather than silent: a missing daemon or stale runtime metadata is reported by daemon discovery before the query runs; an un-embedded store returns `missing_semantic_index`; a mismatched vector returns `incompatible_embedding_dimension`; an empty result is a clean no-match, never a fallback to a direct embedded read. The full verb contract is in [`docs/schema/daemon-query.md`](../schema/daemon-query.md).

### Relationship to issue #58 (relevance gate)

This workflow makes daemon-backed semantic search **available and deterministic** — it does not decide whether the results are *good enough to trust*. Issue **#58** owns relevance calibration: the checked-in corpus and the `eg eval-semantic` top-3 recall gate measure retrieval quality. Use the corpus gate to judge accuracy; use this guidance to choose the transport and to remember that a high score is a lead to confirm, not an answer.

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

Use `eg query file <path>` when:

- You know the file and want every symbol it defines, with record handles — a structural listing, not a ranked approximation.

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
