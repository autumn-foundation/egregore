//! Semantic search relevance evaluation harness.
//!
//! Reads a corpus of natural-language queries with reviewed expected code handles,
//! runs them against an existing embedded store, and computes top-1 accuracy,
//! top-3 recall, mean reciprocal rank, and false-positive count for ambiguous queries.
//!
//! This module does not introduce a new embedding provider, ranking algorithm,
//! graph domain, schema field, daemon verb, or MCP tool. It consumes the existing
//! `eg query semantic` surfaces only.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

use crate::ir::SourceSpan;

/// Parsed corpus of natural-language queries with reviewed expected targets.
#[derive(Debug, Clone, Deserialize)]
pub struct SemanticRelevanceCorpus {
    /// Schema version for the corpus format.
    pub corpus_version: String,
    /// Human-readable description of the corpus scope.
    pub description: String,
    /// All queries in the corpus.
    pub queries: Vec<CorpusQuery>,
}

impl SemanticRelevanceCorpus {
    /// Load a corpus from a JSON file.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or the JSON is invalid.
    pub fn from_json_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read corpus file {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse corpus JSON from {}", path.display()))
    }

    /// Returns only labeled (non-ambiguous) queries.
    #[must_use]
    pub fn labeled_queries(&self) -> Vec<&CorpusQuery> {
        self.queries
            .iter()
            .filter(|q| q.class != QueryClass::Ambiguous)
            .collect()
    }

    /// Returns only ambiguous queries (false-positive risk pool).
    #[must_use]
    pub fn ambiguous_queries(&self) -> Vec<&CorpusQuery> {
        self.queries
            .iter()
            .filter(|q| q.class == QueryClass::Ambiguous)
            .collect()
    }
}

/// One natural-language query with reviewed expected targets and boring-substitute comparison.
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusQuery {
    /// Stable query identifier (e.g. "q001").
    pub id: String,
    /// Natural-language query text sent to the embedding model.
    pub text: String,
    /// Query class for stratified reporting.
    pub class: QueryClass,
    /// Reviewed expected code handles (file paths ± symbol names).
    pub expected: Vec<ExpectedTarget>,
    /// Boring substitute formulation using `rg`/`git grep`.
    pub rg_substitute: Option<BoringSubstitute>,
}

/// Semantic query class for stratified analysis.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    /// Concept terms that are absent from the symbol name.
    ConceptAbsent,
    /// Queries that use different vocabulary than the implementation.
    SynonymHeavy,
    /// File-level architectural questions.
    Architecture,
    /// Error-handling and failure-path queries.
    ErrorHandling,
    /// Database, ingest, and query-path questions.
    PersistenceQuery,
    /// No clear correct answer; counted as false-positive risk.
    Ambiguous,
}

/// Reviewed expected code handle for one natural-language query.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedTarget {
    /// Repository-relative path to the target file.
    pub repo_relative_path: String,
    /// Symbol name within the file, if the query targets a specific symbol.
    pub symbol_name: Option<String>,
    /// Human-readable note explaining why this is the correct target.
    pub note: Option<String>,
}

/// Boring-substitute formulation for a labeled query.
#[derive(Debug, Clone, Deserialize)]
pub struct BoringSubstitute {
    /// `rg`/`git grep` keyword formulation.
    pub keyword: String,
    /// Note explaining what the substitute can and cannot answer.
    pub note: String,
    /// Whether `rg` can answer the semantic intent of this query.
    pub rg_can_answer_semantic_intent: bool,
}

/// One search result returned by the embedded store.
///
/// Mirrors `adapters::aletheiadb::SemanticMatch` without requiring the
/// `embedded-aletheiadb` feature, enabling feature-free metrics tests.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// Stable codegraph record ID.
    pub record_id: String,
    /// Human-readable name when available.
    pub name: Option<String>,
    /// Repository-relative path when available.
    pub repo_relative_path: Option<String>,
    /// Cosine similarity score (higher = more similar).
    pub score: f32,
    /// Source span when available.
    pub span: Option<SourceSpan>,
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
impl From<&crate::adapters::SemanticMatch> for SearchHit {
    fn from(m: &crate::adapters::SemanticMatch) -> Self {
        Self {
            record_id: m.record_id.clone(),
            name: m.name.clone(),
            repo_relative_path: m.repo_relative_path.clone(),
            score: m.score,
            span: m.span,
        }
    }
}

/// Per-query evaluation result.
#[derive(Debug, Clone)]
pub struct QueryEvalResult {
    /// Stable query identifier.
    pub query_id: String,
    /// Natural-language query text.
    pub query_text: String,
    /// Query class.
    pub class: QueryClass,
    /// Top-k results returned by the store.
    pub top_k_results: Vec<SearchHit>,
    /// Whether any expected target appeared at rank 1.
    pub top1_hit: bool,
    /// Whether any expected target appeared in the top 3.
    pub top3_hit: bool,
    /// Reciprocal rank of the first hit (0.0 if no hit).
    pub reciprocal_rank: f64,
    /// For ambiguous queries: true if the store returned any results (false-positive risk).
    pub is_false_positive: bool,
}

