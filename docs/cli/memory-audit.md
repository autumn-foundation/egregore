# eg query memory

Audit the evidence behind one **agent-authored memory claim** — answer the M10
question *"what evidence supports this memory?"* — starting from a memory record
ID or a source artifact / session handle. Local-first; no network access.

> **"No evidence found" is not evidence that the claim is true.** A memory claim
> is an agent's subjective assertion. This audit shows what the local graph can
> and cannot back up; it never turns missing support into a confident answer.

## Synopsis

```text
eg query memory <ID_OR_HANDLE> --graph <PATH> [--verified-only]
eg query memory <ID_OR_HANDLE> --data-dir <DIR> [--verified-only]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`).

## Handle resolution

`<ID_OR_HANDLE>` accepts two handle types (AC2):

1. **Canonical memory record ID** — `agent_memory:v1:<64-hex>` naming an
   auditable claim (`Observation`, `Decision`, `Failure`). When the ID instead
   names an `AgentSession` or `Agent`, it is treated as a **scope handle**: it
   resolves to the claims authored in that scope (matched on the session node's
   `session_id` / agent node's `agent_id` field, falling back to `name`). One
   audit covers one claim, so a scope that resolves to a single claim audits it
   directly, and a scope with multiple claims returns an `Ambiguous` diagnostic
   listing the candidate claim IDs to re-query.
2. **Source artifact / session handle** — a string matching a claim's
   `source_handle`, `source_artifact_path`, `source_artifact_hash`, or
   `session_id` (e.g. `trajectories/run-1.traj`).

| Condition | Exit | Output |
|-----------|------|--------|
| Success | `0` | Audit JSON on stdout |
| Empty handle, or malformed canonical ID | `1` | `{"Unsupported":{...}}` on stderr |
| Handle resolves to more than one claim | `1` | `{"Ambiguous":{...}}` on stderr |
| Handle matches nothing in the store | `2` | `{"ok":false,"error":{"code":"no_match",...}}` on stdout |
| Handle names a tombstoned (deleted) record | `2` | `{"ok":false,"error":{"code":"stale_handle",...}}` on stdout |

The workflow never infers a replacement for an unsupported, ambiguous, stale, or
missing handle, and it never reads raw transcript bodies.

## Response shape

The response separates trust classes so an agent-authored claim is **never
presented as source truth or proof by itself** (AC3). Every section is canonical
(record-ID) ordered for determinism (AC8). Every item carries a `record_id` plus
a non-empty `citable_handle` (AC4).

| Section | Contents | Trust class |
|---------|----------|-------------|
| `memory_claim` | The audited agent-authored claim(s). | `agent_authored` |
| `direct_provenance` | Agent / session / source-artifact handles. | — |
| `supporting_evidence` | Artifacts and other cited support. | `artifact` / `agent_authored` |
| `contradicting_evidence` | Records linked via `CONTRADICTS`. | per record |
| `superseding_records` | Records that supersede the claim (`SUPERSEDES` / `superseded_by`). | per record |
| `related_code_handles` | Cited `File` / `Symbol` code facts. | `source_fact` |
| `related_project_handles` | Cited `Task` / `AcceptanceCriterion`. | `project_state` |
| `verification_evidence` | Cited verification-domain evidence. | `verification_evidence` |
| `diagnostics` | Stable codes for unresolved/protected/redacted handles. | — |
| `excluded` | Records dropped by `--verified-only`, reported not hidden. | — |
| `page` | Deterministic pagination block (`cursor`, `has_more`, `returned`). | — |

A later contradiction or superseding record appears as a **separate audit item**;
the original memory remains queryable and is never rewritten or hidden (AC7).

### `--verified-only`

Excludes unverified agent-authored observations from the evidence sections. A
claim is "verified" when it cites at least one present verification-domain record
(`VALIDATED_BY` / `HAS_EVIDENCE` / `PRODUCED_EVIDENCE`). 100% of excluded records
are reported in `excluded` with `reason: "unverified_observation"` rather than
silently disappearing (AC5).

### Safety: no raw payloads

Output never includes raw transcript text, raw command output, patch hunks, issue
bodies, PR comments, environment values, or bearer tokens (AC9). The claim's raw
`text` body is never emitted (a `Failure` keeps a command-output excerpt there);
the audit exposes only the bounded `summary`, a `text_hash` handle, and a
`redacted` flag. Protected payloads (command output, patch bytes, task bodies) are
referenced by **hash only**, flagged with `"protected": true` on the item, and
surfaced as `protected_payload` diagnostics.

### Diagnostic codes

| Code | Meaning |
|------|---------|
| `unresolved_evidence_link` | An `evidence_links` target ID is absent from the store. |
| `evidence_target_unresolved` | An evidence link supplied only a triple, not a record ID; not resolved heuristically. |
| `protected_payload` | A raw payload (stdout/stderr/patch/body) is withheld; the hash is the handle. |
| `redacted_payload` | The claim carries a redaction policy version. |

## When to use this versus other tools

| Reach for | When you want |
|-----------|---------------|
| **`eg query memory`** (this) | Why an **agent believes a claim** — its provenance, support, contradictions, and supersessions. |
| `eg query context` (#38) | Evidence-backed context for a **symbol**. |
| `eg query task` (#48) | Evidence for a **task / acceptance criterion**. |
| commit-range context (#62) | What changed across a **commit range**. |
| prior failed-attempt queries (#63) | A code/task handle's **failure history**. |
| `rg` | Fast recursive **text** search when you know where to look. |
| `jq` | Filtering raw **JSONL** you have already located. |
| raw transcript review | Replaying an agent run turn-by-turn. |

`rg` + `jq` over transcripts and JSONL are honest substitutes, but they do not
traverse memory-to-evidence relationships, separate trust classes, report
unresolved handles, or protect against treating unverified agent prose as proof.

## Example

```sh
eg query memory agent_memory:v1:aaaa... --graph graph.jsonl
eg query memory trajectories/run-1.traj --graph graph.jsonl --verified-only
```

```json
{
  "ok": true,
  "memory_id": "agent_memory:v1:aaaa...",
  "verified_only": false,
  "memory_claim": [
    {
      "record_id": "agent_memory:v1:aaaa...",
      "kind": "Observation",
      "trust_class": "agent_authored",
      "summary": "Refactored foo",
      "text_hash": "blake3:9f2c...",
      "confidence": "0.9",
      "redacted": true
    }
  ],
  "direct_provenance": {
    "provenance_handle": "agent_1:sess_1",
    "agent_id": "agent_1",
    "session_id": "sess_1",
    "source_handle": "src/lib.rs:sha256:deadbeef",
    "redaction_policy_version": "1"
  },
  "contradicting_evidence": [
    {
      "record_id": "agent_memory:v1:dddd...",
      "kind": "Observation",
      "trust_class": "agent_authored",
      "relation": "CONTRADICTS",
      "citable_handle": "trajectories/run-2.traj",
      "summary": "regression"
    }
  ],
  "related_code_handles": [
    {
      "record_id": "codegraph:v1:cccc...",
      "kind": "File",
      "trust_class": "source_fact",
      "relation": "OBSERVES",
      "citable_handle": "src/lib.rs:1-100",
      "repo_relative_path": "src/lib.rs"
    }
  ],
  "verification_evidence": [
    {
      "record_id": "verification:v1:bbbb...",
      "kind": "Verification",
      "trust_class": "verification_evidence",
      "relation": "VALIDATED_BY",
      "citable_handle": "command_run",
      "status": "pass"
    }
  ],
  "diagnostics": [
    {
      "code": "unresolved_evidence_link",
      "source_record_id": "agent_memory:v1:aaaa...",
      "target_handle": "agent_memory:v1:0000...",
      "relation": "RELATES_TO",
      "target_domain": "agent_memory"
    }
  ],
  "excluded": [],
  "page": { "cursor": null, "has_more": false, "returned": 4 }
}
```

## Scope

This slice consumes existing agent-memory, user-context, project, artifact,
verification, redaction, daemon-query, and evidence-link contracts. It introduces
no new graph domain, importer, edge vocabulary, trust model, hosted service,
LLM-generated answer, preference-approval UI, or language expansion.
