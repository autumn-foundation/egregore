//! Unit tests for the issue #185 memory-evidence-health sweep: bucket
//! classification, drift-record selection, array-vs-edge integrity, liveness
//! edge cases, and determinism. The pure function under test is
//! [`super::run_memory_evidence_health_audit`].

use super::*;
use crate::ir::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind,
    SEMANTIC_SCHEMA_VERSION, SelectionBasis, SemanticDriftMetadata, SourceSpan,
};

fn link(target: Option<&str>, relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: target.map(str::to_owned),
        target_domain: "codegraph".to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

fn observation(id: &str, kind: NodeKind, links: Vec<EvidenceLink>) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        None,
        "memory node".to_owned(),
    )
    .with_evidence_links(links)
}

fn symbol(id: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(SourceSpan {
            start_byte: 0,
            end_byte: 42,
            start_line: 10,
            end_line: 20,
            start_column: None,
            end_column: None,
        }),
        Some("sym".to_owned()),
        "symbol".to_owned(),
    )
}

fn edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        label,
        source.to_owned(),
        target.to_owned(),
        None,
        "edge".to_owned(),
    )
}

fn tombstone(id: &str, deleted_id: &str) -> GraphRecord {
    GraphRecord::Tombstone {
        id: id.to_owned(),
        schema_version: 1,
        deleted_id: deleted_id.to_owned(),
        summary: "removed".to_owned(),
        producer: None,
    }
}

fn drift_meta(prior_id: &str, target_id: &str, after_valid_time: &str) -> SemanticDriftMetadata {
    SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "p".to_owned(),
            name: "m".to_owned(),
            version: "v".to_owned(),
            dim: 8,
            content_hash: "h".to_owned(),
        },
        target_record_id: target_id.to_owned(),
        prior_record_id: prior_id.to_owned(),
        before_git_commit: "aaa".to_owned(),
        after_git_commit: "bbb".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: after_valid_time.to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.7,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    }
}

fn drift_node(
    drift_id: &str,
    prior_id: &str,
    target_id: &str,
    after_valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        drift_id.to_owned(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "Drift".to_owned(),
    )
    .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
    .with_semantic_drift(drift_meta(prior_id, target_id, after_valid_time))
}

fn link_rows(lines: &[MemoryEvidenceHealthLine]) -> Vec<&LinkHealthRow> {
    lines
        .iter()
        .filter_map(|line| match line {
            MemoryEvidenceHealthLine::Link(row) => Some(row),
            _ => None,
        })
        .collect()
}

fn violation_rows(lines: &[MemoryEvidenceHealthLine]) -> Vec<&IntegrityViolationRow> {
    lines
        .iter()
        .filter_map(|line| match line {
            MemoryEvidenceHealthLine::IntegrityViolation(row) => Some(row),
            _ => None,
        })
        .collect()
}

fn summary(lines: &[MemoryEvidenceHealthLine]) -> &MemoryEvidenceHealthSummary {
    lines
        .iter()
        .find_map(|line| match line {
            MemoryEvidenceHealthLine::Summary(s) => Some(s),
            _ => None,
        })
        .expect("summary line is always last")
}

#[test]
fn observation_class_predicate_covers_kinds() {
    assert!(is_observation_class_kind(NodeKind::Observation));
    assert!(is_observation_class_kind(NodeKind::Decision));
    assert!(is_observation_class_kind(NodeKind::Failure));
    // Lesson/Hypothesis have no NodeKind variants yet; everything else is out.
    assert!(!is_observation_class_kind(NodeKind::ToolCall));
    assert!(!is_observation_class_kind(NodeKind::Agent));
    assert!(!is_observation_class_kind(NodeKind::Symbol));
    assert!(!is_observation_class_kind(NodeKind::SemanticDrift));
}

