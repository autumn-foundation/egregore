#![allow(missing_docs)]

use std::collections::BTreeMap;

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    TemporalMetadata,
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
    assert!(
        (drift.selection_threshold - 0.4).abs() <= SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE
    );
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

    assert_eq!(first.keys().collect::<Vec<_>>(), second.keys().collect::<Vec<_>>());
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
