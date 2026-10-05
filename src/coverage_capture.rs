// clippy::too_long_first_doc_paragraph fires on this module's doc without a span
// (nursery lint span-reporting bug in clippy 0.1.94); suppress at module level.
#![allow(clippy::too_long_first_doc_paragraph)]
//! Capture a `cargo llvm-cov` run as a citable, deterministic
//! verification-domain `CoverageReport` record (issue #230).
//!
//! This module is CAPTURE-ONLY: it parses the machine-readable coverage
//! artefact (`cargo llvm-cov --json` output, or LCOV) that the caller already
//! captured to a file. It NEVER executes `cargo llvm-cov`. A captured coverage
//! number is a recorded observation of one run — lines executed by some test,
//! never proof the behavior is correct.
//!
//! Raw report text never enters the graph. The only coverage payload that
//! reaches a record is a bounded, normalized summary of percentages, counts,
//! file paths, and function names stored through an [`OutputHandle`]; the raw
//! artefact is referenced by path and content hash only (or through the
//! protected store, never inline).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ir::{
    EdgeLabel, GraphRecord, NodeKind, OutputHandle, Producer, ProducerKind, TemporalMetadata,
    VERIFICATION_SCHEMA_VERSION, stable_id, verification_stable_id,
};

/// Inline-payload ceiling shared with the typed evidence writer: an
/// [`OutputHandle`] stores content inline only when it is at or below this many
/// bytes (16 KiB), otherwise it references the content by hash only.
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

/// The default `--format` value: `cargo llvm-cov --json` output.
pub const LLVM_COV_JSON_FORMAT: &str = "llvm-cov-json";
/// The alternate `--format` value: LCOV (`cargo llvm-cov --lcov`) output.
pub const LLVM_COV_LCOV_FORMAT: &str = "llvm-cov-lcov";

/// Normalized-summary format tag stamped into the `stdout_handle` payload.
/// Format tag of the normalized summary stored in `stdout_handle` — distinct
/// from the input format: the summary is egregore's re-parseable record, not
/// the coverage tool's raw artefact.
pub const NORMALIZED_SUMMARY_FORMAT: &str = "llvm-cov-v1";

/// Default `--tool` value, used as the `tool_id` record-ID input when the
/// caller does not name the tool explicitly.
pub const DEFAULT_TOOL_ID: &str = "cargo-llvm-cov";

/// Diagnostic machine codes (stamped into `symbol_kind` on `Diagnostic` nodes).
pub const EMPTY_COVERAGE_OUTPUT_CODE: &str = "empty_coverage_report";
/// See [`EMPTY_COVERAGE_OUTPUT_CODE`].
pub const UNPARSEABLE_COVERAGE_OUTPUT_CODE: &str = "unparseable_coverage_report";
/// See [`EMPTY_COVERAGE_OUTPUT_CODE`]. The artefact carries the coverage tool's
/// own "command not found" text instead of a coverage report.
pub const COVERAGE_TOOL_MISSING_CODE: &str = "coverage_tool_missing";
/// See [`EMPTY_COVERAGE_OUTPUT_CODE`].
pub const COVERAGE_FILE_UNRESOLVED_CODE: &str = "coverage_file_unresolved";
/// See [`EMPTY_COVERAGE_OUTPUT_CODE`].
pub const COVERAGE_SYMBOL_UNRESOLVED_CODE: &str = "coverage_symbol_unresolved";
/// See [`EMPTY_COVERAGE_OUTPUT_CODE`].
pub const COVERAGE_SYMBOL_AMBIGUOUS_CODE: &str = "coverage_symbol_ambiguous";

/// Case-insensitive markers that identify a "the coverage tool never ran"
/// artefact: the caller redirected the shell's / cargo's own error text into
/// the artefact file instead of a coverage report.
const TOOL_MISSING_MARKERS: &[&str] = &[
    "no such command",
    "command not found",
    "is not recognized as an internal or external command",
];

/// Reason a coverage artefact could not be captured as a `CoverageReport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageCaptureError {
    /// The input file was empty (or whitespace only).
    Empty,
    /// The input was not a parseable coverage artefact of the declared format,
    /// or it carried zero measurable files.
    Unparseable,
    /// The input carries the coverage tool's own "not installed / not found"
    /// error text instead of a coverage report.
    ToolMissing,
}

impl CoverageCaptureError {
    /// Returns the stable machine code for this error.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Empty => EMPTY_COVERAGE_OUTPUT_CODE,
            Self::Unparseable => UNPARSEABLE_COVERAGE_OUTPUT_CODE,
            Self::ToolMissing => COVERAGE_TOOL_MISSING_CODE,
        }
    }
}

impl std::fmt::Display for CoverageCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty coverage artefact"),
            Self::Unparseable => write!(f, "unparseable coverage artefact"),
            Self::ToolMissing => write!(f, "coverage tool missing or not executable"),
        }
    }
}

impl std::error::Error for CoverageCaptureError {}

// ── Parsed outcome model ────────────────────────────────────────────────────

/// Covered/total/percentage triple for one coverage dimension.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoverageCounts {
    /// Number of coverable units (lines, branches, regions, functions).
    pub total: u64,
    /// Number of executed units.
    pub covered: u64,
    /// Coverage percentage: the artefact's reported value when present and
    /// finite, else `covered / total * 100` rounded to four decimals
    /// (0.0 when `total` is 0).
    pub percent: f64,
}

impl CoverageCounts {
    #[allow(clippy::cast_precision_loss)]
    fn new(total: u64, covered: u64, reported_percent: Option<f64>) -> Self {
        let percent = reported_percent
            .filter(|v| v.is_finite())
            .unwrap_or_else(|| {
                if total == 0 {
                    0.0
                } else {
                    (covered as f64 / total as f64 * 100.0 * 10_000.0).round() / 10_000.0
                }
            });
        Self {
            total,
            covered,
            percent,
        }
    }

    const fn zero() -> Self {
        Self {
            total: 0,
            covered: 0,
            percent: 0.0,
        }
    }
}

/// One measured function (LCOV `FN`/`FNDA` records carry names; the llvm-cov
/// JSON export carries no per-function names, so this is `None` there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFunction {
    /// Function name as reported by the artefact.
    pub name: String,
    /// Whether the function was executed at least once.
    pub covered: bool,
}

/// One measured file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
    /// Path as reported by the artefact (absolute for llvm-cov).
    pub path: String,
    /// Repo-relative path when the caller supplied `--repo-root` and the path
    /// fell under it.
    pub repo_relative_path: Option<String>,
    /// Line coverage.
    pub lines: CoverageCounts,
    /// Branch coverage (LCOV `BRDA` records).
    pub branches: Option<CoverageCounts>,
    /// Region coverage (llvm-cov JSON `regions` summary; llvm-cov's region
    /// model approximates branch coverage — reported as regions, honestly).
    pub regions: Option<CoverageCounts>,
    /// Function coverage (summary counts).
    pub functions: CoverageCounts,
    /// Per-function execution flags, when the artefact names functions.
    pub function_details: Vec<ParsedFunction>,
}

