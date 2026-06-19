# Protected artifact schema (v1)

This document defines the `ProtectedHandle` record schema, store layout, and handle identity
rule used by `eg protected` (issue #60).

## `ProtectedHandle` fields

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `handle` | string | yes | `"protected:v1:<blake3 hex>"` — content-addressed stable ID |
| `schema_version` | u32 | yes | Always `1` |
| `source_class` | string | yes | One of `transcript`, `command_output`, `patch`, `task_narrative`, `report` |
| `source_path` | string | when known | Repo-relative or absolute source path at capture time; `null` for synthetic payloads |
| `content_hash` | string | yes | BLAKE3 hex of the full raw payload bytes |
| `byte_len` | u64 | yes | Exact byte length of the raw payload |
| `captured_at` | string | yes | RFC 3339 wall-clock time of first capture; NOT part of handle identity |
| `producer_id` | string | yes | Stable identity of the capturing producer (e.g. operator email, agent ID) |
| `producer_version` | string | yes | Egregore version string at capture time (`CARGO_PKG_VERSION`) |

## Handle identity rule

The stable handle is computed as:

```text
handle = "protected:v1:" + BLAKE3(source_class + "\n" + content_hash + "\n" + source_path_or_empty)
```

`captured_at` is **intentionally excluded** from the identity hash so that re-capturing an
unchanged source yields the same handle and produces zero duplicate manifest entries
(idempotency contract, AC7).  `captured_at` is first-capture-wins.

## Store layout

```text
<store>/blobs/<content_hash>   — raw bytes; keyed by BLAKE3 hex of content
<store>/manifest.jsonl         — one ProtectedHandle JSON per line; canonical-sorted (lexicographic)
```

### Authorization

There is no separate ACL file.  Authorization is **derived from the manifest**: an operator may
retrieve payloads iff it is the `producer_id` of at least one committed record.  Because the
authorization and the handle are the same record, a single atomic `manifest.jsonl` write commits
both — authorization is crash-atomic and cannot desync (no missing/empty/orphaned ACL states).

### Canonical ordering

`manifest.jsonl` is sorted **lexicographically by JSON line text**, the same rule used by
`Graph::to_jsonl` for graph JSONL files.  De-duplication is applied before each write.  This
ensures byte-identical output across re-imports and is how AC7 is verified.

## Payload classes

| `source_class` value | Meaning |
|----------------------|---------|
| `transcript` | Agent-session transcript text (Claude Code, Codex, traj file body) |
| `command_output` | Command standard output or error (e.g. `cargo test` stdout) |
| `patch` | Raw patch bytes (unified diff) |
| `task_narrative` | Task or issue narrative (markdown description, AC list, PRD body) |
| `report` | Generated report (analysis, scan summary, evaluation result) |

## Schema version

`PROTECTED_SCHEMA_VERSION = 1`.  This constant is recorded in every `ProtectedHandle` record.
Future additions to the record shape must increment this version and document a migration path.

## Security invariants

* Blobs are stored as plain bytes (no encryption).  Operators who need encryption at rest should
  layer a filesystem-level solution; issue #54 owns the Egregore encrypted-store workflow.
* Redaction runs *before* capture; this store does not re-apply or bypass the redaction gate
  (issue #41).
* `get` verifies the BLAKE3 content hash before returning bytes.  A `hash_mismatch` error is
  returned when stored bytes do not match the recorded hash; no corrupted bytes are ever returned.
* Error envelopes never echo raw payload bytes, bearer tokens, patch hunks, secrets, or command
  output.

## Relationship to other schema domains

This schema is entirely separate from the graph JSONL schema (`codegraph:v4`, `agent_memory:v1`,
etc.).  Protected handles are never written to graph JSONL, never ingested into AletheiaDB, and
never indexed for semantic search.  The only way to access raw bytes is through an explicit
`eg protected get` call with a registered operator identity.
