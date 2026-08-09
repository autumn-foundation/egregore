# eg query sessions

`eg query sessions <REPO>` returns a recency-ordered digest of the recent agent
sessions recorded for one repository (issue #112): who worked here lately, when,
with what recorded run outcome, which tasks they touched and where those tasks
stand now, and bounded per-kind counts of what each session observed, decided,
and failed at. It is the resume-a-repo lane — the answer an agent (or its
operator) reads before picking up where the last session left off, instead of
re-reading raw transcripts or guessing from `git log`. Local-first; no network
access, no hosted indexing, no embeddings.

> Every session row is an **agent claim**, never verification: an
> `outcome=success` is what the agent recorded, not proof the work is correct,
> complete, or tested. Task `status` is a recorded **project-domain fact**
> carried through verbatim, not a correctness judgement. An absent citation is
> not evidence that no work happened.

## Synopsis

```powershell
eg query sessions <REPO> --graph graph.jsonl
eg query sessions <REPO> --data-dir .egregore [--limit 20] [--format json|text]
```

`<REPO>` is any selector the shared repository-identity contract accepts: the
stable `Repository` record ID, the identity basename / operator override, the
normalized remote URL, the root commit SHA, the canonical path, or a
final-path-segment shorthand (see `docs/cli/query.md` §"Repository scope").

## How a session is scoped to a repository

Agent-memory records carry no repository field, so repo scope is **derived from
existing edges** — never from name or path matching, and never guessed:

1. **Membership** is edge-derived only: a record belongs to a session through
   `AUTHORED_BY` / `SESSION_OF` chains, at most three hops
   (`X -AUTHORED_BY-> AgentTurn -AUTHORED_BY-> AgentRun -SESSION_OF-> Session`),
   covering both the canonical `AgentSession -> Agent` shape and the legacy
   trajectory-importer `AgentRun -> AgentSession` shape. A record merely
   *stamped* with a matching `session_id` string but reachable by no edge is
   **not** a member; it is counted once under the
   `unlinked_session_stamped_records` diagnostic.
2. **Code citations** by any member — `MENTIONS_SYMBOL`, `TOUCHED_FILE`,
   `OBSERVES`, `FAILED_ON`, as graph edge records *or* on-node evidence links —
   resolve through the repository index (`owner_of`) to the owning repository:
   basis `code_citation`.
3. **Task references**: one hop through a member's `REFERENCES_TASK` to the
   project `Task`, then that task's own `MENTIONS_SYMBOL` / `TOUCHES_FILE` code
   citations: basis `task_reference`. The traversal never continues from one
   task to another.

A session appears in the digest of **every** repository in its derived scope
(cross-repo work is reported truthfully in each, with the full
`repository_scope` set on the row). A session with **no** derivable repository
is excluded from every digest and reported under the
`unresolved_repository_scope` diagnostic — never silently attributed.

## Ordering, time bounds, and limit

Sessions carry no native time-bound fields, so bounds are **derived**:
`first_activity` / `last_activity` are the min/max parseable `observed_at` over
the session node and all its members (runs and turns included; `executed_at` on
verification records is never folded in), with the transaction-time pair
`first_ingested_at` / `last_ingested_at` reported separately. `time_basis` is
`derived_from_member_observed_at`, or `absent` when nothing parsed
(`time_source_count` says how many values fed the bounds).

Rows are ordered by `last_activity` **descending** (timestamp-less sessions sort
last), tie-broken by `session_record_id` ascending, so equal timestamps never
reorder. All comparisons are by parsed UTC instant, never raw RFC 3339 string
order. `--limit` (default 20, max 200) truncates after ordering with a
`results_truncated` diagnostic carrying the true total; per-row `runs` and
`tasks` arrays are independently capped at 20 with `runs_truncated` /
`tasks_truncated` diagnostics.

## Run outcome honesty

`AgentRun` outcome/exit_reason are not structured fields in the schema — the
trajectory importer interpolates them into the run summary. The digest parses
**only** the exact producer template `AgentRun outcome=<X> exit_reason=<Y>`
where both tokens match `[A-Za-z0-9_.:-]{1,64}`; anything else (including the
claude-code importer's constant `AgentRun claude-code` summary — the **common**
case for claude-code imports, an expectation, not a bug) reports
`outcome: null` with `run_status: "outcome_unrecorded"`, plus an
`outcome_not_enum_shaped` diagnostic when the summary claimed an outcome but was
malformed. A session with no run at all is `run_status: "run_absent"` with
`runs: []` — never a fabricated outcome. Two or more runs report
`run_status: "multiple_runs"` with each run listed.

## Response shape

One pretty-printed JSON envelope:

| Field | Contents |
|-------|----------|
| `ok`, `lane` | `true`, `"sessions"` |
| `repository_id`, `repository` | The resolved `Repository` record ID and display handle |
| `disclaimer` | The standing epistemic disclaimer, verbatim on every answer |
| `unsupported_count_kinds` | `["lesson"]` — no `Lesson` node kind exists in this schema, so `record_counts.lesson` is always `null` (never a fabricated `0`) |
| `sessions` | Ordered session rows (below) |
| `diagnostics` | Machine-readable diagnostics, sorted by `(code, session, run)` |

Each session row carries: `session_record_id`, `trust_class`
(`agent_authored`), `agent_record_id` (the `Agent` node reached by the
session's own `SESSION_OF` edge, `null` when absent — a stamped `agent_id`
string alone is not a citable agent handle), `agent_id` / `session_id` string
handles, `summary_label` + `summary_hash` (a structured label plus a BLAKE3
handle — the raw summary never leaves the store), the four derived time fields
+ `time_basis` + `time_source_count`, `repository_scope` + `scope_basis`,
`run_status` + `runs[]`, `tasks[]` (each with `record_id`, validated `status`,
`status_recorded`, and `trust_class: "project_state"`), and `record_counts`
(`observation` / `decision` / `failure` / `lesson`).

An out-of-vocabulary or absent task status is reported as `"unknown"` with
`status_recorded: false` — the closed vocabulary is
`open` / `in_progress` / `blocked` / `closed_completed` / `closed_dropped` /
`unknown`, shared with the project importer so the two can never drift.

### Diagnostic codes

| Code | Meaning |
|------|---------|
| `no_sessions` | The repository resolved but has zero scoped sessions — an explicit empty success (exit 0), never a fabricated row |
| `unresolved_repository_scope` | Sessions with no derivable repository, listed by record ID, excluded from every digest |
| `unlinked_session_stamped_records` | Live records stamped with a scoped session's `session_id` string but reachable by no edge path |
| `results_truncated` / `runs_truncated` / `tasks_truncated` | A cap fired; carries `matched`, `returned`, `limit` |
| `outcome_not_enum_shaped` | A run summary claimed an outcome but did not match the producer template; the malformed bytes are never echoed |
| `unparseable_timestamp` | A member carried a non-RFC-3339 `observed_at`; skipped for bounds, counted per session |

### Exit codes

| Condition | Exit | Output |
|-----------|------|--------|
| ≥1 session row, or explicit empty (`no_sessions`) | 0 | Envelope on stdout |
| Unknown repository selector | 1 | `{"code":"unknown_repository_selector",...}` on stderr |
| Ambiguous repository selector | 1 | `{"code":"ambiguous_repository_selector","candidates":[...]}` on stderr |
| `--limit` outside 1..=200 | 1 | `{"ok":false,"error":{"code":"invalid_limit",...}}` on stderr |
| Missing/both of `--graph`/`--data-dir`, unreadable input | 1 | Load-error diagnostic |

## Safety: no raw payloads

Output is allow-list only: record IDs, agent/session handles, time fields,
status/outcome enums, bounded counts, structured summary labels, BLAKE3 hashes,
and repository record IDs. Raw transcript text, observation/decision/failure
text, command output, patch hunks, tool-call arguments, task titles,
commit-message bodies, env values, and tokens never appear. Session and run
summaries are reduced to a structured label plus a hash.

## Read-only and deterministic

Strictly read-only: `--graph` never writes, and `--data-dir` reads a throwaway
temporary copy so the live store stays byte-for-byte untouched. Re-running the
identical query against an unchanged store/graph produces byte-identical output.
This slice has **no** temporal selectors (`--at` / `--as-of`) and no corpus
flags: the answer is the current recorded state (who-imports precedent).

## Daemon verb

`agent_sessions_for_repo` (POST `/v1/query`) is the daemon face of this lane —
the formerly reserved verb, now implemented. Params:
`{"repo": "<selector>"}` (alias `repository_id`; `repo` wins when both are
present) and optional `limit`. The result carries the identical
`sessions` / `diagnostics` values the CLI envelope serializes, so the two
transports cannot drift. See `docs/schema/daemon-query.md`.

## When to use this versus neighboring lanes

| Reach for | When you want |
|-----------|---------------|
| `eg query sessions <repo>` | Resume a repo: who did what recently, with what recorded outcome, and which tasks are in flight |
| `eg query task <task>` (#48) | One task's acceptance criteria and evidence chain |
| `eg query failures <handle>` (#63) | Prior failed attempts against one symbol/file before retrying |
| `eg query semantic-memory <text>` (#91) | Memory recall by meaning, not by repo/session |
| `eg query changes <range>` (#62) | What changed in a commit range, joined to code facts |
| `git log` / reflog / raw transcripts | Commit/command history with no task, outcome, or memory joins — the boring substitute this lane exists to beat |

## Scope

This slice consumes existing contracts only: agent-memory, agent-actions,
project-graph, repository-identity, daemon-query, schema-versioning, and
redaction. It introduces no new graph domain, node kind, edge vocabulary,
importer, trust class, hosted service, or LLM-generated summary. `Lesson`
counts stay `null` until a `Lesson` node kind exists in the schema; repo
attribution for agent-memory records stays derived until a schema issue adds a
first-class field (the log domain's `repository_id` precedent).
