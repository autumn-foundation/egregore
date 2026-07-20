//! Unit tests for the store-wide evidence-link integrity audit (issue #217).

use super::*;
use crate::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EvidenceLink, GraphRecord, NodeKind, PROJECT_SCHEMA_VERSION,
    SourceSpan,
};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn mk_span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 42,
        start_line,
        end_line,
    }
}

/// A code `Symbol` node carrying a repo-relative path + span.
fn symbol(id: &str, path: &str, span: SourceSpan) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span),
        Some("sym".to_owned()),
        "symbol".to_owned(),
    )
}

/// An agent-memory `Observation` node with optional inline evidence links.
fn observation(id: &str, links: Vec<EvidenceLink>) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        session_id,
        text,
        evidence_links,
        ..
    } = &mut rec
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent_1".to_owned());
        *session_id = Some("session_1".to_owned());
        *text = Some("SENTINEL_OBSERVATION_BODY".to_owned());
        if !links.is_empty() {
            *evidence_links = Some(links);
        }
    }
    rec
}

/// A project-domain node of the given kind (e.g. `AcceptanceCriterion`, `Task`).
fn project_node(id: &str, kind: NodeKind) -> GraphRecord {
    let mut rec = GraphRecord::node(id.to_owned(), kind, None, None, None, "project".to_owned());
    if let GraphRecord::Node {
        schema_version,
        domain,
        ..
    } = &mut rec
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *domain = Some("project".to_owned());
    }
    rec
}

/// A standalone edge record with a custom stable ID.
fn edge(id: &str, label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::Edge {
        id: id.to_owned(),
        schema_version: 1,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence: None,
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        basis: None,
        temporal: None,
        summary: "SENTINEL_EDGE_SUMMARY".to_owned(),
        producer: None,
    }
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

/// An inline evidence link, target by record ID.
fn link(target: &str, target_domain: &str, relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: target_domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

/// An inline evidence link with NO `target_record_id` (triple-only).
fn triple_only_link(relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: None,
        target_domain: "codegraph".to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: Some("src/lib.rs".to_owned()),
        target_span: Some(mk_span(1, 2)),
        target_git_commit: None,
    }
}

fn find_broken<'a>(
    report: &'a EvidenceLinkAuditReport,
    target: &str,
) -> Option<&'a BrokenEvidenceEdge> {
    report
        .broken_edges
        .iter()
        .find(|b| b.target_record_id == target)
}

// ---------------------------------------------------------------------------
// One RED test per defect class (cross-domain, >=3 domains — AC1)
// ---------------------------------------------------------------------------

// 1. agent_memory OBSERVES an absent code Symbol.
#[test]
fn observes_to_absent_code_symbol() {
    let obs = observation(
        "agent_memory:v1:obs1",
        vec![link("codegraph:v1:ghost", "codegraph", "OBSERVES")],
    );
    let report = run_evidence_link_audit(&[obs]);

    let broken = find_broken(&report, "codegraph:v1:ghost").expect("dangling OBSERVES reported");
    assert_eq!(broken.case, BrokenCase::Absent);
    assert_eq!(broken.source_domain, "agent_memory");
    assert_eq!(broken.source_kind, "Observation");
    assert_eq!(broken.edge_label, "OBSERVES");
    assert_eq!(
        broken.representation,
        EdgeRepresentation::InlineEvidenceLink
    );
    assert!(broken.target_repo_relative_path.is_none());
    assert!(!report.ok);
    assert_eq!(report.broken_edge_count, 1);
}

