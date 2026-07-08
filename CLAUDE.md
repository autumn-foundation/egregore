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
cargo run -- query semantic-memory "parser edge case on empty input" --data-dir .egregore
```

Query commands (local JSONL graph, no network):

```powershell
# Symbol-level cross-domain context
cargo run -- query context <symbol_name> --graph graph.jsonl

# Subsystem-scoped cross-domain context (issue #83)
cargo run -- query subsystem src/adapters --graph graph.jsonl   # exit 0 on match
cargo run -- query subsystem src/nonexistent --graph graph.jsonl  # exit 2 (no_match)
cargo run -- query subsystem "" --graph graph.jsonl               # exit 1 (malformed_prefix)

# Change-impact blast-radius leads for a symbol or file (issue #76)
cargo run -- query change-impact <symbol_name> --graph graph.jsonl        # exit 0 on match
cargo run -- query change-impact src/lib.rs --graph graph.jsonl           # file handle
cargo run -- query change-impact codegraph:v1:zzz --graph graph.jsonl     # exit 1 (Unsupported)
cargo run -- query change-impact does_not_exist --graph graph.jsonl        # exit 2 (no_match)
cargo run -- query change-impact <symbol_name> --graph graph.jsonl --depth 2  # wider neighborhood

# Symbol- and file-level deltas across a commit range (issue #118)
cargo run -- query deltas <base_sha> <head_sha> --graph history.graph.jsonl  # exit 0 on match
cargo run -- query deltas <sha> <sha> --graph history.graph.jsonl            # exit 1 (identical_endpoints)
cargo run -- query deltas ffffffffffff <head_sha> --graph history.graph.jsonl # exit 2 (missing_commit)

# Externally-reachable public API surface (issue #213)
cargo run -- query public-api --graph graph.jsonl                 # exit 0 (even when surface is empty)
cargo run -- query public-api --graph graph.jsonl --repo acme/widget  # scope one repo; bad selector exits 1
```

`eg query subsystem <prefix>` returns code facts, agent observations, project state, artifacts,
verification evidence, and semantic drift for everything under a repo-relative directory prefix.
Path matching is segment-aware: `src/alpha` never bleeds into `src/alphabet`.
Output is a deterministic JSON envelope with trust-separated sections.

`eg query change-impact <handle>` returns graph-derived impact leads grouped by relation
(`direct_callers`, `direct_callees`, `referencing_files`, `implementation_symbols`,
`containing_context`) for a symbol name, canonical record ID, or repo-relative file path.
Rows are leads to inspect before editing, not proof of breakage. The response is deterministic
and byte-identical across runs. Default depth is 1 (direct neighbors only); use `--depth 2`
for a wider BFS neighborhood. Output is a JSON envelope with an always-present disclaimer.

`eg query deltas <base> <head>` returns the observed structural deltas between two commit
handles (full SHA or unique prefix) from a `scan-history` graph or embedded store, grouped by
stable change class (`added_symbols`, `removed_symbols`, `modified_symbols`, `added_files`,
`removed_files`, `modified_files`, plus an `unresolved` diagnostic group). Renames surface as
a removed+added pair. Each row carries a stable record ID, schema version, path, span when
available, and the introducing commit with its valid time. Semantic drift inside the range is
folded in where drift records exist and marked unavailable otherwise. Rows are observed
deltas, never proof of behavior change; the response is deterministic and byte-identical
across runs. See `docs/cli/deltas.md`.

`eg query public-api` enumerates the Rust library crate's externally-reachable public API
surface from recorded per-symbol visibility (issue #124) and module containment — never a
`pub` token grep. `pub` items trapped in non-`pub` modules are excluded; `pub use` re-exports
are included and attributed to the re-export site; `pub(crate)`/`pub(super)`/`pub(in path)`
items are crate-internal and tallied, not listed. Every row carries a stable record ID plus a
repo-relative file/span handle. An empty surface is an explicit machine-readable success
(exit 0 with an `empty_surface` diagnostic), not an error. Output is deterministic and
byte-identical across runs. Parse-derived, never a build-verified or semver claim.
See `docs/cli/public-api.md`.

Protected raw-artifact commands (issue #60):

```powershell
# Preview (no writes) — compute hashes and handles only
cargo run -- protected capture --manifest evidence.jsonl --store .egregore/protected

# Enabled — store raw bytes for each entry
cargo run -- protected capture --manifest evidence.jsonl --store .egregore/protected --protected-raw-artifacts --producer op-1

# Retrieve by handle (verifies BLAKE3 hash before returning bytes)
cargo run -- protected get protected:v1:<hex> --store .egregore/protected --operator op-1 --out retrieved.txt

# List metadata (no raw bytes)
cargo run -- protected list --store .egregore/protected
```

`eg protected capture` captures raw transcript text, command output, patch bytes, task/issue
narratives, and generated reports into a local content-addressed store that the graph, query,
and semantic surfaces never read. Disabled by default; enabled with `--protected-raw-artifacts`.
After sources are moved or deleted, payloads remain retrievable by stable handle with BLAKE3
hash verification. See `docs/cli/protected-artifacts.md` for the full operator workflow.

Citation-completeness audit (issue #65):

```powershell
# Gate over a local JSONL graph (semantic reported disabled without embeddings)
cargo run -- audit citations --graph graph.jsonl        # exit 0 pass, 1 gate fail, 2 load error

# Gate over an embedded store (drives the semantic workflow too)
cargo run -- audit citations --data-dir .egregore --min-code-citation 0.95
```

`eg audit citations` drives every public query workflow over a seeded local record set and
reports, per workflow and overall, whether returned rows carry the citation handles their
trust class requires. The default gate fails when fewer than 95% of code-answer rows carry a
stable record ID plus a repo-relative file/span handle (or a documented absent-span reason),
or when any non-code trust-class row lacks a source/evidence/policy handle. Output is
deterministic and redaction-safe — record IDs, handles, hashes, markers, and counts only,
never raw payloads. See `docs/cli/citation-audit.md`.

Query-answer token-cost gate (issue #84):

```powershell
# Measure eg answer token cost vs the ripgrep baseline over a pinned corpus
cargo run -- audit token-cost                                  # exit 0 pass, 1 gate fail, 2 load error
cargo run -- audit token-cost --min-ratio 5                    # stricter savings floor
```

`eg audit token-cost` measures, per question class (exact-symbol, file-defines,
semantic) and in aggregate, the baseline-to-Egregore token-savings ratio with raw counts,
using one pinned deterministic token-count method (`word-punct-v1`) on both sides. The grep
baseline is computed in-process (no ripgrep dependency, no network). Correctness is held
constant: an answer counts only if it carries the expected record ID plus a repo-relative
file/span or commit handle; an uncited answer is a miss, not a win. A class below threshold
yields a distinct exit code and a `below_token_savings_threshold` diagnostic naming the class
and ratio. Output is deterministic and redaction-safe. See `docs/cli/token-cost.md`.

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