#[test]
fn every_link_lands_in_exactly_one_bucket() {
    let records = vec![
        symbol("codegraph:v1:live"),
        symbol("codegraph:v1:drifted"),
        symbol("codegraph:v1:dead"),
        tombstone("codegraph:v1:tomb1", "codegraph:v1:dead"),
        drift_node(
            "semantic:v1:d1",
            "codegraph:v1:drifted",
            "codegraph:v1:drifted_after",
            "2026-02-01T00:00:00Z",
        ),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![
                link(Some("codegraph:v1:live"), "OBSERVES"),
                link(Some("codegraph:v1:drifted"), "OBSERVES"),
                link(Some("codegraph:v1:absent"), "OBSERVES"),
                link(Some("codegraph:v1:dead"), "OBSERVES"),
            ],
        ),
    ];
    // Stored edges agree with the array so no integrity rows appear.
    let mut records = records;
    for target in [
        "codegraph:v1:live",
        "codegraph:v1:drifted",
        "codegraph:v1:absent",
        "codegraph:v1:dead",
    ] {
        records.push(edge(EdgeLabel::Observes, "agent_memory:v1:obs", target));
    }

    let lines = run_memory_evidence_health_audit(&records);
    let rows = link_rows(&lines);
    assert_eq!(rows.len(), 4);

    let bucket_of = |target: &str| {
        rows.iter()
            .find(|row| row.target_record_id == target)
            .expect("row exists")
            .bucket
    };
    assert_eq!(bucket_of("codegraph:v1:live"), LinkBucket::ResolvesLive);
    assert_eq!(bucket_of("codegraph:v1:drifted"), LinkBucket::Drifted);
    assert_eq!(bucket_of("codegraph:v1:absent"), LinkBucket::Dangling);
    assert_eq!(bucket_of("codegraph:v1:dead"), LinkBucket::Dangling);

    let drifted = rows
        .iter()
        .find(|row| row.bucket == LinkBucket::Drifted)
        .unwrap();
    assert_eq!(drifted.drift_record_id.as_deref(), Some("semantic:v1:d1"));

    let absent = rows
        .iter()
        .find(|row| row.target_record_id == "codegraph:v1:absent")
        .unwrap();
    assert_eq!(absent.tombstoned, Some(false));
    let dead = rows
        .iter()
        .find(|row| row.target_record_id == "codegraph:v1:dead")
        .unwrap();
    assert_eq!(dead.tombstoned, Some(true));

    // Non-dangling rows carry no drift/tombstone annotations.
    let live = rows
        .iter()
        .find(|row| row.target_record_id == "codegraph:v1:live")
        .unwrap();
    assert_eq!(live.drift_record_id, None);
    assert_eq!(live.tombstoned, None);

    let s = summary(&lines);
    assert_eq!(s.links_checked, 4);
    assert_eq!(s.resolves_live, 1);
    assert_eq!(s.drifted, 1);
    assert_eq!(s.dangling, 2);
    assert_eq!(s.integrity_violations, 0);
    assert!(!s.ok);
}