// 2. agent_memory OBSERVES a tombstoned code Symbol — path/span recovered.
#[test]
fn observes_to_tombstoned_code_symbol() {
    let sym = symbol("codegraph:v1:sym1", "src/lib.rs", mk_span(10, 20));
    let obs = observation(
        "agent_memory:v1:obs2",
        vec![link("codegraph:v1:sym1", "codegraph", "OBSERVES")],
    );
    let tomb = tombstone("codegraph:v1:tomb1", "codegraph:v1:sym1");
    let report = run_evidence_link_audit(&[sym, obs, tomb]);

    let broken = find_broken(&report, "codegraph:v1:sym1").expect("tombstoned OBSERVES reported");
    assert_eq!(broken.case, BrokenCase::Tombstoned);
    assert_eq!(broken.source_domain, "agent_memory");
    assert_eq!(
        broken.target_repo_relative_path.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(broken.target_span, Some(mk_span(10, 20)));
}

// 3. verification target of a VALIDATED_BY link is absent.
#[test]
fn validated_by_missing_verification() {
    let obs = observation(
        "agent_memory:v1:obs3",
        vec![link(
            "verification:v1:ghost",
            "verification",
            "VALIDATED_BY",
        )],
    );
    let report = run_evidence_link_audit(&[obs]);

    let broken = find_broken(&report, "verification:v1:ghost").expect("dangling VALIDATED_BY");
    assert_eq!(broken.case, BrokenCase::Absent);
    assert_eq!(broken.edge_label, "VALIDATED_BY");
    assert_eq!(broken.target_domain.as_deref(), Some("verification"));
}

// 4. project CLOSES_ACCEPTANCE_CRITERION edge to a missing record.
#[test]
fn closes_acceptance_criterion_missing() {
    let ac = project_node("project:v1:ac1", NodeKind::AcceptanceCriterion);
    let e = edge(
        "project:v1:edge_closes",
        EdgeLabel::ClosesAcceptanceCriterion,
        "project:v1:ac1",
        "verification:v1:ghost",
    );
    let report = run_evidence_link_audit(&[ac, e]);

    let broken = find_broken(&report, "verification:v1:ghost").expect("dangling CLOSES edge");
    assert_eq!(broken.case, BrokenCase::Absent);
    assert_eq!(broken.edge_label, "CLOSES_ACCEPTANCE_CRITERION");
    assert_eq!(broken.representation, EdgeRepresentation::EdgeRecord);
    assert_eq!(
        broken.edge_record_id.as_deref(),
        Some("project:v1:edge_closes")
    );
    assert_eq!(broken.source_domain, "project");
    assert_eq!(broken.source_kind, "AcceptanceCriterion");
}

// 5. agent_memory REFERENCES_TASK an absent project Task.
#[test]
fn references_task_missing_task() {
    let obs = observation(
        "agent_memory:v1:obs5",
        vec![link("project:v1:ghost_task", "project", "REFERENCES_TASK")],
    );
    let report = run_evidence_link_audit(&[obs]);

    let broken = find_broken(&report, "project:v1:ghost_task").expect("dangling REFERENCES_TASK");
    assert_eq!(broken.case, BrokenCase::Absent);
    assert_eq!(broken.edge_label, "REFERENCES_TASK");
    assert_eq!(broken.source_domain, "agent_memory");
}

// 6. project OWNED_BY_TASK edge to a tombstoned Task.
#[test]
fn owned_by_task_tombstoned_task() {
    let task = project_node("project:v1:task1", NodeKind::Task);
    let child = project_node("project:v1:child1", NodeKind::AcceptanceCriterion);
    let e = edge(
        "project:v1:edge_owned",
        EdgeLabel::OwnedByTask,
        "project:v1:child1",
        "project:v1:task1",
    );
    let tomb = tombstone("project:v1:tomb_task", "project:v1:task1");
    let report = run_evidence_link_audit(&[task, child, e, tomb]);

    let broken = find_broken(&report, "project:v1:task1").expect("tombstoned OWNED_BY_TASK");
    assert_eq!(broken.case, BrokenCase::Tombstoned);
    assert_eq!(broken.source_domain, "project");
    assert_eq!(broken.edge_label, "OWNED_BY_TASK");
}

// 7. one broken target reached via BOTH a standalone edge and an inline link.
#[test]
fn edge_record_and_inline_both_swept() {
    let obs = observation(
        "agent_memory:v1:obs7",
        vec![link("codegraph:v1:ghost7", "codegraph", "OBSERVES")],
    );
    let e = edge(
        "agent_memory:v1:edge7",
        EdgeLabel::Observes,
        "agent_memory:v1:obs7",
        "codegraph:v1:ghost7",
    );
    let report = run_evidence_link_audit(&[obs, e]);

    let rows: Vec<&BrokenEvidenceEdge> = report
        .broken_edges
        .iter()
        .filter(|b| b.target_record_id == "codegraph:v1:ghost7")
        .collect();
    assert_eq!(rows.len(), 2, "both representations reported");
    assert!(
        rows.iter()
            .any(|b| b.representation == EdgeRepresentation::EdgeRecord)
    );
    assert!(
        rows.iter()
            .any(|b| b.representation == EdgeRepresentation::InlineEvidenceLink)
    );
}

// ---------------------------------------------------------------------------
// Clean-pass test (AC5 — zero false positives)
// ---------------------------------------------------------------------------

// 8. every checked evidence edge resolves; excluded code topology is ignored.
#[test]
fn all_targets_live_reports_clean() {
    let sym = symbol("codegraph:v1:sym_live", "src/a.rs", mk_span(1, 5));
    let obs = observation(
        "agent_memory:v1:obs_live",
        vec![link("codegraph:v1:sym_live", "codegraph", "OBSERVES")],
    );
    // A code-topology CALLS edge to a removed node must NOT be reported.
    let removed = symbol("codegraph:v1:removed", "src/b.rs", mk_span(1, 2));
    let calls = edge(
        "codegraph:v1:edge_calls",
        EdgeLabel::Calls,
        "codegraph:v1:sym_live",
        "codegraph:v1:removed",
    );
    let calls_tomb = tombstone("codegraph:v1:tomb_removed", "codegraph:v1:removed");

    let report = run_evidence_link_audit(&[sym, obs, removed, calls, calls_tomb]);
    assert!(report.ok);
    assert_eq!(report.broken_edge_count, 0);
    assert!(report.broken_edges.is_empty());
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "no_broken_evidence_links")
    );
}

