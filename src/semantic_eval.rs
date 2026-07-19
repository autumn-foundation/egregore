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
use serde::{Deserialize, Serialize};

use crate::ir::SourceSpan;

/// Parsed corpus of natural-language queries with reviewed expected targets.
#[derive(Debug, Clone, Deserialize)]
pub struct SemanticRelevanceCorpus {
    /// Schema version for the corpus format.
    pub corpus_version: String,
    /// Human-readable description of the corpus scope.
    pub description: String,
    /// The exact source snapshot the corpus was authored against (issue #106).
    ///
    /// Pins a commit SHA (or a committed-fixture marker) so the reviewed
    /// expected targets stay valid and the eval stays deterministic. Optional
    /// with `#[serde(default)]` so pre-#106 corpora (which lacked the pin) still
    /// deserialize; a corpus intended for the relevance gate should always carry
    /// it.
    #[serde(default)]
    pub source_snapshot: Option<SourceSnapshot>,
    /// All queries in the corpus.
    pub queries: Vec<CorpusQuery>,
}

/// The source snapshot a relevance corpus (or ranking fixture) was authored
/// against (issue #106).
///
/// Re-pinning `commit` invalidates the reviewed expected targets, so the floors
/// must be re-validated whenever the snapshot changes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SourceSnapshot {
    /// Pinned commit SHA, or a marker such as `"fixture"` for a committed
    /// synthetic fixture that is not tied to a source checkout.
    pub commit: String,
    /// Optional human-readable note about how the snapshot was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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

// ===========================================================================
// Issue #106 — falsifiable + gate-able relevance harness.
//
// Extends the #58 baseline with hit-rate@k (k = 1, 5, 10), recall, and a floor
// gate (hit-rate@5 >= 0.90 AND MRR >= 0.70). All logic below is pure and
// feature-free so it is exercised offline over committed ranking fixtures — no
// embedding model or live store required — mirroring the #58 metric functions.
// ===========================================================================

/// Primary floor: minimum hit-rate@5 the corpus must clear (issue #106).
pub const DEFAULT_MIN_HIT_RATE_AT_5: f64 = 0.90;

/// Secondary floor: minimum mean reciprocal rank the corpus must clear (issue #106).
pub const DEFAULT_MIN_MRR: f64 = 0.70;

/// Determinism tolerance for metric-value comparison across runs (issue #106).
///
/// Byte-identical rankings must reproduce identical metric values; where f32
/// embedding scores are involved the same store must reproduce metrics within
/// this tolerance, consistent with the drift-replay tolerance.
pub const RELEVANCE_DETERMINISM_TOLERANCE: f64 = 1e-5;

/// Schema tag stamped on the emitted relevance report.
pub const RELEVANCE_REPORT_SCHEMA: &str = "semantic_relevance_report/v1";

/// The pass/fail floors enforced by the relevance gate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RelevanceFloors {
    /// Minimum hit-rate@5 (fraction of labeled queries whose expected target is
    /// within the top 5 returned results).
    pub min_hit_rate_at_5: f64,
    /// Minimum mean reciprocal rank over labeled queries.
    pub min_mrr: f64,
}

impl Default for RelevanceFloors {
    fn default() -> Self {
        Self {
            min_hit_rate_at_5: DEFAULT_MIN_HIT_RATE_AT_5,
            min_mrr: DEFAULT_MIN_MRR,
        }
    }
}

/// A ranking fixture: pre-computed, canonically-ordered returned hits per query.
///
/// Lets the metric + gate harness run deterministically offline (no embeddings).
#[derive(Debug, Clone, Deserialize)]
pub struct RelevanceRankingFixture {
    /// Human-readable description of the fixture.
    pub description: String,
    /// Source snapshot the fixture was authored against (issue #106).
    #[serde(default)]
    pub source_snapshot: Option<SourceSnapshot>,
    /// Queries with their reviewed expected targets and ranked returned hits.
    pub queries: Vec<RankedQuery>,
}

impl RelevanceRankingFixture {
    /// Load a ranking fixture from a JSON file.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or the JSON is invalid.
    pub fn from_json_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read ranking fixture {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| {
            format!(
                "failed to parse ranking fixture JSON from {}",
                path.display()
            )
        })
    }
}

/// One query in a ranking fixture: reviewed expected targets plus the ranked
/// list of returned hits (best-first, already canonically ordered).
#[derive(Debug, Clone, Deserialize)]
pub struct RankedQuery {
    /// Stable query identifier.
    pub id: String,
    /// Natural-language query text.
    pub text: String,
    /// Query class for stratified reporting.
    pub class: QueryClass,
    /// Reviewed expected code handles (empty for ambiguous queries).
    #[serde(default)]
    pub expected: Vec<ExpectedTarget>,
    /// Ranked returned hits, best-first.
    #[serde(default)]
    pub ranked_hits: Vec<FixtureHit>,
}