/// Aggregate evaluation metrics over the corpus.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalMetrics {
    /// Number of labeled (non-ambiguous) queries.
    pub labeled_count: usize,
    /// Number of ambiguous queries.
    pub ambiguous_count: usize,
    /// Fraction of labeled queries where the expected target appeared at rank 1.
    pub top1_accuracy: f64,
    /// Fraction of labeled queries where the expected target appeared in the top 3.
    pub top3_recall: f64,
    /// Mean reciprocal rank over labeled queries.
    pub mean_reciprocal_rank: f64,
    /// Number of ambiguous queries that returned any result.
    pub false_positive_count: usize,
}

/// Full evaluation report combining per-query results and aggregate metrics.
#[derive(Debug, Clone)]
pub struct EvalReport {
    /// Aggregate metrics.
    pub metrics: EvalMetrics,
    /// Per-query evaluation results in corpus order.
    pub query_results: Vec<QueryEvalResult>,
    /// Minimum top-3 recall required to pass.
    pub threshold: f64,
    /// Whether the evaluation met the success metric.
    pub passed: bool,
}

/// Returns `true` if any of the expected targets matches the search hit.
///
/// A file-level expected target (no `symbol_name`) is satisfied by any result
/// from that file. A symbol-level target requires both path and name to match.
#[must_use]
pub fn hit_matches_expected(hit: &SearchHit, expected: &[ExpectedTarget]) -> bool {
    expected.iter().any(|exp| {
        let path_matches =
            hit.repo_relative_path.as_deref() == Some(exp.repo_relative_path.as_str());
        if !path_matches {
            return false;
        }
        exp.symbol_name.as_deref().is_none_or(|sym| {
            hit.name.as_deref() == Some(sym)
                || hit
                    .name
                    .as_deref()
                    .is_some_and(|n| n.ends_with(&format!("::{sym}")))
        })
    })
}

/// Evaluates one query's top-k results against its reviewed expected targets.
///
/// Ambiguous queries are never counted as hits. An ambiguous query is a false
/// positive only when at least one returned result exceeds `fp_score_threshold`
/// — this prevents HNSW from making the count saturate at `ambiguous_count`
/// when every query unconditionally returns `top_k` candidates regardless of
/// similarity.
#[must_use]
pub fn evaluate_query(
    query: &CorpusQuery,
    results: &[SearchHit],
    fp_score_threshold: f32,
) -> QueryEvalResult {
    let is_ambiguous = query.class == QueryClass::Ambiguous;

    let (top1_hit, top3_hit, reciprocal_rank) = if is_ambiguous {
        (false, false, 0.0)
    } else {
        let mut top1 = false;
        let mut top3 = false;
        let mut rr = 0.0_f64;

        for (i, hit) in results.iter().enumerate() {
            if hit_matches_expected(hit, &query.expected) {
                let rank = i + 1;
                if rank == 1 {
                    top1 = true;
                }
                if rank <= 3 {
                    top3 = true;
                }
                rr = 1.0 / f64::from(u32::try_from(rank).unwrap_or(u32::MAX));
                break;
            }
        }
        (top1, top3, rr)
    };

    let is_false_positive = is_ambiguous && results.iter().any(|h| h.score >= fp_score_threshold);

    QueryEvalResult {
        query_id: query.id.clone(),
        query_text: query.text.clone(),
        class: query.class.clone(),
        top_k_results: results.to_vec(),
        top1_hit,
        top3_hit,
        reciprocal_rank,
        is_false_positive,
    }
}

/// Computes aggregate metrics over a slice of per-query evaluation results.
///
/// Ambiguous queries are excluded from top-1, top-3, and MRR; they are counted
/// separately in `false_positive_count`.
#[must_use]
pub fn compute_metrics(results: &[QueryEvalResult]) -> EvalMetrics {
    let labeled: Vec<&QueryEvalResult> = results
        .iter()
        .filter(|r| r.class != QueryClass::Ambiguous)
        .collect();
    let ambiguous: Vec<&QueryEvalResult> = results
        .iter()
        .filter(|r| r.class == QueryClass::Ambiguous)
        .collect();

    let labeled_count = labeled.len();
    let ambiguous_count = ambiguous.len();
    let false_positive_count = ambiguous.iter().filter(|r| r.is_false_positive).count();

    if labeled_count == 0 {
        return EvalMetrics {
            labeled_count: 0,
            ambiguous_count,
            top1_accuracy: 0.0,
            top3_recall: 0.0,
            mean_reciprocal_rank: 0.0,
            false_positive_count,
        };
    }

    let top1_hits = labeled.iter().filter(|r| r.top1_hit).count();
    let top3_hits = labeled.iter().filter(|r| r.top3_hit).count();
    let rr_sum: f64 = labeled.iter().map(|r| r.reciprocal_rank).sum();

    let lc = f64::from(u32::try_from(labeled_count).unwrap_or(u32::MAX));
    EvalMetrics {
        labeled_count,
        ambiguous_count,
        top1_accuracy: f64::from(u32::try_from(top1_hits).unwrap_or(u32::MAX)) / lc,
        top3_recall: f64::from(u32::try_from(top3_hits).unwrap_or(u32::MAX)) / lc,
        mean_reciprocal_rank: rr_sum / lc,
        false_positive_count,
    }
}

