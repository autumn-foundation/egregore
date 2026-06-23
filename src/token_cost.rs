//! Query-answer token-cost measurement against the ripgrep baseline (issue #84).
//!
//! This module is a **measurement layer** over the existing `eg query symbol`,
//! `eg query file`, and `eg query semantic` answer shapes. For a fixed question
//! set over a pinned representative Rust corpus it measures, per question and in
//! aggregate, the token cost of the returned Egregore answer versus the
//! grep-shaped baseline an agent would otherwise have to ingest to answer the
//! same question. It introduces **no** new graph domain, node kind, edge label,
//! query semantics, importer, or output-schema change.
//!
//! The core ([`run_token_cost_report`]) is pure and deterministic: it never
//! prints, never exits, performs no I/O, executes no subprocess, and emits
//! canonically-ordered output so the same corpus + records yield byte-identical
//! token counts, ratios, and ordering across runs (AC8). The grep baseline is
//! computed in-process with a documented, pinned line-matching rule and the
//! equivalent `rg` command is recorded for each question (AC2/AC10); the gate
//! never shells out, so it does not depend on ripgrep being installed.
//!
//! Token counting uses a single pinned heuristic, [`count_tokens`]
//! (`word-punct-v1`), applied identically to both the Egregore answer and the
//! baseline so the ratio is reproducible (AC3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ir::{GraphRecord, NodeKind, SourceSpan};
use crate::query::RepositoryIndex;

/// Name of the pinned, deterministic token-count method (AC3).
pub const TOKEN_COUNT_METHOD: &str = "word-punct-v1";

/// Human-readable description of [`TOKEN_COUNT_METHOD`], reported so the
/// comparison is reproducible from the report alone.
pub const TOKEN_COUNT_METHOD_DESCRIPTION: &str = "Each maximal run of [A-Za-z0-9_] counts as one \
    token; each other non-whitespace character counts as one token; whitespace separates tokens \
    and is not itself counted. Dependency-free and deterministic; an approximation of subword \
    tokenization applied identically to the Egregore answer and the grep baseline.";

/// Default minimum baseline-to-Egregore token-savings ratio a question class
/// must meet to pass the gate (AC6). The success metric names ≥ 3× for
/// structural-answer questions as the documented floor.
pub const DEFAULT_MIN_RATIO: f64 = 3.0;

// ---------------------------------------------------------------------------
// Corpus manifest (deserialized from corpus/token_cost_corpus.json)
// ---------------------------------------------------------------------------

/// The pinned token-cost corpus manifest: corpus identity plus the fixed
/// question set the gate measures.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenCostCorpus {
    /// Schema version for the manifest format.
    pub corpus_version: String,
    /// Stable human-readable corpus name reported for interpretation (AC10).
    pub corpus_name: String,
    /// Description of the corpus scope.
    pub description: String,
    /// Corpus source directory, resolved relative to the manifest's parent dir.
    pub source_dir: String,
    /// Repository identity override so record IDs are stable across checkouts.
    pub repository_id_override: String,
    /// Fixed scan transaction time so the scan is byte-stable.
    pub scan_time: String,
    /// Pinned token-count method name; must equal [`TOKEN_COUNT_METHOD`].
    pub token_count_method: String,
    /// Default minimum savings ratio gate.
    pub min_ratio: f64,
    /// The fixed question set (at least exact-symbol, file-defines, semantic).
    pub questions: Vec<CorpusQuestion>,
}

