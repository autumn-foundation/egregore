//! Deterministic per-symbol structural complexity ranking (issue #162).
//!
//! Every Rust callable `Symbol` (`function` / `method` / `test` symbol kinds)
//! carries a source-derived integer `complexity`: 1 plus one per decision
//! point in the item's own body (`if` / `else if`, `for`, `while`, `loop`,
//! each `match` arm, each `?`, each `&&`, each `||`). The score is a
//! `TrustClass::SourceDerived` code fact — no agent-authored confidence.
//!
//! [`symbol_complexity_ranking`] ranks those symbols highest-score first so
//! hotspot triage can start with the most decision-dense callables. Ordering
//! is byte-stable: complexity descending, then qualified name ascending
//! (documented tie-break), then symbol record ID ascending (deterministic
//! fallback for duplicate qualified names across repositories).

use std::collections::BTreeSet;

use super::RepositoryIndex;
use super::liveness::Liveness;
use crate::ir::{GraphRecord, NodeKind, SourceSpan};

// ─────────────────────────────────────────────────────────────────────────────
// Symbol complexity ranking (issue #162)
// ─────────────────────────────────────────────────────────────────────────────

/// Default `--limit` for `eg query complexity`.
pub const COMPLEXITY_DEFAULT_LIMIT: usize = 50;

/// Maximum accepted `--limit` for `eg query complexity`.
pub const COMPLEXITY_MAX_LIMIT: usize = 500;

/// Symbol kinds that carry a structural complexity score (issue #162).
const CALLABLE_SYMBOL_KINDS: [&str; 3] = ["function", "method", "test"];

/// One ranked symbol row in a complexity ranking.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct SymbolComplexityRow {
    /// 1-based rank position after the documented ordering.
    pub rank: usize,
    /// Qualified symbol name (the documented tie-break key).
    pub name: String,
    /// Language-specific symbol category: `function`, `method`, or `test`.
    pub symbol_kind: String,
    /// Deterministic structural complexity score (minimum 1).
    pub complexity: u32,
    /// Stable record ID of the `Symbol` node the row resolves to.
    pub symbol_record_id: String,
    /// Record schema version of the cited `Symbol` node.
    pub schema_version: u32,
    /// Repository-relative file handle.
    pub repo_relative_path: String,
    /// Source span of the symbol, when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Owning repository record ID, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Human-usable repository identity handle, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// Deterministic per-symbol structural complexity ranking.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct SymbolComplexityReport {
    /// Documented ranking basis: structural complexity score.
    pub ranking_basis: &'static str,
    /// Documented stable tie-break key for equal scores.
    pub tie_break: &'static str,
    /// The limit the ranking was truncated to.
    pub limit: usize,
    /// Total ranked symbols before truncation.
    pub total_symbol_count: usize,
    /// Symbols returned after truncation.
    pub returned_symbol_count: usize,
    /// Completeness signal: whether `symbols` was truncated by `limit`.
    pub truncated: bool,
    /// Ranked rows, highest complexity first.
    pub symbols: Vec<SymbolComplexityRow>,
    /// Corpus this lane read (issue #427): `union` over a scan-history
    /// store, `single_snapshot` over a snapshot-less store. This lane reads
    /// current-state symbol records by design; the disclosure never changes
    /// traversal.
    pub corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    pub corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    pub corpus_disclaimer: String,
}

/// Errors returned by the symbol complexity query.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum SymbolComplexityError {
    /// No callable symbol in scope carries a complexity score — the store
    /// holds no scored Rust callables (or predates issue #162).
    #[error("no callable symbols with complexity scores in scope")]
    NoMatch,
}

/// Ranks Rust callable symbols by structural complexity, highest first
/// (issue #162).
///
/// Counting is graph-native and read-only: each row cites a `Symbol` node of
/// kind `function` / `method` / `test` carrying the extractor-stamped
/// `complexity` field. Only live (non-tombstoned) symbols in the selected
/// repository scope can rank. Symbols without a score (records produced
/// before issue #162) are skipped, never fabricated.
///
/// Ordering is deterministic and byte-stable: `complexity` descending, then
/// qualified `name` ascending (documented tie-break), then
/// `symbol_record_id` ascending (deterministic fallback for duplicate
/// qualified names, e.g. the same path indexed for two repositories).
/// `limit` truncates the ranking after ordering; the report states the
/// truncation explicitly.
///
/// # Errors
///
/// Returns [`SymbolComplexityError::NoMatch`] when no scored callable symbol
/// exists in scope.
pub fn symbol_complexity_ranking(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
) -> Result<SymbolComplexityReport, SymbolComplexityError> {
    let index = RepositoryIndex::build(records);
    let is_owned =
        |id: &str| -> bool { repo_id.is_none_or(|r_id| index.owner_of(id) == Some(r_id)) };

    // Latest-write-wins liveness (issues #421/#432), same gate as the churn
    // lane: a symbol re-ingested after its own tombstone is live again.
    let liveness = Liveness::new(records);
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .filter(|&id| liveness.deleted(id))
        .collect();

    let mut rows: Vec<SymbolComplexityRow> = Vec::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            name,
            symbol_kind,
            repo_relative_path,
            span,
            schema_version,
            complexity,
            ..
        } = record
        else {
            continue;
        };
        if *kind != NodeKind::Symbol {
            continue;
        }
        if tombstoned.contains(id.as_str()) || !is_owned(id) {
            continue;
        }
        let (Some(qualified_name), Some(kind_str), Some(path), Some(score)) = (
            name.as_deref(),
            symbol_kind.as_deref(),
            repo_relative_path.as_deref(),
            *complexity,
        ) else {
            continue;
        };
        if !CALLABLE_SYMBOL_KINDS.contains(&kind_str) {
            continue;
        }
        let owner = index.owner_of(id);
        rows.push(SymbolComplexityRow {
            rank: 0,
            name: qualified_name.to_owned(),
            symbol_kind: kind_str.to_owned(),
            complexity: score,
            symbol_record_id: id.clone(),
            schema_version: *schema_version,
            repo_relative_path: path.to_owned(),
            span: *span,
            repository_id: owner.map(ToOwned::to_owned),
            repository: owner
                .and_then(|o| index.display_of(o))
                .map(ToOwned::to_owned),
        });
    }

    if rows.is_empty() {
        return Err(SymbolComplexityError::NoMatch);
    }

    rows.sort_by(|a, b| {
        b.complexity
            .cmp(&a.complexity)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.symbol_record_id.cmp(&b.symbol_record_id))
    });
    for (position, row) in rows.iter_mut().enumerate() {
        row.rank = position + 1;
    }

    let total_symbol_count = rows.len();
    rows.truncate(limit);

    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        super::disclose_corpus(records, super::CorpusMode::Union);

    Ok(SymbolComplexityReport {
        ranking_basis: "structural_complexity",
        tie_break: "qualified_name",
        limit,
        total_symbol_count,
        returned_symbol_count: rows.len(),
        truncated: total_symbol_count > rows.len(),
        symbols: rows,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    })
}
