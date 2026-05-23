# Egregore Code Graph Domain PRD

This PRD defines the first Egregore domain: deterministic source-derived code facts and Git history. It is subordinate to the broader Egregore product vision, which connects code facts with agent memory, project/task state, artifacts, and verification evidence in the same AletheiaDB-backed graph.

## Summary

Egregore's code graph domain turns a local source repository and its Git history into a durable, queryable, bi-temporal code graph for coding agents. It parses source files with Tree-sitter, replays repository history commit-by-commit, produces a stable intermediate representation of files, symbols, relationships, and changes, and writes that graph into an embedded AletheiaDB store so future agents can recall not only what the codebase looks like now, but how it became that way. Derived drift measurements live in the separate [`semantic` domain schema](../schema/semantic-drift.md).

The project starts as a standalone repo. That keeps parser and ingestion experiments out of the AletheiaDB crate release path while preserving a clear integration contract with AletheiaDB.

## Problem

Agents repeatedly re-scan the same codebases to answer structural questions: where a type is defined, which modules call a function, how a route reaches storage, or what files are likely affected by a change. They also lose the historical context that explains why a symbol exists, when it drifted semantically, and which commits changed the meaning of a subsystem. Plain transcript memory is not enough because codebase knowledge is relational, temporal, semantic, and needs stable handles.

The missing piece is a local code intelligence pipeline that extracts current and historical code structure into a graph that AletheiaDB can store, traverse, time-travel, and enrich with semantic search.

## Goals

- Parse local repositories into deterministic graph data using Tree-sitter.
- Replay Git history into graph snapshots and change events.
- Represent files, modules, symbols, definitions, references, imports, calls, and containment relationships with stable IDs.
- Ingest graph nodes and edges into an embedded AletheiaDB store through a narrow adapter boundary.
- Use AletheiaDB's bi-temporal storage intentionally: Git commit time is the valid-time axis, and ingestion/indexing time is the transaction-time axis.
- Use AletheiaDB semantic features, including semantic search and temporal semantic drift, to surface changes in code meaning over history.
- Support incremental re-indexing so changed files update the graph without rebuilding everything.
- Give agents a shared memory substrate for codebase navigation, impact analysis, and project recall.
- Keep the extractor standalone and testable without requiring a running AletheiaDB instance.

## Non-Goals

- No remote crawling, GitHub indexing service, or hosted SaaS in the MVP.
- No attempt to replace rust-analyzer, TypeScript language services, or full compiler semantic analysis.
- No cross-language type resolution in the MVP.
- No automatic code modification.
- No dependency on AletheiaDB internals or crate-private APIs; embedded integration must use public AletheiaDB crate APIs.
- No mandatory remote embedding service in the MVP. Local embedding execution is explicit and routes through AletheiaDB's `embeddings` feature, which re-exports `embed_anything`.

## Users

### Primary User: Coding Agent

The agent needs fast answers to codebase navigation questions and durable context across sessions.

### Primary User: Mark

Mark needs local-first, inspectable tooling that can feed AletheiaDB memory without creating release coupling with the database repo.

### Secondary User: Future Maintainer

The maintainer needs deterministic output, focused tests, and clear schema versioning so graph changes do not silently corrupt memory.
Record-level compatibility is governed by
[`docs/schema/schema-versioning.md`](../schema/schema-versioning.md): codegraph
uses `SCHEMA_VERSION` as its domain default, while readers check
`(domain, kind, schema_version)` before accepting records from JSONL, embedded
storage, daemon reads, or incremental cache reuse.

## MVP Scope

### CLI

The MVP should expose a small CLI:

```text
egregore scan <repo-path> --out graph.jsonl
egregore scan-history <repo-path> --out history.graph.jsonl
egregore inspect graph.jsonl
egregore ingest graph.jsonl --adapter embedded --data-dir .egregore
eg query symbol <name>  --graph graph.jsonl
eg query symbol <name>  --graph history.graph.jsonl --at <commit>
eg query file <path>    --graph graph.jsonl
eg query drift          --graph history.graph.jsonl --limit 10
```

The CLI should be deterministic: the same repository state and config produce the same graph IDs and JSONL output.

The primary binary is `egregore`; `eg` is a short alias for repeated local use.

`scan` indexes the current working tree. `scan-history` walks Git commits in deterministic topological order, checks out each tree through Git object reads rather than mutating the user's workspace, extracts code graph records for each commit, and emits temporal metadata for AletheiaDB ingestion.

`query` reads a JSONL graph and answers agent-callable questions with stable record IDs and file/span/commit handles. Output is newline-delimited JSON by default; `--format text` produces human-readable terminal output. The stable output schema is documented in [docs/cli/query.md](../cli/query.md).

### Language Support

Rust is the first supported language. Add TypeScript and Python only after the Rust extractor has stable schema coverage, fixtures, and ingestion tests.

