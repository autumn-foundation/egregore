# Semantic Code Search Guidance

## When to Use `eg query semantic`

Use semantic search when you need to find code by **meaning** rather than by exact names — when you know the concept but not the symbol or file name.

## Embedded vs. Daemon-Backed (`--daemon`)

`eg query semantic` has two transports over the **same** ranking behavior:

- **Embedded (default):** `eg query semantic "<query>" --data-dir .egregore` opens the store directly. Best for one-off, exclusive work where no daemon is running.
- **Daemon-backed:** `eg query semantic "<query>" --daemon --data-dir .egregore` routes the query through the running Egregore daemon. Prefer this in **multi-agent** operation: the daemon is the one shared local owner of the store, so the query honors daemon discovery, token checks, and a consistent snapshot instead of bypassing them with a direct read.

Both embed the query text locally with the same model and run the same vector search, so for a fixed store and query they return the **same top-k record IDs in the same order** (scores agree within a tight tolerance). The daemon path sends only the resulting query vector to the daemon — no embedding model is loaded daemon-side, no remote service is contacted, and there is no background indexing.

Daemon-backed results are still **retrieval leads, not proof**: each row carries a `record_id`, `score`, `repo_relative_path`, and `span` (omitted when the node has none), and nothing else. They are not verification evidence, task completion, source truth beyond deterministic code facts, or agent memory.

Stable diagnostics make failures actionable rather than silent: a missing daemon or stale runtime metadata is reported by daemon discovery before the query runs; an un-embedded store returns `missing_semantic_index`; a store whose index exists on disk but was skipped at load as corrupted returns the distinct `semantic_index_unreadable` (issue #489 — a damaged index is never reported as a never-built one); a mismatched vector returns `incompatible_embedding_dimension`; an empty result is a clean no-match, never a fallback to a direct embedded read. The full verb contract is in [`docs/schema/daemon-query.md`](../schema/daemon-query.md).

> **`--daemon` does not apply the vector-space compatibility gate (issue #104).**
> The daemon path checks vector *dimension* only, and two different models can
> share a dimension — so a store whose index was built by a different model
> returns a confident ranking over `--daemon` that the embedded lane would
> refuse. The daemon path prints a one-line stderr disclosure saying so. Daemon
> verb wiring is owned by #59/#53; until then, prefer the embedded lane when the
> answer's trustworthiness matters more than shared-store coordination, and see
> [`semantic-index-identity.md`](semantic-index-identity.md).

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

## Natural-language → evidence-backed context (`eg query semantic-context`, issue #90)

`eg query semantic` *locates* code by meaning but returns only ranked leads.
`eg query context <NAME>` *contextualizes* code but needs a symbol name you
already know — and it cannot start from a file-typed hit. `eg query
semantic-context` bridges the two: one read-only call takes a natural-language
query and returns, per top-N match, the stable record ID, the repo-relative
file/span handle, the relevance score, **and** the same five trust-separated
context sections produced by `eg query context` (`source_facts`,
`observations`, `project_state`, `artifacts`, `verification_evidence`, plus
`topology_edges` and `unresolved`).

### Shortest workflow

```sh
# one-time: build an embedded store with embeddings
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore --embed

# ask a question; get cited context for the top matches in one call
eg query semantic-context "where does the tool measure similarity between code versions" --data-dir .egregore
```

Useful flags:

- `--limit N` — number of matches to expand (bounded; **default 5**, smaller
  than `eg query semantic` because each match expands to a full context bundle).
- `--min-score F` — relevance floor in `[0.0, 1.0]` (**default 0.0**). Matches
  scoring below it are dropped; when *no* match clears the floor the command
  prints `{"ok":false,"error":{"code":"no_match",...}}` to stdout and exits `2`
  — never an empty success or a hallucinated fallback. This floor only gates the
  no-match diagnostic; it does **not** re-tune relevance (that is issue #58).

### How file vs symbol matches differ

Each match carries a `match_kind`:

- `symbol` — anchored on a `Symbol` node; `source_facts` include the symbol and
  its co-located file.
- `file` — anchored on a `File` node (no symbol name). File matches are
  first-class: the file plus the symbols it **defines** are seeded into
  `source_facts`, and observations / artifacts / verification attached to those
  are returned rather than the match being dropped.
- `other` — any other embeddable node kind.

When a match name resolves to more than one live symbol, `ambiguous` is `true`
and `candidate_record_ids` lists **every** candidate — the workflow surfaces the
ambiguity instead of silently picking one.

Every returned row carries a stable record ID and a citable handle; rows whose
evidence link has no resolvable target are surfaced under `unresolved` rather
than emitting uncited prose. The five context sections carry the same fields as
`eg query context` — including agent-facing summary text, observation text, and
`OutputHandle` inline content bounded by the ingest-time 16 KiB ceiling and
redaction policies applied at ingest. The answer is read-only and deterministic:
identical queries against an unchanged store return byte-identical matches,
sections, and ordering.

### When to use which tool

| You want… | Use |
|---|---|
| Code **and** what's known about it, from a vague question | **`eg query semantic-context`** |
| Just *locate* code by meaning (ranked leads, no context) | `eg query semantic` |
| Context when you **already know the symbol name** | `eg query context <NAME>` |
| Blast radius of a change from a known symbol | change-impact (issue #76) |
| Everything under a subsystem path prefix | subsystem scope (issue #83) |
| An exact identifier or all literal occurrences | `rg` / `git grep`, then read |
| Interactive navigation from a cursor in an editor | rust-analyzer / LSP |

`eg query semantic-context` is the right default when you do **not** know where
something lives and you want the cited context bundle, not just a location, and
not the whole file. It consumes the existing semantic ranking and symbol-context
contracts; it adds no new domain, schema, model, or LLM-generated answer.

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
