# eg query verification-coverage

Partition the crate's **externally-reachable public API surface** into
**verification-covered** and **uncovered** symbols by joining the recorded
verification-domain nodes over the evidence-link edge/citation registry. A
citable, deterministic, read-only query lane. Local-first, no network, no
build, no coverage run.

> **Presence/absence of recorded evidence only — never a testedness verdict.**
> Absence of recorded verification evidence for a symbol is a **prioritization
> signal**, NEVER proof that the code is untested, unverified in reality,
> unsafe, or broken. Presence of a link is a **recorded citation**, NEVER proof
> of correctness or that a test, proof, benchmark, or CI check actually passed.

## What "covered" means

A public symbol `S` (in file `F`) is **covered** when some **verification-domain
node** `V` is connected to it by an **evidence relation**:

- **symbol-direct** (`link_level: "symbol"`) — `V` links to `S` itself by any
  evidence-link label (`MENTIONS_SYMBOL`, `FAILED_ON`, `VALIDATED_BY`,
  `HAS_EVIDENCE`, `PRODUCED_EVIDENCE`, ...);
- **file-level** (`link_level: "file"`) — `V` links to `S`'s containing file
  `F` via `TOUCHED_FILE` or `FAILED_ON`.

A verification-domain node is a `Verification`, `CommandRun`, `TestRun`,
`ProofResult`, `CIStatus`, `CommandEvidence`, `BenchmarkRun`, or
`CoverageReport` node, or any node whose `domain` override is `verification`.
Both edge directions and both representations (graph edges **and**
`EvidenceLink`s carried on a node's `evidence_links` / `supporting_evidence`)
are considered. An **agent-memory** node linking to `S` never confers
coverage — one endpoint of the relation must be a verification-domain node.

## Capability-degradation contract

On trunk **no writer links a verification-domain node to a code Symbol/File**
(the code-linking evidence edges are emitted only from agent-memory nodes, and
verification nodes are explicitly excluded). So by default this is a
**capability-absent** lane, exactly mirroring `eg query undocumented`'s
`doc_facts_unavailable` verdict:

- **no verification records** in the (repo-scoped) store → `capability:
  "verification_facts_unavailable"`, reason `no_verification_records`;
- **verification records present but none link to code** → `capability:
  "verification_facts_unavailable"`, reason `no_verification_code_links`.

When capability is absent, **both buckets are empty** and a single
`verification_facts_unavailable` diagnostic is emitted (exit 0). The lane
**never floods every symbol into "uncovered"** just because the linking writer
never ran. When at least one verification→code link is recorded, `capability`
is `verification_links_recorded` and the in-scope surface is partitioned.

## Synopsis

```text
eg query verification-coverage [SCOPE] --graph <PATH>    [--repo <SELECTOR>] [--at <SHA>] [--limit N] [--format json|text]
eg query verification-coverage [SCOPE] --data-dir <DIR>  [--repo <SELECTOR>] [--at <SHA>] [--limit N] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`, read strictly through a throwaway copy — the live store is never
mutated). Exactly one of `--graph`/`--data-dir` is required.

- `SCOPE` (optional, positional) filters to one code item, resolved in
  precedence order: **exact record ID**, then **exact symbol name**, then a
  **segment-aware repo-relative path prefix** (`src/alpha` matches
  `src/alpha/x.rs` but never `src/alphabet/y.rs`).
- `--repo <SELECTOR>` restricts the surface and links to one repository in a
  multi-repo store; an unknown/ambiguous selector is rejected (exit 1). A link
  counts only when the code item and — when the verification node carries
  repository attribution — the verification node both belong to the scoped
  repository.
- `--at <SHA>` pins the surface to a single-commit snapshot (full SHA or unique
  prefix; requires a history-bearing store).
- `--limit N` caps **each bucket independently** after sorting; a truncated
  bucket sets its `*_truncated` count flag and pushes a `results_truncated`
  diagnostic carrying the true pre-truncation count. Never drops a whole bucket
  to fit the other. Range `1..=1000`; out-of-range is rejected (exit 1).
- `--format json` (default) or `--format text`.