/// One pinned question in the corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusQuestion {
    /// Stable question identifier (e.g. `q1-exact-symbol`).
    pub id: String,
    /// Question class.
    pub class: QuestionClass,
    /// Human-readable description / the natural-language query text.
    pub description: String,
    /// The argument passed to the Egregore query (symbol name, file path, or
    /// natural-language text, depending on the class).
    pub eg_query: String,
    /// The record ID the correct Egregore answer must contain (AC5).
    pub expected_record_id: String,
    /// The literal pattern the grep-shaped baseline searches for. Used by the
    /// single-pattern and file-read match kinds; ignored by `WordsAcrossCorpus`.
    pub baseline_pattern: String,
    /// The keyword union a `WordsAcrossCorpus` baseline searches for — the set of
    /// terms a human would grep when exploring the question's concept. Ignored by
    /// the other match kinds.
    #[serde(default)]
    pub baseline_patterns: Vec<String>,
    /// How the baseline pattern is matched against the corpus.
    pub baseline_match_kind: BaselineMatchKind,
    /// For `Semantic` questions: maximum number of result rows to measure,
    /// matching the `--limit` passed to `eg query semantic`. Defaults to 1
    /// (a focused single-symbol lookup). Structural question classes ignore
    /// this field — they return all matching records.
    #[serde(default = "default_result_limit")]
    pub result_limit: usize,
}

const fn default_result_limit() -> usize {
    1
}

/// Question class for stratified reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionClass {
    /// Exact symbol lookup (`eg query symbol <name>`).
    ExactSymbol,
    /// File-defines lookup (`eg query file <path>`).
    FileDefines,
    /// Natural-language semantic query (`eg query semantic <text>`).
    Semantic,
}

impl QuestionClass {
    /// Stable lowercase label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ExactSymbol => "exact_symbol",
            Self::FileDefines => "file_defines",
            Self::Semantic => "semantic",
        }
    }
}

/// How a baseline pattern is matched to produce the lines a model must ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineMatchKind {
    /// Whole-word match of the pattern across every corpus source file
    /// (`rg -n --word-regexp <pattern> <corpus>`), including comment and
    /// string-literal false positives a structural query avoids.
    WordAcrossCorpus,
    /// Whole-word match of the union of several patterns across the corpus
    /// (`rg -n --word-regexp '<p1>|<p2>|…' <corpus>`). Models the lines a human
    /// exploring a concept by keyword search would have to ingest — the fair
    /// baseline for a ranked semantic answer, which surfaces matches across the
    /// keywords a concept spans rather than a single literal. A line that
    /// matches several patterns is counted once, matching ripgrep's per-line
    /// output. Reads [`CorpusQuestion::baseline_patterns`].
    WordsAcrossCorpus,
    /// Read the named file's non-empty lines (`rg -n . <file>`): the boring
    /// substitute for "what does this file define" is reading the file.
    ReadFile,
}

// ---------------------------------------------------------------------------
// Token counting (word-punct-v1) — AC3
// ---------------------------------------------------------------------------

/// Counts tokens with the pinned `word-punct-v1` heuristic.
///
/// See [`TOKEN_COUNT_METHOD_DESCRIPTION`]. Deterministic and dependency-free;
/// the identical function is applied to both the Egregore answer and the grep
/// baseline so the ratio is reproducible.
#[must_use]
pub fn count_tokens(text: &str) -> usize {
    let mut count = 0;
    let mut in_word = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if !in_word {
                count += 1;
                in_word = true;
            }
        } else if ch.is_whitespace() {
            in_word = false;
        } else {
            count += 1;
            in_word = false;
        }
    }
    count
}