/// One returned hit in a ranking fixture (a feature-free, deserializable
/// stand-in for a store-returned `SearchHit`).
#[derive(Debug, Clone, Deserialize)]
pub struct FixtureHit {
    /// Stable record ID.
    pub record_id: String,
    /// Human-readable name when available.
    #[serde(default)]
    pub name: Option<String>,
    /// Repository-relative path when available.
    #[serde(default)]
    pub repo_relative_path: Option<String>,
    /// Similarity score (higher = more similar).
    pub score: f32,
}

impl FixtureHit {
    /// Converts a fixture hit into a `SearchHit` for scoring.
    #[must_use]
    pub fn as_search_hit(&self) -> SearchHit {
        SearchHit {
            record_id: self.record_id.clone(),
            name: self.name.clone(),
            repo_relative_path: self.repo_relative_path.clone(),
            score: self.score,
            span: None,
        }
    }
}

/// Per-query relevance evaluation carrying the first-hit rank needed for
/// hit-rate@k and recall (issue #106).
#[derive(Debug, Clone)]
pub struct RelevanceQueryResult {
    /// Stable query identifier.
    pub query_id: String,
    /// Natural-language query text.
    pub query_text: String,
    /// Query class.
    pub class: QueryClass,
    /// Whether this is an ambiguous (no-correct-answer) query.
    pub is_ambiguous: bool,
    /// 1-based rank of the first expected hit, or `None` if not found in the
    /// returned list.
    pub first_hit_rank: Option<usize>,
    /// Reciprocal rank of the first hit (0.0 if no hit).
    pub reciprocal_rank: f64,
    /// For ambiguous queries: true if any returned result meets the FP score
    /// threshold.
    pub is_false_positive: bool,
    /// Returned hits retained for miss reporting.
    pub returned: Vec<SearchHit>,
    /// Reviewed expected targets (for miss reporting).
    pub expected: Vec<ExpectedTarget>,
}

/// Evaluates one query's ranked results, recording the first-hit rank.
///
/// Ambiguous queries never count as hits; an ambiguous query is a false
/// positive only when a returned result meets `fp_score_threshold`.
#[must_use]
pub fn evaluate_relevance_query(
    query_id: &str,
    query_text: &str,
    class: &QueryClass,
    expected: &[ExpectedTarget],
    hits: &[SearchHit],
    fp_score_threshold: f32,
) -> RelevanceQueryResult {
    let is_ambiguous = *class == QueryClass::Ambiguous;

    let first_hit_rank = if is_ambiguous {
        None
    } else {
        hits.iter()
            .position(|hit| hit_matches_expected(hit, expected))
            .map(|i| i + 1)
    };

    let reciprocal_rank = first_hit_rank.map_or(0.0, |rank| {
        1.0 / f64::from(u32::try_from(rank).unwrap_or(u32::MAX))
    });

    let is_false_positive = is_ambiguous && hits.iter().any(|h| h.score >= fp_score_threshold);

    RelevanceQueryResult {
        query_id: query_id.to_owned(),
        query_text: query_text.to_owned(),
        class: class.clone(),
        is_ambiguous,
        first_hit_rank,
        reciprocal_rank,
        is_false_positive,
        returned: hits.to_vec(),
        expected: expected.to_vec(),
    }
}

/// Evaluates every query in a ranking fixture (convenience for offline tests).
#[must_use]
pub fn evaluate_relevance_fixture(
    fixture: &RelevanceRankingFixture,
    fp_score_threshold: f32,
) -> Vec<RelevanceQueryResult> {
    fixture
        .queries
        .iter()
        .map(|q| {
            let hits: Vec<SearchHit> = q
                .ranked_hits
                .iter()
                .map(FixtureHit::as_search_hit)
                .collect();
            evaluate_relevance_query(
                &q.id,
                &q.text,
                &q.class,
                &q.expected,
                &hits,
                fp_score_threshold,
            )
        })
        .collect()
}

/// Fraction of labeled queries whose expected target appears within the top `k`
/// returned results. Returns 0.0 when there are no labeled queries.
#[must_use]
pub fn hit_rate_at_k(results: &[RelevanceQueryResult], k: usize) -> f64 {
    let labeled: Vec<&RelevanceQueryResult> = results.iter().filter(|r| !r.is_ambiguous).collect();
    if labeled.is_empty() {
        return 0.0;
    }
    let hits = labeled
        .iter()
        .filter(|r| r.first_hit_rank.is_some_and(|rank| rank <= k))
        .count();
    ratio(hits, labeled.len())
}

