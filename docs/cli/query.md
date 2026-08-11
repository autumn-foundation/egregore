# eg query

Query an existing graph JSONL for symbols, files, who last changed a symbol, semantic drift records, or by natural-language similarity.

## Synopsis

```text
eg query symbol   <NAME>  --graph <PATH>    [--at <COMMIT>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query symbol   <NAME>  --data-dir <DIR>  [--at <COMMIT>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query symbols  <PATTERN> --graph <PATH>  [--case-insensitive] [--repo <SELECTOR>] [--format json|text]
eg query symbols  <PATTERN> --data-dir <DIR> [--case-insensitive] [--repo <SELECTOR>] [--format json|text]
eg query file     <PATH>  --graph <PATH>    [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query file     <PATH>  --data-dir <DIR>  [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query who      <NAME>  --graph <PATH>    [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query who      <NAME>  --data-dir <DIR>  [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--repo-path <DIR>] [--format json|text]
eg query drift            --graph <PATH>    [--limit N] [--repo <SELECTOR>] [--format json|text]
eg query drift            --data-dir <DIR>  [--limit N] [--repo <SELECTOR>] [--format json|text]
eg query semantic <QUERY> --data-dir <DIR>  [--limit N] [--repo <SELECTOR>] [--under <PREFIX>] [--format json|text]
eg query semantic-context <QUERY> --data-dir <DIR> [--limit N] [--min-score F] [--repo <SELECTOR>]
eg query semantic-memory <QUERY> --data-dir <DIR> [--limit N] [--repo <SELECTOR>] [--verified-only] [--format json|text]
eg query implementors <TRAIT> --graph <PATH>   [--at <COMMIT>] [--as-of <INSTANT>] [--repo <SELECTOR>] [--format json|text]
eg query implementors <TRAIT> --data-dir <DIR> [--at <COMMIT>] [--as-of <INSTANT>] [--repo <SELECTOR>] [--format json|text]
eg query context  <NAME>  --graph <PATH>    [--repo-path <DIR>]
eg query task     <HANDLE> --graph <PATH>
eg query memory   <HANDLE> --graph <PATH>   [--verified-only]
eg query failures <HANDLE> --graph <PATH>   [--repo <SELECTOR>]
eg query change-impact <HANDLE> --graph <PATH> [--repo <SELECTOR>] [--depth N]
eg query transitive-callers <HANDLE> --graph <PATH> [--repo <SELECTOR>] [--max-depth N] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query transitive-callees <HANDLE> --graph <PATH> [--repo <SELECTOR>] [--max-depth N] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query deps     <HANDLE> --graph <PATH>   [--repo <SELECTOR>] [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
eg query deltas   <BASE> <HEAD> --graph <PATH> [--repo <SELECTOR>]
eg query coupling <PATH>  --graph <PATH>    [--repo <SELECTOR>] [--base <COMMIT> --head <COMMIT> | --at <COMMIT> | --as-of <RFC3339>] [--min-support N] [--limit N] [--format json|text]
eg query lifeline <SYMBOL> --graph <PATH>   [--repo <SELECTOR>] [--format json|text]
eg query public-api       --graph <PATH>   [--repo <SELECTOR>]
eg query undocumented     --graph <PATH>   [--repo <SELECTOR>] [--limit N] [--include-private] [--format json|text]
eg query ownership [PATH] --graph <PATH>   [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--threshold <PERCENT>] [--limit N] [--format json|text]
eg query unreferenced     --graph <PATH>   [--repo <SELECTOR>]
eg query cycles   [SCOPE] --graph <PATH>   [--repo <SELECTOR>] [--format json|text]

eg query at       <PATH>:<LINE> --graph <PATH> [--at <COMMIT>] [--repo <SELECTOR>]
eg query locate   <PATH>:<LINE> --graph <PATH> [--at <COMMIT> | --as-of <INSTANT>] [--repo <SELECTOR>] [--format json|text]
eg query manifest-deps    --graph <PATH>   [--name <CRATE>] [--repo <SELECTOR>] [--format json|text]
eg query churn            --graph <PATH>    [--repo <SELECTOR>] [--limit N] [--format json|text]
eg query churn            --data-dir <DIR>  [--repo <SELECTOR>] [--limit N] [--format json|text]
eg query producer-drift   --graph <PATH>   [--repo <SELECTOR>] [--format json|text]
```

Evidence-backed audit subcommands have their own pages:

