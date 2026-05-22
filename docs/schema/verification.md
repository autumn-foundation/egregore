# Verification-Evidence Domain Schema — v1

**Status:** Active at v1. Adding a new field, node kind, or edge label is
additive. Renaming, removing, or changing the semantics of an existing field
requires a schema version bump enforced by the `schema_version` field on every
record.

**Source of truth:** This document. `src/ir.rs` (node kinds, edge labels, ID
functions, `VERIFICATION_SCHEMA_VERSION`) and `src/daemon.rs` (write applier
validation, `validate_verification_domain_records`) must conform to it.

**Trust class:** `deterministic-but-runtime-derived` — evidence is captured
from real execution artefacts (process exit codes, content hashes, timing) but
is not checkable by static analysis alone.

**Related documents:**
- Wire contract: [`docs/schema/daemon-api.md`](daemon-api.md)
- Agent-memory domain (cross-domain edges): [`docs/schema/agent-memory.md`](agent-memory.md)
- Agent actions and tool-call evidence links: [`docs/schema/agent-actions.md`](agent-actions.md)
- Vision PRD: [`docs/prd/0000-egregore-vision.md`](../prd/0000-egregore-vision.md)
- Daemon design: [`docs/plans/2026-05-17-egregore-daemon-design.md`](../plans/2026-05-17-egregore-daemon-design.md)

---

## 1 — Domain identity

| Field | Value |
|-------|-------|
| Domain name | `verification` |
| `schema_version` | `1` |
| ID prefix | `verification:v1:` |
| Rust constant | `VERIFICATION_SCHEMA_VERSION = 1` |
| ID function | `verification_stable_id(parts: &[&str])` in `src/ir.rs` |

Verification IDs use the `verification:v1:` prefix so they cannot collide with
code-graph (`codegraph:v1:`) or agent-memory (`agent_memory:v1:`) IDs even when
the Blake3 content hashes are identical.

---

## 2 — Node kinds

All verification kinds share the base `GraphRecord::Node` shape. The `verification_kind`
field disambiguates them at query time when the caller has mixed-kind results.

| `NodeKind` | `verification_kind` string | Description |
|------------|---------------------------|-------------|
| `CommandRun` | `command_run` | Shell command execution with exit status and output handles. |
| `Verification` | `verification` | Umbrella verification claim backed by handles or child evidence. |
| `TestRun` | `test_run` | Result of one test-suite invocation (unit, integration, e2e). |
| `CIStatus` | `ci_status` | Snapshot of a CI pipeline run (build, lint, full pipeline). |
| `BenchmarkRun` | `benchmark_run` | Benchmark measurement (throughput, latency, memory). |
| `CoverageReport` | `coverage_report` | Code-coverage snapshot produced by a coverage tool. |
| `ProofResult` | `proof_result` | Formal-verification or type-checking result (e.g., Coq, Lean, Miri). |

### 2a — Shared base fields

Every verification node inherits all base `GraphRecord::Node` fields.
Relevant fields for this domain:

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `id` | string | yes | `verification:v1:{blake3}` — see §3. |
| `domain` | string | yes | `"verification"` |
| `schema_version` | u32 | yes | `1` |
| `kind` | NodeKind | yes | One of the verification kinds above. |
| `verification_kind` | string | no | Human-readable disambiguation; redundant with `kind` but aids plain-JSON consumers. |
| `status` | string | no | `"pass"`, `"fail"`, `"skip"`, `"error"`, `"timeout"` — free string, no enum enforcement at v1. |
| `executed_at` | string | no | RFC 3339 timestamp when execution completed. |
| `summary` | string | yes | Short prose description of the outcome. |
| `source_artifact_path` | string | no | Repo-relative path to the script, config file, or CI definition that triggered this run. |
| `source_artifact_hash` | string | no | BLAKE3 hex of the artefact at `source_artifact_path` at run time. |
| `stdout_handle` | OutputHandle | no | Structured handle for captured stdout (see §4). |
| `stderr_handle` | OutputHandle | no | Structured handle for captured stderr (see §4). |
| `evidence_quality` | string | no | `"high"`, `"medium"`, `"low"` — caller-set quality signal for downstream filtering. |
| `temporal` | TemporalMetadata | no | Bitemporal provenance (see §5). |

