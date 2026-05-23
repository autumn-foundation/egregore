#![allow(missing_docs)]

use aletheia_egregore::{
    EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis, SemanticDriftMetadata,
    TemporalMetadata,
    query::{largest_semantic_drifts, symbol_at_commit},
};

#[test]
fn symbol_at_commit_returns_the_temporal_symbol_record() {
    let first = GraphRecord::node(
        "symbol:answer:first".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "Rust function answer returns the old parser path".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let second = GraphRecord::node(
        "symbol:answer:second".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "Rust function answer returns the semantic history path".to_owned(),
    )
    .with_temporal(temporal("bbbbbbbb", "2026-01-02T00:00:00Z"));

    let records = [first, second];
    let found = symbol_at_commit(&records, "answer", "bbbbbbbb")
        .expect("symbol should be present at commit");

    assert_eq!(found.id(), "symbol:answer:second");
}

#[test]
fn largest_semantic_drifts_rank_drift_nodes_by_score() {
    let small = drift("drift:small", "answer", "aaaaaaaa", "bbbbbbbb", 0.25);
    let large = drift("drift:large", "answer", "bbbbbbbb", "cccccccc", 0.9);
    let edge = GraphRecord::edge(
        aletheia_egregore::EdgeLabel::DriftsFrom,
        "drift:large".to_owned(),
        "symbol:answer".to_owned(),
        Some("1.0".to_owned()),
        "drift edge".to_owned(),
    );

    let records = [small, edge, large];
    let ranked = largest_semantic_drifts(&records, 1);

    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].id(), "drift:large");
}

fn drift(id: &str, name: &str, before: &str, after: &str, score: f64) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::SemanticDrift,
        Some("src/lib.rs".to_owned()),
        None,
        Some(name.to_owned()),
        format!("Semantic drift for {name}"),
    )
    .with_temporal(temporal(after, "2026-01-02T00:00:00Z"))
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "fake-code-model".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: format!("symbol:{name}"),
        prior_record_id: format!("symbol:{name}"),
        before_git_commit: before.to_owned(),
        after_git_commit: after.to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    })
}

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: None,
        observed_at: valid_time.to_owned(),
        valid_time_source: None,
    }
}

const fn span(start_line: usize, end_line: usize) -> aletheia_egregore::SourceSpan {
    aletheia_egregore::SourceSpan {
        start_byte: 0,
        end_byte: 10,
        start_line,
        end_line,
    }
}
