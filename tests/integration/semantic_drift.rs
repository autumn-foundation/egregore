#![allow(missing_docs)]

use std::collections::BTreeMap;

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis, TemporalMetadata,
    embeddings::{CandidateVector, EmbeddingCandidate, semantic_drift_records},
    ir::{SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE, SEMANTIC_SCHEMA_VERSION},
};

#[test]
fn semantic_drift_records_emit_structured_contract_and_prior_edge() {
    let before = vector(
        "codegraph:v4:prior-symbol",
        "answer",
        "aaaaaaaa",
        "2026-01-01T00:00:00Z",
        vec![1.0, 0.0],
    );
    let after = vector(
        "codegraph:v4:target-symbol",
        "answer",
        "bbbbbbbb",
        "2026-01-02T00:00:00Z",
        vec![0.0, 1.0],
    );

    let records = semantic_drift_records(
        &[before, after],
        "sentence-transformers/all-MiniLM-L6-v2",
        0.4,
    );

    let drift_node = records
        .iter()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::SemanticDrift,
                    ..
                }
            )
        })
        .expect("drift node should be emitted");

    let GraphRecord::Node {
        id,
        schema_version,
        domain,
        semantic_drift: Some(drift),
        ..
    } = drift_node
    else {
        panic!("drift record should be a semantic drift node");
    };

    assert!(id.starts_with("semantic:v1:"));
    assert_eq!(*schema_version, SEMANTIC_SCHEMA_VERSION);
    assert_eq!(domain.as_deref(), Some("semantic"));
    assert_eq!(
        drift.embedding_model,
        EmbeddingModel {
            provider: "aletheiadb_re_export".to_owned(),
            name: "sentence-transformers/all-MiniLM-L6-v2".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            dim: 384,
            content_hash: "unknown".to_owned(),
        }
    );
    assert_eq!(drift.target_record_id, "codegraph:v4:target-symbol");
    assert_eq!(drift.prior_record_id, "codegraph:v4:prior-symbol");
    assert_eq!(drift.metric_kind, MetricKind::CosineDistance);
    assert!((drift.score - 1.0).abs() <= SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE);
    assert!((drift.selection_threshold - 0.4).abs() <= SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE);
    assert_eq!(drift.selection_basis, SelectionBasis::ThresholdOnly);

    let edges = records
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Edge {
                label,
                source,
                target,
                schema_version,
                ..
            } if source == id => Some((*label, target.as_str(), *schema_version)),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(edges.contains(&(
        EdgeLabel::DriftsFrom,
        "codegraph:v4:target-symbol",
        SEMANTIC_SCHEMA_VERSION
    )));
    assert!(edges.contains(&(
        EdgeLabel::DriftsPrior,
        "codegraph:v4:prior-symbol",
        SEMANTIC_SCHEMA_VERSION
    )));
}

#[test]
fn semantic_drift_id_changes_when_threshold_changes() {
    let before = vector(
        "codegraph:v4:prior-symbol",
        "answer",
        "aaaaaaaa",
        "2026-01-01T00:00:00Z",
        vec![1.0, 0.0],
    );
    let after = vector(
        "codegraph:v4:target-symbol",
        "answer",
        "bbbbbbbb",
        "2026-01-02T00:00:00Z",
        vec![0.0, 1.0],
    );

    let loose = semantic_drift_records(&[before.clone(), after.clone()], "model-a", 0.2);
    let tight = semantic_drift_records(&[before, after], "model-a", 0.4);

    let loose_id = drift_id(&loose);
    let tight_id = drift_id(&tight);
    assert_ne!(
        loose_id, tight_id,
        "selection_threshold is part of semantic drift identity"
    );
}

#[test]
fn semantic_drift_replay_is_deterministic_within_tolerance() {
    let vectors = vec![
        vector(
            "codegraph:v4:prior-symbol",
            "answer",
            "aaaaaaaa",
            "2026-01-01T00:00:00Z",
            vec![1.0, 0.0],
        ),
        vector(
            "codegraph:v4:target-symbol",
            "answer",
            "bbbbbbbb",
            "2026-01-02T00:00:00Z",
            vec![0.0, 1.0],
        ),
    ];

    let first_records = semantic_drift_records(&vectors, "model-a", 0.4);
    let second_records = semantic_drift_records(&vectors, "model-a", 0.4);
    let first = drift_scores_by_id(&first_records);
    let second = drift_scores_by_id(&second_records);

    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        second.keys().collect::<Vec<_>>()
    );
    for (id, first_score) in first {
        let second_score = second.get(id).expect("same drift ID should replay");
        assert!(
            (first_score - second_score).abs() <= SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE,
            "score for {id} drifted by more than documented tolerance"
        );
    }
}