/// The deterministic result of parsing a coverage artefact.
#[derive(Debug, Clone)]
pub struct CoverageCaptureParse {
    files: Vec<ParsedFile>,
    totals_lines: CoverageCounts,
    totals_branches: Option<CoverageCounts>,
    totals_regions: Option<CoverageCounts>,
    totals_functions: CoverageCounts,
}

impl CoverageCaptureParse {
    /// The parsed files, sorted by repo-relative path (or raw path when not
    /// relativized).
    #[must_use]
    pub fn files(&self) -> &[ParsedFile] {
        &self.files
    }

    /// Overall line coverage.
    #[must_use]
    pub const fn totals_lines(&self) -> CoverageCounts {
        self.totals_lines
    }

    /// Overall branch coverage (LCOV only).
    #[must_use]
    pub const fn totals_branches(&self) -> Option<CoverageCounts> {
        self.totals_branches
    }

    /// Overall region coverage (llvm-cov JSON only).
    #[must_use]
    pub const fn totals_regions(&self) -> Option<CoverageCounts> {
        self.totals_regions
    }

    /// Overall function coverage.
    #[must_use]
    pub const fn totals_functions(&self) -> CoverageCounts {
        self.totals_functions
    }

    /// Number of measured files.
    #[must_use]
    pub const fn file_count(&self) -> usize {
        self.files.len()
    }
}

// ── llvm-cov JSON parsing ───────────────────────────────────────────────────

/// A tolerant view of one llvm-cov summary counter triple. Unknown extra
/// fields (llvm-cov versions carry more) are ignored.
#[derive(Debug, Deserialize)]
struct RawCounts {
    count: Option<u64>,
    covered: Option<u64>,
    percent: Option<f64>,
}

/// A tolerant view of an llvm-cov per-file / totals summary.
#[derive(Debug, Deserialize)]
struct RawSummary {
    lines: Option<RawCounts>,
    functions: Option<RawCounts>,
    regions: Option<RawCounts>,
    branches: Option<RawCounts>,
}

/// A tolerant view of one llvm-cov JSON export file entry.
#[derive(Debug, Deserialize)]
struct RawExportFile {
    filename: Option<String>,
    summary: Option<RawSummary>,
}

/// A tolerant view of one llvm-cov JSON export `data` element.
#[derive(Debug, Deserialize)]
struct RawExportData {
    files: Option<Vec<RawExportFile>>,
    totals: Option<RawSummary>,
}

/// A tolerant view of the `cargo llvm-cov --json` top-level shape.
#[derive(Debug, Deserialize)]
struct RawExport {
    data: Option<Vec<RawExportData>>,
}

fn counts_of(raw: Option<&RawCounts>) -> CoverageCounts {
    raw.map_or_else(CoverageCounts::zero, counts_from)
}

fn counts_from(raw: &RawCounts) -> CoverageCounts {
    CoverageCounts::new(
        raw.count.unwrap_or(0),
        raw.covered.unwrap_or(0),
        raw.percent,
    )
}

/// Parses `cargo llvm-cov --json` output into a deterministic
/// [`CoverageCaptureParse`].
///
/// # Errors
///
/// Returns [`CoverageCaptureError::Empty`] for empty / whitespace-only input,
/// [`CoverageCaptureError::ToolMissing`] when the input carries the coverage
/// tool's own not-installed error text, and [`CoverageCaptureError::Unparseable`]
/// for invalid JSON, a missing `data` array, or zero measurable files.
pub fn parse_llvm_cov_json(
    input: &str,
    repo_root: Option<&str>,
) -> Result<CoverageCaptureParse, CoverageCaptureError> {
    if input.trim().is_empty() {
        return Err(CoverageCaptureError::Empty);
    }
    if tool_missing(input) {
        return Err(CoverageCaptureError::ToolMissing);
    }
    let export: RawExport =
        serde_json::from_str(input).map_err(|_| CoverageCaptureError::Unparseable)?;
    let data = export.data.ok_or(CoverageCaptureError::Unparseable)?;

    let mut files: Vec<ParsedFile> = Vec::new();
    let mut totals_lines = CoverageCounts::zero();
    let mut totals_branches: Option<CoverageCounts> = None;
    let mut totals_regions: Option<CoverageCounts> = None;
    let mut totals_functions = CoverageCounts::zero();

    for element in &data {
        if let Some(totals) = &element.totals {
            // The first totals block wins; later ones (multi-binary exports)
            // must not silently overwrite it. Deterministic: data order.
            if totals_lines.total == 0 && totals_functions.total == 0 {
                totals_lines = counts_of(totals.lines.as_ref());
                totals_functions = counts_of(totals.functions.as_ref());
                if totals_branches.is_none() {
                    totals_branches = totals.branches.as_ref().map(counts_from);
                }
                if totals_regions.is_none() {
                    totals_regions = totals.regions.as_ref().map(counts_from);
                }
            }
        }
        for file in element.files.iter().flatten() {
            let Some(filename) = file.filename.as_deref() else {
                continue;
            };
            let summary = file.summary.as_ref();
            files.push(ParsedFile {
                path: filename.to_owned(),
                repo_relative_path: relativize(filename, repo_root),
                lines: counts_of(summary.and_then(|s| s.lines.as_ref())),
                branches: summary.and_then(|s| s.branches.as_ref()).map(counts_from),
                regions: summary.and_then(|s| s.regions.as_ref()).map(counts_from),
                functions: counts_of(summary.and_then(|s| s.functions.as_ref())),
                // The llvm-cov JSON export carries no per-function names.
                function_details: Vec::new(),
            });
        }
    }

    if files.is_empty() {
        return Err(CoverageCaptureError::Unparseable);
    }
    // When the artefact carried no usable totals block, derive totals from the
    // files so the summary never claims 0/0 on a non-empty report.
    if totals_lines.total == 0 && totals_functions.total == 0 {
        totals_lines = sum_counts(files.iter().map(|f| f.lines));
        totals_functions = sum_counts(files.iter().map(|f| f.functions));
        totals_regions = sum_opt_counts(files.iter().filter_map(|f| f.regions));
        totals_branches = sum_opt_counts(files.iter().filter_map(|f| f.branches));
    }

    files.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));
    Ok(CoverageCaptureParse {
        files,
        totals_lines,
        totals_branches,
        totals_regions,
        totals_functions,
    })
}