### 2b — Kind-specific guidance

**TestRun**
- `status` SHOULD be `"pass"` or `"fail"`.
- `stdout_handle` SHOULD carry the test runner's output (JUnit XML path or
  truncated inline).
- Use `FAILED_ON` edges (see §6) to link individual test failures to the code
  symbols or files they exercised.

**CIStatus**
- `status` SHOULD reflect the pipeline-level outcome: `"pass"`, `"fail"`, or
  `"error"` (infrastructure failure distinct from test failure).
- `source_artifact_path` SHOULD be the CI config file (e.g., `.github/workflows/ci.yml`).

**BenchmarkRun**
- `summary` SHOULD include the measured metric and units
  (e.g., `"throughput: 1.2 Mrps ± 0.03"`).
- `stdout_handle` SHOULD carry the raw benchmark output for later re-parsing.

**CoverageReport**
- `summary` SHOULD include overall line- or branch-coverage percentage.
- `source_artifact_path` SHOULD point to the LCOV or coverage.json artefact.

**ProofResult**
- `status` SHOULD be `"pass"` (verified), `"fail"` (counter-example found), or
  `"error"` (prover error / timeout).
- `stdout_handle` SHOULD carry the prover transcript.

---

## 3 — Stable ID composition

```rust
pub fn verification_stable_id(parts: &[&str]) -> String {
    // Returns "verification:v1:{blake3_hex_of_joined_parts}"
}
```

Recommended `parts` slices by kind:

| Kind | `parts` |
|------|---------|
| `TestRun` | `["test_run", session_id, commit_sha, suite_name]` |
| `CIStatus` | `["ci_status", pipeline_id, run_id]` |
| `BenchmarkRun` | `["benchmark_run", session_id, commit_sha, suite_name]` |
| `CoverageReport` | `["coverage_report", session_id, commit_sha, tool_id]` |
| `ProofResult` | `["proof_result", session_id, commit_sha, proof_target]` |

All parts are ASCII-lowercased before hashing. Callers MUST include at least
one stable external identifier (commit SHA, pipeline ID, or content hash) so
IDs remain deterministic across re-imports.

---

## 4 — OutputHandle

`OutputHandle` is a structured wrapper for captured process output. It supports
both inline storage (for small payloads) and hash-only storage (for large
payloads that are kept in external artifact storage).

```rust
pub struct OutputHandle {
    /// Inline UTF-8 content, present only when `bytes <= 16 KiB`.
    pub inline: Option<String>,
    /// BLAKE3 hex of the full output, even when `inline` is absent.
    pub hash: String,
    /// Exact byte length of the full output before any truncation.
    pub bytes: u64,
}
```

### 4a — Inline ceiling

`inline` MUST be `None` when `bytes > 16 384` (16 KiB). The daemon write
applier enforces this at ingestion time and returns
`inline_payload_exceeds_ceiling` if violated.
Callers MUST demote to handle-only (`inline: None`, `hash` and `bytes` set)
before writing large outputs.

### 4b — Evidence handle requirement

Every verification-domain node MUST carry at least one evidence handle.
A record satisfies this constraint when any of the following is present:

1. `source_artifact_hash` is non-empty.
2. `source_artifact_path` is non-empty.
3. `stdout_handle.hash` is non-empty.
4. `stderr_handle.hash` is non-empty.

Nodes that carry none of the above are rejected at ingestion time with the
`missing_evidence_handle` error code (HTTP 422).

---

## 5 — Bitemporal provenance

