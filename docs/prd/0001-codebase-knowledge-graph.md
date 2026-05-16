# Aletheia Codegraph PRD

## Summary

Aletheia Codegraph turns a local source repository into a durable, queryable code graph for coding agents. It parses source files with Tree-sitter, produces a stable intermediate representation of files, symbols, and relationships, and ingests that graph into AletheiaDB so future agents can recall codebase structure without rediscovering it from scratch.

The project starts as a standalone repo. That keeps parser and ingestion experiments out of the AletheiaDB crate release path while preserving a clear integration contract with AletheiaDB.

## Problem

Agents repeatedly re-scan the same codebases to answer structural questions: where a type is defined, which modules call a function, how a route reaches storage, or what files are likely affected by a change. Plain transcript memory is not enough because codebase knowledge is relational, changes over time, and needs stable handles.

The missing piece is a local code intelligence pipeline that extracts code structure into a graph that AletheiaDB can store, traverse, and eventually enrich with semantic search.

## Goals

- Parse local repositories into deterministic graph data using Tree-sitter.
- Represent files, modules, symbols, definitions, references, imports, calls, and containment relationships with stable IDs.
- Ingest graph nodes and edges into AletheiaDB through a transport adapter.
- Support incremental re-indexing so changed files update the graph without rebuilding everything.
- Give agents a shared memory substrate for codebase navigation, impact analysis, and project recall.
- Keep the extractor standalone and testable without requiring a running AletheiaDB instance.

## Non-Goals

- No remote crawling, GitHub indexing service, or hosted SaaS in the MVP.
- No attempt to replace rust-analyzer, TypeScript language services, or full compiler semantic analysis.
- No cross-language type resolution in the MVP.
- No automatic code modification.
- No dependency on AletheiaDB internals or crate-private APIs.
- No mandatory embeddings in the MVP; semantic enrichment is a follow-up layer.

## Users

### Primary User: Coding Agent

The agent needs fast answers to codebase navigation questions and durable context across sessions.

### Primary User: Mark

Mark needs local-first, inspectable tooling that can feed AletheiaDB memory without creating release coupling with the database repo.

### Secondary User: Future Maintainer

The maintainer needs deterministic output, focused tests, and clear schema versioning so graph changes do not silently corrupt memory.

## MVP Scope

### CLI

The MVP should expose a small CLI:

```text
aletheia-codegraph scan <repo-path> --out graph.jsonl
aletheia-codegraph inspect graph.jsonl
aletheia-codegraph ingest graph.jsonl --adapter aletheia-cli
```

The CLI should be deterministic: the same repository state and config produce the same graph IDs and JSONL output.

### Language Support

Rust is the first supported language. Add TypeScript and Python only after the Rust extractor has stable schema coverage, fixtures, and ingestion tests.

### Graph Model

Initial node kinds:

| Kind | Purpose | Stable ID Input |
|------|---------|-----------------|
| `Repository` | Indexed repo root | canonical repo root plus VCS remote when available |
| `File` | Source file | repo-relative path |
| `Module` | Language module namespace | repo-relative path plus module path |
| `Symbol` | Function, struct, enum, trait, impl, const, static, type alias, route, or test | file path plus syntax span plus normalized name |
| `Import` | Import/use declaration | file path plus syntax span |
| `Diagnostic` | Extractor warning or unsupported construct | file path plus message hash |

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

Every emitted node must include:

- `id`
- `kind`
- `schema_version`
- `repo_relative_path` when file-backed
- `span` when syntax-backed
- `name` when named
- `summary`

### AletheiaDB Ingestion

The MVP adapter writes nodes and edges to AletheiaDB without relying on MCP. Until the daemon/client contract is ready, the first adapter may shell out to the installed `aletheia` CLI and use `ALETHEIADB_CONFIG`/`ALETHEIADB_DATA_DIR`.

The ingestion layer must be isolated behind a trait so future adapters can target:

- AletheiaDB CLI
- AletheiaDB daemon/API
- AletheiaDB Rust SDK
- JSONL-only dry runs

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

### PR-4: Safe AletheiaDB Writes

The CLI adapter must not corrupt shared memory or silently claim writes succeeded.

Acceptance criteria:

- Ingestion can target a temporary AletheiaDB data directory.
- Each write reads back the node or edge when the transport supports it.
- Failures preserve the JSONL input for retry.
- The adapter reports partial success clearly.

### PR-5: Agent-Useful Query Handles

The output must preserve handles agents can cite in answers.

Acceptance criteria:

- File paths are repo-relative.
- Symbols include name, kind, span, and containing file.
- Edges include source, target, label, and optional confidence.

## Success Metrics

- Index a representative Rust crate and produce a valid graph without panics.
- Re-running `scan` on an unchanged repo yields byte-for-byte equivalent JSONL after canonical ordering.
- Ingest a small fixture graph into a temporary AletheiaDB store and traverse `Repository -> File -> Symbol`.
- For a changed file, incremental scan touches only the changed file and affected tombstones.
- Agents can answer "where is this symbol defined?" and "what symbols does this file define?" from graph output alone.

## Architecture

Proposed module boundaries:

| Module | Responsibility |
|--------|----------------|
| `config` | Load scan and ingest configuration |
| `fs` | Discover files, apply ignore rules, compute file hashes |
| `ir` | Stable graph node/edge types and JSONL serialization |
| `parser` | Tree-sitter parser orchestration |
| `languages::rust` | Rust-specific Tree-sitter queries and extraction |
| `incremental` | Cache, diff, and tombstone planning |
| `adapters` | JSONL, fake, and AletheiaDB transports |
| `cli` | Command-line interface |

The parser should produce IR, not database writes. The adapter layer owns persistence. This keeps tests fast and avoids coupling extractor correctness to AletheiaDB runtime behavior.

## Risks

- Stable IDs are easy to get subtly wrong. Span-only IDs can churn after edits; name-only IDs can collide.
- Tree-sitter syntax coverage is not the same as compiler semantic truth.
- AletheiaDB CLI currently has a limited query/update surface, so ingestion may need append-oriented behavior before true updates.
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

### M4: AletheiaDB Adapter

- JSONL ingest
- Temporary-store integration test
- Read-back verification
- Partial failure reporting

### M5: Incremental Cache

- File hashing
- Reuse unchanged file output
- Tombstone output for removals
- Regression tests for rename/change/delete

## Open Questions

- Should the first CLI binary be named `aletheia-codegraph`, `acg`, or both?
- Should stable IDs include a repository identity prefix derived from Git remote, local path, or an explicit config value?
- Should the AletheiaDB ingestion schema use generic `CodeEntity` nodes or specific labels like `File`, `Symbol`, and `Module`?
- Should semantic embeddings attach to symbols, files, or both?
- Should deleted symbols be represented as tombstone nodes, status updates, or temporal validity changes once the AletheiaDB update surface exists?