fn drift_id(records: &[GraphRecord]) -> &str {
    records
        .iter()
        .find_map(|record| {
            if let GraphRecord::Node {
                kind: NodeKind::SemanticDrift,
                id,
                ..
            } = record
            {
                Some(id.as_str())
            } else {
                None
            }
        })
        .expect("drift node should exist")
}

fn drift_scores_by_id(records: &[GraphRecord]) -> BTreeMap<&str, f64> {
    records
        .iter()
        .filter_map(|record| {
            let GraphRecord::Node {
                id,
                semantic_drift: Some(drift),
                ..
            } = record
            else {
                return None;
            };
            Some((id.as_str(), drift.score))
        })
        .collect()
}

fn vector(
    record_id: &str,
    name: &str,
    commit: &str,
    valid_time: &str,
    values: Vec<f32>,
) -> CandidateVector {
    CandidateVector {
        candidate: EmbeddingCandidate {
            record_id: record_id.to_owned(),
            target: "symbol".to_owned(),
            text: format!("symbol {name}"),
            repo_relative_path: Some("src/lib.rs".to_owned()),
            name: Some(name.to_owned()),
            temporal: Some(temporal(commit, valid_time)),
        },
        vector: values,
    }
}

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_resolve_drift_target_temporal_fallback() {
    use aletheia_egregore::ir::{SEMANTIC_SCHEMA_VERSION, SemanticDriftMetadata, SourceSpan};
    use aletheia_egregore::query::resolve_drift_target;

    let target_id = "codegraph:v4:my-symbol";
    let drift_id = "semantic:v1:drift-node";

    let drift = SemanticDriftMetadata {
        before_git_commit: "commit_a".to_owned(),
        after_git_commit: "commit_b".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        embedding_model: EmbeddingModel {
            provider: "provider".to_owned(),
            name: "model".to_owned(),
            version: "version".to_owned(),
            dim: 128,
            content_hash: "hash".to_owned(),
        },
        metric_kind: MetricKind::CosineDistance,
        prior_record_id: "codegraph:v4:prior-symbol".to_owned(),
        target_record_id: target_id.to_owned(),
        score: 0.5,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    };

    let drift_node = GraphRecord::node(
        drift_id.to_owned(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "Drift summary".to_owned(),
    )
    .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
    .with_semantic_drift(drift.clone());

    let edge = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id.to_owned(),
        target_id.to_owned(),
        None,
        "Drifts From".to_owned(),
    );

    // Node 1: Temporal node matching after_git_commit ("commit_b")
    let node_matching = GraphRecord::node(
        target_id.to_owned(),
        NodeKind::Symbol,
        Some("src/matching.rs".to_owned()),
        Some(SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: 10,
            end_line: 20,
        }),
        Some("my_symbol".to_owned()),
        "Summary".to_owned(),
    )
    .with_temporal(temporal("commit_b", "2026-01-02T00:00:00Z"));

    // Node 2: Temporal node with different commit ("commit_other")
    let node_other_commit = GraphRecord::node(
        target_id.to_owned(),
        NodeKind::Symbol,
        Some("src/other_commit.rs".to_owned()),
        Some(SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: 30,
            end_line: 40,
        }),
        Some("my_symbol".to_owned()),
        "Summary".to_owned(),
    )
    .with_temporal(temporal("commit_other", "2026-01-03T00:00:00Z"));

    // Node 3: Non-temporal node representing the current check
    let node_nontemporal = GraphRecord::node(
        target_id.to_owned(),
        NodeKind::Symbol,
        Some("src/nontemporal.rs".to_owned()),
        Some(SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: 50,
            end_line: 60,
        }),
        Some("my_symbol".to_owned()),
        "Summary".to_owned(),
    );

    // Case 1: All records present. Temporal node with matching commit exists.
    // It should select the matching temporal node (Node 1).
    let records = vec![
        drift_node.clone(),
        edge.clone(),
        node_matching,
        node_other_commit.clone(),
        node_nontemporal.clone(),
    ];
    let (path, _name, span) = resolve_drift_target(&records, drift_id, &drift, None, None);
    assert_eq!(path, Some("src/matching.rs"));
    assert_eq!(span.unwrap().start_line, 10);

    // Case 2: Matching temporal node is absent, but another temporal node exists (node_other_commit).
    // It should NOT match the non-temporal node (Node 3) since has_temporal is true.
    // It should return the fallback (drift_path, drift_name, None).
    let records2 = vec![
        drift_node.clone(),
        edge.clone(),
        node_other_commit,
        node_nontemporal.clone(),
    ];
    let (path2, name2, span2) = resolve_drift_target(
        &records2,
        drift_id,
        &drift,
        Some("src/fallback.rs"),
        Some("fallback_name"),
    );
    assert_eq!(path2, Some("src/fallback.rs"));
    assert_eq!(name2, Some("fallback_name"));
    assert!(span2.is_none());

    // Case 3: No temporal nodes at all exist. Only non-temporal node is present.
    // It should fall back to matching the non-temporal node (Node 3).
    let records3 = vec![drift_node, edge, node_nontemporal];
    let (path3, _name3, span3) = resolve_drift_target(&records3, drift_id, &drift, None, None);
    assert_eq!(path3, Some("src/nontemporal.rs"));
    assert_eq!(span3.unwrap().start_line, 50);
}

