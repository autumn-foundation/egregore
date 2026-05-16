# Codebase Knowledge Graph Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the PRD MVP for deterministic Rust code graph extraction, Git history replay, JSONL inspection, embedded bi-temporal AletheiaDB ingestion, incremental re-indexing, and semantic drift analysis through AletheiaDB's embedding and semantic features.

**Architecture:** The parser emits a stable intermediate representation only; persistence lives behind adapters. Current-tree scans and Git-history scans share the same IR, with commit metadata attached during history replay. The primary persistence path embeds AletheiaDB through its public Rust API, using valid time for Git commit time and transaction time for ingestion observations.

**Tech Stack:** Rust 2024, Tree-sitter Rust, Git object/tree reads, clap, serde JSONL, blake3, thiserror/anyhow, AletheiaDB `semantic-search`, `semantic-temporal`, `semantic-diagnostics`, optional AletheiaDB `embeddings`/`nova`.

---

### Task 1: Deterministic IR And Snapshot Surface

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Create: `src/error.rs`
- Create: `src/ir.rs`
- Create: `tests/fixtures/rust_basic/src/lib.rs`
- Create: `tests/deterministic_scan.rs`

**Step 1: Write failing tests**

Add a fixture test that scans the same Rust fixture twice and asserts byte-for-byte identical JSONL with repo-relative paths and no absolute workspace path.

**Step 2: Verify red**

Run: `cargo test deterministic_scan_produces_stable_jsonl --test deterministic_scan`
Expected: FAIL because scan APIs and IR do not exist.

**Step 3: Implement minimal IR and scan API**

Add graph record types, stable IDs, deterministic sorting, and a `scan_repository` API that can produce repository and file records.

**Step 4: Verify green**

Run: `cargo test deterministic_scan_produces_stable_jsonl --test deterministic_scan`
Expected: PASS.

### Task 2: Rust Tree-sitter Symbol Coverage

**Files:**
- Modify: `Cargo.toml`
- Create: `src/fs.rs`
- Create: `src/parser.rs`
- Create: `src/languages/mod.rs`
- Create: `src/languages/rust.rs`
- Modify: `tests/fixtures/rust_basic/src/lib.rs`
- Modify: `tests/deterministic_scan.rs`

**Step 1: Write failing tests**

Assert extraction for modules, functions, structs, enums, traits, impl blocks, methods, constants, statics, type aliases, tests, imports, and macro diagnostics.

**Step 2: Verify red**

Run: `cargo test rust_fixture_covers_common_symbols --test deterministic_scan`
Expected: FAIL because Tree-sitter extraction is not implemented.

**Step 3: Implement minimal extractor**

Walk Tree-sitter Rust syntax, emit Module, Symbol, Import, Diagnostic nodes plus CONTAINS, DEFINES, IMPORTS, IMPLEMENTS, CALLS, MENTIONS edges where syntactically visible.

**Step 4: Verify green**

Run: `cargo test rust_fixture_covers_common_symbols --test deterministic_scan`
Expected: PASS.

### Task 3: CLI Scan And Inspect

**Files:**
- Modify: `Cargo.toml`
- Create: `src/cli.rs`
- Create: `src/main.rs`
- Create: `tests/cli.rs`

**Step 1: Write failing tests**

Use `assert_cmd` to run `aletheia-codegraph scan <fixture> --out graph.jsonl` and `inspect graph.jsonl`.

**Step 2: Verify red**

Run: `cargo test scan_and_inspect_work_without_aletheiadb --test cli`
Expected: FAIL because the binary does not exist.

**Step 3: Implement CLI**

Use clap subcommands. `scan` writes deterministic JSONL. `inspect` prints record, node, edge, tombstone, and diagnostic counts to stdout and errors to stderr.

**Step 4: Verify green**

Run: `cargo test scan_and_inspect_work_without_aletheiadb --test cli`
Expected: PASS.

### Task 4: Adapter Boundary And Safe Embedded Ingest Semantics

**Files:**
- Create: `src/adapters/mod.rs`
- Create: `src/adapters/aletheiadb.rs`
- Modify: `src/cli.rs`
- Create: `tests/ingest.rs`

**Step 1: Write failing tests**

Test fake adapter success, partial failure reporting, input preservation, embedded-store read-back semantics, and CLI `ingest graph.jsonl --adapter dry-run`.

**Step 2: Verify red**

Run: `cargo test adapter_reports_partial_success --test ingest`
Expected: FAIL because adapters are missing.

**Step 3: Implement boundary**

Define a graph sink trait, fake/dry-run sink, and an embedded AletheiaDB sink that writes through public AletheiaDB APIs and reads records back before reporting success.

**Step 4: Verify green**

Run: `cargo test adapter_reports_partial_success --test ingest`
Expected: PASS.

### Task 5: Incremental Cache

**Files:**
- Create: `src/incremental.rs`
- Modify: `src/fs.rs`
- Modify: `src/cli.rs`
- Create: `tests/incremental.rs`

**Step 1: Write failing tests**

Test unchanged files are reused, changed files are rebuilt, and removed files emit tombstones.

**Step 2: Verify red**

Run: `cargo test incremental_reuses_unchanged_files_and_tombstones_removed_files --test incremental`
Expected: FAIL because no cache exists.

**Step 3: Implement cache**

Store per-file hashes and records in JSON. Reuse unchanged file records, rebuild changed files, and emit deterministic file tombstones.

**Step 4: Verify green**

