//! Agent-facing graph query helpers.

use std::cmp::Ordering;

use chrono::DateTime;

use crate::ir::{GraphRecord, NodeKind, SemanticDriftMetadata};

/// Finds a symbol record by name at a specific Git commit.
///
/// `commit` may be a full SHA or a unique prefix from the caller's graph.
#[must_use]
pub fn symbol_at_commit<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    commit: &str,
) -> Option<&'records GraphRecord> {
    let mut matches = records
        .iter()
        .filter(|record| matches_symbol_at_commit(record, symbol_name, commit))
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id().cmp(right.id()));
    matches.into_iter().next()
}

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

fn matches_symbol_at_commit(record: &GraphRecord, symbol_name: &str, commit: &str) -> bool {
    let GraphRecord::Node {
        kind,
        name,
        temporal,
        ..
    } = record
    else {
        return false;
    };
    *kind == NodeKind::Symbol
        && name.as_deref() == Some(symbol_name)
        && temporal
            .as_ref()
            .is_some_and(|temporal| temporal.git_commit.starts_with(commit))
}

fn semantic_drift(record: &GraphRecord) -> Option<&SemanticDriftMetadata> {
    let GraphRecord::Node {
        kind,
        semantic_drift,
        ..
    } = record
    else {
        return None;
    };
    if *kind == NodeKind::SemanticDrift {
        semantic_drift.as_deref()
    } else {
        None
    }
}

fn drift_score(drift: &SemanticDriftMetadata) -> f32 {
    drift.score.parse::<f32>().unwrap_or(0.0)
}

/// Finds a symbol record by name at the most recent commit at or before `as_of`.
///
/// `as_of` must be an RFC 3339 timestamp string. Returns an error string if the
/// timestamp cannot be parsed. Returns `None` when no record exists at or before
/// the given instant.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbol_as_of_valid_time<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
) -> Result<Option<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut best: Option<(&GraphRecord, DateTime<chrono::FixedOffset>)> = None;

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        // Resolve valid_time from history temporal block (history records) or
        // node-level field (current-tree records stamped by with_valid_time_inferred).
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else {
            continue;
        };
        let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        let is_better = best.as_ref().is_none_or(|(prev_r, prev_vt)| {
            vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
        });
        if is_better {
            best = Some((record, vt));
        }
    }

    Ok(best.map(|(r, _)| r))
}
