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
    [--limit <N>] [--verified-only] [--agent <AGENT_ID>] [--not-agent <AGENT_ID>] \
    [--supersession <exclude|include-but-flag>] [--format json|text]
eg query semantic-memory <QUERY> --data-dir <DIR> --collapse \
    [--collapse-mode <auto|embedding-cosine|normalized-text>] \
    [--similarity-threshold <F>] [--limit <N>] [--format json|text]
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
| `--agent <AGENT_ID>` | Restrict recall to observations authored by this agent identity (see "Author scoping"). |
| `--not-agent <AGENT_ID>` | Exclude observations authored by this agent identity (see "Author scoping"). |
| `--supersession <mode>` | `exclude` (default) or `include-but-flag` (see [recall-supersession.md](recall-supersession.md)). |
| `--format` | `json` (default) or `text`. |
| `--collapse` | Collapse near-duplicate observations: one representative row per cluster, preceded by an envelope naming the requested/actual mode, threshold, and source/representative counts (issue #163; see below). |
| `--collapse-mode <mode>` | `auto` (default), `embedding-cosine`, or `normalized-text`. |
| `--similarity-threshold <F>` | Cosine-similarity threshold in `[0.0, 1.0]` (default `0.85`, echoed in the envelope); ignored by normalized-text equality clustering. |

| Condition | Exit |
|-----------|------|
| Results found | `0` |
| No memory recalled (no embeddings, or all hits filtered) | `2` |
| Query embedder does not match the indexed embedding model (issue #104) | `7` / `8` / `9` / `10` |

Memory recall reads the **same shared vector index** code search does, so it
carries the same cross-vector-space hazard and applies the same
[vector-space compatibility gate](semantic-index-identity.md): a mismatched
embedder is refused with a stable machine-readable envelope on stdout and a
distinct nonzero exit code, before the query is embedded and before any row is
returned.

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

### Author scoping (issue #195)

In multi-agent deployments many agents write observations into one shared
store. The author selector scopes recall by the authoring agent identity:

```sh
# Only what agent_1 concluded about the parser:
eg query semantic-memory "what breaks when the input file is empty?" \
  --data-dir .egregore --agent agent_1

# Everything except agent_1's observations:
eg query semantic-memory "what breaks when the input file is empty?" \
  --data-dir .egregore --not-agent agent_1
```

- The selector is spelled `--agent <AGENT_ID>` (include) and
  `--not-agent <AGENT_ID>` (exclude). Both may be given: a recalled
  observation must satisfy both, so when both name the same agent the
  exclusion wins.
- Every recalled observation already carries its author as first-class
  answer fields: **`agent_id`** and **`session_id`** on each row (never
  buried inside raw provenance).
- Matching is exact, case-sensitive equality on the `agent_id` handle.
- Composes with `--repo`, `--verified-only`, and `--supersession`; with no
  selector recall is unscoped (default behavior unchanged).
- Deterministic code-graph facts carry no `agent_id` and are never returned
  by an author-scoped recall.
- When the selector matches no observations the answer is an explicit empty
  result — exit `0`, not an error and not a silent fallback to unscoped
  recall:

```json
{
  "ok": true,
  "query": "what breaks when the input file is empty?",
  "results": [],
  "author_scope": {
    "agent": "agent_9",
    "not_agent": null,
    "author_field": "agent_id",
    "observations_matched": 0
  },
  "message": "no observations matched the author selector"
}
```

`author_scope.author_field` names the row field carrying the author
(`agent_id`); `agent` / `not_agent` echo the selectors that were applied.

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

## Collapsing near-duplicate observations (issue #163)

When several sessions re-learn the same lesson, plain recall returns every
restatement. `--collapse` groups near-duplicates into clusters and returns one
representative row per cluster:

```sh
eg query semantic-memory "what did past sessions learn?" \
  --data-dir .egregore --collapse
```

The answer is an envelope line followed by one JSON row per representative
(`--limit` bounds representatives, not source rows):

```jsonc
{"ok": true, "query": "...", "collapse": {"enabled": true, "mode": "embedding-cosine",
  "mode_requested": "auto", "similarity_threshold": 0.85,
  "threshold_monotonicity": "raising the threshold monotonically refines the partition …",
  "source_records": 25, "representatives": 5, "collapsed_away": 20}}
{"record_id": "…", "kind": "Observation", "trust_class": "agent_authored",
  "representative_trust_class": "agent_verified", "cluster_size": 5,
  "member_ids": ["…", "…"], "cluster_observed_at_min": "…",
  "cluster_observed_at_max": "…", "trust_spread": {"agent_verified": 2, "agent_unverified": 3},
  "primary_cited_target": {"record_id": "…", "relation": "OBSERVES"}, "memory_text": "…"}
```

- **Eligibility**: agent-authored observation-class records sharing the same
  primary cited code target (`OBSERVES` / `MENTIONS_SYMBOL`, inline links and
  standalone edges; `as_of_commit` is part of the identity). Target-less
  records stay singletons; observations never merge with deterministic code
  facts.
- **Modes**: `embedding-cosine` reuses stored vectors (missing vectors fail
  closed to singletons); `normalized-text` clusters on normalized stored-text
  equality without loading a model. `auto` picks `embedding-cosine` when the
  store has a vector index and degrades to `normalized-text` on a store that
  was never embedded; a damaged index is refused, not degraded over.
- **Representative**: highest confidence, then earliest `observed_at`, then
  smallest record ID — a real stored record, provenance intact.
- **Read-only**: collapse never writes; the store is untouched. This is
  presentation, not persistence. It differs from the adjacent recall
  surfaces on purpose: #94 (composition-health measurement) measures and
  explicitly never collapses; #131 (budget-fit packing) fits an answer to a
  token budget but collapses nothing; #92 (supersession flagging) flags
  superseded/contradicted records while `--collapse` groups the recalled
  ones. A cluster may mix trust classes and reports that mix in
  `trust_spread`.

## Scope

This slice reuses the existing semantic ingest/query machinery, agent-memory
schema, evidence-link contract, and redaction policy. It introduces no new
embedding provider, graph domain, importer, daemon verb, MCP tool, or hosted
service, and adds no network access. It is recall-only over already-ingested
memory; appending observations in-session is a separate slice.
