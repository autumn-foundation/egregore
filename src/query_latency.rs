//! Cold query-latency budget for time-to-first-symbol-answer (issue #255).
//!
//! This module is the **measurement layer** behind `eg audit query-latency`.
//! For a pinned reference corpus it measures cold wall-clock latency from
//! process start to the first emitted result line of `eg query symbol <NAME>`,
//! separately for the `--graph <JSONL>` and `--data-dir <embedded store>`
//! input sources, and gates the p50 against a documented budget.
//!
//! The orchestration (corpus build, subprocess spawning) lives in
//! `crate::cli::audit`; everything here is pure and deterministic except
//! [`measure_cold_query`], which spawns exactly one child process and times
//! it. Percentiles use linear interpolation over sorted samples.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Default number of cold samples measured per input source.
pub const DEFAULT_SAMPLES: usize = 11;

/// Default p50 budget in milliseconds: time-to-first-symbol-answer must stay
/// under 2s on the reference corpus (issue #255 success metric).
pub const DEFAULT_BUDGET_P50_MS: f64 = 2000.0;

/// Minimum record count that keeps the reference corpus representative. The
/// gate refuses to pass on a collapsed corpus instead of rubber-stamping a
/// fast-but-meaningless measurement.
pub const MIN_REFERENCE_RECORDS: u64 = 8000;

/// The pinned latency-benchmark corpus manifest, deserialized from
/// `corpus/query_latency_corpus.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryLatencyCorpus {
    /// Schema version for the manifest format.
    pub corpus_version: String,
    /// Stable human-readable corpus name reported for interpretation.
    pub corpus_name: String,
    /// Description of the corpus scope.
    pub description: String,
    /// Corpus source directory, resolved relative to the manifest's parent dir.
    pub source_dir: String,
    /// Repository identity override so record IDs are stable across checkouts.
    pub repository_id_override: String,
    /// Fixed scan transaction time so the scan is deterministic.
    pub scan_time: String,
    /// Symbol name the benchmark queries (must match >= 1 record).
    pub query_symbol: String,
    /// Cold samples measured per input source.
    pub samples: usize,
    /// p50 budget in milliseconds each source must meet.
    pub budget_p50_ms: f64,
    /// Record count measured when the corpus was pinned (auditability).
    pub reference_record_count: u64,
    /// Named machine class the budget is defined against.
    pub reference_machine_class: String,
}

/// Host facts recorded alongside every report so a measurement stays
/// interpretable without re-running it.
#[derive(Debug, Clone, Serialize)]
pub struct MachineInfo {
    /// Operating system (`std::env::consts::OS`).
    pub os: String,
    /// CPU architecture (`std::env::consts::ARCH`).
    pub arch: String,
    /// Available parallelism at benchmark time (0 when undetectable).
    pub parallelism: usize,
}

/// Current host facts.
#[must_use]
pub fn machine_info() -> MachineInfo {
    MachineInfo {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        parallelism: std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(0),
    }
}

/// Per-source latency summary.
#[derive(Debug, Clone, Serialize)]
pub struct SourceLatency {
    /// Input source: `"graph"` or `"data_dir"`.
    pub source: String,
    /// Every cold sample in milliseconds, ascending.
    pub samples_ms: Vec<f64>,
    /// Median cold latency in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile cold latency in milliseconds.
    pub p95_ms: f64,
    /// Fastest cold sample in milliseconds.
    pub min_ms: f64,
    /// Slowest cold sample in milliseconds.
    pub max_ms: f64,
    /// Budget this source was gated against, in milliseconds.
    pub budget_p50_ms: f64,
    /// Whether `p50_ms <= budget_p50_ms`.
    pub pass: bool,
    /// True when the source was not measured at all.
    pub skipped: bool,
    /// Why the source was skipped (present only when `skipped`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// Full benchmark report printed by `eg audit query-latency`.
#[derive(Debug, Clone, Serialize)]
pub struct LatencyReport {
    /// Pinned corpus name.
    pub corpus_name: String,
    /// Pinned corpus version.
    pub corpus_version: String,
    /// The exact query that was timed.
    pub query: String,
    /// Records in the scanned corpus (auditable corpus size).
    pub record_count: u64,
    /// Record count at corpus-pinning time (drift signal, not gated).
    pub reference_record_count: u64,
    /// Named machine class the budget is defined against.
    pub reference_machine_class: String,
    /// Host the measurement actually ran on.
    pub machine: MachineInfo,
    /// Cold samples per source.
    pub samples: usize,
    /// p50 budget in milliseconds.
    pub budget_p50_ms: f64,
    /// Per-source results keyed by `"graph"` / `"data_dir"`.
    pub sources: std::collections::BTreeMap<String, SourceLatency>,
    /// True when every measured source passed its budget.
    pub ok: bool,
}

/// Linear-interpolation percentile over ascending samples (numpy `linear`
/// method). Returns `None` for empty input or `p` outside `[0, 100]`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn percentile(sorted_samples: &[f64], p: f64) -> Option<f64> {
    if sorted_samples.is_empty() || !(0.0..=100.0).contains(&p) {
        return None;
    }
    let last = sorted_samples.len() - 1;
    // SAFETY: `last` is a sample count (small); `rank` is in `[0, last]`, so the
    // float→usize casts below cannot truncate or lose sign in practice.
    let rank = p / 100.0 * last as f64;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    let frac = rank - low as f64;
    Some(sorted_samples[low].mul_add(1.0 - frac, sorted_samples[high] * frac))
}

