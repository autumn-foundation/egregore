# RFC 0001: Egregore as Vertere's Code-Intelligence and Provenance Engine

## Status

Draft. A positioning and mapping note, not a committed decision. Egregore-side
scope only: nothing here changes Egregore's standalone positioning (ADR 0001) or
product identity (ADR 0002). Vertere is a separate project; this RFC records how
Vertere should consume Egregore, and what Egregore would need to add, without
pulling any Vertere-specific type into Egregore core.

## Context

Vertere is a separate Rust platform that unifies four surfaces:

- **Typed rooms** — chat as configurable collaboration environments (RepoRoom,
  ReviewRoom, PipelineRoom).
- **Native git hosting** — smart HTTP/SSH, Sapling stacks.
- **Durable workflows** ("Autumn Harvest") — a Temporal-style engine on Postgres
  running agents, CI, and approvals as persistent workflows with signals and
  activities.
- **AletheiaDB** — a bi-temporal knowledge graph answering "what did we know when
  this decision was made."

Vertere's PRD Phase 0 names "a basic AletheiaDB adapter for knowledge writes from
messages and git pushes" and "a Harvest workflow for repository indexing." Phase 1
adds agents that query AletheiaDB before acting and write knowledge after acting,
plus code search across structural, semantic, graph, and temporal queries. Phase 2
adds a Room Timeline (time scrubber, snapshots, compare, provenance drill-down) and
semantic drift detection. Vertere's stated hypothesis #4 is that temporal knowledge
plus provenance is the enterprise differentiator.

Egregore already implements the adapter and indexing layer those phases describe.
The thesis of this RFC:

> Egregore is that layer. Vertere should consume it, not rebuild it.

The rest of this document maps each Vertere surface to a shipped Egregore capability,
names the integration seams, lists the domains Vertere needs that Egregore does not
have yet, and lists the gaps a cloud/multi-writer deployment must close. Egregore's
existing commands, issue numbers, and `docs/` contracts are the ground truth cited
throughout; every mapping points at a real command and its documented contract.
Issue references distinguish "shipped" (a merged change with a `docs/cli/` contract)
from "future" (an open tracking issue).

## 1. Positioning

Egregore stays a standalone repository with thin adapter boundaries (ADR 0001;
`CLAUDE.md`, "Project Shape"). Vertere consumes it as a library/binary, not as a
service embedded in Vertere's codebase:

- **Harvest indexing activity.** On git push, a Harvest workflow activity runs
  `eg scan` (or `eg scan-history` for historical backfill) over the pushed tree and
  `eg ingest` into the shared AletheiaDB store. Scan and ingest are separate steps
  with JSONL between them (§3), so each is an independently retryable workflow
  activity.
- **Local daemon is the single-tenant special case.** `eg daemon start` over an
  embedded store (ADR 0003) is the self-hosted, single-writer deployment. It is not
  a different product; it is one transport.
- **Cloud SaaS is a transport/tenancy extension, not a redesign.** Multi-tenant
  cloud is an adapter/transport and tenancy-scoping concern layered on the same
  extractor and schema. The extractor stays deterministic and filesystem-local
  (`CLAUDE.md`, "Working Rules"); only the write transport and tenancy scoping
  change, not the graph model.

The contract between the two projects is the schema (`docs/schema/`) plus the
CLI/adapter surface — never shared Rust types (§6).

## 2. Domain Mapping

Each Vertere surface below maps to a shipped Egregore command with a documented
contract.

### 2.1 RepoRoom knowledge panel → `eg query subsystem` / `eg query context`