Run: `cargo test incremental_reuses_unchanged_files_and_tombstones_removed_files --test incremental`
Expected: PASS.

### Task 6: Semantic Enrichment Boundary

**Files:**
- Modify: `Cargo.toml`
- Create: `src/embeddings.rs`
- Create: `tests/embeddings.rs`

**Step 1: Write failing tests**

Assert embedding candidates are generated for symbol summaries and file summaries without requiring model downloads.

**Step 2: Verify red**

Run: `cargo test embedding_candidates_target_agent_useful_handles --test embeddings`
Expected: FAIL because semantic enrichment is missing.

**Step 3: Implement embed-anything integration boundary**

Add optional `embeddings` feature that enables AletheiaDB's `embeddings` feature and re-exports AletheiaDB's embedding module. Do not depend on `embed_anything` directly from Codegraph.

**Step 4: Verify green**

Run: `cargo test embedding_candidates_target_agent_useful_handles --test embeddings`
Expected: PASS.

### Task 7: Git History Replay

**Files:**
- Modify: `Cargo.toml`
- Create: `src/history.rs`
- Modify: `src/ir.rs`
- Create: `tests/history.rs`
- Create fixture repo inside the test using `git`

**Step 1: Write failing tests**

Create a temporary Git repo with three commits:
- commit 1 creates `src/lib.rs` with one public function
- commit 2 changes that function and adds a helper
- commit 3 removes or renames a file

Assert `scan_repository_history(&repo)` emits deterministic `Commit`, `Change`, `PARENT_OF`, and commit-backed file/symbol records with `git_commit` and `valid_time` metadata. Assert the working tree HEAD and contents are unchanged after the history scan.

**Step 2: Verify red**

Run: `cargo test git_history_replay_emits_bitemporal_records_without_mutating_checkout --test history`
Expected: FAIL because Git history replay APIs do not exist.

**Step 3: Implement minimal history replay**

Use Git object reads through `git show`, `git ls-tree`, and `git diff-tree` to list commits in deterministic oldest-to-newest topological order without mutating the active checkout. Scan each source blob, attach commit metadata to records, and emit commit, change, ancestry, and changed-in records.

**Step 4: Verify green**

Run: `cargo test git_history_replay_emits_bitemporal_records_without_mutating_checkout --test history`
Expected: PASS.

### Task 8: Bi-Temporal Embedded Ingestion

**Files:**
- Modify: `src/adapters/aletheiadb.rs`
- Modify: `src/cli.rs`
- Create or extend: `tests/ingest.rs`

**Step 1: Write failing tests**

Assert `aletheia-codegraph ingest history.graph.jsonl --adapter embedded --data-dir <tmp>` stores commit-backed records and can traverse `Commit -> Change -> Symbol` or equivalent temporal handles from the embedded store.

**Step 2: Verify red**

Run: `cargo test embedded_history_ingest_traverses_commit_change_symbol --test ingest`
Expected: FAIL until embedded ingest maps commit/valid-time metadata.

**Step 3: Implement temporal mapping**

Map Git commit time into AletheiaDB valid-time fields or properties available through the public API, preserve ingestion transaction-time observation, and read back each record before reporting success.

**Step 4: Verify green**

Run: `cargo test embedded_history_ingest_traverses_commit_change_symbol --test ingest`
Expected: PASS.

### Task 9: Semantic Drift

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/embeddings.rs`
- Modify: `tests/embeddings.rs`

**Step 1: Write failing tests**

Create two versions of the same symbol across commits and assert drift candidates compare the same logical symbol across before/after commits. The test must not download models; it can use deterministic fake vectors to prove score, model ID, before commit, after commit, and summary are preserved.

**Step 2: Verify red**

Run: `cargo test semantic_drift_records_capture_vector_movement_between_commits --test embeddings`
Expected: FAIL because drift records do not exist.

**Step 3: Implement drift record generation**

Enable AletheiaDB semantic features through Codegraph features. Generate `SemanticDrift` graph records from embedding candidate pairs and vector distance scores. Preserve model ID, target entity ID, before/after commit SHAs, valid-time range, score, and explanation summary.

**Step 4: Verify green**

Run: `cargo test semantic_drift_records_capture_vector_movement_between_commits --test embeddings`
Expected: PASS.

### Task 10: Agent Query Helpers

**Files:**
- Create: `src/query.rs`
- Create: `tests/query.rs`
- Modify: `src/lib.rs`

**Step 1: Write failing tests**

Assert graph records can answer "symbol at commit" and rank semantic drift nodes by score for largest-drift demos.

**Step 2: Verify red**

Run: `cargo test symbol_at_commit_returns_the_temporal_symbol_record --test query`
Expected: FAIL because query helpers do not exist.

**Step 3: Implement query helpers**

Add graph-only query helpers for symbol lookup by commit SHA/prefix and largest semantic drift ranking.

**Step 4: Verify green**

Run: `cargo test --test query`
Expected: PASS.

### Task 11: Completion Audit

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md` if workflow changed

**Step 1: Run verification**

Run:
- `cargo fmt --all`
- `cargo test --all-targets`
- `cargo test --all-targets --no-default-features`
- `cargo test --all-targets --features nova`
- `cargo test --all-targets --all-features`
- `cargo clippy --all-targets --all-features -- -D warnings`

**Step 2: Audit PRD**

Map each PRD requirement to code, tests, or documented out-of-scope follow-up. Fix any uncovered MVP behavior before claiming completion.
