# Aletheia Codegraph

Aletheia Codegraph turns a local Rust repository and its Git history into a deterministic graph that agents can query through embedded AletheiaDB memory.

The current slice parses Rust with Tree-sitter, emits stable JSONL graph records, replays Git history without mutating the working checkout, ingests records into an embedded AletheiaDB store, and exposes semantic-drift/query helpers for agent workflows.

## Status

Implemented MVP surfaces:

- `scan` for current-tree JSONL extraction
- `scan-history` for commit-by-commit temporal extraction
- `inspect` for graph summaries
- `ingest --adapter dry-run`
- `ingest --adapter embedded --data-dir <path>`
- Rust symbol extraction for common modules, imports, definitions, calls, mentions, impls, and diagnostics
- Incremental file-cache planning with tombstones
- AletheiaDB embedding re-export through the optional `embeddings` feature
- Semantic drift records and query helpers for symbol-at-commit and largest-drift workflows

`embedded-aletheiadb` is enabled by default and uses the published `aletheiadb` crate with `semantic-search`, `semantic-temporal`, and `semantic-diagnostics`.

On MSVC, this repo sets `CXXFLAGS_x86_64_pc_windows_msvc=/MT` in `.cargo/config.toml` so AletheiaDB's optional `embeddings` feature can link the transitive `embed_anything`/tokenizers native C++ stack consistently.

## Usage

```powershell
cargo run -- scan . --out graph.jsonl
cargo run -- scan-history . --out history.graph.jsonl
cargo run -- inspect graph.jsonl
cargo run -- ingest graph.jsonl --adapter dry-run
cargo run -- ingest history.graph.jsonl --adapter embedded --data-dir .aletheia-codegraph
```

## Primary Documents

- Product requirements: [docs/prd/0001-codebase-knowledge-graph.md](docs/prd/0001-codebase-knowledge-graph.md)
- Architecture decisions: [docs/adr/README.md](docs/adr/README.md)
- Implementation plans: [docs/plans/](docs/plans/)

## Development

```powershell
cargo fmt --all
cargo test --all-targets
cargo test --all-targets --no-default-features
cargo test --all-targets --features nova
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
```
