# Producer Identity Envelope

**Status:** Active. This document is the single source of truth for the `producer`
field shape on every persisted Egregore graph record.

**Schema version:** 1. Adding a new `producer_kind` or a new well-known
`producer_components` key is **additive**. Removing or renaming an existing key
is a `/v2/` bump. (`PRODUCER_ENVELOPE_SCHEMA_VERSION` constant in `src/ir.rs`.)

**Coordination:** `docs/schema/schema-versioning.md` §Producer Envelope.

---

## 1 — Why This Field Exists

Egregore promises deterministic extraction (PR-1:
`docs/prd/0001-codebase-knowledge-graph.md` — *"Given the same repository state,
config, and binary version, `scan` must produce stable node IDs and edge IDs"*)
and load-bearing temporal semantics (PR-6, M5, M6, M8 — agents must be able to
ask "what did this symbol mean at commit X?" and "when did this symbol
semantically drift?"). Both promises silently depend on a fact that no record
carried before this slice: **which producer wrote this record.**

Without producer identity:

- A `tree-sitter-rust` bump silently churns symbol spans across a re-scan,
  invalidating stable IDs with no per-record attribution.
- Bi-temporal as-of responses cannot distinguish "the code at commit X" from
  "the extractor at observation Y."
- The re-extraction escape hatch in `docs/schema/schema-versioning.md` cannot
  be made operationally safe — a reader cannot tell which records would change.

See also: SCIP/LSIF `toolInfo`, Glean predicate+writer-identity, CodeQL database
headers, OpenTelemetry `service.version` conventions, and OCI image manifests.

---

## 2 — The `Producer` Shape

Every persisted `Node`, `Edge`, `Tombstone`, and JSONL `GraphRecord` carries one
top-level `producer` field. The value is a single embedded `Producer` object
(defined in `src/ir.rs`).

```json
{
  "record_type": "node",
  "id": "codegraph:v4:…",
  "kind": "Symbol",
  "schema_version": 4,
  "producer": {
    "egregore_version": "0.1.0",
    "egregore_git": { "commit": "abc123dead", "dirty": false },
    "producer_kind": "code_graph_extractor",
    "producer_components": {
      "tree_sitter": "0.26.8",
      "tree_sitter_rust": "0.24.2"
    },
    "producer_started_at": "2026-01-01T12:00:00Z"
  },
  "summary": "…"
}
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `egregore_version` | `String` | yes | Semver string from `CARGO_PKG_VERSION`. |
| `egregore_git` | `EgregoreGit?` | no | Git provenance; absent for clean release tarballs. |
| `producer_kind` | `ProducerKind` | yes | Which subsystem produced these records. |
| `producer_components` | `Map<String,String>` | yes | Named version strings (see §4). |
| `producer_started_at` | `String` | yes | RFC 3339 wall-clock time the producer process started. |

**`EgregoreGit`**:

| Field | Type | Description |
|-------|------|-------------|
| `commit` | `String` | Short or full git commit SHA of the build tree. |
| `dirty` | `bool` | `true` when uncommitted changes were present at build time. |

### `ProducerKind` values

| Serialized value | Rust variant | Subsystem |
|-----------------|--------------|-----------|
| `code_graph_extractor` | `CodeGraphExtractor` | `scan` command (Tree-sitter extraction). |
| `history_replay` | `HistoryReplay` | `scan-history` command (Git history replay). |
| `incremental_cache` | `IncrementalCache` | Incremental scan with cache. |
| `traj_importer` | `TrajImporter` | `.traj` file importer. |
| `codex_importer` | `CodexImporter` | Codex-format importer. |
| `claude_code_importer` | `ClaudeCodeImporter` | Claude Code session importer. |
| `observation_writer` | `ObservationWriter` | Agent-memory observation writer. |
| `task_writer` | `TaskWriter` | Project / task writer. |
| `drift_engine` | `DriftEngine` | Semantic drift engine. |
| `log_importer` | `LogImporter` | `scan-logs` log-signature importer (issues #319/#320). |
| `doc_importer` | `DocImporter` | Repo-local design-doc importer (issue #149). |
| `other` | `Other` | Any producer not enumerated above. |

Adding a new `producer_kind` is **additive** (no schema version bump required).
Removing or renaming an existing value is a `/v2/` bump.

---

## 3 — Placement

`producer` is a **single embedded field on `GraphRecord`**, not a per-variant
duplication of fields. When a producer emits a batch of records, every record in
that batch carries the same `Producer` value. JSONL deduplication or compression
is left to a later optimization slice and is out of scope here.

---

## 4 — Per-Kind `producer_components` Minimum Field Set

Each `producer_kind` MUST populate at minimum the following keys in
`producer_components`. Additional keys are always additive and never require a
version bump.

| `producer_kind` | Required keys |
|----------------|---------------|
| `code_graph_extractor` | `tree_sitter`, `tree_sitter_rust` |
| `history_replay` | `tree_sitter`, `tree_sitter_rust` (plus `git2`/`gix` when introduced) |
| `incremental_cache` | `cache_format_version` |
| `drift_engine` | `embedding_model_id` (redundant with `SemanticDriftMetadata.embedding_model`; see §7) |
| `traj_importer` | `importer_schema_version`, `source_format_version` |
| `codex_importer` | `importer_schema_version`, `source_format_version` |
| `claude_code_importer` | `importer_schema_version`, `source_format_version` |
| `log_importer` | `importer_schema_version`, `source_format_version` (`plain-v1`/`jsonl-v1`), `fingerprint_algorithm` (`template-v1`) |
| `doc_importer` | `importer_schema_version`, `source_format_version` (`doc-ingest/v1`) |
| `observation_writer` | `transport` when written over the MCP transport (issue #183); absent for CLI writes |

The `observation_writer` example over MCP:
```json
"producer_components": {
  "transport": "mcp"
}
```

The `transport` component distinguishes how an observation entered the store:
`eg write observation` (CLI) stamps empty components; the MCP
`record_observation` tool stamps `transport: "mcp"`. Both stamp the same
`producer_kind` (`observation_writer`), so origin-aware readers can
distinguish transport while transport-agnostic readers keep working. The key
is additive: it never participates in stable ID composition (§5).

The `code_graph_extractor` example:
```json
"producer_components": {
  "tree_sitter": "0.26.8",
  "tree_sitter_rust": "0.24.2"
}
```

Adding a new language grammar (e.g., TypeScript) MUST add its grammar version
under a stable key (e.g., `tree_sitter_typescript`).

---

## 5 — Non-Identity Rule

> **`producer` MUST NOT contribute to any stable ID composition.**

Two records produced by different binary versions over identical input MUST have
identical stable IDs. This preserves PR-1 across binary upgrades — only the
*observation* changes, not the *identity*.

The test fixture that falsifies any regression is:
```
tests/producer_version.rs :: producer_non_identity_rule
```

It creates two records with the same semantic content but different `Producer`
values and asserts that:
1. Their stable IDs are identical.
2. Their `producer` envelopes differ.

---

## 6 — Legacy-Record Policy

Records persisted **before this slice** have no `producer` field. Readers MUST
tolerate this and surface it as a typed `producer: legacy_pre_v1` marker rather
than synthesizing a producer identity.

This is the one exception to the per-`(domain, kind)` compatibility rule from
`docs/schema/schema-versioning.md`: records cannot be retroactively known to have
come from a specific producer; pretending otherwise would corrupt the audit trail.

**`eg inspect`** groups these records under the `legacy_pre_v1` bucket in both
the `producer_kind` and `egregore_version` breakdowns.

The conformance test for this rule is:
```
tests/producer_version.rs :: legacy_record_inspect_policy
```

It writes a JSONL with one producer-stamped record and one legacy record, runs
`eg inspect`, and asserts that both `producer_kind code_graph_extractor: 1` and
`producer_kind legacy_pre_v1: 1` appear in the output.

**Re-extraction to backfill `producer` is forever out of scope.** Legacy records
stay legacy.

---

## 7 — Cross-Domain Coordination

### #7 — Repository Identity

Repository identity is composed of `(canonical_root, vcs_remote)`. Producer
identity is the orthogonal "who observed it" axis. The two are independent.

### #8 — Bi-Temporal As-Of Selector

Bi-temporal as-of responses MUST surface `producer` per record so the agent can
distinguish "the code at commit X" from "the extractor at observation Y."

### #12 — Symbol Identity Stability

Symbol identity stability is asserted *relative to a fixed grammar*. The
conformance fixture for #12 should use a pinned grammar version; cross-grammar
churn is documented but not asserted.

### #15 — `SemanticDrift` Schema

`SemanticDriftMetadata.embedding_model` (see `docs/schema/semantic-drift.md`)
and `Producer.producer_components.embedding_model_id` are redundant by design.
The canonical source is the drift record's own `embedding_model` field; the
producer envelope duplicate is for query-routing convenience only.

### #16 — Schema Versioning

`producer` is added to the per-`(domain, kind)` `schema_version` story as a
*non-versioned envelope field*. It does not participate in the per-domain
`schema_version` bump cycle. Adding a new `producer_kind` is additive; changing
a `producer_components` key shape is breaking and requires a
`PRODUCER_ENVELOPE_SCHEMA_VERSION` bump.

---

## 8 — Inspect and Query Surface

### `eg inspect`

`eg inspect <graph.jsonl>` prints a per-`producer_kind` and per-`egregore_version`
breakdown after the record-type counts:

```
records: 200
nodes: 150
edges: 49
tombstones: 1
…
producer_kind code_graph_extractor: 120
producer_kind legacy_pre_v1: 80
egregore_version 0.1.0: 120
egregore_version legacy_pre_v1: 80
```

### Daemon `/v1/status`

The `/v1/status` response (specced in `docs/schema/daemon-runtime.md`) MUST
include the producer identity of the currently-running daemon binary. The
`producer_kind` for the daemon binary itself is not a record producer (it is a
store manager), so its identity is reported as a separate top-level field on the
status response rather than as a `GraphRecord` producer envelope.

### Daemon `/v1/query`

The `/v1/query` response (specced in `docs/schema/daemon-query.md`) MUST surface
the `producer` field on every returned record without redaction. It is metadata,
not payload, and cannot be selectively withheld.

---

## 9 — Success Metric

After this slice, given a single embedded AletheiaDB store ingested across two
extractor binary versions A and B, an operator can answer **all three** of the
following with one query each, with **zero false positives**:

1. "Which records were observed by binary version A vs. binary version B?" —
   group on `producer.egregore_version`.
2. "Which records would change if I re-extracted with binary version C (a
   hypothetical grammar bump)?" — list records with
   `producer.producer_kind = code_graph_extractor` and
   `producer_components.tree_sitter_rust != <current>`.
3. "Which records came from a producer revision I no longer trust?" — filter on
   `producer.egregore_version` or `producer.egregore_git.commit`.

---

## 10 — Producer Trust-Class Audit (issue #236)

**`eg query trust-audit`** (read-only; documented in `docs/cli/trust-audit.md`)
scans every persisted node, edge, and tombstone and verifies that each record's
`producer_kind` is consistent with the trust class its node kind implies. The
rule set below is the reviewable contract the lane enforces; the classifier in
`src/query/trust_audit.rs` is the executable twin — the two must not drift.

### Producer trust classes

| Class | `producer_kind` values | Meaning |
|-------|------------------------|---------|
| `deterministic` | `code_graph_extractor`, `history_replay`, `incremental_cache`, `log_importer`, `drift_engine` | Facts derived deterministically from source (extraction, history replay, cache refresh, log-signature import, embedding drift measurement). |
| `agent` | `traj_importer`, `codex_importer`, `claude_code_importer`, `observation_writer`, `task_writer` | Agent-authored content (observations, trajectory/tool-call imports, project/task state). |

`drift_engine` is `deterministic` by construction: it writes `SemanticDrift`,
`EmbeddingModel`, and `EmbeddingVector` records — measurements computed from
source bytes plus model bytes, not agent prose.

### Node-kind trust classes

`code_fact` — deterministic code-fact kinds. May **only** be written by a
`deterministic`-class producer:

`Repository`, `File`, `Module`, `Symbol`, `Import`, `Diagnostic`,
`PanicRiskSite`, `DebtMarker`, `UnsafeSite`, `DependencyDeclaration`,
`ScanCoverage`, `HistoryReplayWindow`, `Commit`, `Change`, `SemanticDrift`,
`EmbeddingModel`, `EmbeddingVector`, `LogSource`, `ErrorSignature`, `LogEvent`,
`LogOccurrenceBucket`.

`agent_authored` — everything else (agent-memory, verification, user-context,
project, and artifact kinds). May **only** be written by an `agent`-class
producer:

`Agent`, `AgentSession`, `Observation`, `AgentRun`, `AgentTurn`, `ToolCall`,
`CommandRun`, `FileEdit`, `PatchArtifact`, `Failure`, `Decision`, `CostUsage`,
`TestRun`, `CIStatus`, `BenchmarkRun`, `CoverageReport`, `ProofResult`,
`PromoteCandidate`, `PromotionPrompt`, `PromotionDecision`, `Preference`,
`WorkflowRule`, `NamingDecision`, `Constraint`, `Retraction`, `Task`,
`AcceptanceCriterion`, `ExternalLink`, `Product`, `Project`, `Plan`,
`GitHubIssue`, `PR`, `Review`, `ExternalIdentity`, `ReviewStateTransition`,
`LocalTask`, `Artifact`, `Verification`, `CommandEvidence`.

### Rules

| Rule id | Meaning |
|---------|---------|
| `code_fact_written_by_agent_producer` | A `code_fact` kind written by an `agent`-class producer. This is the headline corruption case: agent-authored content masquerading as deterministic source truth. |
| `agent_authored_written_by_deterministic_producer` | An `agent_authored` kind written by a `deterministic`-class producer. The extractor must never author observations, decisions, or project state. |
| `unclassifiable_producer_kind` | `producer_kind: other` — an unknown or future variant this binary cannot classify. Flagged fail-closed: an unclassifiable producer is a violation, never a silent pass. |

Edges are audited against the trust class implied by their **source** node's
kind (resolved in-batch, last-write-wins — the same semantics as the daemon's
`lookup_node_kind`). Tombstones are audited against the trust class implied by
their **target** record's kind. An edge or tombstone whose endpoint kind cannot
be resolved in the batch is **not** a trust-class violation — it is skipped
(referential integrity is `eg validate`'s job, not this audit's) — **unless**
its producer is `other`, which fails closed regardless of resolvability.

Legacy records (`producer: None`, §6) are exempt: a record that predates the
producer envelope cannot be retroactively known to come from a specific
producer, and pretending otherwise would corrupt the audit trail.

### Additive versioning

- Adding a new `producer_kind` is additive (§2) **but the new variant enters
  the audit unclassified**: it deserializes to `other` on older binaries and is
  flagged `unclassifiable_producer_kind` until this section documents its trust
  class. A new producer kind is only "clean" once it is listed above.
- Adding a new `NodeKind` is additive per `docs/schema/schema-versioning.md`,
  but the audit's classifier matches every `NodeKind` variant with **no
  wildcard arm** (mirroring `TrustIndex::classify`, issue #114): the crate
  fails to compile until the new kind is deliberately placed in `code_fact` or
  `agent_authored`, so a future kind can never silently inherit a trust class.
