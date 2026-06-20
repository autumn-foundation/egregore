//! Agent-memory recall evaluation harness (issue #91).
//!
//! Reads a corpus of natural-language questions with reviewed expected memory
//! record IDs, runs them against an existing embedded store seeded with imported
//! memory records, and computes top-1 accuracy, top-3 recall, and mean
//! reciprocal rank over the recalled memory.
//!
//! This module does not introduce a new embedding provider, ranking algorithm,
//! graph domain, schema field, daemon verb, or MCP tool. It consumes the
//! existing semantic ingest/query machinery only, narrowed to agent-memory
//! observation-class hits.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

/// Parsed corpus of natural-language questions with reviewed expected memory
/// record IDs.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryRecallCorpus {
    /// Schema version for the corpus format.
    pub corpus_version: String,
    /// Human-readable description of the corpus scope.
    pub description: String,
    /// All questions in the corpus.
    pub questions: Vec<MemoryQuery>,
}

impl MemoryRecallCorpus {
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
}

/// One natural-language question with reviewed expected memory targets.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryQuery {
    /// Stable question identifier (e.g. "m001").
    pub id: String,
    /// Natural-language question text sent to the embedding model.
    pub text: String,
    /// Reviewed expected memory record IDs (`agent_memory:v1:...`). At least one
    /// must rank in the top results for the question to count as a hit.
    pub expected_record_ids: Vec<String>,
    /// Human-readable note explaining the recall intent (e.g. "free-floating
    /// lesson, attached to no symbol").
    #[serde(default)]
    pub note: Option<String>,
}

/// One recalled memory hit returned by the store.
///
/// Mirrors the trust-separated `eg query semantic-memory` row shape without
/// requiring the `embedded-aletheiadb` feature, enabling feature-free metrics
/// tests.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    /// Stable agent-memory record ID.
    pub record_id: String,
    /// Cosine similarity score (higher = more similar).
    pub score: f32,
}

/// Per-question evaluation result.
#[derive(Debug, Clone)]
pub struct MemoryQueryEvalResult {
    /// Stable question identifier.
    pub query_id: String,
    /// Natural-language question text.
    pub query_text: String,
    /// Top-k recalled memory record IDs in rank order.
    pub top_k_record_ids: Vec<String>,
    /// Whether any expected target appeared at rank 1.
    pub top1_hit: bool,
    /// Whether any expected target appeared in the top 3.
    pub top3_hit: bool,
    /// Reciprocal rank of the first hit (0.0 if no hit).
    pub reciprocal_rank: f64,
}

/// Aggregate evaluation metrics over the corpus.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEvalMetrics {
    /// Number of questions evaluated.
    pub question_count: usize,
    /// Fraction of questions where an expected target appeared at rank 1.
    pub top1_accuracy: f64,
    /// Fraction of questions where an expected target appeared in the top 3.
    pub top3_recall: f64,
    /// Mean reciprocal rank over all questions.
    pub mean_reciprocal_rank: f64,
}

/// Full evaluation report combining per-question results and aggregate metrics.
#[derive(Debug, Clone)]
pub struct MemoryEvalReport {
    /// Aggregate metrics.
    pub metrics: MemoryEvalMetrics,
    /// Per-question evaluation results in corpus order.
    pub query_results: Vec<MemoryQueryEvalResult>,
    /// Minimum top-3 recall required to pass.
    pub threshold: f64,
    /// Whether the evaluation met the success metric.
    pub passed: bool,
}

/// Canonical ordering for hits: score descending, then record ID ascending so
/// ties resolve deterministically across runs (AC7).
#[must_use]
pub fn canonical_order(hits: &[MemoryHit]) -> Vec<String> {
    let mut ordered: Vec<&MemoryHit> = hits.iter().collect();
    ordered.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    ordered.into_iter().map(|h| h.record_id.clone()).collect()
}

