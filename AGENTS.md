# Egregore Agent Guide

## Project Shape

Egregore is a standalone Rust project for building an AletheiaDB-backed knowledge graph substrate for agentic software engineering. Keep the repo independent from AletheiaDB release mechanics; integrate through thin adapter boundaries and documented schema contracts.

The current implemented domain is code graph extraction: current and historical codebase structure becomes deterministic graph records that can share an AletheiaDB store with future agent memory, project/task, artifact, and verification-evidence domains.

## Current Status

The MVP extraction, CLI, embedded ingest, history replay, incremental cache, semantic drift, and query helper surfaces are implemented with tests.

Primary commands:

```powershell
cargo run -- scan . --out graph.jsonl
cargo run -- scan-history . --out history.graph.jsonl
cargo run -- inspect graph.jsonl
cargo run -- ingest graph.jsonl --adapter dry-run
cargo run -- ingest history.graph.jsonl --adapter embedded --data-dir .egregore
```

The primary binary is `egregore`; `eg` is also built as a short CLI alias.

## Working Rules

- Use SPEC-PROOF-RED-GREEN-REFACTOR for implementation work.
- No behavior ships without a test.
- Keep core graph extraction deterministic and filesystem-local.
- Keep deterministic code facts separate from agent-authored observations through typed nodes, provenance, and evidence links.
- History replay must read Git objects without mutating the user's active checkout.
- Prefer Tree-sitter for syntax parsing rather than ad hoc regex parsing.
- Keep AletheiaDB writes behind an adapter so embedded, daemon, SDK, or CLI transports can swap without changing the extractor.
- Use the published `aletheiadb` crate. Do not use a local path dependency.
- Do not depend on `embed_anything` directly. Use AletheiaDB's `embeddings` feature and re-export boundary.
- Keep `.cargo/config.toml`'s MSVC C++ CRT flag unless the upstream tokenizers/esaxx-rs link mismatch is fixed.
- Do not introduce network crawling or remote repository fetching in the MVP.

## Expected Verification

Run these before claiming implementation work is done:

```powershell
cargo fmt --all
cargo test --all-targets
cargo test --all-targets --no-default-features
cargo test --all-targets --features nova
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

For ingestion work, also test against a temporary AletheiaDB data dir before touching the shared memory store.
