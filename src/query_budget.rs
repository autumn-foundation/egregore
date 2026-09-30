//! Structural query latency budget and scaling regression gate (issue #120).
//!
//! This module is the **measurement layer** behind `eg audit query-budget`.
//! Where issue #255 (`eg audit query-latency`) gates cold p50 for
//! `eg query symbol` alone, this benchmark covers the agent-critical
//! structural path — `eg query symbol`, `eg query file`, and
//! `eg query drift` — with cold **and** warm invocations against a pinned
//! fixture store at two sizes (1x and 10x record counts), records a ripgrep
//! baseline for the equivalent symbol lookup, and enforces the scaling
//! assertion: a targeted single-symbol lookup must grow sub-linearly with
//! total store record count (10x the records costs at most 2x the latency).
//!
//! The absolute p50/p95 budgets are **advisory**: they are reported per
//! cell so regressions are visible, but they never fail the gate — only the
//! relative scaling assertion (machine-independent) gates. This keeps CI
//! green across hardware variance while still catching algorithmic
//! regressions (a linear scan where an index belongs).
//!
//! Measurement definitions:
//! - *Cold*: a fresh process per sample, timed from just before spawn to the
//!   first non-empty stdout line — process start is included, OS page cache
//!   is the only warm state (same contract as issue #255).
//! - *Warm*: a fresh process per sample run immediately after one unmeasured
//!   priming invocation of the identical query — the process still
//!   cold-starts, so warm isolates hot page-cache / filesystem effects.
//!
//! The orchestration (corpus build, subprocess spawning) lives in
//! `crate::cli::audit`; everything here is pure and deterministic except the
//! re-exported [`crate::query_latency::measure_cold_query`], which spawns
//! exactly one child process and times it. Percentiles use linear
//! interpolation over sorted samples.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ir::{
    EmbeddingModel, GraphRecord, MetricKind, NodeKind, SEMANTIC_SCHEMA_VERSION, SelectionBasis,
    SemanticDriftMetadata, semantic_stable_id,
};
use crate::query_latency::{MachineInfo, percentile};

/// The pinned benchmark corpus manifest, deserialized from
/// `corpus/query_budget_corpus.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryBudgetCorpus {
    /// Schema version for the manifest format.
    pub corpus_version: String,
    /// Stable human-readable corpus name reported for interpretation.
    pub corpus_name: String,
    /// Description of the corpus scope.
    pub description: String,
    /// Corpus source directory, resolved relative to the manifest's parent dir.
    pub source_dir: String,
    /// Repository identity base for the deterministic scans. Scale copies
    /// append `-sNN` so every copy's record IDs stay disjoint.
    pub repository_id_override: String,
    /// Fixed scan transaction time so the scan is deterministic.
    pub scan_time: String,
    /// Symbol name the benchmark queries (must match >= 1 record per copy).
    pub query_symbol: String,
    /// Repository-relative file path the benchmark queries (must match >= 1
    /// record per copy).
    pub query_file: String,
    /// `--limit` for the benchmarked `eg query drift` invocation.
    pub drift_limit: usize,
    /// Deterministic synthetic `SemanticDrift` nodes appended per repo copy
    /// so `eg query drift` has a stable answer on the fixture.
    pub synthetic_drift_nodes_per_repo: usize,
    /// Store-size multiplier for the scaling assertion (10x).
    pub scale_factor: usize,
    /// Cold samples measured per query x temperature x size cell.
    pub samples: usize,
    /// Warm samples measured per query x size cell.
    pub warm_samples: usize,
    /// Scaling ceiling: the 10x store must answer within this multiple of
    /// the 1x p50.
    pub max_scaling_ratio: f64,
    /// Advisory absolute p50 budget in milliseconds (reported, not gated).
    pub budget_p50_ms: f64,
    /// Advisory absolute p95 budget in milliseconds (reported, not gated).
    pub budget_p95_ms: f64,
    /// Record count measured when the corpus was pinned (auditability).
    pub reference_record_count: u64,
    /// Named machine class the advisory budgets are defined against.
    pub reference_machine_class: String,
}

/// Minimum record count that keeps the reference corpus representative. The
/// benchmark refuses to measure on a collapsed corpus instead of gating a
/// fast-but-meaningless number.
pub const MIN_REFERENCE_RECORDS: u64 = 8000;

