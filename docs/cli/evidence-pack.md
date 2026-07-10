# `eg audit evidence-pack` (issue #338)

Assemble a control-scoped, time-windowed **evidence pack** and re-verify it
offline. The pack composes existing contracts into one deterministic,
redaction-safe artifact tied to a single SOC2 control and a single half-open
valid-time window.

```powershell
# Assemble a pack for control CC8.1 over a valid-time window
eg audit evidence-pack assemble --control CC8.1 \
  --from 2026-03-01T00:00:00Z --to 2026-04-01T00:00:00Z \
  --graph history.graph.jsonl > pack.json          # exit 1 (review coverage < 1.0)

# Same, over an embedded store (read through a throwaway read-only copy)
eg audit evidence-pack assemble --control CC8.1 \
  --from 2026-03-01T00:00:00Z --to 2026-04-01T00:00:00Z \
  --data-dir .egregore --min-review-coverage 0.8

# Re-verify an assembled pack offline (integrity, coverage, safety, window)
eg audit evidence-pack verify pack.json            # exit 0 clean, 1 defect, 2 load error
```

## Shortest end-to-end workflow

```powershell
eg scan . --out graph.jsonl                 # current structure (public API, docs, deltas source)
eg scan-history . --out history.graph.jsonl # commit history: commits, valid times, authorship
eg import github <owner/repo> --out gh.jsonl # PR Tasks (#333), Reviews, external links
cat graph.jsonl history.graph.jsonl gh.jsonl > all.graph.jsonl
eg ingest all.graph.jsonl --adapter embedded --data-dir .egregore
eg audit evidence-pack assemble --control CC8.1 \
  --from <T0> --to <T1> --data-dir .egregore > pack.json
```

## What it composes

| Contract | Role in the pack |
|----------|------------------|
| #337 control catalog (`eg audit control-catalog`) | The control → evidence-class map. The manifest echoes the catalog id/version and its `control_catalog:v1:<hash>` pin. |
| #68 evidence bundle (`eg bundle export/verify`) | The scrub-to-hash primitive (`scrub_record` + BLAKE3), canonical ordering, and the integrity/coverage/safety verify verdicts. |
| #65 citation audit (`eg audit citations`) | The per-trust-class citation classification the pack's citation gate reuses byte-for-byte. |
| #118 deltas / #157 public-API deltas | Section-level disclaimers propagated verbatim into the `structural_deltas` / `public_api_deltas` sections. |
| #103 validate | Referential-integrity gating is a *separate* pre-ingest step; the pack assumes a validated graph. |
| #60 protected artifacts | `protected:v1:` handles survive as citations; raw bytes never enter the pack. |
| #333 PR head/base/merge SHAs | First-class PR Task fields drive merge-target and review joins. |
| #334 reviewed-commit facts | *Not yet merged.* Two gap classes degrade gracefully (see below). |

## When to use `eg bundle export` instead

`eg bundle export` is **record-closure-scoped**: pick a root selector, walk its
reference closure, and emit exactly the reachable records. `eg audit
evidence-pack` is **control- and window-scoped**: enumerate every class a
control requires over a valid-time window, always emitting one section per
class (empty when there is nothing in the window) plus a control gate. Reach for
the bundle when you want "everything connected to X"; reach for the evidence
pack when you want "everything control C requires between T0 and T1, with a
pass/fail gate".

## Window semantics

- Half-open: `from <= t < to` on **valid time**.
- Per-record valid time resolves in the fixed order
  `temporal.valid_time -> node valid_time -> executed_at`.
- A class-relevant record with no resolvable valid time is **excluded** and
  counted under a `missing_valid_time` diagnostic (and a `missing_valid_time`
  gap).
- `--from >= --to` exits 2 (`reversed_window`); a non-RFC-3339 bound exits 2
  (`invalid_timestamp`).

## Catalog integration and the three-way class outcome