fn versioned_symbol(
    id: &str,
    path: &str,
    line: usize,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    use aletheia_egregore::ir::SourceSpan;
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: line,
            end_line: line + 10,
        }),
        Some("symbol".to_owned()),
        "Summary".to_owned(),
    )
    .with_temporal(temporal(commit, valid_time))
}

fn drift_targeting(drift_id: &str, target_id: &str, after_commit: &str) -> GraphRecord {
    use aletheia_egregore::ir::{SEMANTIC_SCHEMA_VERSION, SemanticDriftMetadata};
    let drift = SemanticDriftMetadata {
        before_git_commit: "commit_before".to_owned(),
        after_git_commit: after_commit.to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        embedding_model: EmbeddingModel {
            provider: "provider".to_owned(),
            name: "model".to_owned(),
            version: "version".to_owned(),
            dim: 128,
            content_hash: "hash".to_owned(),
        },
        metric_kind: MetricKind::CosineDistance,
        prior_record_id: target_id.to_owned(),
        target_record_id: target_id.to_owned(),
        score: 0.5,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    };
    GraphRecord::node(
        drift_id.to_owned(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "Drift summary".to_owned(),
    )
    .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
    .with_semantic_drift(drift)
}

/// Issue #497 Codex review: rendering a symbol's full `drift_history` called
/// `resolve_drift_target` once per row, each doing its own O(N) scan of
/// `records` — O(D×N) for D drift rows. `resolve_drift_targets` batches this
/// into one O(N) pass. It must return EXACTLY what looping
/// `resolve_drift_target` per row would — including correctly distinguishing
/// two unrelated drift records that resolve to two different targets, so the
/// shared index built once cannot cross-contaminate rows.
#[test]
fn resolve_drift_targets_batch_matches_per_row_resolution() {
    use aletheia_egregore::query::{resolve_drift_target, resolve_drift_targets};

    // Two unrelated symbols, each with two temporal versions.
    let first_symbol_id = "codegraph:v4:first-symbol";
    let second_symbol_id = "codegraph:v4:second-symbol";
    let first_old = versioned_symbol(
        first_symbol_id,
        "src/first_old.rs",
        1,
        "commit_1a",
        "2026-01-01T00:00:00Z",
    );
    let first_new = versioned_symbol(
        first_symbol_id,
        "src/first_new.rs",
        2,
        "commit_1b",
        "2026-01-02T00:00:00Z",
    );
    let second_old = versioned_symbol(
        second_symbol_id,
        "src/second_old.rs",
        3,
        "commit_2a",
        "2026-01-01T00:00:00Z",
    );
    let second_new = versioned_symbol(
        second_symbol_id,
        "src/second_new.rs",
        4,
        "commit_2b",
        "2026-01-02T00:00:00Z",
    );

    // Two drift records: one targets the first symbol's old snapshot, the
    // other targets the second symbol's new snapshot.
    let first_drift_id = "semantic:v1:first-drift";
    let second_drift_id = "semantic:v1:second-drift";
    let first_drift_node = drift_targeting(first_drift_id, first_symbol_id, "commit_1a");
    let second_drift_node = drift_targeting(second_drift_id, second_symbol_id, "commit_2b");
    let GraphRecord::Node {
        semantic_drift: Some(first_drift),
        ..
    } = &first_drift_node
    else {
        unreachable!("drift_targeting always builds a SemanticDrift node")
    };
    let GraphRecord::Node {
        semantic_drift: Some(second_drift),
        ..
    } = &second_drift_node
    else {
        unreachable!("drift_targeting always builds a SemanticDrift node")
    };

    let records = vec![
        first_old,
        first_new,
        second_old,
        second_new,
        first_drift_node.clone(),
        second_drift_node.clone(),
    ];

    let expected_first = resolve_drift_target(&records, first_drift_id, first_drift, None, None);
    let expected_second = resolve_drift_target(&records, second_drift_id, second_drift, None, None);

    let batched = resolve_drift_targets(&records, &[&first_drift_node, &second_drift_node]);
    assert_eq!(
        batched,
        vec![expected_first, expected_second],
        "batched resolution must match per-row resolve_drift_target exactly, \
         with no cross-contamination between unrelated drift rows"
    );
    // Sanity: the two rows really do resolve to their OWN distinct targets.
    assert_eq!(batched[0].0, Some("src/first_old.rs"));
    assert_eq!(batched[1].0, Some("src/second_new.rs"));
}
