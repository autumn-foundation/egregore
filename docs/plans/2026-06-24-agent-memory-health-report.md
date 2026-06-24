# YYYY-MM-DD-agent-memory-health-report Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a read-only agent-memory health report (`eg audit memory-health`) to flag reviewability risk and sludge accumulation in the `agent_memory` domain.

**Architecture:** Add a new module `src/memory_health.rs` that aggregates `Observation` nodes in a fast, pure, and deterministic single-pass audit. It checks provenance coverage, verification links, supersession/contradiction status, and dangling evidence links. Configurable thresholds gate the execution (exit code 1 on failure).

**Tech Stack:** Rust, `serde` for serialization, `TemporalResolver` for supersession/contradiction checks.

---

### Task 1: Create `src/memory_health.rs` and Register in `src/lib.rs`

**Files:**
- Create: `src/memory_health.rs`
- Modify: `src/lib.rs`

**Step 1: Write the failing test**
Create a stub or test in `tests/memory_health.rs` that tries to call `memory_health::run_memory_health_audit`.

**Step 2: Run test to verify it fails**
Run: `cargo test --test memory_health`
Expected: Compile failure (no module `memory_health`).

**Step 3: Write minimal implementation**
Register module in `src/lib.rs`.
Create `src/memory_health.rs` with:
```rust
use crate::ir::GraphRecord;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct MemoryHealthConfig {
    pub min_provenance_coverage: f64,
    pub max_dangling_evidence: f64,
    pub max_unverified: Option<f64>,
    pub max_current_guidance_contamination: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryHealthCounts {
    pub numerator: usize,
    pub denominator: usize,
    pub ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgeDistribution {
    pub oldest: Option<String>,
    pub newest: Option<String>,
    pub buckets: std::collections::BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryHealthReport {
    pub ok: bool,
    pub total_observations: usize,
    pub provenance_coverage: MemoryHealthCounts,
    pub unverified: MemoryHealthCounts,
    pub superseded: MemoryHealthCounts,
    pub contradicted: MemoryHealthCounts,
    pub dangling_evidence: MemoryHealthCounts,
    pub current_guidance_contamination: MemoryHealthCounts,
    pub missing_provenance: MemoryHealthCounts,
    pub weak_provenance: MemoryHealthCounts,
    pub unratified_memory: MemoryHealthCounts,
    pub stale_or_contradicted_memory: MemoryHealthCounts,
    pub source_transcript_concentration: std::collections::BTreeMap<String, usize>,
    pub age_distribution: AgeDistribution,
    pub diagnostics: Vec<MemoryHealthDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryHealthDiagnostic {
    pub code: String,
    pub threshold: f64,
    pub observed: f64,
    pub message: String,
}

pub fn run_memory_health_audit(
    _records: &[GraphRecord],
    _config: &MemoryHealthConfig,
) -> MemoryHealthReport {
    todo!()
}
```

**Step 4: Run test to verify it passes**
Run: `cargo test --test memory_health`
Expected: Passes (or fails on the `todo!()` inside the test instead of compilation failure).

**Step 5: Commit**
`git add src/lib.rs src/memory_health.rs`
`git commit -m "feat: register memory_health module and types"`

---

### Task 2: Implement Audit Calculations in `src/memory_health.rs`

**Files:**
- Modify: `src/memory_health.rs`

**Step 1: Write the failing test**
Add a test in `tests/memory_health.rs` with a sample fixture containing:
- 1 observation with full provenance, verified
- 1 observation missing provenance (no `session_id`, `agent_id`, or `source_handle`)
- 1 observation superseded
- 1 observation contradicted
- 1 observation with dangling evidence link (target not in graph)
Assert that the report correctly counts them.

**Step 2: Run test to verify it fails**
Run: `cargo test --test memory_health`
Expected: panic on `todo!()` in `run_memory_health_audit`.

**Step 3: Write minimal implementation**
Implement `run_memory_health_audit`:
- Collect observations.
- Build index/maps.
- Iterate and check properties.
- Check thresholds and populate `diagnostics` and `ok`.

**Step 4: Run test to verify it passes**
Run: `cargo test --test memory_health`
Expected: PASS.

**Step 5: Commit**
`git add src/memory_health.rs`
`git commit -m "feat: implement memory health audit calculations"`

---

### Task 3: CLI Subcommand Integration

**Files:**
- Modify: `src/cli.rs`

**Step 1: Write the failing test**
Create a test in `tests/memory_health.rs` that executes the CLI command:
`cargo run -- audit memory-health --graph <nonexistent>`
Assert exit code is 2.

`cargo run -- audit memory-health --graph <valid_graph>`
Assert exit code is 0 (or 1 depending on thresholds).

**Step 2: Run test to verify it fails**
Run: `cargo test --test memory_health`
Expected: Failure because `audit memory-health` is not a recognized subcommand.

**Step 3: Write minimal implementation**
Register `MemoryHealth` variant in `AuditSubcommand`.
Implement `audit_memory_health_cmd` in `src/cli.rs`.

**Step 4: Run test to verify it passes**
Run: `cargo test --test memory_health`
Expected: PASS.

**Step 5: Commit**
`git add src/cli.rs`
`git commit -m "feat: integrate memory-health subcommand in CLI"`

---

### Task 4: Add Documentation

**Files:**
- Create: `docs/cli/memory-health.md`

**Step 1: Create the markdown documentation**
Document how to run `eg audit memory-health`, the metrics computed, the exit codes, and how it differs from other query/audit tools.

**Step 2: Commit**
`git add docs/cli/memory-health.md`
`git commit -m "docs: add agent-memory-health CLI documentation"`