/// Builds deterministic synthetic `SemanticDrift` node records.
///
/// A plain deterministic scan of the fixture corpus produces no drift nodes
/// (drift is an embeddings-over-history product), so `eg query drift` would
/// have no answer to time. These fixture nodes give the drift query a
/// stable, deterministic answer instead: the first `count` symbol records by
/// ID each get one drift node with a strictly descending score, so the
/// ranked answer is byte-stable across runs. Node IDs embed `repo_tag`,
/// keeping per-copy nodes disjoint when the corpus is replicated for the
/// scaled store.
#[must_use]
#[allow(clippy::cast_precision_loss)]
// SAFETY: `index` is bounded by `synthetic_drift_nodes_per_repo` (tens), far
// below the 2^53 exact-representation limit for f64.
pub fn synthetic_drift_records(
    records: &[GraphRecord],
    repo_tag: &str,
    scan_time: &str,
    count: usize,
) -> Vec<GraphRecord> {
    let mut symbols: Vec<&GraphRecord> = records
        .iter()
        .filter(|record| record.node_kind_ref() == Some(NodeKind::Symbol))
        .collect();
    symbols.sort_by(|left, right| left.id().cmp(right.id()));
    symbols
        .into_iter()
        .take(count)
        .enumerate()
        .map(|(index, target)| {
            let target_id = target.id().to_owned();
            let (repo_relative_path, name) = match target {
                GraphRecord::Node {
                    repo_relative_path,
                    name,
                    ..
                } => (repo_relative_path.clone(), name.clone()),
                GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => (None, None),
            };
            // Scores descend deterministically so the ranked drift answer is
            // stable; all stay above the 0.5 selection threshold.
            let score = (index as f64).mul_add(-0.01, 0.95);
            let drift_id = semantic_stable_id(&[
                "query-budget",
                "synthetic-drift",
                repo_tag,
                &target_id,
                &index.to_string(),
            ]);
            let drift = SemanticDriftMetadata {
                embedding_model: EmbeddingModel {
                    provider: "query-budget-fixture".to_owned(),
                    name: "synthetic".to_owned(),
                    version: "1".to_owned(),
                    dim: 8,
                    content_hash: "fixture".to_owned(),
                },
                target_record_id: target_id.clone(),
                prior_record_id: target_id.clone(),
                before_git_commit: "fixture-before".to_owned(),
                after_git_commit: "fixture-after".to_owned(),
                before_valid_time: scan_time.to_owned(),
                after_valid_time: scan_time.to_owned(),
                metric_kind: MetricKind::CosineDistance,
                score,
                selection_threshold: 0.5,
                selection_basis: SelectionBasis::ThresholdOnly,
            };
            GraphRecord::node(
                drift_id,
                NodeKind::SemanticDrift,
                repo_relative_path,
                None,
                name,
                format!(
                    "Synthetic semantic drift fixture for the issue #120 query budget: {target_id} scored {score:.2}"
                ),
            )
            .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
            .with_semantic_drift(drift)
        })
        .collect()
}

/// Sub-linearity ratio: how much slower the scaled store answers than the
/// 1x store.
///
/// Returns infinity when the 1x p50 is not positive — a zero-time 1x
/// measurement cannot justify any scaled-store time, so the gate fails
/// closed instead of dividing by zero.
#[must_use]
pub fn scaling_ratio(p50_base_ms: f64, p50_scaled_ms: f64) -> f64 {
    if p50_base_ms > 0.0 {
        p50_scaled_ms / p50_base_ms
    } else {
        f64::INFINITY
    }
}

/// Whether a scaling ratio passes the gate: finite and within the ceiling.
#[must_use]
pub fn scaling_passes(ratio: f64, max_ratio: f64) -> bool {
    ratio.is_finite() && ratio <= max_ratio
}

/// One measured query x temperature x store-size cell.
#[derive(Debug, Clone, Serialize)]
pub struct QueryCell {
    /// Query name: `"symbol"`, `"file"`, or `"drift"`.
    pub query: String,
    /// Invocation temperature: `"cold"` or `"warm"`.
    pub temp: String,
    /// Store size: `"store_1x"` or `"store_10x"`.
    pub size: String,
    /// Every sample in milliseconds, ascending.
    pub samples_ms: Vec<f64>,
    /// Median latency in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: f64,
    /// Fastest sample in milliseconds.
    pub min_ms: f64,
    /// Slowest sample in milliseconds.
    pub max_ms: f64,
    /// Records in the store this cell measured against.
    pub record_count: u64,
}

