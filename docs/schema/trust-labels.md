# Derived Trust Labels on Context Answers

**Status:** Active. This document is the single source of truth for the `trust`
field emitted on every record row of a **cross-domain context answer**
(issue #114) — the surfaces listed in §7.

> **Name collision, stated up front.** Several *single-domain* lanes
> (`eg query deps`, `path`, `transitive-callers`, `transitive-callees`,
> `change-impact`, `who-imports`, `who-constructs`, `unsafe-sites`,
> `unwrap-expect`, `debt-markers`, `coupling`) already emit a field also spelled
> `trust`, carrying a *different* per-lane vocabulary (`source_fact`,
> `reachability_lead`, `dependency_lead`, `impact_lead`,
> `historical_co_change_lead`). Those are row-provenance markers for
> single-tier answers and predate this contract; they are **out of scope here
> and unchanged**. The vocabulary below is closed **for context answers only**.
> Note `source_fact` appears in both sets with different meanings — a consumer
> reading `trust` across lanes must key on the lane, not on the field name
> alone.

**Applies to:** the answer surfaces listed in §7. It is a **read-time query-answer
field**: it is never persisted, never enters a `GraphRecord` payload, and mints no
node kind, edge label, or record-level trust class. No `schema_version` changes.

---

## 1 — Domain is not trust

A record's `domain` (closed by issue #3) says **where a record lives**. It does not
say **how much of an agent's guesswork you must accept to believe the row**.

An `agent_memory` `Observation` may be an unverified hypothesis, a hypothesis
backed by a passing test run, or one that a later record contradicts. All three
share one domain. An agent that cannot tell them apart takes the cheapest path and
treats every row as equally true — the exact failure the PRD rejects.

Context answers therefore carry **two** fields on every record row:

| Field | Question it answers | Vocabulary |
|-------|---------------------|------------|
| `trust_class` | *What kind of record is this?* (provenance domain) | `source_fact`, `agent_authored`, `verification_evidence`, `project_state`, `artifact`, `runtime_observation`, `other` — pre-existing, unchanged |
| `trust` | *How much uncorroborated agent judgement must I accept to believe this row?* | the five values in §2 — new, derived |

They are deliberately not merged. `trust_class` is a **stable domain label** already
consumed by `eg audit citations`, `eg query memory`, `eg query sessions`, and the
log lanes; `trust` is a **derived verdict** that changes as evidence and
contradiction edges change around the same record.

A runtime log signature makes the distinction concrete:

```json
{ "record_id": "log:v3:…", "trust_class": "runtime_observation", "trust": "source_derived" }
```

It is a program's own claim (not verification evidence, hence the `trust_class`),
but it was deterministically parsed from a captured artifact rather than asserted
by an agent (hence `source_derived`).

---

## 2 — The closed `trust` vocabulary

Exactly five values. There is deliberately **no** `other` / `unknown` escape hatch:
the derivation is an exhaustive match over `NodeKind` with no wildcard arm, so a new
node kind fails to compile until it is classified.

| Value | Meaning |
|-------|---------|
| `source_derived` | Deterministically derived from a source artifact rather than asserted by an agent. |
| `verification_evidence` | A recorded verification-domain execution (test run, command run, CI status, proof result, …). |
| `agent_verified` | An agent-authored claim carrying at least one live, direct, supporting link to a **passing** verification record at this snapshot. |
| `agent_unverified` | An agent-authored claim with no such supporting link. |
| `agent_contradicted` | An agent-authored claim that a live `CONTRADICTS` or `SUPERSEDES` relationship acts on at this snapshot. |

### What `source_derived` does NOT mean

`source_derived` states only that **no agent judgement was interposed** between a
source artifact and the row. It is **not** a claim that the content is true,
current, correct, complete, or verified. An imported GitHub issue title is
`source_derived` because the importer transcribed it deterministically — not
because the issue is accurate.

### What `agent_verified` does NOT mean

`agent_verified` records that a **live supporting link to a passing verification
record existed at this snapshot**. It is never proof that the claim is correct,
that the verification actually exercised the claim, or that the code still
behaves that way. Conversely `agent_unverified` is the **absence of recorded
corroboration**, never evidence that the claim is wrong.

---

## 3 — Derivation rules

`trust` is a pure function of `(record kind, the record's evidence links and
incident edges, the supersession/contradiction graph)` **at the queried snapshot** —
the same record slice the rest of the answer was computed from. It reads no wall
clock, no randomness, and no hash-iteration order.

Precedence is strictly top-down; the first matching row wins.

| # | Condition | Label |
|---|-----------|-------|
| 1 | Node kind is a verification kind (§4) | `verification_evidence` |
| 2 | Node kind is not an agent-claim kind (§4) | `source_derived` |
| 3 | Agent-claim kind **and** supersession status is `superseded`, `contradicted`, or `cycle` | `agent_contradicted` |
| 4 | Agent-claim kind **and** at least one qualifying supporting link (§5) | `agent_verified` |
| 5 | Agent-claim kind, otherwise | `agent_unverified` |

Three consequences are decisions, not accidents:

- **Rows 1–2 make the "never mislabeled" guarantee structural.** The agent branch is
  the only place an `agent_*` value is produced, and only an agent-claim kind
  reaches it. A code-graph, semantic, project, artifact, or log record therefore
  *cannot* be labeled with an agent trust class — it is unreachable, not merely
  untested.
- **Contradiction beats verification** (row 3 precedes row 4). A claim that is both
  contradicted and linked to a passing test run is `agent_contradicted`. A stale
  green run never rescues a superseded claim.
- **A supersession `cycle` is `agent_contradicted`** (fail-closed). The resolver
  could not establish clean currency, so the row is never presented as verified or
  as merely unverified.

### Sources of contradiction

All three representations the store supports are honoured, via the existing
`crate::temporal_status::TemporalResolver` (no duplicate logic):

1. the node's own `superseded_by` field,
2. an on-node `EvidenceLink` whose `relation` is `SUPERSEDES` or `CONTRADICTS`,
3. an `EdgeLabel::Supersedes` / `EdgeLabel::Contradicts` edge record.

Supersession is followed **transitively**, matching recall-time supersession
(`docs/cli/recall-supersession.md`).

---

## 4 — Kind classification

Classification is by `NodeKind`, consistent with the existing `trust_class_for`
and with `is_verified_claim` in the memory audit.

**Verification kinds** → `verification_evidence`:
`Verification`, `CommandEvidence`, `TestRun`, `CommandRun`, `CIStatus`,
`BenchmarkRun`, `CoverageReport`, `ProofResult`.

**Asserted-claim kinds** → the agent branch (rows 3–5):
`Observation`, `Decision`, `Failure`, `Agent`, `AgentSession`, `AgentRun`,
`AgentTurn`, `ToolCall`, plus the user-context policy kinds
(`PromoteCandidate`, `PromotionPrompt`, `PromotionDecision`, `Preference`,
`WorkflowRule`, `NamingDecision`, `Constraint`).

**Everything else** → `source_derived`: code-graph kinds, `SemanticDrift` and the
embedding kinds, project kinds, artifact kinds (`Artifact`, `PatchArtifact`,
`FileEdit`), log kinds, `Diagnostic`, `Retraction`, `CostUsage`, `ScanCoverage`.

> **Why the user-context kinds sit in the claim branch.** Issue #114 explicitly
> **defers** promotion / preference trust to the promotion flow, and
> `classify_node` returns `None` for every one of these kinds, so no context
> answer renders them today. The wildcard-free match still forces a choice, and
> the fail-closed one is the claim branch: a `Preference` or `Constraint` is an
> *assertion*, not something derived from a source artifact, so `source_derived`
> would over-claim. The honest caveat is that the emitted value would read
> `agent_unverified`, which understates authorship (these are operator-authored)
> while correctly stating that nothing corroborates them. Whoever first renders
> them must revisit this.

> **Honest limit — trajectory-imported records.** The trajectory importers
> (`src/traj.rs`, `src/codex.rs`, `src/claude_code.rs`, `src/antigravity.rs`)
> mint `Verification`, `CommandRun`, `ToolCall`, `FileEdit`, and `PatchArtifact`
> nodes under `agent_memory:v1:` record IDs, because the evidence was recovered
> from the agent's own transcript. Classification is by node kind, not by ID
> prefix, so a trajectory-imported `Verification` reads `verification_evidence`
> and can confer `agent_verified`, and a trajectory-imported `FileEdit` /
> `PatchArtifact` reads `source_derived`.
>
> Two things follow. First, that is a deterministic transcription of a recorded
> command execution — **not** independent verification. Second, the `FileEdit`
> case is weaker than `source_derived` normally implies: `src/traj.rs` sets
> `before_hash`/`after_hash` to *surrogate* hashes derived from the transcript
> (no file was read), `repo_relative_path` comes from a shell-string parse,
> and `edit_kind`/`hunk_count` are hardcoded. A store built only from agent
> trajectories should be read with both limits in mind.

---

## 5 — What counts as a supporting verification link

A link promotes an agent-authored claim to `agent_verified` **only if all five hold**:

1. **Direction is forward** — the link runs *from* the claim *to* the verification
   record. Backward traversal is refused for the reason documented at
   `is_forward_only_label` (`src/query/semantic.rs`): two claims validated by the
   same run would otherwise verify each other.
2. **Relation is a backing relation** — `VALIDATED_BY`, `HAS_EVIDENCE`, or
   `PRODUCED_EVIDENCE`, in either representation (an edge record or an on-node
   `EvidenceLink`). A generic `RELATES_TO`/`MENTIONS_SYMBOL` link that merely
   happens to point at a verification record confers nothing. This mirrors
   `is_verified_claim` in `src/query/memory_audit.rs`.
3. **The target is a verification kind** (§4). A link to another agent-authored
   record never qualifies — *an agent-authored claim is never counted as evidence
   for itself*.
4. **The target is live at this snapshot** — it is the **latest physical
   version** of its record ID, it is not tombstoned, and its own supersession
   status is `current`. The latest-version rule matters: `eg export`
   deliberately re-emits superseded versions, and `eg capture-tests` keys a
   `TestRun` on `(session, commit, suite)` with `status` **not** an identity
   input, so a suite re-run that went red yields two versions of one ID.
   Without it a stale green version would outvote the current red one, and
   `--graph` would disagree with `--data-dir`.
5. **The link itself is live** — the edge is the latest version of its edge ID
   and is not tombstoned. `eg forget` can retract an evidence-link edge; a
   retracted link must stop conferring trust, not merely stop being traversed.
6. **The target is passing** (§6).

**Direct links only — one hop.** There is no file-level widening and no transitive
closure. `eg query verification-coverage` deliberately widens to `link_level: file`;
this lane deliberately does not, because a passing test that touched a file must
never promote every observation about that file.

The check is an existential over a sorted iteration, so a claim carrying one passing
and one failing link is `agent_verified` regardless of link order — order-independent
by construction.

---

## 6 — When a verification record counts as passing

Verification `status` is a free string at schema v1 (`docs/schema/verification.md`),
and the trajectory importers do not set it at all — they record only `exit_code`.
The rule is therefore closed and fail-closed:

| Recorded state | Passing? |
|----------------|----------|
| `status` present, ASCII-lowercased/trimmed value is `pass` or `passed` | **yes** |
| `status` present, any other value (`fail`, `skip`, `error`, `timeout`, unrecognized) | no |
| `status` absent or empty, `exit_code == 0`, **and** the kind is `Verification` / `TestRun` / `CIStatus` / `ProofResult` / `BenchmarkRun` / `CoverageReport` | **yes** |
| `status` absent or empty, `exit_code == 0`, kind is `CommandRun` / `CommandEvidence` | **no** |
| `status` absent or empty, `exit_code != 0` | no |
| `status` absent or empty, no `exit_code` | no |

The exit-code fallback is **kind-restricted on purpose**. `src/traj.rs` mints a
status-less `Verification` only for a *test* command (it is gated on
`is_test_command`), so a zero exit there really is a verification verdict. But
`src/codex.rs` and `src/claude_code.rs` mint a `CommandRun` for **every** shell
invocation — and `src/claude_code.rs` also mints a
`ToolCall --PRODUCED_EVIDENCE--> CommandRun` edge, a write-path-valid backing
relation. Without the restriction, an agent claim linked to a successful `ls`
would read `agent_verified`. A `CommandRun` must therefore say `pass`
explicitly.

**Link confidence is not consulted.** An `EvidenceLink` carries a `confidence`
string, and a link with `confidence: "0.0"` still confers `agent_verified` when
every condition in §5 holds. This matches `is_verified_claim` in the memory
audit. Thresholding a free-string confidence would invent a policy this contract
does not own; the raw `evidence_links` ride on the row, so a consumer that cares
can apply its own.

`status` wins when present: a record carrying `status: "fail"` is not passing even
if `exit_code` is `0`. `pass` is the canonical token
(`docs/schema/verification.md`, `eg evidence` write-path validation, and
`is_pass_status` in `src/query/failure_history.rs`); `passed` is accepted as the
one alias that occurs in this repository's own data. `success`, `ok`, and `green`
are **not** accepted — they appear nowhere in the schema or in any writer, and
inventing synonyms would risk reading a non-pass as a pass.

**A link to a failing verification record yields `agent_unverified`** — explicitly
not `agent_verified`, and explicitly not `agent_contradicted`. A red run is not a
`CONTRADICTS` relationship: an observation of the form "this path is flaky" is
entirely consistent with a failing run. Unknown, absent, and unrecognized states
are treated the same way: **unknown is never a pass**.

---

## 7 — Where `trust` is emitted

Every record row of every cross-domain context answer:

| Surface | Sections carrying `trust` |
|---------|---------------------------|
| `eg query context` | `source_facts`, `observations`, `project_state`, `artifacts`, `verification_evidence`, `drift_history`, `excluded` |
| `eg query subsystem` | the same, plus `semantic_drift` and `log_signatures` |
| `eg query locate` | the bundle sections |
| `eg query semantic-context` | the per-match bundle sections |
| `eg query task` | `tasks`, `acceptance_criteria` (including a nested `verification_record`), `source_facts`, `observations`, `artifacts`, `verification_evidence`, `reviews`, `external_links` |
| `eg query error-context` | `source_facts`, `observations`, `project_state`, `artifacts`, `verification_evidence` (each row also keeps its own domain-keyed `trust_class`) |
| `eg query changes` | `observations`, `project_state`, `artifacts`, `verification_evidence` |
| daemon `observations_for_symbol` | the same sections as `eg query context` |
| daemon `criteria_for_task` | the same sections as `eg query task` |
| MCP `symbol_context`, `task_evidence` | the same sections as their CLI equivalents |

`eg query subsystem` has no `drift_history`; its drift rows are the
`semantic_drift` section, and it adds `log_signatures`. Both carry `trust`.

### Rows that are deliberately outside this contract

The label exists because a **trust-MIXED** section can hide the difference
between a fact and a guess. Rows that are single-tier by construction, or that
are not context rows at all, are left alone:

| Row | Why it carries no `trust` |
|-----|---------------------------|
| `eg query locate`'s `symbol` and `enclosing_chain` | These are the *location* answer, identical in shape to `eg query at` (a single-domain lane). The located record also appears in `source_facts`, where it does carry its label. |
| `eg query changes`' `changed_files`, `changed_symbols`, `commits`, `tombstones`, `drift_records`, `unexplained_changes` | Every row in each is a code-graph record, so the section is single-tier and its name already says what it holds. Only that lane's genuinely mixed sections (`observations`, `project_state`, `artifacts`, `verification_evidence`) are labeled. |
| `eg query evidence-path` hops | A witness path over evidence edges, not a trust-separated context bundle. Hop rows keep their existing `trust_class`. |
| Single-domain lanes (`deps`, `who-imports`, `unsafe-sites`, …) | Single-tier answers; see the name-collision note at the top of this document. |

All transports derive the label with the same function over the same record slice,
so a CLI answer and a daemon answer cannot disagree.

### The label is computed before the supersession filter

`--supersession exclude` is the default, and it moves superseded/contradicted
observations out of `observations` and into `excluded`. The trust label is a
property of the record and its edges, **not** of the rendering filter, so it is
derived at row-construction time and `excluded` rows carry it too.

Consequently `agent_contradicted` is observable in **both** modes: in `excluded`
under the default `exclude`, and in `observations` under `include-but-flag`. The
same record yields the same label in either mode.

### What is deliberately NOT labeled

| Section | Why |
|---------|-----|
| `topology_edges` | An edge is a *relation*, not a record. It has no domain of its own and no evidence links; any label would be invented semantics. |
| `unresolved` | These are evidence-link handles whose target is **absent** from the store slice. There is no record to classify, and stamping a trust label on a record that is not there would be fabrication. They already carry `verification_status: "unresolved"`. |

Both omissions are contracts, asserted by a guard test — not oversights.

---

## 8 — Determinism

Re-running the same answer over an unchanged store yields byte-identical `trust`
labels. Guaranteed by construction:

- inputs are only the record slice, its evidence links, its incident edges, and the
  supersession graph — all snapshot-scoped;
- every index owned by the derivation is a `BTreeMap`/`BTreeSet`;
- the supporting-link check is an order-independent existential;
- no wall clock, no randomness, no filesystem, no network.

One caveat, stated precisely because the easy version of this claim is false:
the derivation embeds a `TemporalResolver`, whose internal indexes **are**
`HashMap`/`HashSet`. Determinism there does not come from container choice. It
comes from the shape of the answer: `resolve_status` returns a scalar status
that is set-determined rather than iteration-ordered — its DFS keeps only the
current path in the visited set and pops on backtrack, so it is exhaustive over
simple paths and reports a cycle iff one is reachable, and its head set is the
order-invariant set of successor-less reachable nodes — and its returned
reference lists are sorted before use.

The label is also invariant to the physical order of records in the input JSONL.

### Scope caveats a consumer should know

- **The label is snapshot-relative, not absolute.** `eg query context` derives
  it over the *corpus-filtered* slice, so a record can read `agent_verified`
  under `--all-history` and `agent_unverified` under the default HEAD-anchored
  corpus if the supporting run is not current at HEAD. That is the intended
  reading of "at the queried snapshot", but a consumer caching labels across
  flag combinations will see them move.
- **The daemon's `as_of` narrowing drops tombstones**, so under
  `as_of.valid_time` the §5 liveness rules see a slice with no tombstones and a
  retracted supporting link can still confer `agent_verified`. Pre-existing
  filter behaviour, disclosed here because this field turns a silent omission
  into a positive verdict.
- **"The transports cannot disagree"** means they call the *same derivation over
  the slice each transport resolved*. Where the slices differ — the CLI applies
  corpus filtering that the daemon verb does not — the labels can differ,
  honestly reflecting the different corpora.

---

## 9 — See also

- `docs/cli/query.md` — the `trust` field on `eg query context` and friends.
- `docs/schema/daemon-query.md` — the `observations_for_symbol` response.
- `docs/cli/recall-supersession.md` — the `exclude` / `include-but-flag` modes.
- `docs/schema/verification.md` — verification node kinds and `status`.
- `docs/cli/verification-coverage.md` — the deliberately *wider*, file-level join.
