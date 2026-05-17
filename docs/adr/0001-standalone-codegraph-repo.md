# ADR 0001: Keep Egregore Separate From AletheiaDB

## Status

Accepted

## Context

Egregore needs to experiment with Tree-sitter parsing, graph schemas, incremental indexing, agent memory schemas, project/task memory, and AletheiaDB ingestion. Putting that work directly inside AletheiaDB would couple agentic SWE product churn to the database crate release path.

The system still needs a clear AletheiaDB integration contract because the end goal is durable agent memory, not a disconnected analyzer.

## Decision

Build Egregore as a standalone Rust repository.

Use explicit schema and adapter boundaries for integration with AletheiaDB. The code graph extractor produces a stable intermediate graph; later agent memory, project/task, artifact, and verification domains must attach through typed records and evidence links rather than AletheiaDB crate-private internals.

## Consequences

Positive:

- Parser and schema experiments can move without blocking AletheiaDB releases.
- The extractor and future memory domains can be tested without a running database.
- AletheiaDB integration remains swappable as the CLI/daemon/MCP surface evolves.

Negative:

- Version and contract drift must be managed explicitly.
- Cross-repo changes will need coordinated fixtures or compatibility tests.
- The adapter boundary must stay narrow or this repo will grow database-shaped roots.
