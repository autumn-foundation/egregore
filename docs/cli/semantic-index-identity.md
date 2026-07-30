# Semantic index identity — reading it, and what a refusal means

Issue #104. `eg query semantic` (and its `semantic-memory` / `semantic-context`
siblings) refuse to answer when the query embedder does not demonstrably share
the store's vector space.

## Why this exists

The semantic index stores dense vectors plus a dimension. **Two different models
can share a dimension** — several widely used sentence encoders are 384-dim — so
a dimension check alone cannot tell "same vector space" from "different vector
space that happens to be the same width".

Without an identity check, a model swap or a binary version bump yields a cosine
ranking computed **across incompatible vector spaces**, returned with a
confident-looking score attached. That is a guess masquerading as source truth,
and it fails *silently*, which is worse than refusing. `rg` cannot answer by
meaning, but it also never returns a confident ranking built on incompatible
math; the semantic lane only beats it if its answers are trustworthy by
construction.

So: an `--embed` write records **which model produced the index**, and a semantic
query proceeds only when the query embedder's identity matches it.

### What this gate does and does not detect

Be precise about the guarantee, because a compatibility gate that overstates
itself is its own kind of silent failure.

**Detected today:**

- A recorded identity differing from the query embedder on `provider`, `name`,
  `version`, or `content_hash` (exit `10`).
- Any dimension disagreement — against the physical vector index *and* against
  the vector the model actually returns (exit `9`).
- A legacy index with no recorded identity (exit `7`), and an index recording
  several distinct identities (exit `8`).

**NOT detected today:** a change in the model *weights* behind an unchanged
model name. `content_hash` is the literal string `unknown`, because the provider
boundary this crate uses does not expose model bytes to it. So a Hugging Face
cache that resolves the same model id to different weights produces a
**byte-identical identity tuple** and passes the gate. Closing that requires a
real weight hash from the provider boundary; until then, treat `content_hash` as
a reserved field, not a working defense.

Because the other three fields are compile-time constants of the producing
binary, the field that varies in practice is `version` — which is exactly the
"binary version skew" case, and is genuinely caught.

## What gets persisted

Every `--embed` write path — `eg ingest --embed`, `eg refresh --embed`, and the
`eg watch --embed` loop — writes one semantic-domain `EmbeddingModel` node
carrying the full [`EmbeddingModel`](../schema/semantic-drift.md) identity the
semantic-drift schema already defines:

| Field | Meaning |
|-------|---------|
| `provider` | Provider boundary that supplied the model. |
| `name` | Model name from the provider registry. |
| `version` | Producing binary's version pin. |
| `dim` | Dense vector dimensionality actually produced. |
| `content_hash` | BLAKE3 hash of model weights, or `unknown` when the provider does not expose them. **Always `unknown` today** — see "What this gate does and does not detect" above. |

No new node kind, edge label, or trust class is introduced: `EmbeddingModel` is
an existing kind in the existing `semantic` domain.

The record ID is **fixed**: a store has exactly one physical vector index, so it
has exactly one identity, and a later `--embed` write supersedes the earlier one
at the same ID. Deriving the ID from the identity tuple instead would let two
identity records coexist — and since `eg forget` refuses every semantic-domain
record and `eg forget-repo` never evicts a repo-agnostic one, that state would be
**unrecoverable**: every semantic query would refuse forever with no in-place
remedy. A fixed ID keeps every bad state fixable by re-running `--embed`.

What actually prevents a mixed-model index is the **write-time refusal**:
embedding into a store whose index was built by a *different* model is refused
before anything is written, with the stable code
`embedding_index_identity_conflict` and a nonzero exit. So the natural in-place
"fix" after a version bump (`eg refresh --embed`) tells you plainly that you need
a fresh store, instead of quietly leaving the index holding vectors from two
models and reporting success. Re-embedding with the **same** model is allowed and
idempotent.

The identity node is repo-agnostic (it describes the store's index, not a
repository), carries no edges, and is not an orphan-checked kind, so an
`eg export` of the store stays `eg validate`-clean.

## Reading the indexed model identity

```powershell
eg inspect --data-dir .egregore
```

The JSON report carries a `semantic_index` block:

```json
{
  "semantic_index": {
    "index_present": true,
    "index_dimensions": 384,
    "identity_recorded": true,
    "indexed_models": [
      {
        "provider": "aletheiadb_re_export",
        "name": "sentence-transformers/all-MiniLM-L6-v2",
        "version": "0.1.0",
        "dim": 384,
        "content_hash": "unknown"
      }
    ]
  }
}
```

`--format text` prints the same facts as `semantic index: 384-dimensional
vectors` followed by one `embedding model: …` line per recorded identity. The
block is deterministic: identities are deduplicated and sorted, so repeated
`eg inspect` runs over an unchanged store are byte-identical.

`identity_recorded: false` with `index_present: true` means a **legacy** store —
embedded before identity stamping. Its compatibility is unverifiable, and
semantic queries against it are refused (see below).

## Refusal outcomes

Every refusal prints a stable machine-readable envelope on **stdout**, a one-line
human summary on **stderr**, and exits with a distinct nonzero code. A refused
query returns **no ranked result rows** — the gate runs before the query is even
embedded, so a refusal costs no model load.