Verification nodes SHOULD carry `temporal` metadata so queries can reconstruct
the verification state at any point in history.

| Field | Notes |
|-------|-------|
| `valid_time` | When the run actually occurred (RFC 3339). |
| `transaction_time` | When the record was written to the store (set by daemon). |
| `git_commit` | Commit SHA the run targeted, when applicable. |

---

## 6 — Cross-domain edges

Verification records participate in the cross-domain edge registry defined in
`docs/schema/agent-memory.md` §6a. The following labels connect verification
records to records in other domains.

### FROM verification

| Label | FROM | TO domain(s) | FROM kind(s) | TO kind(s) | Notes |
|-------|------|-------------|-------------|-----------|-------|
| `VALIDATED_BY` | `agent_memory` | `verification` | `Observation`, `Decision` | any verification | An observation or decision is backed by a verification run. |
| `FAILED_ON` | `verification` | `codegraph` | `TestRun`, `CIStatus` | `Symbol`, `File` | A specific failure targets a code symbol or file. |
| `CONTRADICTS` | `verification` | any | any verification | any | A later run contradicts an earlier claim. |
| `MENTIONS_SYMBOL` | `verification` | `codegraph` | any verification | `Symbol` | A verification record references a specific symbol. |
| `TOUCHED_FILE` | `verification` | `codegraph` | any verification | `File` | A verification run exercised a specific file. |
| `PRODUCED_EVIDENCE` | `agent_memory` | `verification` | `ToolCall` | `CommandRun`, `TestRun` | A tool call produced verification evidence. |

### TO verification

Any domain may write `HAS_EVIDENCE` edges that terminate at verification nodes.

---

## 7 — Write applier validation rules

The `validate_verification_domain_records` function in `src/daemon.rs` enforces
these rules on every ingest write that contains verification-domain records.

A record is treated as verification-domain when:
- Its `id` starts with `verification:v1:`, **or**
- Its `domain` field equals `"verification"`.

Rules enforced at write time:

1. **Schema version** — `schema_version` must equal `1` (`VERIFICATION_SCHEMA_VERSION`).  
   Error: `bad_request` (HTTP 400).

2. **Node kind** — `kind` must be one of `TestRun`, `CIStatus`, `BenchmarkRun`,
   `CoverageReport`, or `ProofResult`.  
   Error: `bad_request` (HTTP 400).

3. **`executed_at` format** — when present, `executed_at` must be a valid RFC 3339
   timestamp.  
   Error: `bad_request` (HTTP 400).

4. **Evidence handle required** — at least one of `source_artifact_hash`,
   `source_artifact_path`, `stdout_handle.hash`, or `stderr_handle.hash` must
   be present and non-empty.  
   Error: `missing_evidence_handle` (HTTP 422).

5. **Inline ceiling** — `stdout_handle.inline` and `stderr_handle.inline` must
   be `None` when the corresponding actual content length or `bytes` field
   exceeds 16 384.  
   Error: `inline_payload_exceeds_ceiling` (HTTP 400).

---

## 8 — Error codes

| Code | HTTP | Meaning |
|------|------|---------|
| `missing_evidence_handle` | 422 | A verification-domain node was submitted without any evidence handle. |
| `inline_payload_exceeds_ceiling` | 400 | An inline output handle exceeded the 16 KiB ceiling. |
| `bad_request` | 400 | Wrong `schema_version`, disallowed `kind`, invalid `executed_at` format, or invalid handle metadata. |

See `docs/schema/daemon-api.md` §5 for the full error-code table.

---

## 9 — Appendix: NodeKind exhaustive-match requirement

`src/embeddings.rs` contains an exhaustive match over `NodeKind`. All
verification node kinds must appear in the `return None` arm (they are not
embedded). The `tests/daemon.rs::all_node_kinds_have_documented_schema` test
enforces that every `NodeKind` variant is documented; verification kinds map to
the `"verification-domain-documented"` bucket.