// ── LCOV parsing ────────────────────────────────────────────────────────────

/// Accumulates one LCOV `SF:` record.
#[derive(Debug, Default)]
struct LcovFileAccum {
    path: String,
    line_counts: BTreeMap<u64, u64>,
    branch_taken: BTreeMap<(u64, u64, u64), bool>,
    function_counts: BTreeMap<String, u64>,
}

/// Parses LCOV (`cargo llvm-cov --lcov`) output into a deterministic
/// [`CoverageCaptureParse`].
///
/// Recognized records: `SF:`, `FN:`, `FNDA:`, `DA:`, `BRDA:`,
/// `end_of_record`. `TN:`, `LF:`/`LH:`, `FNF:`/`FNH:`, `BRF:`/`BRH:` are
/// accepted and ignored (totals are recomputed from the raw records so a
/// hand-edited summary can never desync the figures).
///
/// # Errors
///
/// Same contract as [`parse_llvm_cov_json`].
#[allow(clippy::too_many_lines)]
pub fn parse_lcov(
    input: &str,
    repo_root: Option<&str>,
) -> Result<CoverageCaptureParse, CoverageCaptureError> {
    if input.trim().is_empty() {
        return Err(CoverageCaptureError::Empty);
    }
    if tool_missing(input) {
        return Err(CoverageCaptureError::ToolMissing);
    }

    let mut files: Vec<LcovFileAccum> = Vec::new();
    let mut current: Option<LcovFileAccum> = None;
    let mut saw_records = false;

    for raw_line in input.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix("SF:") {
            if let Some(accum) = current.take() {
                files.push(accum);
            }
            current = Some(LcovFileAccum {
                path: path.to_owned(),
                ..LcovFileAccum::default()
            });
            saw_records = true;
        } else if line == "end_of_record" {
            if let Some(accum) = current.take() {
                files.push(accum);
            }
            saw_records = true;
        } else if let Some(accum) = current.as_mut() {
            if let Some(rest) = line.strip_prefix("FNDA:") {
                // `FNDA:<count>,<name>` — the name may itself contain commas
                // (Rust paths do not, but be conservative: split once).
                if let Some((count_text, name)) = rest.split_once(',')
                    && let Ok(count) = count_text.trim().parse::<u64>()
                {
                    accum
                        .function_counts
                        .entry(name.trim().to_owned())
                        .and_modify(|c| *c = (*c).max(count))
                        .or_insert(count);
                }
            } else if let Some(rest) = line.strip_prefix("DA:") {
                if let Some((lineno_text, count_text)) = rest.split_once(',')
                    && let (Ok(lineno), Ok(count)) = (
                        lineno_text.trim().parse::<u64>(),
                        count_text
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .parse::<u64>(),
                    )
                {
                    accum
                        .line_counts
                        .entry(lineno)
                        .and_modify(|c| *c = (*c).max(count))
                        .or_insert(count);
                }
            } else if let Some(rest) = line.strip_prefix("BRDA:") {
                // `BRDA:<line>,<block>,<branch>,<taken>`; taken is `-` when
                // never executed.
                let parts: Vec<&str> = rest.split(',').collect();
                if parts.len() >= 4
                    && let (Ok(lineno), Ok(block), Ok(branch)) = (
                        parts[0].trim().parse::<u64>(),
                        parts[1].trim().parse::<u64>(),
                        parts[2].trim().parse::<u64>(),
                    )
                {
                    let taken = parts[3].trim();
                    let executed = taken != "-" && taken != "0";
                    accum
                        .branch_taken
                        .entry((lineno, block, branch))
                        .and_modify(|e| *e = *e || executed)
                        .or_insert(executed);
                }
            }
            // `FN:`, `TN:`, `LF:`, `LH:`, `FNF:`, `FNH:`, `BRF:`, `BRH:` are
            // accepted and ignored; totals are recomputed below.
        } else if !saw_records {
            // Content before the first `SF:` that is not a recognized global
            // record: not LCOV.
            if !line.starts_with("TN:") {
                return Err(CoverageCaptureError::Unparseable);
            }
        }
    }
    if let Some(accum) = current.take() {
        files.push(accum);
    }

    if files.is_empty() {
        return Err(CoverageCaptureError::Unparseable);
    }

    let mut parsed: Vec<ParsedFile> = files
        .into_iter()
        .map(|accum| {
            let line_total = accum.line_counts.len() as u64;
            let line_covered = accum.line_counts.values().filter(|&&c| c > 0).count() as u64;
            let branch_total = accum.branch_taken.len() as u64;
            let branch_covered = accum.branch_taken.values().filter(|&&e| e).count() as u64;
            let fn_total = accum.function_counts.len() as u64;
            let fn_covered = accum.function_counts.values().filter(|&&c| c > 0).count() as u64;
            let mut function_details: Vec<ParsedFunction> = accum
                .function_counts
                .into_iter()
                .map(|(name, count)| ParsedFunction {
                    name,
                    covered: count > 0,
                })
                .collect();
            function_details.sort_by(|a, b| a.name.cmp(&b.name));
            ParsedFile {
                path: accum.path.clone(),
                repo_relative_path: relativize(&accum.path, repo_root),
                lines: CoverageCounts::new(line_total, line_covered, None),
                branches: Some(CoverageCounts::new(branch_total, branch_covered, None)),
                regions: None,
                functions: CoverageCounts::new(fn_total, fn_covered, None),
                function_details,
            }
        })
        .collect();
    parsed.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));

    let totals_lines = sum_counts(parsed.iter().map(|f| f.lines));
    let totals_functions = sum_counts(parsed.iter().map(|f| f.functions));
    let totals_branches = sum_opt_counts(parsed.iter().filter_map(|f| f.branches));

    Ok(CoverageCaptureParse {
        files: parsed,
        totals_lines,
        totals_branches,
        totals_regions: None,
        totals_functions,
    })
}

/// Sort key for deterministic file order: repo-relative path when known, else
/// the raw artefact path.
fn sort_key(file: &ParsedFile) -> &str {
    file.repo_relative_path.as_deref().unwrap_or(&file.path)
}

fn sum_counts(counts: impl Iterator<Item = CoverageCounts>) -> CoverageCounts {
    let mut total = 0u64;
    let mut covered = 0u64;
    for c in counts {
        total += c.total;
        covered += c.covered;
    }
    CoverageCounts::new(total, covered, None)
}

fn sum_opt_counts(counts: impl Iterator<Item = CoverageCounts>) -> Option<CoverageCounts> {
    let mut total = 0u64;
    let mut covered = 0u64;
    let mut any = false;
    for c in counts {
        any = true;
        total += c.total;
        covered += c.covered;
    }
    any.then(|| CoverageCounts::new(total, covered, None))
}

