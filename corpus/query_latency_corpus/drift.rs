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

/// Batch form of [`resolve_drift_target`] for rendering an entire
/// `drift_history`/`semantic_drift` section in one pass.
///
/// `resolve_drift_target` performs up to three linear scans of `records` per
/// call (the `DriftsFrom` edge lookup, the target's temporal-version check,
/// and the version-matching scan). Calling it once per row — as `eg query
/// context`, the daemon's `observations_for_symbol`, and the MCP
/// `symbol_context` tool all do when rendering a symbol's full drift history —
/// makes rendering O(D×N) for D drift rows over N total records; a long-lived
/// symbol's drift history over a large `scan-history` store can make that
/// non-trivial (issue #497 Codex review). This builds the `DriftsFrom`
/// source→target lookup and a by-id record index once (O(N)), then resolves
/// each row against those indexes, so N is scanned once regardless of D.
///
/// Returns one resolved `(path, name, span)` triple per entry in
/// `drift_records`, in the same order — positional, not keyed by record ID,
/// since two entries may share one physical drift's ID across temporal
/// versions (issue #421). Non-drift records resolve to `(None, None, None)`
/// (defensive; `drift_records` is expected to contain only `SemanticDrift`
/// nodes by construction, mirroring `resolve_drift_target`'s callers).
#[must_use]
pub fn resolve_drift_targets<'a>(
    records: &'a [GraphRecord],
    drift_records: &[&'a GraphRecord],
) -> Vec<(
    Option<&'a str>,
    Option<&'a str>,
    Option<crate::ir::SourceSpan>,
)> {
    let mut drifts_from_target: std::collections::BTreeMap<&str, &str> =
        std::collections::BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::DriftsFrom,
            source,
            target,
            ..
        } = r
        {
            drifts_from_target
                .entry(source.as_str())
                .or_insert(target.as_str());
        }
    }

    let mut by_id: std::collections::BTreeMap<&str, Vec<&'a GraphRecord>> =
        std::collections::BTreeMap::new();
    let mut has_temporal_ids: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for r in records {
        by_id.entry(r.id()).or_default().push(r);
        if matches!(
            r,
            GraphRecord::Node {
                temporal: Some(_),
                ..
            }
        ) {
            has_temporal_ids.insert(r.id());
        }
    }

    drift_records
        .iter()
        .map(|record| {
            let GraphRecord::Node {
                id,
                semantic_drift: Some(drift),
                repo_relative_path: drift_path,
                name: drift_name,
                ..
            } = record
            else {
                return (None, None, None);
            };
            let target_id = drifts_from_target
                .get(id.as_str())
                .copied()
                .unwrap_or(drift.target_record_id.as_str());
            let has_temporal = has_temporal_ids.contains(target_id);

            // Mirrors `resolve_drift_target`'s `rfind` exactly: find the LAST
            // record (of any variant) matching `target_id` whose predicate
            // holds, THEN attempt to destructure it as a Node — rather than
            // skipping past a matching non-Node record (e.g. an Edge sharing
            // `target_id` by coincidence) to keep searching earlier ones.
            // `rfind` stops at that last match and fails the outer `Node`
            // destructure, falling back to the drift's own handle; a
            // `find_map` that returns `None` for a non-Node instead continues
            // to an earlier Node, silently diverging from the per-row
            // resolver (issue #497 Codex review).
            let resolved = by_id.get(target_id).and_then(|versions| {
                versions
                    .iter()
                    .rev()
                    .find(|r| match r {
                        GraphRecord::Node {
                            temporal: Some(t), ..
                        } => t.git_commit == drift.after_git_commit,
                        _ => !has_temporal,
                    })
                    .and_then(|r| {
                        if let GraphRecord::Node {
                            repo_relative_path,
                            name,
                            span,
                            ..
                        } = r
                        {
                            Some((repo_relative_path.as_deref(), name.as_deref(), *span))
                        } else {
                            None
                        }
                    })
            });
            resolved.unwrap_or((drift_path.as_deref(), drift_name.as_deref(), None))
        })
        .collect()
}

// ── Repository scope (issue #67) ──────────────────────────────────────────────
