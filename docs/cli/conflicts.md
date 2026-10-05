# eg query conflicts

Surface **recorded contradicting observations on a shared code target** —
answer the operator-visible question *"who disagrees with whom about this
code, and what did each side cite?"* — starting from a symbol name, a
repo-relative file path, or a subsystem path prefix. Local-first; no network
access.

> **A returned conflict is a recorded disagreement, never an adjudication.**
> The lane only *surfaces* `CONTRADICTS` edges the store already records; it
> never infers semantic disagreement, never ranks the parties, and never
> declares a winner. A passing verification shown as a `verification` party
> against a recorded `Failure` belief is your adjudication lead — the evidence
> is citable, the verdict is yours. An empty result means the graph records no
> disagreement for this scope, not that the code is correct or uncontested.

## Synopsis

```text
eg query conflicts <SCOPE> --graph <PATH> [--repo <SELECTOR>] [--include-resolved] [--format json|text]
eg query conflicts <SCOPE> --data-dir <DIR> [--repo <SELECTOR>] [--include-resolved] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`).

## Scope resolution

`<SCOPE>` accepts these handle types, tried in order:

1. **Canonical code record ID** — `codegraph:v<N>:<hex>` naming a `Symbol`
   or `File`.
2. **Repo-relative file path** — e.g. `src/auth/token.rs`. The scope covers
   the file record **and the symbols the file defines** (via `DEFINES`): a
   disagreement about a symbol in the file is a disagreement about the file.
3. **Exact symbol name** — e.g. `login`. Several symbols of the same name in
   one repository form a multi-target scope; the same name across repositories
   is ambiguous unless `--repo` is given.
4. **Subsystem path prefix** — e.g. `src/auth/` (a trailing slash is
   normalized away). Segment-aware: `src/auth` never bleeds into
   `src/authz/`. Covers every live `File`/`Symbol` under the prefix.