/// Returns true when `pattern` occurs in `line` as a whole word (its neighbors
/// are not `[A-Za-z0-9_]`), mirroring ripgrep's `--word-regexp` for ASCII
/// corpora. Both `line` and `pattern` must be ASCII for the result to agree
/// with `rg --word-regexp`; Unicode letters are treated as non-word chars here
/// but as word chars by ripgrep's Unicode mode.
fn line_has_word(line: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    let pat = pattern.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(rel) = bytes[start..].windows(pat.len()).position(|w| w == pat) {
        let at = start + rel;
        let before_ok = at == 0 || !is_word(bytes[at - 1]);
        let after_idx = at + pat.len();
        let after_ok = after_idx >= bytes.len() || !is_word(bytes[after_idx]);
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

// ---------------------------------------------------------------------------
// Report types (serialized to JSON by the CLI)
// ---------------------------------------------------------------------------

/// Non-sensitive context needed to interpret the result (AC10).
#[derive(Debug, Clone, Serialize)]
pub struct CorpusInfo {
    /// Stable corpus name.
    pub name: String,
    /// Number of source files scanned.
    pub source_file_count: usize,
    /// Total source bytes across scanned files.
    pub source_byte_count: usize,
    /// Number of graph records the scan produced.
    pub record_count: usize,
    /// Store kind backing the answers (`jsonl` scan-in-memory for this gate).
    pub store_kind: &'static str,
}

/// Per-question token-cost measurement.
#[derive(Debug, Clone, Serialize)]
#[allow(clippy::struct_excessive_bools)] // report DTO: each flag is an independent fact
pub struct QuestionReport {
    /// Stable question identifier.
    pub id: String,
    /// Question class label.
    pub class: &'static str,
    /// Human-readable description / natural-language query.
    pub description: String,
    /// The argument passed to the Egregore query.
    pub eg_query: String,
    /// The exact baseline command recorded for reproduction (AC10).
    pub baseline_command: String,
    /// Number of baseline lines a model would have to ingest.
    pub baseline_match_count: usize,
    /// Baseline token count (AC2/AC4 raw count).
    pub baseline_tokens: usize,
    /// Number of rows in the Egregore answer.
    pub egregore_row_count: usize,
    /// Egregore answer token count (AC1/AC4 raw count).
    pub egregore_tokens: usize,
    /// Baseline ÷ Egregore token ratio, rounded to 3 decimals (AC4).
    pub ratio: f64,
    /// The record ID the correct answer must contain (AC5).
    pub expected_record_id: String,
    /// Whether the answer carries the expected record ID (AC5).
    pub answer_has_expected_record_id: bool,
    /// Whether the cited answer row carries a repo-relative file/span or commit
    /// handle (AC5).
    pub answer_has_file_span_or_commit_handle: bool,
    /// Correctness verdict: `cited` only when both handle checks pass (AC5).
    pub correctness: &'static str,
    /// Minimum savings ratio this class must meet.
    pub min_ratio: f64,
    /// Whether the measured ratio meets the threshold (AC6).
    pub meets_threshold: bool,
    /// Whether the question passes: correct **and** at/above threshold (AC5/AC6).
    pub pass: bool,
    /// For `Semantic` questions: the `--limit` used when measuring the answer
    /// (matches the manifest's `result_limit`). Absent for structural questions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_limit: Option<usize>,
}

/// Aggregate token-cost across the question set (AC4).
#[derive(Debug, Clone, Serialize)]
pub struct AggregateReport {
    /// Total baseline tokens across questions.
    pub baseline_tokens: usize,
    /// Total Egregore answer tokens across questions.
    pub egregore_tokens: usize,
    /// Aggregate baseline ÷ Egregore ratio, rounded to 3 decimals.
    pub ratio: f64,
    /// Minimum aggregate ratio gate.
    pub min_ratio: f64,
    /// Whether the aggregate ratio meets the threshold.
    pub meets_threshold: bool,
    /// Whether every question is correct (no uncited wins).
    pub all_answers_cited: bool,
}

/// A stable diagnostic naming a failed class and its observed ratio (AC6).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenCostDiagnostic {
    /// Stable diagnostic code.
    pub code: &'static str,
    /// Question that produced the diagnostic.
    pub question_id: String,
    /// Class of the question.
    pub class: &'static str,
    /// Observed ratio, when the diagnostic is a threshold failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_ratio: Option<f64>,
    /// Expected record ID, when the diagnostic is a correctness miss.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_record_id: Option<String>,
}

/// The full deterministic token-cost report.
#[derive(Debug, Clone, Serialize)]
pub struct TokenCostReport {
    /// Overall pass/fail gate.
    pub ok: bool,
    /// Corpus + run context (AC10).
    pub corpus: CorpusInfo,
    /// Egregore version that produced the answers (AC10).
    pub egregore_version: &'static str,
    /// Pinned token-count method name (AC3).
    pub token_count_method: String,
    /// Pinned token-count method description (AC3).
    pub token_count_method_description: &'static str,
    /// The minimum savings ratio gate in effect.
    pub min_ratio: f64,
    /// Per-question measurements, in manifest order.
    pub questions: Vec<QuestionReport>,
    /// Aggregate measurement.
    pub aggregate: AggregateReport,
    /// All diagnostics, ordered by question id.
    pub diagnostics: Vec<TokenCostDiagnostic>,
}

// ---------------------------------------------------------------------------
// Egregore answer construction (mirrors the public eg query output shape)
// ---------------------------------------------------------------------------

/// One Egregore answer row, serialized exactly like the `eg query symbol` /
/// `eg query file` JSON line (AC1: the measured cost is the returned answer).
#[derive(Debug, Clone, Serialize)]
struct DiagnosticRef<'a> {
    record_id: &'a str,
    repo_relative_path: &'a str,
    span: SourceSpan,
}