/// Whether the artefact text carries the coverage tool's own not-installed
/// error instead of a coverage report (case-insensitive).
fn tool_missing(input: &str) -> bool {
    let lowered = input.to_lowercase();
    TOOL_MISSING_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Relativizes an artefact path against `--repo-root`, returning `None` when
/// no root was supplied or the path falls outside it.
fn relativize(path: &str, repo_root: Option<&str>) -> Option<String> {
    let root = repo_root?;
    let stripped = std::path::Path::new(path)
        .strip_prefix(std::path::Path::new(root))
        .ok()?;
    if stripped.as_os_str().is_empty() {
        return None;
    }
    Some(
        stripped
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_owned(),
    )
}

// ── Record building ─────────────────────────────────────────────────────────

/// Run metadata for a captured coverage run. All string fields are
/// caller-supplied; none are read from a wall clock.
#[derive(Debug, Clone)]
pub struct CoverageReportRequest<'a> {
    /// Stable session identity (part of the record ID).
    pub session_id: &'a str,
    /// Commit handle / external identifier (part of the record ID).
    pub commit: &'a str,
    /// Optional suite/target name (node `name`, not part of the record ID).
    pub suite: Option<&'a str>,
    /// The exact command that produced the artefact. Stored, never executed.
    pub command: &'a str,
    /// The coverage command's exit status.
    pub exit_code: i64,
    /// Caller-supplied RFC 3339 timestamp (validated by the caller).
    pub executed_at: &'a str,
    /// Tool identity, e.g. `cargo-llvm-cov` (part of the record ID).
    pub tool: &'a str,
    /// Optional tool version.
    pub tool_version: Option<&'a str>,
    /// The artefact format that was parsed (`llvm-cov-json` / `llvm-cov-lcov`).
    pub artifact_format: &'a str,
    /// The `--input` path, recorded as the source artifact path.
    pub source_artifact_path: &'a str,
    /// BLAKE3 hex hash of the raw input bytes.
    pub source_artifact_hash: &'a str,
}

/// Outcome of building a `CoverageReport` record batch.
#[derive(Debug)]
pub struct CoverageReportOutcome {
    /// The stable `CoverageReport` record ID (citable handle).
    pub record_id: String,
    /// All emitted records (the `CoverageReport` node, diagnostics, edges).
    pub records: Vec<GraphRecord>,
    /// Number of files that resolved to a code-graph `File` (with `--graph`).
    pub resolved_file_count: u64,
    /// Number of files that did not resolve.
    pub unresolved_file_count: u64,
    /// Files that did not resolve to a code-graph `File`, sorted — the
    /// distinct `unresolved` set. Never dropped silently.
    pub unresolved: Vec<String>,
    /// Number of functions that resolved to exactly one `Symbol`.
    pub resolved_function_count: u64,
    /// Number of functions that resolved to zero symbols.
    pub unresolved_function_count: u64,
    /// Number of functions that resolved to two or more symbols.
    pub ambiguous_function_count: u64,
}

#[derive(Serialize, Deserialize)]
struct NormalizedCounts {
    total: u64,
    covered: u64,
    percent: f64,
}

impl From<CoverageCounts> for NormalizedCounts {
    fn from(c: CoverageCounts) -> Self {
        Self {
            total: c.total,
            covered: c.covered,
            percent: c.percent,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct NormalizedFunction {
    name: String,
    covered: bool,
}

#[derive(Serialize, Deserialize)]
struct NormalizedFile {
    /// Repo-relative path when resolved, else the raw artefact path.
    path: String,
    resolved: bool,
    lines: NormalizedCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    branches: Option<NormalizedCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regions: Option<NormalizedCounts>,
    functions: NormalizedCounts,
    /// Per-function execution flags (identifiers only — never source text).
    /// Present only when the artefact names functions (LCOV).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    function_details: Vec<NormalizedFunction>,
}

#[derive(Serialize, Deserialize)]
struct NormalizedSummary<'a> {
    format: &'a str,
    tool: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_version: Option<&'a str>,
    artifact_format: &'a str,
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    suite: Option<&'a str>,
    exit_code: i64,
    status: &'a str,
    files_measured: u64,
    lines: NormalizedCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    branches: Option<NormalizedCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regions: Option<NormalizedCounts>,
    functions: NormalizedCounts,
    files: Vec<NormalizedFile>,
}

/// A machine-readable view of a captured `CoverageReport`'s normalized
/// summary, for the query lane. Parses `stdout_handle.inline`; returns `None`
/// when the record carries no parseable summary.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CoverageSummaryView {
    /// Tool identity as captured.
    pub tool: String,
    /// Overall line coverage percent.
    pub line_percent: f64,
    /// Overall branch/region coverage percent, when the artefact had it.
    pub branch_percent: Option<f64>,
    /// Number of measured files.
    pub files_measured: u64,
}

/// Reads the normalized summary off a `CoverageReport` node for the query
/// lane. Returns `None` for non-`CoverageReport` nodes or records with no
/// parseable summary.
#[must_use]
pub fn coverage_summary_of(record: &GraphRecord) -> Option<CoverageSummaryView> {
    let GraphRecord::Node {
        kind,
        stdout_handle,
        ..
    } = record
    else {
        return None;
    };
    if *kind != NodeKind::CoverageReport {
        return None;
    }
    let inline = stdout_handle.as_ref()?.inline.as_ref()?;
    let parsed: NormalizedSummary<'_> = serde_json::from_str(inline).ok()?;
    let branch_percent = parsed
        .branches
        .as_ref()
        .or(parsed.regions.as_ref())
        .map(|c| c.percent);
    Some(CoverageSummaryView {
        tool: parsed.tool.to_owned(),
        line_percent: parsed.lines.percent,
        branch_percent,
        files_measured: parsed.files_measured,
    })
}

/// Builds the deterministic normalized-summary JSON stored in `stdout_handle`.
/// `resolved` marks, per file, whether it anchored to a code-graph `File`.
fn normalized_summary_json(
    req: &CoverageReportRequest,
    parse: &CoverageCaptureParse,
    status: &str,
    resolved_paths: &BTreeSet<String>,
) -> String {
    let files: Vec<NormalizedFile> = parse
        .files()
        .iter()
        .map(|f| {
            let display = f
                .repo_relative_path
                .clone()
                .unwrap_or_else(|| f.path.clone());
            NormalizedFile {
                resolved: resolved_paths.contains(&display),
                path: display,
                lines: f.lines.into(),
                branches: f.branches.map(NormalizedCounts::from),
                regions: f.regions.map(NormalizedCounts::from),
                functions: f.functions.into(),
                function_details: f
                    .function_details
                    .iter()
                    .map(|d| NormalizedFunction {
                        name: d.name.clone(),
                        covered: d.covered,
                    })
                    .collect(),
            }
        })
        .collect();
    let summary = NormalizedSummary {
        format: NORMALIZED_SUMMARY_FORMAT,
        tool: req.tool,
        tool_version: req.tool_version,
        artifact_format: req.artifact_format,
        command: req.command,
        suite: req.suite,
        exit_code: req.exit_code,
        status,
        files_measured: parse.file_count() as u64,
        lines: parse.totals_lines().into(),
        branches: parse.totals_branches().map(NormalizedCounts::from),
        regions: parse.totals_regions().map(NormalizedCounts::from),
        functions: parse.totals_functions().into(),
        files,
    };
    serde_json::to_string(&summary).expect("normalized summary serialization is infallible")
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
fn coverage_capture_producer(executed_at: &str) -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::ObservationWriter,
        producer_components: BTreeMap::new(),
        producer_started_at: executed_at.to_owned(),
    }
}

