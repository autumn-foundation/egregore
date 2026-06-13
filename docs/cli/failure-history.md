# eg query failures

Surface **prior failed attempts** linked to a code or task handle — answer the
operator-visible question *"what failed here before, and what evidence proves
that failure happened?"* — starting from a symbol, file, or task handle.
Local-first; no network access.

> **Absence of a prior failure is not evidence of safety.** An empty result means
> the local graph has no recorded failure for this handle, not that the code or
> task is correct. This query reports what the graph can and cannot show; it never
> infers a failure cause when supporting evidence is absent.

## Synopsis

```text
eg query failures <HANDLE> --graph <PATH> [--repo <SELECTOR>]
eg query failures <HANDLE> --data-dir <DIR> [--repo <SELECTOR>]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`).

## Handle resolution

`<HANDLE>` accepts these handle types, tried in order (AC2):

1. **Canonical code record ID** — `codegraph:v<N>:<hex>` naming a `Symbol` or
   `File`.
2. **Task / task-source handle** — a canonical Task ID (`project:v<N>:<hex>`), a
   GitHub URL or short handle (`owner/repo#num`), or a local JSONL handle
   (`path.jsonl:local_id`). Resolved through the same contract as
   [`eg query task`](task-queries.md), and expanded to the task's
   `AcceptanceCriterion` records — including the verifications those ACs close
   via `CLOSES_ACCEPTANCE_CRITERION` — so failures and superseding runs attached
   to an AC are included.
3. **Repo-relative file path** — e.g. `src/lib.rs`.
4. **Exact symbol name** — e.g. `foo`. Several symbols of the same name in one
   repository form a multi-target query; the same name across repositories is
   ambiguous unless `--repo` is given.
5. **Source / provenance handle** — a string matching a failure's
   `source_handle`, `source_artifact_path`, `source_artifact_hash`, or
   `session_id`.