A RepoRoom scoped to a directory shows a knowledge panel for that subsystem.
`eg query subsystem <prefix>` (issue #83, shipped) returns a deterministic,
trust-separated JSON envelope — code facts, agent observations, project state,
artifacts, verification evidence, and semantic drift — for everything under a
repo-relative prefix. Path matching is segment-aware: `src/alpha` never bleeds into
`src/alphabet`. `eg query context <symbol>` (issue #38, shipped) is the symbol-level
form of the same trust-separated bundle, and `eg query locate <path>:<line>` (issue
#212) is the positional entry into that same contract. See `docs/cli/query.md`.

Runtime error-signatures are **not yet** part of the subsystem envelope. Folding the
log-graph `log_signatures` section into it is tracked as open issue #325 ("Fold error
signatures into the subsystem context envelope"). Until it lands, a RepoRoom that
wants runtime context queries the log surface directly (§2.3).

### 2.2 ReviewRoom approval and provenance → the compliance track

A ReviewRoom presents an approval workflow whose evidentiary trail is exactly what
Egregore's compliance track already records:

- **Reviewer identity and requests** (issue #335, shipped). The GitHub importer
  records reviewer identity and review requests as graph nodes with
  `REVIEWED_BY` / `REQUESTED_REVIEW_FROM` edges. See `docs/schema/import-github.md`
  and `docs/schema/project-graph.md`.
- **Review anchoring** (issues #333/#334, shipped). PR head/base/merge SHAs (#333)
  and the `review_commit_sha` anchor / `REVIEWS_COMMIT` edge (#334) tie an approval
  to the exact commit it approved, so a later push can be detected as invalidating a
  stale approval.
- **Review-state history** (issue #336, shipped). `eg import github` mints
  append-only `ReviewStateTransition` nodes (with a `TRANSITIONS_REVIEW` edge on
  dismissal) from the PR timeline. `Review.review_state` stays a last-write-wins
  current-state summary; the transitions are the durable history. A dismissal
  overwrites `approved`→`dismissed` on the summary but never erases the approval
  evidence — a consumer needing "review state as of T" joins the transitions rather
  than reading the summary field. See `docs/cli/github-import.md`.
- **Review-coverage gate** (issue #339, shipped). `eg audit review-coverage
  --from <T0> --to <T1>` classifies every PR merged in a valid-time window into a
  closed verdict set (`covered`, `approval_stale_head`, `self_approved_only`,
  `uncovered`) and gates the covered ratio. Its per-PR derivation is the single
  shared implementation that the evidence-pack `review_coverage` section also calls,
  so the two surfaces never diverge. See `docs/cli/review-coverage.md`.
- **Evidence packs** (issues #337/#338/#340). `eg audit control-catalog` (#337)
  loads and BLAKE3 hash-pins a versioned SOC2 control→evidence-class catalog.
  `eg audit evidence-pack assemble` (#338) builds a deterministic, redaction-safe
  pack scoped to one control and one half-open valid-time window, echoing the catalog
  hash pin in the manifest; `eg audit evidence-pack verify` re-checks it offline.
  Runtime log-graph incident evidence folds into the CC7.2/CC7.3 sections (#340; PR
  merged, tracking issue #340 open) with content-addressed exemplar handles and no
  raw log text. Rows are recorded observations of process execution, never proof of
  control effectiveness — the manifest carries that disclaimer verbatim. See
  `docs/cli/evidence-pack.md` and `docs/cli/control-catalog.md`.

This is the substrate behind Vertere's hypothesis #4: the provenance and
compliance differentiator is already built and cited, not aspirational.

### 2.3 PipelineRoom and CI failures → the log-graph domain

A PipelineRoom surfaces CI runs and failures. Egregore's log-graph domain turns a
captured log into deterministic, redaction-safe graph records:

- **`eg scan-logs`** (issues #319/#320, shipped). Extracts a `LogSource`, one
  `ErrorSignature` per `template-v1` fingerprint (a 1000×-repeated error collapses to
  one signature with `occurrence_count` == the raw count), capped `LogEvent`
  exemplars, and hourly `LogOccurrenceBucket` counts. Trust class
  `runtime_observation` — a program's own claim, deterministically parsed but never
  verified. Raw log text never enters the graph; excerpts are `template-v1`-normalized,
  redacted, and bounded. See `docs/cli/scan-logs.md` and `docs/schema/log-graph.md`.
- **`eg resolve-frames`** (issue #322, shipped). Resolves the structured,
  redaction-safe backtrace frames on each `ErrorSignature` to code-graph targets,
  emitting `FRAME_RESOLVES_TO` edges labeled with a closed resolution set
  (`resolved` / `ambiguous` / `path_only` / `unresolved`). A binding proves the frame
  *names* the symbol, never that the symbol is at fault. See
  `docs/cli/resolve-frames.md`.
- **`eg link-logs`** (issue #323, shipped). Links each `ErrorSignature` to the agent
  runs/commands that produced it via `EMITTED_DURING` edges, each carrying exactly
  one closed `basis`: `content_hash_join` (exact stdout/stderr byte equality,
  confidence 1.0) or `temporal_correlation` (a run-window overlap, confidence 0.5 —
  "a correlation lead, never causation"). See `docs/cli/link-logs.md`.
- **`eg query error-context`** (issue #324; PR merged, tracking issue #324 open).
  Resolves one `ErrorSignature` and assembles a single deterministic, trust-separated
  cross-domain envelope — code facts (via each resolved frame target), occurrence
  history, agent trajectory, and history `first_seen_range` — one cited answer where
  an agent otherwise runs four tools. Rows are correlation leads, never proof of
  cause. See `docs/cli/error-context.md`.
- **`eg query log-deltas`** (issue #326, shipped). Classifies runtime
  error-signatures across a commit range into `new` / `ceased` / `continuing`, with
  per-window occurrence counts. A new signature is a regression lead; a ceased one is
  not proof of a fix. See `docs/cli/log-deltas.md`.

Webhook and CI event streams that Vertere already ingests become future `LogSource`
kinds feeding the same domain (§3).

### 2.4 Agent memory loop → semantic recall + citation/token gates

Vertere Phase 1 agents "query before acting, write knowledge after acting." Egregore
provides both halves plus the trust discipline that keeps them honest:

- **`eg query semantic-memory`** and the trust-class model separate deterministic
  code facts from agent-authored observations through typed nodes, provenance, and
  evidence links. A standing invariant: an agent-authored claim is never counted as
  evidence for itself, and a runtime observation is never counted as verification.
- **`eg audit citations`** (issue #65, shipped; log-domain coverage via #328) drives
  every public query workflow over a seeded record set and fails when returned rows
  lack the citation handles their trust class requires — an uncited answer is a miss,
  not a win. See `docs/cli/citation-audit.md`.
- **`eg audit token-cost`** (issue #84, shipped) measures the baseline-to-Egregore
  token-savings ratio per question class against an in-process ripgrep baseline, with
  correctness held constant (an uncited answer does not count as a win). This is the
  measurable "answer from the graph beats grepping the checkout" SLA number. See
  `docs/cli/token-cost.md`.

### 2.5 Room Timeline → `eg scan-history` replay + temporal queries

Vertere Phase 2's Room Timeline (time scrubber, snapshots, compare, provenance
drill-down) maps to Egregore's bi-temporal reconstruction surface:

- **`eg scan-history`** replays Git history and reads Git objects without mutating
  the working checkout (`CLAUDE.md`, "Working Rules").
- **`eg query at <path>:<line> --at <sha>`** (issue #151) resolves a raw location to
  the smallest enclosing symbol as it existed at a commit; **`eg query file <path>
  --at <sha>` / `--as-of <instant>`** (issue #158) reconstructs a file's defined-symbol
  set at a past point.
- **`eg query lifeline <symbol>`** (issues #96/#215) returns one symbol's ordered
  lifecycle events, folding in `SemanticDrift` records where they exist.
- Semantic drift detection is already an extracted, temporal graph fact, not a
  separate service. See `docs/cli/lifeline.md` and `docs/cli/query.md`.

### 2.6 Raw artifacts → the #60 protected store

Transcripts, command output, patch bytes, and generated reports go to the protected
raw-artifact store (issue #60): a local content-addressed store, BLAKE3-verified on
retrieval, that the graph, query, and semantic surfaces never read. Disabled by
default; the graph stores only a handle, never the bytes. See
`docs/cli/protected-artifacts.md`. This is where a Room's raw message and log payloads
live without polluting the citable graph.

## 3. Integration Seams

- **The adapter boundary.** All AletheiaDB writes go through one adapter so embedded,
  daemon, SDK, or CLI transports swap without changing the extractor (`CLAUDE.md`,
  "Working Rules"; ADR 0003). Vertere picks a transport per deployment; the extractor
  is unchanged.
- **JSONL as the interchange format.** `eg scan` / `eg scan-history` emit newline-
  delimited graph records; `eg ingest` consumes them. This is the seam between a
  Harvest scan activity and a Harvest ingest activity — each side is a pure,
  retryable step, and the JSONL is inspectable between them.
- **The pre-ingest validation gate.** `eg validate <graph.jsonl>` (issue #103) runs
  one read-only referential-integrity pass between scan and ingest — a natural Harvest
  activity boundary that fails a workflow step before a malformed graph reaches the
  shared store. See `docs/cli/validate.md`.
- **Documented schema contracts.** `docs/schema/` is the interchange contract:
  `log-graph.md`, `project-graph.md`, `import-github.md`, `redaction.md`,
  `schema-versioning.md`, and the rest. Vertere codes against these, not against
  Egregore's internal types.
- **Write concurrency.** The embedded store takes an OS-level exclusive lease
  (issue #200): one writer per data dir, with concurrent writers routed through
  `eg daemon start` + `--adapter daemon`. A cloud deployment's multi-writer story
  builds on this lease, not around it (§5). See `docs/cli/embedded-concurrency.md`.
- **Event streams as future LogSource kinds.** Vertere's webhook and CI event streams
  become additional `LogSource` kinds feeding the existing log-graph domain, rather
  than a new ingestion path.

## 4. New Domains Vertere Needs That Egregore Does Not Have Yet

Vertere has first-class objects with no Egregore domain today: **room messages**,
**decisions**, and **workflow (Harvest) events**. These should be added the way the
log-graph domain was added, following that template:

1. Typed nodes with a declared trust class (a message and a decision are
   agent/human-authored `project_state`-style claims; a workflow event is closer to
   `runtime_observation`), kept distinct from deterministic code facts.
2. Structural rules in `eg validate` (issue #103) so every new edge endpoint and
   allowed target kind is checked before ingest.
3. Coverage in `eg audit citations` (issue #65) so the new rows are gated on the
   citation handles their trust class requires.
4. Evidence-pack wiring (issue #338) where a new domain carries compliance-relevant
   evidence (a decision record, an approval-adjacent message).

Whether these domains live in Egregore core depends on genericity: a generic
"message" / "decision" / "workflow-event" domain can live in Egregore behind the same
adapter boundary; Vertere-specific semantics stay in Vertere (§6).

## 5. Gap List for Cloud / Multi-Writer Mode

The single-writer, filesystem-local model is correct for the local daemon and needs
concrete work before a multi-tenant cloud deployment. Open tracking issues:

- **Log retention and identity.** `LogOccurrenceBucket` identity is not source-aware,
  so distinct sources and rescans are not distinguished in per-window counts
  (issue #361); the embedded read path collapses duplicate non-temporal log records,
  breaking multi-scan coalescing for `eg query log-deltas --data-dir` (issue #363);
  log records carry no persisted repository attribution, so `--repo` cannot filter
  log signatures (issue #362); endpoint-exact occurrence counts need sub-hour
  per-occurrence timestamps (issue #364).
- **Evidence-pack hardening.** Bind `frame_resolution` labels on error-signature
  evidence-pack rows via co-located `FRAME_RESOLVES_TO` edges (issue #371); extend the
  `runtime_observation` provenance requirement to the shared citation classifier used
  by the evidence-pack and bundle gates (issue #372); require exactly one `AGGREGATES`
  attribution edge per bucket in the occurrence bind (issue #374); window occurrence
  buckets by payload `bucket_start` rather than node `valid_time` (issue #375).
- **Validation and citation coverage.** Match daemon `source_kind` gates for
  reviewer-identity edges in `eg validate` (issue #369); wire the `error-context`
  (#324) and subsystem `log_signatures` (#325) workflows into `eg audit citations`
  (issue #376); resolve module-only frames in `eg resolve-frames --at` against the
  commit view rather than the whole history graph (issue #377).
- **Beyond current issues.** Tenancy and authorization beyond today's repo scoping;
  multi-writer concurrency beyond the single-writer lease (issue #200); and an event
  ingestion transport for webhook/CI streams (§3).

These are the shared priorities a Vertere integration would surface first.

## 6. Non-Goals

- Egregore does not become a service embedded inside Vertere's codebase. It is
  consumed as a library/binary through the adapter boundary.
- No Vertere-specific type enters Egregore core. RepoRoom, ReviewRoom, Harvest, and
  Room Timeline are Vertere concepts; Egregore exposes generic code-graph, log-graph,
  project, and evidence domains that Vertere maps onto.
- The contract is the schema (`docs/schema/`) plus the CLI/adapter surface — not
  shared Rust types, not a private API.
- Egregore does not fetch remote repositories or crawl (`CLAUDE.md`, "Working Rules").
  Vertere's native git hosting feeds Egregore local trees; Egregore does not reach
  back out to a remote.

## Consequences

- Vertere's Phase 0/1 "AletheiaDB adapter + repository-indexing Harvest workflow"
  reduces to a thin Harvest wrapper over shipped Egregore commands (`scan` →
  `validate` → `ingest`, then the `query` surface), rather than a second
  implementation of the same substrate.
- Vertere's hypothesis #4 (temporal knowledge + provenance as the enterprise
  differentiator) rests on already-built, cited surfaces: review-state history
  (#336), control-scoped evidence packs (#338/#340), and citation/token gates
  (#65/#84).
- Egregore gains a concrete downstream consumer, and the §5 gap list becomes a shared,
  prioritized roadmap instead of latent hardening debt.
- Divergence risk between the two projects is contained to one contract — the schema
  and CLI/adapter surface — because no types are shared across the boundary.