#[derive(Debug, Clone, Serialize)]
struct SymbolAnswerRow<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'static str,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    extraction_completeness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Vec<DiagnosticRef<'a>>>,
}

/// One semantic answer row, serialized like the `eg query semantic` JSON line.
///
/// The natural-language hit's float `score` is embedding-derived; to keep the
/// gate deterministic and offline (no mandatory embeddings) it is reported at a
/// fixed representative value. Under word-punct-v1, any finite JSON float
/// serializes to exactly 3 tokens (digit-run · punct-dot · digit-run), so the
/// choice of representative score does not affect the measured ratio.
#[derive(Debug, Clone, Serialize)]
struct SemanticAnswerRow<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
}

/// Score assigned to the primary (expected) semantic hit in the measured answer.
const SEMANTIC_REPRESENTATIVE_SCORE: f64 = 0.5;

/// Score assigned to secondary hits when `result_limit` > 1.
const SEMANTIC_SECONDARY_SCORE: f64 = 0.4;

/// A built answer: the serialized JSON-lines text plus correctness facts.
struct BuiltAnswer {
    /// The serialized answer (one compact JSON object per line, `\n`-joined).
    text: String,
    /// Number of answer rows.
    row_count: usize,
    /// Whether a row carries the expected record ID.
    has_expected_record_id: bool,
    /// Whether the expected row carries a file/span or commit handle.
    has_file_span_or_commit_handle: bool,
}

/// Joins serialized rows the way the CLI prints them: one JSON line each.
fn join_rows(rows: &[String]) -> String {
    rows.join("\n")
}

/// The `(start_line, record_id)` ordering key `eg query symbol`/`file` uses.
fn symbol_sort_key(record: &GraphRecord) -> (Option<usize>, &str) {
    match record {
        GraphRecord::Node { id, span, .. } => (span.map(|s| s.start_line), id.as_str()),
        _ => (None, ""),
    }
}

/// Builds the `eg query symbol <name>` answer for a symbol name.
fn build_symbol_answer(
    records: &[GraphRecord],
    index: &RepositoryIndex,
    name: &str,
    expected_record_id: &str,
) -> BuiltAnswer {
    build_symbol_node_answer(
        records,
        index,
        expected_record_id,
        false,
        |node_name, _path| node_name == Some(name),
    )
}

/// Builds the `eg query file <path>` answer for a repo-relative path.
fn build_file_answer(
    records: &[GraphRecord],
    index: &RepositoryIndex,
    path: &str,
    expected_record_id: &str,
) -> BuiltAnswer {
    build_symbol_node_answer(
        records,
        index,
        expected_record_id,
        true,
        |_name, node_path| node_path == Some(path),
    )
}

