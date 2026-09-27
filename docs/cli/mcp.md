# `eg mcp` — the agent integration path (issue #260)

`eg mcp` is the **primary way a coding agent talks to Egregore**: a stdio
[Model Context Protocol](https://modelcontextprotocol.io/) server exposing
seven **read-only**, citation-bearing tools backed by the running local
daemon. Register it once in your agent host; the agent then calls Egregore's
evidence tools over MCP instead of shelling out to `eg query …` per question —
which keeps tool discovery, the structured-output contract, and the citation
envelope intact.

> Shipped (issue #53). Earlier design docs called this a "future" server —
> see the note at the bottom. The surface below is additive; the contract is
> not yet frozen (issue #194).

## The 5-step path (binary → connected agent → citation handle)

1. **Build** (once):

   ```sh
   cargo build --release
   # binaries: target/release/egregore and the short alias target/release/eg
   ```

2. **Start the daemon** for the store you want the agent to read:

   ```sh
   eg daemon start --data-dir /abs/path/to/.egregore
   eg daemon status --data-dir /abs/path/to/.egregore   # confirm it is up
   ```

   All seven tools fail closed when no daemon answers for `--data-dir`
   (see [Error envelope](#error-envelope) — gate on it, don't retry blindly).

3. **Register the server** in your agent host with one of the copy-paste
   configs below (replace the absolute paths).

4. **Smoke-test the handshake** over stdio:

   ```sh
   printf '%s\n' \
     '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
     '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
     '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
   | eg mcp --data-dir /abs/path/to/.egregore | head -3
   ```

   The first line must contain `"serverInfo":{"name":"egregore",…}` and the
   third must list exactly `inspect_store`, `symbol_context`, `task_evidence`,
   `store_freshness`, `failure_history`, `search_code`, `symbol_at`.
   (This is the same handshake `tests/integration/mcp_stdio.rs` proves on
   every CI run.)

5. **Issue one tool call** (via your agent host, or raw JSON-RPC):

   ```jsonc
   // tools/call
   { "name": "symbol_context",
     "arguments": { "symbol_name": "DaemonClient::from_data_dir",
                    "data_dir": "/abs/path/to/.egregore" } }
   ```

   The `content[0].text` payload is JSON with `"ok": true` and items carrying
   `record_id` plus a `repo_relative_path`/`span` citation handle — e.g.
   `"repo_relative_path": "src/daemon.rs"` with
   `"span": {"start_line": 1699, …}`.

## Server registration

Use absolute paths: agent hosts spawn the server with their own working
directory, so a relative binary path or `--data-dir` will resolve somewhere
unexpected. `eg` and `egregore` are the same binary; either works.

### Claude Code

Add to `.mcp.json` (project) or `~/.claude.json` (global), under `mcpServers`:

```json
{
  "mcpServers": {
    "egregore": {
      "command": "/abs/path/to/eg",
      "args": ["mcp", "--data-dir", "/abs/path/to/.egregore"]
    }
  }
}
```

Then `/mcp` in Claude Code lists `egregore`; call e.g.
`mcp__egregore__symbol_context` with `{"symbol_name": "<name>"}`.

### Codex CLI

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.egregore]
command = "/abs/path/to/eg"
args = ["mcp", "--data-dir", "/abs/path/to/.egregore"]
```

### Cursor

Add to `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "egregore": {
      "command": "/abs/path/to/eg",
      "args": ["mcp", "--data-dir", "/abs/path/to/.egregore"]
    }
  }
}
```

## Tools

Every tool takes an optional `data_dir` argument (string, defaults to the
`--data-dir` the server was started with) and returns a JSON text payload.
The top-level shape is stable: **`"ok": true`** with data fields, or
**`"ok": false`** with an `"error"` object — never a bare string, never an
HTTP-style status. The seven tool names are stable; the response is additive
(fields may be added, existing fields are not renamed or removed without a
contract change — tracked by issue #194).

### `inspect_store`

Store-inspection summary: record counts, domain breakdown, schema versions,
repository identities. Requires a running daemon.

Arguments: `{ "data_dir"?: string, "repo_path"?: string }`

Stable `ok: true` fields:

| Field | Meaning |
|---|---|
| `snapshot_timestamp` | RFC 3339 timestamp the daemon reported with the record snapshot |
| `records`, `nodes`, `edges`, `tombstones`, `diagnostics` | counts |
| `domain_counts` | per-domain-kind counts, keyed by display category (`"Deterministic Source Facts"`, `"Derived Measurements"`, `"Agent-Authored Claims"`, `"Project/Work State"`, `"Artifacts"`, `"Verification Evidence"`, `"User Context"`, `"Runtime Observations"`) |
| `schema_versions` | `"<domain>:<kind>:<version>" → count` |
| `unknown_schema_versions` | same key shape, for records the daemon flagged |
| `repositories` | `[{ "id", "identity_summary" }]` per repository node |
| `freshness` | store-freshness object (issue #220) — see [Freshness stamping](#freshness-stamping) |

### `symbol_context`

Evidence-backed context for a named code symbol, trust-separated by domain.
Every item carries `record_id`, a `trust` class, and at least one citation
handle; observations are agent-authored — never treat them as source truth.

Arguments: `{ "symbol_name": string (required, non-empty), "data_dir"?: string, "repo_path"?: string }`

Stable `ok: true` fields: `symbol_name`, then sections
`source_facts`, `topology_edges`, `observations`, `project_state`,
`artifacts`, `verification_evidence`, `drift_history`, `unresolved`,
plus the `freshness` object (issue #220) — see [Freshness stamping](#freshness-stamping).

Citation handles per item:

- code-graph facts: `record_id`, `repo_relative_path`, `span`
  (`start_byte`/`end_byte`/`start_line`/`end_line`), `git_commit`
- observations: `record_id`, `provenance_handle` (`agent_id:session_id`),
  `evidence_links[]` with `target_record_id`
- edges: `record_id`, `label`, `source_id`, `target_id`
- unresolved: `source_record_id`, `target_handle`, `verification_status:
  "unresolved"`

Stable error codes: `missing_argument` (empty `symbol_name`),
`no_match` (symbol absent), plus the daemon codes below.

### `task_evidence`

Evidence-backed context for a task: canonical record ID, GitHub URL, GitHub
short handle, or local JSONL handle.

Arguments: `{ "id_or_handle": string, "data_dir"?: string, "repo_path"?: string }`

Stable `ok: true` fields: `task_id`, then sections `tasks`,
`acceptance_criteria` (verified ACs carry an embedded `verification_record`),
`source_facts`, `observations`, `artifacts`, `verification_evidence`,
`reviews`, `external_links`, `unresolved`,
plus the `freshness` object (issue #220) — see [Freshness stamping](#freshness-stamping).

Stable error codes: `no_match`, `ambiguous_handle` (with `candidates`),
`unsupported_handle` (with `message`), plus the daemon codes below.

### `store_freshness`

Whole-store freshness verdict: does the store still match the working tree?
No symbol or task argument required. Requires a running daemon.

Arguments: `{ "data_dir"?: string, "repo_path"?: string }`

Stable `ok: true` fields: `freshness` — the same object the other tools stamp
(see [Freshness stamping](#freshness-stamping)).

### `failure_history`

Prior failed attempts for a symbol, file, or task handle (issue #188) —
the same answer as `eg query failures`, trust-separated for agents.
Requires a running daemon.

Arguments: `{ "handle": string (required, non-empty), "data_dir"?: string, "repo_path"?: string }`

The `handle` resolves in `eg query failures` order: canonical code record
ID, task/task-source handle, repo-relative file path, exact symbol name,
source/provenance handle.

Stable `ok: true` fields: `handle`, `target_type`
(`symbol` | `file` | `task` | `source`), `target_ids`, then the
trust-separated sections:

| Field | Meaning |
|---|---|
| `runtime_failures` | runtime verification failures (trust `verification_evidence`) |
| `agent_failures` | agent-authored failure claims (trust `agent_authored`) |
| `superseding_successes` | later passing verifications (never hide the older failures) |
| `patch_artifacts` | patch artifacts produced by the failures |
| `diagnostics` | stable failure-history diagnostics (`code`, `source_record_id`, `target_handle`) |
| `safety_note` | states the safety semantics: absence of recorded failure is not evidence of safety, and no failure cause is inferred |
| `freshness` | store-freshness object (issue #220) — see [Freshness stamping](#freshness-stamping) |

Every failure carries a stable `record_id`/evidence handle and a read-time
`resolution_status` (`still_failing` | `since_resolved`); sections are
canonically ordered oldest-first, so identical stores yield byte-identical
output. Rows are redaction-safe: no raw transcript text, command
stdout/stderr, or patch hunks appear anywhere — only hashes, handles,
bounded summaries, and redaction markers.

A resolved target with no recorded failures returns a successful,
explicitly-empty answer (empty sections + `safety_note`) — distinct from an
unresolvable handle.

Stable error codes: `missing_argument` (empty `handle`), `unsupported_handle`
(malformed canonical ID), `ambiguous_handle` (cross-repository collision, with
`candidates`), `no_match` (handle resolved to nothing live),
`stale_handle` (handle named only tombstoned records), plus the daemon codes
below.

### `search_code`

Semantic code search over the embedded vector index (issue #182) — the
natural-language entry point the MCP surface was missing. Embeds the query
with the default local model and ranks it through the daemon's
`semantic_search` verb, returning the same ranked matches as
`eg query semantic`: per-match `record_id`, `score`, `name`,
`repo_relative_path`, `span`, plus the per-row confidence rendering
(`confidence_band`, `selection_threshold`, `selection_basis`), the issue #243
`embedding_provenance` envelope (query/index model identity, metric, index
fingerprint, model-match verdict), and the issue #221 `confidence` verdict.
Requires a running daemon and a store ingested with `--embed`.

Arguments: `{ "query": string (required, non-empty), "data_dir"?: string, "limit"?: integer, "repo_path"?: string }`

`limit` defaults to 10 (the `eg query semantic` default) and is clamped to
100, the daemon verb's ceiling. An empty query is rejected with the stable
`missing_argument` envelope before any daemon is contacted.

Stable `ok: true` fields: `query` (echoed), `limit` (effective),
`matches` (ranked rows, daemon order), `embedding_provenance`,
`confidence`, `compatibility_note`, `freshness`. An index with no vectors
clearing the relevance floor is a successful, explicitly-empty answer
(`matches: []` with a `no_semantic_matches` message) — never a fabricated
record.

Stable error codes: `missing_semantic_index` (structural-only store, never
`--embed`ed — re-ingest to enable search), `semantic_index_unreadable`
(present but damaged index, refused as data loss, never misreported as
"never embedded"), `embedding_dimension_mismatch` (the issue #104 refusal:
query embedding width disagrees with the index), `embeddings_unavailable`
(the build lacks the `embeddings` feature or the model failed to load), plus
the daemon codes below.

Rows are redaction-safe: bounded handles only — no raw source text,
summaries, or payload bytes appear anywhere.

### `symbol_at`

Temporal symbol lookup (issue #181) — the temporal counterpart to
`symbol_context`, mirroring `eg query symbol --at` / `--as-of`. Looks up a
symbol's state at a Git commit or as of a valid-time instant and returns the
citable row for that version: `record_id`, `name`, `kind`,
`repo_relative_path`, `span`, `git_commit`, `valid_time`.

Arguments: `{ "symbol_name": string (required, non-empty), "commit"?: string,
"as_of"?: string, "data_dir"?: string, "repo_path"?: string }`

Exactly one of `commit` / `as_of` is required (both, or neither, is a
`bad_request` parameter error, raised before any daemon is contacted).
`commit` is a full SHA or unique prefix, as with `--at`; an ambiguous prefix
yields the stable `ambiguous_commit_prefix` code (with `commit_prefix` and
`matching_commits`). `as_of` is an RFC 3339 instant, as with `--as-of`; a
malformed instant is rejected with `invalid_timestamp`. A same-name collision
across repositories fails closed with `ambiguous_repository` (issue #67);
a miss yields `no_match`. The transaction-time selector (`tx_as_of`) is
rejected with `not_implemented` — the transaction-time axis currently covers
`eg query symbol` only.

Stable `ok: true` fields: `symbol_name` (echoed), `selector` (`kind` +
`value`, echoing the validated temporal selector), `symbol` (the citable
row), `freshness`.

## Freshness stamping

Every successful tool response carries a non-fatal, machine-readable
`freshness` **object** (issue #220): the store-freshness verdict from the
#186 contract (`fresh` / `stale_head` / `stale_dirty` / `unknown`), the stored
source-snapshot identity the answer was derived from, and the working-tree
state it was compared against — a trust signal, never suppression. A
non-`fresh` verdict still returns the full answer payload; gate trust in the
cited handles, not the answer itself.

- `repo_path` (optional on every tool) selects the working tree the verdict is
  computed against; it defaults to the MCP server's current directory,
  mirroring `eg freshness`'s default `.`.
- The verdict always agrees with `eg freshness` for the same store and tree —
  both run the identical classification path.
- `stored_snapshot.head.state` is `"pre_stamping"` for stores that predate
  snapshot stamping (explicit, never a silent absence, never a false `fresh`);
  `"no_git"` / `"unborn_head"` reuse the on-disk head serialization.
- The probe is strictly read-only and offline (`git rev-parse` /
  `git status` with `GIT_OPTIONAL_LOCKS=0`); stamping creates, modifies, or
  deletes nothing. For a fixed store + working-tree state the object is
  byte-identical across calls.

See [`docs/cli/freshness.md`](freshness.md) for the full contract.

## Error envelope

```jsonc
// daemon not reachable for this data dir:
{ "ok": false,
  "error": { "code": "daemon_not_running",
             "message": "<daemon client error text>" } }

// daemon metadata present but stale (crashed/killed daemon, moved data dir):
{ "ok": false,
  "error": { "code": "daemon_stale",
             "message": "<daemon client error text>" } }
```

**Agent gating rule:** on `ok: false`, branch on `error.code`. For
`daemon_not_running` / `daemon_stale`, tell the operator to run
`eg daemon start --data-dir <dir>` (or `eg doctor --data-dir <dir>` to
diagnose) — do not retry the tool call in a loop, do not shell out to
`eg query …` as a silent fallback, and do not fabricate an answer from
training data. For `no_match` / `ambiguous_handle`, surface the code and the
handle to the user; these are answerable states, not infrastructure failures.

## Read-only boundary

This slice ships **no write tools and no raw-text tools**: you cannot ingest
records, write observations, or fetch protected payloads (transcripts, patch
bodies, issue bodies) through MCP. Those surfaces exist only in the CLI and
daemon HTTP API today. The tool surface is additive by design:

- #181 — semantic-search tool
- #182 — temporal-lookup tool
- #183 — observation-write tool
- #220 — (further tool expansion)
- #194 — freeze/version the MCP tool I/O contract

## Design history note

`docs/adr/0003-egregore-daemon-shared-store.md` and
`docs/plans/2026-05-17-egregore-daemon-design.md` were written when the MCP
server was still "future". It shipped in issue #53 as a read-only stdio
server (`EgregoreMcpServer`, `src/mcp.rs`); both documents are annotated to
say so. Nothing about the daemon-first decision changed — the server discovers
the daemon per tool call via the same `DaemonClient::from_data_dir` contract.
