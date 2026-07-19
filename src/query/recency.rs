use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use super::{CommitOrder, RepositoryIndex};
use crate::ir::{GraphRecord, NodeKind, SourceSpan};

// ─────────────────────────────────────────────────────────────────────────────
// Symbol recency / dormancy ranking (issue #219)
// ─────────────────────────────────────────────────────────────────────────────

/// Default `--limit` for `eg query recency`.
pub const RECENCY_DEFAULT_LIMIT: usize = 50;

/// Maximum accepted `--limit` for `eg query recency`.
pub const RECENCY_MAX_LIMIT: usize = 500;

/// The newest indexed commit dormancy is measured against, for one repository
/// scope.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct RecencyAnchor {
    /// Repository record ID; `None` groups rows the store topology cannot
    /// attribute (legacy records without repository containment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Human-usable repository identity handle, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The newest indexed commit SHA (highest topological rank, SHA-ascending
    /// tie-break) — the reference point every dormancy in this scope is
    /// measured against.
    pub commit_sha: String,
    /// The anchor commit's valid time.
    pub valid_time: String,
}

/// One ranked symbol row in a dormancy ranking, most dormant first.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct RecencyRow {
    /// 1-based rank position after the documented ordering.
    pub rank: usize,
    /// Stable record ID of the `Symbol` node (ADR-0004 identity).
    pub record_id: String,
    /// The symbol's name at its last-change commit.
    pub symbol_name: String,
    /// Record schema version of the cited `Symbol` node.
    pub schema_version: u32,
    /// Repository-relative file handle at the last-change commit.
    pub repo_relative_path: String,
    /// Syntax source span at the last-change commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented reason the span is absent (e.g. module-level symbols carry
    /// no span), rather than silently omitting it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<String>,
    /// The commit SHA at which the symbol last changed (introduced or modified).
    pub last_change_commit: String,
    /// The valid time of the last-change commit.
    pub last_change_valid_time: String,
    /// Dormancy in whole seconds: anchor valid time − last-change valid time.
    pub dormancy_seconds: i64,
    /// Dormancy in whole days (`dormancy_seconds / 86_400`).
    pub dormancy_days: i64,
    /// Owning repository record ID, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Human-usable repository identity handle, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// Deterministic symbol-dormancy ranking over a `scan-history` temporal store.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct RecencyReport {
    /// Documented ranking basis: least-recent last change first.
    pub ranking_basis: &'static str,
    /// Documented stable tie-break chain for equal dormancy.
    pub tie_break: &'static str,
    /// How dormancy is defined — never wall-clock.
    pub dormancy_basis: &'static str,
    /// The limit the ranking was truncated to.
    pub limit: usize,
    /// Total ranked symbols before truncation.
    pub total_symbol_count: usize,
    /// Symbols returned after truncation.
    pub returned_symbol_count: usize,
    /// Completeness signal: whether `symbols` was truncated by `limit`.
    pub truncated: bool,
    /// The newest-indexed-commit anchor(s) dormancy was measured against, one
    /// per repository scope, sorted by repository record ID.
    pub anchors: Vec<RecencyAnchor>,
    /// Ranked rows, most dormant first.
    pub symbols: Vec<RecencyRow>,
    /// Corpus this history-analysis lane read (issue #427): `union` over a
    /// scan-history store, `single_snapshot` over a snapshot-less store. This
    /// lane analyzes full commit history by design; the disclosure never
    /// changes traversal.
    pub corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    pub corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    pub corpus_disclaimer: String,
}

/// Errors returned by the symbol recency query.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum RecencyError {
    /// The store (or the selected repository scope) holds no `Commit` nodes, so
    /// dormancy is undefined — only a current-tree scan was ingested (no
    /// history replay). Reporting recency here would falsely imply every symbol
    /// is brand-new (AC5 honesty contract).
    #[error("store has no commit history; run `eg scan-history` first")]
    NoHistory,
    /// Commits exist but no live `Symbol` node carries an attributable
    /// last-change commit in scope, so there is nothing to rank.
    #[error("no symbols with an attributable last change found in the commit history")]
    NoMatch,
}

/// Per-symbol snapshot cited for a ranked row: the recorded symbol state as of
/// its last-change commit.
struct SnapInfo<'a> {
    summary: &'a str,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    schema_version: u32,
    name: Option<&'a str>,
}

