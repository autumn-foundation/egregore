# Semantic Drift Domain Schema - v1

**Status:** Frozen at v1. Additive fields or enum values are allowed under
`SEMANTIC_SCHEMA_VERSION = 1`; removing or renaming fields, changing stable ID
inputs, or changing metric semantics requires `semantic:v2:` records.

**Source of truth:** This document. `src/ir.rs`, `src/embeddings.rs`,
`src/daemon.rs`, and `tests/semantic_drift.rs` must conform to it.

**Related documents:**
- Vision PRD: [`docs/prd/0000-egregore-vision.md`](../prd/0000-egregore-vision.md)
- Code graph PRD: [`docs/prd/0001-codebase-knowledge-graph.md`](../prd/0001-codebase-knowledge-graph.md)
- Agent-memory edge registry: [`docs/schema/agent-memory.md`](agent-memory.md)
- Daemon query contract: [`docs/schema/daemon-query.md`](daemon-query.md)
- Daemon design: [`docs/plans/2026-05-17-egregore-daemon-design.md`](../plans/2026-05-17-egregore-daemon-design.md)

## 1 - Domain Identity

| Field | Value |
|-------|-------|
| Domain name | `semantic` |
| Schema version | `1` |
| ID prefix | `semantic:v1:` |
| Rust constant | `SEMANTIC_SCHEMA_VERSION = 1` |

`SemanticDrift`, `EmbeddingModel`, and `EmbeddingVector` belong to the
`semantic` domain, not the `codegraph` domain. Code graph records remain the
measurement subjects; semantic records describe derived measurements over
those subjects.

Reserved semantic node kinds:

| Kind | Status | Notes |
|------|--------|-------|
| `SemanticDrift` | active | Drift measurement between a prior code graph observation and a later target. |
| `EmbeddingModel` | reserved | Optional persisted model identity record for `MEASURED_BY`. |
| `EmbeddingVector` | reserved | Optional persisted vector record if vectors become first-class records. |

## 2 - Trust Class

`SemanticDrift` is a derived measurement with this trust class:

`derived-from-pinned-model-with-pinned-threshold`

The trust class means:

- `embedding_model` equality requires all model fields to match exactly.
- `metric_kind` and `selection_threshold` are identity inputs.
- Records emitted with non-equal models are not comparable.
- Scores may differ by at most `1e-5` during deterministic replay checks.
- A record is immutable at a stable ID. A same-ID score mutation is rejected
  with `drift_record_immutable`.

Thresholds are additive for migration. Lowering a threshold emits additional
records under different stable IDs because `selection_threshold` is an identity
input. Existing records are not rewritten or deleted by threshold changes.

## 3 - `embedding_model`

`model_id` is not used for semantic drift records. Every drift record carries a
structured model identity:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `provider` | string | yes | Provider or boundary that supplied the model. |
| `name` | string | yes | Provider-local model name. |
| `version` | string | yes | Pinned version string. |
| `dim` | u32 | yes | Embedding dimension. |
| `content_hash` | string | yes | Hash of model content or `"unknown"` when the provider cannot expose one. |

Two embedding models are equal if and only if all five fields are equal.
Records from non-equal models MUST NOT be compared or merged.

## 4 - `SemanticDrift` Record Shape

`score` is a JSON number (`f64`), not a string.

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `record_type` | `"node"` | yes | |
| `id` | `semantic:v1:{hash}` | yes | Stable ID; see section 5. |
| `kind` | `"SemanticDrift"` | yes | |
| `schema_version` | `1` | yes | Must equal `SEMANTIC_SCHEMA_VERSION`. |
| `domain` | `"semantic"` | yes | Distinguishes from `codegraph`. |
| `valid_time` | RFC3339 | yes | The later measurement valid time. |
| `valid_time_source` | string | yes | Usually `after_valid_time`. |
| `ingested_at` | RFC3339 | yes | Store ingestion time or deterministic fixture time. |
| `semantic_drift.embedding_model` | object | yes | See section 3. |
| `semantic_drift.target_record_id` | record ID | yes | Later target; must be a `codegraph:` File or Symbol. |
| `semantic_drift.prior_record_id` | record ID | yes | Prior target; must be a `codegraph:` File or Symbol. |
| `semantic_drift.before_git_commit` | string | yes | Prior commit SHA. |
| `semantic_drift.after_git_commit` | string | yes | Later commit SHA. |
| `semantic_drift.before_valid_time` | RFC3339 | yes | Prior valid time. |
| `semantic_drift.after_valid_time` | RFC3339 | yes | Later valid time. |
| `semantic_drift.metric_kind` | enum | yes | `cosine_distance`, `l2_distance`, or `learned_delta_v1`. |
| `semantic_drift.score` | f64 JSON number | yes | Drift score. |
| `semantic_drift.selection_threshold` | f64 JSON number | yes | Threshold that caused selection. |
| `semantic_drift.selection_basis` | enum | yes | `threshold_only`, `top_k_per_pair`, or `top_k_per_symbol`. |

## 5 - Stable ID

Stable IDs use the semantic namespace:

```text
semantic:v<schema_version>:<blake3(
  domain ||
  kind ||
  embedding_model.provider ||
  embedding_model.name ||
  embedding_model.version ||
  embedding_model.dim ||
  embedding_model.content_hash ||
  metric_kind ||
  selection_threshold ||
  prior_record_id ||
  target_record_id ||
  before_git_commit ||
  after_git_commit
)>
```

Changing model provider/name/version/dim/content hash, metric kind, selection
threshold, prior/target IDs, or before/after commits produces a different ID.
Once written, the record body for a stable ID is immutable.

## 6 - Edge Contract

Semantic drift edges live in the agent-memory edge registry and are enforced by
the daemon write applier:

| Label | Source | Target | Required | Notes |
|-------|--------|--------|----------|-------|
| `DRIFTS_FROM` | `SemanticDrift` | `codegraph:` File or Symbol | yes | Later target matching `target_record_id`. |
| `DRIFTS_PRIOR` | `SemanticDrift` | `codegraph:` File or Symbol | yes | Prior target matching `prior_record_id`. |
| `MEASURED_BY` | `SemanticDrift` | `EmbeddingModel` | reserved | Optional once model records are materialized. |

If `prior_record_id` or the `DRIFTS_PRIOR` target is not a code graph File or
Symbol, the daemon returns `drift_prior_target_mismatch`.

## 7 - Redaction

Semantic drift records are derived from code graph summaries and embeddings.
They must not carry raw source snippets, prompt text, secrets, or unredacted
model/provider diagnostics. If a future provider adds diagnostic fields, those
fields must use the same redaction policy versioning as agent-memory records.

## 8 - Deterministic Replay

A deterministic replay fixture must prove:

- Same fixture plus same `embedding_model`, `metric_kind`, and
  `selection_threshold` produces the same drift IDs.
- Replayed scores for the same ID differ by at most `1e-5`.
- Changing the model, threshold, metric, prior/target, or commit inputs changes
  the stable ID.

## 9 - Coordination Notes

- Issue #2: daemon query responses for `drift_top_n` expose the structured
  semantic fields and score as a JSON number.
- Issue #3: `semantic:v1:` is a separate domain namespace.
- Issue #5: Problem Details include `drift_prior_target_mismatch` and
  `drift_record_immutable`.
- Issue #6: daemon ingestion validates the semantic edge registry.
- Issue #10: the future `drift` query verb is reserved and should reuse the
  same result shape as `drift_top_n`.
- Issue #12: redaction remains handle/policy-version based; drift records must
  not introduce raw unredacted payload fields.
