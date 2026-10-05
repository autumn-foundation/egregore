//! Time-to-first-citable-answer product gate (issue #57).
//!
//! This module is the **measurement layer** behind `eg audit symbol-latency`.
//! Where issue #120 (`eg audit query-budget`) gates a relative scaling
//! assertion with advisory absolute budgets, this benchmark is the product
//! gate the issue text demands: warm `eg query symbol`, `eg query file`,
//! `eg query symbol --at <sha>`, and `eg query drift` must return
//! **correct, citable** answers (record ID + repo-relative file/span or
//! commit handle — latency without correctness does not count) with warm
//! p95 under 2s across 5 consecutive runs, or the workflow fails with a
//! stable diagnostic naming the query class and the observed p95.
//!
//! Reused from the issue #120 / #255 layers (never duplicated):
//! [`crate::query_latency::measure_cold_query`] (spawn-to-first-line
//! harness), [`crate::query_latency::percentile`],
//! [`crate::query_latency::machine_info`], and
//! [`crate::query_budget::synthetic_drift_records`].
//!
//! New for #57: the correctness-checked answer (every measured answer is
//! validated against a fixture expectation and must carry citable handles),
//! the hard warm-p95 budget gate, the boring-substitute comparison with
//! explicit "not comparable" notes (`rg`/`git grep`/`git show`), the
//! separately-reported setup phases (cold scan, history replay, ingest,
//! embedding setup), the environment context record, and the 5-run answer
//! determinism check.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::query_latency::{machine_info, percentile};

/// The pinned benchmark corpus manifest, deserialized from
/// `corpus/symbol_latency_corpus.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct SymbolLatencyCorpus {
    /// Schema version for the manifest format.
    pub corpus_version: String,
    /// Stable human-readable corpus name reported for interpretation.
    pub corpus_name: String,
    /// Description of the corpus scope and its documented scale.
    pub description: String,
    /// Corpus source directory, resolved relative to the manifest's parent dir.
    pub source_dir: String,
    /// Repository identity override so record IDs are stable across checkouts.
    pub repository_id_override: String,
    /// Fixed scan transaction time so the scan is deterministic.
    pub scan_time: String,
    /// Symbol name the benchmark queries (must match >= 1 record).
    pub query_symbol: String,
    /// Repository-relative file path the benchmark queries (must match >= 1
    /// record).
    pub query_file: String,
    /// `--limit` for the benchmarked `eg query drift` invocation.
    pub drift_limit: usize,
    /// Deterministic synthetic `SemanticDrift` nodes appended so
    /// `eg query drift` has a stable answer on the fixture.
    pub synthetic_drift_nodes_per_repo: usize,
    /// Depth of the deterministic synthetic git history built for the
    /// history-replay phase and the symbol-at-commit lookup.
    pub history_commits: usize,
    /// Which commit (0-based, oldest first) of the synthetic history the
    /// symbol-at-commit lookup targets.
    pub at_commit_index: usize,
    /// Warm samples measured per query (5 consecutive runs per the success
    /// metric).
    pub warm_samples: usize,
    /// Hard warm-p95 budget in milliseconds each query class must meet.
    pub budget_p95_ms: f64,
    /// Record count measured when the corpus was pinned (auditability).
    pub reference_record_count: u64,
    /// Named machine class the budget is defined against.
    pub reference_machine_class: String,
    /// Documented fixture scale: Rust lines of code in the source snapshot.
    pub fixture_rust_loc: u64,
    /// Documented fixture scale: symbol records in the pinned scan.
    pub fixture_symbol_count: u64,
}

/// The four query classes the product gate measures (issue #57 AC2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueryClass {
    /// Exact symbol lookup: `eg query symbol <NAME>`.
    Symbol,
    /// File-defines lookup: `eg query file <PATH>`.
    File,
    /// Symbol-at-commit lookup: `eg query symbol <NAME> --at <SHA>`.
    SymbolAtCommit,
    /// Top semantic-drift listing: `eg query drift --limit <N>`.
    Drift,
}

impl QueryClass {
    /// Stable kebab-case name used in reports and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::SymbolAtCommit => "symbol-at-commit",
            Self::Drift => "drift",
        }
    }

    /// All four classes in report order.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::Symbol, Self::File, Self::SymbolAtCommit, Self::Drift]
    }
}

/// What a measured answer must satisfy to count (issue #57 AC3): latency
/// without correctness does not count.
#[derive(Debug, Clone)]
pub struct AnswerExpectation {
    /// Which query class the answer came from.
    pub query: QueryClass,
    /// Expected symbol name (`Symbol` class).
    pub symbol_name: Option<String>,
    /// Expected repository-relative file path (`File` class).
    pub file_path: Option<String>,
    /// Expected commit SHA (`SymbolAtCommit` class).
    pub commit_sha: Option<String>,
    /// Minimum answer rows (default 1: an empty answer has no
    /// time-to-first-citable-answer).
    pub min_rows: usize,
}