### Graph Model

Initial node kinds:

| Kind | Purpose | Stable ID Input |
|------|---------|-----------------|
| `Repository` | Indexed repo root | VCS remote URL (case 1), root commit SHA (case 2), or canonical absolute path (case 3) — see [docs/schema/repository-identity.md](../schema/repository-identity.md) |
| `File` | Source file | repo-relative path |
| `Module` | Language module namespace | repo-relative path plus module path |
| `Symbol` | Function, struct, enum, trait, impl, const, static, type alias, route, or test | repository namespace plus repo-relative path, symbol kind, qualified name, and source-order disambiguator; see [ADR 0004](../adr/0004-symbol-identity.md) |
| `Import` | Import/use declaration | file path plus syntax span |
| `Diagnostic` | Extractor warning or unsupported construct | file path plus message hash |
| `Commit` | Git commit observed during history replay | repository identity plus commit SHA |
| `Change` | File or symbol change between commits | commit SHA plus entity stable ID plus change kind |
| `SemanticDrift` | Semantic movement for a file or symbol over time | See [`docs/schema/semantic-drift.md`](../schema/semantic-drift.md); this is a `semantic` domain record, not a code-graph record. |

Initial edge labels:

| Label | From -> To | Meaning |
|-------|-----------|---------|
| `CONTAINS` | Repository/Directory/File/Module -> child | Hierarchical ownership |
| `DEFINES` | File/Module -> Symbol | Definition lives here |
| `IMPORTS` | File/Module -> Import | Import declaration appears here |
| `REFERENCES` | Symbol/Import -> Symbol | Best-effort syntactic reference |
| `CALLS` | Symbol -> Symbol | Best-effort function or method call |
| `IMPLEMENTS` | Symbol -> Symbol | Impl/trait relationship where syntactically resolvable |
| `MENTIONS` | Symbol -> Symbol | Weaker unresolved textual/syntactic mention |
| `CHANGED_IN` | File/Symbol -> Commit/Change | Entity changed in a commit |
| `PARENT_OF` | Commit -> Commit | Git commit ancestry |
| `DRIFTS_FROM` | SemanticDrift -> File/Symbol | Semantic-domain edge; see [`docs/schema/semantic-drift.md`](../schema/semantic-drift.md). |
| `DRIFTS_PRIOR` | SemanticDrift -> File/Symbol | Semantic-domain prior edge; see [`docs/schema/semantic-drift.md`](../schema/semantic-drift.md). |
| `MEASURED_BY` | SemanticDrift -> EmbeddingModel | Reserved semantic-domain model edge; see [`docs/schema/semantic-drift.md`](../schema/semantic-drift.md). |

Every emitted node must include:

- `id`
- `kind`
- `schema_version`
- `repo_relative_path` when file-backed
- `span` when syntax-backed
- `name` when named
- `summary`

History-backed records must also include:

- `git_commit`
- `git_parent_commits` when commit-backed
- `valid_time` derived from Git commit time
- `author_time` preserved from Git author metadata when available
- `observed_at` or equivalent transaction-time ingest metadata

### Git History And Bi-Temporal Model

The MVP must treat Git history as first-class input, not just context for incremental indexing. For every indexed commit, the code graph domain should extract the same IR shape as the current-tree scan and attach commit metadata to every file-backed and syntax-backed record.

Bi-temporal mapping:

- **Valid time:** the Git commit timestamp for when a code fact became true in repository history. Author time should be preserved as metadata; committer time is the default valid-time ordering because it reflects when the commit entered the project timeline.
- **Transaction time:** the time AletheiaDB observes and ingests the fact. Re-indexing the same repo later should create a new observation without pretending the Git history itself changed.
- **Identity:** stable IDs identify logical code entities across observations; temporal validity captures when a definition or relationship was present.

Symbol identity is defined in [ADR 0004](../adr/0004-symbol-identity.md):
`span` is a source-coordinate field, not an identity component, and the
source-order `disambiguator` resolves same-file collisions such as multiple
`impl Widget` blocks.

The selector grammar for querying across both axes is specified in [`docs/schema/temporal-selectors.md`](../schema/temporal-selectors.md) and implemented at `SCHEMA_VERSION = 2`. Current-tree records carry `valid_time_source: "inferred_from_transaction_time"`; history records carry `valid_time_source: "git_commit_committer_date"`.

History replay must not mutate the user's working tree. Use Git object reads, temporary worktrees, or a safe staging directory. If checkout-based replay is used, it must run outside the user's active checkout and clean up after itself.

### Embedded AletheiaDB Ingestion

The MVP adapter writes nodes and edges directly to an embedded AletheiaDB store without relying on MCP, a daemon, or shelling out to the installed `aletheia` CLI. The Egregore code graph domain owns extraction and schema mapping; AletheiaDB owns durable graph storage, bi-temporal indexing, semantic search, and semantic drift support through its public Rust API.