// A target tombstoned then RE-INGESTED after its tombstone is live again (the
// Liveness revival rule), so its evidence edge is not reported.
#[test]
fn revived_target_is_not_broken() {
    let obs = observation(
        "agent_memory:v1:obs_rev",
        vec![link("codegraph:v1:revived", "codegraph", "OBSERVES")],
    );
    let sym_v1 = symbol("codegraph:v1:revived", "src/r.rs", mk_span(1, 3));
    let tomb = tombstone("codegraph:v1:tomb_rev", "codegraph:v1:revived");
    let sym_v2 = symbol("codegraph:v1:revived", "src/r.rs", mk_span(1, 4));
    // Append order: obs, symbol, tombstone, re-added symbol (revival).
    let report = run_evidence_link_audit(&[obs, sym_v1, tomb, sym_v2]);
    assert!(
        report.ok,
        "re-ingested-after-tombstone target is live again"
    );
    assert_eq!(report.broken_edge_count, 0);
}

// ---------------------------------------------------------------------------
// Triple-only inline links (Out-of-Scope boundary)
// ---------------------------------------------------------------------------

#[test]
fn triple_only_link_is_diagnostic_not_broken() {
    let obs = observation(
        "agent_memory:v1:obs_triple",
        vec![triple_only_link("OBSERVES")],
    );
    let report = run_evidence_link_audit(&[obs]);
    assert!(
        report.ok,
        "a triple-only link is not an absent/tombstoned defect"
    );
    assert_eq!(report.broken_edge_count, 0);
    let diag = report
        .diagnostics
        .iter()
        .find(|d| d.code == "unresolvable_link_no_target_id")
        .expect("triple-only tally diagnostic");
    assert_eq!(diag.count, Some(1));
}

// ---------------------------------------------------------------------------
// Partition / disclosure
// ---------------------------------------------------------------------------