/// Recall over labeled queries: the fraction whose expected target appears
/// anywhere in the returned candidate list (hit-rate at the full retrieval
/// depth). Returns 0.0 when there are no labeled queries.
#[must_use]
pub fn recall_rate(results: &[RelevanceQueryResult]) -> f64 {
    let labeled: Vec<&RelevanceQueryResult> = results.iter().filter(|r| !r.is_ambiguous).collect();
    if labeled.is_empty() {
        return 0.0;
    }
    let found = labeled
        .iter()
        .filter(|r| r.first_hit_rank.is_some())
        .count();
    ratio(found, labeled.len())
}

fn ratio(num: usize, den: usize) -> f64 {
    if den == 0 {
        return 0.0;
    }
    f64::from(u32::try_from(num).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(den).unwrap_or(u32::MAX))
}

/// Aggregate relevance metrics over a slice of per-query results (issue #106).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RelevanceMetrics {
    /// Number of labeled (non-ambiguous) queries.
    pub labeled_count: usize,
    /// Number of ambiguous queries.
    pub ambiguous_count: usize,
    /// Hit-rate@1 over labeled queries.
    pub hit_rate_at_1: f64,
    /// Hit-rate@5 over labeled queries (primary gate metric).
    pub hit_rate_at_5: f64,
    /// Hit-rate@10 over labeled queries.
    pub hit_rate_at_10: f64,
    /// Mean reciprocal rank over labeled queries (secondary gate metric).
    pub mean_reciprocal_rank: f64,
    /// Recall over labeled queries (hit rate at full retrieval depth).
    pub recall: f64,
    /// Number of ambiguous queries that returned a result meeting the FP threshold.
    pub false_positive_count: usize,
}

/// Computes aggregate relevance metrics. Ambiguous queries are excluded from
/// hit-rate/MRR/recall and counted only in `false_positive_count`.
#[must_use]
pub fn compute_relevance_metrics(results: &[RelevanceQueryResult]) -> RelevanceMetrics {
    let labeled: Vec<&RelevanceQueryResult> = results.iter().filter(|r| !r.is_ambiguous).collect();
    let ambiguous: Vec<&RelevanceQueryResult> = results.iter().filter(|r| r.is_ambiguous).collect();

    let rr_sum: f64 = labeled.iter().map(|r| r.reciprocal_rank).sum();
    let mean_reciprocal_rank = if labeled.is_empty() {
        0.0
    } else {
        rr_sum / f64::from(u32::try_from(labeled.len()).unwrap_or(u32::MAX))
    };

    RelevanceMetrics {
        labeled_count: labeled.len(),
        ambiguous_count: ambiguous.len(),
        hit_rate_at_1: hit_rate_at_k(results, 1),
        hit_rate_at_5: hit_rate_at_k(results, 5),
        hit_rate_at_10: hit_rate_at_k(results, 10),
        mean_reciprocal_rank,
        recall: recall_rate(results),
        false_positive_count: ambiguous.iter().filter(|r| r.is_false_positive).count(),
    }
}

/// A breached floor: the metric name, its observed value, and the floor it
/// missed (issue #106 machine-readable diagnostic).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FloorBreach {
    /// Stable metric name (`hit_rate_at_5` or `mrr`).
    pub metric: &'static str,
    /// Observed value of the metric.
    pub observed: f64,
    /// Floor the metric failed to meet.
    pub floor: f64,
}

/// One returned hit in a miss report row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReturnedHit {
    /// Stable record ID.
    pub record_id: String,
    /// Repository-relative path when available.
    pub repo_relative_path: Option<String>,
    /// Human-readable name when available.
    pub name: Option<String>,
    /// Similarity score.
    pub score: f32,
}

/// A missed labeled query (expected target not in the top 5) reported for
/// debugging without re-running the eval by hand.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RelevanceMiss {
    /// Stable query identifier.
    pub query_id: String,
    /// Natural-language query text.
    pub query_text: String,
    /// Query class.
    pub class: QueryClass,
    /// 1-based rank of the first hit, or `None` if not found at all.
    pub first_hit_rank: Option<usize>,
    /// Reviewed expected handles (`path` or `path::symbol`).
    pub expected: Vec<String>,
    /// Actual returned top hits.
    pub returned: Vec<ReturnedHit>,
}

/// The full deterministic, redaction-safe relevance report (issue #106).
#[derive(Debug, Clone, Serialize)]
pub struct RelevanceReport {
    /// Schema tag.
    pub schema: &'static str,
    /// Overall pass/fail: `true` iff the capability is available and no floor is breached.
    pub ok: bool,
    /// Capability marker: `"available"` or a `requires_*`/`*_unavailable` reason.
    pub capability: String,
    /// Enforced floors.
    pub floors: RelevanceFloors,
    /// Determinism tolerance in effect.
    pub determinism_tolerance: f64,
    /// The source snapshot the corpus/fixture pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_snapshot: Option<SourceSnapshot>,
    /// Aggregate metrics (absent when the capability is unavailable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<RelevanceMetrics>,
    /// Breached floors, each naming its metric and observed value.
    pub breaches: Vec<FloorBreach>,
    /// Missed labeled queries with expected vs returned handles.
    pub misses: Vec<RelevanceMiss>,
}