The ingestion layer must still be isolated behind a trait so tests stay fast and future adapters can target:

- Embedded AletheiaDB store
- AletheiaDB daemon/API
- AletheiaDB Rust SDK
- AletheiaDB CLI compatibility fallback
- JSONL-only dry runs

Egregore should enable AletheiaDB's stable `semantic-search` feature and the temporal/diagnostic semantic cohorts needed for drift analysis by default when embedded ingestion is enabled. Full AletheiaDB `nova` should remain an explicit Egregore feature flag until a concrete workflow needs every experimental cohort.

### Incremental Indexing

MVP incremental behavior:

- Hash each indexed file.
- Reuse unchanged file graph output.
- Rebuild changed files.
- Emit tombstones for files removed since the previous snapshot.

Full graph diff application inside AletheiaDB is a later milestone if CLI update/delete support is not available.

## Product Requirements

### PR-1: Deterministic Extraction

Given the same repository state, config, and binary version, `scan` must produce stable node IDs and edge IDs.

Acceptance criteria:

- Fixture tests compare JSONL snapshots for a small Rust repo.
- Output ordering is deterministic.
- Absolute machine-local paths do not leak into stable IDs.

### PR-2: Rust Symbol Coverage

The Rust extractor must identify top-level and nested definitions for common Rust constructs.

Acceptance criteria:

- Fixtures cover modules, functions, structs, enums, traits, impl blocks, methods, constants, statics, type aliases, tests, and macro invocations as diagnostics or explicit unsupported nodes.
- Unsupported constructs produce diagnostics instead of panics.

### PR-3: Adapter Boundary

Scanning must work without AletheiaDB installed or running.

Acceptance criteria:

- `scan` and `inspect` work in JSONL-only mode.
- AletheiaDB-specific code lives behind an adapter boundary.
- Tests use a fake adapter for ingestion behavior.

### PR-4: Safe Embedded AletheiaDB Writes

The embedded adapter must not corrupt shared memory or silently claim writes succeeded.

Acceptance criteria:

- Ingestion can target a temporary AletheiaDB data directory.
- Each write reads back the node or edge through the embedded store API.
- Failures preserve the JSONL input for retry.
- The adapter reports partial success clearly.

### PR-5: Git History Replay

The code graph domain must be able to index a repository's Git history without mutating the user's checkout.

Acceptance criteria:

- A fixture Git repo with at least three commits produces deterministic `Commit`, `Change`, and file/symbol records.
- Commit ancestry is represented with `PARENT_OF`.
- File and symbol records include commit SHA and valid-time metadata.
- Re-running history scan on the same repo yields byte-for-byte equivalent JSONL after canonical ordering.
- The user's working tree remains unchanged after history replay.

### PR-6: Agent-Useful Query Handles

The output must preserve handles agents can cite in answers, and the `eg query` CLI must expose them without requiring Rust.

Acceptance criteria:

- File paths are repo-relative.
- Symbols include name, kind, span, and containing file.
- Edges include source, target, label, and optional confidence.
- `eg query symbol <name> --graph` prints JSONL with `record_id`, `name`, `kind`, `repo_relative_path`, `span`, and `git_commit` when present. Exit `0` on match, `2` on no match.
- `eg query symbol <name> --graph --at <commit>` returns the single best match at that commit SHA or prefix; exits `1` with `error: ambiguous commit prefix` for non-unique prefixes.
- `eg query file <path> --graph` lists all symbols defined in the file via `DEFINES` edges, sorted by span then record ID.
- `eg query drift --graph [--limit N]` lists the top-N `SemanticDrift` nodes by score with target `repo_relative_path` and `name` resolved from `DRIFTS_FROM` edges.

### PR-7: Temporal Semantic Drift

The code graph domain must identify semantic movement in files and symbols over Git history when embeddings are enabled.

Acceptance criteria:

- Embedding candidates are generated for file and symbol summaries through AletheiaDB's `embeddings` feature and its `embed_anything` re-export.
- Drift records compare the same logical file or symbol across commits.
- Drift records preserve before/after commit SHAs, valid-time range, structured `embedding_model`, score, selection threshold, and explanation summary.
- Semantic drift can be queried from graph output alone, and embedded AletheiaDB ingestion preserves the records for temporal traversal.

## Success Metrics