/// Formats a percentage deterministically: up to four decimals, no trailing
/// noise.
fn fmt_pct(value: f64) -> String {
    let rounded = (value * 10_000.0).round() / 10_000.0;
    let text = format!("{rounded:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Human-readable one-line summary, e.g. `coverage report (cargo-llvm-cov):
/// 87.42% lines (1234/1411), 76.1% branches (…), 12 files measured`.
fn human_summary(req: &CoverageReportRequest, parse: &CoverageCaptureParse) -> String {
    use std::fmt::Write as _;
    let lines = parse.totals_lines();
    let mut s = format!(
        "coverage report ({}): {}% lines ({}/{})",
        req.tool,
        fmt_pct(lines.percent),
        lines.covered,
        lines.total
    );
    if let Some(branches) = parse.totals_branches() {
        let _ = write!(
            s,
            ", {}% branches ({}/{})",
            fmt_pct(branches.percent),
            branches.covered,
            branches.total
        );
    } else if let Some(regions) = parse.totals_regions() {
        let _ = write!(
            s,
            ", {}% regions ({}/{})",
            fmt_pct(regions.percent),
            regions.covered,
            regions.total
        );
    }
    let functions = parse.totals_functions();
    let _ = write!(
        s,
        ", {}% functions ({}/{})",
        fmt_pct(functions.percent),
        functions.covered,
        functions.total
    );
    let _ = write!(s, ", {} files measured", parse.file_count());
    s
}

/// Coverage-run status: the coverage command's own exit status. A nonzero
/// exit means the coverage data itself is suspect — never a silent pass.
const fn run_status(exit_code: i64) -> &'static str {
    if exit_code == 0 { "pass" } else { "fail" }
}

/// Builds the stable `CoverageReport` node.
fn build_coverage_report_node(
    req: &CoverageReportRequest,
    parse: &CoverageCaptureParse,
    record_id: &str,
    resolved_paths: &BTreeSet<String>,
) -> GraphRecord {
    let status = run_status(req.exit_code);
    let name = Some(req.suite.map_or_else(
        || {
            req.tool_version.map_or_else(
                || req.tool.to_owned(),
                |version| format!("{} {version}", req.tool),
            )
        },
        str::to_owned,
    ));
    let mut node = GraphRecord::node(
        record_id.to_owned(),
        NodeKind::CoverageReport,
        None,
        None,
        name,
        human_summary(req, parse),
    );
    let handle = output_handle(&normalized_summary_json(req, parse, status, resolved_paths));
    if let GraphRecord::Node {
        schema_version,
        domain,
        verification_kind,
        status: node_status,
        exit_code,
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
        *verification_kind = Some("coverage_report".to_owned());
        *node_status = Some(status.to_owned());
        *exit_code = Some(req.exit_code);
        *executed_at = Some(req.executed_at.to_owned());
        *source_artifact_path = Some(req.source_artifact_path.to_owned());
        *source_artifact_hash = Some(req.source_artifact_hash.to_owned());
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
        *producer = Some(coverage_capture_producer(req.executed_at));
    }
    node
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

    /// Resolves a repo-relative path to a File record id: exact match first,
    /// else the longest repo-relative path that suffix-matches the artefact
    /// path at a `/` boundary. A tie between two different paths of equal
    /// length is ambiguous → `None`.
    fn resolve_file(&self, display_path: &str) -> Option<&'a str> {
        if let Some(id) = self.file_id_by_path.get(display_path) {
            return Some(id);
        }
        let mut best: Option<(&'a str, usize)> = None;
        let mut tied = false;
        for (path, id) in &self.file_id_by_path {
            if display_path == *path {
                return Some(id);
            }
            if display_path.len() > path.len()
                && display_path.ends_with(*path)
                && display_path.as_bytes()[display_path.len() - path.len() - 1] == b'/'
            {
                match best {
                    None => best = Some((id, path.len())),
                    Some((_, len)) if path.len() > len => {
                        best = Some((id, path.len()));
                        tied = false;
                    }
                    Some((_, len)) if path.len() == len => {
                        tied = true;
                    }
                    _ => {}
                }
            }
        }
        if tied { None } else { best.map(|(id, _)| id) }
    }
}

/// Reads `(id, repo_relative_path)` off a Symbol record.
fn symbol_handles(record: &GraphRecord) -> (&str, Option<&str>) {
    match record {
        GraphRecord::Node {
            id,
            repo_relative_path,
            ..
        } => (id.as_str(), repo_relative_path.as_deref()),
        _ => unreachable!("index only stores Node records"),
    }
}

/// The final `::`-segment of a (possibly generic/closure-mangled) function
/// name — the conservative resolution key.
fn final_segment(name: &str) -> &str {
    // Strip a trailing closure/generic suffix heuristically is unsafe; keep the
    // raw final segment and let the exactly-one-match rule stay conservative.
    name.rsplit("::").next().unwrap_or(name)
}

/// Builds a codegraph-domain resolution `Diagnostic` for a file or function
/// that resolved to zero or two-plus graph records. Non-orphan by doctrine:
/// `Diagnostic` markers are permitted to stand alone (`eg validate`), matching
/// the test/bench capture lanes.
fn resolution_diagnostic(
    req: &CoverageReportRequest,
    display: &str,
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
            req.tool,
            display,
        ]),
        NodeKind::Diagnostic,
        None,
        None,
        Some(display.to_owned()),
        format!(
            "coverage {display} resolved to {matched} candidate graph records ({code}): no anchoring edge emitted"
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(code.to_owned());
    }
    record
}

