# Egregore

Egregore is an `AletheiaDB`-backed knowledge graph substrate for agentic software engineering. It connects deterministic code facts, agent memory, project state, artifacts, and verification evidence in one temporal graph.

The current implemented slice is the code graph domain: it parses Rust with Tree-sitter, emits stable JSONL graph records, replays Git history without mutating the working checkout, ingests records into an embedded AletheiaDB store, and exposes structural and semantic query helpers for agent workflows.

## Getting Started

This section walks you from a fresh clone to a fully queryable semantic search index of your codebase.

### 1. Prerequisites

- **Rust** (stable, edition 2024) — install via [rustup](https://rustup.rs)
- **Git** — for history replay
- **Python 3.8+** with **pip** — required once to download the embedding model

On MSVC (Windows), `.cargo/config.toml` sets `CXXFLAGS_x86_64_pc_windows_msvc=/MT` to align the C++ runtime used by the `embed_anything` / tokenizers native stack. This is already committed; no manual action needed.

### 2. Build

```powershell
cargo build --release
```

The release binary lands at `target/release/egregore.exe` (and `eg.exe` as a short alias). During development, substitute `cargo run --` for `egregore`.

### 3. Scan your codebase

Point `scan` at any Rust repository. It parses every `.rs` file with Tree-sitter and emits a deterministic JSONL graph of nodes (files, modules, symbols, imports, diagnostics) and edges (DEFINES, CALLS, IMPORTS, MENTIONS, CONTAINS).

```powershell
egregore scan . --out graph.jsonl
egregore inspect graph.jsonl
# records: 4386   nodes: 1353   edges: 3033   tombstones: 0   diagnostics: 599
```

### 4. Ingest into AletheiaDB (structural index)

```powershell
egregore ingest graph.jsonl --adapter embedded --data-dir .egregore
```

This writes the graph into a local AletheiaDB store. The store is self-contained in `.egregore/` and does not require a running server.

### 5. Verify setup before semantic work (`eg doctor`)

Before the semantic embedding step, run the setup preflight to confirm this machine
has the tools and permissions needed for the next steps:

```powershell
egregore doctor .
```

Exit 0 means the structural workflow (scan, ingest) is ready. `semantic_ready: true`
in the JSON output means Python and the embedding model cache are also in place and
you can skip to step 7. Consult the `next_command` field in the output for the
shortest path forward.

```powershell
# Human-readable version
egregore doctor . --format text
```

> **Note:** `eg doctor` is a _setup_ preflight — it runs before ingest and checks
> whether your local tools and permissions are correct. It is distinct from the
> post-ingest semantic index readiness report (issue #71), which checks whether an
> already-ingested store has adequate embedding coverage. See
> [docs/cli/doctor.md](docs/cli/doctor.md) for the full check matrix, exit codes,
> and the distinction between the two reports.

### 6. Download the embedding model (first time only)

The semantic search feature uses `sentence-transformers/all-MiniLM-L6-v2` (384 dimensions, ~90 MB). Rust's TLS stack may not share your OS certificate store, so the most reliable way to prime the cache is via Python:

```powershell
pip install -U sentence-transformers
python -c "from sentence_transformers import SentenceTransformer; SentenceTransformer('sentence-transformers/all-MiniLM-L6-v2')"
```

This downloads the model to `~/.cache/huggingface/hub/`, where Rust's `hf-hub` will find it on subsequent runs. You only need to do this once per machine.

### 7. Ingest with semantic embeddings

```powershell
egregore ingest graph.jsonl --adapter embedded --data-dir .egregore-semantic --embed
# Generating embeddings for 650 file/symbol nodes…
# attempted: 4386   succeeded: 4386   failed: 0
```

The `--embed` flag generates a 384-dimensional dense vector for every file and symbol node and stores it in AletheiaDB's HNSW vector index. The store at `.egregore-semantic/` supports both structural and semantic queries.

> **Tip:** Use a separate `--data-dir` for the embedded store so you can keep a fast structural-only store alongside the larger semantic one.

### 8. Query

**Structural queries** (exact name, file, or drift — works with or without `--embed`):

```powershell
# Find a symbol by fully-qualified name
egregore query symbol daemon::handle_query --data-dir .egregore --format text

# List all symbols defined in a file
egregore query file src/daemon.rs --data-dir .egregore --format text

# Show semantic drift records ranked by change score
egregore query drift --data-dir .egregore --limit 10 --format text

# Pin a symbol query to a specific commit
egregore query symbol daemon::handle_query --data-dir .egregore --at <commit-sha>

# Scope a query to one repository in a shared multi-repo store
egregore query symbol handle_query --data-dir .egregore --repo owner/name
```

**Semantic queries** (natural language — requires `--embed` store):

```powershell
# Default output is JSON; pass --format text for human-readable lines
egregore query semantic "write nodes to database storage" --data-dir .egregore-semantic
egregore query semantic "error handling and budget limits" --data-dir .egregore-semantic --limit 5
egregore query semantic "temporal history git commit tracking" --data-dir .egregore-semantic --format text
```

Agent-citable JSON result (stable fields: `record_id`, `score`, `name`, `repo_relative_path`, `span`):

```json
{"record_id":"codegraph:v1:abc123","name":"EmbeddedAletheiaSink::write_record","repo_relative_path":"src/sink/embedded.rs","score":0.9231,"span":{"start_byte":4096,"end_byte":5200,"start_line":142,"end_line":168}}
```

Human-readable text output (`--format text`, not a stable parsing contract):

```
history::GitCommit score=0.4974 @ src/history.rs:138
history::list_commits score=0.4767 @ src/history.rs:168
history::git_output score=0.4610 @ src/history.rs:312
```

The semantic index finds code by **meaning**, not by name. Querying `"write nodes to database storage"` surfaces `EmbeddedAletheiaSink::write_record` even though none of those words appear in the function name.

See [docs/cli/query.md](docs/cli/query.md) for the full JSON output contract, no-result exit codes, and operator actions.

---

## Why not just use ripgrep?

Structural and semantic queries return a typed, citable handle instead of every
line a text search would make an agent ingest — no false positives from comments
or string literals, and no bodies to read just to learn what a file defines.

That saving is **measured, not asserted**. On the pinned token-cost corpus
(`corpus/token_cost_corpus/`), counting answer tokens with one deterministic
method on both sides, the baseline-to-Egregore ratios are:

| Question | `eg` answer vs. `rg` baseline | Ratio |
|----------|-------------------------------|-------|
| Exact symbol lookup | `eg query symbol` vs. `rg -w <name>` | ~4.8× cheaper |
| File-defines lookup | `eg query file` vs. reading the file | ~3.6× cheaper |
| Semantic concept query | `eg query semantic` vs. `rg <keyword>` | ~5.2× cheaper |
| **Aggregate** | | **~4× cheaper** |

Reproduce it with `eg audit token-cost`; the gate fails if any class regresses
below 3×. The multiplier scales with the corpus — the more comment and
string-literal false positives a text search drags in, and the larger the file
bodies a structural answer summarizes, the larger the saving. See
[docs/cli/token-cost.md](docs/cli/token-cost.md).

---

## All commands

```powershell
egregore scan . --out graph.jsonl                          # current-tree extraction
egregore scan-history . --out history.graph.jsonl          # commit-by-commit temporal extraction
egregore inspect graph.jsonl                               # graph summary
egregore ingest graph.jsonl --adapter dry-run              # validate without writing
egregore ingest graph.jsonl --adapter embedded \
    --data-dir .egregore                                   # structural store
egregore ingest graph.jsonl --adapter embedded \
    --data-dir .egregore-semantic --embed                  # structural + semantic store
egregore query symbol <name>  --data-dir .egregore
egregore query file   <path>  --data-dir .egregore
egregore query drift          --data-dir .egregore
egregore query semantic <text> --data-dir .egregore-semantic
egregore import-codex session.jsonl --out agent.graph.jsonl  # Codex session/rollout import
# eg import github <owner>/<repo> --out github.jsonl          # planned — see docs/schema/import-github.md
```

---

## Status

Implemented surfaces:

- `scan` — current-tree JSONL extraction (Rust via Tree-sitter)
- `scan-history` — commit-by-commit temporal extraction
- `inspect` — graph summary
- `ingest` — dry-run, embedded AletheiaDB, and daemon adapters
- `import-codex` — Codex session/rollout JSONL → agent-memory graph records (M3)
- `query symbol / file / drift / semantic` — structural and semantic agent queries
- Incremental file-cache planning with tombstones
- AletheiaDB embedding re-export through the optional `embeddings` feature
- Semantic drift records and query helpers for symbol-at-commit and largest-drift workflows
- HNSW vector index via `embeddings` feature (enabled by default)

`embedded-aletheiadb` is enabled by default and uses the published `aletheiadb` crate with `semantic-search`, `semantic-temporal`, and `semantic-diagnostics`.

On MSVC, this repo sets `CXXFLAGS_x86_64_pc_windows_msvc=/MT` in `.cargo/config.toml` so AletheiaDB's optional `embeddings` feature can link the transitive `embed_anything`/tokenizers native C++ stack consistently.

## Usage

```powershell
cargo run -- scan . --out graph.jsonl
cargo run -- scan-history . --out history.graph.jsonl
cargo run -- inspect graph.jsonl
cargo run -- ingest graph.jsonl --adapter dry-run
cargo run -- ingest history.graph.jsonl --adapter embedded --data-dir .egregore

# Import a rust-swe-agent / Maxwell's Daemon .traj file as agent-memory records
cargo run -- import-traj run.traj --out agent-memory.jsonl
cargo run -- ingest agent-memory.jsonl --adapter embedded --data-dir .egregore

# Query from a JSONL graph (where-is-this-symbol, what-does-this-file-define, when-did-this-drift)
eg query symbol scan_repository --graph graph.jsonl
eg query file src/lib.rs --graph graph.jsonl
eg query drift --graph history.graph.jsonl --limit 5
eg query symbol scan_repository --graph history.graph.jsonl --at <commit-sha>
eg query symbol scan_repository --graph history.graph.jsonl --as-of 2026-05-01T00:00:00Z

# Query directly from an ingested AletheiaDB store
eg query symbol scan_repository --data-dir .egregore
eg query file src/lib.rs --data-dir .egregore
eg query drift --data-dir .egregore --limit 5

# Flag agent notes whose cited code has drifted since recording (freshness lead, not a truth claim)
eg query evidence-freshness --graph history.graph.jsonl --stale-only
```

The primary binary is `egregore`; `eg` is also built as a short CLI alias.

Query output is newline-delimited JSON by default (`--format json`). Pass `--format text` for human-readable terminal output. See [docs/cli/query.md](docs/cli/query.md) for the stable output schema.

## Primary Documents

- Product vision: [docs/prd/0000-egregore-vision.md](docs/prd/0000-egregore-vision.md)
- Code graph requirements: [docs/prd/0001-codebase-knowledge-graph.md](docs/prd/0001-codebase-knowledge-graph.md)
- **Agent-memory domain schema (v1):** [docs/schema/agent-memory.md](docs/schema/agent-memory.md)
- **Agent-actions and PatchArtifact schema (v1):** [docs/schema/agent-actions.md](docs/schema/agent-actions.md)
- **Verification-evidence domain schema (v1):** [docs/schema/verification.md](docs/schema/verification.md)
- **Project-graph domain schema (v1):** [docs/schema/project-graph.md](docs/schema/project-graph.md)
- **User-context domain schema (v1):** [docs/schema/user-context.md](docs/schema/user-context.md)
- **Local project/task JSONL file format (v1):** [docs/schema/local-project-jsonl.md](docs/schema/local-project-jsonl.md)
- **Semantic drift domain schema (v1):** [docs/schema/semantic-drift.md](docs/schema/semantic-drift.md)
- **Record schema-version policy:** [docs/schema/schema-versioning.md](docs/schema/schema-versioning.md)
- **Producer identity envelope (v1):** [docs/schema/producer-version.md](docs/schema/producer-version.md)
- **Redaction schema (v1):** [docs/schema/redaction.md](docs/schema/redaction.md)
- **Bi-temporal selector grammar (v2):** [docs/schema/temporal-selectors.md](docs/schema/temporal-selectors.md)
- **Symbol identity ADR:** [docs/adr/0004-symbol-identity.md](docs/adr/0004-symbol-identity.md)
- Architecture decisions: [docs/adr/README.md](docs/adr/README.md)
- **Daemon runtime discovery contract (v1):** [docs/schema/daemon-runtime.md](docs/schema/daemon-runtime.md)
- **Daemon HTTP wire contract (v1):** [docs/schema/daemon-api.md](docs/schema/daemon-api.md)
- **Daemon query verb set (v1):** [docs/schema/daemon-query.md](docs/schema/daemon-query.md)
- **Repository node identity (v2):** [docs/schema/repository-identity.md](docs/schema/repository-identity.md)
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
