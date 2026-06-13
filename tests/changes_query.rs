#![allow(missing_docs)]

use aletheia_egregore::{
    ChangesError, EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind,
    SelectionBasis, SemanticDriftMetadata, TemporalMetadata, changes_context,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, agent_memory_stable_id, stable_id},
};

fn commit(sha: &str, parents: &[&str]) -> GraphRecord {
    let id = stable_id(&["node", "commit", "repo_test", sha]);
    GraphRecord::node(
        id,
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Commit {sha}"),
    )
    .with_temporal(TemporalMetadata {
        git_commit: sha.to_owned(),
        git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    })
}

fn parent_edge(parent: &str, child: &str) -> GraphRecord {
    let p_id = stable_id(&["node", "commit", "repo_test", parent]);
    let c_id = stable_id(&["node", "commit", "repo_test", child]);
    GraphRecord::edge(
        EdgeLabel::ParentOf,
        p_id,
        c_id,
        Some("1.0".to_owned()),
        format!("{parent} is parent of {child}"),
    )
}

fn file_node(path: &str, commit: &str) -> GraphRecord {
    let id = stable_id(&["node", "file", "repo_test", path]);
    GraphRecord::node(
        id,
        NodeKind::File,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        format!("File {path}"),
    )
    .with_temporal(TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    })
}

fn symbol_node(name: &str, path: &str, commit: &str) -> GraphRecord {
    let id = stable_id(&["node", "symbol", "repo_test", path, name]);
    GraphRecord::node(
        id,
        NodeKind::Symbol,
        Some(path.to_owned()),
        None,
        Some(name.to_owned()),
        format!("Symbol {name} in {path}"),
    )
    .with_temporal(TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    })
}

#[test]
fn test_empty_history() {
    let records = vec![];
    let res = changes_context(&records, "aaaa", "bbbb");
    assert!(res.is_err());
    assert!(matches!(res.unwrap_err(), ChangesError::EmptyHistory));
}

#[test]
fn test_missing_commit() {
    let records = vec![commit("aaaaaaaa", &[])];
    let res = changes_context(&records, "aaaa", "bbbb");
    assert!(res.is_err());
    if let ChangesError::MissingCommit { commit_prefix } = res.unwrap_err() {
        assert_eq!(commit_prefix, "bbbb");
    } else {
        panic!("expected MissingCommit");
    }
}

#[test]
fn test_ambiguous_prefix() {
    let records = vec![commit("aaaaaaaa", &[]), commit("aaeebbbb", &[])];
    let res = changes_context(&records, "aa", "aaeebbbb");
    assert!(res.is_err());
    if let ChangesError::AmbiguousCommitPrefix {
        commit_prefix,
        matches,
    } = res.unwrap_err()
    {
        assert_eq!(commit_prefix, "aa");
        assert_eq!(matches.len(), 2);
    } else {
        panic!("expected AmbiguousCommitPrefix");
    }
}

#[test]
fn test_reversed_range() {
    // aaaaaaaa -> bbbbbbbb -> cccccccc
    let records = vec![
        commit("aaaaaaaa", &[]),
        commit("bbbbbbbb", &["aaaaaaaa"]),
        parent_edge("aaaaaaaa", "bbbbbbbb"),
        commit("cccccccc", &["bbbbbbbb"]),
        parent_edge("bbbbbbbb", "cccccccc"),
    ];
    // Asking for cccccccc to aaaaaaaa (reversed)
    let res = changes_context(&records, "cccc", "aaaa");
    assert!(res.is_err());
    assert!(matches!(
        res.unwrap_err(),
        ChangesError::ReversedRange { .. }
    ));
}