impl AnswerExpectation {
    /// Expectation for an exact symbol lookup.
    #[must_use]
    pub fn symbol(name: &str) -> Self {
        Self {
            query: QueryClass::Symbol,
            symbol_name: Some(name.to_owned()),
            file_path: None,
            commit_sha: None,
            min_rows: 1,
        }
    }

    /// Expectation for a file-defines lookup.
    #[must_use]
    pub fn file(path: &str) -> Self {
        Self {
            query: QueryClass::File,
            symbol_name: None,
            file_path: Some(path.to_owned()),
            commit_sha: None,
            min_rows: 1,
        }
    }

    /// Expectation for a symbol-at-commit lookup.
    #[must_use]
    pub fn symbol_at_commit(name: &str, commit_sha: &str) -> Self {
        Self {
            query: QueryClass::SymbolAtCommit,
            symbol_name: Some(name.to_owned()),
            file_path: None,
            commit_sha: Some(commit_sha.to_owned()),
            min_rows: 1,
        }
    }

    /// Expectation for the top drift listing.
    #[must_use]
    pub const fn drift() -> Self {
        Self {
            query: QueryClass::Drift,
            symbol_name: None,
            file_path: None,
            commit_sha: None,
            min_rows: 1,
        }
    }
}

/// Why an answer failed its correctness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerCheckError {
    /// The answer had fewer rows than the expectation requires.
    NoRows {
        /// Query class that produced the short answer.
        query: &'static str,
        /// Rows found.
        found: usize,
        /// Rows required.
        min_rows: usize,
    },
    /// An answer line was not valid JSON.
    UnparseableRow {
        /// Query class that produced the bad line.
        query: &'static str,
        /// 1-based line number.
        line: usize,
    },
    /// A row carried no usable record ID.
    MissingRecordId {
        /// Query class that produced the row.
        query: &'static str,
        /// 1-based line number.
        line: usize,
    },
    /// A symbol row named a different symbol than the fixture expects.
    NameMismatch {
        /// Query class that produced the row.
        query: &'static str,
        /// Fixture-expected symbol name.
        expected: String,
        /// Name the row actually carried.
        found: String,
    },
    /// A row had no repo-relative file/span citable handle.
    MissingFileSpan {
        /// Query class that produced the row.
        query: &'static str,
        /// 1-based line number.
        line: usize,
    },
    /// A file-defines row named a different file than the fixture expects.
    FileMismatch {
        /// Query class that produced the row.
        query: &'static str,
        /// Fixture-expected repo-relative path.
        expected: String,
        /// Path the row actually carried.
        found: String,
    },
    /// A symbol-at-commit row cited a different commit than requested.
    CommitMismatch {
        /// Query class that produced the row.
        query: &'static str,
        /// Requested commit SHA (or prefix).
        expected: String,
        /// Commit the row actually cited.
        found: String,
    },
}

impl std::fmt::Display for AnswerCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRows {
                query,
                found,
                min_rows,
            } => write!(
                f,
                "{query}: expected at least {min_rows} answer row(s), found {found}"
            ),
            Self::UnparseableRow { query, line } => {
                write!(f, "{query}: answer line {line} is not valid JSON")
            }
            Self::MissingRecordId { query, line } => {
                write!(f, "{query}: answer line {line} carries no record ID")
            }
            Self::NameMismatch {
                query,
                expected,
                found,
            } => write!(
                f,
                "{query}: expected symbol `{expected}`, answer named `{found}`"
            ),
            Self::MissingFileSpan { query, line } => write!(
                f,
                "{query}: answer line {line} carries no repo-relative file/span handle"
            ),
            Self::FileMismatch {
                query,
                expected,
                found,
            } => write!(
                f,
                "{query}: expected file `{expected}`, answer cited `{found}`"
            ),
            Self::CommitMismatch {
                query,
                expected,
                found,
            } => write!(
                f,
                "{query}: expected commit `{expected}`, answer cited `{found}`"
            ),
        }
    }
}

/// The citable content of a checked answer: every row's record ID plus its
/// human-readable citable handle.
#[derive(Debug, Clone, Serialize)]
pub struct AnswerSummary {
    /// Number of answer rows checked.
    pub rows: usize,
    /// Record IDs cited, in answer order.
    pub record_ids: Vec<String>,
    /// Human-readable citable handles, in answer order
    /// (`path:start-end`, a commit SHA, or a drift `before..after` range).
    pub handles: Vec<String>,
}

