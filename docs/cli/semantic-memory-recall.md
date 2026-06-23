# eg query semantic-memory

Recall **prior agent memory by meaning** — answer the question *"what did a past
session learn that is relevant to what I am doing now?"* — from a
natural-language query, without already knowing which symbol the lesson was
attached to. Local-first; no network access.

> **A recalled memory is not source truth and not verification.** Every hit is a
> prior agent's subjective claim. A high semantic score means the lesson reads as
> relevant, never that it is correct or that the code actually works. Results are
> typed `agent_authored` and are kept strictly separate from deterministic code
> facts so a guess is never mistaken for a fact.

This closes the recall half of the memory loop (issue #91): Egregore already
ingests rich agent memory (`.traj` #9/#29, Codex JSONL #79) and embeds code by
meaning, but observation-class memory was excluded from the vector index, so a
lesson attached to no symbol was invisible to recall. This workflow embeds that
memory and recalls it by meaning, with provenance.

## How memory is indexed

Memory is embedded **into the existing store**, not a parallel index. The
`embedding_candidates` selection now includes agent-memory observation-class
nodes (`Observation`, `Decision`, `Failure`) alongside `File`/`Symbol`, embedding
each record's authored body text with the same local model cache. Trust
separation happens **at query time** by node kind:

- `eg query semantic` returns only deterministic **code** hits (`File`/`Symbol`).
- `eg query semantic-memory` returns only **agent-authored** memory hits.

Neither surface ever blends the two. No new embedding provider, graph domain,
daemon verb, or remote service is introduced.

### Indexing workflow

```sh
# 1. Import agent memory (already supported): .traj or Codex JSONL → graph JSONL
eg ingest trajectories.graph.jsonl --adapter embedded --data-dir .egregore --embed

# 2. Recall by meaning
eg query semantic-memory "what breaks when the input file is empty?" \
  --data-dir .egregore
```

`--embed` is required: the store must carry the vector index. Re-run ingest with
`--embed` if recall returns no results.

## Synopsis

```text
eg query semantic-memory <QUERY> --data-dir <DIR> [--repo <SELECTOR>] \
    [--limit <N>] [--verified-only] [--supersession <exclude|include-but-flag>] [--format json|text]
```

Reads directly from an embedded AletheiaDB store (`--data-dir`). The query string
is embedded locally with the default model; no remote embedding service is
contacted.

| Argument | Meaning |
|----------|---------|
| `<QUERY>` | Natural-language question to recall memory by meaning. |
| `--data-dir <DIR>` | Embedded store ingested with `--embed`. |
| `--repo <SELECTOR>` | Restrict results to one repository (issue #67). |
| `--limit <N>` | Maximum memory hits to return (default 10). |
| `--verified-only` | Exclude unverified observations (see below). |
| `--supersession <mode>` | `exclude` (default) or `include-but-flag` (see [recall-supersession.md](recall-supersession.md)). |
| `--format` | `json` (default) or `text`. |

| Condition | Exit |
|-----------|------|
| Results found | `0` |
| No memory recalled (no embeddings, or all hits filtered) | `2` |

## Response shape

Each row is one recalled memory record, typed `agent_authored`. A hit that
**cannot cite where it came from is excluded**, never returned with empty
provenance.

| Field | Meaning |
|-------|---------|
| `record_id` | Stable agent-memory record ID (`agent_memory:v1:…`). |
| `kind` | `Observation`, `Decision`, or `Failure`. |
| `trust_class` | Always `agent_authored`. |
| `retrieval_score` | Cosine similarity (higher = more similar). Recall signal only. |
| `source_handle` | Citable source transcript / session / turn handle. |
| `agent_id` / `agent_kind` | Authoring agent identity. |
| `session_id` | Authoring session handle. |
| `confidence` | Author-stated confidence in `[0.0, 1.0]`. |
| `observed_at` / `ingested_at` | When the agent observed / the store committed it. |
| `review_state` | `verified` when the claim cites present verification evidence, else `unverified`. |
| `redacted` | `true` when the record carries a redaction policy version. |
| `superseded_by` | Record ID that supersedes this one, when present. |
| `linked_code_handles` | Resolved code handles the memory cites (`OBSERVES`/`MENTIONS_SYMBOL`/…). |
| `memory_text` | The recalled lesson body (post-redaction stored text). |
| `repository_id` / `repository` | Owning repository handle, when known. |

### `--verified-only`

Excludes unverified agent observations. A memory is **verified** when it cites at
least one present verification-domain record through an evidence link
(`VALIDATED_BY`, `HAS_EVIDENCE`, `PRODUCED_EVIDENCE`) — the same structural,
non-inferential rule the [`eg query memory`](memory-audit.md) audit uses. This is
consistent with the PR-3 / PR-4 trust rules: an unverified claim is never
silently promoted to a fact.

### `--supersession`

Controls how superseded or contradicted observations are filtered or annotated. By default (`exclude`), they are excluded from the `observations` list and added to `excluded` diagnostics. With `include-but-flag`, they are returned alongside their temporal status and forward references. See [recall-supersession.md](recall-supersession.md) for full details.

### Example

```sh
eg query semantic-memory "which error crate should I use in a library?" \
  --data-dir .egregore --limit 3
```

```json
{
  "record_id": "agent_memory:v1:obs-thiserror-libraries",
  "kind": "Observation",
  "trust_class": "agent_authored",
  "retrieval_score": 0.82,
  "source_handle": "trajectories/run-1.traj",
  "agent_id": "agent_1",
  "agent_kind": "claude-code",
  "session_id": "sess_1",
  "confidence": "0.8",
  "observed_at": "2026-06-03T12:00:00Z",
  "ingested_at": "2026-06-03T12:00:01Z",
  "review_state": "unverified",
  "redacted": false,
  "memory_text": "Prefer thiserror over anyhow in library crates so callers can match on structured error variants."
}
```

This lesson is attached to **no symbol**, yet it is recalled by meaning — proving
recall does not depend on the #90 symbol-bridge path.

## Recall corpus and metric

A checked-in corpus of natural-language questions exercises recall quality:

```sh
eg eval-memory-recall \
  --corpus corpus/agent_memory_recall_corpus.json \
  --data-dir .egregore
```

- `corpus/agent_memory_recall_corpus.json` — ≥ 20 questions, each labeling one or
  more reviewed expected memory record IDs.
- `corpus/agent_memory_recall_seed.jsonl` — the memory records the corpus is
  scored against; ingest it with `--embed` to build the eval store.

The command embeds each question, recalls memory, and reports top-1 accuracy,
top-3 recall, and mean reciprocal rank, exiting non-zero below the threshold
(default 0.80 top-3 recall). Ties are canonical-ordered by record ID, so
re-running the unchanged corpus against the same store and model mode produces
equivalent ordered results and identical aggregate metrics.

## When to use this versus other tools

| Reach for | When you want |
|-----------|---------------|
| **`eg query semantic-memory`** (this) | Recall a prior lesson/decision/failure by **meaning**, with provenance. |
| `eg query semantic` (#59) | Find **code** by meaning. Never returns memory. |
| `eg query memory` (#64) | Audit the **evidence** behind one known memory claim. |
| `eg query context` (#38) | Evidence-backed context for a **symbol** you can name. |
| `rg` / `git grep` over a memory JSONL | Literal-text search — "panics on empty input" will not surface from "parser edge case", and the match carries no typed provenance or trust class. |

`rg` + `jq` over transcripts are honest substitutes for literal recall, but they
cannot retrieve by meaning, separate trust classes, or attach citable record and
transcript handles.

## Scope

This slice reuses the existing semantic ingest/query machinery, agent-memory
schema, evidence-link contract, and redaction policy. It introduces no new
embedding provider, graph domain, importer, daemon verb, MCP tool, or hosted
service, and adds no network access. It is recall-only over already-ingested
memory; appending observations in-session is a separate slice.