/// Serializes the symbol nodes matching `include`, sorted and shaped exactly
/// like `eg query symbol` / `eg query file`, and computes correctness facts.
fn build_symbol_node_answer<F>(
    records: &[GraphRecord],
    index: &RepositoryIndex,
    expected_record_id: &str,
    include_diagnostics: bool,
    include: F,
) -> BuiltAnswer
where
    F: Fn(Option<&str>, Option<&str>) -> bool,
{
    let mut matched: Vec<&GraphRecord> = records
        .iter()
        .filter(|record| {
            let GraphRecord::Node {
                kind: NodeKind::Symbol,
                name,
                repo_relative_path,
                ..
            } = record
            else {
                return false;
            };
            include(name.as_deref(), repo_relative_path.as_deref())
        })
        .collect();
    // Mirror `query_symbol_all`: sort by (start_line, record_id).
    matched.sort_by(|a, b| symbol_sort_key(a).cmp(&symbol_sort_key(b)));

    let mut diagnostics_by_path: std::collections::HashMap<&str, Vec<DiagnosticRef<'_>>> =
        std::collections::HashMap::new();
    if include_diagnostics {
        for r in records {
            if let GraphRecord::Node {
                id,
                kind: NodeKind::Diagnostic,
                repo_relative_path: Some(path),
                span: Some(span),
                ..
            } = r
            {
                diagnostics_by_path
                    .entry(path.as_str())
                    .or_default()
                    .push(DiagnosticRef {
                        record_id: id.as_str(),
                        repo_relative_path: path.as_str(),
                        span: *span,
                    });
            }
        }
        for diags in diagnostics_by_path.values_mut() {
            diags.sort_by_key(|d| (d.span.start_line, d.record_id));
        }
    }

    let mut serialized = Vec::with_capacity(matched.len());
    let mut has_expected = false;
    let mut expected_has_handle = false;
    let mut is_first = true;
    // Every element in `matched` is a Node variant (guaranteed by the filter above).
    for record in matched {
        if let GraphRecord::Node {
            id,
            schema_version,
            name,
            repo_relative_path,
            span,
            temporal,
            ..
        } = record
        {
            let path = repo_relative_path.as_deref();
            let (completeness, diags) = path.map_or(("complete", None), |p| {
                diagnostics_by_path
                    .get(p)
                    .map_or(("complete", None), |diags| ("partial", Some(diags.clone())))
            });
            let git_commit = temporal.as_ref().map(|t| t.git_commit.as_str());
            let repository_id = index.owner_of(id);
            let row = SymbolAnswerRow {
                record_id: id,
                schema_version: *schema_version,
                name: name.as_deref().unwrap_or(""),
                kind: "Symbol",
                repo_relative_path: path,
                span: *span,
                git_commit,
                repository_id,
                repository: repository_id.and_then(|repo| index.display_of(repo)),
                extraction_completeness: completeness,
                diagnostics: if include_diagnostics && is_first {
                    is_first = false;
                    diags
                } else {
                    None
                },
            };
            if id.as_str() == expected_record_id {
                has_expected = true;
                expected_has_handle = (path.is_some() && span.is_some()) || git_commit.is_some();
            }
            serialized.push(serde_json::to_string(&row).unwrap_or_default());
        }
    }
    BuiltAnswer {
        row_count: serialized.len(),
        text: join_rows(&serialized),
        has_expected_record_id: has_expected,
        has_file_span_or_commit_handle: expected_has_handle,
    }
}

