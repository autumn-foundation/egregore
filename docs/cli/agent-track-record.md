# eg query track-record

Rank the agents contributing to a shared store **by the downstream fate of
their observations**: which agents produce promotable, durable knowledge,
and which produce noise that gets rejected or superseded — without reading
their transcripts.

> **Reports, never verdicts.** Every row is a deterministic count over
> recorded claims and recorded facts, trust-separated by section. An
> agent's counts are never presented as proof its claims are true; a
> passing verification is evidence a check ran, never proof code is
> correct. This slice reports; gating, weighting, throttling, or disabling
> agents belongs to the orchestrator, and there is no learned reputation
> score here — only counts over existing edges.

## Synopsis

```text
eg query track-record --graph <PATH>    [--repo <SELECTOR>] [--format json|text]
eg query track-record --data-dir <DIR>  [--repo <SELECTOR>] [--format json|text]
eg query agents        ...              # alias, byte-identical output
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the evaluated records to one
repository in a multi-repo store; an unknown or ambiguous selector is
rejected with a machine-readable stderr diagnostic (exit 1), never resolved
implicitly. JSON is the default; `--format text` renders a skimmable table.
Strictly read-only: no records, indexes, or runtime files are created,
modified, or deleted. Requires no network and no `--embed` store.

| Condition | Exit | Output |
|-----------|------|--------|
| Track record computed — including an **empty** one | `0` | JSON on stdout, `ok:true` |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input | `1` | Error message on stderr |

An empty agent set is an explicit verdict, never silence: `ok:true`, an
empty `agents` array, and a `no_agents` diagnostic — distinct from a
store-absent error (exit 1).

## What each row counts

One row per `agent_id` (with `agent_kind`), in canonical byte-sorted
`agent_id` order:

- **`observations_written`** — `Observation` nodes stamped with this
  `agent_id`. *Trust basis: `agent_authored_claims`.*
- **`promotion_outcomes`** — for `PromoteCandidate`s whose supporting
  evidence includes this agent's observations, the terminal verdict of the
  latest `PromotionDecision`, split into the closed vocabulary `approved`,
  `edited_then_approved`, `rejected`, `deferred`, `expired`. A candidate
  supported by several agents credits each of them exactly once; a
  candidate with no terminal decision is reported under the
  `candidates_without_terminal_decision` diagnostic, never forced into a
  verdict. *Trust basis: `recorded_operator_decisions`.*
- **`superseded_observations`** — the agent's observations carrying
  `superseded_by`. *Trust basis: `agent_authored_claims`.*
- **`verification_outcomes`** — `passed` / `failed` / `inconclusive`
  outcomes of verification-domain records (`TestRun`, `CommandRun`,
  `CIStatus`, `BenchmarkRun`, `CoverageReport`, `ProofResult`) reachable
  from the agent's sessions via `AUTHORED_BY` / `SESSION_OF` edges (at most
  three hops), classified by the shared pass/fail rule (`query::trust`):
  an absent or unrecognized outcome is fail-closed `inconclusive`, never a
  pass. *Trust basis: `verification_evidence`.*

Every nonzero bucket cites resolvable `record_id` handles (`record_ids`,
plus `candidate_ids` / `decision_ids` for promotion buckets); every zero
bucket cites none. Counts always equal the length of their handle lists.

## Attribution rules

- **Agent identity** is the stamped `agent_id` provenance string. An
  observation with no `agent_id` cannot be attributed and is reported under
  the `observations_without_agent_id` diagnostic.
- **Supporting evidence** for a candidate is read from both
  representations importers write — the on-node `supporting_evidence` links
  and the `ProposedBy` edges — deduplicated by record id.
- **Latest decision wins**: among a candidate's decisions, the greatest
  `decided_at` (RFC 3339 instants sort lexicographically) decides, with the
  record id as the deterministic tie-break. An earlier decision's verdict
  never leaks into a bucket.
- **Session membership is edge-derived only**, like `eg query sessions`: a
  verification merely stamped with a matching `session_id` string is not a
  member. A verification reaching no session is reported under the
  `verifications_without_session` diagnostic.
- **`agent_kind`** is the lexicographically smallest kind observed on the
  agent's records (`null` when none is stamped) — deterministic, never
  synthesized.
- **Repository scope** (`--repo`): an observation or verification record is
  in scope when it cites — via on-node evidence links or outbound
  `OBSERVES` / `MENTIONS_SYMBOL` / `TOUCHED_FILE` / `FAILED_ON` /
  `TOUCHES_FILE` edges — a record owned by the selected repository. A
  promotion credit follows its supporting observations, so it inherits the
  scope. Records with no resolvable repository are excluded under `--repo`,
  never silently counted.

## Determinism

Rows sort by `agent_id`; every handle list is sorted; multi-decision
candidates resolve by `(decided_at, record_id)`; nodes coalesce
last-write-wins by record id so an append-only `--graph` agrees with the
embedded current-state read. No input order leaks into the answer:
repeated runs are byte-identical across runs and platforms.

## Operator recipe

Rank every contributing agent by observation-acceptance rate with one
command:

```text
approved + edited_then_approved promotions ÷ promotion-decided observations
```

computed per row from `promotion_outcomes`; 100% of nonzero buckets cite
resolvable handles. A rate is undefined (not zero) for an agent with no
decided promotions.

## Exit codes

- `0` — rows returned, or the explicit empty verdict (`no_agents`).
- `1` — unknown / ambiguous repository selector, or an unreadable graph
  input (machine-readable JSON on stderr).
