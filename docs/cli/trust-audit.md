# eg query trust-audit

Audit every persisted node, edge, and tombstone for producer trust-class
consistency: deterministic code-fact kinds may only be written by
deterministic producers, and agent-authored kinds may only be written by
agent producers. Code facts only — every violation is a citable JSON record,
never synthesized prose.

> **Audit, not advice.** The lane reports which records break the trust-class
> contract; it never reclassifies, repairs, or re-stamps anything. A violation
> is a fact about the store, not a verdict about the code it describes.

## Synopsis

```text
eg query trust-audit --graph <PATH>    [--format json|text]
eg query trust-audit --data-dir <DIR>  [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). Strictly read-only: no records, indexes, or runtime files are
created, modified, or deleted (`--data-dir` is read through a throwaway copy,
like the other read-only lanes).

The default JSON output is a single compact JSON envelope, one document on one
line, byte-identical across runs and platforms on an unchanged store.
`--format text` prints one human-readable line per violation.

| Condition | Exit | Output |
|-----------|------|--------|
| Audit ran; at least one trust-class violation | `0` | `{"ok":true,"violations":[...],...}` on stdout |
| Audit ran; zero violations | `0` | `{"ok":true,"violations":[],"empty_reason":"no_trust_violations",...}` on stdout |
| Unreadable / missing graph input, or both/neither of `--graph`/`--data-dir` | `1` | Error message on stderr |

A clean store is an explicit affirmative verdict (`ok:true`,
`violations:[]`, counted totals) — never silence.

## The rule set

The mapping from node kind to permitted producer trust class is documented in
`docs/schema/producer-version.md` §10 (issue #236) and is additive-versioned;
the classifier in `src/query/trust_audit.rs` is its executable twin.

- **`code_fact`** kinds (deterministic source facts: `Repository`, `File`,
  `Module`, `Symbol`, `Import`, `Diagnostic`, `PanicRiskSite`, `DebtMarker`,
  `UnsafeSite`, `DependencyDeclaration`, `ScanCoverage`, `HistoryReplayWindow`,
  `Commit`, `Change`, `SemanticDrift`, `EmbeddingModel`, `EmbeddingVector`,
  `LogSource`, `ErrorSignature`, `LogEvent`, `LogOccurrenceBucket`) may only be
  written by **deterministic** producers (`code_graph_extractor`,
  `history_replay`, `incremental_cache`, `log_importer`, `drift_engine`).
- **`agent_authored`** kinds (everything else: agent-memory, verification,
  user-context, project, and artifact kinds) may only be written by **agent**
  producers (`traj_importer`, `codex_importer`, `claude_code_importer`,
  `observation_writer`, `task_writer`).

Edges are audited against the trust class implied by their **source** node's
kind; tombstones against their **target** record's kind (both resolved
in-batch, last-write-wins). Legacy records without a producer envelope are
exempt (§6 of the schema doc). An `other` (unknown/future) producer kind is
flagged fail-closed — never silently accepted.

## Response fields (JSON)

| Field | Meaning |
|-------|---------|
| `ok` | `true` when the audit ran |
| `lane` | `"trust_audit"` |
| `violations[]` | Violation records, sorted canonically by (`record_id`, `rule`) — byte-stable across runs |
| `counts.records_scanned` | Total nodes + edges + tombstones examined |
| `counts.nodes_scanned` / `counts.edges_scanned` / `counts.tombstones_scanned` | Per-type totals |
| `counts.legacy_records_skipped` | Records with no producer envelope (exempt, not violations) |
| `counts.violations` | `violations.len()` |
| `empty_reason` | `"no_trust_violations"` on a clean store |
| `message` | Human-readable clean-store verdict |

Each violation record:

| Field | Meaning |
|-------|---------|
| `record_id` | Stable record ID of the offending record |
| `record_type` | `"node"`, `"edge"`, or `"tombstone"` |
| `kind` | Node kind (nodes), edge label (edges), or target record kind (tombstones) |
| `producer_kind` | The offending record's serialized producer kind |
| `rule` | The broken rule: `code_fact_written_by_agent_producer`, `agent_authored_written_by_deterministic_producer`, or `unclassifiable_producer_kind` |
| `repo_relative_path` | Repo-relative handle, when the record carries one |
| `span` | Source span, when the record carries one |
| `source_id` / `target_id` | Edge endpoints (edges only) |
| `deleted_id` | Tombstone target (tombstones only) |

## Determinism

The audit is deterministic: the same store yields byte-identical violation
output across repeated runs and across platforms. Violations sort by
(`record_id`, `rule`) with byte-wise string ordering — no timestamps, no
iteration-order dependence.

## Soundness boundary

The audit verifies producer *attribution*, not producer *honesty*: a
`code_graph_extractor`-stamped `Symbol` passes even if the extraction was
wrong. And "no violations" proves only that every record's producer class is
consistent with its kind — it is not proof the store is complete or correct.