- Index a representative Rust crate and produce a valid graph without panics.
- Re-running `scan` on an unchanged repo yields byte-for-byte equivalent JSONL after canonical ordering.
- Replaying a representative Git history yields deterministic temporal graph output without mutating the working tree.
- Ingest a small fixture graph into a temporary embedded AletheiaDB store and traverse `Repository -> File -> Symbol`.
- Ingest a small fixture history into a temporary embedded AletheiaDB store and answer "what did this symbol mean at commit X?" and "when did this symbol semantically drift?"
- For a changed file, incremental scan touches only the changed file and affected tombstones.
- Agents can answer "where is this symbol defined?" and "what symbols does this file define?" from graph output alone.
- Agents can answer "when did this symbol change?", "which commit introduced this call edge?", and "which files drifted semantically during this feature?"

## Architecture

Proposed module boundaries:

| Module | Responsibility |
|--------|----------------|
| `config` | Load scan and ingest configuration |
| `fs` | Discover files, apply ignore rules, compute file hashes |
| `history` | Read Git repository metadata, commit DAGs, trees, blobs, and safe history snapshots |
| `ir` | Stable graph node/edge types and JSONL serialization |
| `parser` | Tree-sitter parser orchestration |
| `languages::rust` | Rust-specific Tree-sitter queries and extraction |
| `incremental` | Cache, diff, and tombstone planning |
| `adapters` | JSONL, fake, embedded AletheiaDB, and fallback transports |
| `embeddings` | AletheiaDB embedding re-export, embedding candidates, semantic search hooks, and drift records |
| `query` | Agent-facing helpers such as symbol-at-commit and ranked semantic drift |
| `cli` | Command-line interface |

The parser should produce IR, not database writes. The adapter layer owns persistence. This keeps tests fast and avoids coupling extractor correctness to AletheiaDB runtime behavior.

## Risks

- Stable IDs are easy to get subtly wrong. Span-only IDs can churn after edits; name-only IDs can collide.
- Tree-sitter syntax coverage is not the same as compiler semantic truth.
- AletheiaDB public embedded APIs may not expose every graph update primitive Egregore wants initially, so ingestion may need append-oriented behavior before true updates.
- Git history replay can be expensive on large repos; MVP fixtures must prove deterministic ordering first, then optimization can follow.
- Commit-time semantics are subtle: rebases, cherry-picks, and amended commits can change transaction-time observations without changing the valid-time story.
- Semantic drift can look impressive while being noisy. The MVP must preserve structured `embedding_model`, target text, threshold, metric, and score so agents can explain evidence instead of hallucinating insight.
- Incremental indexing can create stale edges if file-level invalidation is too narrow.
- Multi-language support can sprawl unless Rust reaches a clean MVP first.

## Milestones

### M1: Repo Skeleton and PRD

- Rust crate scaffold
- PRD
- ADR directory
- Agent guide

### M2: Rust Fixture Extractor

- Tree-sitter Rust parser dependency
- Fixture repo
- JSONL IR output
- Snapshot tests

### M3: CLI and Inspect

- `scan`
- `inspect`
- Stable ordering
- Human-readable diagnostics

### M4: Embedded AletheiaDB Adapter

- JSONL ingest
- Temporary-store integration test
- Read-back verification
- Partial failure reporting

### M5: Git History Replay

- Commit DAG scan
- Git tree/blob extraction without mutating working checkout
- Commit, parent, and change records
- Valid-time metadata on file and symbol records
- Deterministic history JSONL tests

### M6: Bi-Temporal Embedded Ingestion

- Temporary-store history integration test
- Read-back verification for temporal metadata
- Traverse `Commit -> Change -> Symbol`
- Query or adapter helper for "symbol at commit"

### M7: Incremental Cache

- File hashing
- Reuse unchanged file output
- Tombstone output for removals
- Regression tests for rename/change/delete

### M8: Semantic Drift

- AletheiaDB `semantic-search` and `semantic-temporal` feature integration
- AletheiaDB `embeddings` feature re-export for `embed_anything`
- Symbol/file embedding candidates by commit
- Drift records for changed semantic meaning
- Demo query for "largest drift over this history range"

## MVP Decisions And Follow-Ups

Resolved for the MVP:

- The first binary is `egregore`, with `eg` as a short alias.
- The embedded AletheiaDB schema uses specific codegraph labels such as `Repository`, `File`, `Symbol`, `Commit`, and `Change`; semantic labels such as `SemanticDrift` are owned by [`docs/schema/semantic-drift.md`](../schema/semantic-drift.md).
- Semantic embedding candidates attach to both files and symbols.
- Committer time drives valid-time ordering; author time is preserved as temporal metadata.
- `semantic-search`, `semantic-temporal`, and `semantic-diagnostics` are default-on with embedded AletheiaDB ingestion.
- Full `nova` remains an explicit Egregore feature flag.

Follow-up product decisions:

- Whether to add `acg` as a short alias binary.
- Whether shared multi-repo stores need an explicit repository identity prefix beyond the current repo-scoped stable IDs.
- Whether deleted symbols should become tombstone nodes, status updates, or temporal validity changes once the AletheiaDB update surface grows first-class update/delete semantics.