/// Summarizes ascending samples into a [`QueryCell`]. Returns `None` when
/// there are no samples.
#[must_use]
pub fn summarize_cell(
    query: &str,
    temp: &str,
    size: &str,
    mut samples_ms: Vec<f64>,
    record_count: u64,
) -> Option<QueryCell> {
    if samples_ms.is_empty() {
        return None;
    }
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(QueryCell {
        query: query.to_owned(),
        temp: temp.to_owned(),
        size: size.to_owned(),
        p50_ms: percentile(&samples_ms, 50.0)?,
        p95_ms: percentile(&samples_ms, 95.0)?,
        min_ms: samples_ms[0],
        max_ms: samples_ms[samples_ms.len() - 1],
        samples_ms,
        record_count,
    })
}

/// The ripgrep baseline: wall-clock for the equivalent symbol lookup
/// (`rg <symbol> <corpus source>`).
///
/// Measured with the same spawn-to-first-line harness so the comparison to
/// the boring substitute is explicit.
#[derive(Debug, Clone, Serialize)]
pub struct RipgrepBaseline {
    /// Exact command measured.
    pub command: String,
    /// Every sample in milliseconds, ascending.
    pub samples_ms: Vec<f64>,
    /// Median latency in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: f64,
    /// Fastest sample in milliseconds.
    pub min_ms: f64,
    /// Slowest sample in milliseconds.
    pub max_ms: f64,
    /// Advisory absolute p50 budget in milliseconds (reported, not gated).
    pub budget_p50_ms: f64,
    /// Advisory absolute p95 budget in milliseconds (reported, not gated).
    pub budget_p95_ms: f64,
    /// True when ripgrep was unavailable and the baseline was not measured.
    pub skipped: bool,
    /// Why the baseline was skipped (present only when `skipped`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// Builds a measured [`RipgrepBaseline`], or an explicit skip record when
/// ripgrep is not on `PATH` — never silently absent.
#[must_use]
pub fn summarize_ripgrep(
    command: &str,
    mut samples_ms: Vec<f64>,
    budget_p50_ms: f64,
    budget_p95_ms: f64,
) -> RipgrepBaseline {
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (p50_ms, p95_ms, min_ms, max_ms) = if samples_ms.is_empty() {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        (
            percentile(&samples_ms, 50.0).unwrap_or(0.0),
            percentile(&samples_ms, 95.0).unwrap_or(0.0),
            samples_ms[0],
            samples_ms[samples_ms.len() - 1],
        )
    };
    RipgrepBaseline {
        command: command.to_owned(),
        samples_ms,
        p50_ms,
        p95_ms,
        min_ms,
        max_ms,
        budget_p50_ms,
        budget_p95_ms,
        skipped: false,
        skip_reason: None,
    }
}

/// Builds an explicit ripgrep skip record.
#[must_use]
pub fn skipped_ripgrep(
    command: &str,
    budget_p50_ms: f64,
    budget_p95_ms: f64,
    reason: &str,
) -> RipgrepBaseline {
    RipgrepBaseline {
        command: command.to_owned(),
        samples_ms: Vec::new(),
        p50_ms: 0.0,
        p95_ms: 0.0,
        min_ms: 0.0,
        max_ms: 0.0,
        budget_p50_ms,
        budget_p95_ms,
        skipped: true,
        skip_reason: Some(reason.to_owned()),
    }
}

/// The scaling assertion result: the relative, machine-independent gate.
#[derive(Debug, Clone, Serialize)]
pub struct ScalingAssertion {
    /// Gated query (`"symbol"` — the targeted single-symbol lookup).
    pub query: String,
    /// Gated temperature (`"cold"` — the agent edit-loop path).
    pub temp: String,
    /// `p50_10x / p50_1x`: the measured sub-linearity ratio.
    pub ratio_p50: f64,
    /// `p95_10x / p95_1x`, reported for diagnosis.
    pub ratio_p95: f64,
    /// Ceiling the ratio must stay within.
    pub max_ratio: f64,
    /// Whether the gate passed.
    pub pass: bool,
}

/// One advisory absolute-budget check: reported, never gating.
#[derive(Debug, Clone, Serialize)]
pub struct AdvisoryP95 {
    /// Query name.
    pub query: String,
    /// Invocation temperature.
    pub temp: String,
    /// Store size.
    pub size: String,
    /// Measured p95 in milliseconds.
    pub p95_ms: f64,
    /// Advisory budget in milliseconds.
    pub budget_p95_ms: f64,
    /// Whether the cell stayed within the advisory budget.
    pub within_budget: bool,
}

/// Collects advisory p95 entries for every measured cell.
#[must_use]
pub fn advisory_p95_entries(cells: &[&QueryCell], budget_p95_ms: f64) -> Vec<AdvisoryP95> {
    cells
        .iter()
        .map(|cell| AdvisoryP95 {
            query: cell.query.clone(),
            temp: cell.temp.clone(),
            size: cell.size.clone(),
            p95_ms: cell.p95_ms,
            budget_p95_ms,
            within_budget: cell.p95_ms <= budget_p95_ms,
        })
        .collect()
}

/// Advisory absolute budgets: documented targets, reported per cell, never
/// gating (issue #120 AC6 — hardware variance must not red the gate).
#[derive(Debug, Clone, Serialize)]
pub struct AdvisoryBudgets {
    /// Advisory p50 target in milliseconds.
    pub p50_ms: f64,
    /// Advisory p95 target in milliseconds.
    pub p95_ms: f64,
    /// Always true: absolute budgets are advisory, only the relative
    /// scaling assertion gates.
    pub advisory: bool,
}

/// Full benchmark report printed by `eg audit query-budget`.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetReport {
    /// Pinned corpus name.
    pub corpus_name: String,
    /// Pinned corpus version.
    pub corpus_version: String,
    /// The exact symbol query that was timed.
    pub query_symbol: String,
    /// The exact file query that was timed.
    pub query_file: String,
    /// `--limit` used for the drift query.
    pub drift_limit: usize,
    /// Records in the 1x store.
    pub record_count_1x: u64,
    /// Records in the scaled store.
    pub record_count_10x: u64,
    /// Store-size multiplier.
    pub scale_factor: usize,
    /// Record count at corpus-pinning time (drift signal, not gated).
    pub reference_record_count: u64,
    /// Named machine class the advisory budgets are defined against.
    pub reference_machine_class: String,
    /// Host the measurement actually ran on.
    pub machine: MachineInfo,
    /// Cold samples per cell.
    pub samples: usize,
    /// Warm samples per cell.
    pub warm_samples: usize,
    /// Advisory absolute budgets (reported, never gating).
    pub budgets: AdvisoryBudgets,
    /// Per-cell results keyed by query -> temp -> size.
    pub queries: BTreeMap<String, BTreeMap<String, BTreeMap<String, QueryCell>>>,
    /// The ripgrep baseline for the equivalent symbol lookup.
    pub ripgrep: RipgrepBaseline,
    /// The scaling assertion (the gate).
    pub scaling: ScalingAssertion,
    /// Advisory absolute-p95 checks, one per measured cell.
    pub advisory_p95: Vec<AdvisoryP95>,
    /// True when the scaling assertion passed.
    pub ok: bool,
}