- `eg query context` — evidence-backed context for a **symbol** (issue #38).
- `eg query semantic-context` — **natural-language query → evidence-backed
  context** for the top-N semantic matches in one call
  ([semantic-search-guidance.md](semantic-search-guidance.md), issue #90).
- `eg query task` — evidence for a **task** ([task-queries.md](task-queries.md), issue #48).
- `eg query memory` — audit the evidence behind one **agent-authored memory
  claim** ([memory-audit.md](memory-audit.md), issue #64).
- `eg query semantic-memory` — recall prior **agent memory by meaning** with
  provenance, trust-separated from code
  ([semantic-memory-recall.md](semantic-memory-recall.md), issue #91). Supports
  recall-time filtering of superseded and contradicted memories
  ([recall-supersession.md](recall-supersession.md), issue #92).
- `eg query failures` — **prior failed attempts** linked to a code or task
  handle ([failure-history.md](failure-history.md), issue #63).
- `eg query change-impact` — **graph-derived impact leads** grouped by relation
  for a symbol or file handle, for blast-radius triage before editing
  ([change-impact.md](change-impact.md), issue #76).
- `eg query transitive-callers` — the **transitive inbound-reachable set** of a
  symbol with one concrete connecting call path per row, bounded by
  `--max-depth`, deterministic under cycles, honoring `--at`/`--as-of`
  temporal views, with `CALLS` resolution labels propagated along each path
  ([transitive-callers.md](transitive-callers.md), issue #139).
- `eg query transitive-callees` — the **transitive outbound-reachable set** of a
  symbol (its callees/dependencies) with one concrete connecting dependency
  path per row, bounded by `--max-depth`, deterministic under cycles, honoring
  `--at`/`--as-of` temporal views, with an explicit `unresolved` category and
  `CALLS` resolution labels propagated along each path — the outbound mirror of
  `eg query transitive-callers` ([transitive-callees.md](transitive-callees.md),
  issue #253).
- `eg query deps` — the **direct outbound dependencies** of a symbol — what it
  calls, implements, imports, and references — labeled by edge type, with
  unresolved targets as an explicit category, honoring `--at`/`--as-of`
  temporal views ([deps.md](deps.md), issue #123).
- `eg query deltas` — **symbol- and file-level changes between two commits**,
  grouped by stable change class with citable handles
  ([deltas.md](deltas.md), issue #118).
- `eg query coupling` — the files that **historically changed in the same
  commits** as a target file, ranked by a documented normalized coupling
  strength with a minimum-support threshold; historical co-change leads,
  never dependency proof ([coupling.md](coupling.md), issue #153).
- `eg query lifeline` — **one symbol's full evolution timeline** across commit
  history: introduced, each modifying commit with its drift record, removal
  and reintroduction, as newline-delimited JSON events
  ([lifeline.md](lifeline.md), issues #96, #215).
- `eg query public-api` — the crate's **externally-reachable public API
  surface** from recorded visibility and module containment, re-exports
  included, trapped `pub` items excluded
  ([public-api.md](public-api.md), issue #213).
- `eg query undocumented` — **externally-reachable public symbols with no
  recorded doc comment**, each row carrying citable evidence;
  presence/absence only, never doc quality
  ([undocumented.md](undocumented.md), issue #257).
- `eg query ownership` — **per-file authorship aggregates with a primary
  owner and bus-factor signal** from a history store: ranked ownership
  shares, deterministic tie-breaks, `--at`/`--as-of` time travel, and an
  explicit empirical-not-declared boundary
  ([ownership.md](ownership.md), issue #245).
- `eg query unreferenced` — symbols with **no recorded inbound reference
  edges**, as prune-triage leads with citable handles — never proof of dead
  code ([unreferenced.md](unreferenced.md), issue #113).
- `eg query churn` — rank Git-tracked files by **change frequency** across the
  commit history captured by `eg scan-history`, for hotspot triage
  ([churn.md](churn.md), issue #128).
- `eg query producer-drift` — **producer-identity drift** between stored
  records and the running binary: which records a re-extraction with the
  installed grammar/extractor versions could change, grouped by producer
  signature, with legacy and non-code producers bucketed separately
  ([producer-drift.md](producer-drift.md), issue #234).
- `eg query cycles` — **dependency cycles among files** over resolved
  `CALLS` edges and unambiguous imports, each with its ordered closing path
  and citable evidence, optionally scoped to one node for the pre-refactor
  check ([cycles.md](cycles.md), issue #138).

- `eg query at` — resolve a **`file:line` location to its smallest enclosing
  code symbol**, with the enclosing chain reported outermost → innermost
  ([below](#eg-query-at), issue #151).

- `eg query locate` — resolve a **`file:line` position to its innermost symbol
  *and* that symbol's trust-separated `eg query context` bundle** (source facts,
  observations, project state, artifacts, verification evidence) in one call —
  positional entry into the evidence graph without a name guess
  ([below](#eg-query-locate), issue #212).

- `eg query manifest-deps` — every **directly-declared Cargo dependency** with its
  declared requirement, lockfile-resolved version (or a documented unresolved
  marker), declaring package, and manifest handle; `--name` answers the
  direct "do we depend on X?" lookup
  ([manifest-deps.md](manifest-deps.md), issue #180).

Most subcommands accept exactly one input source:

- `--graph <PATH>` — read from a JSONL file produced by `eg scan` or `eg scan-history`.
- `--data-dir <DIR>` — read from an embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. Requires the `embedded-aletheiadb` feature (enabled by default). Providing both `--graph` and `--data-dir` is an error.

`eg query semantic`, `eg query semantic-context`, and `eg query semantic-memory` accept **only** `--data-dir`. The store must additionally have been populated with the `--embed` flag (`eg ingest --adapter embedded --data-dir <DIR> --embed`); a store without embeddings returns no results. `eg query semantic` returns only deterministic **code** hits; `eg query semantic-memory` returns only **agent-authored** memory hits — the two are never blended (issue #91). `eg query semantic-context` follows the `eg query context` no-match convention: on no semantic hit clearing `--min-score` it prints `{"ok":false,"error":{"code":"no_match",...}}` to **stdout** and exits `2`.

### Subsystem scoping — `eg query semantic --under <PREFIX>` (issue #198)

`eg query semantic` accepts an optional `--under <PREFIX>` flag that scopes the
result set to a repo-relative directory prefix, so an agent working inside a
known subsystem retrieves concept-matched code only from that subsystem instead
of hand-filtering crate-wide hits. It reuses the same **segment-aware** prefix
matcher as `eg query subsystem` (issue #83):

- **Segment-aware, no prefix bleed** — `--under src/alpha` returns hits whose
  `repo_relative_path` is under `src/alpha/` but never under `src/alphabet/`.
  The trailing-slash and bare forms resolve identically (`src/alpha` ==
  `src/alpha/`).
- **Filtered before `--limit`** — scoping is applied to the full candidate pool
  *before* the top-N cap, so `--limit N` returns the N best **in-subsystem** hits
  rather than N global hits filtered down to fewer. Ordering is deterministic
  (score descending, then record ID ascending); re-running an identical scoped
  query against an unchanged store produces byte-identical output.
- **Composes with `--repo` and `--limit`** — repository scoping and subsystem
  scoping stack; both apply before truncation.
- **Per-record shape is unchanged** — scoping changes *which* records appear, not
  the per-record JSON contract (`record_id`, `score`, `name`,
  `repo_relative_path`, `span`).
- **Local only** — `--under` is a local-CLI surface for this slice and cannot be
  combined with `--daemon` (clap rejects the combination); daemon/MCP exposure of
  scoped retrieval is owned by other issues.

Outcomes:

- A **malformed/empty** prefix (empty, or nothing left after stripping trailing
  slashes) prints a stable `{"ok":false,"error":{"code":"malformed_under_prefix",...}}`
  diagnostic to **stdout** and exits `1` — an empty prefix is never silently
  reported as success.
- A **valid** prefix that matches zero embedded nodes prints a distinct
  `scoped to '<prefix>', no matches …` message to **stderr** and exits `2` — this
  outcome is worded distinctly from the `no results — store may not have
  embeddings` message so an agent can tell "nothing under this prefix" apart from
  "this store has no semantic index".

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one result was found and printed. |
| `1` | An error occurred (missing file, malformed JSONL, ambiguous commit prefix, unknown/ambiguous repository selector, ambiguous unscoped repository collision). A single-line message is written to stderr. No partial JSON appears on stdout. |
| `2` | No match found. A single-line message is written to stderr. Stdout is empty. |

## Output format

### `--format json` (default)

One JSON object per line (JSONL). Field names are stable across releases. Machine consumers should depend only on the fields documented here; additional fields may be added later.

### `--format text`

One human-readable line per result for terminal use. The exact format is not stable and must not be parsed by scripts.

---

## Corpus scope (issue #427)

Every lane on this page discloses the corpus its answer was computed over via the
envelope fields `corpus_mode` / `corpus_mode_source` / `corpus_disclaimer`. The
full contract — the four `corpus_mode` values, the `--at-head`/`--all-history`
flag pair, and the per-category defaults — is documented once in
[Corpus scope for query lanes](corpus-modes.md).

- **`eg query at`, `eg query locate`, `eg query file`, `eg query who`** are
  **HEAD-anchored** by default (they disclose `head_anchored`, or
  `single_snapshot` over a snapshot-less store); an `--at`/`--as-of` selector
  pins a single commit (`commit_pinned`).
- **`eg query subsystem`, `eg query symbol`, `eg query context`** are now
  **HEAD-anchored** by default too (issue #456): over a `scan-history` store they
  read the records current at each repository's stamped HEAD (a record removed
  before HEAD no longer appears), disclosing `head_anchored` / `default` (or
  `single_snapshot` over a snapshot-less store). Pass `--all-history` for the
  **union of all commit snapshots** (`union` / `explicit_flag`) — the pre-#456
  default — or `--at-head` to force the HEAD-anchored view explicitly. The two
  flags are mutually exclusive with each other and, on `eg query symbol`, with
  its `--at`/`--as-of` selector (a conflict exits `1` with
  `unsupported_combination`). `eg query symbol` discloses the corpus per row
  (it emits bare NDJSON rows); `subsystem`/`context` disclose it on the envelope.

---

## Trust class (`trust`, issue #114)

Every record a **cross-domain context answer** returns carries a single `trust`
field. A cross-domain answer deliberately puts code facts, agent observations,
project state, and verification evidence side by side — the one place where the
distinction between them can collapse. Without a per-row label the cheapest
thing a consuming agent can do is treat every row as equally true, which is the
failure the PRD calls out ("cannot distinguish source truth from agent guesses").

The lanes that carry the derived `trust` class described here:

| Surface | Notes |
|---|---|
| `eg query context` | all record sections + the `excluded` diagnostics |
| `eg query subsystem` | same, plus `semantic_drift` and `log_signatures` |
| `eg query locate` | the context sections **and** the located `symbol` / `enclosing_chain` rows |
| `eg query at` | the located `symbol` / `enclosing_chain` rows (shares the row shape; not itself a cross-domain answer) |
| `eg query semantic-context` | the context sections of each match **and** each match's own anchor row |
| `eg query task` | `tasks`, `acceptance_criteria` (incl. the nested `verification_record`), `source_facts`, `observations`, `artifacts`, `verification_evidence`, `reviews`, `external_links` |
| `eg query changes` | every record-shaped section: `changed_files`, `changed_symbols`, `commits`, `tombstones`, `drift_records`, `unexplained`, `observations`, `project_state`, `artifacts`, `verification_evidence` |
| `eg query error-context` | every `Row`, alongside its pre-existing domain-lookup `trust_class` (see below) |
| daemon `observations_for_symbol`, `criteria_for_task` | the same sections as their CLI counterparts |
| MCP `symbol_context`, `task_evidence` | the same sections; MCP is the primary agentic consumption path |

The CLI, daemon, and MCP transports all call the same derivation over the same
kind of slice, so the *rule* cannot drift between them. One caveat: the CLI
lanes derive over their corpus-filtered slice (`--at-head` / `--all-history`,
see [Corpus scope](#corpus-scope-issue-427)) while the daemon has no corpus
selector — same rule, potentially a different snapshot.

### Domain is not trust

Records already carry a `domain` namespace (issue #3), but `domain` is *where a
record lives*, not *how much it should be trusted*. An `agent_memory`
`Observation` may be an unverified guess, a guess backed by a passing
verification record, or one that has since been contradicted — all three sit in
the same domain. `trust` is what separates them.

### Vocabulary (closed)

| `trust` | Meaning | Typical kinds |
|---|---|---|
| `source_derived` | Deterministic code-graph or semantic-derived fact | `Symbol`, `File`, `Module`, `Import`, `Commit`, `Change`, `Diagnostic`, `ScanCoverage`, `PanicRiskSite`, `DebtMarker`, `UnsafeSite`, `DependencyDeclaration`, `Repository`, `SemanticDrift`, `EmbeddingModel`, `EmbeddingVector`, and code-graph topology edges |
| `verification_evidence` | A recorded verification execution | `Verification`, `CommandEvidence`, `CommandRun`, `TestRun`, `CIStatus`, `BenchmarkRun`, `CoverageReport`, `ProofResult` |
| `agent_verified` | Agent-authored claim citing at least one live **passing** verification record | `Observation`, `Decision`, `Failure` |
| `agent_unverified` | Agent-authored claim with no such citation | `Observation`, `Decision`, `Failure` |
| `agent_contradicted` | Agent-authored claim displaced by a live `CONTRADICTS`/`SUPERSEDES` relationship | `Observation`, `Decision`, `Failure` |
| `project_state` | Imported external work state | `Task`, `AcceptanceCriterion`, `LocalTask`, `GitHubIssue`, `PR`, `Review`, `ExternalIdentity`, `ReviewStateTransition`, `ExternalLink`, `Product`, `Project`, `Plan` |
| `artifact` | Produced bytes | `Artifact`, `PatchArtifact`, `FileEdit` |
| `runtime_observation` | A program's own claim about its execution | `LogSource`, `ErrorSignature`, `LogEvent`, `LogOccurrenceBucket` |
| `other` | No derivation rule applies | `Agent`, `AgentSession`, `AgentRun`, `AgentTurn`, `ToolCall`, `CostUsage`, `Retraction`, user-context kinds, non-topology edges |

The vocabulary is closed and the classifier is an exhaustive match over every
node kind with **no wildcard arm**, so a newly added kind fails to compile until
it is deliberately classified. A kind with no rule maps to `other` — never to
`source_derived` or `verification_evidence`.

### Derivation rules

`trust` is a deterministic function of the record's node kind plus its evidence
and contradiction edges **at the queried snapshot**. Re-running the same answer
over an unchanged store yields byte-identical labels; no wall clock, ranking, or
numeric confidence is involved (per-observation `confidence` is a separate
field and is unaffected).

Non-agent kinds are labelled from their kind alone. Agent-authored kinds
(`Observation`, `Decision`, `Failure`) are resolved in this precedence order:

1. **Displaced → `agent_contradicted`.** The claim is superseded or contradicted
   at this snapshot. Resolved by the same resolver that drives the
   `temporal_status` field and the `excluded` section, so `trust` can never
   disagree with them. A supersession *cycle* counts as displaced for the same
   reason `--supersession exclude` drops it: the chain is broken, so the claim
   cannot be presented as current.

   Issue #114 folds `SUPERSEDES` and `CONTRADICTS` into this single label. Read
   it as **displaced** — "another record has taken this one's place, do not act
   on it" — rather than as a claim that the two records assert opposite things.

2. **`agent_verified`.** The claim cites at least one live verification-domain
   record through a backing relation (`VALIDATED_BY`, `HAS_EVIDENCE`, or
   `PRODUCED_EVIDENCE` — as an evidence link on the claim or an outgoing edge
   from it) **and** that record's recorded outcome is passing. A passing outcome
   is a `status` of `pass`, `passed`, or `success` (trimmed, case-insensitive);
   when `status` is absent, an `exit_code` of `0` counts. Anything else —
   including an absent or unrecognized status — is **not** passing, so the
   derivation fails closed to `agent_unverified` rather than overclaiming.

   Contradiction wins over verification: a displaced claim that also cites a
   passing run is `agent_contradicted`, not `agent_verified`.

3. Otherwise **`agent_unverified`.**

A tombstoned verification record is treated as absent and confers nothing. A
generic relation that merely happens to point at a verification record (e.g.
`RELATES_TO`) does not verify a claim.

The backing-relation traversal is the one `--verified-only` uses on
`eg query memory` and `eg query semantic-memory`, so the two surfaces agree on
*which* records back a claim; `agent_verified` adds the passing requirement on
top.

### What a `trust` label does not claim

- `verification_evidence` marks a **recorded verification execution**, never
  proof of correctness or that the check passed. Read the row's own `status`.
- `agent_verified` states that the claim **cites** a live passing verification
  record. It is a structural fact about the graph, not a truth judgement about
  the claim.
- `source_derived` states that the record was derived deterministically from
  source — not that it still matches the working tree. Use `--repo-path` for
  that ([store freshness](#store-freshness---repo-path-issue-82)).
- `project_state` is a claim made by an issue tracker, not verified by Egregore.
- `agent_contradicted` marks the claim as displaced. It is not a verdict that
  the displacing record is correct.

### A different `trust` key on the single-domain lanes

Several **single-domain** lanes already emit a top-level `trust` field with a
*different* vocabulary, and they are unchanged:

| Lane | `trust` value | What it means there |
|---|---|---|
| `who-imports`, `who-constructs`, `debt-markers`, `unsafe-sites`, `unwrap-expect` | `source_fact` | every row is an extractor fact — a fixed row-class label, not a derived class |
| `change-impact` | `impact_lead` | the row is a lead to inspect, not a fact |
| `deps`, `transitive-callees` | `dependency_lead` | ” |
| `path`, `transitive-callers` | `reachability_lead` | ” |
| `coupling` | `historical_co_change_lead` | ” |

Those lanes return rows from **one** domain, so there is nothing to
disambiguate: their `trust` names the kind of answer the whole lane gives. The
derived vocabulary on this page applies **only** to the cross-domain answers
listed above, which is where the distinction actually has to be made per row.

The practical consequence: `trust` is only comparable across lanes of the same
family. A consumer that wants the derived class must read it from a
cross-domain answer; treating `who-imports`'s `source_fact` and `context`'s
`source_derived` as the same string will not work. This overlap is a known
wart — unifying it would be a breaking change to the documented output of ten
shipped lanes and is deliberately out of scope here.

### Relationship to the older `trust_class` field

Some lanes (`eg query memory`, `eg query sessions`, `eg query
transaction-time`, `eg query error-context`, and the subsystem `log_signatures`
rows) already emit a `trust_class` field. That one is a **static, kind-only** bucket and is
unchanged. `trust` is the **derived** per-row class described above; where the
two overlap they agree, modulo two deliberate refinements:

| `trust_class` | `trust` |
|---|---|
| `source_fact` | `source_derived` |
| `agent_authored` | `agent_verified` / `agent_unverified` / `agent_contradicted` |
| all others | identical string |

`eg query error-context` emits both. Its `trust_class` is a pure **domain**
lookup — every agent-memory record reads `agent_observation` whatever its
evidence — so a verified and a contradicted observation are indistinguishable
there. Read `trust` when that distinction matters.

### Rows that carry no `trust`

The `unresolved` section describes an evidence link whose target record is
**absent** from the store slice. There is no record to classify, so those
entries carry no `trust` — labelling an absence would be fabrication. The
`excluded` diagnostics section does carry `trust`, since those entries name real
records the answer chose to withhold.

---

## Store freshness (`--repo-path`, issue #82)

`eg query symbol`, `eg query file`, and `eg query context` accept an optional
`--repo-path <DIR>` pointing at a working tree. When set, each result carries a
non-fatal `freshness` field with a stable code (`fresh` / `stale_head` /
`stale_dirty` / `unknown`) computed by comparing the store's stamped
[source snapshot](../schema/source-snapshot.md) against that working tree, so an
agent can downgrade trust in a cited `repo_relative_path` + `span` handle. The
result is **never suppressed** on a non-`fresh` verdict.

Without `--repo-path` the `freshness` field is absent and output is unchanged.
See [`freshness.md`](freshness.md) for the standalone store-level report.

---

## Extraction completeness (`extraction_completeness`, issue #87)

`eg query symbol` and `eg query file` return an `extraction_completeness` signal (`complete` / `partial`) in their JSON output format.
- `complete`: The queried file or symbol scope contains zero `Diagnostic` markers (unparsed macro blocks or syntax errors).
- `partial`: The scope contains at least one active `Diagnostic` marker, indicating that code extraction may have been incomplete, and some symbols or elements within this scope might be missing from the index.

For `eg query file <file>`, if the completeness is `partial`, the output also includes a `diagnostics` array enumerating the active diagnostic markers within that file (each containing `record_id`, `repo_relative_path`, and `span`).

The completeness signal is advisory and states only that extraction *may* be incomplete in the scope; it does not make direct truth claims about symbol presence or absence.


---

## Cross-Producer Determinism & Portable Spans

Egregore guarantees cross-platform and cross-producer byte-for-byte stable scans for identical Git commits:

1. **Path Normalization**: All `repo_relative_path` and path-based fields are normalized to use forward slashes (`/`) as separators, ensuring that scans run on Windows do not leak backslashes (`\`) compared to scans on POSIX systems.
2. **Line-Ending Normalization (Portable Spans)**: Source files are normalized to use LF (`\n`) line endings before syntax parsing and span generation. As a result, all `SourceSpan` byte offsets (`start_byte`, `end_byte`) are completely portable and stable regardless of whether the local checkout has LF or CRLF (`\r\n`) line endings (e.g. from Git's `core.autocrlf` setting).
3. **Identity Stability**: Node and edge stable record IDs are computed on normalized inputs and match perfectly across platforms.

### Distinctions from other axes

- **VS. Freshness/Staleness (issue #82)**: Line-ending normalization ensures that identical content produces identical facts. Working-tree staleness (dirty files or HEAD changes) is an orthogonal axis tracked by the `freshness` signals.
- **VS. Extraction Accuracy (issue #93)**: Portable spans guarantee structural identity and portability of spans across platforms; they do not impact the linguistic precision or syntax coverage of the language-specific extractors.

---

## Repository scope (`--repo`, issue #67)

A shared local store can hold more than one repository, and two repositories
routinely contain the same repo-relative path (`src/lib.rs`) or the same
symbol name (`Widget`). `name` and `repo_relative_path` alone are therefore
not unique handles: a citation that names the right-looking path in the wrong
repository is worse than a clean no-match. Repository scope keeps the
repository boundary explicit on every public code query.

### Shortest local workflow

```sh
# Two local repositories scanned into one store — no network, no git remote
# required (operator overrides force distinct identities for plain dirs):
eg scan ./widget-a --repo-id-override widget-a --out a.jsonl
eg scan ./widget-b --repo-id-override widget-b --out b.jsonl
cat a.jsonl b.jsonl > store.jsonl

# Scope a query to exactly one repository:
eg query symbol widget --graph store.jsonl --repo widget-a
eg query file src/lib.rs --graph store.jsonl --repo widget-a
```

The same `--repo` flag works against an embedded store (`--data-dir`) and
through the daemon (`--daemon`), where it maps to the `repo` query-verb param
(see [`docs/schema/daemon-query.md` §5.1](../schema/daemon-query.md)).

### Selector forms

`--repo` accepts the stable `Repository` record ID or any human-usable handle
from the repository identity payload
([`docs/schema/repository-identity.md`](../schema/repository-identity.md)):

| Selector | Example |
|----------|---------|
| Stable repository record ID | `codegraph:v4:e2aa0c…` |
| Display name (remote-derived `owner/name`) | `acme/widget` |
| Short name (final path segment of the display name) | `widget` |
| Basename / operator override | `widget-a` |
| Normalized remote URL | `https://github.com/acme/widget` |
| Root commit SHA (`local_root_commit` identities) | `83fa99…` |
| Canonical path (`local_path` identities) | `/home/me/src/widget` |

Tombstoned `Repository` records (e.g. after an identity change in an
incremental scan) are not selectable and never make a live repository's
selector ambiguous.

Failures are stable, machine-readable JSON on stderr with exit code `1`:

- `{"code":"unknown_repository_selector","selector":"…"}` — nothing matches.
- `{"code":"ambiguous_repository_selector","selector":"…","candidates":[…]}` —
  more than one repository matches (e.g. two repos sharing a basename). The
  candidates list every matching repository record ID; Egregore never picks
  one implicitly.

### When repository scope is required

- **Unscoped list queries** (`eg query symbol`, `eg query file`, `eg query
  drift`, `eg query semantic`) stay usable in a multi-repo store: every row
  carries `repository_id` and `repository`, and colliding rows from different
  repositories are returned side by side — never merged across the boundary.
- **Unscoped single-answer time views** (`eg query symbol --as-of`, `--at`)
  fail closed on a collision with
  `{"code":"ambiguous_repository","repositories":[…]}` and exit `1`, because a
  single row cannot represent two repositories. Re-run with `--repo`. Rows the
  store topology cannot attribute (legacy records) count as their own
  candidate group; when one contributes to the collision the diagnostic adds
  `"includes_unattributed_rows":true`.
- **Scoped `eg query file`** excludes a matching path in another repository
  from the result set and reports it only through a stderr diagnostic:
  `{"code":"excluded_other_repositories","repo_relative_path":"src/lib.rs",
  "excluded_repository_count":1,"excluded_row_count":2}`.

Repository scope composes with the temporal selectors: `--repo` answers
"which repository?", `--at`/`--as-of`/`--tx-as-of` answer "which time view?"
([`docs/schema/temporal-selectors.md`](../schema/temporal-selectors.md));
neither dimension silently widens the other.

### Compared to the boring alternatives

| Tool | How it scopes | What it lacks here |
|------|---------------|--------------------|
| `rg` / `git grep` / `git log -S` from one checkout | Implicitly: you run it inside a single working tree | No shared store, no temporal graph, no drift/semantic/evidence joins; scoping disappears the moment results from several checkouts are merged into one report |
| GitHub Code Search `repo:owner/name` ([syntax](https://docs.github.com/en/search-github/github-code-search/understanding-github-code-search-syntax)) | Explicit `repo:` qualifier | Hosted-only; no local store, no AletheiaDB trust separation, no valid/transaction-time views |
| Sourcegraph `repo:^github\.com/acme/widget$` ([docs](https://sourcegraph.com/docs/code-search/queries)) | Explicit `repo:` filter with revision scoping | Server deployment; not a local-first evidence-citable graph that joins to agent memory and verification records |
| rust-analyzer / SCIP workspace navigation | Project-root-relative documents inside one workspace | Single-workspace by construction; cannot answer for a shared store where two repositories carry the same project-root-relative path |

Egregore's slice is narrower than any of these on raw search, and that is the
point: a repository-scoped, locally citable row (`record_id` +
`repository_id` + path + span) that later joins to memory, task, artifact, and
verification domains without leaving the shared store.

---

## eg query subsystem

Return every known cross-domain fact for a repository-relative directory / module
prefix in one trust-separated envelope (issue #83): code facts, agent
observations, project state, artifacts, verification evidence, semantic drift,
runtime error signatures, and unresolved evidence links.

```text
eg query subsystem <PREFIX> --graph <PATH> [--format json|text]
eg query subsystem <PREFIX> --data-dir <DIR> [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PREFIX>` | yes | Repository-relative directory / module prefix, e.g. `src/adapters`. |
| `--graph <PATH>` | one of `--graph` / `--data-dir` | Graph JSONL produced by `eg scan`, `eg scan-history`, `eg scan-logs`, and/or `eg resolve-frames`, concatenated. |
| `--data-dir <DIR>` | one of `--graph` / `--data-dir` | Embedded store directory (transaction-time-current view). |
| `--format` | no | `json` (default) or `text`. |

### Prefix matching

Matching is **segment-aware**: `src/alpha` matches `src/alpha` and
`src/alpha/foo.rs` but never `src/alphabet/x.rs`. Both `src/alpha` and
`src/alpha/` normalize identically.

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | At least one record resolved under the prefix. |
| `1` | `malformed_prefix` — the prefix is empty or reduces to nothing after stripping trailing slashes. |
| `2` | `no_match` — no records found under the prefix. |

### Envelope sections

The JSON envelope carries `ok`, `prefix`, and the trust-separated sections
`source_facts`, `topology_edges`, `observations`, `project_state`, `artifacts`,
`verification_evidence`, `semantic_drift`, `log_signatures`, `unresolved`, and
`excluded`. `log_signatures` and `unresolved` are **always present** (`[]` when
empty); `topology_edges` and `excluded` are omitted when empty.

### `log_signatures` (issue #325)

Runtime `ErrorSignature` records (from `eg scan-logs` #320, resolved to code by
`eg resolve-frames` #322) whose backtrace frames land under the prefix.

**Scope rule (frozen).** A signature appears iff at least one of its
`FRAME_RESOLVES_TO` edges carries a `resolved` **or** `path_only` resolution
whose code-graph target's `repo_relative_path` passes the same segment-aware
prefix matcher used everywhere else in this lane. A signature whose only
in-prefix frames are `ambiguous` or `unresolved` never enters `log_signatures`;
its dangling frame targets surface through the `unresolved` section instead, so
they are never silently dropped. Sibling paths never bleed: a signature
resolving into `src/alphabet` never appears for `src/alpha`.

Rows are coalesced by stable signature ID (a graph combining multiple
`scan-logs` outputs carries the same signature ID once per source): earliest
`first_seen`, latest `last_seen`, summed `occurrence_count`. Rows sort by
`record_id` and output is byte-identical across runs.

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable `ErrorSignature` record ID (`log:v2:…`). |
| `kind` | string | yes | Always `"ErrorSignature"`. |
| `trust_class` | string | yes | Always `"runtime_observation"`. |
| `schema_version` | number | yes | Log-domain schema version. |
| `severity` | string | yes | Closed severity class: `fatal` / `error` / `warn`. |
| `occurrence_count` | number | yes | Total occurrences summed across scanned sources. |
| `template_excerpt` | string | yes | Bounded, post-redaction template excerpt — the only permitted message text; never raw log text. |
| `first_seen_valid_time` | string | when recorded | Valid time of the earliest occurrence. |
| `last_seen_valid_time` | string | when recorded | Valid time of the latest occurrence. |
| `resolved_frames` | array | yes | In-prefix resolved frames, each with `frame_index`, `frame_resolution` (`resolved` / `path_only`), `target_repo_relative_path`, and (for a `Symbol` target) `target_span`. |

**Rows are leads, not proof.** A signature is a producing program's own claim,
deterministically parsed but never verified; a frame binding proves the frame
*names* the symbol, never that the symbol is at fault. An empty `log_signatures`
means "no scanned source resolved here", **not** "no errors exist".

`occurrence_count` reflects the **scanned log sources only**, not all runtime
reality: counts sum across the sources you scanned, and re-scanning the same log
inflates them (issue #361). Over `--graph` (concatenated JSONL) duplicate-ID
signatures from distinct sources are coalesced here; over `--data-dir` the
embedded store already retains one record per stable log ID (last-write-wins),
so cross-scan coalescing is not reconstructable there — combine `scan-logs`
outputs at the `--graph` level or use per-source stores for multi-scan
aggregation (issue #363).

**When to use vs `eg query error-context` (#324).** Use `subsystem` for
directory-first triage ("what's happening under `src/adapters`?"); use
`error-context` (when available) for error-first triage ("what does this
signature touch?"). This lane is the health report; `error-context` is the
error's neighborhood.

---

## eg query symbol

Find `Symbol` nodes by name.

```text
eg query symbol <NAME> --graph <PATH> [--at <COMMIT>] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<NAME>` | yes | Exact symbol name to look up. |
| `--graph <PATH>` | yes | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--at <COMMIT>` | no | Restrict to the single best record whose `git_commit` starts with this SHA prefix. Exit `1` with `error: ambiguous commit prefix` when the prefix matches more than one distinct commit SHA. Requires a history graph. |
| `--repo <SELECTOR>` | no | Restrict results to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable BLAKE3-based record ID (`codegraph:v1:…`). |
| `schema_version` | number | yes | Record schema version for the returned `Symbol`; see [`docs/schema/schema-versioning.md`](../schema/schema-versioning.md). |
| `name` | string | yes | Symbol name. |
| `kind` | string | yes | Always `"Symbol"`. |
| `repo_relative_path` | string or null | yes | Repository-relative file path, e.g. `"src/lib.rs"`. |
| `span` | object or null | yes | Source span with `start_byte`, `end_byte`, `start_line`, `end_line`. |
| `visibility` | string | Rust declaration-surface symbols | Declaration visibility class from the closed set `public` / `crate` / `restricted` / `private`, derived from the source `pub` modifier (`pub(in path)` and `pub(super)` map to `restricted`; `pub(self)` and no modifier map to `private`). Present on Rust `fn` / method / `struct` / `enum` / `trait` / type-alias / `const` / `static` symbols extracted at issue #124 or later; absent on `impl` symbols and records from older graphs. |
| `signature` | string | Rust declaration-surface symbols | Normalized declaration header: item keyword through the end of the parameter list / return type / where-clause for callables (or the item header for type-defining items), body excluded, interior whitespace collapsed deterministically. Same presence rules as `visibility`. |
| `doc` | string | when the item has a doc comment | Doc-comment text (`///` or `/** */`) after redaction policy v1 (see [`docs/schema/redaction.md`](../schema/redaction.md)); a secret-shaped value is replaced by a `<REDACTED:class:hash>` marker. Omitted entirely when the item has no doc comment — never an empty string. |
| `git_commit` | string | only in history graphs | Full commit SHA for history-backed records. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the row. Absent only for legacy graphs without repository topology. |
| `repository` | string | when attributable | Human-usable repository identity handle, e.g. `acme/widget`. |

### Example

```sh
eg scan . --out g.jsonl
eg query symbol scan_repository --graph g.jsonl
```

```json
{"record_id":"codegraph:v1:abc...","schema_version":1,"name":"scan_repository","kind":"Symbol","repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":500,"start_line":51,"end_line":71},"visibility":"public","signature":"fn scan_repository(repo_path:impl AsRef<Path>)->Result<Graph>","doc":"Scans a repository working tree into a code graph."}
```

---

## eg query symbols

Find `Symbol` nodes by **partial name** — a literal substring or an anchored
`*` glob — against the structural store, with no embedding model or `--embed`
store required (issue #102).

```text
eg query symbols <PATTERN> --graph <PATH> [--case-insensitive] [--repo <SELECTOR>] [--format json|text]
```

### Pattern semantics

| Pattern | Meaning | Example matches |
|---------|---------|-----------------|
| no `*` | Literal substring anywhere in the name. | `handle_` matches `handle_input`, `try_handle_x`. |
| contains `*` | Anchored glob over the **whole** name: each `*` matches any (possibly empty) run of characters; every other character is literal. | `handle_*` (prefix), `*_sink` (suffix), `Embedded*Adapter`. |

No other metacharacters are supported: this slice is literal substring plus
`*` glob only — full regular expressions and fuzzy/typo-tolerant ranking are
out of scope. An empty pattern is rejected as malformed (exit `1`).

Matching is **case-sensitive by default**; pass `--case-insensitive` to
compare both sides Unicode-lowercased.

Only `Symbol` node names are searched. Comments, string literals, and doc
text never produce a match — the false-positive class a raw `rg` search
cannot avoid. Tombstoned (deleted) symbols are excluded from current-state
results, in parity with `eg query file`.

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATTERN>` | yes | Substring or anchored `*`-glob name pattern (must be non-empty). |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store. Structural records only — the store does **not** need `--embed`. |
| `--case-insensitive` | no | Lowercase both pattern and names before matching. |
| `--repo <SELECTOR>` | no | Restrict results to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--format` | no | `json` (default) or `text`. |

### Output

Rows have the same shape and fields as [`eg query symbol`](#eg-query-symbol):
`record_id`, `schema_version`, `name`, `kind`, `repo_relative_path`, `span`,
the declaration-surface fields when present, `git_commit` for temporal
(history-graph) records, repository identity, and
`extraction_completeness`.

Output is deterministic and byte-stable: identical inputs produce identical
bytes, with rows sorted by `(repo_relative_path, span.start_line,
record_id)` ascending.

### Exit codes

Same contract as the other query subcommands, so a no-match is never
conflated with a store-absent or malformed-input condition:

| Code | Meaning |
|------|---------|
| `0` | At least one symbol matched and was printed. |
| `1` | Error: missing/malformed store, both or neither of `--graph`/`--data-dir`, empty pattern, unknown/ambiguous `--repo` selector. Stderr carries the message; stdout stays empty. |
| `2` | No symbol name matched the pattern. Stderr says `no match found for pattern ...`; stdout stays empty. |

### Example

```sh
eg scan . --out g.jsonl
eg query symbols 'handle_*' --graph g.jsonl
```

```json
{"record_id":"codegraph:v1:abc...","schema_version":1,"name":"handle_input","kind":"Symbol","repo_relative_path":"src/alpha.rs","span":{"start_byte":0,"end_byte":100,"start_line":10,"end_line":20},"extraction_completeness":"complete"}
{"record_id":"codegraph:v1:def...","schema_version":1,"name":"handle_request","kind":"Symbol","repo_relative_path":"src/beta.rs","span":{"start_byte":0,"end_byte":100,"start_line":30,"end_line":40},"extraction_completeness":"complete"}
```

This subcommand is **CLI-only** in this slice: the daemon exposes no
partial-name verb (see
[`docs/schema/daemon-query.md`](../schema/daemon-query.md)), and it does not
accept `--at`/`--as-of`/`--tx-as-of` temporal selectors or `--daemon`
routing.

---

## eg query file

List all `Symbol` nodes defined in a file, resolved through `DEFINES` edges.

```text
eg query file <PATH> --graph <PATH> [--at <COMMIT> | --as-of <RFC3339>] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATH>` | yes | Repository-relative file path, e.g. `src/lib.rs`. |
| `--graph <PATH>` | yes | Graph JSONL to query. |
| `--at <COMMIT>` | no | Pin the listing to the file's recorded state at this commit SHA or unique prefix (issue #158). Requires a `scan-history` store. Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Pin the listing to the file's recorded state at the most recent commit at or before this instant (valid-time axis). Mutually exclusive with `--at`. |
| `--tx-as-of <RFC3339>` | no | Reserved for `query file`: always returns a `not_implemented` error envelope and exit `1`, never a silently coerced result. Transaction time currently covers `query symbol` only (issue #66). |
| `--repo <SELECTOR>` | no | Restrict results to one repository. A matching path in another repository is excluded and reported only through the `excluded_other_repositories` stderr diagnostic. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

Same fields as `eg query symbol` (see above), except the declaration-surface fields `visibility`, `signature`, and `doc`, which are omitted from file listing rows to keep the per-file answer lean — use `eg query symbol <NAME>` for a symbol's contract. Results are sorted by `span.start_line` ascending, then `record_id`.

### File snapshot at a point in time (`--at` / `--as-of`, issue #158)

`--at <COMMIT>` / `--as-of <RFC3339>` reconstruct the deterministic set of
symbols the file defined **at that point**, from a `scan-history` graph or an
embedded store — no network, no re-parsing of `git show` output, no working-tree
reads. `scan-history` records a full snapshot at every commit, so a symbol
tombstoned at or before the selected point simply has no snapshot there and can
never leak into the result; spans and names are the recorded state as-of the
point, not the current tree. The answer is code-facts only (`File`/`Symbol`/
`Commit` history records) — agent observations, project/task, artifact, and
verification records are never mixed in.

```sh
# Replay history first (reads Git objects only, never mutates the checkout)
eg scan-history . --out history.graph.jsonl

# What symbols did src/parser.rs define at commit 4f0c2b1?
eg query file src/parser.rs --graph history.graph.jsonl --at 4f0c2b1

# ... and as of an instant (most recent commit at or before it)?
eg query file src/parser.rs --graph history.graph.jsonl --as-of 2026-01-02T00:00:00Z
```

Unlike the current-state listing, the point-in-time answer is a **single JSON
envelope** (not JSONL rows), byte-identical across repeated runs:

```json
{
  "ok": true,
  "path": "src/parser.rs",
  "at": "4f0c2b1",
  "as_of": null,
  "resolved_commit": "<full SHA the answer was computed against>",
  "resolved_valid_time": "2026-01-01T00:00:00Z",
  "file_record_id": "codegraph:v5:...",
  "file_schema_version": 5,
  "symbols": [
    {
      "record_id": "codegraph:v5:...",
      "schema_version": 5,
      "name": "parse",
      "kind": "Symbol",
      "symbol_kind": "function",
      "repo_relative_path": "src/parser.rs",
      "span": { "start_byte": 0, "end_byte": 30, "start_line": 1, "end_line": 1 },
      "commit": "<resolved full SHA>",
      "valid_time": "2026-01-01T00:00:00Z"
    }
  ],
  "returned": 1,
  "diagnostics": []
}
```

Every row carries a stable record ID plus a repo-relative file/span handle
resolved as-of the point (module-level symbols without a span carry a
documented `absent_span_reason`), and the envelope records the resolved
commit/instant it was computed against. Output is bounded and redaction-safe:
record IDs, commit handles, paths, spans, and counts only — never raw source
text, patch hunks, or commit-message bodies.

Not-found is never conflated with empty: a file that existed at the point but
defined zero symbols returns `ok: true` with an explicit `empty_symbol_set`
diagnostic (exit `0`), while an unknown path or a path that did not exist at
the point fails with a machine-readable error and exit `2`.

In a shared multi-repository store, `--as-of` resolves the instant on the
queried path's **own repository timeline**: an unrelated repository's newer
commit never wins the at-or-before race, so it cannot make the file look
absent at a commit its repository never had. When more than one repository
records the same path, an unscoped `--as-of` query fails closed with
`ambiguous_repository` (rerun with `--repo <SELECTOR>`), matching the
issue #67 contract for single-answer time views.

#### Exit codes for `--at` / `--as-of`

Failures print `{"ok":false,"error":{"error_type":...}}` to stdout — never a
fabricated symbol, never an empty success:

| Condition | `error_type` | Exit |
|-----------|--------------|------|
| Resolved point (including an `empty_symbol_set` result) | — | `0` |
| Path matches nothing at any recorded commit | `unknown_path` | `2` |
| Path known to history but absent at the resolved point | `file_absent_at_point` | `2` |
| Commit prefix matches nothing | `missing_commit` | `2` |
| Store has no commit history (plain `scan` graph) | `empty_history` | `2` |
| `--as-of` instant is not valid RFC 3339 | `invalid_instant` | `2` |
| No commit at or before the `--as-of` instant | `no_commit_at_or_before_instant` | `2` |
| `--at` combined with `--as-of` | — (flag conflict, clap) | `2` |
| Commit prefix matches multiple commits | `ambiguous_commit_prefix` | `1` |
| Unscoped path+commit collision across repositories | `ambiguous_repository` | `1` |
| `--tx-as-of` supplied | `not_implemented` (as `error.code`) | `1` |

#### When to use which temporal tool

- **`eg query file <PATH> --at/--as-of`** — you have a *path* and want its
  past symbol set; you do not yet know the symbol names.
- **`eg query symbol <NAME> --at/--as-of`** — you already know the *name* and
  want that one symbol's state at a point.
- **`eg query lifeline` (issue #96)** — one symbol's full lifecycle across all
  history, not a per-file snapshot.
- **`eg query deltas <BASE> <HEAD>` (issue #118)** — what changed *between* two
  commits, not what existed *at* one.
- **`eg query symbol --tx-as-of` (issue #66)** — what the *store* knew at a
  transaction instant; the file listing has no transaction-time view yet.
- **`git show <COMMIT>:<PATH>`** — raw file bytes at a commit; you must
  re-parse them and re-filter same-name/comment/string noise yourself, with no
  stable record IDs to join against the graph.

---

## eg query who

Find who last changed a symbol (issue #116).

```text
eg query who <NAME> --graph <PATH> [--at <COMMIT> | --as-of <RFC3339>] [--repo <SELECTOR>] [--format json|text]
```

Answers "who last changed `<NAME>`" by returning the Git author of the most
recent commit at or before the queried point in history that actually changed
the symbol (following file renames), together with the commit SHA and the
symbol's repo-relative file handle. Requires a history graph produced by
`eg scan-history`: a plain `eg scan` graph carries no `Commit` records and
yields a no-match exit `2`.

Authorship is a **deterministic VCS-derived fact**, not an agent-authored
observation: `eg scan-history` records the normalized Git author identity
(`author_name` + `author_email`, from the commit's `%an` / `%ae` author
metadata) on every `Commit` record, on its own fields distinct from the
`author_time` timestamp in the temporal metadata. It states who empirically
made a change; it is **not an ownership claim** — it asserts nothing about
declared ownership, responsibility, or review authority (CODEOWNERS-style
declarations answer a different question). Repeated `eg scan-history` runs of
an unchanged repository reproduce the author fields byte-for-byte.

`author_email` is **redaction-eligible** PII: a local store retains the raw
address at rest, while evidence-bundle export always scrubs it to a
`<REDACTED:email:hash_prefix>` marker
(see [`docs/schema/redaction.md`](../schema/redaction.md) and
[`bundle.md`](bundle.md)).

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<NAME>` | yes | Exact symbol name to look up. |
| `--graph <PATH>` | one of | History graph JSONL produced by `eg scan-history`. Mutually exclusive with `--data-dir`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated from a history graph. Mutually exclusive with `--graph`. |
| `--at <COMMIT>` | no | Report authorship as of this commit SHA or unique prefix (valid-time axis). Exit `1` on an ambiguous prefix. Mutually exclusive with `--as-of`. |
| `--as-of <RFC3339>` | no | Report authorship at the most recent commit at or before this instant (valid-time axis). Mutually exclusive with `--at`. |
| `--tx-as-of <RFC3339>` | no | Reserved for `eg query who`; exits `1` with an error rather than silently ignoring the flag. |
| `--repo <SELECTOR>` | no | Restrict results to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--repo-path <DIR>` | no | Working-tree path for the non-fatal `freshness` code (see [Store freshness](#store-freshness---repo-path-issue-82)). |
| `--format` | no | `json` (default) or `text`. |

Without a temporal selector the answer is computed at HEAD. With `--at` /
`--as-of` authorship is reported **as-of the queried point in history**, scoped
to the HEAD lineage — the same valid-time semantics as `eg query symbol`
([`docs/schema/temporal-selectors.md`](../schema/temporal-selectors.md)).

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `symbol_name` | string | yes | The queried symbol name. |
| `commit_sha` | string | yes | Full SHA of the most recent commit at or before the queried point that changed the symbol — the citable commit handle. |
| `author_name` | string | when recorded | Git author display name from that commit. |
| `author_email` | string | when recorded | Git author email address from that commit. Redaction-eligible PII (see above). |
| `valid_time` | string | yes | Committer timestamp of that commit (valid-time axis). |
| `repo_relative_path` | string or null | yes | Repository-relative file path of the symbol — the citable file handle. |
| `freshness` | string | with `--repo-path` | Non-fatal store-freshness code (`fresh` / `stale_head` / `stale_dirty` / `unknown`). |

### Example

```sh
eg scan-history . --out history.graph.jsonl
eg query who scan_repository --graph history.graph.jsonl
eg query who scan_repository --graph history.graph.jsonl --as-of 2026-01-02T00:00:00Z
```

```json
{"symbol_name":"scan_repository","commit_sha":"83fa99...","author_name":"Jane Dev","author_email":"jane@example.com","valid_time":"2026-01-01T12:00:00Z","repo_relative_path":"src/lib.rs"}
```

---

## eg query drift

Find `SemanticDrift` nodes ranked by cosine-distance score descending.

```text
eg query drift --graph <PATH> [--limit N] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--graph <PATH>` | yes | Graph JSONL produced by `eg scan-history` after semantic drift computation. |
| `--limit N` | no | Maximum results (default `10`). Bounds the scoped result set when `--repo` is supplied. |
| `--repo <SELECTOR>` | no | Restrict results to drift records whose target belongs to one repository. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable record ID for the `SemanticDrift` node. |
| `schema_version` | number | yes | Record schema version for the returned `SemanticDrift`; see [`docs/schema/schema-versioning.md`](../schema/schema-versioning.md). |
| `before_commit` | string | yes | Commit SHA for the earlier embedding. |
| `after_commit` | string | yes | Commit SHA for the later embedding. |
| `before_valid_time` | string | yes | Valid time for the earlier embedding. |
| `after_valid_time` | string | yes | Valid time for the later embedding. |
| `prior_record_id` | string | yes | Prior codegraph File/Symbol record ID. |
| `target_record_id` | string | yes | Later codegraph File/Symbol record ID. |
| `metric_kind` | string | yes | Drift metric, e.g. `cosine_distance`. |
| `score` | number | yes | Cosine distance as a JSON number. Higher = more drift. |
| `selection_threshold` | number | yes | Threshold that selected this drift record. |
| `selection_basis` | string | yes | Selection policy, e.g. `threshold_only`. |
| `embedding_model_provider` | string | yes | Provider or boundary that supplied the model. |
| `embedding_model_name` | string | yes | Embedding model name. |
| `embedding_model_version` | string | yes | Pinned model version. |
| `embedding_model_dim` | number | yes | Embedding dimension. |
| `embedding_model_content_hash` | string | yes | Model content hash or `unknown`. |
| `repo_relative_path` | string or null | when resolvable | Path of the drift target, resolved from `DRIFTS_FROM` edges. |
| `name` | string or null | when resolvable | Name of the drift target. |
| `repository_id` | string | when attributable | Stable `Repository` record ID of the drift target's repository. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

Ties in `score` are broken by `record_id` ascending. Drift records from
different repositories are never merged: each row carries its own repository
identity.

---

## eg query semantic

Find code nodes by natural-language similarity using dense vector embeddings.

```text
eg query semantic <QUERY> --data-dir <DIR> [--limit N] [--format json|text]
```

The query string is embedded with the same model used during ingest and compared against stored vectors using cosine similarity. Results are returned in descending similarity order.

The embedded store **must** have been populated with `eg ingest --embed`. A store created without `--embed` contains no embedding vectors and returns no results.

### Vector-space compatibility gate (issue #104)

**BREAKING CHANGE (issue #104):** every `--embed` store created before this
change carries no recorded embedding-model identity, so semantic queries against
it now refuse with `embedding_identity_unrecorded` (exit `7`) instead of
returning results whose vector-space compatibility was never verified. Re-ingest
with `--embed` to record the identity. See
[`semantic-index-identity.md`](semantic-index-identity.md).

Before the query is embedded, `eg query semantic` verifies that the query
embedder's model identity matches the identity recorded for the store's vector
index. Two different models can share a dimension, so a dimension check alone
would let a model swap, cache change, or version bump produce a cosine ranking
computed **across incompatible vector spaces** and return it as a confident
answer.

When the identities match, results are returned **exactly as before**: the check
adds no change to ranking, scores, ordering, or the output schema. When they do
not, the query is refused with a stable machine-readable envelope on stdout and a
distinct nonzero exit code — never a ranked result list:

| Exit | `code` | Condition |
|------|--------|-----------|
| `7` | `embedding_identity_unrecorded` | Index exists but records no model identity (legacy store); compatibility is unverifiable. |
| `8` | `embedding_identity_ambiguous` | Index records more than one distinct producing model. |
| `9` | `embedding_dimension_mismatch` | Index and query embedder produce different dimensionalities. |
| `10` | `embedding_model_mismatch` | Same dimension, different model. |

`eg query semantic-memory` and `eg query semantic-context` read the same shared
vector index and apply the same gate. The daemon path (`--daemon`) does not —
daemon/MCP exposure is owned by #59/#53.

Read the indexed identity with `eg inspect --data-dir <DIR>`. The full contract,
envelope shape, and the re-ingest workflow are documented in
[`docs/cli/semantic-index-identity.md`](semantic-index-identity.md).

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<QUERY>` | yes | Natural-language search text, symbol name, or code snippet. |
| `--data-dir <DIR>` | yes | Embedded `AletheiaDB` store created by `eg ingest --adapter embedded --embed`. `--graph` is not accepted by this subcommand. |
| `--limit N` | no | Maximum number of results (default `10`). Bounds the scoped result set when `--repo` is supplied. |
| `--repo <SELECTOR>` | no | Restrict retrieval leads to one repository. |
| `--format` | no | `json` (default) or `text`. |

### JSON output fields

One JSON object per line (JSONL). The default output format is `json`. Field names are stable across releases.

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable BLAKE3-based record ID (`codegraph:v1:…`). Safe to cite, log, and pass to other `eg` commands. |
| `score` | number | yes | Cosine similarity score (0.0–1.0). Higher = more similar to the query. |
| `name` | string | when available | Symbol or file name from the matched record. Absent when the record has no name field. |
| `repo_relative_path` | string | when available | Repository-relative file path, e.g. `"src/lib.rs"`. Absent when the record has no path field. |
| `span` | object | when available | Source span: `start_byte`, `end_byte`, `start_line`, `end_line` (all integers). Absent when the record has no span. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the row. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

Machine consumers must depend only on the fields listed above. Additional fields may be added in future releases; removing or renaming any of the fields above constitutes a breaking contract change and requires a version bump.

### No-result and missing-embedding behavior

| Condition | Exit code | Stderr message | Operator action |
|-----------|-----------|----------------|-----------------|
| Store directory does not exist | `1` | `embedded store not found … run ingest` | Create the store: `eg ingest --adapter embedded --data-dir <DIR> [--embed]`. |
| Store has no vector index at all (never `--embed`ed) | `2` | `no results — store may not have embeddings (re-run ingest with --embed)` | Re-run ingest: `eg ingest --adapter embedded --data-dir <DIR> --embed`. Reported by the issue #104 gate before the model is loaded. |
| `semantic_search` returns no matches | `2` | `no results — store may not have embeddings (re-run ingest with --embed)` | Re-run ingest: `eg ingest --adapter embedded --data-dir <DIR> --embed`. |
| Query embedder does not match the indexed model | `7` / `8` / `9` / `10` | `semantic query refused: …` plus a machine-readable envelope on stdout | See [`docs/cli/semantic-index-identity.md`](semantic-index-identity.md); the remedy is always re-ingest, never editing the store. |

Exit code `2` is also returned when the query produced no cosine-similar results above the search threshold. This is semantically equivalent to "no match" in other query subcommands.

### `--format text`

`--format text` emits one human-readable line per result for terminal use, for example:

```text
scan_repository score=0.9500 @ src/lib.rs:51
```

The exact format of `--format text` output is **not stable** and must not be parsed by scripts or agents. Use `--format json` for machine-readable output with stable field names.

### Example

```sh
eg ingest graph.jsonl --adapter embedded --data-dir .egregore-semantic --embed
eg query semantic "write nodes to database storage" --data-dir .egregore-semantic
```

```json
{"record_id":"codegraph:v1:abc123","name":"EmbeddedAletheiaSink::write_record","repo_relative_path":"src/sink/embedded.rs","score":0.9231,"span":{"start_byte":4096,"end_byte":5200,"start_line":142,"end_line":168}}
```

The `record_id` is stable across re-scans of the same commit and can be cited in agent-memory records. The `repo_relative_path` and `span` together give a file and line-range handle that agents can pass directly to editor tools or other `eg` commands.

---

## eg query at

Resolve a `file:line` location — a compiler diagnostic (`src/daemon.rs:412`),
a panic backtrace frame, a PR diff hunk, or `git blame -L` output — to the
**smallest enclosing `Symbol` node** whose recorded span contains that line
(issue #151). Purely a span-containment lookup over already-stored
`SourceSpan` data: no daemon, no embeddings, no `--embed` store required.
Strictly read-only: a `--data-dir` embedded store is read through a throwaway
temporary copy (opening the live engine re-persists its index files), so the
original store stays byte-for-byte untouched.

```text
eg query at <PATH>:<LINE> --graph <PATH>   [--at <COMMIT>] [--repo <SELECTOR>]
eg query at <PATH>:<LINE> --data-dir <DIR> [--at <COMMIT>] [--repo <SELECTOR>]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATH>:<LINE>` | yes | Repo-relative path and 1-based line, e.g. `src/lib.rs:42`. The line is taken after the **last** `:`. |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store. Structural records only — the store does **not** need `--embed`. |
| `--at <COMMIT>` | no | Temporal pin: resolve the location against symbol spans **as they existed at this commit** (full SHA or unique prefix). Requires a history-bearing store. |
| `--repo <SELECTOR>` | no | Restrict resolution to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--format` | no | Accepted for CLI-surface consistency; the envelope is JSON. |

### Resolution semantics

- **Innermost wins.** When nested symbols contain the line (a method inside an
  `impl` inside a module), the primary `symbol` is the one with the narrowest
  containing span (line width, then byte width, then record ID — all
  ascending). The full enclosing chain of containing `Module` and `Symbol`
  nodes is also reported, ordered **outermost → innermost**; the last chain
  entry is always the primary symbol.
- **Absence is the answer.** A line outside every symbol span (blank line,
  file-level `use`/attribute, inter-item whitespace — even inside a module)
  yields the typed `no_enclosing_symbol` error, never a nearest-neighbor
  guess.
- **Current state by default.** Without `--at`, tombstoned records are
  excluded, and a history graph is anchored to its stamped HEAD snapshot
  (issue #82): a path deleted or renamed at HEAD is a `no_match`, never a
  stale pre-deletion symbol. Legacy stores without a stamped snapshot fall
  back to each stable ID's newest recorded version (by valid time,
  independent of record emission order). With `--at`, only records observed
  at that commit participate, so the same line can resolve to different
  symbols (or to none) at different commits.
- **Repository boundary stays explicit.** An unscoped location whose path
  exists in more than one repository fails closed with the
  `ambiguous_repository` stderr diagnostic (exit `1`); re-run with `--repo`.

### JSON output fields

A single deterministic JSON envelope on stdout, byte-identical across runs
for identical inputs and store state:

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `ok` | boolean | yes | `true` on success. |
| `path` | string | yes | Queried repo-relative path, echoed back. |
| `line` | number | yes | Queried 1-based line, echoed back. |
| `symbol` | object | yes | The smallest enclosing `Symbol` node. |
| `enclosing_chain` | array | yes | Containing `Module`/`Symbol` nodes, outermost → innermost. The innermost entry is the primary `symbol`. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the answer. |
| `repository` | string | when attributable | Human-usable repository identity handle. |

`symbol` and each `enclosing_chain` entry carry: `record_id`,
`kind` (`"Symbol"` or `"Module"` for chain entries), `schema_version`, `name`,
`symbol_kind` (e.g. `function` / `method` / `struct` / `impl`; absent on
modules), `repo_relative_path`, `span` (`start_byte`, `end_byte`,
`start_line`, `end_line`), and — when recorded — `language`, `visibility`,
`signature`, `git_commit` (history-backed records), and `valid_time`.

### Exit codes and error envelopes

Errors are one-line `{"ok":false,"error":{...}}` envelopes on stdout:

| Code | Error `code` | Meaning |
|------|--------------|---------|
| `0` | — | An enclosing symbol was found and printed. |
| `1` | `malformed_location` | Input is not `<path>:<line>` with a positive 1-based line. |
| `1` | `ambiguous_commit_prefix` | `--at` prefix matches more than one commit; `candidates` lists them. |
| `1` | `ambiguous_repository` / `unknown_repository_selector` / `ambiguous_repository_selector` | Repository selection failed (stderr diagnostic, as elsewhere). |
| `2` | `no_match` | No record carries the path in the selected view (unknown file, or file absent at `--at <COMMIT>`). |
| `2` | `missing_commit` | `--at` commit is absent from the store's history. |
| `2` | `no_enclosing_symbol` | The path is known but no symbol span contains the line. Carries `file_record_id` when the `File` node resolved. |

### Example

```sh
eg scan . --out g.jsonl
eg query at src/lib.rs:412 --graph g.jsonl
```

```json
{
  "ok": true,
  "path": "src/lib.rs",
  "line": 412,
  "symbol": {
    "record_id": "codegraph:v4:abc...",
    "kind": "Symbol",
    "schema_version": 4,
    "name": "outer::Gadget::method_one",
    "symbol_kind": "method",
    "repo_relative_path": "src/lib.rs",
    "span": {"start_byte": 4096, "end_byte": 5200, "start_line": 409, "end_line": 431},
    "language": "rust",
    "visibility": "public",
    "signature": "pub fn method_one(&self) -> usize"
  },
  "enclosing_chain": [
    {"record_id": "codegraph:v4:m01...", "kind": "Module", "schema_version": 4, "name": "outer", "repo_relative_path": "src/lib.rs", "span": {"start_byte": 100, "end_byte": 6000, "start_line": 3, "end_line": 480}, "language": "rust"},
    {"record_id": "codegraph:v4:i01...", "kind": "Symbol", "schema_version": 4, "name": "outer::impl Gadget", "symbol_kind": "impl", "repo_relative_path": "src/lib.rs", "span": {"start_byte": 3900, "end_byte": 5300, "start_line": 405, "end_line": 433}, "language": "rust"},
    {"record_id": "codegraph:v4:abc...", "kind": "Symbol", "schema_version": 4, "name": "outer::Gadget::method_one", "symbol_kind": "method", "repo_relative_path": "src/lib.rs", "span": {"start_byte": 4096, "end_byte": 5200, "start_line": 409, "end_line": 431}, "language": "rust"}
  ]
}
```

The returned `record_id` is a stable handle: feed it directly to
`eg query change-impact`, `eg query failures`, or `eg query context` to pivot
from a raw location into callers, prior failures, and evidence without ever
scanning the file's full symbol list.

---

## eg query locate

Positional entry into the `eg query context` contract (issue #212). Where
`eg query at` returns only the enclosing symbol handle, `eg query locate`
resolves the innermost symbol at a `file:line` **and** returns that symbol's
same trust-separated cross-domain bundle as `eg query context` — source facts,
agent observations, project state, artifacts, and verification evidence — in one
call. An agent holding a stack-trace frame, `git blame` line, diff hunk, or
compiler diagnostic gets the evidence graph without first guessing a symbol
name to enter it.

The span-containment resolution is identical to `eg query at` (it reuses the
same resolver): innermost-of-nested selection, the outermost → innermost
enclosing chain, current-state-by-default view with HEAD-snapshot anchoring,
and the no-guess discipline for lines outside every span. Strictly read-only:
a `--data-dir` embedded store is read through a throwaway temporary copy, so the
original store stays byte-for-byte untouched. Rust extraction only.

```text
eg query locate <PATH>:<LINE> --graph <PATH>   [--at <COMMIT> | --as-of <INSTANT>] [--repo <SELECTOR>] [--supersession <MODE>] [--format json|text]
eg query locate <PATH>:<LINE> --data-dir <DIR> [--at <COMMIT> | --as-of <INSTANT>] [--repo <SELECTOR>] [--supersession <MODE>] [--format json|text]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<PATH>:<LINE>` | yes | Repo-relative path and 1-based line, e.g. `src/lib.rs:42`. The line is taken after the **last** `:`. |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store. Structural records only — no `--embed` required. |
| `--at <COMMIT>` | no | Temporal pin: resolve the position against symbol spans **as they existed at this commit** (full SHA or unique prefix). Requires a history-bearing store. Mutually exclusive with `--as-of`. |
| `--as-of <INSTANT>` | no | Temporal pin: resolve against the most recent commit **at or before** this RFC 3339 instant. Mutually exclusive with `--at`. |
| `--repo <SELECTOR>` | no | Restrict resolution to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--supersession <MODE>` | no | How superseded/contradicted observations are handled in the bundle: `exclude` (default) or `include-but-flag`, matching `eg query context`. |
| `--format` | no | `json` (default) or `text`. |

### The context bundle

On a successful locate, the envelope carries the located `symbol` and
`enclosing_chain` (identical shape and fields to `eg query at`) **plus** the
five trust-separated sections of `eg query context`, anchored on the located
record ID so only the located symbol (not every same-named symbol) drives the
answer:

| Section | Trust class | Contents |
|---------|-------------|----------|
| `source_facts` | deterministic | The located `Symbol`/`File` code-graph nodes. |
| `topology_edges` | deterministic | Structural edges (`DEFINES`, etc.) among the source facts. |
| `observations` | agent-authored | `Observation`/`Decision`/`Failure` nodes citing the symbol. |
| `project_state` | project | Linked `Task`/`AcceptanceCriterion`/issue/PR nodes. |
| `artifacts` | artifact | Linked `Artifact`/`PatchArtifact`/`FileEdit` nodes. |
| `verification_evidence` | verification | Linked `Verification`/`TestRun`/`CommandRun` evidence. |
| `unresolved` | diagnostic | Evidence-link targets absent from this store slice. |

The bundle is **not** temporally filtered under a commit pin: the pin selects
*which* symbol is located, and the bundle is everything known about that symbol
identity — consistent with `eg query context`, which has no temporal selector.

### Resolution semantics

Same as [`eg query at`](#eg-query-at): innermost wins, the enclosing chain is
reported outermost → innermost, current state is the default view, and an
unscoped cross-repository path collision fails closed. Two absence answers are
distinguished:

- `no_enclosing_symbol` — the line sits in a gap inside the file (blank line,
  file-level `use`, comment, inter-item whitespace).
- `line_out_of_range` — the line is **beyond the file's last recorded
  structural span**. `File` nodes carry no span, so the file's true line count
  is not stored; the recorded extent (`max_known_line`) is the deterministic
  upper bound, and a line past it is reported as out of range rather than
  guessed. Both are typed answers carrying the resolved `file_record_id`; a
  nearest-neighbor symbol is never returned.

### Exit codes and error envelopes

Errors are one-line `{"ok":false,"error":{...}}` envelopes on stdout (repository
**selector** rejections print to stderr, as elsewhere):

| Code | Error `code` | Meaning |
|------|--------------|---------|
| `0` | — | An enclosing symbol was found; the symbol, chain, and context bundle were printed. |
| `1` | `malformed_location` | Input is not `<path>:<line>` with a positive 1-based line. |
| `1` | `malformed_timestamp` | `--as-of` is not a valid RFC 3339 instant. |
| `1` | `ambiguous_commit_prefix` | `--at` prefix matches more than one commit; `candidates` lists them. |
| `1` | `ambiguous_repository` | Unscoped path exists in more than one repository; re-run with `--repo`. |
| `1` | `unknown_repository_selector` / `ambiguous_repository_selector` | `--repo` selector failed (stderr diagnostic). |
| `2` | `no_match` | No record carries the path in the selected view (unknown file, or file absent at the pinned point). |
| `2` | `missing_commit` | `--at` commit is absent from the store's history. |
| `2` | `no_commit_at_or_before` | No commit exists at or before the `--as-of` instant. |
| `2` | `no_enclosing_symbol` | The path is known but no symbol span contains the line. Carries `file_record_id`. |
| `2` | `line_out_of_range` | The line is beyond the file's last recorded structural span. Carries `max_known_line` and `file_record_id`. |

### Example

```sh
eg scan . --out g.jsonl
eg query locate src/lib.rs:412 --graph g.jsonl
```

```json
{
  "ok": true,
  "path": "src/lib.rs",
  "line": 412,
  "symbol": {
    "record_id": "codegraph:v4:abc...",
    "kind": "Symbol",
    "name": "outer::Gadget::method_one",
    "symbol_kind": "method",
    "repo_relative_path": "src/lib.rs",
    "span": {"start_byte": 4096, "end_byte": 5200, "start_line": 409, "end_line": 431}
  },
  "enclosing_chain": [ /* module → impl → method, outermost → innermost */ ],
  "source_facts": [ {"record_id": "codegraph:v4:abc...", "kind": "Symbol", "name": "outer::Gadget::method_one", "repo_relative_path": "src/lib.rs"} ],
  "observations": [ {"record_id": "agent_memory:v1:...", "text_summary": "method_one panics on empty input"} ],
  "project_state": [],
  "artifacts": [],
  "verification_evidence": [],
  "unresolved": []
}
```

`locate` is the one-call form of `eg query at` followed by
`eg query context <name>` — but it never needs the intermediate name guess, and
it anchors on the exact located record, so same-name symbols never bleed in.

> **Note.** An MCP-tool wrapper for `locate` is tracked separately (the issue
> #181/#182 tool-surface cluster) and can follow once the CLI contract is
> stable, mirroring how `eg query at` deferred its MCP exposure.

---

## eg query implementors

List the recorded implementors of a locally-defined trait by walking the
inbound `IMPLEMENTS` edges of the resolved trait symbol (issue #133).

```text
eg query implementors <TRAIT> --graph <PATH> [--at <COMMIT>] [--as-of <INSTANT>] [--repo <SELECTOR>] [--format json|text]
```

Strictly read-only and deterministic: no records, indexes, or runtime files
are created, modified, or deleted, and an identical query against an
unchanged source produces byte-identical rows, ordering, and handles across
runs (rows are sorted by `trait_record_id`, then impl `record_id`, then
`edge_record_id`). Because opening the embedded engine re-persists its index
files, the `--data-dir` path reads a throwaway copy of the store; the live
store stays byte-for-byte untouched. Both handle forms — trait name and
canonical record ID — work with and without the temporal selectors.
Implementing-type resolution never crosses the repository boundary: a
same-named type in another repository is never cited, and the row falls back
to `parsed_only` instead.

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `<TRAIT>` | yes | Exact trait symbol name (qualified when module-nested, e.g. `mymod::Renderable`) or the trait's canonical Symbol record ID. |
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan` or `eg scan-history`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. |
| `--at <COMMIT>` | no | Return the implementor set at this commit SHA or unique prefix (valid-time axis keyed by commit). Exit `1` on an ambiguous prefix. Requires a history graph. Mutually exclusive with `--as-of`. |
| `--as-of <INSTANT>` | no | Return the implementor set at the most recent record at or before this RFC 3339 instant. Mutually exclusive with `--at`. Same temporal-selector contract as `eg query symbol`. |
| `--repo <SELECTOR>` | no | Restrict resolution to one repository (see [Repository scope](#repository-scope---repo-issue-67)). |
| `--format` | no | `json` (default, newline-delimited) or `text`. |

### Completeness contract (`local_traits_only`)

The extractor records an `IMPLEMENTS` edge for a trait whose definition is
resolvable **anywhere in the scanned repository**. As of issue #344 this
includes **cross-file out-of-line trait impls**: the common module layout where
`src/lib.rs` defines `trait T` and a separate `mod m;` file (`src/m.rs`) holds
`impl crate::T for Foo` now edge-backs. A repo-wide index of every trait/type
definition is built after per-file extraction, and each impl the per-file pass
could not resolve locally is retried against it with the same
`crate::`/`self::`/`super::` and module-scope-walk semantics as same-file
resolution (an unqualified `impl Draw for Button` in `m.rs` walks outward to a
crate-root `Draw`; an absolute `impl crate::T for Foo` resolves from the crate
root). Local resolution still wins, so an edge is never emitted twice, and the
pass recomputes from cached per-file facts each scan, so editing either side
re-derives (or retires) the edge.

`impl Display for Foo` (an external/std trait) still produces **no** edge: an
unresolved or ambiguous trait path is left edge-free rather than diagnosed,
because a cross-crate trait is external by construction — this is exactly what
`local_traits_only` means. Generic trait impl headers **are** trait-edge-backed
as of issue #343 (a generic-binder impl `impl<T> Trait for Type<T>` and a
generic-trait instantiation `impl Trait<Args> for Type` both resolve their
trait), and this now applies cross-file too. Two forms stay deliberately
bounded out: an **inherent** generic impl (`impl<T> Type<T>`, no `for` clause)
keeps its self-referential record edge and is never a trait implementor, and a
**blanket** impl (`impl<T> Trait for T`, whose `for` target is a bare binder
type parameter) mints no edge at all — it covers every type and has no single
implementing-type record. A **negative** impl (`impl !Trait for Foo`, e.g.
`impl !Send for Foo`) also mints **no** edge: it asserts that the type
explicitly does *not* implement the trait, so it never edge-backs and never
surfaces `Foo` as an implementor of `Trait`. The remaining honest bounds:
**cross-crate** traits (std/deps), **non-Rust** languages, blanket impls, and a
`use`-alias of a trait in a **non-root** module that the scope walk cannot see
stay unresolved. A bare (unqualified) name whose **simple name is
ambiguous** across the same-file or repo impl-target definitions and that carries
**no** captured `use` import naming it (see import-aware resolution below, issue
#393) is left unresolved: it may be a `use`-alias of a non-root definition, so
rather than guess a root binding a missing edge is preferred over a wrong-target
one. This same-name ambiguity
bound applies on **both** resolution paths, sharing one counting predicate so
they cannot diverge: the **local/inline-module** per-file resolver (a bare name
that a same-file scope walk resolves only by reaching **outward** to a
same-named root/outer definition — e.g. `mod m { use crate::a::T; impl<U> T for
Foo<U> {} }` beside a root `trait T` — is left unresolved rather than mis-bound
to the root), and the repo-wide **cross-file** pass (an out-of-line impl whose
bare trait/type name is ambiguous across the repo index). A bare name that
resolves in the impl's **own** module is trusted verbatim and never suppressed.
The ambiguity bound covers **all impl-target kinds**
(traits **and** type-defining targets: `struct` / `enum` / `type_alias`), not
just traits, because a non-generic inherent impl (`impl Foo {}`) carries the
**type** name `Foo` as its bare reference and the scope walk resolves bare names
against every one of those kinds. So a bare name shared across crate/module
scopes by any impl-target kind — a root `T` and a non-root `a::T` (traits), or a
root `struct Foo` and a `use`-aliased `a::Foo` behind `impl Foo {}` (types) — is
left unresolved rather than mis-targeted. The accepted trade-off: when a trait
`T` and an unrelated type `T` coexist across files, a bare `impl T for X` that
once resolved is now left unresolved — a rare potential wrong-edge converted into
a rare missed-edge, the honest-bound direction consistent with
`local_traits_only`.

**Import-aware bare-name resolution (issue #393).** The same-name ambiguity
rule above only sees definitions the impl-target index *already knows*. An
external or std trait brought in by a `use` — `use std::fmt::Display;` — is not
in that index, so an ambiguity count of one would let the scope walk mis-bind a
bare `impl Display for Foo` to a same-named **local** `trait Display`. Rather than
leave every such bare name unresolved, the extractor captures the resolved import
**path** each **module-item** `use` binds (a direct `use` item of the impl's own
module, *not* one nested inside a function body or block), and the cross-file
resolver binds a bare (unqualified) trait/type name to the target that import
**names** — `use crate::a::T; impl T for Foo` edge-backs `Foo` to `a::T` (not a
root `T`), `use crate::a::T as U; impl U for Bar` binds through the rename, and a
grouped `use crate::a::{T};` distributes the group prefix. Import-aware in-repo
resolution decides whether an import can name a local target by its **first
`::`-separated segment**, not by requiring an explicit root prefix (Codex
"crate-root local imports" on PR #399). An import path is resolved in-repo when it
is either **in-repo-rooted** — first segment `crate`, `self`, `super`, or `Self` —
**or** its first segment is **not** a known **extern-prelude crate name**. The
extern-prelude exclusion set is closed: `std`, `core`, `alloc`, `proc_macro`,
`test`. This splits bare first-segment imports two ways per Rust 2018+ path
resolution:

- `use std::fmt::Display;` / `use core::fmt::Debug;` — first segment is an
  extern-prelude crate, so the path names an **external crate** and resolves to
  **no** in-repo definition, minting **no** edge even when the repo
  *coincidentally* defines a same-path local module (e.g. `mod std { mod fmt {
  trait Display {} } }`). Without this exclusion the captured extern path would run
  through the in-repo scope walk and steal the coincident local trait — a
  wrong-target edge (the round-1 behaviour, preserved).
- `use a::T;` — first segment `a` is **not** an extern-prelude crate, so it names a
  **local crate-root module** (valid Rust 2018) and edge-backs `impl T for Foo` to
  the local `a::T`, exactly as `use crate::a::T;` does. Recovering these bare
  crate-root-local imports fixes the round-1 recall regression that dropped every
  bare (non-`crate::`-prefixed) root-local import.

Resolution still requires an actual in-repo target to exist, so an ordinary
external dependency (`use serde::Serialize;`) mints **no** edge simply because no
local `serde::Serialize` matches — **no dependency list is consulted**. Resolvable
paths are matched against the crate-root-partitioned index: an in-repo-rooted path
is normalized (`crate::`/`self::`/`super::` stripping, exactly as a qualified impl
trait path), and a bare crate-root-local path (`a::T`, no prefix to strip) is
looked up as a crate-root-relative qualified name in the impl's own crate root — so
the recall recovery never reintroduces a wrong-target edge. **Accepted rare
bound:** a local module whose name collides with a declared dependency crate (e.g.
a hand-rolled `mod core`) is ambiguous/invalid Rust and is treated as external —
out of scope. Import capture respects Rust's non-inherited
`use` visibility exactly (own module scope only; an ancestor/root, sibling-module,
or **block-local** `use` is never captured for the impl's scope), so a bare name
recovered here can only bind what a co-located `use` truly imports. A bare name
resolvable in the impl's **own** module still wins at depth 0 before any import
handling. Three bare-name cases stay bounded out. A **glob** import (`use a::*;`)
binds no specific segment, so the bare `impl T for Foo` beside it is genuinely
un-imported and — when its simple name is ambiguous across the crate root's
impl-target definitions — is left **unresolved** (the honest missing-edge
direction).

The import lookup is **tri-state** (Codex round-5 finding P2, PR #399), and the
three states are what keep a shadowed name from leaking into the scope walk:

- **Resolved** — exactly one `use` import binds the name to a resolvable in-repo
  path (in-repo-rooted, or a bare non-extern-prelude first segment). The path is
  matched against the crate-root-partitioned index; a match mints the edge, an
  index miss mints **no** edge — and in neither case does it fall through to the
  scope walk.
- **Veto** — a `use` import binds the name but it resolves **externally** (a single
  extern-prelude path such as `use std::fmt::Display;`) or **ambiguously** (2+
  distinct cfg-gated paths). The import *shadows* the bare name, so the resolver
  mints **no** edge **and suppresses the scope-walk fall-through**. This is the fix
  for the round-4 regression where a vetoed binding silently fell through and let
  the scope walk mis-bind a coincidental **root-local same-name trait** (e.g. a
  root `trait Display` reached at depth 0) — a wrong-target edge in the
  configuration where the name is actually the external `std::fmt::Display`. A
  single external import shadows the local name exactly as an ambiguous one does.
- **NoImport** — no `use` import binds the name at all; the resolver falls through
  to the scope walk and the conservative same-name ambiguity bound above (normal
  #389 recall for same/parent-module traits). Only an actual `use`-binding of the
  name triggers a Veto — a bare `impl T for X` with **no** `use` importing `T`
  still resolves outward via the scope walk.

A **cfg-gated same-name collision** — the same simple name bound to two *distinct*
paths in one module scope, `#[cfg(feature = "std")] use std::fmt::Display;`
alongside `#[cfg(not(feature = "std"))] use crate::local::Display;` (or
`use crate::Display;` for a root-local trait) — is the ambiguous Veto case: the
extractor keeps *both* import facts (it does not last-wins collapse them), and
because one name resolves to two distinct paths the resolver mints **no** edge and
does **not** scope-walk (Codex round-4 finding E and round-5 finding P2, PR #399).
Collapsing the two to the in-repo path would have minted a local edge even in the
configuration where the name is the external std trait; vetoing both is the
conservative shadow-veto outcome. One name resolving to one distinct path (the same
path repeated is not ambiguity) still resolves. The
incremental-cache schema version is **13** for the serde-default
`use_trait_imports` per-file facts this capture records (alongside the crate-root
partitioning below); the finding-E fix keeps the same fact *shape* (it changes only
how many facts a colliding name emits), and the round-5 tri-state veto is a pure
in-memory resolution change that records **no** new fact, so the schema version
stays **13** — no further bump is required and older caches rebuild.

**Multi-crate-root packages are partitioned by crate root (issue #394).**
The repo-wide index keys on `(crate_root, crate-root-relative qualified name)`, so
when a package has more than one crate root a root `trait T` in each is a
**distinct** index entry. Auxiliary buildable targets each form their own crate
root — `src/bin/<name>.rs` / `src/bin/<name>/**` (`bin:<name>`), `examples/**`
(`example:<name>`), `tests/**` (`test:<name>`), `benches/**` (`bench:<name>`), and
`build.rs` — while `src/lib.rs`, `src/main.rs`, and their module files share the
primary crate root. A pending impl's candidates are restricted to its **own**
crate root, so a library `impl crate::T for Foo` now correctly edge-backs to the
**library** `T` even when a binary crate root also defines a root `T`, and the
binary's `impl crate::T for Bar` binds the **binary** `T` — the two never
cross-pool. One documented residual bound: a package carrying **both** `src/lib.rs`
and `src/main.rs`, each defining a same-named **root** trait, still pools those two
in the shared primary root (a single file's path cannot distinguish the library
crate from the default binary crate); the common auxiliary-target case is fully
partitioned.

**Test / example / bench HELPER modules are assigned to the entry crate that
`mod`-includes them.** Path classification alone would stamp a shared helper like
`tests/common/mod.rs` into its own synthetic crate root `test:common`, even though
it actually compiles as a module of the entry crate `tests/it.rs` (`test:it`) that
declares `mod common;`. Because a pending impl's candidates are restricted to its
own crate root, an `impl crate::T for Foo` in that helper would otherwise miss a
trait `T` defined in the entry file. The cross-file pass therefore consults the
`mod` inclusion graph: each entry crate root (`tests/<name>.rs`,
`examples/<name>.rs`, `benches/<name>.rs`, and their `<name>/main.rs` directory
forms) is walked down its transitive plain `mod <name>;` declarations, and a
helper reachable from **exactly one** entry crate is reassigned to that entry's
crate root — so the helper's impls resolve against the entry's traits.
Conservatively, a helper reachable from **two or more** distinct entry crates (a
genuinely shared `mod common;` included by several test binaries), or a file cargo
also compiles as its **own** aux target (`tests/common.rs`), keeps its path-based
crate root and stays **unresolved** — a missing edge, never a wrong one. Only
test/example/bench helper files are subject to this reassignment; `lib`/`bin`/
`build` assignment is unchanged. Residual bound: only the crate-root partition key
is remapped (not a helper symbol's qualified name), so a deeply nested helper
defining a root-level symbol that collides by simple name with the entry crate's
own root symbol can become same-name-ambiguous and stay conservatively
unresolved.

The query surfaces these bounds instead of hiding them:

- Every implementor row and every zero-implementors signal carries
  `completeness: "local_traits_only"`.
- A trait that resolves to a live record but has zero recorded `IMPLEMENTS`
  edges emits an explicit `zero_implementors_recorded` signal row (exit `0`)
  with a `note` explaining the bound — never a bare empty answer presented as
  authoritative.
- A name that resolves to **no** live record (typical for external/std
  traits, which have no local Symbol record at all) is a `no_match` (exit
  `2`), with the same explanation in `error.message`.

Absence of a row is therefore never proof that no implementation exists.

### Ambiguity rule

A trait name that resolves to multiple distinct live symbols (same name in
different modules/files) returns the implementors of **every** candidate,
each row labeled by its `trait_record_id` and `trait_name`; a candidate is
never picked implicitly. Ordering is deterministic (candidates by record ID).
This holds across repositories too: the unscoped current view is a list
query per the [repository-scope contract](#when-repository-scope-is-required)
— colliding same-named traits from different repositories are returned side
by side, each row carrying its own `repository_id`/`repository`, never
merged; use `--repo` to narrow. Only the single-answer temporal views fail
closed on an unscoped multi-repository collision.
Pass the qualified name or the trait's record ID to narrow to one candidate.
Name resolution considers only symbols whose kind can be an `IMPLEMENTS`
target (trait, class, interface, struct/enum/union, type aliases; legacy
records without a recorded kind are included): a `fn` sharing a trait's name
lives in a different namespace and never adds a spurious zero signal. A name
resolving only to non-target kinds is a `no_match` whose `error` names the
kinds (`non_target_symbol_kinds`); an explicit record-ID handle always
answers for exactly that record.
The rule is uniform across time views: `--at` and `--as-of` also return
every same-named candidate at the pinned point (per candidate, the newest
record at or before the instant), labeled the same way. An unscoped
multi-repository collision fails closed with the `ambiguous_repository`
diagnostic.

Scan-history stores replay full per-commit snapshots and emit no tombstone
when a symbol or relationship disappears (removal is visible as absence from
later snapshots). Consistent with the sibling verbs (`eg query symbol` lists
every snapshot version; `transitive-callers` walks deduplicated history
edges), an **unpinned** query over such a store answers with the union
across recorded history — every row labeled with its provenance
`git_commit`, never silently frontier-trimmed. Pin `--at <tip>` for the
frontier/current answer; current-tree stores (`eg scan` / incremental)
carry real tombstones and are frontier-accurate unpinned. `--as-of` is
timestamp-based at parity with `eg query symbol --as-of`: a trait that
stopped appearing without a tombstone resolves at its newest snapshot at or
before the instant, and the implementor rows are pinned to — and labeled
with — that snapshot's commit. When two commits share one committer
timestamp, the tie resolves from stored facts, independent of store read
order: the tied commit that no other tied candidate records as a parent
wins (recorded parent links recover chains); between unrelated equal-time
commits — where true topological order is not recoverable from stored
facts — the largest commit SHA wins as a documented deterministic
fallback.

Over an embedded store (`--data-dir`), a temporal selector reads the
history-inclusive store view (through the same throwaway read-only copy).
The embedded store keeps every per-commit node snapshot but only the latest
physical version of each edge, so a pinned view additionally accepts an
`IMPLEMENTS` edge whose source is an `impl`-block symbol with a snapshot at
the pinned commit — sound because the impl block's identity encodes the
implemented trait. Type-source edges (Python/TS classes, Go embedding) get
no such fallback: heritage can change without the class identity changing,
and a fabricated pinned relationship would be a guess.

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | Implementor rows returned, or the trait resolved with zero recorded implementors (explicit `zero_implementors_recorded` signal row). |
| `1` | Invalid selector: malformed `--as-of` timestamp, ambiguous `--at` commit prefix, unknown/ambiguous repository selector, or ambiguous unscoped repository collision. Message on stderr. |
| `2` | The trait could not be resolved. Stdout carries `{"ok":false,"error":{"code":"no_match",...}}`, or `{"ok":false,"error":{"code":"stale_handle",...}}` when the handle resolves only to tombstoned record(s), or `{"ok":false,"error":{"code":"no_commit_anchor",...}}` when a temporal selector resolves a snapshot without a commit anchor (current-tree/refresh records) — a pinned implementor set requires scan-history commit snapshots, and answering with the unpinned view would leak later changes into the past. |

### JSON output — implementor rows

One JSON object per line (JSONL), one line per recorded implementor.

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `record_id` | string | yes | Stable record ID of the `impl` Symbol — the citable handle for the impl block. |
| `schema_version` | number | yes | Record schema version of the impl Symbol. |
| `kind` | string | yes | Always `"Symbol"`. |
| `symbol_kind` | string | when recorded | Symbol kind of the impl record (typically `"impl"`; absent on legacy graphs without symbol-kind metadata). |
| `name` | string | yes | Impl display name, e.g. `"impl Renderable for Circle"`. |
| `implementing_type` | string | yes | Qualified name of the implementing type, resolved from the impl against the graph's Symbol records when possible. |
| `implementing_type_record_id` | string | when resolved | Stable record ID of the implementing type's Symbol record. |
| `implementing_type_resolution` | string | yes | `"resolved"` when the type maps to a live Symbol record, `"parsed_only"` when only the impl display name could be parsed (e.g. the type is defined outside the store). |
| `trait_record_id` | string | yes | Stable record ID of the resolved trait this row belongs to. |
| `trait_name` | string | yes | Resolved trait's (qualified) name. |
| `repo_relative_path` | string | when recorded | Repo-relative file of the impl block. |
| `span` | object or null | yes | Source span of the impl block (`start_byte`, `end_byte`, `start_line`, `end_line`). |
| `edge_record_id` | string | yes | Stable record ID of the connecting `IMPLEMENTS` edge. |
| `git_commit` | string | history graphs / pinned views | Commit SHA the edge (or impl record) was recorded at. |
| `repository_id` | string | when attributable | Stable `Repository` record ID owning the impl row. |
| `repository` | string | when attributable | Human-usable repository identity handle. |
| `completeness` | string | yes | Always `"local_traits_only"`. |

### JSON output — zero-implementors signal

Emitted (exit `0`) once per resolved trait candidate that has no recorded
`IMPLEMENTS` edge:

| Field | Type | Description |
|-------|------|-------------|
| `ok` | bool | `true` — the trait resolved; the answer is the explicit zero signal. |
| `code` | string | Always `"zero_implementors_recorded"`. |
| `trait_record_id` / `trait_name` | string | The resolved trait's handle. |
| `repo_relative_path` / `span` | string / object | The trait definition's citable handle. |
| `repository_id` / `repository` | string | Owning repository, when attributable. |
| `implementors_recorded` | number | Always `0`. |
| `completeness` | string | Always `"local_traits_only"`. |
| `note` | string | Human-readable statement of the locally-defined-traits-only bound. |

### Example

```sh
eg scan . --out g.jsonl
eg query implementors Renderable --graph g.jsonl
```

```json
{"record_id":"codegraph:v4:1f0a…","schema_version":4,"kind":"Symbol","symbol_kind":"impl","name":"impl Renderable for Circle","implementing_type":"Circle","implementing_type_record_id":"codegraph:v4:77aa…","implementing_type_resolution":"resolved","trait_record_id":"codegraph:v4:0be1…","trait_name":"Renderable","repo_relative_path":"src/shapes.rs","span":{"start_byte":120,"end_byte":420,"start_line":7,"end_line":15},"edge_record_id":"codegraph:v4:9c3d…","completeness":"local_traits_only"}
```

`--format text` prints one human-readable line per row (e.g.
`Circle (impl Renderable for Circle) implements Renderable @ src/shapes.rs:7 (completeness: local_traits_only)`);
the text format is not stable and must not be parsed.

Out of scope for this verb: emitting edges for external/std (cross-crate)
traits, blanket impls (`impl<T> Trait for T`), method-level breakage analysis,
and the outbound direction ("what does this type implement"). Same-file generic
trait impls are in scope as of issue #343; cross-file out-of-line trait impls
are in scope as of issue #344.

## eg query evidence-path

Trace **one deterministic cross-domain evidence witness path between two
records** (issue #247): given two exact record handles, return the shortest
connecting chain over the graph's evidence/provenance edge subgraph — or an
explicit `no_path` verdict. Answers *"is record A grounded in record B, and by
what chain?"* without hand-walking the graph.

```sh
eg query evidence-path <SOURCE_ID> <TARGET_ID> --graph graph.jsonl        # exit 0 on a witness path
eg query evidence-path <SOURCE_ID> <TARGET_ID> --data-dir .egregore       # embedded, read-only
eg query evidence-path <SOURCE_ID> <TARGET_ID> --graph graph.jsonl --format text
```

Only **evidence/provenance** edges are traversed (`OBSERVES`, `HAS_EVIDENCE`,
`VALIDATED_BY`, `PRODUCED_EVIDENCE`, `FRAME_RESOLVES_TO`, `EMITTED_DURING`,
`REFERENCES_TASK`, `CLOSES_ACCEPTANCE_CRITERION`, `OWNED_BY_TASK`, …); code-graph
topology (`CALLS`, `CONTAINS`, `DEFINES`, …) and intra-memory scaffolding
(`SESSION_OF`, `AUTHORED_BY`) are **excluded** by design. The classification is an
exhaustive compile-time partition of every edge label (the completeness
invariant), and both class lists ride in the envelope so a `no_path` verdict is
never presented as proof no grounding exists.

Reachability is **undirected** (a grounding chain mixes edge directions), so each
hop reports the edge's native `from`/`to` plus a `traversal_direction`
(`forward`/`reverse`). The path is the deterministic shortest path: fewest hops,
then the smallest `(neighbor_record_id, edge_record_id)` at each step. Deleted
(tombstoned, non-temporal) records — and edges touching them — are excluded (a
current-state view; no `--at`/`--as-of`). An `EMITTED_DURING` hop additionally
carries its `basis` (`content_hash_join` / `temporal_correlation`) and documented
`confidence` (`1.0` / `0.5`) — a correlation lead, never causation.

Exit codes: `0` on a witness path (≥ 1 hop); `1` on identical endpoints or a
`no_path` verdict between two live endpoints; `2` when an endpoint is absent
(`endpoint_not_found`) or tombstoned (`endpoint_tombstoned`, a distinct label).
The lane is repo-agnostic (endpoints are exact IDs; a chain may cross repos), so
there is no `--repo` flag. Read-only, redaction-safe (only IDs, domains, kinds,
edge labels, paths, spans, counts, and basis strings escape), and byte-identical
across runs. A witness path proves a live evidence-edge chain connects two
records; it is not proof the cited code still matches current source. See
`docs/cli/evidence-path.md`.
