//! Temporal resolver tests.

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind,
    temporal_status::{TemporalReference, TemporalResolver},
};

fn make_node(
    id: &str,
    superseded_by: Option<&str>,
    agent_id: Option<&str>,
    session_id: Option<&str>,
) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_string(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("Observation {id}"),
    );
    if let GraphRecord::Node {
        superseded_by: ref mut node_sub_by,
        agent_id: ref mut node_agent_id,
        session_id: ref mut node_session_id,
        ..
    } = rec
    {
        *node_sub_by = superseded_by.map(String::from);
        *node_agent_id = agent_id.map(String::from);
        *node_session_id = session_id.map(String::from);
    }
    rec
}

fn make_node_with_links(
    id: &str,
    agent_id: Option<&str>,
    session_id: Option<&str>,
    links: Vec<EvidenceLink>,
) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_string(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("Observation {id}"),
    );
    if let GraphRecord::Node {
        agent_id: ref mut node_agent_id,
        session_id: ref mut node_session_id,
        evidence_links: ref mut node_links,
        ..
    } = rec
    {
        *node_agent_id = agent_id.map(String::from);
        *node_session_id = session_id.map(String::from);
        *node_links = Some(links);
    }
    rec
}

fn make_edge(source: &str, target: &str, label: EdgeLabel) -> GraphRecord {
    GraphRecord::edge(
        label,
        source.to_string(),
        target.to_string(),
        None,
        format!("{source} {label:?} {target}"),
    )
}

#[test]
fn test_uncontested_record() {
    let records = vec![make_node("A", None, Some("agent-1"), Some("session-1"))];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "current");
    assert!(superseded_by_refs.is_empty());
    assert!(contradicted_by_refs.is_empty());
}

#[test]
fn test_simple_superseded_by_field() {
    let records = vec![
        make_node("A", Some("B"), Some("agent-1"), Some("session-1")),
        make_node("B", None, Some("agent-1"), Some("session-2")),
    ];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "superseded");
    assert_eq!(
        superseded_by_refs,
        vec![TemporalReference {
            record_id: "B".to_string(),
            handle: "agent-1:session-2".to_string()
        }]
    );
    assert!(contradicted_by_refs.is_empty());

    let (b_status, b_superseded, b_contradicted) = resolver.resolve_status("B");
    assert_eq!(b_status, "current");
    assert!(b_superseded.is_empty());
    assert!(b_contradicted.is_empty());
}

#[test]
fn test_supersedes_edge() {
    let records = vec![
        make_node("A", None, Some("agent-1"), Some("session-1")),
        make_node("B", None, Some("agent-1"), Some("session-2")),
        make_edge("B", "A", EdgeLabel::Supersedes), // B supersedes A
    ];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "superseded");
    assert_eq!(
        superseded_by_refs,
        vec![TemporalReference {
            record_id: "B".to_string(),
            handle: "agent-1:session-2".to_string()
        }]
    );
    assert!(contradicted_by_refs.is_empty());
}

#[test]
fn test_supersedes_evidence_link() {
    let link = EvidenceLink {
        target_record_id: Some("A".to_string()),
        target_domain: "agent_memory".to_string(),
        relation: "SUPERSEDES".to_string(),
        confidence: "1.0".to_string(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    };
    let records = vec![
        make_node("A", None, Some("agent-1"), Some("session-1")),
        make_node_with_links("B", Some("agent-1"), Some("session-2"), vec![link]),
    ];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "superseded");
    assert_eq!(
        superseded_by_refs,
        vec![TemporalReference {
            record_id: "B".to_string(),
            handle: "agent-1:session-2".to_string()
        }]
    );
    assert!(contradicted_by_refs.is_empty());
}

#[test]
fn test_transitive_supersession() {
    let records = vec![
        make_node("A", Some("B"), Some("agent-1"), Some("session-1")),
        make_node("B", Some("C"), Some("agent-1"), Some("session-2")),
        make_node("C", Some("D"), Some("agent-1"), Some("session-3")),
        make_node("D", None, Some("agent-1"), Some("session-4")),
    ];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "superseded");
    assert_eq!(
        superseded_by_refs,
        vec![TemporalReference {
            record_id: "D".to_string(),
            handle: "agent-1:session-4".to_string()
        }]
    );
    assert!(contradicted_by_refs.is_empty());

    let (b_status, b_sub, _) = resolver.resolve_status("B");
    assert_eq!(b_status, "superseded");
    assert_eq!(
        b_sub,
        vec![TemporalReference {
            record_id: "D".to_string(),
            handle: "agent-1:session-4".to_string()
        }]
    );
}

#[test]
fn test_supersession_cycle() {
    let records = vec![
        make_node("A", Some("B"), Some("agent-1"), Some("session-1")),
        make_node("B", Some("A"), Some("agent-1"), Some("session-2")),
    ];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "cycle");
    assert!(superseded_by_refs.is_empty());
    assert!(contradicted_by_refs.is_empty());
}

#[test]
fn test_simple_contradiction() {
    let records = vec![
        make_node("A", None, Some("agent-1"), Some("session-1")),
        make_node("B", None, Some("agent-1"), Some("session-2")),
        make_edge("A", "B", EdgeLabel::Contradicts),
    ];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "contradicted");
    assert_eq!(
        contradicted_by_refs,
        vec![TemporalReference {
            record_id: "B".to_string(),
            handle: "agent-1:session-2".to_string()
        }]
    );
    assert!(superseded_by_refs.is_empty());

    let (b_status, _, b_contra) = resolver.resolve_status("B");
    assert_eq!(b_status, "contradicted");
    assert_eq!(
        b_contra,
        vec![TemporalReference {
            record_id: "A".to_string(),
            handle: "agent-1:session-1".to_string()
        }]
    );
}

#[test]
fn test_contradiction_evidence_link() {
    let link = EvidenceLink {
        target_record_id: Some("A".to_string()),
        target_domain: "agent_memory".to_string(),
        relation: "CONTRADICTS".to_string(),
        confidence: "1.0".to_string(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    };
    let records = vec![
        make_node("A", None, Some("agent-1"), Some("session-1")),
        make_node_with_links("B", Some("agent-1"), Some("session-2"), vec![link]),
    ];
    let resolver = TemporalResolver::build(&records);
    let (status, superseded_by_refs, contradicted_by_refs) = resolver.resolve_status("A");
    assert_eq!(status, "contradicted");
    assert_eq!(
        contradicted_by_refs,
        vec![TemporalReference {
            record_id: "B".to_string(),
            handle: "agent-1:session-2".to_string()
        }]
    );
    assert!(superseded_by_refs.is_empty());
}