#[test]
fn drifted_does_not_fail_the_gate() {
    let records = vec![
        symbol("codegraph:v1:drifted"),
        drift_node(
            "semantic:v1:d1",
            "codegraph:v1:drifted",
            "codegraph:v1:drifted_after",
            "2026-02-01T00:00:00Z",
        ),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:drifted"), "OBSERVES")],
        ),
        edge(
            EdgeLabel::Observes,
            "agent_memory:v1:obs",
            "codegraph:v1:drifted",
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    let s = summary(&lines);
    assert_eq!(s.drifted, 1);
    assert!(s.ok, "drifted is a freshness lead, not rot");
}

#[test]
fn earliest_drift_record_wins() {
    let records = vec![
        symbol("codegraph:v1:drifted"),
        drift_node(
            "semantic:v1:d_late",
            "codegraph:v1:drifted",
            "codegraph:v1:after",
            "2026-03-01T00:00:00Z",
        ),
        drift_node(
            "semantic:v1:d_early",
            "codegraph:v1:drifted",
            "codegraph:v1:after",
            "2026-02-01T00:00:00Z",
        ),
        // Same earliest instant, larger id loses the tie-break.
        drift_node(
            "semantic:v1:d_early_b",
            "codegraph:v1:drifted",
            "codegraph:v1:after",
            "2026-02-01T00:00:00Z",
        ),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:drifted"), "OBSERVES")],
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    let rows = link_rows(&lines);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bucket, LinkBucket::Drifted);
    assert_eq!(
        rows[0].drift_record_id.as_deref(),
        Some("semantic:v1:d_early")
    );
}

#[test]
fn tombstoned_drift_record_does_not_apply() {
    let records = vec![
        symbol("codegraph:v1:drifted"),
        drift_node(
            "semantic:v1:d1",
            "codegraph:v1:drifted",
            "codegraph:v1:after",
            "2026-02-01T00:00:00Z",
        ),
        tombstone("semantic:v1:tomb_d", "semantic:v1:d1"),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:drifted"), "OBSERVES")],
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    let rows = link_rows(&lines);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bucket, LinkBucket::ResolvesLive);
}

#[test]
fn reingested_drift_version_collapses_to_one() {
    let drifted = drift_node(
        "semantic:v1:d1",
        "codegraph:v1:drifted",
        "codegraph:v1:after",
        "2026-02-01T00:00:00Z",
    );
    let records = vec![
        symbol("codegraph:v1:drifted"),
        drifted.clone(),
        drifted, // same id re-ingested: last write wins, no double count
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:drifted"), "OBSERVES")],
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    let rows = link_rows(&lines);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bucket, LinkBucket::Drifted);
    assert_eq!(rows[0].drift_record_id.as_deref(), Some("semantic:v1:d1"));
}

#[test]
fn integrity_flags_both_disagreement_directions() {
    let records = vec![
        symbol("codegraph:v1:live"),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![
                link(Some("codegraph:v1:live"), "MENTIONS_SYMBOL"),
                // No stored edge: array_without_edge.
                link(Some("codegraph:v1:live"), "OBSERVES"),
                // Duplicate array entry: array 2 vs edge 1.
                link(Some("codegraph:v1:live"), "OBSERVES"),
            ],
        ),
        edge(
            EdgeLabel::MentionsSymbol,
            "agent_memory:v1:obs",
            "codegraph:v1:live",
        ),
        edge(
            EdgeLabel::Observes,
            "agent_memory:v1:obs",
            "codegraph:v1:live",
        ),
        // Stored edge with no array entry: edge_without_array.
        edge(
            EdgeLabel::ValidatedBy,
            "agent_memory:v1:obs",
            "codegraph:v1:live",
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    let violations = violation_rows(&lines);
    assert_eq!(violations.len(), 2);

    let missing_edge = violations
        .iter()
        .find(|v| v.violation == IntegrityViolationKind::ArrayWithoutEdge)
        .unwrap();
    assert_eq!(missing_edge.relation, "OBSERVES");
    assert_eq!(missing_edge.array_count, 2);
    assert_eq!(missing_edge.edge_count, 1);

    let missing_array = violations
        .iter()
        .find(|v| v.violation == IntegrityViolationKind::EdgeWithoutArray)
        .unwrap();
    assert_eq!(missing_array.relation, "VALIDATED_BY");
    assert_eq!(missing_array.array_count, 0);
    assert_eq!(missing_array.edge_count, 1);

    let s = summary(&lines);
    assert_eq!(s.integrity_violations, 2);
    assert!(!s.ok);
}

#[test]
fn non_evidence_edges_do_not_trigger_violations() {
    // AUTHORED_BY is intra-agent-memory scaffolding, not grounding evidence:
    // a stored edge with no array entry must not flag edge_without_array.
    let records = vec![
        symbol("codegraph:v1:live"),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:live"), "OBSERVES")],
        ),
        edge(
            EdgeLabel::Observes,
            "agent_memory:v1:obs",
            "codegraph:v1:live",
        ),
        edge(
            EdgeLabel::AuthoredBy,
            "agent_memory:v1:obs",
            "agent_memory:v1:session",
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    assert!(violation_rows(&lines).is_empty());
    assert!(summary(&lines).ok);
}

#[test]
fn tombstoned_and_superseded_edges_do_not_count() {
    let records = vec![
        symbol("codegraph:v1:live"),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:live"), "OBSERVES")],
        ),
        edge(
            EdgeLabel::Observes,
            "agent_memory:v1:obs",
            "codegraph:v1:live",
        ),
    ];
    // Tombstone the edge after its write: the array entry now lacks a live edge.
    let mut tombstoned = records.clone();
    let edge_id = match &tombstoned[2] {
        GraphRecord::Edge { id, .. } => id.clone(),
        _ => panic!("edge record"),
    };
    tombstoned.push(tombstone("edge_tomb", &edge_id));
    let lines = run_memory_evidence_health_audit(&tombstoned);
    let violations = violation_rows(&lines);
    assert_eq!(violations.len(), 1);
    assert_eq!(
        violations[0].violation,
        IntegrityViolationKind::ArrayWithoutEdge
    );

    // Re-ingested duplicate edge collapses to one: no violation either way.
    let mut reingested = records;
    reingested.push(edge(
        EdgeLabel::Observes,
        "agent_memory:v1:obs",
        "codegraph:v1:live",
    ));
    let lines = run_memory_evidence_health_audit(&reingested);
    assert!(violation_rows(&lines).is_empty());
}

#[test]
fn non_observation_class_nodes_are_skipped() {
    let records = vec![
        symbol("codegraph:v1:live"),
        // ToolCall carries evidence_links but is not observation-class.
        observation(
            "agent_memory:v1:tc",
            NodeKind::ToolCall,
            vec![link(Some("codegraph:v1:absent"), "OBSERVES")],
        ),
        observation(
            "agent_memory:v1:agent",
            NodeKind::Agent,
            vec![link(Some("codegraph:v1:absent"), "OBSERVES")],
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    assert!(link_rows(&lines).is_empty());
    assert!(violation_rows(&lines).is_empty());
    let s = summary(&lines);
    assert_eq!(s.sources_checked, 0);
    assert!(s.ok);
}

#[test]
fn tombstoned_source_nodes_are_skipped() {
    let records = vec![
        symbol("codegraph:v1:live"),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:absent"), "OBSERVES")],
        ),
        tombstone("agent_memory:v1:tomb_o", "agent_memory:v1:obs"),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    assert!(link_rows(&lines).is_empty());
    assert!(summary(&lines).ok);
}

#[test]
fn triple_only_link_is_dangling() {
    let records = vec![
        symbol("codegraph:v1:live"),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(None, "OBSERVES")],
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    let rows = link_rows(&lines);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bucket, LinkBucket::Dangling);
    assert_eq!(rows[0].target_record_id, "");
    assert_eq!(rows[0].tombstoned, Some(false));
    assert!(!summary(&lines).ok);
}

#[test]
fn empty_store_yields_zero_summary() {
    let lines = run_memory_evidence_health_audit(&[]);
    let s = summary(&lines);
    assert_eq!(s.links_checked, 0);
    assert_eq!(s.sources_checked, 0);
    assert_eq!(s.integrity_violations, 0);
    assert!(s.ok);
    assert_eq!(lines.len(), 1, "only the summary line");
}

#[test]
fn output_ordering_is_canonical_regardless_of_input_order() {
    let mut records = vec![
        symbol("codegraph:v1:b"),
        symbol("codegraph:v1:a"),
        observation(
            "agent_memory:v1:obs_b",
            NodeKind::Observation,
            vec![
                link(Some("codegraph:v1:b"), "OBSERVES"),
                link(Some("codegraph:v1:a"), "OBSERVES"),
            ],
        ),
        observation(
            "agent_memory:v1:obs_a",
            NodeKind::Decision,
            vec![link(Some("codegraph:v1:zzz_absent"), "OBSERVES")],
        ),
    ];
    let first = run_memory_evidence_health_audit(&records);
    records.reverse();
    let second = run_memory_evidence_health_audit(&records);
    assert_eq!(first, second, "input order must not affect output");

    let rows = link_rows(&first);
    let order: Vec<(&str, &str)> = rows
        .iter()
        .map(|row| (row.source_record_id.as_str(), row.target_record_id.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("agent_memory:v1:obs_a", "codegraph:v1:zzz_absent"),
            ("agent_memory:v1:obs_b", "codegraph:v1:a"),
            ("agent_memory:v1:obs_b", "codegraph:v1:b"),
        ]
    );
    // Summary is always the last line.
    assert!(matches!(
        first.last(),
        Some(MemoryEvidenceHealthLine::Summary(_))
    ));
}

#[test]
fn rows_serialize_with_stable_field_contract() {
    let records = vec![
        symbol("codegraph:v1:live"),
        observation(
            "agent_memory:v1:obs",
            NodeKind::Observation,
            vec![link(Some("codegraph:v1:gone"), "OBSERVES")],
        ),
    ];
    let lines = run_memory_evidence_health_audit(&records);
    let json: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::to_value(line).unwrap())
        .collect();
    assert_eq!(json[0]["type"], "link");
    assert_eq!(json[0]["bucket"], "dangling");
    assert_eq!(json[0]["source_record_id"], "agent_memory:v1:obs");
    assert_eq!(json[0]["target_record_id"], "codegraph:v1:gone");
    assert_eq!(json[0]["tombstoned"], false);
    assert!(json[0].get("drift_record_id").is_none());
    // The link has no stored edge, so an integrity row follows the link rows.
    assert_eq!(json[1]["type"], "integrity_violation");
    assert_eq!(json[1]["violation"], "array_without_edge");
    assert_eq!(json[2]["type"], "summary");
    assert_eq!(json[2]["ok"], false);
}