`--repo <SELECTOR>` restricts file/symbol resolution to one repository (issue
#67); without it, a file or symbol that matches more than one repository is
reported as `Ambiguous` rather than resolved implicitly.

| Condition | Exit | Output |
|-----------|------|--------|
| Success (including a resolved target with **no** recorded failures) | `0` | Failure-history JSON on stdout, `ok:true` |
| Empty handle or malformed canonical ID | `1` | `{"Unsupported":{...}}` on stderr |
| Handle matches targets in more than one repository | `1` | `{"Ambiguous":{...}}` on stderr |
| Handle resolves to no live target in the store | `2` | `{"ok":false,"error":{"code":"no_match",...}}` on stdout |
| Handle names a tombstoned (deleted) target | `2` | `{"ok":false,"error":{"code":"stale_handle",...}}` on stdout |

A resolved target that simply has no recorded failures is a **real, successful,
empty answer** (exit 0), distinct from a handle that resolves to nothing (exit 2).
The workflow never guesses a replacement for an unsupported, ambiguous, stale, or
missing handle, and never falls back to transcript text search (AC6).

## Response shape

The response separates **failure facts from interpretation** so neither a runtime
failure nor an agent-authored claim is presented as source truth or
task-completion proof (AC4). Every section is canonically ordered for determinism
(AC7); failed attempts are ordered oldest-first by their observed/executed time.

| Section | Contents | Trust class |
|---------|----------|-------------|
| `runtime_failures` | Verification-domain command/test/CI failures (`status` fail/error/timeout, or a nonzero `exit_code` when `status` is absent). | `verification_evidence` |
| `agent_failures` | Agent-authored `Failure` claims. | `agent_authored` |
| `superseding_successes` | Passing verifications (status `pass`, or a status-absent `CommandRun` with a zero `exit_code`) strictly **later** than a reached failure on a shared target — a pass with no failures, or one predating every failure, superseded nothing and is omitted. | `verification_evidence` |
| `patch_artifacts` | Patch artifacts and runtime evidence attached to a reached failure via `PRODUCED_PATCH` / `FAILED_ON`. | `artifact` |
| `diagnostics` | Stable codes for unresolved / stale / missing / protected / redacted handles. | — |
| `page` | Deterministic pagination block (`cursor`, `has_more`, `returned`). | — |
| `target_ids` / `target_type` | The resolved anchor record IDs and handle type. | — |

Each item in `runtime_failures` / `agent_failures` carries a `record_id`, a
non-empty `citable_handle`, the failure's time and provenance, a `matched_target`
(the anchor record ID it linked to), and a **read-time `resolution_status`**.

### Read-time `resolution_status` (AC5)

Each failed attempt is annotated `still_failing` or `since_resolved`:

- `since_resolved` — a passing verification on a **shared target handle** has an
  execution time strictly **after** the attempt; the attempt also carries
  `resolved_by` with that verification's record ID.
- `still_failing` — the conservative default whenever supersession cannot be
  proven, including when either side has a missing or unparseable timestamp.

The superseding success is surfaced as a **separate** item; the older failed
attempt is never deleted, hidden, or rewritten (AC5). A failure whose timestamp
cannot be parsed stays `still_failing` and emits a `missing_timestamp`
diagnostic — the query never claims resolution without evidence.

### Safety: no raw payloads (AC8)

Output never includes raw transcript text, raw command output, patch hunks, issue
bodies, PR comments, environment values, or bearer tokens. An agent `Failure`'s
raw `text` body (which holds a command-output excerpt) is never emitted; protected
payloads (command output, patch bytes) are referenced by **hash only**, flagged
`"protected": true`, and surfaced as `protected_payload` diagnostics. Only bounded
summaries, hashes, redaction markers, record IDs, and file/span handles appear.

### Diagnostic codes

| Code | Meaning |
|------|---------|
| `unresolved_evidence_link` | A failure's `evidence_links` target ID is absent from the store. |
| `evidence_target_unresolved` | An evidence link supplied only a triple, not a record ID; not resolved heuristically. |
| `stale_evidence_target` | An evidence target exists only as a tombstone; treated as deleted. |
| `missing_code_handle` / `missing_task_ref` | A resolved target ID has no live node. |
| `stale_code_handle` | A resolved target ID exists only as a tombstone. |
| `missing_timestamp` | A failure or success has no parseable execution/observation time. |
| `protected_payload` | A raw payload (stdout/stderr/patch) is withheld; the hash is the handle. |
| `redacted_payload` | The attempt carries a redaction policy version. |

## When to use this versus other tools

| Reach for | When you want |
|-----------|---------------|
| **`eg query failures`** (this) | **What failed here before**, and the evidence proving it — to avoid repeating a known-bad approach. |
| `eg query context` (#38) | General evidence-backed context for a **symbol**. |
| `eg query task` (#48) | Whether a **task / acceptance criterion** is actually complete. |
| commit-range context (#62) | What changed across a **commit range**. |
| `eg query memory` (#64) | Why an agent **believes one claim**. |
| `rg` / `git grep` | Fast recursive **text** search when you know the string. |
| `git log -S` | Commits where matching **text changed** (pickaxe). |
| CI / Actions logs | One run's **operational output**. |

`rg`, `git log -S`, and CI logs are fast and honest when you know what to look
for, but they do not connect failed agent attempts to code handles, tasks, patch
artifacts, verification records, redaction state, or later superseding evidence.
This query is local, handle-driven, redaction-safe, and queryable across sessions.

## Example

```sh
eg query failures codegraph:v4:abc123... --graph graph.jsonl
eg query failures src/lib.rs --graph graph.jsonl --repo repo-a
eg query failures '#63' --data-dir .egregore
```

```json
{
  "ok": true,
  "target_handle": "codegraph:v4:abc123...",
  "target_type": "symbol",
  "target_ids": ["codegraph:v4:abc123..."],
  "runtime_failures": [
    {
      "record_id": "verification:v1:rrrr...",
      "kind": "TestRun",
      "trust_class": "verification_evidence",
      "relation": "VALIDATED_BY",
      "citable_handle": "test_run",
      "status": "fail",
      "executed_at": "2026-01-15T00:00:00Z",
      "stdout_hash": "blake3:stdouthash",
      "protected": true,
      "resolution_status": "since_resolved",
      "resolved_by": "verification:v1:ssss...",
      "matched_target": "codegraph:v4:abc123..."
    }
  ],
  "agent_failures": [
    {
      "record_id": "agent_memory:v1:ffff...",
      "kind": "Failure",
      "trust_class": "agent_authored",
      "relation": "FAILED_ON",
      "citable_handle": "trajectories/run-1.traj",
      "summary": "Failure by agent_1:sess_1",
      "summary_hash": "blake3:...",
      "agent_id": "agent_1",
      "session_id": "sess_1",
      "observed_at": "2026-01-01T00:00:00Z",
      "source_artifact_path": "trajectories/run-1.traj",
      "failure_kind": "command_failure",
      "resolution_status": "since_resolved",
      "resolved_by": "verification:v1:ssss...",
      "matched_target": "codegraph:v4:abc123..."
    }
  ],
  "superseding_successes": [
    {
      "record_id": "verification:v1:ssss...",
      "kind": "TestRun",
      "trust_class": "verification_evidence",
      "relation": "VALIDATED_BY",
      "citable_handle": "test_run",
      "status": "pass"
    }
  ],
  "patch_artifacts": [
    {
      "record_id": "artifact:v1:pppp...",
      "kind": "PatchArtifact",
      "trust_class": "artifact",
      "relation": "PRODUCED_PATCH",
      "citable_handle": "blake3:patchhash",
      "patch_status": "rejected_validation",
      "patch_bytes_hash": "blake3:patchhash",
      "protected": true
    }
  ],
  "diagnostics": [
    {
      "code": "unresolved_evidence_link",
      "source_record_id": "agent_memory:v1:ffff...",
      "target_handle": "agent_memory:v1:0000...",
      "relation": "RELATES_TO",
      "target_domain": "agent_memory"
    }
  ],
  "page": { "cursor": null, "has_more": false, "returned": 4 }
}
```

## Scope

This slice consumes existing agent-memory, verification, artifact, project,
redaction, daemon-query, and evidence-link contracts. It introduces no new graph
domain, importer, edge vocabulary, trust model, hosted service, LLM-generated
answer, or language expansion (AC10).
