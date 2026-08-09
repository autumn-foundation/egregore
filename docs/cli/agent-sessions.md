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

## Shortest local workflow

The lane reads what is already recorded — it imports nothing and mints no
edges — so a useful digest needs three producers to have run first:

```powershell
# 1. Code facts: the Repository / File / Symbol nodes repo scoping resolves against.
eg scan . --out code.jsonl

# 2. Agent memory: sessions, runs, turns, observations, decisions, failures.
#    (or import-traj / import-codex / import-antigravity)
eg import-claude-code <transcript>.jsonl --out memory.jsonl

# 3. Evidence links: this is what mints the MENTIONS_SYMBOL / TOUCHED_FILE /
#    FAILED_ON citations that tie a memory record to code — WITHOUT this pass a
#    session has nothing to resolve through.
eg link-evidence --code-graph code.jsonl --evidence memory.jsonl --out links.jsonl

# 4. One corpus, then the query.
Get-Content code.jsonl, memory.jsonl, links.jsonl | Set-Content combined.jsonl
eg query sessions <REPO> --graph combined.jsonl
```

Imported sessions with **no** `link-evidence` pass carry no code citation and no
task reference, so they resolve to no repository at all and land under the
`unresolved_repository_scope` diagnostic **by design** — that is the honest
answer ("we cannot attribute this session"), not a bug and not a silent drop.
The same holds for the `--data-dir` path: ingest the same three graphs into one
store and query it with `--data-dir` instead of `--graph`.

## How a session is scoped to a repository

Agent-memory records carry no repository field, so repo scope is **derived from
existing edges** — never from name or path matching, and never guessed:

1. **Membership** is edge-derived only: a record belongs to a session through
   any `AUTHORED_BY` / `SESSION_OF` chain of **at most three hops**, whatever
   the intermediate record kinds — the deepest recorded shape is
   `X -AUTHORED_BY-> AgentTurn -AUTHORED_BY-> AgentRun -SESSION_OF-> Session`,
   but nothing about the walk is keyed on those kinds, so both the canonical
   `AgentSession -> Agent` shape and the legacy trajectory-importer
   `AgentRun -> AgentSession` shape are covered, as is any other chain of the
   same two labels. A record merely *stamped* with a matching `session_id`
   string but reachable by no edge is **not** a member; it is counted once
   under the `unlinked_session_stamped_records` diagnostic.
2. **Code citations** by any member — `MENTIONS_SYMBOL`, `TOUCHED_FILE`,
   `OBSERVES`, `FAILED_ON`, as graph edge records *or* on-node evidence links —
   resolve through the repository index (`owner_of`) to the owning repository:
   basis `code_citation`. (The issue's acceptance criteria name a
   `MENTIONS_FILE` label; **no such label exists in the schema** — `TOUCHED_FILE`
   is the real file-citation label and is what this lane consults.)
3. **Task references**: one hop through a member's `REFERENCES_TASK` to the
   project `Task`, then that task's own `MENTIONS_SYMBOL` / `TOUCHES_FILE` code
   citations: basis `task_reference`. The traversal never continues from one
   task to another.

A session appears in the digest of **every** repository in its derived scope
(cross-repo work is reported truthfully in each, with the full
`repository_scope` set on the row). `scope_basis` is the **union** of every
basis; `scope_basis_by_repository` reports the basis set **per repository**, so
a session code-citing repo A and reaching repo B only through a task shows
`{A: ["code_citation"], B: ["task_reference"]}` and repo B's digest never
claims a code citation that is not a fact about B.

A session with **no** derivable repository is excluded from every digest and
reported under the `unresolved_repository_scope` diagnostic — never silently
attributed. That diagnostic lists the same repo-unattributed session IDs in
**every** repository's digest, by design: those sessions belong to no
repository, so there is no digest they could belong to instead, and the listed
values are opaque record handles carrying no repository information.

Over a `scan-history` store the code-side attribution join is effectively the
**union of all commit snapshots**: this slice has no corpus flags
(`--all-history` / `--at-head`), so a citation into a symbol that existed only
at an earlier commit still confers scope.

## Ordering, time bounds, and limit

Sessions carry no native time-bound fields, so bounds are **derived**:
`first_activity` / `last_activity` are the min/max parseable `observed_at` over
the session node and all its members (runs and turns included; `executed_at` on
verification records is never folded in), with the transaction-time pair
`first_ingested_at` / `last_ingested_at` reported separately. `time_basis` is
`derived_from_member_observed_at`, or `absent` when nothing parsed
(`time_source_count` says how many values fed the bounds — `observed_at` only).

All four time-bound fields are re-rendered from the parsed instant with
`SecondsFormat::AutoSi`, so **sub-second precision is preserved**: importers
emit millisecond timestamps, and truncating them to whole seconds would report
two distinct instants as one string. A whole-second value renders with no
fractional part at all, exactly as before.

Rows are ordered by `last_activity` **descending** (timestamp-less sessions sort
last), tie-broken by `session_record_id` ascending, so equal timestamps never
reorder. All comparisons are by parsed UTC instant — the **original** parsed
value, never a re-parse of the rendered string — so no rendering choice can turn
a sub-second difference into an artificial tie. `--limit` (default 20, max 200)
truncates after ordering with a `results_truncated` diagnostic carrying the true
total; per-row `runs` and `tasks` arrays are independently capped at 20 with
`runs_truncated` / `tasks_truncated` diagnostics.

