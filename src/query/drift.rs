use super::{drift_score, semantic_drift};
use crate::ir::{EdgeLabel, GraphRecord, SemanticDriftMetadata};

/// Returns semantic drift nodes ranked by score descending.
///
/// The ranking is a total order (issue #199): score descending via
/// [`f64::total_cmp`] — so NaN takes a deterministic position instead of
/// collapsing to `Equal` — then record ID ascending. No two distinct drift
/// rows can ever swap positions across runs.
#[must_use]
pub fn largest_semantic_drifts(records: &[GraphRecord], limit: usize) -> Vec<&GraphRecord> {
    let mut drifts = records
        .iter()
        .filter_map(|record| semantic_drift(record).map(|drift| (record, drift_score(drift))))
        .collect::<Vec<_>>();
    drifts.sort_by(|(left_record, left_score), (right_record, right_score)| {
        right_score
            .total_cmp(left_score)
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

#[cfg(test)]
mod ordering_tests {
    //! Ordering-contract regression tests (issue #199): drift ranking is a
    //! total order — score descending, then record ID ascending — so tied
    //! rows can never swap positions across runs.
    use super::*;
    use crate::ir::{EmbeddingModel, MetricKind, NodeKind, SelectionBasis, SemanticDriftMetadata};

    fn drift_node(id: &str, score: f64) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::SemanticDrift,
            Some("src/lib.rs".to_owned()),
            None,
            Some("drifted_fn".to_owned()),
            format!("drift {id}"),
        )
        .with_semantic_drift(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "test".to_owned(),
                name: "test-model-v1".to_owned(),
                version: "v1".to_owned(),
                dim: 2,
                content_hash: "fixture".to_owned(),
            },
            target_record_id: "target".to_owned(),
            prior_record_id: "target".to_owned(),
            before_git_commit: "aaaaaaaa".to_owned(),
            after_git_commit: "bbbbbbbb".to_owned(),
            before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score,
            selection_threshold: 0.2,
            selection_basis: SelectionBasis::ThresholdOnly,
        })
    }

    fn ranked_ids(records: &[GraphRecord]) -> Vec<String> {
        largest_semantic_drifts(records, 10)
            .iter()
            .map(|r| r.id().to_owned())
            .collect()
    }

    #[test]
    fn tied_drift_scores_break_by_record_id_ascending() {
        // Insertion order is deliberately NOT record-ID order.
        let records = vec![
            drift_node("drift/zeta", 0.5),
            drift_node("drift/alpha", 0.5),
            drift_node("drift/mike", 0.9),
        ];
        assert_eq!(
            ranked_ids(&records),
            vec!["drift/mike", "drift/alpha", "drift/zeta"],
            "tied drift scores must break by record ID ascending"
        );
    }

    #[test]
    fn nan_drift_score_sorts_to_a_deterministic_position() {
        // `partial_cmp` collapses NaN comparisons to `Equal`, which would pin
        // the NaN row to its input position — input-order-dependent, not a
        // total order. `total_cmp` ranks NaN deterministically instead.
        let records = vec![drift_node("drift/a", 0.5), drift_node("drift/n", f64::NAN)];
        assert_eq!(
            ranked_ids(&records),
            vec!["drift/n", "drift/a"],
            "a NaN drift score must sort to a deterministic position"
        );
        // And the reverse insertion order must converge to the same ranking.
        let reversed = vec![drift_node("drift/n", f64::NAN), drift_node("drift/a", 0.5)];
        assert_eq!(
            ranked_ids(&reversed),
            vec!["drift/n", "drift/a"],
            "NaN positioning must not depend on input order"
        );
    }

    #[test]
    fn drift_ranking_is_stable_across_repeated_runs() {
        let records = vec![
            drift_node("drift/zeta", 0.5),
            drift_node("drift/alpha", 0.5),
            drift_node("drift/mike", 0.9),
            drift_node("drift/beta", 0.1),
        ];
        let reference = ranked_ids(&records);
        for _ in 0..20 {
            assert_eq!(
                ranked_ids(&records),
                reference,
                "drift ranking must be byte-identical across runs"
            );
        }
    }
}