/// Builds the error-path `Diagnostic` node for an empty, unparseable, or
/// tool-missing artefact.
///
/// This is the ONLY record written on the error paths — never a
/// `CoverageReport`. `Diagnostic` markers legitimately stand alone under
/// `eg validate`.
#[must_use]
pub fn coverage_error_diagnostic(
    session_id: &str,
    commit: &str,
    tool: &str,
    source_artifact_path: &str,
    error: CoverageCaptureError,
) -> GraphRecord {
    let code = error.code();
    let message = match error {
        CoverageCaptureError::Empty => {
            format!("empty coverage artefact from {tool}: no CoverageReport captured")
        }
        CoverageCaptureError::Unparseable => format!(
            "unparseable coverage artefact from {tool}: no measurable files; no CoverageReport captured"
        ),
        CoverageCaptureError::ToolMissing => format!(
            "coverage tool {tool} missing or not executable (artefact carries the tool's own error text): no CoverageReport captured"
        ),
    };
    let mut record = GraphRecord::node(
        stable_id(&["node", "diagnostic", code, session_id, commit, tool]),
        NodeKind::Diagnostic,
        Some(source_artifact_path.to_owned()),
        None,
        Some(tool.to_owned()),
        message,
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(code.to_owned());
    }
    record
}

/// Builds a `CoverageReport` record batch from a parsed artefact and an
/// optional code graph.
///
/// Emits exactly one `CoverageReport` node with the stable ID
/// `verification_stable_id(["coverage_report", session_id, commit, tool])`.
/// With a code graph, each measured file that resolves to a `File` record
/// mints a `TOUCHED_FILE` edge, and each named function whose final
/// `::`-segment resolves to EXACTLY ONE `Symbol` mints a `MENTIONS_SYMBOL`
/// edge. Files that do not resolve join the outcome's `unresolved` set and get
/// a `coverage_file_unresolved` diagnostic — never silently dropped, never a
/// wrong edge. Without a code graph the batch is self-contained and every
/// measured file is reported unresolved.
///
/// The code graph's own nodes are not re-emitted; union the batch with the
/// code graph to resolve edge endpoints.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn build_coverage_report_records(
    req: &CoverageReportRequest,
    parse: &CoverageCaptureParse,
    code_graph: Option<&[GraphRecord]>,
) -> CoverageReportOutcome {
    let record_id =
        verification_stable_id(&["coverage_report", req.session_id, req.commit, req.tool]);

    let index = code_graph.map(CodeGraphIndex::build);

    // First pass: resolve every file so the node's normalized summary can
    // stamp each file's `resolved` flag deterministically.
    let mut resolved_paths: BTreeSet<String> = BTreeSet::new();
    let mut file_ids: Vec<Option<String>> = Vec::with_capacity(parse.file_count());
    let mut unresolved: Vec<String> = Vec::new();
    let mut resolved_file_count = 0u64;
    let mut unresolved_file_count = 0u64;

    for file in parse.files() {
        let display = file
            .repo_relative_path
            .clone()
            .unwrap_or_else(|| file.path.clone());
        let file_id = index.as_ref().and_then(|idx| idx.resolve_file(&display));
        if let Some(id) = file_id {
            resolved_file_count += 1;
            resolved_paths.insert(display.clone());
            file_ids.push(Some(id.to_owned()));
        } else {
            unresolved_file_count += 1;
            unresolved.push(display.clone());
            file_ids.push(None);
        }
    }

    let mut records: Vec<GraphRecord> = Vec::new();
    records.push(build_coverage_report_node(
        req,
        parse,
        &record_id,
        &resolved_paths,
    ));

    let mut resolved_function_count = 0u64;
    let mut unresolved_function_count = 0u64;
    let mut ambiguous_function_count = 0u64;

    for (file, file_id) in parse.files().iter().zip(file_ids.iter()) {
        let display = file
            .repo_relative_path
            .clone()
            .unwrap_or_else(|| file.path.clone());
        match file_id {
            Some(id) => {
                records.push(GraphRecord::edge(
                    EdgeLabel::TouchedFile,
                    record_id.clone(),
                    id.clone(),
                    None,
                    format!("coverage report {} TOUCHED_FILE {id}", req.tool),
                ));
            }
            None => {
                records.push(resolution_diagnostic(
                    req,
                    &display,
                    COVERAGE_FILE_UNRESOLVED_CODE,
                    0,
                ));
            }
        }

        // Per-function anchoring (only when the artefact names functions).
        if let Some(index) = index.as_ref() {
            for function in &file.function_details {
                let segment = final_segment(&function.name);
                match index.symbols_by_name.get(segment).map(Vec::as_slice) {
                    Some([symbol]) => {
                        resolved_function_count += 1;
                        let (symbol_id, _) = symbol_handles(symbol);
                        records.push(GraphRecord::edge(
                            EdgeLabel::MentionsSymbol,
                            record_id.clone(),
                            symbol_id.to_owned(),
                            None,
                            format!("coverage report {} MENTIONS_SYMBOL {symbol_id}", req.tool),
                        ));
                    }
                    Some(many) if many.len() >= 2 => {
                        ambiguous_function_count += 1;
                        records.push(resolution_diagnostic(
                            req,
                            &function.name,
                            COVERAGE_SYMBOL_AMBIGUOUS_CODE,
                            many.len(),
                        ));
                    }
                    _ => {
                        unresolved_function_count += 1;
                        records.push(resolution_diagnostic(
                            req,
                            &function.name,
                            COVERAGE_SYMBOL_UNRESOLVED_CODE,
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

    CoverageReportOutcome {
        record_id,
        records,
        resolved_file_count,
        unresolved_file_count,
        unresolved,
        resolved_function_count,
        unresolved_function_count,
        ambiguous_function_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unreadable_literal)]
    const JSON_ARTEFACT: &str = r#"{
  "data": [
    {
      "files": [
        {
          "filename": "/repo/src/lib.rs",
          "summary": {
            "lines": {"count": 10, "covered": 8, "percent": 80.0, "notcovered": 2},
            "functions": {"count": 3, "covered": 2, "percent": 66.66666666666667, "notcovered": 1},
            "regions": {"count": 20, "covered": 15, "percent": 75.0, "notcovered": 5}
          }
        },
        {
          "filename": "/repo/src/unused.rs",
          "summary": {
            "lines": {"count": 4, "covered": 0, "percent": 0.0, "notcovered": 4},
            "functions": {"count": 1, "covered": 0, "percent": 0.0, "notcovered": 1},
            "regions": {"count": 4, "covered": 0, "percent": 0.0, "notcovered": 4}
          }
        }
      ],
      "totals": {
        "lines": {"count": 14, "covered": 8, "percent": 57.142857142857146, "notcovered": 6},
        "functions": {"count": 4, "covered": 2, "percent": 50.0, "notcovered": 2},
        "regions": {"count": 24, "covered": 15, "percent": 62.5, "notcovered": 9}
      }
    }
  ],
  "type": "llvm.coverage.json.export",
  "version": "2.0.1"
}"#;

    const LCOV_ARTEFACT: &str = "TN:\nSF:/repo/src/lib.rs\nFN:1,covered_fn\nFN:5,dead_fn\nFNDA:3,covered_fn\nFNDA:0,dead_fn\nFNF:2\nFNH:1\nDA:1,3\nDA:2,3\nDA:5,0\nDA:6,0\nLF:4\nLH:2\nBRDA:2,0,0,1\nBRDA:2,0,1,-\nBRF:2\nBRH:1\nend_of_record\nSF:/repo/src/unused.rs\nFN:1,never\nFNDA:0,never\nFNF:1\nFNH:0\nDA:1,0\nDA:2,0\nLF:2\nLH:0\nend_of_record\n";

    const fn req<'a>() -> CoverageReportRequest<'a> {
        CoverageReportRequest {
            session_id: "sess-1",
            commit: "deadbeef",
            suite: Some("mini-cov"),
            command: "cargo llvm-cov --json",
            exit_code: 0,
            executed_at: "2026-07-19T12:00:00Z",
            tool: "cargo-llvm-cov",
            tool_version: Some("0.6.0"),
            artifact_format: LLVM_COV_JSON_FORMAT,
            source_artifact_path: "cov.json",
            source_artifact_hash: "abc123",
        }
    }

    #[test]
    fn json_parse_reads_files_and_totals() {
        let parse = parse_llvm_cov_json(JSON_ARTEFACT, Some("/repo")).expect("parse ok");
        assert_eq!(parse.file_count(), 2);
        // Sorted by repo-relative path.
        assert_eq!(
            parse.files()[0].repo_relative_path.as_deref(),
            Some("src/lib.rs")
        );
        let lib = &parse.files()[0];
        assert_eq!((lib.lines.total, lib.lines.covered), (10, 8));
        assert!((lib.lines.percent - 80.0).abs() < f64::EPSILON);
        assert_eq!(lib.regions.map(|r| (r.total, r.covered)), Some((20, 15)));
        assert!(parse.totals_branches().is_none());
        assert_eq!(
            parse.totals_regions().map(|r| (r.total, r.covered)),
            Some((24, 15))
        );
        let totals = parse.totals_lines();
        assert_eq!((totals.total, totals.covered), (14, 8));
        assert!((totals.percent - 57.1428_5714_2857_146).abs() < f64::EPSILON);
    }

    #[test]
    fn json_percent_computed_when_absent() {
        let parse = parse_llvm_cov_json(
            r#"{"data":[{"files":[{"filename":"a.rs","summary":{"lines":{"count":3,"covered":1}}}],"totals":{"lines":{"count":3,"covered":1}}}]}"#,
            None,
        )
        .expect("parse ok");
        // 1/3*100 rounded to four decimals.
        assert!((parse.files()[0].lines.percent - 33.3333).abs() < 1e-9);
    }

    #[test]
    fn json_zero_files_is_unparseable() {
        let err = parse_llvm_cov_json(
            r#"{"data":[{"files":[],"totals":{"lines":{"count":0,"covered":0}}]}"#,
            None,
        )
        .unwrap_err();
        assert_eq!(err, CoverageCaptureError::Unparseable);
        assert_eq!(err.code(), "unparseable_coverage_report");
    }

    #[test]
    fn json_empty_is_empty() {
        assert_eq!(
            parse_llvm_cov_json("  \n ", None).unwrap_err(),
            CoverageCaptureError::Empty
        );
    }

    #[test]
    fn tool_missing_markers_detected() {
        for text in [
            "error: no such command: `llvm-cov`\n",
            "bash: cargo-llvm-cov: command not found\n",
            "'cargo-llvm-cov' is not recognized as an internal or external command\n",
        ] {
            let err = parse_llvm_cov_json(text, None).unwrap_err();
            assert_eq!(err, CoverageCaptureError::ToolMissing, "{text}");
            assert_eq!(err.code(), "coverage_tool_missing");
            let err = parse_lcov(text, None).unwrap_err();
            assert_eq!(err, CoverageCaptureError::ToolMissing, "{text}");
        }
    }

    #[test]
    fn lcov_parse_reads_lines_branches_functions() {
        let parse = parse_lcov(LCOV_ARTEFACT, Some("/repo")).expect("parse ok");
        assert_eq!(parse.file_count(), 2);
        let lib = &parse.files()[0];
        assert_eq!(lib.repo_relative_path.as_deref(), Some("src/lib.rs"));
        assert_eq!((lib.lines.total, lib.lines.covered), (4, 2));
        assert!((lib.lines.percent - 50.0).abs() < f64::EPSILON);
        assert_eq!(lib.branches.map(|b| (b.total, b.covered)), Some((2, 1)));
        assert!(parse.totals_regions().is_none());
        assert_eq!(
            parse.totals_branches().map(|b| (b.total, b.covered)),
            Some((2, 1))
        );
        assert_eq!((lib.functions.total, lib.functions.covered), (2, 1));
        assert_eq!(lib.function_details.len(), 2);
        assert_eq!(lib.function_details[0].name, "covered_fn");
        assert!(lib.function_details[0].covered);
        assert!(!lib.function_details[1].covered);
        let totals = parse.totals_lines();
        assert_eq!((totals.total, totals.covered), (6, 2));
    }

    #[test]
    fn lcov_garbage_before_sf_is_unparseable() {
        let err = parse_lcov("hello world\n", None).unwrap_err();
        assert_eq!(err, CoverageCaptureError::Unparseable);
    }

    #[test]
    fn build_emits_stable_id_and_verification_domain() {
        let parse = parse_llvm_cov_json(JSON_ARTEFACT, Some("/repo")).expect("parse ok");
        let out = build_coverage_report_records(&req(), &parse, None);
        assert_eq!(
            out.record_id,
            verification_stable_id(&["coverage_report", "sess-1", "deadbeef", "cargo-llvm-cov"])
        );
        assert!(out.record_id.starts_with("verification:v1:"));
        let node = out
            .records
            .iter()
            .find(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::CoverageReport,
                        ..
                    }
                )
            })
            .expect("CoverageReport node");
        match node {
            GraphRecord::Node {
                domain,
                verification_kind,
                status,
                exit_code,
                source_artifact_hash,
                executed_at,
                evidence_quality,
                stdout_handle,
                ..
            } => {
                assert_eq!(domain.as_deref(), Some("verification"));
                assert_eq!(verification_kind.as_deref(), Some("coverage_report"));
                assert_eq!(status.as_deref(), Some("pass"));
                assert_eq!(*exit_code, Some(0));
                assert_eq!(source_artifact_hash.as_deref(), Some("abc123"));
                assert_eq!(executed_at.as_deref(), Some("2026-07-19T12:00:00Z"));
                assert_eq!(evidence_quality.as_deref(), Some("summarized"));
                let handle = stdout_handle.as_ref().expect("stdout_handle");
                assert!(handle.bytes <= INLINE_PAYLOAD_CEILING);
                let inline = handle.inline.as_ref().expect("inline summary");
                assert!(inline.contains("\"format\":\"llvm-cov-v1\""));
            }
            _ => unreachable!(),
        }
        // No graph: every file unresolved, no edges, report still emitted.
        assert_eq!(out.resolved_file_count, 0);
        assert_eq!(out.unresolved_file_count, 2);
        assert_eq!(out.unresolved, vec!["src/lib.rs", "src/unused.rs"]);
        assert!(
            !out.records
                .iter()
                .any(|r| matches!(r, GraphRecord::Edge { .. })),
            "no --graph: no cross-domain edges"
        );
    }

    #[test]
    fn build_is_byte_identical_across_runs() {
        let parse = parse_llvm_cov_json(JSON_ARTEFACT, Some("/repo")).expect("parse ok");
        let jsonl = |out: &CoverageReportOutcome| {
            let mut graph = crate::ir::Graph::new();
            for r in &out.records {
                graph.push(r.clone());
            }
            graph.to_jsonl().expect("jsonl must serialize")
        };
        let first = build_coverage_report_records(&req(), &parse, None);
        let second = build_coverage_report_records(&req(), &parse, None);
        assert_eq!(jsonl(&first), jsonl(&second));
    }

    #[test]
    fn nonzero_exit_code_marks_status_fail() {
        let parse = parse_lcov(LCOV_ARTEFACT, Some("/repo")).expect("parse ok");
        let req = CoverageReportRequest {
            exit_code: 1,
            ..req()
        };
        let out = build_coverage_report_records(&req, &parse, None);
        let node = out
            .records
            .iter()
            .find(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::CoverageReport,
                        ..
                    }
                )
            })
            .expect("node");
        match node {
            GraphRecord::Node { status, .. } => {
                assert_eq!(status.as_deref(), Some("fail"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn file_resolution_mints_touched_file_edges() {
        let parse = parse_lcov(LCOV_ARTEFACT, Some("/repo")).expect("parse ok");
        let file = GraphRecord::node(
            "codegraph:v1:file-lib".to_owned(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            "file src/lib.rs".to_owned(),
        );
        let sym = GraphRecord::node(
            "codegraph:v1:sym-covered".to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("covered_fn".to_owned()),
            "symbol covered_fn".to_owned(),
        );
        let graph = vec![file, sym];
        let out = build_coverage_report_records(&req(), &parse, Some(&graph));
        assert_eq!(out.resolved_file_count, 1);
        assert_eq!(out.unresolved_file_count, 1);
        assert_eq!(out.unresolved, vec!["src/unused.rs"]);

        let touched: Vec<&GraphRecord> = out
            .records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Edge {
                        label: EdgeLabel::TouchedFile,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(touched.len(), 1);
        match touched[0] {
            GraphRecord::Edge { target, .. } => {
                assert_eq!(target, "codegraph:v1:file-lib");
            }
            _ => unreachable!(),
        }

        // covered_fn resolves to exactly one symbol → MENTIONS_SYMBOL.
        let mentions = out
            .records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Edge {
                        label: EdgeLabel::MentionsSymbol,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(mentions, 1);
        assert_eq!(out.resolved_function_count, 1);
        // dead_fn and never resolve to zero symbols → unresolved diagnostics.
        assert!(out.unresolved_function_count >= 2);

        // No wrong edges: every edge target is a graph record id.
        for record in &out.records {
            if let GraphRecord::Edge { target, .. } = record {
                assert!(
                    target == "codegraph:v1:file-lib" || target == "codegraph:v1:sym-covered",
                    "unexpected edge target {target}"
                );
            }
        }
    }

    #[test]
    fn ambiguous_function_mints_no_edge() {
        let parse = parse_lcov(LCOV_ARTEFACT, Some("/repo")).expect("parse ok");
        let mk_sym = |id: &str| {
            GraphRecord::node(
                id.to_owned(),
                NodeKind::Symbol,
                Some("src/lib.rs".to_owned()),
                None,
                Some("covered_fn".to_owned()),
                "symbol covered_fn".to_owned(),
            )
        };
        let graph = vec![mk_sym("codegraph:v1:s1"), mk_sym("codegraph:v1:s2")];
        let out = build_coverage_report_records(&req(), &parse, Some(&graph));
        assert_eq!(out.ambiguous_function_count, 1);
        assert!(
            !out.records.iter().any(|r| matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::MentionsSymbol,
                    ..
                }
            )),
            "ambiguous symbol: no MENTIONS_SYMBOL edge"
        );
        assert!(out.records.iter().any(|r| match r {
            GraphRecord::Node { symbol_kind, .. } =>
                symbol_kind.as_deref() == Some(COVERAGE_SYMBOL_AMBIGUOUS_CODE),
            _ => false,
        }));
    }

    #[test]
    fn coverage_summary_view_round_trips() {
        let parse = parse_llvm_cov_json(JSON_ARTEFACT, Some("/repo")).expect("parse ok");
        let out = build_coverage_report_records(&req(), &parse, None);
        let node = out
            .records
            .iter()
            .find(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::CoverageReport,
                        ..
                    }
                )
            })
            .expect("node");
        let view = coverage_summary_of(node).expect("summary view");
        assert_eq!(view.tool, "cargo-llvm-cov");
        assert!((view.line_percent - 57.1428_5714_2857_146).abs() < f64::EPSILON);
        assert_eq!(view.branch_percent, Some(62.5));
        assert_eq!(view.files_measured, 2);
    }

    #[test]
    fn coverage_summary_view_rejects_other_kinds() {
        let node = GraphRecord::node(
            "codegraph:v1:x".to_owned(),
            NodeKind::Symbol,
            None,
            None,
            Some("x".to_owned()),
            "x".to_owned(),
        );
        assert!(coverage_summary_of(&node).is_none());
    }

    #[test]
    fn error_diagnostic_codes_are_stable() {
        assert_eq!(CoverageCaptureError::Empty.code(), "empty_coverage_report");
        assert_eq!(
            CoverageCaptureError::Unparseable.code(),
            "unparseable_coverage_report"
        );
        assert_eq!(
            CoverageCaptureError::ToolMissing.code(),
            "coverage_tool_missing"
        );
    }

    #[test]
    fn final_segment_uses_last_double_colon_component() {
        assert_eq!(final_segment("mini_cov::covered_add"), "covered_add");
        assert_eq!(final_segment("covered_add"), "covered_add");
    }
}
