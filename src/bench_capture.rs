// clippy::too_long_first_doc_paragraph fires on this module's doc without a span
// (nursery lint span-reporting bug in clippy 0.1.94); suppress at module level.
#![allow(clippy::too_long_first_doc_paragraph)]
//! Capture criterion benchmark runs as citable, deterministic
//! verification-domain `BenchmarkRun` records (issue #237).
//!
//! This module is CAPTURE-ONLY: it parses the machine-readable `estimates.json`
//! files criterion already wrote under a `target/criterion`-shaped directory.
//! It NEVER executes a benchmark runner. A captured run is a recorded
//! observation of one measurement — "no captured regression" is not proof of
//! performance.
//!
//! Input shape: `<input>/<benchmark-id>/new/estimates.json`, plus an optional
//! `<input>/<benchmark-id>/base/estimates.json` saved baseline. Criterion
//! reports every estimate in nanoseconds; the central estimate of record is
//! the `mean` point estimate, with `unit: "ns"`.
//!
//! When a baseline is present the record carries a `regression | improvement |
//! unchanged` verdict plus the percentage delta of the mean point estimates;
//! with no baseline the verdict is the explicit `no_baseline`, never a silent
//! `unchanged`. The noise floor ([`BENCH_NOISE_THRESHOLD_PCT`]) keeps
//! sub-threshold jitter out of the verdict.
//!
//! Stable IDs extend the schema's recommended `benchmark_run` tuple with the
//! benchmark id — `["benchmark_run", session_id, commit_sha, suite_name,
//! benchmark_id]` — because one record is emitted per benchmark and the
//! 4-tuple alone cannot address them. All parts are ASCII-lowercased before
//! hashing (see [`verification_stable_id`]); re-capturing identical input
//! yields byte-identical records, so ingest converges to a no-op (issue #130).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ir::{
    EdgeLabel, GraphRecord, NodeKind, OutputHandle, Producer, ProducerKind, TemporalMetadata,
    VERIFICATION_SCHEMA_VERSION, stable_id, verification_stable_id,
};

/// Inline-payload ceiling shared with the typed evidence writer: an
/// [`OutputHandle`] stores content inline only when it is at or below this many
/// bytes (16 KiB), otherwise it references the content by hash only.
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

/// The single accepted `--format` value.
pub const CRITERION_ESTIMATES_FORMAT: &str = "criterion-estimates";

/// Normalized-summary format tag stamped into the `stdout_handle` payload.
/// Format tag of the normalized summary stored in `stdout_handle` — distinct
/// from the input format: the summary is egregore's re-parseable record, not
/// criterion's raw artifact.
pub const NORMALIZED_SUMMARY_FORMAT: &str = "criterion-estimates-v1";

/// Diagnostic machine codes (stamped into `symbol_kind` on `Diagnostic` nodes).
pub const EMPTY_BENCH_OUTPUT_CODE: &str = "empty_bench_output";
/// See [`EMPTY_BENCH_OUTPUT_CODE`].
pub const UNPARSEABLE_BENCH_OUTPUT_CODE: &str = "unparseable_bench_output";
/// See [`EMPTY_BENCH_OUTPUT_CODE`].
pub const BENCH_SYMBOL_UNRESOLVED_CODE: &str = "bench_symbol_unresolved";
/// See [`EMPTY_BENCH_OUTPUT_CODE`].
pub const BENCH_SYMBOL_AMBIGUOUS_CODE: &str = "bench_symbol_ambiguous";

/// Noise floor for the baseline verdict, in percent: `|delta_pct|` at or below
/// this is `unchanged` (measurement noise), never a silent default.
///
/// Criterion's own bootstrap confidence intervals routinely span more than
/// this on shared CI hardware, so a fixed floor keeps the verdict
/// deterministic without re-bootstrapping criterion's statistics.
pub const BENCH_NOISE_THRESHOLD_PCT: f64 = 1.0;

/// Regression verdict of a benchmark run against its saved baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkVerdict {
    /// The new mean is slower than the baseline by more than the noise floor.
    Regression,
    /// The new mean is faster than the baseline by more than the noise floor.
    Improvement,
    /// Within the noise floor of the baseline (or the baseline is degenerate).
    Unchanged,
    /// No baseline was captured with this run — never a silent `unchanged`.
    NoBaseline,
}

impl BenchmarkVerdict {
    /// Returns the stable serialized verdict string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Regression => "regression",
            Self::Improvement => "improvement",
            Self::Unchanged => "unchanged",
            Self::NoBaseline => "no_baseline",
        }
    }
}