/// Assembles a full evaluation report from per-query results and a threshold.
#[must_use]
pub fn build_report(results: Vec<QueryEvalResult>, threshold: f64) -> EvalReport {
    let metrics = compute_metrics(&results);
    let passed = metrics.top3_recall >= threshold;
    EvalReport {
        metrics,
        query_results: results,
        threshold,
        passed,
    }
}

/// Formats a human-readable diagnostic listing the failed queries and their top results.
///
/// Printed to stderr when the evaluation fails; includes the query IDs and
/// observed top results so the operator can diagnose retrieval quality.
#[must_use]
pub fn format_diagnostic(report: &EvalReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "FAIL: top-3 recall {:.1}% is below threshold {:.1}%",
        report.metrics.top3_recall * 100.0,
        report.threshold * 100.0,
    ));
    lines.push(format!(
        "  top-1 accuracy={:.1}%  top-3 recall={:.1}%  MRR={:.4}  false-positives={}",
        report.metrics.top1_accuracy * 100.0,
        report.metrics.top3_recall * 100.0,
        report.metrics.mean_reciprocal_rank,
        report.metrics.false_positive_count,
    ));
    lines.push("Missed query IDs and observed top results:".to_owned());

    for result in &report.query_results {
        if result.class == QueryClass::Ambiguous || result.top3_hit {
            continue;
        }
        lines.push(format!("  [{}] {}", result.query_id, result.query_text));
        if result.top_k_results.is_empty() {
            lines.push("    (no results returned)".to_owned());
        } else {
            for hit in result.top_k_results.iter().take(3) {
                lines.push(format!(
                    "    score={:.4}  path={}  name={}",
                    hit.score,
                    hit.repo_relative_path.as_deref().unwrap_or("(none)"),
                    hit.name.as_deref().unwrap_or("(none)"),
                ));
            }
        }
    }

    lines.join("\n")
}

/// Formats the full evaluation report to a writer.
///
/// # Errors
/// Returns an error if writing fails.
pub fn print_report<W: std::io::Write>(report: &EvalReport, mut out: W) -> anyhow::Result<()> {
    writeln!(
        out,
        "Semantic relevance evaluation — {} labeled, {} ambiguous",
        report.metrics.labeled_count, report.metrics.ambiguous_count
    )?;
    let top1_count = report
        .query_results
        .iter()
        .filter(|r| r.class != QueryClass::Ambiguous && r.top1_hit)
        .count();
    let top3_count = report
        .query_results
        .iter()
        .filter(|r| r.class != QueryClass::Ambiguous && r.top3_hit)
        .count();
    writeln!(
        out,
        "  top-1 accuracy:    {:.1}% ({}/{})",
        report.metrics.top1_accuracy * 100.0,
        top1_count,
        report.metrics.labeled_count,
    )?;
    writeln!(
        out,
        "  top-3 recall:      {:.1}% ({}/{})",
        report.metrics.top3_recall * 100.0,
        top3_count,
        report.metrics.labeled_count,
    )?;
    writeln!(
        out,
        "  mean recip. rank:  {:.4}",
        report.metrics.mean_reciprocal_rank
    )?;
    writeln!(
        out,
        "  false positives:   {} / {}",
        report.metrics.false_positive_count, report.metrics.ambiguous_count
    )?;
    writeln!(
        out,
        "  threshold:         {:.1}%  status: {}",
        report.threshold * 100.0,
        if report.passed { "PASS" } else { "FAIL" }
    )?;
    writeln!(out)?;

    writeln!(out, "Per-query results:")?;
    for r in &report.query_results {
        if r.class == QueryClass::Ambiguous {
            writeln!(
                out,
                "  [{}] AMBIGUOUS  fp={}  {}",
                r.query_id, r.is_false_positive, r.query_text
            )?;
        } else {
            let status = match (r.top1_hit, r.top3_hit) {
                (true, _) => "HIT@1",
                (false, true) => "HIT@3",
                _ => "MISS ",
            };
            writeln!(
                out,
                "  [{}] {}  rr={:.4}  {}",
                r.query_id, status, r.reciprocal_rank, r.query_text
            )?;
            for hit in r.top_k_results.iter().take(3) {
                writeln!(
                    out,
                    "         score={:.4}  {}  {}",
                    hit.score,
                    hit.repo_relative_path.as_deref().unwrap_or("(none)"),
                    hit.name.as_deref().unwrap_or(""),
                )?;
            }
        }
    }

    Ok(())
}