Every class the control maps becomes a section. A class is **available** when
the full record set holds at least one record of that class (capability),
independent of the window; `review_coverage` is available whenever any
pull-request record exists. `evaluate_requirement` (#337) yields:

| requirement | availability | outcome | effect |
|-------------|--------------|---------|--------|
| required | available | `pass` | section populated, or **explicitly empty** |
| required | unavailable | `gate_fail` | verdict fails, `required_class_unavailable` diagnostic |
| optional | available | `pass` | section populated |
| optional | unavailable | `reported_optional_unavailable` | `{"status":"unavailable","unavailable_reason":...}` + `evidence_class_unavailable` diagnostic |

### What "available" means per class

A class is available when the input holds at least one record of that class's
**stored backing node kind**. When unavailable, the `unavailable_reason` names
one of three honest families:

| classes | backing | `unavailable_reason` when absent |
|---------|---------|----------------------------------|
| `commits` (`Commit`), `pull_requests` (`Task`/`github_pr`), `reviews` (`Review`), `structural_deltas` (`Change`), `verification_evidence` (`Verification`/`CommandRun`/`TestRun`/`CIStatus`/`CommandEvidence`/`BenchmarkRun`/`CoverageReport`/`ProofResult`) | real stored node kind | `<class>_domain_absent` (e.g. `delta_domain_absent`) |
| `public_api_deltas` (#157), `validation_runs` (#103) | computed/derived surface, **no stored node kind** | `derived_class_not_materialized` |
| `error_signatures` (`ErrorSignature`), `occurrence_buckets` (`LogOccurrenceBucket`), `remediation_links` | log-signature domain (issues #319/#340), not yet emitted | `log_domain_absent` |
| `review_coverage` | computed over merged PRs (available whenever any PR exists) | `no_pull_requests_to_measure` |

`structural_deltas` is a genuine stored class: `scan-history` emits one
`NodeKind::Change` record per file touched in a commit (`src/history.rs`), each
carrying the commit's valid time. In-window `Change` records populate the
`structural_deltas` section; when Change records exist in the store but none fall
in the window the section is **present but empty** (an optional present-empty
class passes), never `unavailable`. `public_api_deltas` and `validation_runs`
are derived query surfaces with no stored record kind, so they can never be
materialized as pack evidence rows — they degrade with the honest
`derived_class_not_materialized` reason rather than being mislabeled as a missing
domain. Only the log-signature classes degrade with `log_domain_absent`.

## Verdicts and exit codes

`assemble` prints the full pack to stdout and exits:

- **0** — every verdict passed (`verdicts.ok: true`). An empty window is a
  *vacuous success* (every section explicitly empty, all required classes still
  resolve as available).
- **1** — a verdict failed: `required_class_unavailable`, citation shortfall,
  review coverage below `--min-review-coverage` (default `1.0`), or an
  integrity/safety failure. The full report is still printed.
- **2** — usage/load error: `unknown_control` (naming the catalog's known IDs),
  `reversed_window`, `invalid_timestamp`, `conflicting_input_flags`,
  `missing_input_flag`, `invalid_min_review_coverage`, `catalog_read_error`,
  `catalog`-parse errors, an unreadable/missing store or graph, or
  `empty_evidence_input` (naming the source path) when the input loads **zero
  records** — a genuinely empty or whitespace-only graph, or an initialized
  store holding zero records. This is distinct from the exit-0 vacuous
  `empty_window` success, which is a *non-empty* input whose records merely fall
  outside the window.

The per-verdict block is: `required_classes`, `citation` (with per-trust-class
tallies), `review_coverage`, `integrity`, `safety`.

## Gaps — the closed class set

`gaps` always report, regardless of verdicts. Each gap row cites the record IDs
it derives from.

| Gap class | Meaning | Status |
|-----------|---------|--------|
| `merged_pr_without_approving_review` | A merged PR Task with no linked **in-window** approving `Review` (via `REFERENCES_TASK`). An approving review that resolves outside the pack window — or has no resolvable valid time — is omitted from the `reviews` section and does **not** suppress this gap. | Fully implemented. |
| `commit_outside_any_pr` | An in-window `Commit` not claimed by any PR via `MERGED_AS`. | Fully implemented. |
| `missing_valid_time` | A class-relevant record with no resolvable valid time. | Fully implemented. |
| `review_unanchored_no_commit_sha` | A review with no anchoring reviewed-commit SHA. | **Needs #334.** Degrades. |
| `approval_precedes_final_head` | A recorded approval whose commit predates the PR's final head. | **Needs #334.** Degrades. |

Because issue #334 (the `review_commit_sha` field / `REVIEWS_COMMIT` edge) is
not merged, the last two classes have no backing facts. The pack **detects the
field/edge at runtime**; when absent it emits a single `capability_unavailable`
diagnostic naming issue #334 and produces zero rows for those two classes — it
never fabricates. The enum stays closed; the two classes populate automatically
once #334's facts appear.

## `verify <path>`

Re-verifies an assembled pack offline and read-only:

- **Integrity** — recompute the BLAKE3 hash of each scrubbed record and check
  the per-section `(valid_time, record_id)` canonical order.
- **Coverage** — recompute the #65 citation thresholds (>=95% code rows cited;
  100% non-code rows cited).
- **Safety** — no raw sensitive classes (`redaction::detect_secret`), scrubbed
  prose/inline-payload fields are `None`.
- **Window-consistency** — every row's resolved valid time is inside the
  manifest window.

Exit 0 all checks pass, 1 any fails (report still printed), 2 unreadable or
unparseable pack.

## Determinism, redaction, and safety

- Byte-identical across runs on an unchanged store; no wall clock is read unless
  `--captured-at <RFC3339>` is pinned.
- Output is allow-list only: record IDs, handles (file/span, commit SHA,
  `system_native_id`, `protected:v1:`), hashes, bounded labels, valid times, and
  counts — never bodies, hunks, or payloads. `author_email` is always
  `<REDACTED:email:...>` (#116).
- Distinct `(kind, schema_version)` tuples are counted in the manifest so no
  tuple is folded or silently skipped.

## Disclaimer (verbatim in every manifest)

> rows are recorded observations of process execution as imported; never proof
> of control effectiveness, compliance, or completeness; absence of a record
> means no imported evidence, not no event; not an auditor opinion

This is evidence of process execution, never an auditor opinion and never proof
of control effectiveness.