/// Computes the baseline verdict from the new and baseline mean point
/// estimates (nanoseconds).
///
/// Returns `(verdict, delta_pct)` where `delta_pct` is the percentage change
/// `(new - base) / base * 100` rounded to four decimals, or `None` when no
/// delta is definable (no baseline, or a non-positive/degenerate baseline
/// mean that cannot divide).
#[must_use]
pub fn benchmark_verdict(
    new_mean_ns: f64,
    base_mean_ns: Option<f64>,
) -> (BenchmarkVerdict, Option<f64>) {
    let Some(base) = base_mean_ns else {
        return (BenchmarkVerdict::NoBaseline, None);
    };
    if !base.is_finite() || base <= 0.0 {
        // A degenerate baseline cannot define a delta; report `unchanged`
        // explicitly rather than inventing one.
        return (BenchmarkVerdict::Unchanged, None);
    }
    let delta = (new_mean_ns - base) / base * 100.0;
    let delta_rounded = (delta * 10_000.0).round() / 10_000.0;
    let verdict = if delta_rounded.abs() <= BENCH_NOISE_THRESHOLD_PCT {
        BenchmarkVerdict::Unchanged
    } else if delta_rounded > 0.0 {
        BenchmarkVerdict::Regression
    } else {
        BenchmarkVerdict::Improvement
    };
    (verdict, Some(delta_rounded))
}

/// Reason a criterion output directory could not be captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchCaptureError {
    /// The input path is not a readable directory, or it held no
    /// `*/new/estimates.json` files.
    Empty,
    /// At least one `estimates.json` was not valid criterion estimates JSON
    /// (or carried no finite `mean` point estimate).
    Unparseable,
}

impl BenchCaptureError {
    /// Returns the stable machine code for this error.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Empty => EMPTY_BENCH_OUTPUT_CODE,
            Self::Unparseable => UNPARSEABLE_BENCH_OUTPUT_CODE,
        }
    }
}

impl std::fmt::Display for BenchCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "no criterion benchmark estimates found"),
            Self::Unparseable => write!(f, "unparseable criterion estimates"),
        }
    }
}

impl std::error::Error for BenchCaptureError {}

// ── Directory scan ──────────────────────────────────────────────────────────

/// One criterion benchmark directory: the raw `estimates.json` bytes plus
/// provenance. Produced by [`scan_criterion_dir`], parsed by
/// [`parse_benchmark_inputs`].
#[derive(Debug, Clone)]
pub struct BenchmarkInput {
    /// Benchmark id: the `new/` directory's path relative to the scan root,
    /// with `/` separators (mirrors criterion's `<benchmark-id>/new` layout,
    /// including nested group directories).
    pub benchmark_id: String,
    /// Raw bytes of `<benchmark-id>/new/estimates.json`.
    pub new_estimates_bytes: Vec<u8>,
    /// Display path of the `new` estimates file, as found.
    pub new_estimates_path: String,
    /// Raw bytes of `<benchmark-id>/base/estimates.json`, when a saved
    /// baseline exists.
    pub base_estimates_bytes: Option<Vec<u8>>,
    /// Display path of the `base` estimates file, when present.
    pub base_estimates_path: Option<String>,
}

