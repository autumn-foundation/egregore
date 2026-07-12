# `eg audit review-coverage`

A standing, citable **review-coverage gate** (issue #339). For every pull request
merged in a half-open valid-time window `[from, to)` — keyed on `merged_at` — this
lane classifies the PR into a CLOSED verdict-class set and gates on the
`covered / merged_prs` ratio.

```powershell
eg audit review-coverage --from 2026-03-01T00:00:00Z --to 2026-04-01T00:00:00Z --graph history.graph.jsonl
eg audit review-coverage --from <T0> --to <T1> --data-dir .egregore --min-coverage 0.8
eg audit review-coverage --from <T0> --to <T1> --graph g.jsonl --require-final-head false
```

## What it measures — and what it does not

This lane measures **recorded review execution captured in the graph** — nothing
more. It is:

- **NOT** GitHub branch-protection configuration. Branch protection *prevents* an
  unreviewed merge before it happens; this lane *measures* what the imported
  history actually recorded after the fact. Use branch protection for prevention;
  use this lane for recorded-execution measurement and audit evidence.
- **NOT** review quality. An approving review counts structurally; the lane never
  judges whether the review was thorough.
- **NOT** proof that no review happened elsewhere. Absence of a recorded approving
  review means *no imported evidence of one*, not that no review occurred.

The verbatim disclaimer appears in every report.

## Substrate

- **#333** promoted the PR fields consumed here first-class on the PR `Task`:
  `merged_at`, `head_sha`, `merge_commit_sha`, `system_native_id`.
- **#334** anchors each review to the commit it reviewed (`review_commit_sha`,
  `REVIEWS_COMMIT`), enabling the final-head check and the two #334-dependent
  evidence-pack gap classes.
- **#46** is the GitHub importer that captures the PR/review records.
- **#335** (identity nodes / `ExternalIdentity`) is **not yet merged**. The
  non-author check therefore compares the recorded author **logins** on the PR
  `Task` (`author`) and the `Review` (`author`). When a login is missing on
  either side the row degrades to the `identity_unavailable` sub-label and the
  lane NEVER fabricates a self-approval. No `ExternalIdentity` node kind is
  invented; the login is the available signal, with this documented limitation.

## Verdict classes (closed set)

| Verdict | Meaning |
| --- | --- |
| `covered` | An approving review from a non-author identity, anchored at-or-after the reviewed head (`review_commit_sha == head_sha` when `--require-final-head` is on), valid at-or-before `merged_at`. |
| `approval_stale_head` | Approved, but the approval's `review_commit_sha` differs from the PR's final `head_sha` (approved-then-force-pushed), or the approval is anchored but the PR carries no `head_sha` so the final head is unverifiable (`head_sha_unavailable` sub-label). Only classified as stale when `--require-final-head` is on (default). |
| `self_approved_only` | The only approving review(s) come from the PR author identity. Requires both logins present; if a login is missing the row degrades to `covered` + `identity_unavailable` rather than a fabricated self-approval. |
| `uncovered` | Merged with zero approving reviews. |

## Sub-labels

Sub-labels annotate a row without leaving the closed verdict set:

- `identity_unavailable` — the non-author check could not compare identities
  because a login was missing (#335 identity nodes unavailable). The row is not
  claimed to be a self-approval.
- `approval_unanchored` — the deciding approval carries no `review_commit_sha`
  (#334 anchor absent), so the final-head check could not run; the row is not
  guessed to be stale.
- `head_sha_unavailable` — the deciding approval IS anchored (has a
  `review_commit_sha`), but the PR `Task` carries no `head_sha` (e.g. a pre-#333
  or partial import), so the review cannot be confirmed as reviewing the final
  head. Under `--require-final-head` (default on) the row degrades to
  `approval_stale_head` with this sub-label rather than silently passing the
  final-head check and being counted `covered`. With `--require-final-head` off
  the head is not checked and the row is unaffected.

## Citation set per row

Every row cites the PR `Task` record ID, its `system_native_id`, and
`merge_commit_sha`. `covered` rows additionally cite the approving `Review` record
ID, its `review_commit_sha`, and the approver login (`approver_login`); the
`approver_identity_id` field is reserved and always `null` until #335 lands.

## Windowing

Reuses #338's half-open valid-time semantics on `merged_at` (`from <= t < to`).
A PR that looks merged (carries a `merge_commit_sha`) but has no
window-resolvable `merged_at` is excluded from the merged set under a counted
`excluded_unresolvable_merge_time` diagnostic — never windowed on the PR's
update time. An **empty window** (zero merged PRs) is an explicit VACUOUS PASS
(`empty_window` diagnostic, exit 0), never an error.

## Strictness knobs

| Flag | Default | Effect |
| --- | --- | --- |
| `--require-non-author` | `true` | Require an approving review from a non-author identity. Compares recorded logins; degrades to `identity_unavailable` without them. |
| `--require-final-head` | `true` | Require the approval anchored at the PR's final head. Classifies case (b) as `approval_stale_head` rather than `covered`. |

## Gate & exit codes

`--min-coverage <ratio>` (default `1.0`) gates on `covered / merged_prs`.

| Exit | Condition |
| --- | --- |
| 0 | Coverage met the threshold (`ok: true`). Empty windows are vacuous success. |
| 1 | Coverage below threshold (`ok: false`). The full report is still printed with a `below_review_coverage_threshold` diagnostic naming the ratio and the failing (non-covered) PR record IDs. |
| 2 | Usage/load error: reversed window (`reversed_window`), invalid timestamp (`invalid_timestamp`), both input flags (`conflicting_input_flags`), neither input flag (`missing_input_flag`), out-of-range `--min-coverage` (`invalid_min_coverage`), or an empty/unreadable store/graph. |

## JSON contract

One deterministic object:

```json
{
  "ok": false,
  "window": { "from": "…", "to": "…" },
  "options": { "require_non_author": true, "require_final_head": true },
  "min_coverage": 1.0,
  "merged_pr_count": 4,
  "covered_count": 1,
  "coverage": 0.25,
  "verdict_counts": {
    "approval_stale_head": 1,
    "covered": 1,
    "self_approved_only": 1,
    "uncovered": 1
  },
  "rows": [
    {
      "pr_task_id": "project:v1:pr01",
      "system_native_id": "1",
      "merge_commit_sha": "mc-pr01",
      "merged_at": "2026-03-05T12:00:00Z",
      "verdict": "covered",
      "approving_review_id": "project:v1:r01",
      "review_commit_sha": "head-pr01",
      "approver_login": "rev-1"
    }
  ],
  "diagnostics": [
    { "code": "below_review_coverage_threshold", "record_ids": ["…"], "detail": "…" }
  ],
  "disclaimer": "measures recorded review execution captured in the graph; …"
}
```

## Determinism & safety

The report is computed in a PURE core (`src/review_coverage.rs`) with no I/O and
no wall clock; the CLI is a thin wrapper. Output is byte-identical across runs on
an unchanged store (verified across 5 runs) and identical across `--graph` vs
`--data-dir`. The output is allow-list only — record IDs, SHAs, logins, ratios,
bounded labels, and counts; NO bodies, hunks, or payloads.

## Shared implementation with the evidence pack

The per-PR classification is a single shared implementation
(`evidence_pack::derive_review_coverage`) that BOTH this lane and #338's
evidence-pack `review_coverage` section / `merged_pr_without_approving_review` gap
call — the pack with lenient options, this lane with its strict defaults — so the
two surfaces can never diverge. See `docs/cli/evidence-pack.md`.