/// Summarizes ascending cold samples into a [`SourceLatency`] gated at
/// `budget_p50_ms`. Returns `None` when there are no samples.
#[must_use]
pub fn summarize(
    source: &str,
    mut samples_ms: Vec<f64>,
    budget_p50_ms: f64,
) -> Option<SourceLatency> {
    if samples_ms.is_empty() {
        return None;
    }
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50_ms = percentile(&samples_ms, 50.0)?;
    let p95_ms = percentile(&samples_ms, 95.0)?;
    let min_ms = samples_ms[0];
    let max_ms = samples_ms[samples_ms.len() - 1];
    Some(SourceLatency {
        source: source.to_owned(),
        samples_ms,
        p50_ms,
        p95_ms,
        min_ms,
        max_ms,
        budget_p50_ms,
        pass: p50_ms <= budget_p50_ms,
        skipped: false,
        skip_reason: None,
    })
}

/// Builds a skipped [`SourceLatency`] so a source that cannot run (e.g. the
/// embedded store without the `embedded-aletheiadb` feature) is explicit in
/// the report instead of silently absent.
#[must_use]
pub fn skipped_source(source: &str, budget_p50_ms: f64, reason: &str) -> SourceLatency {
    SourceLatency {
        source: source.to_owned(),
        samples_ms: Vec::new(),
        p50_ms: 0.0,
        p95_ms: 0.0,
        min_ms: 0.0,
        max_ms: 0.0,
        budget_p50_ms,
        pass: true,
        skipped: true,
        skip_reason: Some(reason.to_owned()),
    }
}

/// Spawns `exe` with `args` and returns the wall-clock milliseconds from just
/// before spawn until the first non-empty stdout line is observed.
///
/// The child is killed once the first line arrives; the measured quantity is
/// unaffected by the cleanup. Returns an error (with the child's stderr)
/// when the child emits no stdout line — a query that answers nothing has no
/// time-to-first-answer, and the benchmark fails closed instead of timing an
/// empty result.
///
/// # Errors
///
/// Returns an error if the child process cannot be spawned, if stdout was not
/// piped, or if the child emits no stdout line before exiting.
pub fn measure_cold_query(exe: &Path, args: &[&str]) -> Result<f64, String> {
    let start = Instant::now();
    let mut child = Command::new(exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to spawn {}: {error}", exe.display()))?;

    let first_line_at = {
        let stdout = child.stdout.take().ok_or("child stdout was not piped")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut found: Option<f64> = None;
        loop {
            line.clear();
            let bytes = reader
                .read_line(&mut line)
                .map_err(|error| format!("failed to read child stdout: {error}"))?;
            if bytes == 0 {
                break;
            }
            if !line.trim().is_empty() {
                found = Some(start.elapsed().as_secs_f64() * 1000.0);
                break;
            }
        }
        found
    };

    // Stop the clock at the first line; then clean up the child, which may
    // still be printing the rest of its answer.
    let _ = child.kill();
    let stderr = child
        .stderr
        .take()
        .map(|stderr| {
            let mut err = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut err);
            err
        })
        .unwrap_or_default();
    let _ = child.wait();

    first_line_at.ok_or_else(|| {
        let detail = stderr.trim();
        if detail.is_empty() {
            "query child emitted no stdout lines".to_owned()
        } else {
            format!("query child emitted no stdout lines; stderr: {detail}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_linear_interpolation() {
        let samples = vec![10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile(&samples, 50.0), Some(25.0));
        assert_eq!(percentile(&samples, 0.0), Some(10.0));
        assert_eq!(percentile(&samples, 100.0), Some(40.0));
        // 95th percentile of 4 samples: rank 2.85 -> 30 + 0.85 * 10.
        assert_eq!(percentile(&samples, 95.0), Some(38.5));
    }

    #[test]
    fn percentile_rejects_empty_and_out_of_range() {
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[1.0], -1.0), None);
        assert_eq!(percentile(&[1.0], 101.0), None);
        assert_eq!(percentile(&[1.0], f64::NAN), None);
    }

    #[test]
    fn summarize_sorts_and_gates_p50() {
        let summary = summarize("graph", vec![300.0, 100.0, 200.0], 250.0).expect("samples");
        assert_eq!(summary.samples_ms, vec![100.0, 200.0, 300.0]);
        assert!((summary.p50_ms - 200.0).abs() < f64::EPSILON);
        assert!(summary.pass);
        assert!(!summary.skipped);

        let failing = summarize("graph", vec![300.0, 100.0, 200.0], 150.0).expect("samples");
        assert!(!failing.pass);
    }

    #[test]
    fn summarize_returns_none_without_samples() {
        assert!(summarize("graph", Vec::new(), 2000.0).is_none());
    }

    #[test]
    fn skipped_source_is_explicit_and_passing() {
        let skipped = skipped_source("data_dir", 2000.0, "feature disabled");
        assert!(skipped.skipped);
        assert!(skipped.pass);
        assert_eq!(skipped.skip_reason.as_deref(), Some("feature disabled"));
        assert!(skipped.samples_ms.is_empty());
    }
}