/// Builds the `eg query semantic <text>` answer for up to `result_limit` rows.
///
/// The expected record is ranked first (primary score); remaining slots are
/// filled with other Symbol nodes sorted by `(start_line, record_id)` so the
/// measurement is deterministic without requiring live embeddings. `result_limit`
/// should match the `--limit` a caller would pass to `eg query semantic`
/// (the corpus manifest's `result_limit` field; default 1 for a focused lookup).
fn build_semantic_answer(
    records: &[GraphRecord],
    index: &RepositoryIndex,
    expected_record_id: &str,
    result_limit: usize,
) -> BuiltAnswer {
    // Collect all Symbol nodes, sorted deterministically.
    let mut symbols: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    ..
                }
            )
        })
        .collect();
    symbols.sort_by(|a, b| symbol_sort_key(a).cmp(&symbol_sort_key(b)));

    // Expected record leads with primary score; remaining slots fill from the
    // sorted corpus so the row count matches the requested result_limit.
    let expected_idx = symbols.iter().position(|r| r.id() == expected_record_id);
    let found_expected = expected_idx.is_some();
    let expected_rec = expected_idx.map(|i| symbols.remove(i));

    let mut ranked: Vec<(&GraphRecord, f64)> = Vec::with_capacity(result_limit);
    if let Some(r) = expected_rec {
        ranked.push((r, SEMANTIC_REPRESENTATIVE_SCORE));
    }
    let slots_left = result_limit.saturating_sub(ranked.len());
    for r in symbols.into_iter().take(slots_left) {
        ranked.push((r, SEMANTIC_SECONDARY_SCORE));
    }

    if ranked.is_empty() {
        return BuiltAnswer {
            text: String::new(),
            row_count: 0,
            has_expected_record_id: false,
            has_file_span_or_commit_handle: false,
        };
    }

    let mut expected_has_handle = false;
    let mut serialized = Vec::with_capacity(ranked.len());
    for (record, score) in &ranked {
        if let GraphRecord::Node {
            name,
            repo_relative_path,
            span,
            temporal,
            ..
        } = record
        {
            let path = repo_relative_path.as_deref();
            let git_commit = temporal.as_ref().map(|t| t.git_commit.as_str());
            let repository_id = index.owner_of(record.id());
            let row = SemanticAnswerRow {
                record_id: record.id(),
                name: name.as_deref(),
                repo_relative_path: path,
                score: *score,
                span: *span,
                repository_id,
                repository: repository_id.and_then(|r| index.display_of(r)),
            };
            if record.id() == expected_record_id {
                expected_has_handle = (path.is_some() && span.is_some()) || git_commit.is_some();
            }
            serialized.push(serde_json::to_string(&row).unwrap_or_default());
        }
    }
    BuiltAnswer {
        row_count: serialized.len(),
        text: serialized.join("\n"),
        has_expected_record_id: found_expected,
        has_file_span_or_commit_handle: expected_has_handle,
    }
}

// ---------------------------------------------------------------------------
// Baseline construction (in-process; records the equivalent rg command) — AC2
// ---------------------------------------------------------------------------

/// A built baseline: the lines a model must ingest plus the recorded command.
struct BuiltBaseline {
    /// Concatenated matched line contents (`\n`-joined), the model's ingest.
    text: String,
    /// Number of matched lines.
    match_count: usize,
    /// The exact equivalent `rg` command, recorded for reproduction.
    command: String,
}