/// Evaluates one question's recalled hits against its reviewed expected targets.
#[must_use]
pub fn evaluate_query(query: &MemoryQuery, hits: &[MemoryHit]) -> MemoryQueryEvalResult {
    let ordered = canonical_order(hits);

    let mut top1 = false;
    let mut top3 = false;
    let mut rr = 0.0_f64;
    for (i, record_id) in ordered.iter().enumerate() {
        if query.expected_record_ids.iter().any(|e| e == record_id) {
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

    MemoryQueryEvalResult {
        query_id: query.id.clone(),
        query_text: query.text.clone(),
        top_k_record_ids: ordered,
        top1_hit: top1,
        top3_hit: top3,
        reciprocal_rank: rr,
    }
}

/// Computes aggregate metrics over a slice of per-question evaluation results.
#[must_use]
pub fn compute_metrics(results: &[MemoryQueryEvalResult]) -> MemoryEvalMetrics {
    let question_count = results.len();
    if question_count == 0 {
        return MemoryEvalMetrics {
            question_count: 0,
            top1_accuracy: 0.0,
            top3_recall: 0.0,
            mean_reciprocal_rank: 0.0,
        };
    }

    let top1_hits = results.iter().filter(|r| r.top1_hit).count();
    let top3_hits = results.iter().filter(|r| r.top3_hit).count();
    let rr_sum: f64 = results.iter().map(|r| r.reciprocal_rank).sum();

    let qc = f64::from(u32::try_from(question_count).unwrap_or(u32::MAX));
    MemoryEvalMetrics {
        question_count,
        top1_accuracy: f64::from(u32::try_from(top1_hits).unwrap_or(u32::MAX)) / qc,
        top3_recall: f64::from(u32::try_from(top3_hits).unwrap_or(u32::MAX)) / qc,
        mean_reciprocal_rank: rr_sum / qc,
    }
}

/// Assembles a full evaluation report from per-question results and a threshold.
#[must_use]
pub fn build_report(results: Vec<MemoryQueryEvalResult>, threshold: f64) -> MemoryEvalReport {
    let metrics = compute_metrics(&results);
    let passed = metrics.top3_recall >= threshold;
    MemoryEvalReport {
        metrics,
        query_results: results,
        threshold,
        passed,
    }
}

/// Formats a human-readable diagnostic listing the missed questions.
#[must_use]
pub fn format_diagnostic(report: &MemoryEvalReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "FAIL: top-3 recall {:.1}% is below threshold {:.1}%",
        report.metrics.top3_recall * 100.0,
        report.threshold * 100.0,
    ));
    lines.push("Missed question IDs and observed top results:".to_owned());
    for result in &report.query_results {
        if result.top3_hit {
            continue;
        }
        lines.push(format!("  [{}] {}", result.query_id, result.query_text));
        if result.top_k_record_ids.is_empty() {
            lines.push("    (no memory recalled)".to_owned());
        } else {
            for id in result.top_k_record_ids.iter().take(3) {
                lines.push(format!("    {id}"));
            }
        }
    }
    lines.join("\n")
}

/// Formats the full evaluation report to a writer.
///
/// # Errors
/// Returns an error if writing fails.
pub fn print_report<W: std::io::Write>(
    report: &MemoryEvalReport,
    mut out: W,
) -> anyhow::Result<()> {
    writeln!(
        out,
        "Agent-memory recall evaluation — {} questions",
        report.metrics.question_count
    )?;
    writeln!(
        out,
        "  top-1 accuracy:    {:.1}%",
        report.metrics.top1_accuracy * 100.0
    )?;
    writeln!(
        out,
        "  top-3 recall:      {:.1}%",
        report.metrics.top3_recall * 100.0
    )?;
    writeln!(
        out,
        "  mean recip. rank:  {:.4}",
        report.metrics.mean_reciprocal_rank
    )?;
    writeln!(
        out,
        "  threshold:         {:.1}%  status: {}",
        report.threshold * 100.0,
        if report.passed { "PASS" } else { "FAIL" }
    )?;
    writeln!(out)?;
    writeln!(out, "Per-question results:")?;
    for r in &report.query_results {
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
    }
    Ok(())
}
