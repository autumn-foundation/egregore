# Aletheia Codegraph

Aletheia Codegraph is a standalone experiment for turning local codebases into a durable graph that agents can query through AletheiaDB memory.

The first product slice is intentionally narrow: parse repositories with Tree-sitter, emit a stable intermediate graph, and ingest that graph into the shared local AletheiaDB store without coupling this repo to the AletheiaDB crate release cycle.

## Status

Seeded with a PRD and Rust crate skeleton. Parser, CLI, and Aletheia ingestion code have not been implemented yet.

## Primary Documents

- Product requirements: [docs/prd/0001-codebase-knowledge-graph.md](docs/prd/0001-codebase-knowledge-graph.md)
- Architecture decisions: [docs/adr/README.md](docs/adr/README.md)
- Implementation plans: [docs/plans/](docs/plans/)

## Development

```powershell
cargo fmt --all
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```
