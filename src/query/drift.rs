use std::cmp::Ordering;

use super::{drift_score, semantic_drift};
use crate::ir::{EdgeLabel, GraphRecord, SemanticDriftMetadata};

/// Returns semantic drift nodes ranked by score descending.
#[must_use]
pub fn largest_semantic_drifts(records: &[GraphRecord], limit: usize) -> Vec<&GraphRecord> {
    let mut drifts = records
        .iter()
        .filter_map(|record| semantic_drift(record).map(|drift| (record, drift_score(drift))))
        .collect::<Vec<_>>();
    drifts.sort_by(|(left_record, left_score), (right_record, right_score)| {
        right_score
            .partial_cmp(left_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left_record.id().cmp(right_record.id()))
    });
    drifts
        .into_iter()
        .take(limit)
        .map(|(record, _)| record)
        .collect()
}

/// Resolves the target record ID of a drift entry: a `DriftsFrom` edge whose
/// source is `drift_id` takes precedence over the drift's own recorded
/// `target_record_id` (which can go stale when the target is re-identified;
/// see `query_drift_resolves_target_via_drifts_from_edge_when_target_record_id_is_stale`).
#[must_use]
pub(super) fn drift_target_record_id<'a>(
    records: &'a [GraphRecord],
    drift_id: &str,
    drift: &'a SemanticDriftMetadata,
) -> &'a str {
    records
        .iter()
        .find_map(|r| {
            let GraphRecord::Edge {
                label: EdgeLabel::DriftsFrom,
                source,
                target,
                ..
            } = r
            else {
                return None;
            };
            if source == drift_id {
                Some(target.as_str())
            } else {
                None
            }
        })
        .unwrap_or(drift.target_record_id.as_str())
}

/// Resolves the repo relative path, name, and span of a drift target.
#[must_use]
pub fn resolve_drift_target<'a>(
    records: &'a [GraphRecord],
    drift_id: &str,
    drift: &'a SemanticDriftMetadata,
    drift_path: Option<&'a str>,
    drift_name: Option<&'a str>,
) -> (
    Option<&'a str>,
    Option<&'a str>,
    Option<crate::ir::SourceSpan>,
) {
    let target_id = drift_target_record_id(records, drift_id, drift);

    let has_temporal = records.iter().any(|r| {
        r.id() == target_id
            && matches!(
                r,
                GraphRecord::Node {
                    temporal: Some(_),
                    ..
                }
            )
    });

    if let Some(GraphRecord::Node {
        repo_relative_path,
        name,
        span,
        ..
    }) = records.iter().rfind(|r| {
        if r.id() != target_id {
            return false;
        }
        if let GraphRecord::Node {
            temporal: Some(t), ..
        } = r
        {
            t.git_commit == drift.after_git_commit
        } else {
            !has_temporal
        }
    }) {
        return (repo_relative_path.as_deref(), name.as_deref(), *span);
    }
    (drift_path, drift_name, None)
}

// ── Repository scope (issue #67) ──────────────────────────────────────────────