| Exit | `code` | Condition |
|------|--------|-----------|
| `7` | `embedding_identity_unrecorded` | The vector index exists but records no model identity (legacy store). Compatibility is **unverifiable** — never assumed compatible. |
| `8` | `embedding_identity_ambiguous` | The index records **more than one** distinct producing model, so its vectors span several spaces and no single ranking is meaningful. |
| `9` | `embedding_dimension_mismatch` | The index holds vectors of a different dimensionality than the query embedder produces. |
| `10` | `embedding_model_mismatch` | **Same dimension, different model** — the silent-failure case this gate exists for. |

Unchanged outcomes:

| Exit | Condition |
|------|-----------|
| `0` | Identities match; results are returned **exactly as before** — same ranking, scores, ordering, and output schema. |
| `2` | The store has no vector index at all (never `--embed`ed), or the search matched nothing. A store that was simply never embedded is *not* an identity failure, and reporting one would be a false refusal. |

The two exit-`2` cases are told apart by a stable code prefixing the stderr line
(`eg query` lanes leave stdout empty on exit `2`, so the code rides stderr):
`semantic_index_absent:` for a store that was never `--embed`ed,
`no_semantic_matches:` for an index that exists but matched nothing, and
`scoped_no_match:` for a valid `--under` prefix that selected nothing (issue
#198). "Never embedded" and "embedded but nothing matched" are different operator
problems and must not look alike.

Envelope shape for the same-dimension mismatch:

```json
{
  "ok": false,
  "error": {
    "code": "embedding_model_mismatch",
    "message": "the semantic index was produced by … but the query embedder is …",
    "remedy": "re-ingest the graph into a fresh --data-dir with `eg ingest <graph> --adapter embedded --data-dir <NEW_DIR> --embed` …",
    "indexed_model": { "provider": "…", "name": "…", "version": "…", "dim": 384, "content_hash": "…" },
    "query_model":   { "provider": "…", "name": "…", "version": "…", "dim": 384, "content_hash": "…" },
    "differing_fields": ["name"]
  }
}
```

`differing_fields` names exactly which identity fields disagree, in a fixed
declared order (`provider`, `name`, `version`, `content_hash`), so the diagnostic
is byte-identical across runs and the operator never has to guess *why*.

**Output is allow-list only**: provider, name, version, dimension, content hash,
dimensions, field labels, the stable code, and the remedy. Never model bytes,
never vectors, never indexed source text, and never the operator's query string.

## Re-ingesting when the model changes

The remedy is always **re-ingest**, never editing the store — the store is the
record of what was actually embedded, and mutating it would replace a detectable
incompatibility with a silent lie. Egregore never chooses, downloads, switches,
or auto-upgrades an embedding model, and never re-embeds a store on mismatch.

```powershell
# 1. See what the store was embedded with, and why the query was refused.
eg inspect --data-dir .egregore --format text

# 2. Re-ingest into a FRESH data dir with the current binary's embedder.
eg scan . --out graph.jsonl
eg ingest graph.jsonl --adapter embedded --data-dir .egregore-new --embed

# 3. Confirm the new store records the identity you expect.
eg inspect --data-dir .egregore-new

# 4. Query it.
eg query semantic "request timeout handling" --data-dir .egregore-new
```

A **fresh** data dir matters for a model change, and Egregore enforces it:
re-embedding in place would leave the prior model's vectors in the index
alongside the new ones, so `eg ingest --embed` / `eg refresh --embed` / the
`eg watch --embed` loop **refuse before writing** when the store's index was
built by a different model, with the stable code
`embedding_index_identity_conflict`:

```json
{"ok":false,"error":{
  "code":"embedding_index_identity_conflict",
  "message":"this store's semantic vector index was built by a different embedding model; …",
  "remedy":"re-ingest the graph into a fresh --data-dir …",
  "indexed_models":[{"provider":"…","name":"…","version":"…","dim":384,"content_hash":"…"}],
  "producing_model":{"provider":"…","name":"…","version":"…","dim":384,"content_hash":"…"}
}}
```

Nothing is written, so the store stays in the clean, actionable mismatch state
rather than becoming an un-rankable blend. A fresh dir is the only way to get one
clean vector space back — Egregore does not re-embed a store on mismatch.

### On the `version` field

`version` pins the **producing binary's** version. Per the issue's contract, *any*
identity field difference refuses — a version bump is explicitly one of the cases
this gate exists to catch, because a binary upgrade can change how the embedder is
built or which cached weights it resolves. The practical consequence: **upgrading
`eg` requires re-ingesting `--embed` stores** before semantic queries work again.
That is deliberate. Refusing costs a re-ingest; a silent cross-space ranking costs
an agent acting on a fabricated answer.

## Scope

- Applies to the local embedded lanes: `eg query semantic`,
  `eg query semantic-memory`, and `eg query semantic-context` — all three read the
  same shared vector index and carry the same hazard.
- **Not** wired into the daemon `semantic_search` verb or the MCP surface; those
  are owned by #59/#53.
- No relevance/threshold calibration (#58), drift-signal quality (#55), or latency
  gating (#57).
- No cross-model vector translation. Mismatched spaces are not made comparable —
  they are refused.

## Related

- `docs/cli/query.md` — the `eg query semantic` surface.
- `docs/cli/inspect.md` — the `semantic_index` report block.
- `docs/schema/semantic-drift.md` — the `EmbeddingModel` identity vocabulary.
- `docs/cli/semantic-search-guidance.md` — how to use semantic results.