`--repo <SELECTOR>` restricts resolution to one repository (issue #67);
without it, a handle matching targets in more than one repository is reported
as `Ambiguous` rather than resolved implicitly.

| Condition | Exit | Output |
|-----------|------|--------|
| Success (including a valid scope with **no** recorded conflicts) | `0` | Conflicts JSON on stdout, `ok:true`, `conflicts: []` when empty |
| Empty handle or malformed canonical ID | `1` | `{"Unsupported":{...}}` on stderr |
| Handle matches targets in more than one repository | `1` | `{"Ambiguous":{...}}` on stderr |
| Handle resolves to no live target in the store | `2` | `{"ok":false,"error":{"code":"no_match",...}}` on stdout |
| Handle names only tombstoned (deleted) targets | `2` | `{"ok":false,"error":{"code":"stale_handle",...}}` on stdout |

A valid scope with no recorded conflicts is a **real, successful, empty
answer** (exit 0, stable `no_recorded_conflicts` diagnostic), distinct from a
scope that resolves to nothing (exit 2).

## Response shape

Each conflict is the **unordered pair** of records joined by recorded
`CONTRADICTS` edge(s) — the stable `conflict_id` is
`[min_record_id, max_record_id]`. Parallel edges between the same two records
form one conflict, not duplicated rows. Conflicts, parties, edge IDs,
citation handles, and `resolved_by` are all in record-ID order, so repeated
runs over an unchanged store are byte-identical.

| Field | Contents |
|-------|----------|
| `scope` / `scope_kind` / `target_handle` / `target_ids` | The scope as given, its kind (`symbol` / `file` / `subsystem`), the shared target handle, and every code record ID the scope resolved to (sorted). |
| `conflicts[].parties` | Exactly two parties, in record-ID order. |
| `party.party_class` | `agent_authored` (Observation / Decision / Failure), `verification` (TestRun / CommandRun / CIStatus / …), or `user_context` (Preference / WorkflowRule / …). Deterministic code facts (`Symbol`, `File`, …) are **never** a party — a `CONTRADICTS` edge touching one is skipped whole. |
| `party.trust_class` | The derived trust label (issue #114): `agent_verified` / `agent_unverified` / `agent_contradicted` for agent claims, `verification_evidence` for verification executions, `other` for user-context records. |
| `party.edge_role` | Which end of the `CONTRADICTS` edge the party sits on: `source` / `target`. |
| `party.citation_handles` | The record's own ID first, then its evidence-link target IDs (sorted, deduplicated) — always non-empty, always citable. |
| `party.author` / `observed_at` / `confidence` | `agent_id[:session_id]` (or the deciding user for user-context records); verification `executed_at` else agent `observed_at`; recorded confidence. Stable markers (`unknown_agent`, `unknown_time`, `unspecified`) when the record carries nothing — never invented. |
| `party.summary` / `party.summary_hash` | Bounded payload-safe summary. Agent-authored text is synthesized from typed fields; the stored text is exposed only as a BLAKE3 `summary_hash`. No raw text, command output, or rule bodies. |
| `conflicts[].resolved` / `resolved_by` | `true` when any party carries `superseded_by` naming a **live** later record; `resolved_by` names those superseding records. Excluded by default; shown with `--include-resolved`. |
| `counts` | Deterministic tallies: `contradicts_edges_considered`, `conflicts_found`, `conflicts_returned`, `conflicts_resolved_excluded`, `edges_skipped_ineligible_party`. |
| `diagnostics` | Stable codes; `no_recorded_conflicts` when the scope is valid and scanned but records no disagreements. |

### Resolution is read-time only

A superseded record stays in the store and can still appear as a party when
`--include-resolved` is passed; the lane never deletes, hides, or rewrites
anything. A `superseded_by` pointing at an absent record does **not**
resolve — a phantom pointer must not hide a live disagreement.

### Safety: no raw payloads

Output never includes raw observation text, preference rule bodies, transcript
text, command output, patch hunks, or environment values — only hashes,
handles, bounded summaries, and record IDs. A conflict between agent claims
is citable evidence of a disagreement; it is not a fact about the code.

## When to use this versus other tools

| Reach for | When you want |
|-----------|---------------|
| **`eg query conflicts`** (this) | **Recorded disagreements about one code target** — who contradicts whom, with what each side cited. |
| `eg query failures` (#63) | **What failed here before**, and the evidence proving it. |
| `eg query memory` (#64) | Why an agent **believes one claim** (its evidence chain). |
| `eg query track-record` (#262) | A **per-agent** record of observation outcomes and supersessions. |
| `eg query blind-spots` (#265) | **Where the extractor is blind** (coverage gaps), not where agents disagree. |
| `rg` / `git grep` | Fast recursive **text** search when you know the string. |

## Example

```sh
eg query conflicts src/auth/ --graph graph.jsonl
eg query conflicts login --graph graph.jsonl --format text
eg query conflicts src/auth/token.rs --data-dir .egregore --include-resolved
```

```json
{
  "ok": true,
  "scope": "src/auth/",
  "scope_kind": "subsystem",
  "target_handle": "src/auth",
  "target_ids": ["codegraph:v4:aaaa...", "codegraph:v4:bbbb..."],
  "include_resolved": false,
  "conflicts": [
    {
      "conflict_id": ["agent_memory:v1:1111...", "verification:v1:2222..."],
      "edge_ids": ["agent_memory:v1:eeee..."],
      "parties": [
        {
          "record_id": "agent_memory:v1:1111...",
          "kind": "Failure",
          "party_class": "agent_authored",
          "trust_class": "agent_contradicted",
          "edge_role": "target",
          "citation_handles": ["agent_memory:v1:1111...", "codegraph:v4:aaaa..."],
          "author": "agent_1",
          "observed_at": "2026-01-03T00:00:00Z",
          "confidence": "0.6",
          "target_handle": "src/auth",
          "summary": "Failure by agent_1",
          "summary_hash": "blake3:..."
        },
        {
          "record_id": "verification:v1:2222...",
          "kind": "TestRun",
          "party_class": "verification",
          "trust_class": "verification_evidence",
          "edge_role": "source",
          "citation_handles": ["verification:v1:2222...", "codegraph:v4:aaaa..."],
          "author": "agent_3",
          "observed_at": "2026-01-04T00:00:00Z",
          "confidence": "1.0",
          "target_handle": "src/auth",
          "summary": "cargo test login passed"
        }
      ],
      "resolved": false,
      "resolved_by": []
    }
  ],
  "count": 1,
  "counts": {
    "contradicts_edges_considered": 2,
    "conflicts_found": 1,
    "conflicts_returned": 1,
    "conflicts_resolved_excluded": 0,
    "edges_skipped_ineligible_party": 1
  },
  "diagnostics": []
}
```

## Scope

This slice consumes existing agent-memory, verification, evidence-link, and
trust contracts. It introduces no new graph domain, importer, edge
vocabulary, trust model, hosted service, LLM-generated answer, or language
expansion. Issue #232.
