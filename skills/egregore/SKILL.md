---
name: egregore
description: This skill should be used when the user asks to "scan this codebase", "index this repo", "set up egregore", "query the code graph", "semantic search for X", "find code that does X", "what calls X", "what does this file define", "build a knowledge graph", "use egregore to understand the codebase", "check for semantic drift", or when needing to understand an unfamiliar Rust codebase before making changes. Provides the full egregore workflow from extraction through semantic search.
version: 0.1.0
---

# Egregore: Code Graph + Semantic Search

Egregore extracts a deterministic knowledge graph from a Rust codebase, stores it in an embedded AletheiaDB store, and exposes structural and semantic queries. It replaces grepping with typed graph edges (DEFINES, CALLS, IMPORTS, MENTIONS) and natural-language vector search — reducing agent context cost by 10–15× versus raw text search.

The primary binary is `egregore`; `eg` is the short alias.

## Setup Workflow

Three commands from zero to queryable. Run them once per repo (or on each significant change).

### Step 1 — Scan

```powershell
egregore scan . --out graph.jsonl
egregore inspect graph.jsonl   # verify: nodes, edges, diagnostics counts
```

Parses every `.rs` file with Tree-sitter. Produces a stable JSONL graph of files, modules, symbols, imports, diagnostics, and their relationships. Does not modify the working checkout.

For history (drift detection, point-in-time queries):

```powershell
egregore scan-history . --out history.graph.jsonl
```

### Step 2 — Ingest (structural)

```powershell
egregore ingest graph.jsonl --adapter embedded --data-dir .egregore
```

Writes the graph into a local AletheiaDB store. No server required.

### Step 3 — Ingest with embeddings (semantic)

**First time on a new machine** — prime the model cache via Python (Rust's TLS stack may not trust the OS cert store on Windows):

```powershell
pip install -U sentence-transformers
python -c "from sentence_transformers import SentenceTransformer; SentenceTransformer('sentence-transformers/all-MiniLM-L6-v2')"
```

Then ingest with embeddings:

```powershell
egregore ingest graph.jsonl --adapter embedded --data-dir .egregore-semantic --embed
```

`--embed` generates a 384-dim dense vector for every file and symbol node and stores it in an HNSW index. Use a separate `--data-dir` so the fast structural store stays lean.

## Querying

### Structural queries

Exact-match, no embedding model needed. Works against either JSONL or an ingested store.

```powershell
# Where is this symbol defined?
egregore query symbol daemon::handle_query --data-dir .egregore --format text

# What symbols does this file define?
egregore query file src/daemon.rs --data-dir .egregore --format text

# Which symbols changed the most semantically across commits?
egregore query drift --data-dir .egregore --limit 10 --format text

# What did this symbol look like at a specific commit?
egregore query symbol daemon::handle_query --data-dir .egregore --at <sha>

# Query directly from JSONL (no store required)
egregore query symbol scan_repository --graph graph.jsonl
```

### Semantic queries

Natural language. Requires a store ingested with `--embed`.

```powershell
egregore query semantic "error handling and budget limits" \
    --data-dir .egregore-semantic --format text

egregore query semantic "write nodes to database storage" \
    --data-dir .egregore-semantic --format text --limit 5
```

Results are ranked by cosine similarity. The model finds code by **concept**, not by name — `"write nodes to database storage"` surfaces `EmbeddedAletheiaSink::write_record` even though those words don't appear in the function name.

## Output Format

All queries emit newline-delimited JSON by default (machine-readable, agent-friendly). Add `--format text` for human-readable one-line-per-result output.

Example text output:
```
daemon::handle_query (Symbol) @ src/daemon.rs:1383
daemon::check_query_budget score=0.2522 @ src/daemon.rs:1469
```

## Choosing the Right Query

| Goal | Command |
|------|---------|
| Find where a symbol is defined | `query symbol <fully::qualified::name>` |
| See all symbols in a file | `query file <path>` |
| Find code by concept or description | `query semantic "<description>"` |
| See what changed most between commits | `query drift` |
| Pin a query to a specific commit | Add `--at <sha>` to any structural query |

**Structural queries** are best when you know the name. Use fully-qualified Rust paths (`module::Type::method`), not just the short name.

**Semantic queries** are best when you know the concept but not the name, or when you're exploring an unfamiliar codebase. Phrase queries as descriptions of what the code *does*, not what it *is*.

## Interpreting Results

- **score** on semantic results: cosine similarity (higher = more similar). Scores above ~0.35 are typically strong matches; 0.10–0.20 are thematically related.
- **span** (`start_line`): exact source location. Use `query symbol` with `--format json` to get machine-readable line numbers for follow-up reads.
- **record_id**: stable content-addressed ID. Use this to correlate records across runs or with the raw JSONL.

## Typical Agent Workflow

When starting work on an unfamiliar codebase or planning a refactor:

1. **Scan and ingest** once (keep `.egregore/` and `.egregore-semantic/` out of git).
2. **Semantic search** to orient: `"how is authentication handled"`, `"where is the retry logic"`.
3. **Structural queries** to zoom in: `query symbol` on the top hits, `query file` to see what else lives there.
4. **Impact analysis** before changing anything: search for callers of the target symbol in the JSONL edges (see `references/query-patterns.md`).
5. **Drift check** after a batch of commits to spot unexpected semantic changes.

For detailed patterns — impact analysis, refactoring safety checks, cross-commit comparison, architecture exploration — see `references/query-patterns.md`.

## Additional Resources

- **`references/query-patterns.md`** — Recipes for impact analysis, refactoring, semantic discovery, drift analysis, and architecture exploration.
- Project README: `C:\Users\markm\egregore\README.md` — Full setup guide with example output.
- CLI docs: `C:\Users\markm\egregore\docs\cli\query.md` — Stable output schema.
