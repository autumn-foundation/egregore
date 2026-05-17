#![allow(missing_docs)]

use std::path::PathBuf;

use aletheia_egregore::{
    GraphRecord, NodeKind, TemporalMetadata,
    embeddings::{CandidateVector, embedding_candidates, semantic_drift_records},
    scan_repository,
};
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

#[test]
fn embedding_candidates_target_agent_useful_handles() {
    let graph = scan_repository(fixture_repo()).expect("fixture repo should scan");
    let candidates = embedding_candidates(graph.records());

    assert!(
        candidates.iter().any(|candidate| {
            candidate.target == "file"
                && candidate.repo_relative_path.as_deref() == Some("src/lib.rs")
                && candidate.text.contains("Rust source file src/lib.rs")
        }),
        "file summary should be embeddable"
    );
    assert!(
        candidates.iter().any(|candidate| {
            candidate.target == "symbol"
                && candidate.name.as_deref() == Some("nested::Widget::new")
                && candidate.repo_relative_path.as_deref() == Some("src/lib.rs")
                && candidate.text.contains("Rust method nested::Widget::new")
        }),
        "symbol summary should be embeddable"
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.target == "file" || candidate.target == "symbol"),
        "imports and diagnostics should not be first-class embedding targets"
    );

    let ids = candidates
        .iter()
        .map(|candidate| candidate.record_id.as_str())
        .collect::<Vec<_>>();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "embedding candidate ordering should be stable");
}

#[test]
fn semantic_drift_records_capture_vector_movement_between_commits() {
    let before = GraphRecord::node(
        "file:src/lib.rs".to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "Rust source file src/lib.rs focused on parser setup".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let after = GraphRecord::node(
        "file:src/lib.rs".to_owned(),
        NodeKind::File,
        Some("src/lib.rs".to_owned()),
        None,
        Some("src/lib.rs".to_owned()),
        "Rust source file src/lib.rs focused on temporal semantic search".to_owned(),
    )
    .with_temporal(temporal("bbbbbbbb", "2026-01-02T00:00:00Z"));
    let candidates = embedding_candidates(&[before, after]);
    let vectors = candidates
        .into_iter()
        .map(|candidate| {
            let vector = if candidate
                .temporal
                .as_ref()
                .is_some_and(|temporal| temporal.git_commit == "aaaaaaaa")
            {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            };
            CandidateVector { candidate, vector }
        })
        .collect::<Vec<_>>();

    let records = semantic_drift_records(&vectors, "fake-code-model", 0.5);
    let json = records
        .iter()
        .map(|record| serde_json::to_value(record).expect("record should serialize"))
        .collect::<Vec<Value>>();

    assert!(
        json.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "SemanticDrift"
                && record["semantic_drift"]["model_id"] == "fake-code-model"
                && record["semantic_drift"]["target_record_id"] == "file:src/lib.rs"
                && record["semantic_drift"]["before_git_commit"] == "aaaaaaaa"
                && record["semantic_drift"]["after_git_commit"] == "bbbbbbbb"
                && record["semantic_drift"]["score"] == "1.000000"
        }),
        "missing semantic drift node"
    );
    assert!(
        json.iter().any(|record| {
            record["record_type"] == "edge"
                && record["label"] == "DRIFTS_FROM"
                && record["target"] == "file:src/lib.rs"
        }),
        "missing DRIFTS_FROM edge"
    );
}

#[test]
fn semantic_drift_records_compare_same_symbol_across_commits() {
    let before = GraphRecord::node(
        "symbol:answer:before".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("answer".to_owned()),
        "Rust function answer computes syntax handles".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let after = GraphRecord::node(
        "symbol:answer:after".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("answer".to_owned()),
        "Rust function answer computes temporal semantic handles".to_owned(),
    )
    .with_temporal(temporal("bbbbbbbb", "2026-01-02T00:00:00Z"));
    let candidates = embedding_candidates(&[before, after]);
    let vectors = candidates
        .into_iter()
        .map(|candidate| {
            let vector = if candidate
                .temporal
                .as_ref()
                .is_some_and(|temporal| temporal.git_commit == "aaaaaaaa")
            {
                vec![0.0, 1.0]
            } else {
                vec![1.0, 0.0]
            };
            CandidateVector { candidate, vector }
        })
        .collect::<Vec<_>>();

    let records = semantic_drift_records(&vectors, "fake-code-model", 0.5);
    let json = records
        .iter()
        .map(|record| serde_json::to_value(record).expect("record should serialize"))
        .collect::<Vec<Value>>();

    assert!(
        json.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "SemanticDrift"
                && record["name"] == "answer"
                && record["semantic_drift"]["target_record_id"] == "symbol:answer:after"
                && record["semantic_drift"]["before_git_commit"] == "aaaaaaaa"
                && record["semantic_drift"]["after_git_commit"] == "bbbbbbbb"
        }),
        "missing symbol semantic drift node"
    );
}

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
    }
}