/// Reads one JSON field path (`a.b.c`) from a row, returning the string
/// value when present.
fn row_str<'row>(row: &'row serde_json::Value, path: &[&str]) -> Option<&'row str> {
    let mut current = row;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

/// Checks a full query answer (JSONL, one row per line) against the fixture
/// expectation (issue #57 AC3).
///
/// Every row must carry a non-empty `record_id`, plus the class-appropriate
/// citable handle: symbol rows must name the expected symbol and cite a
/// repo-relative file/span; file rows must cite the expected file with a
/// span; symbol-at-commit rows must cite the requested commit SHA; drift
/// rows must carry the drift commit range. Any violation fails closed —
/// latency without correctness does not count.
///
/// # Errors
///
/// Returns [`AnswerCheckError`] describing the first violation found.
pub fn check_answer_handles(
    stdout: &str,
    expectation: &AnswerExpectation,
) -> Result<AnswerSummary, AnswerCheckError> {
    let query = expectation.query.as_str();
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() < expectation.min_rows {
        return Err(AnswerCheckError::NoRows {
            query,
            found: lines.len(),
            min_rows: expectation.min_rows,
        });
    }
    let mut record_ids = Vec::with_capacity(lines.len());
    let mut handles = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let row: serde_json::Value = serde_json::from_str(line)
            .map_err(|_| AnswerCheckError::UnparseableRow { query, line: index })?;
        let record_id = row_str(&row, &["record_id"])
            .filter(|id| !id.is_empty())
            .ok_or(AnswerCheckError::MissingRecordId { query, line: index })?;
        record_ids.push(record_id.to_owned());

        let path = row_str(&row, &["repo_relative_path"]);
        let start_line = row
            .get("span")
            .and_then(|span| span.get("start_line"))
            .and_then(serde_json::Value::as_u64);
        let end_line = row
            .get("span")
            .and_then(|span| span.get("end_line"))
            .and_then(serde_json::Value::as_u64);

        match expectation.query {
            QueryClass::Symbol => {
                let expected = expectation.symbol_name.as_deref().unwrap_or("");
                let found = row_str(&row, &["name"]).unwrap_or("");
                if found != expected {
                    return Err(AnswerCheckError::NameMismatch {
                        query,
                        expected: expected.to_owned(),
                        found: found.to_owned(),
                    });
                }
                let (Some(path), Some(start), Some(end)) = (path, start_line, end_line) else {
                    return Err(AnswerCheckError::MissingFileSpan { query, line: index });
                };
                handles.push(format!("{path}:{start}-{end}"));
            }
            QueryClass::File => {
                let expected = expectation.file_path.as_deref().unwrap_or("");
                let found = path.unwrap_or("");
                if found != expected {
                    return Err(AnswerCheckError::FileMismatch {
                        query,
                        expected: expected.to_owned(),
                        found: found.to_owned(),
                    });
                }
                let (Some(start), Some(end)) = (start_line, end_line) else {
                    return Err(AnswerCheckError::MissingFileSpan { query, line: index });
                };
                handles.push(format!("{found}:{start}-{end}"));
            }
            QueryClass::SymbolAtCommit => {
                let expected = expectation.commit_sha.as_deref().unwrap_or("");
                let found = row_str(&row, &["git_commit"]).unwrap_or("");
                // The query accepts a unique SHA prefix; the row cites the
                // full SHA, so either direction of prefix match counts.
                if !(found.starts_with(expected) || expected.starts_with(found)) || found.is_empty()
                {
                    return Err(AnswerCheckError::CommitMismatch {
                        query,
                        expected: expected.to_owned(),
                        found: found.to_owned(),
                    });
                }
                match (path, start_line, end_line) {
                    (Some(path), Some(start), Some(end)) => {
                        handles.push(format!("{found}:{path}:{start}-{end}"));
                    }
                    _ => handles.push(found.to_owned()),
                }
            }
            QueryClass::Drift => {
                let before = row_str(&row, &["before_commit"]).unwrap_or("");
                let after = row_str(&row, &["after_commit"]).unwrap_or("");
                if before.is_empty() || after.is_empty() {
                    return Err(AnswerCheckError::MissingFileSpan { query, line: index });
                }
                handles.push(format!("{before}..{after}"));
            }
        }
    }
    Ok(AnswerSummary {
        rows: lines.len(),
        record_ids,
        handles,
    })
}

/// Whether a measured p95 meets the hard product-gate budget (issue #57
/// AC5): the budget gates, it is not advisory.
#[must_use]
pub fn latency_within_budget(p95_ms: f64, budget_p95_ms: f64) -> bool {
    p95_ms.is_finite() && p95_ms <= budget_p95_ms
}

