# Aletheia Codegraph Agent Guide

## Project Shape

Aletheia Codegraph is a standalone Rust project for extracting codebase structure into an AletheiaDB-compatible graph. Keep the repo independent from AletheiaDB release mechanics; integrate through a thin adapter boundary and documented schema contracts.

## Current Status

Only the repo skeleton and PRD exist. Do not pretend parser, CLI, ingestion, or query surfaces are implemented until tests and code are present.

## Working Rules

- Use SPEC-PROOF-RED-GREEN-REFACTOR for implementation work.
- No behavior ships without a test.
- Keep core graph extraction deterministic and filesystem-local.
- Prefer Tree-sitter for syntax parsing rather than ad hoc regex parsing.
- Keep AletheiaDB writes behind an adapter so CLI, daemon, or SDK transports can swap without changing the extractor.
- Do not introduce network crawling or remote repository fetching in the MVP.

## Expected Verification

Run these before claiming implementation work is done:

```powershell
cargo fmt --all
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

For ingestion work, also test against a temporary AletheiaDB data dir before touching the shared memory store.