Row-scoped diagnostics (`unparseable_timestamp`, `outcome_not_enum_shaped`,
`runs_truncated`, `tasks_truncated`) are attached to their row and harvested
**after** ordering and truncation, so a `--limit`ed answer never cites a session
or run it does not contain. The envelope-level codes
(`unresolved_repository_scope`, `unlinked_session_stamped_records`,
`results_truncated`, `no_sessions`) describe the digest as a whole and are
unaffected by truncation.

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

**Runs have no end bound.** A run row carries `observed_at` and nothing else
temporal: no trunk importer records a `finished_at` on `AgentRun` (and
`started_at`, where present, is an import-time fallback rather than a recorded
run start), so a run duration or end instant would have to be invented. Each
run's `observed_at` is therefore the only load-bearing run instant, and it is
re-rendered from the **parsed** value — an absent, empty, or non-RFC-3339 stored
value reports `null` (counted under `unparseable_timestamp`), never the stored
bytes verbatim.

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
+ `time_basis` + `time_source_count`, `repository_scope` + `scope_basis` +
`scope_basis_by_repository`, `run_status` + `runs[]`, `tasks[]` (each with
`record_id`, validated `status`, `status_recorded`, and
`trust_class: "project_state"`), `aggregation_scope`, and `record_counts`
(`observation` / `decision` / `failure` / `lesson`).

| Row field | Contract |
|-----------|----------|
| `scope_basis` | The sorted **union** of every basis across every repository in scope. Says how the session reached code *at all*, not how it reached the repository you queried |
| `scope_basis_by_repository` | An object keyed by repository record ID (keys and values both sorted) giving the basis set that put **that** repository in scope. Read this, not the union, when reasoning about one repository |
| `aggregation_scope` | Always `"whole_session"`. `record_counts`, `runs`, `tasks`, and the four time bounds aggregate over the session's **full** edge-derived membership, regardless of the queried repository — a cross-repo session reports the same totals in every digest it appears in. They are not per-repository slices. This slice **discloses** the aggregation rather than computing per-repository counts; consult `repository_scope` together with `scope_basis_by_repository` to spot such a session |

An out-of-vocabulary or absent task status is reported as `"unknown"` with
`status_recorded: false` — the closed vocabulary is
`open` / `in_progress` / `blocked` / `closed_completed` / `closed_dropped` /
`unknown`, shared with the project importer so the two can never drift.

### Diagnostic codes

| Code | Meaning |
|------|---------|
| `no_sessions` | The repository resolved but has zero scoped sessions — an explicit empty success (exit 0), never a fabricated row |
| `unresolved_repository_scope` | Sessions with no derivable repository, listed by record ID, excluded from every digest. The same list appears in every repository's digest by design — those sessions belong to no repository, and the listed values are opaque handles |
| `unlinked_session_stamped_records` | Live records stamped with a scoped session's `session_id` string but reachable by no edge path |
| `results_truncated` / `runs_truncated` / `tasks_truncated` | A cap fired; carries `matched`, `returned`, `limit` |
| `outcome_not_enum_shaped` | A run summary claimed an outcome but did not match the producer template; the malformed bytes are never echoed |
| `unparseable_timestamp` | A member carried a non-RFC-3339 timestamp field (`observed_at` **or** `ingested_at`); skipped for bounds, counted per session. The unreadable value is never echoed |

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
summaries are reduced to a structured label plus a hash, and a run's stored
`observed_at` is parsed and re-rendered rather than forwarded verbatim.

`agent_id` and `session_id` are unbounded importer-supplied strings. The
core-computed `summary_label` interpolates them, so its `who` component is
capped at 128 characters with a `…` marker — identically on both transports.
The `--format text` renderer additionally replaces control characters with `·`
in `agent_id`, `session_id`, and `summary_label`, so an embedded newline or ANSI
escape cannot forge an output line or drive the reader's terminal; `--format
json` keeps the raw values (serde escapes control characters).

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
transports cannot drift. A non-string selector is a 400 `bad_request` naming
the key the caller actually sent (`params.repo` **or** `params.repository_id`);
a non-integer `limit` is a 400 `bad_request`, and a well-formed integer outside
`1..=200` — negative included — is a 400 `invalid_limit`, the same distinction
the CLI draws. One known gap: an integer literal wider than `u64` (e.g.
`18446744073709551616`) is, once parsed, indistinguishable from an ordinary
whole-valued float (`1.0`) without the `arbitrary_precision` serde_json
feature this crate does not enable, so it stays `bad_request` rather than
being reclassified — an honest limitation, not a guess. The row cap is
further bounded by the request's `budget.max_results` (the smaller of the two
applies, with `results_truncated` naming it), and a `limit` of `0` truncates to
an empty `sessions` array without ever being confused for `no_sessions` — that
code means the repository has no scoped sessions at all, not that the answer
was capped away. This lane has no temporal selectors: `as_of.valid_time` is
rejected with `not_implemented` rather than silently answered. See
`docs/schema/daemon-query.md`.

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