/// The stable failure diagnostic printed when a query class misses the
/// budget (issue #57 AC5): it names the query class and the observed p95.
#[must_use]
pub fn budget_diagnostic(
    query: QueryClass,
    observed_p95_ms: f64,
    budget_p95_ms: f64,
) -> serde_json::Value {
    serde_json::json!({
        "code": "symbol_latency_budget_exceeded",
        "query": query.as_str(),
        "observed_p95_ms": observed_p95_ms,
        "budget_p95_ms": budget_p95_ms,
    })
}

/// The stable failure diagnostic for a non-deterministic answer (issue #57
/// AC8).
#[must_use]
pub fn determinism_diagnostic(query: QueryClass, runs: usize) -> serde_json::Value {
    serde_json::json!({
        "code": "symbol_latency_answer_unstable",
        "query": query.as_str(),
        "runs": runs,
    })
}

/// Whether full answer outputs are byte-identical across repeated warm runs
/// after canonical ordering (issue #57 AC8).
///
/// Each output's non-empty lines are sorted (the queries already emit
/// canonically ordered rows; the sort makes the check robust to that
/// contract), and all runs must agree exactly. Fewer than two runs can never
/// demonstrate determinism, and an empty output is never "deterministic" —
/// an unanswered query fails the correctness check first.
#[must_use]
pub fn answers_deterministic(outputs: &[String]) -> bool {
    if outputs.len() < 2 {
        return false;
    }
    let mut canonical: Vec<String> = Vec::with_capacity(outputs.len());
    for output in outputs {
        let mut lines: Vec<&str> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() {
            return false;
        }
        lines.sort_unstable();
        canonical.push(lines.join("\n"));
    }
    canonical.windows(2).all(|pair| pair[0] == pair[1])
}

/// How a boring substitute relates to the graph query it stands in for
/// (issue #57 AC4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Comparability {
    /// The substitute answers the same question closely enough to time.
    Comparable,
    /// The substitute cannot return citable graph handles, so no honest
    /// latency comparison exists — the note says why.
    NotComparable,
}

/// One boring-substitute comparison row (issue #57 AC4).
#[derive(Debug, Clone, Serialize)]
pub struct SubstituteComparison {
    /// Query class the substitute stands in for (`"symbol"`, `"file"`,
    /// `"symbol-at-commit"`, `"drift"`).
    pub query: String,
    /// Substitute name: `"ripgrep"`, `"git-grep"`, or `"git-show"`.
    pub substitute: String,
    /// Exact command measured.
    pub command: String,
    /// Every sample in milliseconds, ascending.
    pub samples_ms: Vec<f64>,
    /// Median latency in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: f64,
    /// True when the substitute was unavailable and not measured.
    pub skipped: bool,
    /// Why the substitute was skipped (present only when `skipped`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// Whether the timing is an honest comparison.
    pub comparability: Comparability,
    /// Why the substitute is (or is not) comparable — the explicit
    /// "not comparable" note when it cannot return citable graph handles.
    pub comparability_note: String,
}

/// Builds a measured [`SubstituteComparison`].
#[must_use]
pub fn summarize_substitute(
    query: &str,
    substitute: &str,
    command: &str,
    mut samples_ms: Vec<f64>,
    comparability: Comparability,
    comparability_note: &str,
) -> SubstituteComparison {
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (p50_ms, p95_ms) = if samples_ms.is_empty() {
        (0.0, 0.0)
    } else {
        (
            percentile(&samples_ms, 50.0).unwrap_or(0.0),
            percentile(&samples_ms, 95.0).unwrap_or(0.0),
        )
    };
    SubstituteComparison {
        query: query.to_owned(),
        substitute: substitute.to_owned(),
        command: command.to_owned(),
        samples_ms,
        p50_ms,
        p95_ms,
        skipped: false,
        skip_reason: None,
        comparability,
        comparability_note: comparability_note.to_owned(),
    }
}

/// Builds an explicit substitute skip record — never silently absent.
#[must_use]
pub fn skipped_substitute(
    query: &str,
    substitute: &str,
    command: &str,
    reason: &str,
    comparability_note: &str,
) -> SubstituteComparison {
    SubstituteComparison {
        query: query.to_owned(),
        substitute: substitute.to_owned(),
        command: command.to_owned(),
        samples_ms: Vec::new(),
        p50_ms: 0.0,
        p95_ms: 0.0,
        skipped: true,
        skip_reason: Some(reason.to_owned()),
        comparability: Comparability::NotComparable,
        comparability_note: comparability_note.to_owned(),
    }
}