/// Recursively collects files named `estimates.json` under `root`, sorted by
/// path, so the walk is deterministic across filesystems and runs.
fn collect_estimates_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == "estimates.json") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Deterministically scans a `target/criterion`-shaped directory for benchmark
/// runs: every `<benchmark-id>/new/estimates.json`, with the optional sibling
/// `<benchmark-id>/base/estimates.json` as the saved baseline.
///
/// The benchmark id is the `new/` directory's path relative to `root`; results
/// are sorted by id. Reading is the only filesystem effect — nothing is
/// executed.
///
/// # Errors
///
/// Returns [`BenchCaptureError::Empty`] when `root` is not a readable
/// directory or contains no `*/new/estimates.json` files.
pub fn scan_criterion_dir(root: &Path) -> Result<Vec<BenchmarkInput>, BenchCaptureError> {
    if !root.is_dir() {
        return Err(BenchCaptureError::Empty);
    }
    let mut by_id: BTreeMap<String, BenchmarkInput> = BTreeMap::new();
    for file in collect_estimates_files(root) {
        // Accept exactly `<benchmark-id>/{new,base}/estimates.json`.
        let Some(parent) = file.parent().and_then(Path::file_name) else {
            continue;
        };
        let slot = parent.to_string_lossy();
        if slot != "new" && slot != "base" {
            continue;
        }
        let Some(bench_dir) = file.parent().and_then(Path::parent) else {
            continue;
        };
        let Ok(relative) = bench_dir.strip_prefix(root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let benchmark_id = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        let display = file.to_string_lossy().into_owned();
        let entry = by_id
            .entry(benchmark_id.clone())
            .or_insert_with(|| BenchmarkInput {
                benchmark_id: benchmark_id.clone(),
                new_estimates_bytes: Vec::new(),
                new_estimates_path: String::new(),
                base_estimates_bytes: None,
                base_estimates_path: None,
            });
        if slot == "new" {
            entry.new_estimates_bytes = bytes;
            entry.new_estimates_path = display;
        } else {
            entry.base_estimates_bytes = Some(bytes);
            entry.base_estimates_path = Some(display);
        }
    }
    // Drop benchmark dirs whose `new/` estimates are missing (a `base/` alone
    // is a saved baseline with no run to capture, not a benchmark run).
    let inputs: Vec<BenchmarkInput> = by_id
        .into_values()
        .filter(|input| !input.new_estimates_bytes.is_empty())
        .collect();
    if inputs.is_empty() {
        return Err(BenchCaptureError::Empty);
    }
    Ok(inputs)
}

// ── Estimates parsing ───────────────────────────────────────────────────────

/// A tolerant view of one criterion estimate statistic. Unknown extra fields
/// (criterion versions carry more) are ignored; absent `Option` fields
/// deserialize to `None`.
#[derive(Debug, Deserialize)]
struct RawEstimate {
    point_estimate: Option<f64>,
    confidence_interval: Option<RawConfidenceInterval>,
    #[allow(dead_code)]
    standard_error: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawConfidenceInterval {
    confidence_level: Option<f64>,
    lower_bound: Option<f64>,
    upper_bound: Option<f64>,
}

/// A tolerant view of a criterion `estimates.json`. Only `mean` (the central
/// estimate of record) and `median` are read; the rest is ignored.
#[derive(Debug, Deserialize)]
struct RawEstimates {
    mean: Option<RawEstimate>,
    median: Option<RawEstimate>,
}

/// One parsed benchmark: the central estimate, its units, the baseline delta,
/// and the verdict.
#[derive(Debug, Clone)]
pub struct ParsedBenchmark {
    /// Benchmark id (the `new/` directory relative to the scan root).
    pub id: String,
    /// Central estimate of record: the `mean` point estimate, nanoseconds.
    pub mean_ns: f64,
    /// Median point estimate, nanoseconds, when criterion reported one.
    pub median_ns: Option<f64>,
    /// Lower bound of the mean's confidence interval, when reported.
    pub ci_lower_ns: Option<f64>,
    /// Upper bound of the mean's confidence interval, when reported.
    pub ci_upper_ns: Option<f64>,
    /// Confidence level of the interval, when reported.
    pub ci_level: Option<f64>,
    /// Baseline `mean` point estimate, when a saved baseline exists.
    pub base_mean_ns: Option<f64>,
    /// Baseline verdict (see [`benchmark_verdict`]).
    pub verdict: BenchmarkVerdict,
    /// Percentage delta of the mean vs the baseline, when definable.
    pub delta_pct: Option<f64>,
    /// BLAKE3 hex of the raw `new/estimates.json` bytes (evidence handle).
    pub new_artifact_hash: String,
    /// Display path of the `new` estimates file (source artifact path).
    pub new_estimates_path: String,
    /// Display path of the `base` estimates file, when present.
    pub base_estimates_path: Option<String>,
}

/// The deterministic result of parsing scanned criterion estimates.
#[derive(Debug, Clone)]
pub struct BenchCaptureParse {
    benchmarks: Vec<ParsedBenchmark>,
}

impl BenchCaptureParse {
    /// The parsed benchmarks, sorted by benchmark id.
    #[must_use]
    pub fn benchmarks(&self) -> &[ParsedBenchmark] {
        &self.benchmarks
    }
}

/// Extracts the finite `mean` point estimate and optional median/CI from one
/// `estimates.json` payload.
fn parse_estimates_file(
    bytes: &[u8],
) -> Result<(f64, Option<f64>, RawEstimates), BenchCaptureError> {
    let text = std::str::from_utf8(bytes).map_err(|_| BenchCaptureError::Unparseable)?;
    let estimates: RawEstimates =
        serde_json::from_str(text).map_err(|_| BenchCaptureError::Unparseable)?;
    let mean = estimates
        .mean
        .as_ref()
        .and_then(|m| m.point_estimate)
        .filter(|v| v.is_finite())
        .ok_or(BenchCaptureError::Unparseable)?;
    let median = estimates
        .median
        .as_ref()
        .and_then(|m| m.point_estimate)
        .filter(|v| v.is_finite());
    Ok((mean, median, estimates))
}

/// Parses scanned criterion estimates into a deterministic
/// [`BenchCaptureParse`].
///
/// Every `new/estimates.json` must be valid criterion estimates JSON carrying
/// a finite `mean` point estimate; a corrupt file fails the whole capture
/// (exit 5 at the CLI) rather than being silently skipped. A corrupt or
/// missing `base/estimates.json` is different: the baseline is optional, so a
/// baseline that does not parse is treated as absent (`no_baseline` verdict)
/// — the run itself is still evidence.
///
/// # Errors
///
/// Returns [`BenchCaptureError::Unparseable`] when any `new/estimates.json`
/// is invalid.
pub fn parse_benchmark_inputs(
    inputs: &[BenchmarkInput],
) -> Result<BenchCaptureParse, BenchCaptureError> {
    let mut benchmarks = Vec::with_capacity(inputs.len());
    for input in inputs {
        let (mean_ns, median_ns, estimates) = parse_estimates_file(&input.new_estimates_bytes)?;
        let mean_ci = estimates
            .mean
            .as_ref()
            .and_then(|m| m.confidence_interval.as_ref());
        let base_mean_ns = input
            .base_estimates_bytes
            .as_deref()
            .and_then(|bytes| parse_estimates_file(bytes).ok())
            .map(|(mean, _, _)| mean);
        let (verdict, delta_pct) = benchmark_verdict(mean_ns, base_mean_ns);
        let mut hasher = blake3::Hasher::new();
        hasher.update(&input.new_estimates_bytes);
        benchmarks.push(ParsedBenchmark {
            id: input.benchmark_id.clone(),
            mean_ns,
            median_ns,
            ci_lower_ns: mean_ci
                .and_then(|ci| ci.lower_bound)
                .filter(|v| v.is_finite()),
            ci_upper_ns: mean_ci
                .and_then(|ci| ci.upper_bound)
                .filter(|v| v.is_finite()),
            ci_level: mean_ci
                .and_then(|ci| ci.confidence_level)
                .filter(|v| v.is_finite()),
            base_mean_ns,
            verdict,
            delta_pct,
            new_artifact_hash: hasher.finalize().to_hex().to_string(),
            new_estimates_path: input.new_estimates_path.clone(),
            base_estimates_path: input.base_estimates_path.clone(),
        });
    }
    benchmarks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(BenchCaptureParse { benchmarks })
}

// ── Record building ─────────────────────────────────────────────────────────

/// Run metadata for a captured benchmark run. All string fields are
/// caller-supplied; none are read from a wall clock.
#[derive(Debug, Clone)]
pub struct BenchmarkRunRequest<'a> {
    /// Stable session identity (part of the record ID).
    pub session_id: &'a str,
    /// Commit handle / external identifier (part of the record ID).
    pub commit: &'a str,
    /// Suite name (part of the record ID and the node `name`).
    pub suite: &'a str,
    /// The exact command that produced the criterion output. Stored, never
    /// executed.
    pub command: Option<&'a str>,
    /// Caller-supplied RFC 3339 timestamp (validated by the caller).
    pub executed_at: &'a str,
}

/// Outcome of building a `BenchmarkRun` record batch.
#[derive(Debug)]
pub struct BenchRunOutcome {
    /// All emitted records (one `BenchmarkRun` node per benchmark, plus
    /// resolution diagnostics and edges).
    pub records: Vec<GraphRecord>,
    /// Benchmark ids, sorted — parallel to [`Self::record_ids`].
    pub benchmark_ids: Vec<String>,
    /// Stable `BenchmarkRun` record ids, parallel to
    /// [`Self::benchmark_ids`].
    pub record_ids: Vec<String>,
    /// Per-benchmark verdicts, parallel to [`Self::benchmark_ids`].
    pub verdicts: Vec<BenchmarkVerdict>,
    /// Number of benchmarks that resolved to exactly one symbol (with
    /// `--graph`).
    pub resolved_count: u64,
    /// Number of benchmarks that resolved to zero symbols (with `--graph`).
    pub unresolved_count: u64,
    /// Number of benchmarks that resolved to two or more symbols (with
    /// `--graph`).
    pub ambiguous_count: u64,
    /// Benchmark ids with no symbol link — the `unresolved` section of the
    /// capture envelope. Never dropped silently.
    pub unresolved: Vec<String>,
}

/// Machine-readable normalized benchmark summary stored in `stdout_handle`.
/// This is the re-parseable record the schema reserves `stdout_handle` for;
/// the raw criterion artifact stays pinned by `source_artifact_path` +
/// `source_artifact_hash`.
#[derive(Debug, Serialize)]
struct NormalizedBenchmark<'a> {
    format: &'a str,
    benchmark_id: &'a str,
    suite: &'a str,
    session_id: &'a str,
    commit: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a str>,
    /// The measurement criterion reports as the central estimate.
    measurement_of_record: &'a str,
    /// Units of every estimate: nanoseconds, criterion's native unit.
    unit: &'a str,
    mean_ns: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    median_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence_interval: Option<NormalizedConfidenceInterval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline: Option<NormalizedBaseline<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_pct: Option<f64>,
    verdict: &'a str,
}