#[test]
fn partition_is_complete_and_disjoint() {
    let (checked, excluded) = edge_classes();
    assert_eq!(
        checked.len() + excluded.len(),
        ALL_EDGE_LABELS.len(),
        "every EdgeLabel variant is classified exactly once"
    );
    // No overlap.
    for c in &checked {
        assert!(!excluded.contains(c), "{c} classified in both sets");
    }
    // Spot-check the documented vocabulary.
    assert!(checked.contains(&"OBSERVES".to_owned()));
    assert!(checked.contains(&"VALIDATED_BY".to_owned()));
    assert!(checked.contains(&"REFERENCES_TASK".to_owned()));
    assert!(excluded.contains(&"CALLS".to_owned()));
    assert!(excluded.contains(&"CONTAINS".to_owned()));
    assert!(excluded.contains(&"AUTHORED_BY".to_owned()));
}

#[test]
fn report_discloses_partition() {
    let report = run_evidence_link_audit(&[]);
    assert!(report.checked_edge_labels.contains(&"OBSERVES".to_owned()));
    assert!(report.excluded_edge_labels.contains(&"CALLS".to_owned()));
    // Sorted disclosure.
    let mut sorted = report.checked_edge_labels.clone();
    sorted.sort();
    assert_eq!(report.checked_edge_labels, sorted);
}

// ---------------------------------------------------------------------------
// Aggregation + determinism + safety
// ---------------------------------------------------------------------------

fn multi_defect_records() -> Vec<GraphRecord> {
    vec![
        observation(
            "agent_memory:v1:m1",
            vec![
                link("codegraph:v1:g1", "codegraph", "OBSERVES"),
                link("verification:v1:g2", "verification", "VALIDATED_BY"),
            ],
        ),
        project_node("project:v1:ac", NodeKind::AcceptanceCriterion),
        edge(
            "project:v1:e1",
            EdgeLabel::ClosesAcceptanceCriterion,
            "project:v1:ac",
            "verification:v1:g3",
        ),
    ]
}

#[test]
fn aggregates_by_domain_label_and_case() {
    let report = run_evidence_link_audit(&multi_defect_records());
    assert_eq!(report.broken_edge_count, 3);
    assert_eq!(report.by_source_domain.get("agent_memory"), Some(&2));
    assert_eq!(report.by_source_domain.get("project"), Some(&1));
    assert_eq!(report.by_edge_label.get("OBSERVES"), Some(&1));
    assert_eq!(report.by_edge_label.get("VALIDATED_BY"), Some(&1));
    assert_eq!(
        report.by_edge_label.get("CLOSES_ACCEPTANCE_CRITERION"),
        Some(&1)
    );
    assert_eq!(report.by_case.get("absent"), Some(&3));
    assert_eq!(report.checked_edge_count, 3);
}

#[test]
fn output_byte_identical_across_runs() {
    let records = multi_defect_records();
    let first = serde_json::to_string(&run_evidence_link_audit(&records)).unwrap();
    for _ in 0..4 {
        let again = serde_json::to_string(&run_evidence_link_audit(&records)).unwrap();
        assert_eq!(first, again, "byte-identical across runs");
    }
}

// AC8: the report never carries raw source/observation/edge body text.
#[test]
fn no_raw_payloads_in_output() {
    let report = run_evidence_link_audit(&multi_defect_records());
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("SENTINEL_OBSERVATION_BODY"));
    assert!(!json.contains("SENTINEL_EDGE_SUMMARY"));
}

// An excluded-label inline relation (e.g. AUTHORED_BY) is never checked.
#[test]
fn excluded_label_inline_link_ignored() {
    let obs = observation(
        "agent_memory:v1:obs_excl",
        vec![link(
            "agent_memory:v1:ghost_sess",
            "agent_memory",
            "AUTHORED_BY",
        )],
    );
    let report = run_evidence_link_audit(&[obs]);
    assert!(report.ok, "AUTHORED_BY is not a checked evidence edge");
    assert_eq!(report.checked_edge_count, 0);
}
