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
| #334 reviewed-commit facts | *Not yet merged.* Two gap classes always report `capability_unavailable` (see below). |
| #319/#320/#322/#326 log-graph | Runtime incident evidence folded into the CC7.x `error_signatures` / `occurrence_buckets` / `remediation_links` sections (issue #340; see below). |

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
- An in-window `LogOccurrenceBucket` with no `AGGREGATES` attribution edge is
  **excluded** from both the `occurrence_buckets` section and its summary and
  counted under an **`unattributed_bucket`** diagnostic (see the
  `occurrence_buckets` section below) — it cannot be filed under any signature.
- A resolved valid time is **parsed before** the in/out-of-window decision. A
  present-but-malformed (non-RFC-3339) valid time is treated as **unresolved** —
  routed to the same `missing_valid_time` path (count + diagnostic + gap) as a
  truly-absent time — never silently excluded as merely out-of-window. Only a
  record whose valid time parses *and* falls outside `[from, to)` is a legitimate
  no-diagnostic out-of-window exclusion.
- `--from >= --to` exits 2 (`reversed_window`); a non-RFC-3339 bound exits 2
  (`invalid_timestamp`).

### Merged-PR windowing keys on merge time, not update time

The `review_coverage` measurement and the `merged_pr_without_approving_review`
gap window a pull request by its **merge time** — the `merged_at` field promoted
first-class in #333 — using the same half-open `from <= t < to` predicate. This
is deliberately **distinct** from how PR evidence *records* are selected into the
`pull_requests` section: a PR `Task`'s `valid_time` is stamped by the GitHub
importer from `github_updated_at` (the PR's last-*update* time), which routinely
differs from its merge time. Keying the merged-in-window decision on the update
time would drop a PR merged inside the window but updated after it (vacuously
passing coverage and suppressing the gap) and wrongly admit a PR merged before
the window but updated inside it. A PR counts as merged-in-window iff it carries a
`merged_at` whose resolved time is inside the window; a merged PR with no
resolvable `merged_at` is not windowable as merged and never falls back to the
update time. The `pull_requests` section itself still windows records on
`valid_time` (the general per-class rule above) — only the coverage/gap
merged-in-window determination uses `merged_at`.

Consistent with that selection, a `merged_pr_without_approving_review` **gap
row** is timestamped by the **same `merged_at` merge time** used to select the
PR as merged-in-window — never the PR `Task`'s `valid_time` (`github_updated_at`).
A PR merged inside the window but updated after `to` therefore carries an
in-window gap timestamp, so a downstream consumer filtering gaps by the manifest
window keeps it in place instead of dropping or misplacing it. (Gap rows whose
select key already *is* their own valid time — e.g. `commit_outside_any_pr`,
keyed on the commit's own time — are unaffected.)

An approving `Review` suppresses the gap (and counts toward review coverage)
**only when its resolved valid time is at or before the referenced PR's
`merged_at`** merge time. An approval submitted *after* the merge — even if it
still resolves inside the pack window — did not gate the merge and is treated as
post-hoc: it does **not** suppress `merged_pr_without_approving_review` and the
PR counts as unapproved. The comparison uses fields available today (the review's
`temporal.valid_time -> node valid_time -> executed_at` resolution vs the PR's
`merged_at`); it is distinct from the #334-dependent `approval_precedes_final_head`
gap, which compares an approval against the PR's final HEAD commit and stays
`capability_unavailable` until #334 lands.

### Coverage-link edges and their source reviews are included as citable pack content

The specific `REFERENCES_TASK` edges that substantiate an approval — each linking
an **included** approving `Review` to an **included** merged-in-window PR — are
themselves included in the pack, in the `review_coverage` section, as scrubbed +
BLAKE3-hashed records ordered by the same `(valid_time, record_id)` key as every
other section row. So `approved_pr_count` is backed by present, hashed records a
consumer and the offline `verify` can substantiate, rather than by a relationship
the pack never carries. The `review_coverage` measurement's
`approval_link_edge_ids` cites exactly these included edges. A link edge carries
no intrinsic valid time, so it is stamped with its approving review's valid time —
the instant the approval relationship became valid, an in-window value, not a
fabricated one — which makes it window-consistent and lets it flow through the
ordinary scrub/hash/manifest-count pipeline with no special case. Only the edges
that actually back the coverage count are included: unrelated `REFERENCES_TASK`
edges, and links to out-of-window or non-approving reviews, never appear. These
edges are counted in `manifest.included_record_counts` (trust class `other`) and
`manifest.tuple_counts` (`REFERENCES_TASK/v1`).

The `review_coverage` section **also co-locates the approving-`Review` NODES that
are the sources** of those cited link edges, so every coverage edge's source
endpoint resolves during offline `verify` **regardless of whether the catalog maps
a `reviews` section**. Without this, a custom `--catalog` mapping `review_coverage`
but not `reviews` would emit the link edges but hold no source-review node, and the
edge-endpoint check would fail on an assembled-from-an-approved-PR pack. The source
review nodes are scrubbed, hashed, ordered, and counted like every other section
row (trust class `project_state`, `Review/vN` tuple). A review node is admitted to
the section **if and only if it is the source of an included cited coverage edge** —
no arbitrary review nodes. When the control maps **both** `reviews` and
`review_coverage` (as the default `CC8.1` does), an approving review appears in both
sections; that double appearance is intentional (the coverage section stays
self-contained) and the shared manifest-count recompute keeps it self-consistent
across `assemble` and `verify`, so it never breaks Integrity or byte-identity.

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
| `commits` (`Commit`), `pull_requests` (`Task`/`github_pr`), `reviews` (genuine PR `Review` — see below), `structural_deltas` (`Change`), `verification_evidence` (`Verification`/`CommandRun`/`TestRun`/`CIStatus`/`CommandEvidence`/`BenchmarkRun`/`CoverageReport`/`ProofResult`) | real stored node kind | `<class>_domain_absent` (e.g. `delta_domain_absent`) |
| `public_api_deltas` (#157), `validation_runs` (#103) | computed/derived surface, **no stored node kind** | `derived_class_not_materialized` |
| `error_signatures` (`ErrorSignature`), `occurrence_buckets` (`LogOccurrenceBucket`) | log-signature domain (issues #319/#320); populated by #340 when present | `log_domain_absent` |
| `remediation_links` | **derived** join `ErrorSignature → FRAME_RESOLVES_TO → Symbol → CHANGED_IN → Commit` (issue #340), no stored node kind | `log_domain_absent` |
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

The `reviews` class counts **only genuine PR reviews**. A GitHub import emits three
`Review` `review_kind` values (`src/github/records.rs`): `issue_comment` (a comment
on an issue or PR *conversation* — discussion, not a review), `pr_review` (a
submitted pull-request review), and `pr_review_comment` (an inline PR review-thread
comment). Only `pr_review` and `pr_review_comment` are `Reviews`-class evidence; an
`issue_comment` Review — and any `Review` with no recorded `review_kind` — is
excluded from the `reviews` section and does **not** make the `reviews` class
available. The filter is an **allow-list** of genuine review kinds, so a future
non-review `Review` kind cannot silently leak in as review evidence. The same
allow-list gates approval detection: an `issue_comment` Review never counts as an
approving review for `merged_pr_without_approving_review` gap suppression, even if
it carried an `approved` state. This closes a gap where unrelated issue discussion
could mark the required CC8.1 `Reviews` class available and let a relaxed
`--min-review-coverage` pack pass without genuine review evidence.

## Log-graph incident evidence for CC7.x (issue #340)

The monitoring controls **CC7.2** and **CC7.3** fold runtime log-graph incident
evidence into their packs. The foundation is the log-graph domain — umbrella
issue **#319**, the **#320** `ErrorSignature`/fingerprint scan (`eg scan-logs`),
the **#322** `FRAME_RESOLVES_TO` frame resolution, and the **#326** `log-deltas`
valid-time model. This lane is pure **pack-side population** on the **#338**
chassis; it adds no graph domain, kind, edge, or trust class. Exemplar-payload
discipline follows issue **#60**: the pack cites content-addressed `protected:v1:`
handles, never raw log or exemplar bytes.

Three sections carry the evidence, each with a section-level disclaimer:

**`error_signatures`** — one hashed `ErrorSignature` row per in-window signature
(the point predicate on the signature's `first_seen`), plus a `log_summary`
carrying, per signature: the `log:v1:` record ID, `severity`, a `template_hash`
(BLAKE3 of the normalized template excerpt — a redaction-safe fingerprint, never
raw text), a `frame_chain_hash` when frames were captured, the **first/last-seen
valid times clipped to the window** (`first_seen_in_window`/`last_seen_in_window`),
content-addressed **exemplar handles** (`protected:v1:<hash>` + content hash +
source line — handle and hash ONLY, never exemplar text), and each
`FRAME_RESOLVES_TO` join with its `frame_resolution` label **propagated verbatim**
(the #152/#134 precedent). Rows are ordered by `signature_id`.

The exported `ErrorSignature` section rows are **log-text-scrubbed** before they
are hashed (issue #340, Codex round-4 P2): `bundle::scrub_record` never touches
the `log` payload, so a pack-side, log-aware scrub replaces the node's normalized
`template_excerpt` with its own BLAKE3 fingerprint (the exact value the summary
carries as `template_hash`) and replaces each captured backtrace frame's
`module_path`/`file_path` text with its BLAKE3 fingerprint (the structural
`frame_index`/`line` are retained and back `frame_chain_hash`). No raw normalized
template text or readable frame path ever rides a hashed section row — only the
redaction-safe fingerprints the summary already exposes. `LogEvent` exemplar
`event_excerpt` text is likewise fingerprinted (exemplars ride the summary as
content-addressed handles, never as section text).

**Concatenated multi-scan coalescing** (issue #340, Codex round-6). A `LogSource`
is a **non-identity** input: an `ErrorSignature`'s stable ID is
`(repository_id, fingerprint_algorithm, template, severity)` and a
`LogOccurrenceBucket`'s ID omits `LogSource` likewise. A graph built by
concatenating several `scan-logs` outputs for one repo (a documented, legitimate
multi-scan workflow) therefore carries the **same stable log ID once per scan**.
`assemble` **coalesces duplicate log records by stable ID at assemble time**,
BEFORE window filtering and summary building, mirroring the `query log-deltas`
cross-scan semantics: `ErrorSignature` records sharing an ID merge to one node
with the **earliest `first_seen`, latest `last_seen`** (by parsed UTC instant) and
**summed `occurrence_count`**; `LogOccurrenceBucket` records sharing an ID have
their counts **summed** (never deduped by bucket record ID, per issue #361);
`LogSource`/`LogEvent` nodes and log-domain edges collapse to their first
occurrence. Exactly **one summary row and one hashed section node per stable ID**
results, so the concatenated pack still passes its own offline `verify` — the
hard assemble↔verify consistency invariant. A single-scan graph has no duplicate
log IDs, so coalescing is a no-op and every existing pack is byte-identical.

**`occurrence_buckets`** — the **bucket window rule differs from the point
predicate every other class uses**. A `LogOccurrenceBucket` row is in-window iff
its hour `[bucket_start, bucket_start + 1h)` **intersects** the half-open window
`[from, to)`: a partial-overlap hour (its `bucket_start` before `from`, or its
hour extending past `to`) is **included whole, with no interpolation**. The
`log_summary` reports per-signature `in_window_occurrences` — the sum over ONLY
the in-window buckets — with the contributing buckets ordered by `(signature
record_id, hour)`. `verify` applies the same interval-intersection predicate so a
partial-overlap bucket whose `bucket_start` precedes `from` still passes
Window-consistency.

The section **co-locates each bucket's `LogOccurrenceBucket --AGGREGATES-->
ErrorSignature` attribution edge** as a hash-bound row (issue #340, Codex round-3).
The bucket *node* payload carries **no signature field** — the signature is only an
identity input hashed into the bucket's stable ID — so the bucket→signature
attribution that `total.signature_id` asserts must ride a tamper-evident row for
`verify` to re-derive it offline. An `AGGREGATES` edge carries no valid time of its
own; its window relevance rides the bucket it binds, and Window-consistency admits
it on that basis. An **in-window bucket that carries no `AGGREGATES` attribution
edge** cannot be filed under any signature, so it is **excluded from BOTH the
section records and the summary** under a counted **`unattributed_bucket`**
diagnostic (mirroring the `missing_valid_time` exclusion idiom) — never silently
mis-summed, and never left in the section where the reverse-coverage guard would
reject the freshly assembled pack. This upholds a hard invariant: **every pack
`assemble` produces passes its own offline `verify` clean** (the assemble↔verify
consistency invariant).

**`remediation_links`** — a **derived** join, available (capability probe) when
`ErrorSignature` + `FRAME_RESOLVES_TO` + `CHANGED_IN` facts all exist. Each lead
runs `ErrorSignature → FRAME_RESOLVES_TO → Symbol → CHANGED_IN → Commit` where the
commit's valid time is **at or after the signature's window activity**, carries the
`frame_resolution` label verbatim, and cites the signature, symbol, commit, and any
verification records linked to that commit. The section carries **zero hashed
rows** — the leads ride `log_summary` because a remediation commit may legitimately
fall outside the evidence window — and `verify` enforces that empty-row bound as
the section's membership exemption (mirroring `review_coverage`), so no hashed row
can be smuggled in as a remediation "row".

**Integrity binds the log summaries.** The `log_summary` values are the derived
evidence a consumer reads, yet they ride **outside** the hashed `records`. So each
log section carries a `log_summary_hash` — the BLAKE3 of its canonical
`log_summary` — that `verify`'s Integrity recomputes and asserts, exactly as it
binds `review_coverage`'s `measurement`. A tampered summary value (an inflated
`in_window_occurrences`, a swapped `template_hash`/`frame_chain_hash`, a forged
remediation `commit_id`, or a rewritten exemplar handle) whose binding hash was not
also recomputed fails Integrity. On top of that whole-summary hash, the fields with
backing hashed rows are bound to them independently: `error_signatures` rows must
be an exact one-to-one match with the section's `ErrorSignature` nodes and each
row's `template_hash`/`frame_chain_hash`/`severity`/clipped span is bound to that
node's (log-text-scrubbed) payload — `template_hash` binds directly to the node's
stored template fingerprint and `frame_chain_hash` recomputes over the node's
redacted (fingerprinted-path) frame chain, so the per-node bind still catches a
forged summary fingerprint even with the whole-summary hash recomputed, without any
raw log or frame text surviving in the pack — and each `occurrence_buckets` bucket
must resolve to a present
hashed `LogOccurrenceBucket` node with a matching count and hour while every
`in_window_occurrences` must equal the recomputed sum — so the occurrence total
cannot be inflated without adding real, count-matching hashed bucket rows. Each
summary bucket must additionally be **filed under the same signature its co-located
`AGGREGATES` attribution edge names**: moving a bucket under a different
`signature_id` — the node count/hour still bind and both per-signature sums still
balance — fails Integrity because no `AGGREGATES` edge in the section binds that
bucket to the claimed signature, so consumers can never read per-signature
occurrence counts for the **wrong incident**. The
bucket binding is an **exact bijection in both directions**: not only must every
summary bucket resolve to a present hashed node, but every hashed
`LogOccurrenceBucket` node the section carries must be listed by the summary — so a
bucket cannot be silently **dropped** from the summary (under-reporting
`in_window_occurrences` while its hashed row lingers) with the binding hash
recomputed over the reduced summary. The `AGGREGATES` edges the section co-locates
are held to a **bounded membership exemption** (mirroring `review_coverage`): only
an `AGGREGATES` edge whose source is a `LogOccurrenceBucket` node present in the
same section is admitted; any other edge, or an attribution edge for an absent
bucket, fails Integrity. Each summary variant is additionally **bound
to its section class** — `error_signatures`↔`error_signatures`,
`occurrence_buckets`↔`occurrence_buckets`, `remediation_links`↔`remediation_links` —
so a `log_summary` on a non-log section, or a variant relocated onto a mismatched
log section, fails Integrity (the derived `remediation_links` join has no backing
hashed row to catch such a swap, so this class bind is its only guard against
riding the wrong section). The derived `remediation_links` join has no backing
hashed row, so the whole-summary hash plus this class bind are its sole binding
surface.

**Epistemic boundary.** Occurrence counts are **recorded ingestion of the scanned
log sources, not guaranteed-complete telemetry** — absence of a signature is not
proof the error did not occur. Remediation links are **leads, never causal
claims**: a frame binding proves the frame NAMES the symbol, never that the symbol
was at fault, and a changing commit is never asserted to have fixed the error. Log
rows are trust class `runtime_observation` — a program's own claim, parsed but
never verified — and are never tallied as `source_fact` or `verification_evidence`.

## Verdicts and exit codes

`assemble` prints the full pack to stdout and exits:

- **0** — every verdict passed (`verdicts.ok: true`). An empty window is a
  *vacuous success* (every section explicitly empty, all required classes still
  resolve as available).
- **1** — a **non-safety** verdict failed: `required_class_unavailable`,
  citation shortfall, review coverage below `--min-review-coverage` (default
  `1.0`) **for a control that requires review evidence**, or an integrity
  failure. Every such failure leaves a **redaction-safe** pack, so the full
  report is still printed to stdout.
- **2** — usage/load error, **or a whole-artifact safety failure**. A safety
  failure means the pack still carries a raw secret in some field (e.g. a
  `--catalog` control title copied into `manifest.control_title`), so the
  artifact is **suppressed** — it is never serialized to stdout. Instead a
  redaction-safe `pack_safety_failed` envelope is emitted to **stderr**, naming
  the failing field label + secret class (via the safety verdict `detail`) but
  **never the secret value**. Exit 2 ("cannot emit a redaction-safe artifact")
  is used because, like every other exit-2 path, nothing is written to stdout —
  distinct from an ordinary exit-1 verdict failure, which prints the full report.
  The other exit-2 causes are usage/load errors: `unknown_control` (naming the
  catalog's known IDs), `reversed_window`, `invalid_timestamp`,
  `conflicting_input_flags`, `missing_input_flag`, `invalid_min_review_coverage`,
  `catalog_read_error`, `catalog`-parse errors, an unreadable/missing store or
  graph (`graph_read_error`), or `empty_evidence_input` (naming the source path)
  when the input loads **zero records** — a genuinely empty or whitespace-only
  graph, or an initialized store holding zero records. This is distinct from the
  exit-0 vacuous `empty_window` success, which is a *non-empty* input whose
  records merely fall outside the window. A **malformed `--graph` line** — valid
  JSON with a supported schema-version tuple but a wrong-typed `GraphRecord`
  field — is a `graph_parse_error` (exit 2). Its envelope is **sanitized** exactly
  like the catalog/pack parse errors: a stable value-free serde `category` plus
  the 1-based `jsonl_line`, and **never the raw serde message**, which for a
  type error embeds the offending field value — so a secret placed in a mistyped
  field can never leak through the load-error path.

The per-verdict block is: `required_classes`, `citation` (with per-trust-class
tallies), `review_coverage`, `integrity`, `safety`.

The assemble-time `safety` verdict runs the **same whole-artifact scan** as
`verify` (below): it inspects the entire serialized pack — every scrubbed record
*and* every non-record text field (`manifest.control_title` echoed from a
`--catalog`, gap/diagnostic details, verdict details, section and top-level
disclaimers) — before the pack is returned. A secret injected into a non-record
field therefore fails the assembled `safety` verdict (and `verdicts.ok`) rather
than being serialized to stdout while `safety.passed` wrongly reads `true`. When
the assembled `safety` verdict fails, the pack is **not printed at all**: the
handler suppresses the artifact and emits the redaction-safe `pack_safety_failed`
error to stderr at exit **2** (see the exit-code list above), so the raw secret
never reaches stdout.

### `review_coverage` gates only review-requiring controls

The `review_coverage` verdict is **only gating for a control that requires review
evidence** — one whose catalog maps `reviews` or `review_coverage` as
`Requirement::Required` (the same control-scoping predicate that gates the review
gap classes, never a hardcoded control-id list). For such a control (e.g. CC8.1)
the verdict carries `"status":"gating"`, `"applicable":true`, and its `passed`
folds into the pack `ok` exactly as before.

For any other control — a monitoring pack such as CC7.2/CC7.3 that maps **no**
review classes — the verdict is a stable **neutral** result:
`"status":"not_applicable"`, `"applicable":false`, `"passed":true`, and
`"not_applicable_reason":"control_does_not_require_review"`. A neutral verdict
**never contributes to the pack `ok`**, so a monitoring pack assembled over a
shared store that happens to contain an unapproved in-window merged PR does **not**
fail on that unrelated review coverage. The verdict stays visible in the report
(it is never silently dropped) — it simply reports as not-applicable.

Each per-trust-class citation tally carries `total`, `cited`, `missing`, and
`excluded`. `cited` is exactly `eg audit citations`'s satisfying set
(`Cited | AbsentHandleDocumented`); `excluded` counts protected/unverified rows
(`ExcludedProtected`/`ExcludedUnverified`) — a code (`source_fact`) row whose
only handle is a `protected:v1:` payload is counted **excluded, never cited**,
so it drives code-citation completeness **down** exactly as `eg audit citations`
would count it. The tallies are byte-identical to `eg audit citations` on the
same records.

## Gaps — the closed class set

`gaps` always report, regardless of verdicts. Each gap row cites the record IDs
it derives from. Gap classes are **control-scoped**: a PR/commit/review defect is
only emitted for a control that actually requires the relevant evidence class
(derived from the control's `Requirement::Required` entries, never a hardcoded
control-id list). A monitoring control (CC7.2/CC7.3) that requires no
PR/commit/review evidence therefore emits none of those change-management gaps;
`missing_valid_time` stays generic across every control.

| Gap class | Meaning | Emitted when the control requires | Status |
|-----------|---------|-----------------------------------|--------|
| `merged_pr_without_approving_review` | A merged PR Task with no linked **in-window** approving `Review` (via `REFERENCES_TASK`) that resolves **at or before** the PR's `merged_at`. An approving review that resolves outside the pack window, has no resolvable valid time, or is submitted **after** the merge (post-hoc) does **not** suppress this gap. | `pull_requests` and/or `reviews`/`review_coverage` | Fully implemented. |
| `commit_outside_any_pr` | An in-window `Commit` not claimed by any PR via `MERGED_AS`. | `commits` and/or `pull_requests` | Fully implemented. |
| `missing_valid_time` | A class-relevant record with no resolvable valid time — either no valid time at all **or** a present-but-malformed (non-RFC-3339) one. A malformed timestamp is unresolved, not out-of-window. | *(generic — any control)* | Fully implemented. |
| `review_unanchored_no_commit_sha` | A review with no anchoring reviewed-commit SHA. | `reviews`/`review_coverage` | **Needs #334.** Always `capability_unavailable`. |
| `approval_precedes_final_head` | A recorded approval whose commit predates the PR's final head. | `reviews`/`review_coverage` | **Needs #334.** Always `capability_unavailable`. |

Because issue #334 (the `review_commit_sha` field / `REVIEWS_COMMIT` edge) is
not merged — there is no such field or edge in the schema and no derivation logic
exists — the last two classes cannot be derived. When a control requires review
evidence the pack **always** emits a single `capability_unavailable` diagnostic
naming issue #334 and produces zero rows for those two classes. This is
**unconditional**: the pack does **not** probe the input for anything resembling
the #334 facts and does **not** suppress the diagnostic when it finds such a
resemblance, so a pack can never present as if the two checks ran cleanly when
they were in fact skipped (an honest all-clear would be a lie until #334 lands).
A control that requires no review evidence (e.g. `CC7.2`, `CC7.3`) emits neither
those gap classes nor the #334 diagnostic. The enum stays closed; when #334's
derivation lands, the unconditional diagnostic is replaced by real derivation of
the two gap classes.

## `verify <path>`

Re-verifies an assembled pack offline and read-only:

- **Integrity** — recompute the BLAKE3 hash of each scrubbed record and check
  the per-section `(valid_time, record_id)` canonical order. Integrity also
  **recomputes the manifest aggregates** `included_record_counts` (per trust
  class) and `tuple_counts` (per `(kind, schema_version)` tuple) from the actual
  included section rows — via the same helper `assemble` populates them with — and
  fails if either diverges from the manifest's stored values. A pack tampered to
  drop a section row with its section `record_count` adjusted (so the per-section
  length check still matches) but the manifest aggregates left stale therefore
  fails Integrity, with a redaction-safe detail naming the divergent aggregate,
  the first divergent key, and the stored-vs-recomputed numbers (never a payload).
  Integrity also **binds each row to its section's evidence class**: the per-record
  hash covers a row's content but not the section it sits in, so a row moved into
  the wrong section (with both sections' `record_count` fixed and hashes/manifest
  counts left valid) is caught here. Every row in a class-scoped section must map,
  via `evidence_class_for_record`, to that section's class; a mismatch — or a row
  that maps to no evidence class in a class-scoped section — fails Integrity with a
  redaction-safe detail naming the record id, the section it sits in, and the class
  it actually maps to (ids/labels only). The `review_coverage` section is not
  class-scoped — its rows are the substantiating `REFERENCES_TASK` link edges and
  their source approving-`Review` nodes (the `ReviewCoverageMeasurement` rides the
  section's `measurement` field, not a row) — so the row-class check cannot apply.
  Instead, each `review_coverage` row is validated against **two expected shapes**:
  (a) a cited `REFERENCES_TASK` link edge, or (b) an **approving `Review` node that
  is the source of an included cited coverage edge** (co-located so the edge
  endpoints resolve offline even when the catalog maps no `reviews` section). This
  exemption is bounded, not blanket: any other row dropped into `review_coverage` —
  a `Commit`, a `Symbol`, any node that maps to a real evidence class, an
  arbitrary/non-approving review, or any other edge label — fails Integrity with a
  redaction-safe detail naming the record id and `unexpected row in review_coverage
  section`, so a tampered pack cannot present unrelated data as coverage evidence.
  Integrity additionally **binds the `review_coverage` rows to the section's
  `ReviewCoverageMeasurement`**, so the swapped-in edge cannot be a valid-shaped
  but unrelated `REFERENCES_TASK` edge that the measurement still claims to
  substantiate. Three checks: (1) the set of `REFERENCES_TASK` edge row ids must
  **exactly equal** the measurement's cited `approval_link_edge_ids` (only the
  `REFERENCES_TASK` edge rows are compared — the co-located source review nodes are
  bound to the edges by the membership rule above, not cited as approval edges) —
  no edge the measurement does not cite, no cited edge missing from the rows; (2)
  each coverage edge's endpoints must connect an **approving review to a PR task**,
  using the same convention `assemble` used to build the edges: its `source` must be
  an approving `Review` present in the pack (its node is co-located in the
  `review_coverage` section, and may also ride a mapped `reviews` section) and its
  `target`, **when present in the pack**, must be a pull-request task **that
  additionally satisfies `assemble`'s exact coverage-edge eligibility** (so a
  tampered pack cannot re-point a coverage edge at any approving-review→PR pair —
  a post-merge approval, or a PR present for other reasons but not merged
  in-window — recompute hashes/counts, and still substantiate
  `approved_pr_count`): the target must be **merged in-window** (it carries a
  `merged_at` whose parsed time falls inside the manifest window — the
  `merged_pr_ids` selection), and the **source review's resolved valid time must
  be at or before that `merged_at`** (the at-or-before-merge gate; a post-merge
  approval does not count). A merged PR whose Task `valid_time` falls outside the
  window is legitimately **absent** from every section — coverage windows on
  `merged_at` while the PR section windows on `valid_time` — so an *absent* target
  is not a defect and cannot be merge-time-checked; only a *present* target is
  eligibility-checked. And (3) the measurement's `approved_pr_count` must equal the number of
  **distinct PR targets** the coverage edges substantiate (a PR approved by multiple
  reviews yields multiple edges but is one approved PR). Any violation fails
  Integrity with a redaction-safe detail naming the offending edge id and its
  unbound source/target id or the count mismatch — never a payload. Beyond the
  bound rows, Integrity **recomputes and validates the full
  `ReviewCoverageMeasurement`**, since `merged_pr_count`, `coverage`, `passed`,
  and `unapproved_pr_ids` carry no hashed row of their own and could otherwise be
  edited to show a passing result without disturbing any hashed row: (4) **when the
  `review_coverage` verdict is `applicable` (gating)**, `unapproved_pr_ids` must
  **exactly equal** the set of PR ids the pack's own
  `merged_pr_without_approving_review` gap rows cite (both derive from the same
  merged-but-unapproved set, so they can never legitimately diverge). This binding
  is **gated on the verdict's `applicable` flag** because `assemble` always fills
  `unapproved_pr_ids` (merged minus approved) but emits the
  `merged_pr_without_approving_review` gaps **only** for a control that requires
  PR/review evidence — the same condition under which the verdict is
  applicable/gating. A control that maps `review_coverage` merely **optional** (or
  not at all) therefore emits no such gap even with unapproved in-window merged PRs;
  binding to the empty gap set would wrongly fail its own freshly-assembled pack, so
  this check is **skipped** when the verdict is `not_applicable` (a safe relaxation —
  whenever the verdict is applicable the equality holds and is enforced). The
  arithmetic/coverage/`passed` rechecks (5)–(7) below run in **every** case; (5)
  `merged_pr_count` must equal `approved_pr_count + unapproved_pr_ids.len()`
  (every merged-in-window PR is either approved or unapproved); (6) `coverage` is
  recomputed with `assemble`'s exact formula and IEEE-754 arithmetic
  (`approved_pr_count / merged_pr_count`, vacuously `1.0` when none merged) and
  compared bit-for-bit, so no float-epsilon drift is introduced; and (7) `passed`
  must equal `coverage >= min_required` (`min_required` is self-declared — the
  pack carries no independent source for the `--min-review-coverage` value — so
  this catches a lie in `passed` alone against the stored coverage/threshold).
  Finally, the `measurement` is **required on every `review_coverage` section**:
  `assemble` always emits it, even for a 0%-coverage window with merged PRs but no
  approving reviews (empty rows), so an absent measurement — with or without rows —
  fails Integrity rather than silently accepting an artifact stripped of its
  coverage result. Integrity likewise **binds each log section's derived
  `log_summary`** (issue #340), which — like the `measurement` — is the evidence a
  consumer reads yet rides outside the hashed `records`. Every log section carries a
  `log_summary_hash` (BLAKE3 of the canonical `log_summary`); Integrity recomputes
  it and fails on any divergence, and requires the hash to be present iff the
  summary is (a stripped hash, or a hash without a summary, fails). A **present**
  log section (`error_signatures` / `occurrence_buckets` / `remediation_links`)
  must carry **both** a `log_summary` and its `log_summary_hash` (issue #340, Codex
  round-4 P1): `assemble` always emits the derived summary for a present log
  section, and `remediation_links` evidence exists ONLY in the summary (zero hashed
  rows), so stripping **both** the summary and its hash would silently drop all that
  derived evidence yet still verify — absence of either on a present log section is
  now an Integrity defect (an `unavailable` section legitimately carries neither).
  So a tampered
  summary value — an inflated `in_window_occurrences`, a swapped
  `template_hash`/`frame_chain_hash`, a forged remediation `commit_id`, or a
  rewritten exemplar handle — whose binding hash was not also recomputed fails
  Integrity. On top of that whole-summary hash, the fields with backing hashed rows
  are bound to them independently: `error_signatures` rows must be an **exact
  one-to-one match** with the section's `ErrorSignature` nodes, and each row's
  `template_hash`/`frame_chain_hash`/`severity`/window-clipped span is recomputed
  from that node's payload; each `occurrence_buckets` bucket must resolve to a
  present hashed `LogOccurrenceBucket` node with a matching `occurrence_count` and
  hour (no bucket double-counted), and each `in_window_occurrences` must equal the
  recomputed sum — so the occurrence total cannot be inflated without adding real,
  count-matching hashed bucket rows. The bucket binding is an **exact bijection in
  both directions**: every hashed `LogOccurrenceBucket` node the section carries
  must also be listed by the summary, so a bucket cannot be silently **dropped**
  (under-reporting `in_window_occurrences`) with the binding hash recomputed over
  the reduced summary. Finally, each summary **variant is bound to its section
  class** (`error_signatures`↔`error_signatures`,
  `occurrence_buckets`↔`occurrence_buckets`,
  `remediation_links`↔`remediation_links`): a `log_summary` on a non-log section, or
  a variant relocated onto a mismatched log section, fails Integrity. The derived
  `remediation_links` join has no backing hashed row, so the whole-summary hash plus
  this class bind are its sole binding surface.
- **Coverage** — recompute the #65 citation thresholds (>=95% code rows cited;
  100% non-code rows cited).
- **Safety** — scans the **entire serialized pack artifact** for raw sensitive
  classes (`redaction::detect_secret`), not just the section rows. Two things
  must hold. First, every field that `scrub_record` clears is asserted absent on
  each record: the top-level prose (`text`, `validation_summary`,
  `arguments_summary`), the inline handle payloads, **and** the nested
  `user_context` prose (`prompt_text`, `rule_text`, `decision_rationale`,
  `proposed_rule_text`, `edited_rule_text`, `action_summary`, `constraint_text`).
  A tampered row that restores any such field — even with its row hash recomputed
  so Integrity passes — fails Safety with a redaction-safe detail naming the field
  and record id (never the value). This shares one predicate
  (`bundle::first_unscrubbed_field`) with the #68 bundle verify so the pack and
  bundle scrub contracts can never drift. Second, every **non-record** text field
  is scanned too — the manifest fields (including an echoed `control_title` from a
  malicious `--catalog`), section disclaimers and reasons, gap details, diagnostic
  details, verdict details, and the top-level disclaimer — so a secret hidden
  outside the records, with all record hashes left valid, still fails Safety with
  a redaction-safe detail naming the offending area (never the value). A
  whole-artifact backstop scan guarantees a secret in any string field not
  individually enumerated is still caught. The pack's legitimate high-entropy hex
  (BLAKE3 hashes, record IDs, catalog/protected handles, `<REDACTED:email:...>`
  markers) is not flagged, so a clean scrubbed pack passes.
- **Window-consistency** — first the **manifest window bounds themselves** are
  validated, using the same rule `assemble` enforces: both `manifest.window.from`
  and `manifest.window.to` must parse as RFC 3339 and the window must be half-open
  non-empty (`from < to`; a `from >= to` window is the `reversed_window` `assemble`
  rejects). This check runs **before and independent of** the row/gap loops, so a
  vacuous pack (no section rows, no timestamped gaps) whose `manifest.window` was
  hand-edited to an invalid or reversed window fails Window-consistency rather than
  passing vacuously; an unparseable or reversed manifest window fails with a
  redaction-safe detail. Then, reusing those parsed bounds, every section row's
  resolved valid time is inside the manifest window, **and** every timestamped
  `gaps[*].valid_time` is inside the same half-open window. Gaps are timestamped
  rows in the exported pack and consumers filter them by the same window, so a
  tampered gap timestamp outside
  `[from, to)` — or a present-but-malformed (non-RFC-3339) gap timestamp — fails
  Window-consistency with a redaction-safe detail naming the gap class, which
  bound was violated, and the gap's own (allow-listed) valid time. **Exception:**
  a `missing_valid_time` gap is intentionally untimestamped (`valid_time: null`)
  and is allowed, never flagged. Only `missing_valid_time` may be untimestamped:
  any OTHER gap class (e.g. `merged_pr_without_approving_review`,
  `commit_outside_any_pr`) with a `null` `valid_time` fails Window-consistency with
  a redaction-safe detail naming the gap class and `missing required timestamp`.

`verify` scans the **raw supplied artifact** for secrets **before** (and
independently of) deserialization. serde silently discards unknown object keys
when parsing into the pack type, so a secret planted in an unknown field
(top-level or nested) — or in a known-but-mistyped field — would be dropped
before the whole-artifact Safety scan ever ran, letting a visibly secret-bearing
file verify clean. Scanning the raw file text closes that gap: any secret present
in the raw bytes fails the **Safety** verdict (exit 1) with a redaction-safe
detail naming the secret class plus a cheap byte-offset location hint, **never
the secret value**, and neither stdout nor stderr echoes the raw secret. This is
belt-and-suspenders with the per-field and whole-serialized-artifact scans, which
still catch secrets in known fields; the raw scan additionally catches
unknown/dropped fields.

Exit 0 all checks pass, 1 any fails (report still printed), 2 unreadable or
unparseable pack. A parse failure (exit 2) emits a **sanitized** `pack_parse_error`
envelope carrying only the source path, a stable `category` (`io` / `syntax` /
`data` / `eof`), and the 1-based `line` / `column` — never the raw `serde_json`
message. serde's `Display` embeds the offending **value** for a wrong-typed field
(e.g. `invalid type: string "…", expected usize`), so a secret in a mistyped pack
field would otherwise leak; the envelope mirrors the catalog parser's redaction-safe
error contract instead.

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