#[derive(Debug, Serialize)]
struct NormalizedConfidenceInterval {
    level: f64,
    lower_ns: f64,
    upper_ns: f64,
}

#[derive(Debug, Serialize)]
struct NormalizedBaseline<'a> {
    mean_ns: f64,
    artifact_path: &'a str,
}

/// Builds the deterministic normalized-summary JSON stored in `stdout_handle`.
///
/// Every float carried by the summary is pre-filtered finite, so
/// serialization is infallible in practice; a failure is still surfaced as
/// [`BenchCaptureError::Unparseable`] rather than panicking.
fn normalized_summary_json(
    req: &BenchmarkRunRequest,
    bench: &ParsedBenchmark,
) -> Result<String, BenchCaptureError> {
    let confidence_interval = match (bench.ci_level, bench.ci_lower_ns, bench.ci_upper_ns) {
        (Some(level), Some(lower), Some(upper)) => Some(NormalizedConfidenceInterval {
            level,
            lower_ns: lower,
            upper_ns: upper,
        }),
        _ => None,
    };
    let baseline = match (bench.base_mean_ns, bench.base_estimates_path.as_deref()) {
        (Some(mean_ns), Some(artifact_path)) => Some(NormalizedBaseline {
            mean_ns,
            artifact_path,
        }),
        _ => None,
    };
    let summary = NormalizedBenchmark {
        format: NORMALIZED_SUMMARY_FORMAT,
        benchmark_id: &bench.id,
        suite: req.suite,
        session_id: req.session_id,
        commit: req.commit,
        command: req.command,
        measurement_of_record: "mean",
        unit: "ns",
        mean_ns: bench.mean_ns,
        median_ns: bench.median_ns,
        confidence_interval,
        baseline,
        delta_pct: bench.delta_pct,
        verdict: bench.verdict.as_str(),
    };
    serde_json::to_string(&summary).map_err(|_| BenchCaptureError::Unparseable)
}

