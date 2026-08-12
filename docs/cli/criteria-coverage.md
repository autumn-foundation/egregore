# `eg audit criteria-coverage`

The store-wide **acceptance-criterion verification-coverage census and proof-gap
gate** (issue #115). One local, read-only report answering: *across all imported
work, which acceptance criteria are actually proven by passing evidence, and
which tasks are marked done while owning unproven criteria?*

```powershell
# The shortest workflow — over a JSONL graph or an embedded store
eg audit criteria-coverage --graph graph.jsonl
eg audit criteria-coverage --data-dir .egregore

# Loosen the gate for a store that is not yet fully proven
eg audit criteria-coverage --data-dir .egregore --min-proven-ratio 0.8 --max-claimed-done-unproven 5

# Human-readable mirror of the JSON contract
eg audit criteria-coverage --graph graph.jsonl --format text
```

No network, no hosted indexing, no remote embeddings, no daemon.

## How this differs from its neighbours

| Lane | Question it answers |
| --- | --- |
| **`eg audit criteria-coverage`** (#115, this lane) | Store-wide: what fraction of **acceptance criteria** are backed by passing verification evidence, and which closed tasks hide unproven ones? |
| `eg query task <handle>` (#48) | **One task you already named**: its criteria, split verified vs unverified. It cannot tell you how much of the whole project is unproven. |
| `eg query verification-coverage` (#109) | Which **code symbols** carry recorded verification evidence. High code coverage with unproven acceptance criteria is still an unproven feature. |
| `eg audit memory-health` (#94) | The health of **agent-memory** claims — a different domain and trust class entirely. |

This lane is the census that tells you *whether* to go run those.

## What a coverage number is not

A poor coverage number is a reason to **review/verify** — never proof that any
one task is wrong. Specifically:

- Absence of closing evidence means *no imported proof*, **not** that the work is
  broken, untested, or unsafe.
- Presence of a passing verification record is a **recorded execution**, never
  proof of correctness.
- This lane only *measures* the proof gap. It never auto-closes, auto-verifies,
  re-runs tests, or edits any `Task` or criterion status.

The verbatim disclaimer appears in every report.

## Trust separation (the load-bearing rule)

`proven` means exactly one thing: **the criterion is closed by a live
`CLOSES_ACCEPTANCE_CRITERION` link to a verification record whose recorded
outcome is passing.**

Being a verification record takes **four checks, not one**:

1. the record ID is verification-domain (`verification:v<N>:`);
2. the `NodeKind` is one a `CLOSES_ACCEPTANCE_CRITERION` edge may target —
   `Verification`, `CommandRun`, `TestRun`. This is deliberately **narrower**
   than the verification domain itself: a passing `CoverageReport` or
   `BenchmarkRun` closing a criterion is a relationship the write API rejects,
   so the reader must not accept it either;
3. the record carries the **evidence handle** the schema requires
   (`source_artifact_hash`, `source_artifact_path`, or an output-handle hash).
   A bare `TestRun { status: "pass" }` claims an outcome while citing nothing,
   and the daemon refuses to persist one (`MissingEvidenceHandle`);
4. the handle is not the criterion's own record ID.

Checks 2 and 3 exist because `--graph` reads and embedded ingest both **bypass**
the daemon's `validate_verification_domain_records` and `validate_project_edge`.
A store really can hold a node with a `verification:v1:` ID and an `Observation`
kind, or a passing `TestRun` citing no artifact at all — each of which a
prefix-only gate would certify as proof. Every rule here is the *same constant*
the write path enforces (`CLOSURE_TARGET_KINDS`, `has_evidence_handle`), shared
so a reader can never accept what a writer would refuse. Failures land in
`non_verification_evidence` with a precise resolution
(`not_verification_record` / `missing_evidence_handle`).

A criterion is **never** counted proven from:

- `Task.status` — a human toggle in an issue tracker;
- its own `AcceptanceCriterion.status: verified` — a claim. It is echoed on the
  row as `criterion_status` for citation, but never read for bucketing;
- a commit message, prose, or semantic similarity;
- an `agent_memory:v<N>:` record whatever its kind — notably the
  `NodeKind::CommandEvidence` that `eg command-evidence` mints from an
  agent-supplied `--exit-code`.

## What the gate does NOT establish

**`proven` does not mean "someone independently confirmed this works."** The gate
proves the closing record was *minted as verification evidence*; it does not
prove the outcome was independently observed. Concretely, on trunk today:

| Writer | Mints | Outcome comes from |
| --- | --- | --- |
| `eg capture-tests` | `verification:v1:` `TestRun` | a parsed libtest artifact — an independent observation |
| `eg write verification --status pass` | `verification:v1:` `Verification` | **its caller**, verbatim |
| Claude Code / Antigravity transcript importers | `verification:v1:` `Verification` / `CommandRun` | what an agent transcript *said* a command did |
| `eg command-evidence` | `agent_memory:v1:` `CommandEvidence` | an agent-supplied `--exit-code` — correctly **excluded** |

Rather than silently rank writers, every closing link discloses the
`producer_kind` that wrote it, so a reader can see whether a `proven` rests on a
captured test run or on an agent's own account of one. **Honest limit:**
`producer_kind` does not fully discriminate — `eg capture-tests` and
`eg write verification` both record `observation_writer`. Treat a `proven` row as
*"there is a passing verification record here"* and read its `producer_kind`
before treating it as more.

## Buckets (closed set, mutually exclusive)

Every live acceptance criterion lands in exactly one bucket, and the buckets sum
to `total_criteria`.

| Bucket | Meaning |
| --- | --- |
| `proven` | Closed by a resolvable verification-domain record with a **passing** outcome. |
| `failed_evidence` | A closing link resolves to a verification-domain record whose outcome is **failing**. |
| `dangling_evidence` | A closing handle resolves to **no live record** (absent, or tombstoned). The original handle is preserved. |
| `non_verification_evidence` | A closing link resolves to a live record that is **not a verification record** (wrong domain, wrong `NodeKind`, or the criterion itself) — reported, never counted proven, never merged into `unverified`. |
| `inconclusive_evidence` | A closing link resolves to a verification record whose outcome is **neither** passing nor failing (e.g. `skip`, or no status and no exit code), **or** whose live physical versions disagree about the outcome (`ambiguous_versions`). |
| `unverified` | **No** closing verification evidence at all. |

The issue's three named ratios are `proven`, `unverified`, and `failed_evidence`;
`dangling_evidence` is its own required bucket. `non_verification_evidence` and
`inconclusive_evidence` exist so that "never counted proven, and never silently
merged into unverified" holds for the two remaining ways a closing link can fail
to prove anything.

### Bucket precedence (fail-closed)

The schema makes `CLOSES_ACCEPTANCE_CRITERION` many:1, so a criterion normally
carries exactly one closing handle and precedence never fires. When several are
present the **most alarming wins**, so a passing link can never mask a failing or
dangling one:

```
failed_evidence > dangling_evidence > ambiguous_versions
                > non_verification_evidence > inconclusive_evidence
                > proven > unverified
```

Every handle is listed on the row in `closing_links` with its own `resolution`
(`passing` / `failing` / `inconclusive` / `not_verification_record` / `ambiguous_versions` / `unresolved`), so the derivation is auditable rather than asserted.

## Outcome vocabulary

Pass/fail is the **single shared rule** in `src/query/trust.rs`
(`verification_outcome`), reused verbatim so this census and the derived
`agent_verified` trust class cannot drift into two notions of "passing".

- **Passing**: `status` ∈ {`pass`, `passed`, `success`} (trimmed,
  case-insensitive); or, when `status` is absent, `exit_code == 0`.
- **Failing**: `status` ∈ {`fail`, `failed`, `failure`, `error`, `errored`,
  `timeout`, `timed_out`}; or, when `status` is absent, a non-zero `exit_code`.
  `error`/`timeout` are failing because the check *ran and did not succeed*.
- **Inconclusive**: everything else — including `skip` (a skipped check proves
  nothing either way) and a record with neither field.

An **unrecognized** status paired with a **non-zero** `exit_code` still reports
failing: a `status: "cancelled", exit_code: 137` or a panicking `exit_code: 101`
is hard evidence the check did not succeed, and filing it as merely inconclusive
would conceal the severity of a store full of crashed runs. A *zero* exit code
under an unrecognized status stays inconclusive — overriding the record's own
summary would manufacture a pass it never claimed.

A disjointness test pins the two vocabularies apart.

## Claimed-done-but-unproven

The named gap set: criteria owned by a `Task` in a closed/**done** state that are
**not** `proven`. Each row carries its criterion record ID, its parent `Task`
record ID, and the closing verification record ID(s) where any exist.

`done_task_statuses` is echoed into every report so the definition is visible in
the output. It is `["closed_completed"]` — deliberately **not** `closed_dropped`.
A dropped task is *closed* but claims no completion: the work was abandoned, not
asserted done, so counting it would manufacture a proof gap for deliberately
dropped work.

Ownership resolves from **both** representations the schema keeps in sync: the
`OWNED_BY_TASK` edge and the denormalized `parent_task_id` field. Either alone is
sufficient.

The gate reads **every** candidate parent — the field and every edge target,
across every live version of each — and counts the criterion when *any* of them
is done. Narrowing that to the displayed parent would be fail-open in the one
metric this command enforces. Because ambiguity widens the alarm, every input
that widened it is disclosed: `parent_task_status_ambiguous` when any candidate
has live versions recording different statuses, and
`criterion_parent_task_conflict` when the field disagrees with an edge **or**
several edges name different tasks with no field to arbitrate.

The row then cites the **deciding** parent — the one whose done status fired the
gate — so a `claimed_done_unproven` row can never contradict itself by
displaying an `open` task. With no done candidate it cites the record's own
field, else the lexicographically smallest edge target.

## Gate & exit codes

| Flag | Default | Gate |
| --- | --- | --- |
| `--min-proven-ratio <ratio>` | `1.0` | Fails when `proven / total_criteria` falls **below** the bound. |
| `--max-claimed-done-unproven <n>` | `0` | Fails when the claimed-done-but-unproven **count rises above** the bound. |
| `--limit <n>` | `500` (max `1000`) | Caps each row list independently. |

| Exit | Condition |
| --- | --- |
| 0 | Every threshold met (`ok: true`). A store with **zero** acceptance criteria is a vacuous pass carrying `no_acceptance_criteria`. |
| 1 | A threshold was breached (`ok: false`). The full report is still printed, with a `breaches` array naming each metric, its observed value, and the bound, plus the stable diagnostics `below_proven_ratio_threshold` / `above_claimed_done_unproven_threshold`. A store over the proof-gap line is **never** reported as ready. |
| 2 | Usage/load error: both input flags (`conflicting_input_flags`), neither (`missing_input_flag`), out-of-range `--min-proven-ratio` (`invalid_min_proven_ratio`), out-of-range `--limit` (`invalid_limit`), an input holding zero records (`empty_evidence_input`), an unreadable/unparseable `--graph` (`graph_read_error` / `graph_parse_error`), or a missing/invalid `--data-dir` (a plain-text diagnostic naming the path, not a JSON envelope). |

An out-of-range or non-finite threshold is an error rather than a clamp, so the
gate can never be silently disabled or inverted.

## Ratios

Every ratio reports its **numerator and denominator**, never a bare percentage:

```json
{ "numerator": 2, "denominator": 10, "ratio": 0.2 }
```

A zero denominator reports `"ratio": null` — never a divide-by-zero and never a
fabricated `0.0` — alongside the stable `no_acceptance_criteria` diagnostic.
`--format text` renders the same case as `n/a`.

## Diagnostics (stable codes)

| Code | Meaning |
| --- | --- |
| `no_acceptance_criteria` | Zero live criteria; ratios have a zero denominator. Not a failure. |
| `dangling_closing_evidence` | Names every criterion carrying a closing handle that resolves to no live record — including one masked by a higher-precedence bucket. |
| `criterion_parent_task_conflict` | Contested ownership: `OWNED_BY_TASK` target ≠ `parent_task_id`, or several edges name different tasks with no field to arbitrate. |
| `parent_task_status_ambiguous` | Some candidate parent `Task` has several live versions recording DIFFERENT statuses; the claimed-done test counts the criterion when ANY version is done. |
| `criterion_parent_task_unresolved` | The owning `Task` does not resolve to a live `Task`. Still counted in the census, but can never enter the claimed-done set. |
| `superseded_criteria_counted` | Names criteria recorded `status: superseded`, which ARE counted in the proof gap — disclosed because the symmetric argument excludes `closed_dropped` tasks. |
| `results_truncated` | `--limit` truncated a row list; counts stay pre-truncation. |
| `below_proven_ratio_threshold` | Gate breach, naming the ratio and the bound. |
| `above_claimed_done_unproven_threshold` | Gate breach, naming the count, the bound, and every failing criterion ID. |

## JSON contract

`--format json` (the default) is the agent contract. `--format text` is a
deterministic human-readable rendering of the same report carrying the same
citable evidence per row (record ID, parent task, bucket, every closing link with
its resolution and producer, the proving record, path/span, and source handles);
the JSON remains the complete contract. Abridged:

```json
{
  "ok": false,
  "total_criteria": 10,
  "proven": { "numerator": 2, "denominator": 10, "ratio": 0.2 },
  "unverified": { "numerator": 3, "denominator": 10, "ratio": 0.3 },
  "failed_evidence": { "numerator": 1, "denominator": 10, "ratio": 0.1 },
  "dangling_evidence": { "numerator": 2, "denominator": 10, "ratio": 0.2 },
  "non_verification_evidence": { "numerator": 1, "denominator": 10, "ratio": 0.1 },
  "inconclusive_evidence": { "numerator": 1, "denominator": 10, "ratio": 0.1 },
  "proof_gap": { "numerator": 8, "denominator": 10, "ratio": 0.8 },
  "bucket_counts": { "proven": 2, "unverified": 3, "…": 0 },
  "done_task_statuses": ["closed_completed"],
  "claimed_done_unproven_count": 3,
  "claimed_done_unproven": [
    {
      "record_id": "project:v1:ac-f1",
      "parent_task_id": "project:v1:task-done",
      "parent_task_status": "closed_completed",
      "criterion_status": "failed",
      "ordinal": 2,
      "bucket": "failed_evidence",
      "closing_links": [
        {
          "handle": "verification:v1:fail1",
          "origins": ["closes_edge"],
          "resolution": "failing",
          "node_kind": "TestRun",
          "verification_kind": "TestRun",
          "status": "fail",
          "producer_kind": "observation_writer"
        }
      ],
      "proving_verification_id": null,
      "source_handle": "tasks.jsonl:ac-f1:abc123",
      "claimed_done_unproven": true
    }
  ],
  "criteria": [ "… one row per criterion, same shape …" ],
  "thresholds": { "min_proven_ratio": 1.0, "max_claimed_done_unproven": 0 },
  "breaches": [
    { "metric": "proven_ratio", "observed": "0.2000", "bound": "1.0000", "message": "…" }
  ],
  "diagnostics": [ { "code": "dangling_closing_evidence", "record_ids": ["…"], "detail": "…" } ],
  "disclaimer": "measures whether each recorded acceptance criterion resolves to a passing verification record; …"
}
```

Every row carries a citable `record_id`, its `parent_task_id`, and — where
present — `repo_relative_path`/`span` and the source-system `source_handle` /
`external_link_id`. **No row exists only as prose.**

`proving_verification_id` names the deciding passing record, and is present ONLY
on a `proven` row: naming one on any other bucket would call evidence proof.
`parent_task_id` serializes as an explicit `null` when the owning `Task` does not
resolve to a live `Task` record — the case `criterion_parent_task_unresolved`
reports. `node_kind` is the trusted closed vocabulary; `verification_kind` and
`status` are free text read back from the store and are sanitized and
length-capped (see below).

## Redaction & safety

Output is **allow-list only** by field name: record IDs, handles, statuses,
kinds, spans, paths, counts, ratios, bounded messages, and stable diagnostic
codes. It never includes criterion `text`, task titles or bodies, transcript
text, command/test output, patch hunks, issue/PR bodies, env values, or tokens —
pinned by a test that plants sentinel strings in every prose field of the fixture
and asserts they never appear in either output format.

Some emitted VALUES are nonetheless operator- or attacker-controlled, because
they are read back from a store no writer fully validates. `status` in particular
is documented as a **free string with no enum enforcement at v1**, and a dangling
closing `handle` is reported with its original text precisely because nothing
validated it. Two rules apply, in the pure core so both transports inherit them:

- **Free text** (`status`, `verification_kind`, `criterion_status`,
  `parent_task_status`) is control-character-sanitized and length-capped at
  `CRITERIA_FIELD_MAX_CHARS` with a visible `…` marker, delegating to the same
  hardened helper the #104 semantic-index refusal path uses.
- **Handles** (record IDs, `handle`, `repo_relative_path`, `source_handle`,
  `external_link_id`, diagnostic `record_ids`) are control-character-sanitized
  but deliberately **not truncated**: a truncated handle is no longer a citation,
  and a prefix of a record ID silently reads like a valid one.

Without this, a crafted record could forge an entire extra `bucket=proven` row in
`--format text` output and emit an ANSI escape that clears the reader's terminal.
A test plants exactly that payload and asserts neither happens.

## Determinism & read-only

The report is computed in a **pure core** (`src/criteria_coverage.rs`) with no
I/O, no network, and no wall clock; the CLI is a thin wrapper. `--data-dir` is
read through the throwaway store copy `readonly_audit_store` makes, so the
original store is left byte-for-byte untouched — a fixture fingerprints every
file in the store before and after and asserts equality. Output is byte-identical
across 5 consecutive runs on an unchanged store, and independent of input record
order.

Liveness is the shared latest-write-wins `Liveness` gate, so `--graph` and
`--data-dir` agree: a tombstoned criterion leaves the census, a criterion
re-ingested after its own tombstone is live again, and a retracted closing edge
stops proving.

**Transport note.** The ingest write path enforces referential integrity for
*edges*, so a `CLOSES_ACCEPTANCE_CRITERION` **edge** whose target node was never
written cannot exist in an embedded store — that flavour of `dangling_evidence`
is reachable only over `--graph`. The same never-written handle carried in the
denormalized `verification_link_id` **field** ingests cleanly and *is* reachable
over `--data-dir`, as is the tombstoned-target flavour (a record written, then
retracted); the latter is what the `--data-dir` parity fixture exercises.

**Ordering limit.** Egregore writes graph JSONL with lexicographically **sorted**
lines (`Graph::to_jsonl`, `eg export`), so over `--graph` the relative order of
two physical writes of one record ID carries no information about which is
current. This lane therefore never decides an outcome by position: when live
versions of a closing record disagree, the link resolves `ambiguous_versions` and
the criterion is never reported `proven`. The same rule covers the owning `Task`, which is
*mutable* (`open` → `closed_completed`): the claimed-done test reads **every**
live version and counts the criterion when any of them is done, so a status
mutation can never drop unproven criteria out of the gap set by line order.
Disagreeing versions are disclosed as `parent_task_status_ambiguous`.

The residual limit is liveness — a record tombstoned and later re-added cannot be
distinguished from one merely tombstoned in a sorted graph, so it is reported
deleted. **`--data-dir` is the authoritative current-state read.**

## Scope

This slice consumes existing project-graph, verification, evidence-link,
edge-label, and redaction contracts. It introduces **no** new graph domain, node
kind, edge vocabulary, importer, trust model, task editor, schema version, or
language coverage — it is a read-only aggregation pass over records that already
exist.

Out of scope: auto-closing or auto-verifying anything, per-task drill-down (#48),
code-symbol verification gaps (#109), agent-memory health (#94), commit-range
change context (#62), new test-result semantics, and code line/branch coverage
(`CoverageReport`) — which measures what fraction of *code lines* ran, a
different question from what fraction of *acceptance criteria* are proven.

## See also

- [`docs/cli/task-queries.md`](task-queries.md) — `eg query task`, the per-task
  drill-down this census tells you *when* to run.
- [`docs/cli/verification-coverage.md`](verification-coverage.md) — the
  code-**symbol** verification-coverage lane (#109).
- [`docs/cli/verification-freshness.md`](verification-freshness.md) — ages the
  verification evidence this lane counts.
- [`docs/cli/memory-health.md`](memory-health.md) — agent-memory health (#94).
- [`docs/schema/project-graph.md`](../schema/project-graph.md) — the
  `AcceptanceCriterion` / `Task` shapes and the edge registry this lane reads.