/// One setup phase timed separately from warm query latency (issue #57 AC6).
#[derive(Debug, Clone, Serialize)]
pub struct SetupPhase {
    /// Phase name: `"cold-scan"`, `"history-replay"`, `"ingest"`, or
    /// `"embedding-setup"`.
    pub phase: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: f64,
    /// True when the phase could not run in this environment.
    pub skipped: bool,
    /// Why the phase was skipped (present only when `skipped`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// Builds a measured [`SetupPhase`].
#[must_use]
pub fn summarize_setup_phase(phase: &str, duration_ms: f64) -> SetupPhase {
    SetupPhase {
        phase: phase.to_owned(),
        duration_ms,
        skipped: false,
        skip_reason: None,
    }
}

/// Builds an explicit setup-phase skip record — never silently absent.
#[must_use]
pub fn skipped_setup_phase(phase: &str, reason: &str) -> SetupPhase {
    SetupPhase {
        phase: phase.to_owned(),
        duration_ms: 0.0,
        skipped: true,
        skip_reason: Some(reason.to_owned()),
    }
}

/// Non-sensitive environment context recorded with every report (issue #57
/// AC7).
#[derive(Debug, Clone, Serialize)]
pub struct Environment {
    /// Operating system (`std::env::consts::OS`).
    pub os: String,
    /// CPU class: architecture plus available parallelism.
    pub cpu_class: String,
    /// Rust profile the measured binary was built with.
    pub rust_profile: String,
    /// Egregore version (`CARGO_PKG_VERSION`).
    pub egregore_version: String,
    /// Pinned corpus name.
    pub corpus_name: String,
    /// Records in the store the queries measured against.
    pub record_count: u64,
    /// Store kind the queries measured against (`"jsonl"`, `"embedded"`,
    /// `"daemon"`).
    pub store_kind: String,
}

/// The Rust profile the current binary was built with.
#[must_use]
pub const fn rust_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// Collects the environment record for a report.
#[must_use]
pub fn collect_environment(corpus_name: &str, record_count: u64, store_kind: &str) -> Environment {
    let machine = machine_info();
    Environment {
        os: machine.os,
        cpu_class: format!("{} ({} threads)", machine.arch, machine.parallelism),
        rust_profile: rust_profile().to_owned(),
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        corpus_name: corpus_name.to_owned(),
        record_count,
        store_kind: store_kind.to_owned(),
    }
}

/// One measured warm query cell: latency samples plus the
/// correctness-checked answer (issue #57 AC2/AC3/AC5/AC8).
#[derive(Debug, Clone, Serialize)]
pub struct WarmCell {
    /// Query class (`"symbol"`, `"file"`, `"symbol-at-commit"`, `"drift"`).
    pub query: String,
    /// Every warm sample in milliseconds, ascending.
    pub samples_ms: Vec<f64>,
    /// Median warm latency in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile warm latency in milliseconds.
    pub p95_ms: f64,
    /// Fastest warm sample in milliseconds.
    pub min_ms: f64,
    /// Slowest warm sample in milliseconds.
    pub max_ms: f64,
    /// The correctness-checked citable answer.
    pub answer: AnswerSummary,
    /// Whether the full answer was byte-identical across all warm runs
    /// after canonical ordering.
    pub deterministic: bool,
    /// Whether `p95_ms` met the hard budget.
    pub within_budget: bool,
}

/// Summarizes ascending warm samples into a [`WarmCell`]. Returns `None`
/// when there are no samples.
#[must_use]
pub fn summarize_warm_cell(
    query: QueryClass,
    mut samples_ms: Vec<f64>,
    answer: AnswerSummary,
    deterministic: bool,
    budget_p95_ms: f64,
) -> Option<WarmCell> {
    if samples_ms.is_empty() {
        return None;
    }
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95_ms = percentile(&samples_ms, 95.0)?;
    Some(WarmCell {
        query: query.as_str().to_owned(),
        p50_ms: percentile(&samples_ms, 50.0).unwrap_or(0.0),
        p95_ms,
        min_ms: samples_ms[0],
        max_ms: samples_ms[samples_ms.len() - 1],
        samples_ms,
        answer,
        deterministic,
        within_budget: latency_within_budget(p95_ms, budget_p95_ms),
    })
}

/// Full benchmark report printed by `eg audit symbol-latency`.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolLatencyReport {
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
    /// Index (oldest-first) of the synthetic-history commit the
    /// symbol-at-commit lookup targeted.
    pub at_commit_index: usize,
    /// SHA of that commit.
    pub at_commit_sha: String,
    /// Non-sensitive environment context (issue #57 AC7).
    pub environment: Environment,
    /// Setup phases, timed separately from warm query latency (issue #57
    /// AC6).
    pub setup_phases: Vec<SetupPhase>,
    /// Per-class warm results keyed by query class name.
    pub queries: BTreeMap<String, WarmCell>,
    /// Boring-substitute comparisons (issue #57 AC4).
    pub substitutes: Vec<SubstituteComparison>,
    /// Warm samples measured per query class.
    pub warm_samples: usize,
    /// Hard warm-p95 budget in milliseconds.
    pub budget_p95_ms: f64,
    /// Records in the `scan-history` store the symbol-at-commit lookup
    /// measured against (the other three classes use
    /// [`Environment::record_count`]).
    pub history_record_count: u64,
    /// True when every query class met its budget with a correct,
    /// deterministic, citable answer.
    pub ok: bool,
}