#[test]
fn test_no_path() {
    // independent branches
    let records = vec![commit("aaaaaaaa", &[]), commit("bbbbbbbb", &[])];
    let res = changes_context(&records, "aaaa", "bbbb");
    assert!(res.is_err());
    assert!(matches!(res.unwrap_err(), ChangesError::NoPath { .. }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_happy_path() {
    // aaaaaaaa -> bbbbbbbb -> cccccccc
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let c3 = commit("cccccccc", &["bbbbbbbb"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let e2 = parent_edge("bbbbbbbb", "cccccccc");

    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let s1 = symbol_node("hello", "src/lib.rs", "bbbbbbbb");

    let f2 = file_node("src/main.rs", "cccccccc");
    let s2 = symbol_node("main", "src/main.rs", "cccccccc");

    let s2_id = s2.id().to_owned();

    // Add a drift record in commit cccccccc
    let drift_id = "drift:test";
    let drift = GraphRecord::node(
        drift_id.to_owned(),
        NodeKind::SemanticDrift,
        Some("src/main.rs".to_owned()),
        None,
        Some("main".to_owned()),
        "Drift main".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "cccccccc".to_owned(),
        git_parent_commits: vec!["bbbbbbbb".to_owned()],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    })
    .with_semantic_drift(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "test".to_owned(),
            name: "fake-code-model".to_owned(),
            version: "v1".to_owned(),
            dim: 384,
            content_hash: "fixture".to_owned(),
        },
        target_record_id: s2_id.clone(),
        prior_record_id: s1.id().to_owned(),
        before_git_commit: "bbbbbbbb".to_owned(),
        after_git_commit: "cccccccc".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.5,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });

    // Add a tombstone in range
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:test".to_owned(),
        schema_version: 4,
        deleted_id: "node:symbol:repo_test:src/lib.rs:old_symbol".to_owned(),
        summary: "Invalidated stale cached record from src/lib.rs".to_owned(),
        producer: None,
    };

    // Add an observation linked to symbol s1
    let obs_id = agent_memory_stable_id(&["obs", "obs1"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation about hello".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ref mut text,
        ref mut agent_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *text = Some("Obs text".to_owned());
        *agent_id = Some("agent:1".to_owned());
        *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        *confidence = Some("1.0".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(s1.id().to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let records = vec![c1, c2, c3, e1, e2, f1, s1, f2, s2, drift, tombstone, obs];

    // Range aaaaaaaa -> cccccccc should return changes from bbbbbbbb and cccccccc
    let ctx = changes_context(&records, "aaaa", "cccc").unwrap();

    assert_eq!(ctx.commits.len(), 2);
    assert_eq!(ctx.commits[0].commit, "bbbbbbbb");
    assert_eq!(ctx.commits[1].commit, "cccccccc");

    assert_eq!(ctx.changed_files.len(), 2);
    assert_eq!(ctx.changed_files[0].path, "src/lib.rs");
    assert_eq!(ctx.changed_files[1].path, "src/main.rs");

    assert_eq!(ctx.changed_symbols.len(), 2);
    assert_eq!(ctx.changed_symbols[0].name, "hello");
    assert_eq!(ctx.changed_symbols[1].name, "main");

    assert_eq!(ctx.drift_records.len(), 1);
    assert_eq!(ctx.drift_records[0].target_record_id, s2_id);

    assert_eq!(ctx.tombstones.len(), 1);
    assert_eq!(
        ctx.tombstones[0].deleted_id,
        "node:symbol:repo_test:src/lib.rs:old_symbol"
    );

    assert_eq!(ctx.observations.len(), 1);
    assert_eq!(ctx.observations[0].record_id, obs_id.as_str());

    // unexplained check: s2 / main has no evidence links, so it is unexplained
    assert!(!ctx.unexplained.is_empty());
    let unexplained_ids: std::collections::BTreeSet<&str> =
        ctx.unexplained.iter().map(|u| u.record_id).collect();
    assert!(unexplained_ids.contains(s2_id.as_str()));
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_changes_query_redaction() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let s1 = symbol_node("hello", "src/lib.rs", "bbbbbbbb");

    // Create an observation linked to s1
    let obs_id = agent_memory_stable_id(&["obs", "obs_redact"]);
    let mut obs = GraphRecord::node(
        obs_id,
        NodeKind::Observation,
        None,
        None,
        None,
        "Original observation summary".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut text,
        ref mut agent_id,
        ref mut session_id,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *text = Some("Sensitive observation text".to_owned());
        *agent_id = Some("my_agent".to_owned());
        *session_id = Some("my_session".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(s1.id().to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Create a task linked to s1
    let task_id = agent_memory_stable_id(&["task", "task_redact"]);
    let mut task = GraphRecord::node(
        task_id,
        NodeKind::Task,
        None,
        None,
        None,
        "Original task summary".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut title,
        ref mut text,
        ref mut author,
        ref mut evidence_links,
        ..
    } = task
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *title = Some("Sensitive task title".to_owned());
        *text = Some("Sensitive task text".to_owned());
        *author = Some("my_author".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(s1.id().to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Create a verification linked to s1
    let ver_id = agent_memory_stable_id(&["verification", "ver_redact"]);
    let mut ver = GraphRecord::node(
        ver_id,
        NodeKind::Verification,
        None,
        None,
        None,
        "Original verification summary".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut validation_summary,
        ref mut author,
        ref mut evidence_links,
        ..
    } = ver
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *validation_summary = Some("Sensitive validation output".to_owned());
        *author = Some("ver_author".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(s1.id().to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let records = vec![c1, c2, e1, f1, s1, obs, task, ver];
    let ctx = changes_context(&records, "aaaa", "bbbb").unwrap();

    // Verify observation is redacted
    assert_eq!(ctx.observations.len(), 1);
    let o = &ctx.observations[0];
    assert_eq!(o.text, None);
    assert_eq!(o.summary, "Observation by my_agent:my_session");

    // Verify project state (task) is redacted
    assert_eq!(ctx.project_state.len(), 1);
    let t = &ctx.project_state[0];
    assert_eq!(t.title, None);
    assert_eq!(t.text, None);
    assert_eq!(t.summary, "Task by my_author");

    // Verify verification evidence is redacted
    assert_eq!(ctx.verification_evidence.len(), 1);
    let v = &ctx.verification_evidence[0];
    assert_eq!(v.validation_summary, None);
    assert_eq!(v.text, None);
    assert_eq!(v.summary, "Verification by ver_author");
}

#[test]
fn test_changes_query_three_hop_evidence() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let s1 = symbol_node("hello", "src/lib.rs", "bbbbbbbb");
    let s1_id = s1.id().to_owned();

    // 3-hop chain:
    // s1 -> (EdgeLabel::RelatesTo) -> node1 (AgentTurn, relay)
    // node1 -> (EdgeLabel::RelatesTo) -> node2 (AgentRun, relay)
    // node2 -> (EdgeLabel::RelatesTo) -> obs (Observation, evidence)

    let node1_id = "node:agent_turn:turn1";
    let node1 = GraphRecord::node(
        node1_id.to_owned(),
        NodeKind::AgentTurn,
        None,
        None,
        None,
        "Turn 1".to_owned(),
    );

    let node2_id = "node:agent_run:run1";
    let node2 = GraphRecord::node(
        node2_id.to_owned(),
        NodeKind::AgentRun,
        None,
        None,
        None,
        "Run 1".to_owned(),
    );

    let obs_id = agent_memory_stable_id(&["obs", "obs3"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation 3".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }

    let edge1 = GraphRecord::edge(
        EdgeLabel::RelatesTo,
        s1_id.clone(),
        node1_id.to_owned(),
        None,
        "edge 1".to_owned(),
    );

    let edge2 = GraphRecord::edge(
        EdgeLabel::RelatesTo,
        node1_id.to_owned(),
        node2_id.to_owned(),
        None,
        "edge 2".to_owned(),
    );

    let edge3 = GraphRecord::edge(
        EdgeLabel::RelatesTo,
        node2_id.to_owned(),
        obs_id,
        None,
        "edge 3".to_owned(),
    );

    let records = vec![c1, c2, e1, f1, s1, node1, node2, obs, edge1, edge2, edge3];
    let ctx = changes_context(&records, "aaaa", "bbbb").unwrap();

    // Since it's exactly 3 hops away, it is linked to evidence, so s1 should NOT be unexplained!
    let unexplained_ids: std::collections::BTreeSet<&str> =
        ctx.unexplained.iter().map(|u| u.record_id).collect();
    assert!(
        !unexplained_ids.contains(s1_id.as_str()),
        "3-hop evidence node must explain the change"
    );
}