/// Ranks a repository's indexed symbols by least-recent last change — most
/// dormant first — over the commit history captured by `eg scan-history`
/// (issue #219).
///
/// The anchor a symbol's dormancy is measured against is the **newest indexed
/// commit per repository owner** (highest topological rank, SHA-ascending
/// tie-break), never wall-clock "now". This agrees with `eg query churn`'s
/// `last_commit` anchor. A symbol last changed at the anchor commit has
/// dormancy `0`.
///
/// A symbol's last change is the highest-topological-rank commit at which its
/// body differs from its parent snapshot, or at which it was introduced —
/// reusing the `eg query lifeline` (#96/#215) change-detection mechanics. Every
/// derivation is keyed on the symbol node's stable record ID (ADR-0004), never
/// by name, so same-name symbols never collapse.
///
/// Ordering is deterministic and byte-stable: `dormancy_seconds` descending
/// (most dormant first), then the last-change commit's topological rank
/// ascending, then `repo_relative_path` ascending, then `record_id` ascending.
///
/// # Errors
///
/// Returns [`RecencyError::NoHistory`] when no `Commit` node exists in scope
/// (a current-tree-only scan), and [`RecencyError::NoMatch`] when commits exist
/// but no live symbol carries an attributable last-change commit in scope.
pub fn symbol_recency(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
) -> Result<RecencyReport, RecencyError> {
    let index = RepositoryIndex::build(records);
    let is_owned =
        |id: &str| -> bool { repo_id.is_none_or(|r_id| index.owner_of(id) == Some(r_id)) };

    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    // Commit SHA → valid time, parent SHAs, and per-owner commit sets.
    let mut commit_valid_times: BTreeMap<&str, &str> = BTreeMap::new();
    let mut parent_map: BTreeMap<&str, &[String]> = BTreeMap::new();
    let mut commits_by_owner: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
    // Symbol record ID → its per-commit snapshots (keyed by commit SHA).
    let mut symbol_snaps: BTreeMap<&str, BTreeMap<&str, SnapInfo>> = BTreeMap::new();

    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            name,
            repo_relative_path,
            span,
            schema_version,
            summary,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if tombstoned.contains(id.as_str()) || !is_owned(id) {
            continue;
        }
        match kind {
            NodeKind::Commit => {
                let Some(t) = temporal.as_ref() else {
                    continue;
                };
                let sha = t.git_commit.as_str();
                commit_valid_times.insert(sha, t.valid_time.as_str());
                parent_map.insert(sha, t.git_parent_commits.as_slice());
                commits_by_owner
                    .entry(index.owner_of(id))
                    .or_default()
                    .insert(sha);
            }
            NodeKind::Symbol => {
                let Some(t) = temporal.as_ref() else {
                    // Current-tree Symbol nodes carry no commit provenance and
                    // cannot be dated against history; they are ignored here.
                    continue;
                };
                symbol_snaps.entry(id.as_str()).or_default().insert(
                    t.git_commit.as_str(),
                    SnapInfo {
                        summary: summary.as_str(),
                        repo_relative_path: repo_relative_path.as_deref(),
                        span: *span,
                        schema_version: *schema_version,
                        name: name.as_deref(),
                    },
                );
            }
            _ => {}
        }
    }

    if commit_valid_times.is_empty() {
        return Err(RecencyError::NoHistory);
    }

    let commit_order = CommitOrder::build(records);

    // Newest indexed commit per owner: highest topological rank, SHA ascending
    // as the stable tie-break (agrees with churn's `last_commit`).
    let mut anchor_by_owner: BTreeMap<Option<&str>, &str> = BTreeMap::new();
    let mut anchors: Vec<RecencyAnchor> = Vec::new();
    for (owner, shas) in &commits_by_owner {
        let mut ordered: Vec<&str> = shas.iter().copied().collect();
        ordered.sort_by(|a, b| {
            commit_order
                .rank(a)
                .cmp(&commit_order.rank(b))
                .then_with(|| a.cmp(b))
        });
        let Some(&anchor_sha) = ordered.last() else {
            continue;
        };
        anchor_by_owner.insert(*owner, anchor_sha);
        anchors.push(RecencyAnchor {
            repository_id: owner.map(ToOwned::to_owned),
            repository: owner
                .and_then(|o| index.display_of(o))
                .map(ToOwned::to_owned),
            commit_sha: anchor_sha.to_owned(),
            valid_time: commit_valid_times
                .get(anchor_sha)
                .map_or_else(String::new, |vt| (*vt).to_owned()),
        });
    }

    // Did the symbol's body change at `commit` (differs from every present
    // parent snapshot), or was it introduced (no parent snapshot)? Mirrors the
    // lifeline `symbol_body_changed` helper.
    let body_changed = |commit: &str, body: &str, bodies: &BTreeMap<&str, &str>| -> bool {
        let Some(parents) = parent_map.get(commit) else {
            return true;
        };
        if parents.is_empty() {
            return true;
        }
        let mut saw_parent_snapshot = false;
        for parent in *parents {
            if let Some(parent_body) = bodies.get(parent.as_str()) {
                saw_parent_snapshot = true;
                if *parent_body != body {
                    return true;
                }
            }
        }
        !saw_parent_snapshot
    };

    let mut rows: Vec<RecencyRow> = Vec::new();
    for (sym_id, snaps) in &symbol_snaps {
        let owner = index.owner_of(sym_id);
        let Some(&anchor_sha) = anchor_by_owner.get(&owner) else {
            // The symbol's owner has no indexed commit to anchor against.
            continue;
        };
        let Some(anchor_vt) = commit_valid_times
            .get(anchor_sha)
            .and_then(|vt| DateTime::parse_from_rfc3339(vt).ok())
        else {
            continue;
        };

        let bodies: BTreeMap<&str, &str> = snaps
            .iter()
            .map(|(sha, snap)| (*sha, snap.summary))
            .collect();

        // Change commits, ordered by topological rank then SHA ascending; the
        // last-change commit is the highest-rank change.
        let mut change_commits: Vec<&str> = snaps
            .iter()
            .filter(|(sha, snap)| body_changed(sha, snap.summary, &bodies))
            .map(|(sha, _)| *sha)
            .collect();
        change_commits.sort_by(|a, b| {
            commit_order
                .rank(a)
                .cmp(&commit_order.rank(b))
                .then_with(|| a.cmp(b))
        });
        let Some(&last_change_commit) = change_commits.last() else {
            continue;
        };

        let Some(snap) = snaps.get(last_change_commit) else {
            continue;
        };
        let Some(path) = snap.repo_relative_path else {
            // A symbol without a repo-relative path cannot be cited; skip
            // rather than emit an uncitable row.
            continue;
        };
        let Some(last_vt_str) = commit_valid_times.get(last_change_commit) else {
            continue;
        };
        let Some(last_vt) = DateTime::parse_from_rfc3339(last_vt_str).ok() else {
            continue;
        };

        let dormancy_seconds = (anchor_vt - last_vt).num_seconds();
        let dormancy_days = dormancy_seconds / 86_400;

        let absent_span_reason = if snap.span.is_none() {
            Some("no_span_module_level".to_owned())
        } else {
            None
        };

        rows.push(RecencyRow {
            rank: 0,
            record_id: (*sym_id).to_owned(),
            symbol_name: snap.name.unwrap_or_default().to_owned(),
            schema_version: snap.schema_version,
            repo_relative_path: path.to_owned(),
            span: snap.span,
            absent_span_reason,
            last_change_commit: last_change_commit.to_owned(),
            last_change_valid_time: (*last_vt_str).to_owned(),
            dormancy_seconds,
            dormancy_days,
            repository_id: owner.map(ToOwned::to_owned),
            repository: owner
                .and_then(|o| index.display_of(o))
                .map(ToOwned::to_owned),
        });
    }

    if rows.is_empty() {
        return Err(RecencyError::NoMatch);
    }

    rows.sort_by(|a, b| {
        b.dormancy_seconds
            .cmp(&a.dormancy_seconds)
            .then_with(|| {
                commit_order
                    .rank(&a.last_change_commit)
                    .cmp(&commit_order.rank(&b.last_change_commit))
            })
            .then_with(|| a.repo_relative_path.cmp(&b.repo_relative_path))
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    for (position, row) in rows.iter_mut().enumerate() {
        row.rank = position + 1;
    }

    let total_symbol_count = rows.len();
    rows.truncate(limit);

    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        super::disclose_corpus(records, super::CorpusMode::Union);

    Ok(RecencyReport {
        ranking_basis: "least_recent_last_change",
        tie_break: "dormancy_seconds_desc,last_change_commit_rank_asc,repo_relative_path_asc,record_id_asc",
        dormancy_basis: "measured against the newest indexed commit per repository, never wall-clock",
        limit,
        total_symbol_count,
        returned_symbol_count: rows.len(),
        truncated: total_symbol_count > rows.len(),
        anchors,
        symbols: rows,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    })
}