/// A machine-readable view of a captured `BenchmarkRun`'s normalized summary,
/// for the query lane. Parses `stdout_handle.inline`; returns `None` when the
/// record carries no parseable summary (e.g. produced before issue #237).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct BenchmarkSummaryView {
    /// Benchmark id as captured.
    pub benchmark_id: String,
    /// Suite name.
    pub suite: String,
    /// Central estimate, nanoseconds.
    pub mean_ns: f64,
    /// Units (always `"ns"` for criterion captures).
    pub unit: String,
    /// Baseline verdict string.
    pub verdict: String,
    /// Percentage delta vs baseline, when definable.
    pub delta_pct: Option<f64>,
}

/// Reads the normalized summary off a `BenchmarkRun` node for the query lane.
///
/// Falls back to the node `name` for the benchmark id when no parseable
/// summary is present, so records ingested from foreign producers still
/// answer by id.
#[must_use]
pub fn benchmark_summary_of(record: &GraphRecord) -> Option<BenchmarkSummaryView> {
    let GraphRecord::Node {
        kind,
        name,
        stdout_handle,
        ..
    } = record
    else {
        return None;
    };
    if *kind != NodeKind::BenchmarkRun {
        return None;
    }
    let name = name.clone().unwrap_or_default();
    let inline = stdout_handle.as_ref()?.inline.as_ref()?;
    let parsed: BenchmarkSummaryView = serde_json::from_str(inline).ok()?;
    Some(BenchmarkSummaryView {
        benchmark_id: if parsed.benchmark_id.is_empty() {
            name
        } else {
            parsed.benchmark_id
        },
        ..parsed
    })
}

/// Builds an [`OutputHandle`] for the bounded normalized summary.
fn output_handle(content: &str) -> OutputHandle {
    let bytes = content.len() as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(content.as_bytes());
    OutputHandle {
        inline: (bytes <= INLINE_PAYLOAD_CEILING).then(|| content.to_owned()),
        hash: hasher.finalize().to_hex().to_string(),
        bytes,
    }
}

/// A deterministic producer whose `producer_started_at` is the caller-supplied
/// `executed_at` (never a wall clock), so identical input yields byte-identical
/// output.
fn bench_capture_producer(executed_at: &str) -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::ObservationWriter,
        producer_components: BTreeMap::new(),
        producer_started_at: executed_at.to_owned(),
    }
}

/// Formats a nanosecond value deterministically: up to four decimals, no
/// trailing noise.
fn fmt_ns(value: f64) -> String {
    let rounded = (value * 10_000.0).round() / 10_000.0;
    let text = format!("{rounded:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Human-readable one-line summary, e.g.
/// `benchmark parse_scan: mean 1250 ns (suite benches), regression +25% vs
/// baseline` or `benchmark hash_key: mean 500 ns (suite benches), no baseline`.
fn human_summary(req: &BenchmarkRunRequest, bench: &ParsedBenchmark) -> String {
    use std::fmt::Write as _;
    let mut s = format!(
        "benchmark {}: mean {} ns (suite {})",
        bench.id,
        fmt_ns(bench.mean_ns),
        req.suite
    );
    if bench.verdict == BenchmarkVerdict::NoBaseline {
        let _ = write!(s, ", no baseline");
    } else {
        let _ = write!(s, ", {}", bench.verdict.as_str());
        if let Some(delta) = bench.delta_pct {
            let sign = if delta > 0.0 { "+" } else { "" };
            let _ = write!(s, " {sign}{} % vs baseline", fmt_ns(delta));
        }
    }
    s
}

/// Builds one stable `BenchmarkRun` node for a parsed benchmark.
///
/// # Errors
///
/// Returns [`BenchCaptureError::Unparseable`] when the normalized summary
/// fails to serialize (infallible in practice — every float it carries is
/// pre-filtered finite).
fn build_benchmark_run_node(
    req: &BenchmarkRunRequest,
    bench: &ParsedBenchmark,
    record_id: &str,
) -> Result<GraphRecord, BenchCaptureError> {
    let mut node = GraphRecord::node(
        record_id.to_owned(),
        NodeKind::BenchmarkRun,
        None,
        None,
        Some(bench.id.clone()),
        human_summary(req, bench),
    );
    let handle = output_handle(&normalized_summary_json(req, bench)?);
    if let GraphRecord::Node {
        schema_version,
        domain,
        verification_kind,
        status,
        executed_at,
        source_artifact_path,
        source_artifact_hash,
        evidence_quality,
        stdout_handle,
        temporal,
        producer,
        ..
    } = &mut node
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *domain = Some("verification".to_owned());
        *verification_kind = Some("benchmark_run".to_owned());
        // `status` records that the run was captured; the numeric outcome is
        // the verdict in the normalized summary, not a pass/fail boolean.
        *status = Some("pass".to_owned());
        *executed_at = Some(req.executed_at.to_owned());
        *source_artifact_path = Some(bench.new_estimates_path.clone());
        *source_artifact_hash = Some(bench.new_artifact_hash.clone());
        *evidence_quality = Some("summarized".to_owned());
        *stdout_handle = Some(Box::new(handle));
        *temporal = Some(TemporalMetadata {
            git_commit: req.commit.to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: req.executed_at.to_owned(),
            author_time: None,
            observed_at: req.executed_at.to_owned(),
            valid_time_source: Some("author_provided".to_owned()),
        });
        *producer = Some(bench_capture_producer(req.executed_at));
    }
    Ok(node)
}

/// The final `/`-segment of a benchmark id — the conservative resolution key
/// (criterion ids look like `group/bench_name`; the bench name is the code
/// anchor).
fn final_segment(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Indexes a code graph: symbol name → the set of matching Symbol records, and
/// repo-relative path → the File record id.
struct CodeGraphIndex<'a> {
    symbols_by_name: BTreeMap<&'a str, Vec<&'a GraphRecord>>,
    file_id_by_path: BTreeMap<&'a str, &'a str>,
}

impl<'a> CodeGraphIndex<'a> {
    fn build(records: &'a [GraphRecord]) -> Self {
        let mut symbols_by_name: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
        let mut file_id_by_path: BTreeMap<&str, &str> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                id,
                kind,
                name,
                repo_relative_path,
                ..
            } = record
            {
                match kind {
                    NodeKind::Symbol => {
                        if let Some(name) = name.as_deref() {
                            symbols_by_name.entry(name).or_default().push(record);
                        }
                    }
                    NodeKind::File => {
                        if let Some(path) = repo_relative_path.as_deref() {
                            // First writer wins for a given path (deterministic).
                            file_id_by_path.entry(path).or_insert(id.as_str());
                        }
                    }
                    _ => {}
                }
            }
        }
        Self {
            symbols_by_name,
            file_id_by_path,
        }
    }
}