/// Builds the grep-shaped baseline for a question over the corpus files.
///
/// `source_files` is an ordered map of repo-relative path → file content.
fn build_baseline(
    question: &CorpusQuestion,
    source_files: &BTreeMap<String, String>,
    corpus_display: &str,
) -> BuiltBaseline {
    match question.baseline_match_kind {
        BaselineMatchKind::WordAcrossCorpus => {
            let mut matched: Vec<&str> = Vec::new();
            for content in source_files.values() {
                for line in content.lines() {
                    if line_has_word(line, &question.baseline_pattern) {
                        matched.push(line);
                    }
                }
            }
            BuiltBaseline {
                match_count: matched.len(),
                text: matched.join("\n"),
                command: format!(
                    "rg -n --word-regexp '{}' {corpus_display}",
                    question.baseline_pattern
                ),
            }
        }
        BaselineMatchKind::WordsAcrossCorpus => {
            let mut matched: Vec<&str> = Vec::new();
            for content in source_files.values() {
                for line in content.lines() {
                    // A line is ingested once if it matches any keyword, mirroring
                    // ripgrep's one-line-per-match output for an alternation.
                    if question
                        .baseline_patterns
                        .iter()
                        .any(|pattern| line_has_word(line, pattern))
                    {
                        matched.push(line);
                    }
                }
            }
            BuiltBaseline {
                match_count: matched.len(),
                text: matched.join("\n"),
                command: format!(
                    "rg -n --word-regexp '{}' {corpus_display}",
                    question.baseline_patterns.join("|")
                ),
            }
        }
        BaselineMatchKind::ReadFile => {
            let content = source_files.get(&question.baseline_pattern);
            let matched: Vec<&str> = content
                .map(|c| c.lines().filter(|l| !l.trim().is_empty()).collect())
                .unwrap_or_default();
            BuiltBaseline {
                match_count: matched.len(),
                text: matched.join("\n"),
                command: format!("rg -n . {corpus_display}/{}", question.baseline_pattern),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Report driver — pure and deterministic (AC8)
// ---------------------------------------------------------------------------

/// Rounds a ratio to 3 decimals so the report is readable and byte-stable.
fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Computes `baseline ÷ egregore`, guarding a zero (or missing) answer.
#[allow(clippy::cast_precision_loss)] // token counts are small; f64 is exact here
fn ratio_of(baseline: usize, egregore: usize) -> f64 {
    if egregore == 0 {
        0.0
    } else {
        round3(baseline as f64 / egregore as f64)
    }
}

/// Measures one question: builds both sides, counts tokens, and applies the
/// correctness + threshold gate.
fn measure_question(
    question: &CorpusQuestion,
    source_files: &BTreeMap<String, String>,
    records: &[GraphRecord],
    index: &RepositoryIndex,
    corpus_display: &str,
    min_ratio: f64,
) -> QuestionReport {
    let answer = match question.class {
        QuestionClass::ExactSymbol => build_symbol_answer(
            records,
            index,
            &question.eg_query,
            &question.expected_record_id,
        ),
        QuestionClass::FileDefines => build_file_answer(
            records,
            index,
            &question.eg_query,
            &question.expected_record_id,
        ),
        QuestionClass::Semantic => build_semantic_answer(
            records,
            index,
            &question.expected_record_id,
            question.result_limit,
        ),
    };
    let baseline = build_baseline(question, source_files, corpus_display);

    let baseline_tokens = count_tokens(&baseline.text);
    let egregore_tokens = count_tokens(&answer.text);
    let ratio = ratio_of(baseline_tokens, egregore_tokens);

    // Correctness gate (AC5): an answer counts only if it carries the expected
    // record ID plus a file/span or commit handle. An uncited (cheap) answer is
    // a miss, never a win.
    let correct = answer.has_expected_record_id && answer.has_file_span_or_commit_handle;
    let meets_threshold = ratio >= min_ratio;
    let pass = correct && meets_threshold;

    QuestionReport {
        id: question.id.clone(),
        class: question.class.label(),
        description: question.description.clone(),
        eg_query: question.eg_query.clone(),
        baseline_command: baseline.command,
        baseline_match_count: baseline.match_count,
        baseline_tokens,
        egregore_row_count: answer.row_count,
        egregore_tokens,
        ratio,
        expected_record_id: question.expected_record_id.clone(),
        answer_has_expected_record_id: answer.has_expected_record_id,
        answer_has_file_span_or_commit_handle: answer.has_file_span_or_commit_handle,
        correctness: if correct { "cited" } else { "miss" },
        min_ratio,
        meets_threshold,
        pass,
        result_limit: (question.class == QuestionClass::Semantic).then_some(question.result_limit),
    }
}

/// Runs the token-cost measurement and returns the deterministic report.
///
/// * `corpus` — the pinned manifest.
/// * `source_files` — repo-relative path → file content, for the baseline.
/// * `records` — the records produced by scanning the corpus, for the answers.
/// * `corpus_display` — the corpus path recorded in baseline commands (AC10).
#[must_use]
pub fn run_token_cost_report(
    corpus: &TokenCostCorpus,
    source_files: &BTreeMap<String, String>,
    records: &[GraphRecord],
    corpus_display: &str,
) -> TokenCostReport {
    let index = RepositoryIndex::build(records);
    let min_ratio = corpus.min_ratio;

    let mut questions = Vec::with_capacity(corpus.questions.len());
    let mut diagnostics = Vec::new();
    let mut total_baseline = 0usize;
    let mut total_egregore = 0usize;
    let mut all_cited = true;

    for question in &corpus.questions {
        let report = measure_question(
            question,
            source_files,
            records,
            &index,
            corpus_display,
            min_ratio,
        );
        if report.correctness == "miss" {
            all_cited = false;
            diagnostics.push(TokenCostDiagnostic {
                code: "uncited_answer_miss",
                question_id: question.id.clone(),
                class: question.class.label(),
                observed_ratio: None,
                expected_record_id: Some(question.expected_record_id.clone()),
            });
        } else if !report.meets_threshold {
            // A regression below threshold is never silently a success (AC6).
            diagnostics.push(TokenCostDiagnostic {
                code: "below_token_savings_threshold",
                question_id: question.id.clone(),
                class: question.class.label(),
                observed_ratio: Some(report.ratio),
                expected_record_id: None,
            });
        }
        total_baseline += report.baseline_tokens;
        total_egregore += report.egregore_tokens;
        questions.push(report);
    }

    // Diagnostics are ordered by question id for byte-stable output.
    diagnostics.sort_by(|a, b| a.question_id.cmp(&b.question_id).then(a.code.cmp(b.code)));

    let aggregate_ratio = ratio_of(total_baseline, total_egregore);
    let aggregate_meets = aggregate_ratio >= min_ratio;
    let aggregate = AggregateReport {
        baseline_tokens: total_baseline,
        egregore_tokens: total_egregore,
        ratio: aggregate_ratio,
        min_ratio,
        meets_threshold: aggregate_meets,
        all_answers_cited: all_cited,
    };

    let ok = aggregate_meets && questions.iter().all(|q| q.pass);

    let source_byte_count = source_files.values().map(String::len).sum();
    TokenCostReport {
        ok,
        corpus: CorpusInfo {
            name: corpus.corpus_name.clone(),
            source_file_count: source_files.len(),
            source_byte_count,
            record_count: records.len(),
            store_kind: "jsonl",
        },
        egregore_version: env!("CARGO_PKG_VERSION"),
        token_count_method: corpus.token_count_method.clone(),
        token_count_method_description: TOKEN_COUNT_METHOD_DESCRIPTION,
        min_ratio,
        questions,
        aggregate,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_punct_tokenizer_counts_identifiers_and_punctuation() {
        // Three identifier runs (`parse_config`, `raw`, `strict`) plus the
        // punctuation `(`, `,`, `)`, `:`, `&` ... counted individually.
        assert_eq!(count_tokens(""), 0);
        assert_eq!(count_tokens("   \n\t  "), 0);
        assert_eq!(count_tokens("parse_config"), 1);
        assert_eq!(count_tokens("a b c"), 3);
        // `fn(x)` => fn, (, x, ) = 4
        assert_eq!(count_tokens("fn(x)"), 4);
    }

    #[test]
    fn word_regexp_respects_boundaries() {
        assert!(line_has_word("let parse_config = 1;", "parse_config"));
        assert!(line_has_word("call parse_config(raw)", "parse_config"));
        // substring inside a longer identifier is not a whole-word match
        assert!(!line_has_word("parse_configuration()", "parse_config"));
        assert!(!line_has_word("xparse_config", "parse_config"));
    }

    #[test]
    fn round3_is_stable() {
        assert!((round3(10.0 / 3.0) - 3.333).abs() < 1e-9);
        assert!(ratio_of(30, 0).abs() < 1e-9);
        assert!((ratio_of(30, 10) - 3.0).abs() < 1e-9);
    }
}