fn expected_handle(target: &ExpectedTarget) -> String {
    target.symbol_name.as_ref().map_or_else(
        || target.repo_relative_path.clone(),
        |sym| format!("{}::{sym}", target.repo_relative_path),
    )
}

/// Assembles a relevance report from per-query results and the floors.
///
/// A labeled query whose expected target is not within the top 5 is listed as a
/// miss (with its expected and actual returned handles). The report is still
/// fully populated on gate failure so callers print it before exiting nonzero.
#[must_use]
pub fn build_relevance_report(
    results: &[RelevanceQueryResult],
    floors: RelevanceFloors,
    source_snapshot: Option<SourceSnapshot>,
) -> RelevanceReport {
    let metrics = compute_relevance_metrics(results);

    let mut breaches = Vec::new();
    if metrics.hit_rate_at_5 < floors.min_hit_rate_at_5 {
        breaches.push(FloorBreach {
            metric: "hit_rate_at_5",
            observed: metrics.hit_rate_at_5,
            floor: floors.min_hit_rate_at_5,
        });
    }
    if metrics.mean_reciprocal_rank < floors.min_mrr {
        breaches.push(FloorBreach {
            metric: "mrr",
            observed: metrics.mean_reciprocal_rank,
            floor: floors.min_mrr,
        });
    }

    let misses: Vec<RelevanceMiss> = results
        .iter()
        .filter(|r| !r.is_ambiguous && r.first_hit_rank.is_none_or(|rank| rank > 5))
        .map(|r| RelevanceMiss {
            query_id: r.query_id.clone(),
            query_text: r.query_text.clone(),
            class: r.class.clone(),
            first_hit_rank: r.first_hit_rank,
            expected: r.expected.iter().map(expected_handle).collect(),
            returned: r
                .returned
                .iter()
                .take(10)
                .map(|h| ReturnedHit {
                    record_id: h.record_id.clone(),
                    repo_relative_path: h.repo_relative_path.clone(),
                    name: h.name.clone(),
                    score: h.score,
                })
                .collect(),
        })
        .collect();

    let ok = breaches.is_empty();

    RelevanceReport {
        schema: RELEVANCE_REPORT_SCHEMA,
        ok,
        capability: "available".to_owned(),
        floors,
        determinism_tolerance: RELEVANCE_DETERMINISM_TOLERANCE,
        source_snapshot,
        metrics: Some(metrics),
        breaches,
        misses,
    }
}

/// Builds an honest capability-unavailable report (e.g. embeddings feature off).
///
/// Never reports `ok: true`; carries no metrics. The `reason` is a stable
/// machine-readable marker such as `requires_embeddings_feature`.
#[must_use]
pub fn capability_unavailable_report(floors: RelevanceFloors, reason: &str) -> RelevanceReport {
    RelevanceReport {
        schema: RELEVANCE_REPORT_SCHEMA,
        ok: false,
        capability: reason.to_owned(),
        floors,
        determinism_tolerance: RELEVANCE_DETERMINISM_TOLERANCE,
        source_snapshot: None,
        metrics: None,
        breaches: Vec::new(),
        misses: Vec::new(),
    }
}

/// The process exit code for a relevance report, mirroring `eg audit token-cost`.
///
/// `2` when the capability is unavailable (usage/capability error); otherwise
/// `0` on pass and `1` on gate failure.
#[must_use]
pub fn relevance_exit_code(report: &RelevanceReport) -> i32 {
    if report.capability == "available" {
        i32::from(!report.ok)
    } else {
        2
    }
}

/// Renders a relevance report to deterministic canonical JSON.
#[must_use]
pub fn render_relevance_report_json(report: &RelevanceReport) -> String {
    serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_owned())
}

/// Returns `true` if two metric sets agree within `tolerance` on every float
/// field and are exactly equal on every integer count (issue #106 determinism).
#[must_use]
pub fn relevance_metrics_within_tolerance(
    a: &RelevanceMetrics,
    b: &RelevanceMetrics,
    tolerance: f64,
) -> bool {
    a.labeled_count == b.labeled_count
        && a.ambiguous_count == b.ambiguous_count
        && a.false_positive_count == b.false_positive_count
        && (a.hit_rate_at_1 - b.hit_rate_at_1).abs() <= tolerance
        && (a.hit_rate_at_5 - b.hit_rate_at_5).abs() <= tolerance
        && (a.hit_rate_at_10 - b.hit_rate_at_10).abs() <= tolerance
        && (a.mean_reciprocal_rank - b.mean_reciprocal_rank).abs() <= tolerance
        && (a.recall - b.recall).abs() <= tolerance
}