/// Current host facts, re-exported from the issue #255 measurement layer so
/// every report carries the same machine context.
pub use crate::query_latency::machine_info as current_machine_info;

/// Re-exported for the orchestration layer: the spawn-to-first-line harness
/// is temperature-agnostic (cold and warm samples use the same child
/// timing); what differs is the priming, handled by the caller.
pub use crate::query_latency::measure_cold_query as measure_query_sample;

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol_record(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("symbols.rs".to_owned()),
            None,
            Some(format!("sym_{id}")),
            format!("symbol {id}"),
        )
    }

    #[test]
    fn synthetic_drift_is_deterministic_and_ranked() {
        let records: Vec<GraphRecord> = (0..5)
            .map(|i| symbol_record(&format!("sym-{i:03}")))
            .collect();
        let first = synthetic_drift_records(&records, "s00", "2026-01-01T00:00:00Z", 3);
        let second = synthetic_drift_records(&records, "s00", "2026-01-01T00:00:00Z", 3);
        assert_eq!(first.len(), 3);
        assert_eq!(
            first.iter().map(GraphRecord::id).collect::<Vec<_>>(),
            second.iter().map(GraphRecord::id).collect::<Vec<_>>(),
            "synthetic drift must be deterministic"
        );
        // Targets are the first symbols by ID order.
        let targets: Vec<String> = first
            .iter()
            .filter_map(|record| match record {
                GraphRecord::Node { semantic_drift, .. } => semantic_drift
                    .as_ref()
                    .map(|drift| drift.target_record_id.clone()),
                GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => None,
            })
            .collect();
        assert_eq!(targets, vec!["sym-000", "sym-001", "sym-002"]);
        // Scores strictly descend so the ranked answer is stable.
        let scores: Vec<f64> = first
            .iter()
            .filter_map(|record| match record {
                GraphRecord::Node { semantic_drift, .. } => {
                    semantic_drift.as_ref().map(|drift| drift.score)
                }
                GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => None,
            })
            .collect();
        assert!(scores.windows(2).all(|pair| pair[0] > pair[1]));
        // Repo tags keep per-copy node IDs disjoint.
        let other = synthetic_drift_records(&records, "s01", "2026-01-01T00:00:00Z", 3);
        let ids: std::collections::HashSet<&str> = first.iter().map(GraphRecord::id).collect();
        assert!(other.iter().all(|record| !ids.contains(record.id())));
    }

    #[test]
    fn synthetic_drift_takes_only_symbols_and_caps_at_available() {
        let records = vec![symbol_record("only-one")];
        let drifts = synthetic_drift_records(&records, "s00", "2026-01-01T00:00:00Z", 25);
        assert_eq!(
            drifts.len(),
            1,
            "cannot synthesize more drifts than symbols"
        );
        let empty = synthetic_drift_records(&[], "s00", "2026-01-01T00:00:00Z", 25);
        assert!(empty.is_empty());
    }

    #[test]
    fn scaling_ratio_math() {
        assert!((scaling_ratio(100.0, 150.0) - 1.5).abs() < f64::EPSILON);
        assert!((scaling_ratio(100.0, 200.0) - 2.0).abs() < f64::EPSILON);
        assert!(scaling_ratio(0.0, 100.0).is_infinite());
        assert!(scaling_ratio(-5.0, 100.0).is_infinite());
    }

    #[test]
    fn scaling_passes_boundary() {
        assert!(scaling_passes(2.0, 2.0));
        assert!(scaling_passes(1.34, 2.0));
        assert!(!scaling_passes(2.0001, 2.0));
        assert!(!scaling_passes(f64::INFINITY, 2.0));
        assert!(!scaling_passes(f64::NAN, 2.0));
    }

    #[test]
    fn summarize_cell_sorts_and_percentiles() {
        let cell = summarize_cell(
            "symbol",
            "cold",
            "store_1x",
            vec![300.0, 100.0, 200.0],
            1000,
        )
        .expect("samples");
        assert_eq!(cell.samples_ms, vec![100.0, 200.0, 300.0]);
        assert!((cell.p50_ms - 200.0).abs() < f64::EPSILON);
        assert!(cell.p95_ms >= cell.p50_ms);
        assert!((cell.min_ms - 100.0).abs() < f64::EPSILON);
        assert!((cell.max_ms - 300.0).abs() < f64::EPSILON);
        assert_eq!(cell.record_count, 1000);
        assert!(summarize_cell("symbol", "cold", "store_1x", Vec::new(), 1000).is_none());
    }

    #[test]
    fn advisory_entries_flag_budget_exceedance_without_gating() {
        let cell = summarize_cell("symbol", "cold", "store_1x", vec![300.0], 1000).expect("cell");
        let entries = advisory_p95_entries(&[&cell], 200.0);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].within_budget);
        assert!((entries[0].p95_ms - 300.0).abs() < f64::EPSILON);
        let ok_entries = advisory_p95_entries(&[&cell], 500.0);
        assert!(ok_entries[0].within_budget);
    }

    #[test]
    fn ripgrep_summaries_and_skip_are_explicit() {
        let measured = summarize_ripgrep("rg foo .", vec![30.0, 10.0, 20.0], 2000.0, 2000.0);
        assert!(!measured.skipped);
        assert!((measured.p50_ms - 20.0).abs() < f64::EPSILON);
        assert!(measured.skip_reason.is_none());

        let skipped = skipped_ripgrep("rg foo .", 2000.0, 2000.0, "not on PATH");
        assert!(skipped.skipped);
        assert_eq!(skipped.skip_reason.as_deref(), Some("not on PATH"));
        assert!(skipped.samples_ms.is_empty());
    }
}