## Output

The JSON envelope carries `ok`, `language` (`Rust`), `repo_scope`, `scope`,
`capability`, a verbatim `disclaimer`, the `covered` and `uncovered` buckets,
`counts`, and `diagnostics`.

Each `covered` row carries the symbol's `record_id`, `kind`, `path`,
`repo_relative_path`, `span`, and a `verification` array of crediting links —
each with the verification node's `record_id`, `verification_kind`,
`edge_label`, and `link_level` (`symbol` / `file`), sorted by
`(record_id, edge_label, link_level)` and de-duplicated. Each `uncovered` row
carries `record_id`, `kind`, `path`, `repo_relative_path`, `span`,
`schema_version`, and (for history-backed records) `valid_time` / `git_commit`.

Both buckets are sorted by `(repo_relative_path, start_line, record_id)`.
`counts` reports `symbols_in_scope`, `covered`, `uncovered` (pre-truncation
totals), `verification_records_in_store`, `verification_code_links_in_store`,
and the two `*_truncated` flags.

Output is **allow-list only** — record IDs, kinds, labels, paths, spans, and
counts. Raw payload text (verification summaries, command output, log
excerpts) never leaves the store. The response is deterministic and
byte-identical across repeated runs on an unchanged store.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0`  | Report emitted (covered/uncovered partition, or an explicit `verification_facts_unavailable` capability verdict — an empty result is a success, not an error). |
| `1`  | Usage/load error: unknown/ambiguous `--repo`, out-of-range `--limit`, unknown/ambiguous `--at` commit, both/neither `--graph`/`--data-dir`, unreadable store/graph. |
| `2`  | A supplied `SCOPE` matched no in-store code item — `scope_not_found` for a path-shaped handle, `no_match` for a name/id-shaped one. |

## When to use this vs the alternatives

This lane answers **"which of my public symbols have any recorded verification
evidence linked to them in the knowledge graph, and which do not?"** — a
prioritization and doc-of-record question over already-captured facts. It is
**not** a coverage tool and does not run, build, or instrument anything.

- **`cargo llvm-cov` / `cargo tarpaulin`** measure *line/branch execution
  coverage* by compiling and running the test suite. Use them for real
  execution-coverage numbers. This lane records no execution — a covered row
  means "a verification node is linked to this symbol", not "this line ran".
- **`cargo test`** actually executes tests and reports pass/fail. This lane
  never executes anything and never asserts a test passed — a linked
  `TestRun`/`CommandRun` is a recorded citation, not a green check.
- **Verus / proof tools** establish *correctness*. A linked `ProofResult` here
  is a recorded citation that a proof artifact exists, never a soundness
  verdict.
- **CI coverage dashboards** aggregate execution coverage across runs. This
  lane is local, offline, and reads only the supplied store.
- **`grep -r '#\[test\]'` / grepping for test names** finds test *definitions*
  textually. This lane joins recorded verification-domain nodes to the public
  surface over the evidence-link graph — it never greps source, and it credits
  file-level and symbol-direct links regardless of how the test was written.

Because the default trunk store carries no verification→code links, the honest
default answer is the `verification_facts_unavailable` capability verdict — an
explicit "this evidence was never recorded", never a fabricated "everything is
untested".
## Corpus scope

Over a `scan-history` store this lane is **HEAD-anchored** by default: it reports
the state current at each repository's stamped HEAD commit, so an item removed
before HEAD does not appear. The summary envelope discloses `corpus_mode`
(`head_anchored`, or `single_snapshot` over a snapshot-less store),
`corpus_mode_source`, and `corpus_disclaimer`. An `--at`/`--as-of` selector this
lane accepts pins a single commit (`commit_pinned`). See
[Corpus scope for query lanes](corpus-modes.md).

## Store-wide counterpart

This lane measures verification coverage of **code symbols**. For the
**acceptance-criterion** census — what fraction of imported requirements are
closed by passing evidence, and which closed tasks own unproven criteria — see
[`docs/cli/criteria-coverage.md`](criteria-coverage.md). High code coverage with
unproven acceptance criteria is still an unproven feature.