/// Reads `(id, repo_relative_path)` off a Symbol record. Returns `None` for
/// non-Node records (edge records carry no symbol handles and are skipped).
fn symbol_handles(record: &GraphRecord) -> Option<(&str, Option<&str>)> {
    match record {
        GraphRecord::Node {
            id,
            repo_relative_path,
            ..
        } => Some((id.as_str(), repo_relative_path.as_deref())),
        _ => None,
    }
}

/// Builds a codegraph-domain resolution `Diagnostic` for a benchmark that
/// resolved to zero or two-plus symbols. Non-orphan by doctrine: `Diagnostic`
/// markers are permitted to stand alone (`eg validate`), matching
/// `manifest_deps` and the test capture lane.
fn resolution_diagnostic(
    req: &BenchmarkRunRequest,
    benchmark_id: &str,
    code: &str,
    matched: usize,
) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            code,
            req.session_id,
            req.commit,
            req.suite,
            benchmark_id,
        ]),
        NodeKind::Diagnostic,
        None,
        None,
        Some(benchmark_id.to_owned()),
        format!(
            "benchmark {benchmark_id} resolved to {matched} candidate symbols ({code}): no anchoring edge emitted"
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(code.to_owned());
    }
    record
}

/// Builds the error-path `Diagnostic` node for an empty or unparseable
/// criterion output directory.
///
/// This is the ONLY record written on the empty / unparseable paths — never a
/// `BenchmarkRun`. `Diagnostic` markers legitimately stand alone under
/// `eg validate`.
#[must_use]
pub fn bench_error_diagnostic(
    session_id: &str,
    commit: &str,
    suite: &str,
    source_root: &str,
    error: BenchCaptureError,
) -> GraphRecord {
    let code = error.code();
    let message = match error {
        BenchCaptureError::Empty => format!(
            "empty criterion output at {source_root} for suite {suite}: no BenchmarkRun captured"
        ),
        BenchCaptureError::Unparseable => format!(
            "unparseable criterion estimates at {source_root} for suite {suite}: no valid mean point estimate; no BenchmarkRun captured"
        ),
    };
    let mut record = GraphRecord::node(
        stable_id(&["node", "diagnostic", code, session_id, commit, suite]),
        NodeKind::Diagnostic,
        Some(source_root.to_owned()),
        None,
        Some(suite.to_owned()),
        message,
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(code.to_owned());
    }
    record
}

