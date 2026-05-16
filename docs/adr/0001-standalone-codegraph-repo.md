# ADR 0001: Keep Codegraph Extraction in a Standalone Repo

## Status

Accepted

## Context

Aletheia Codegraph needs to experiment with Tree-sitter parsing, graph schemas, incremental indexing, and AletheiaDB ingestion. Putting that work directly inside AletheiaDB would couple parser churn and CLI experiments to the database crate release path.

The system still needs a clear AletheiaDB integration contract because the end goal is durable agent memory, not a disconnected analyzer.

## Decision

Build Aletheia Codegraph as a standalone Rust repository.

Use explicit schema and adapter boundaries for integration with AletheiaDB. The extractor produces a stable intermediate graph; adapters decide how to persist that graph through CLI, daemon/API, SDK, or dry-run JSONL.

## Consequences

Positive:

- Parser and schema experiments can move without blocking AletheiaDB releases.
- The extractor can be tested without a running database.
- AletheiaDB integration remains swappable as the CLI/daemon/MCP surface evolves.

Negative:

- Version and contract drift must be managed explicitly.
- Cross-repo changes will need coordinated fixtures or compatibility tests.
- The adapter boundary must stay narrow or this repo will grow database-shaped roots.
