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

`proven` derives **only** from a live `CLOSES_ACCEPTANCE_CRITERION` link to a
record that provably lives in the **verification domain** — proven by its
`verification:v<N>:` record-ID prefix, the same gate `src/query/trust.rs` applies —
whose recorded outcome is **passing**.

A criterion is **never** counted proven from:

- `Task.status` — a human toggle in an issue tracker;
- its own `AcceptanceCriterion.status: verified` — a claim. It is echoed on the
  row as `criterion_status` for citation, but never read for bucketing;
- an agent observation, a commit message, prose, or semantic similarity.

The domain gate closes a **self-certification** hole. `eg command-evidence` mints
`NodeKind::CommandEvidence` under an `agent_memory:v1:` id, with a status derived
purely from an agent-supplied `--exit-code`. Without the gate an agent could write
its own proof and have Egregore certify a criterion from it. Such a link lands in
`non_verification_evidence` — fully reported, with the claimed status visible,
but never counted. An agent-authored claim is never evidence for itself.

## Buckets (closed set, mutually exclusive)

Every live acceptance criterion lands in exactly one bucket, and the buckets sum
to `total_criteria`.

| Bucket | Meaning |
| --- | --- |
| `proven` | Closed by a resolvable verification-domain record with a **passing** outcome. |
| `failed_evidence` | A closing link resolves to a verification-domain record whose outcome is **failing**. |
| `dangling_evidence` | A closing handle resolves to **no live record** (absent, or tombstoned). The original handle is preserved. |
| `non_verification_evidence` | A closing link resolves to a live record **outside** the verification domain — reported, never counted proven, never merged into `unverified`. |
| `inconclusive_evidence` | A closing link resolves to a verification-domain record whose outcome is **neither** passing nor failing (e.g. `skip`, an unrecognized status, or no status and no exit code). |
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
failed_evidence > dangling_evidence > non_verification_evidence
                > inconclusive_evidence > proven > unverified
```

Every handle is listed on the row in `closing_links` with its own `resolution`
(`passing` / `failing` / `inconclusive` / `non_verification_domain` /
`unresolved`), so the derivation is auditable rather than asserted.

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
sufficient. When they disagree the recorded field wins and the conflict is
reported as `criterion_parent_task_conflict` rather than silently resolved.

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
| 2 | Usage/load error: both input flags (`conflicting_input_flags`), neither (`missing_input_flag`), out-of-range `--min-proven-ratio` (`invalid_min_proven_ratio`), out-of-range `--limit` (`invalid_limit`), or an empty/unreadable graph or store (`empty_evidence_input`). |

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
| `dangling_closing_evidence` | Names every criterion whose closing handle resolves to no live record. |
| `criterion_parent_task_conflict` | `OWNED_BY_TASK` target ≠ `parent_task_id`; the field wins. |
| `criterion_parent_task_unresolved` | The owning `Task` does not resolve to a live `Task`. Still counted in the census, but can never enter the claimed-done set. |
| `results_truncated` | `--limit` truncated a row list; counts stay pre-truncation. |
| `below_proven_ratio_threshold` | Gate breach, naming the ratio and the bound. |
| `above_claimed_done_unproven_threshold` | Gate breach, naming the count, the bound, and every failing criterion ID. |

## JSON contract

`--format json` (the default) is the agent contract; `--format text` mirrors it
field for field. Abridged:

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
          "verification_kind": "TestRun",
          "status": "fail"
        }
      ],
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

## Redaction & safety

Output is **allow-list only**: record IDs, handles, status/outcome enums, kinds,
spans, paths, counts, ratios, bounded messages, and stable diagnostic codes. It
never includes criterion `text`, task titles or bodies, transcript text,
command/test output, patch hunks, issue/PR bodies, env values, or tokens — pinned
by a test that plants sentinel strings in every prose field of the fixture and
asserts they never appear in either output format.

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

**Transport note.** The ingest write path enforces referential integrity, so an
edge whose target node was *never written* cannot exist in an embedded store —
the absent-target flavour of `dangling_evidence` is reachable only over
`--graph`. The tombstoned-target flavour (a record written, then retracted) is
reachable on both, and is what the `--data-dir` parity fixture exercises.

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