/// Builds a `BenchmarkRun` record batch from parsed criterion estimates and an
/// optional code graph.
///
/// Emits exactly one `BenchmarkRun` node per benchmark (sorted by benchmark
/// id). With a code graph, each benchmark whose final `/`-segment resolves to
/// EXACTLY ONE `Symbol` by name mints a `MENTIONS_SYMBOL` edge and, when the
/// symbol's `File` is present, a `TOUCHED_FILE` edge. Zero or two-plus matches
/// mint NO edge and a resolution `Diagnostic` instead; the benchmark id joins
/// the outcome's `unresolved` list either way (unresolved is never dropped
/// silently — the CLI reports it in the envelope's `unresolved` section).
/// Without a code graph the batch is self-contained and every benchmark is
/// reported unresolved.
///
/// The code graph's own nodes are not re-emitted; union the batch with the
/// code graph to resolve edge endpoints.
///
/// # Errors
///
/// Returns [`BenchCaptureError::Unparseable`] when the normalized summary
/// fails to serialize (infallible in practice — every float it carries is
/// pre-filtered finite).
pub fn build_bench_run_records(
    req: &BenchmarkRunRequest,
    parse: &BenchCaptureParse,
    code_graph: Option<&[GraphRecord]>,
) -> Result<BenchRunOutcome, BenchCaptureError> {
    let mut records: Vec<GraphRecord> = Vec::new();
    let mut benchmark_ids: Vec<String> = Vec::new();
    let mut record_ids: Vec<String> = Vec::new();
    let mut verdicts: Vec<BenchmarkVerdict> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();

    let mut resolved_count = 0u64;
    let mut unresolved_count = 0u64;
    let mut ambiguous_count = 0u64;

    let index = code_graph.map(CodeGraphIndex::build);

    for bench in parse.benchmarks() {
        let record_id = verification_stable_id(&[
            "benchmark_run",
            req.session_id,
            req.commit,
            req.suite,
            &bench.id,
        ]);
        records.push(build_benchmark_run_node(req, bench, &record_id)?);
        benchmark_ids.push(bench.id.clone());
        record_ids.push(record_id.clone());
        verdicts.push(bench.verdict);

        match index.as_ref() {
            None => {
                // No code graph: nothing can resolve; the id is still listed
                // so the envelope's `unresolved` section stays complete.
                unresolved.push(bench.id.clone());
            }
            Some(index) => {
                let segment = final_segment(&bench.id);
                match index.symbols_by_name.get(segment).map(Vec::as_slice) {
                    Some([symbol]) => {
                        resolved_count += 1;
                        let Some((symbol_id, symbol_path)) = symbol_handles(symbol) else {
                            // Defensive: the index only stores Node records,
                            // so this branch is unreachable in practice — but
                            // a non-Node record must never mint a wrong edge.
                            unresolved_count += 1;
                            unresolved.push(bench.id.clone());
                            records.push(resolution_diagnostic(
                                req,
                                &bench.id,
                                BENCH_SYMBOL_UNRESOLVED_CODE,
                                0,
                            ));
                            continue;
                        };
                        records.push(GraphRecord::edge(
                            EdgeLabel::MentionsSymbol,
                            record_id.clone(),
                            symbol_id.to_owned(),
                            None,
                            format!("benchmark run {} MENTIONS_SYMBOL {symbol_id}", bench.id),
                        ));
                        if let Some(path) = symbol_path
                            && let Some(file_id) = index.file_id_by_path.get(path)
                        {
                            records.push(GraphRecord::edge(
                                EdgeLabel::TouchedFile,
                                record_id.clone(),
                                (*file_id).to_owned(),
                                None,
                                format!("benchmark run {} TOUCHED_FILE {file_id}", bench.id),
                            ));
                        }
                    }
                    Some(many) if many.len() >= 2 => {
                        ambiguous_count += 1;
                        unresolved.push(bench.id.clone());
                        records.push(resolution_diagnostic(
                            req,
                            &bench.id,
                            BENCH_SYMBOL_AMBIGUOUS_CODE,
                            many.len(),
                        ));
                    }
                    _ => {
                        unresolved_count += 1;
                        unresolved.push(bench.id.clone());
                        records.push(resolution_diagnostic(
                            req,
                            &bench.id,
                            BENCH_SYMBOL_UNRESOLVED_CODE,
                            0,
                        ));
                    }
                }
            }
        }
    }

    // Deduplicate by record ID (stable, keep first) so identical diagnostics
    // collapse and edge re-emission stays idempotent.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    records.retain(|r| seen.insert(r.id().to_owned()));

    Ok(BenchRunOutcome {
        records,
        benchmark_ids,
        record_ids,
        verdicts,
        resolved_count,
        unresolved_count,
        ambiguous_count,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench(mean_ns: f64, verdict: BenchmarkVerdict, delta_pct: Option<f64>) -> ParsedBenchmark {
        ParsedBenchmark {
            id: "unit_bench".to_owned(),
            mean_ns,
            median_ns: Some(mean_ns - 1.0),
            ci_lower_ns: Some(mean_ns - 10.0),
            ci_upper_ns: Some(mean_ns + 10.0),
            ci_level: Some(0.95),
            base_mean_ns: Some(1000.0),
            verdict,
            delta_pct,
            new_artifact_hash: "abc".to_owned(),
            new_estimates_path: "unit_bench/new/estimates.json".to_owned(),
            base_estimates_path: Some("unit_bench/base/estimates.json".to_owned()),
        }
    }

    fn parse_with(benches: Vec<ParsedBenchmark>) -> BenchCaptureParse {
        let mut sorted = benches;
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        BenchCaptureParse { benchmarks: sorted }
    }

    const fn req<'a>() -> BenchmarkRunRequest<'a> {
        BenchmarkRunRequest {
            session_id: "sess-1",
            commit: "deadbeef",
            suite: "benches",
            command: None,
            executed_at: "2026-07-19T12:00:00Z",
        }
    }

    #[test]
    fn verdict_threshold_edges() {
        // Exactly at the floor: unchanged.
        assert_eq!(
            benchmark_verdict(1010.0, Some(1000.0)),
            (BenchmarkVerdict::Unchanged, Some(1.0))
        );
        // Just past the floor: regression.
        let (v, d) = benchmark_verdict(1010.11, Some(1000.0));
        assert_eq!(v, BenchmarkVerdict::Regression);
        assert!(d.is_some_and(|x| x > 1.0));
        // Rounding is to four decimals.
        let (_, d) = benchmark_verdict(1000.0 / 3.0 * 4.0, Some(1000.0));
        assert_eq!(d, Some(33.3333));
    }

    #[test]
    fn verdict_strings_are_stable() {
        assert_eq!(BenchmarkVerdict::Regression.as_str(), "regression");
        assert_eq!(BenchmarkVerdict::Improvement.as_str(), "improvement");
        assert_eq!(BenchmarkVerdict::Unchanged.as_str(), "unchanged");
        assert_eq!(BenchmarkVerdict::NoBaseline.as_str(), "no_baseline");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(BenchCaptureError::Empty.code(), "empty_bench_output");
        assert_eq!(
            BenchCaptureError::Unparseable.code(),
            "unparseable_bench_output"
        );
    }

    #[test]
    fn normalized_summary_round_trips_through_stdout_handle() {
        let parse = parse_with(vec![bench(
            1250.0,
            BenchmarkVerdict::Regression,
            Some(25.0),
        )]);
        let outcome =
            build_bench_run_records(&req(), &parse, None).expect("build is infallible in tests");
        assert_eq!(outcome.records.len(), 1);
        let view = benchmark_summary_of(&outcome.records[0]).expect("summary must parse");
        assert_eq!(view.benchmark_id, "unit_bench");
        assert_eq!(view.suite, "benches");
        assert!(
            (view.mean_ns - 1250.0).abs() < f64::EPSILON,
            "mean_ns should be exactly 1250.0, got {}",
            view.mean_ns
        );
        assert_eq!(view.unit, "ns");
        assert_eq!(view.verdict, "regression");
        assert_eq!(view.delta_pct, Some(25.0));
    }

    #[test]
    fn summary_view_falls_back_to_node_name() {
        let mut node = GraphRecord::node(
            "verification:v1:fallback".to_owned(),
            NodeKind::BenchmarkRun,
            None,
            None,
            Some("legacy_bench".to_owned()),
            "legacy".to_owned(),
        );
        if let GraphRecord::Node { stdout_handle, .. } = &mut node {
            *stdout_handle = Some(Box::new(output_handle(
                r#"{"format":"other","benchmark_id":"","suite":"s","mean_ns":1.0,"unit":"ns","verdict":"no_baseline"}"#,
            )));
        }
        let view = benchmark_summary_of(&node).expect("summary must parse");
        assert_eq!(view.benchmark_id, "legacy_bench");
    }

    #[test]
    fn summary_view_rejects_non_benchmark_nodes() {
        let node = GraphRecord::node(
            "codegraph:v1:x".to_owned(),
            NodeKind::Symbol,
            None,
            None,
            Some("x".to_owned()),
            "x".to_owned(),
        );
        assert!(benchmark_summary_of(&node).is_none());
    }

    #[test]
    fn build_is_byte_identical_across_runs() {
        let parse = parse_with(vec![
            bench(1250.0, BenchmarkVerdict::Regression, Some(25.0)),
            bench(900.0, BenchmarkVerdict::Improvement, Some(-10.0)),
        ]);
        // Fix the ids (the helper stamps one id).
        let mut benches = parse.benchmarks;
        benches[0].id = "a_bench".to_owned();
        benches[1].id = "b_bench".to_owned();
        let parse = BenchCaptureParse {
            benchmarks: benches,
        };

        let first =
            build_bench_run_records(&req(), &parse, None).expect("build is infallible in tests");
        let second =
            build_bench_run_records(&req(), &parse, None).expect("build is infallible in tests");
        let jsonl = |outcome: &BenchRunOutcome| {
            let mut graph = crate::ir::Graph::new();
            for r in &outcome.records {
                graph.push(r.clone());
            }
            graph.to_jsonl().expect("jsonl must serialize")
        };
        assert_eq!(jsonl(&first), jsonl(&second));
    }

    #[test]
    fn human_summary_mentions_verdict_and_delta() {
        let summary = human_summary(
            &req(),
            &bench(1250.0, BenchmarkVerdict::Regression, Some(25.0)),
        );
        assert!(summary.contains("regression"), "{summary}");
        assert!(summary.contains("+25"), "{summary}");
        let no_base = human_summary(
            &req(),
            &ParsedBenchmark {
                verdict: BenchmarkVerdict::NoBaseline,
                delta_pct: None,
                base_mean_ns: None,
                base_estimates_path: None,
                ..bench(500.0, BenchmarkVerdict::NoBaseline, None)
            },
        );
        assert!(no_base.contains("no baseline"), "{no_base}");
    }

    #[test]
    fn final_segment_uses_last_slash_component() {
        assert_eq!(final_segment("group/sub/bench_name"), "bench_name");
        assert_eq!(final_segment("bench_name"), "bench_name");
    }
}
