# eg re-embed

Re-embed an `--embed` store under a different embedding model **without
re-scanning sources** (issue #167).

## Synopsis

```text
eg re-embed --data-dir <DIR> --model <MODEL> [--format json|text] [--dry-run]
```

`<MODEL>` is a local model directory, or a Hugging Face model id resolved
from the **local** HF cache only. Egregore never downloads a model as a side
effect of `re-embed`: a model that is not available locally refuses with
exit `12` and the store is untouched.

## What it does

Every node that already carries a persisted `embedding` vector — exactly the
previously-embedded set, no source scan, no graph re-extraction — is
re-embedded with the target model. The `#104` vector-index identity record
is superseded in the same transaction, so the store's recorded identity
always describes the vectors it actually holds. When the target dimension
differs from the stored one, the vector index is rebuilt from the replaced
vectors; the two vector spaces are never mixed.

After re-embedding, semantic queries must name the new model:

```text
eg query semantic "query text" --data-dir <DIR> --embed-model <MODEL>
```

A query with the default model (or any other model) against a re-embedded
store is still refused by the #104 compatibility gate — the refusal is
read-only and unchanged; `re-embed` is the explicit operator command that
moves the store.

## Report

`--format json` (the default) prints a machine-readable report:

```json
{
  "ok": true,
  "dry_run": false,
  "data_dir": "/path/to/store",
  "candidates": 128,
  "reembedded": 128,
  "skipped": 0,
  "failed": 0,
  "dimension_changed": false,
  "index_rebuilt": false,
  "previous_model": {"provider": "local", "name": "local/<slug>", "version": "<egregore-version>", "dim": 384, "content_hash": "blake3:<hex>"},
  "target_model": {"provider": "local", "name": "local/<slug>", "version": "<egregore-version>", "dim": 384, "content_hash": "blake3:<hex>"}
}
```

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0`  | Success (including a no-op rerun: `reembedded: 0`, `skipped: N`). |
| `1`  | Operational failure before the commit (e.g. a candidate's source text is no longer recoverable, or the model failed to load). The store is untouched — all vectors are computed before the single commit transaction opens. |
| `12` | `embedding_model_unavailable_locally`: the model is not available locally. Nothing was downloaded, nothing was written. |
| `13` | `--dry-run` with work remaining: the plan is printed, the store is byte-identical. |
| `14` | `reembed_nothing_to_migrate`: the store carries no embedding-model identity or no embedded nodes — ingest it with `--embed` first. |

## Failure semantics

Re-embed is atomic-or-explicit: either the commit transaction replaces every
vector and the identity together, or the store still needs re-embedding. The
dimension-change path has two interruption points, both with explicit,
stable states:

- **Crash between the old-index deletion and the vector commit**: identity A
  over A-vectors with no usable index — still a model-A store, just
  index-less. Queries report the documented `semantic_index_absent`
  diagnostic (exit `2`); re-running `eg re-embed --model B` completes the
  migration from this state.
- **Crash between the vector commit and the index rebuild**: identity B over
  B-vectors with no usable index. Queries report `semantic_index_absent`
  (exit `2`), never a ranked answer across mixed vector spaces, and
  re-running `eg re-embed` with the same model heals it (`index_rebuilt:
  true`).

Text is recovered by the full vector key (record ID + temporal identity), so
history stores re-embed each per-commit observation from its own text — two
observations of the same symbol at different commits never share a vector.

## Re-embed vs the neighboring workflows

- **Full rebuild** (`eg scan` + `eg ingest --adapter embedded --data-dir
  <NEW_DIR> --embed`): re-crawls sources and re-extracts the graph. Use it
  when the sources changed or you want a fresh store. Re-embed skips all of
  that — it only touches vectors and the identity record.
- **The #104 refusal** (`eg query semantic` against a model it was not built
  with): read-only, always refuses rather than ranking across vector spaces.
  Re-embed is what an operator runs *after* deciding the store should move to
  the new model; the refusal itself never mutates anything.
- **Source-edit refresh** (`eg scan --refresh`, issue #98): re-embeds only
  the nodes whose sources changed, under the *same* model. Re-embed
  re-embeds *every* previously-embedded node, under a *different* model.
- **`--dry-run`**: plans the re-embed (candidate count, target model,
  whether the index would be rebuilt) and exits `13` while work remains,
  without touching the store.

## Determinism

Same model + same store content → byte-identical vectors and byte-identical
top-k answers across runs. A repeated `eg re-embed` with the same target
model is a no-op (`reembedded: 0`).