/// One full-output sample: wall-clock milliseconds from just before spawn
/// until the first non-empty stdout line, plus the complete stdout.
///
/// Unlike [`crate::query_latency::measure_cold_query`] (which kills the
/// child at the first line), this reads the child to EOF so the answer can
/// be correctness-checked and compared across runs for determinism. The
/// measured quantity is still time-to-FIRST-answer.
pub struct FullSample {
    /// Milliseconds from just before spawn to the first non-empty stdout line.
    pub first_line_ms: f64,
    /// Complete stdout.
    pub stdout: String,
}

/// Spawns `exe` with `args` and returns the [`FullSample`].
///
/// Returns an error (with the child's stderr) when the child emits no
/// stdout line — a query that answers nothing has no
/// time-to-first-citable-answer, and the benchmark fails closed instead of
/// timing an empty result.
///
/// # Errors
///
/// Returns an error if the child process cannot be spawned, if stdout was
/// not piped, or if the child emits no stdout line before exiting.
pub fn measure_query_full(exe: &Path, args: &[&str]) -> Result<FullSample, String> {
    let start = Instant::now();
    let mut child = Command::new(exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to spawn {}: {error}", exe.display()))?;

    let (first_line_ms, stdout) = {
        let stdout_handle = child.stdout.take().ok_or("child stdout was not piped")?;
        let mut reader = BufReader::new(stdout_handle);
        let mut first_line_ms: Option<f64> = None;
        let mut collected = String::new();
        loop {
            let mut line = String::new();
            let bytes = reader
                .read_line(&mut line)
                .map_err(|error| format!("failed to read child stdout: {error}"))?;
            if bytes == 0 {
                break;
            }
            if first_line_ms.is_none() && !line.trim().is_empty() {
                first_line_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
            }
            collected.push_str(&line);
        }
        let first_line_ms =
            first_line_ms.ok_or_else(|| "query child emitted no stdout lines".to_owned())?;
        (first_line_ms, collected)
    };

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

    if stdout.trim().is_empty() {
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            "query child emitted no stdout lines".to_owned()
        } else {
            format!("query child emitted no stdout lines; stderr: {detail}")
        });
    }
    Ok(FullSample {
        first_line_ms,
        stdout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol_stdout() -> &'static str {
        "{\"record_id\":\"rec-001\",\"schema_version\":7,\"name\":\"RepositoryIndex\",\"kind\":\"Symbol\",\"repo_relative_path\":\"symbols.rs\",\"span\":{\"start_line\":10,\"end_line\":40,\"start_column\":0,\"end_column\":1}}\n{\"record_id\":\"rec-002\",\"schema_version\":7,\"name\":\"RepositoryIndex\",\"kind\":\"Symbol\",\"repo_relative_path\":\"index.rs\",\"span\":{\"start_line\":3,\"end_line\":9,\"start_column\":0,\"end_column\":1}}\n"
    }

    #[test]
    fn check_symbol_answer_requires_name_and_file_span_handles() {
        let summary = check_answer_handles(
            symbol_stdout(),
            &AnswerExpectation::symbol("RepositoryIndex"),
        )
        .expect("valid symbol answer");
        assert_eq!(summary.rows, 2);
        assert_eq!(summary.record_ids, vec!["rec-001", "rec-002"]);
        assert_eq!(summary.handles, vec!["symbols.rs:10-40", "index.rs:3-9"]);
    }

    #[test]
    fn check_symbol_answer_rejects_wrong_name() {
        let bad = symbol_stdout().replace("RepositoryIndex", "SomethingElse");
        let error = check_answer_handles(&bad, &AnswerExpectation::symbol("RepositoryIndex"))
            .expect_err("wrong name must fail");
        assert_eq!(
            error,
            AnswerCheckError::NameMismatch {
                query: "symbol",
                expected: "RepositoryIndex".to_owned(),
                found: "SomethingElse".to_owned(),
            }
        );
    }

    #[test]
    fn check_symbol_answer_rejects_missing_record_id() {
        let bad = "{\"name\":\"RepositoryIndex\",\"repo_relative_path\":\"symbols.rs\",\"span\":{\"start_line\":10,\"end_line\":40}}\n";
        let error = check_answer_handles(bad, &AnswerExpectation::symbol("RepositoryIndex"))
            .expect_err("missing record_id must fail");
        assert_eq!(
            error,
            AnswerCheckError::MissingRecordId {
                query: "symbol",
                line: 0
            }
        );
    }

    #[test]
    fn check_symbol_answer_rejects_missing_span() {
        let bad = "{\"record_id\":\"rec-001\",\"name\":\"RepositoryIndex\",\"repo_relative_path\":\"symbols.rs\"}\n";
        let error = check_answer_handles(bad, &AnswerExpectation::symbol("RepositoryIndex"))
            .expect_err("missing span must fail");
        assert_eq!(
            error,
            AnswerCheckError::MissingFileSpan {
                query: "symbol",
                line: 0
            }
        );
    }

    #[test]
    fn check_answer_rejects_empty_and_unparseable() {
        let error = check_answer_handles("", &AnswerExpectation::symbol("RepositoryIndex"))
            .expect_err("empty answer must fail");
        assert_eq!(
            error,
            AnswerCheckError::NoRows {
                query: "symbol",
                found: 0,
                min_rows: 1
            }
        );
        let error =
            check_answer_handles("not json\n", &AnswerExpectation::symbol("RepositoryIndex"))
                .expect_err("unparseable row must fail");
        assert_eq!(
            error,
            AnswerCheckError::UnparseableRow {
                query: "symbol",
                line: 0
            }
        );
    }

    #[test]
    fn check_file_answer_requires_expected_file_and_spans() {
        let stdout = "{\"record_id\":\"rec-010\",\"name\":\"build_index\",\"kind\":\"Symbol\",\"repo_relative_path\":\"symbols.rs\",\"span\":{\"start_line\":10,\"end_line\":40,\"start_column\":0,\"end_column\":1}}\n";
        let summary = check_answer_handles(stdout, &AnswerExpectation::file("symbols.rs"))
            .expect("valid file answer");
        assert_eq!(summary.handles, vec!["symbols.rs:10-40"]);

        let error = check_answer_handles(stdout, &AnswerExpectation::file("other.rs"))
            .expect_err("wrong file must fail");
        assert_eq!(
            error,
            AnswerCheckError::FileMismatch {
                query: "file",
                expected: "other.rs".to_owned(),
                found: "symbols.rs".to_owned(),
            }
        );
    }

    #[test]
    fn check_symbol_at_commit_answer_requires_matching_commit_sha() {
        let stdout = "{\"record_id\":\"rec-001\",\"name\":\"RepositoryIndex\",\"kind\":\"Symbol\",\"repo_relative_path\":\"symbols.rs\",\"span\":{\"start_line\":10,\"end_line\":40,\"start_column\":0,\"end_column\":1},\"git_commit\":\"abc123def456\",\"valid_time\":\"2026-01-02T00:00:00Z\"}\n";
        let summary = check_answer_handles(
            stdout,
            &AnswerExpectation::symbol_at_commit("RepositoryIndex", "abc123def456"),
        )
        .expect("valid at-commit answer");
        assert_eq!(summary.handles, vec!["abc123def456:symbols.rs:10-40"]);

        // A unique SHA prefix is accepted, matching `--at` semantics.
        check_answer_handles(
            stdout,
            &AnswerExpectation::symbol_at_commit("RepositoryIndex", "abc123"),
        )
        .expect("sha prefix must be accepted");

        let error = check_answer_handles(
            stdout,
            &AnswerExpectation::symbol_at_commit("RepositoryIndex", "fff000"),
        )
        .expect_err("wrong commit must fail");
        assert_eq!(
            error,
            AnswerCheckError::CommitMismatch {
                query: "symbol-at-commit",
                expected: "fff000".to_owned(),
                found: "abc123def456".to_owned(),
            }
        );
    }

    #[test]
    fn check_drift_answer_requires_commit_range_handles() {
        let stdout = "{\"record_id\":\"drift-001\",\"kind\":\"SemanticDrift\",\"before_commit\":\"aaa\",\"after_commit\":\"bbb\",\"score\":0.95}\n";
        let summary =
            check_answer_handles(stdout, &AnswerExpectation::drift()).expect("valid drift answer");
        assert_eq!(summary.handles, vec!["aaa..bbb"]);

        let bad = "{\"record_id\":\"drift-001\",\"kind\":\"SemanticDrift\",\"score\":0.95}\n";
        check_answer_handles(bad, &AnswerExpectation::drift())
            .expect_err("drift row without commit range must fail");
    }

    #[test]
    fn latency_budget_gate_is_hard_not_advisory() {
        assert!(latency_within_budget(1999.9, 2000.0));
        assert!(latency_within_budget(2000.0, 2000.0));
        assert!(!latency_within_budget(2000.1, 2000.0));
        assert!(!latency_within_budget(f64::NAN, 2000.0));
        assert!(!latency_within_budget(f64::INFINITY, 2000.0));
    }

    #[test]
    fn budget_diagnostic_names_query_class_and_observed_p95() {
        let diagnostic = budget_diagnostic(QueryClass::SymbolAtCommit, 2341.5, 2000.0);
        assert_eq!(diagnostic["code"], "symbol_latency_budget_exceeded");
        assert_eq!(diagnostic["query"], "symbol-at-commit");
        assert_eq!(diagnostic["observed_p95_ms"], 2341.5);
        assert_eq!(diagnostic["budget_p95_ms"], 2000.0);
    }

    #[test]
    fn answers_deterministic_after_canonical_ordering() {
        let run_a = "{\"b\":2}\n{\"a\":1}\n".to_owned();
        let run_b = "{\"a\":1}\n{\"b\":2}\n".to_owned();
        assert!(answers_deterministic(&[
            run_a.clone(),
            run_b,
            run_a.clone()
        ]));
        assert!(answers_deterministic(&[run_a.clone(), run_a.clone()]));

        let different = "{\"a\":1}\n{\"b\":3}\n".to_owned();
        assert!(!answers_deterministic(&[run_a, different]));
        assert!(!answers_deterministic(&[]));
        assert!(!answers_deterministic(&["   \n".to_owned()]));
        assert!(!answers_deterministic(&["{\"a\":1}\n".to_owned()]));
    }

    #[test]
    fn substitute_comparability_is_explicit() {
        let measured = summarize_substitute(
            "symbol",
            "ripgrep",
            "rg -n foo .",
            vec![30.0, 10.0, 20.0],
            Comparability::Comparable,
            "text match only; no record IDs",
        );
        assert!(!measured.skipped);
        assert_eq!(measured.comparability, Comparability::Comparable);
        assert!((measured.p50_ms - 20.0).abs() < f64::EPSILON);

        let skipped = skipped_substitute(
            "drift",
            "ripgrep",
            "rg -n foo .",
            "not on PATH",
            "semantic drift has no text-search equivalent",
        );
        assert!(skipped.skipped);
        assert_eq!(skipped.comparability, Comparability::NotComparable);
        assert_eq!(skipped.skip_reason.as_deref(), Some("not on PATH"));
    }

    #[test]
    fn setup_phases_report_measured_and_skipped() {
        let measured = summarize_setup_phase("cold-scan", 1234.5);
        assert!(!measured.skipped);
        assert!((measured.duration_ms - 1234.5).abs() < f64::EPSILON);

        let skipped = skipped_setup_phase("embedding-setup", "no local model configured");
        assert!(skipped.skipped);
        assert_eq!(
            skipped.skip_reason.as_deref(),
            Some("no local model configured")
        );
        assert!(skipped.duration_ms == 0.0);
    }

    #[test]
    fn environment_collects_non_sensitive_context() {
        let env = collect_environment("fixture", 10466, "jsonl");
        assert!(!env.os.is_empty());
        assert!(!env.cpu_class.is_empty());
        assert!(env.rust_profile == "debug" || env.rust_profile == "release");
        assert!(!env.egregore_version.is_empty());
        assert_eq!(env.corpus_name, "fixture");
        assert_eq!(env.record_count, 10466);
        assert_eq!(env.store_kind, "jsonl");
    }

    #[test]
    fn query_class_names_are_stable() {
        assert_eq!(
            QueryClass::all().map(QueryClass::as_str),
            ["symbol", "file", "symbol-at-commit", "drift"]
        );
    }

    #[test]
    fn summarize_warm_cell_sorts_and_gates() {
        let answer = AnswerSummary {
            rows: 1,
            record_ids: vec!["rec-001".to_owned()],
            handles: vec!["symbols.rs:10-40".to_owned()],
        };
        let cell = summarize_warm_cell(
            QueryClass::Symbol,
            vec![300.0, 100.0, 200.0],
            answer,
            true,
            2000.0,
        )
        .expect("samples");
        assert_eq!(cell.samples_ms, vec![100.0, 200.0, 300.0]);
        assert!(cell.within_budget);
        assert!(cell.deterministic);

        let over = summarize_warm_cell(
            QueryClass::Symbol,
            vec![2100.0],
            AnswerSummary {
                rows: 1,
                record_ids: vec!["r".to_owned()],
                handles: vec!["h".to_owned()],
            },
            true,
            2000.0,
        )
        .expect("samples");
        assert!(!over.within_budget);
        assert!(
            summarize_warm_cell(
                QueryClass::Symbol,
                Vec::new(),
                AnswerSummary {
                    rows: 0,
                    record_ids: Vec::new(),
                    handles: Vec::new(),
                },
                true,
                2000.0
            )
            .is_none()
        );
    }
}
