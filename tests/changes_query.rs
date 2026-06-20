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

fn changed_in_edge(file_path: &str, commit: &str) -> GraphRecord {
    let src = stable_id(&["node", "file", "repo_test", file_path]);
    let tgt = stable_id(&["node", "commit", "repo_test", commit]);
    GraphRecord::edge(
        EdgeLabel::ChangedIn,
        src,
        tgt,
        None,
        format!("{file_path} changed in {commit}"),
    )
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
    let res = changes_context(&records, "aaaa", "bbbb", None);
    assert!(res.is_err());
    assert!(matches!(res.unwrap_err(), ChangesError::EmptyHistory));
}

#[test]
fn test_missing_commit() {
    let records = vec![commit("aaaaaaaa", &[])];
    let res = changes_context(&records, "aaaa", "bbbb", None);
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
    let res = changes_context(&records, "aa", "aaeebbbb", None);
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
    let res = changes_context(&records, "cccc", "aaaa", None);
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
    let res = changes_context(&records, "aaaa", "bbbb", None);
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
    let ctx = changes_context(&records, "aaaa", "cccc", None).unwrap();

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
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

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
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    // Since it's exactly 3 hops away, it is linked to evidence, so s1 should NOT be unexplained!
    let unexplained_ids: std::collections::BTreeSet<&str> =
        ctx.unexplained.iter().map(|u| u.record_id).collect();
    assert!(
        !unexplained_ids.contains(s1_id.as_str()),
        "3-hop evidence node must explain the change"
    );
}

/// A `File`/`Symbol` stable ID is reused by every temporal snapshot of the same
/// path. Seeding changed facts from `CHANGED_IN` edges must therefore be scoped
/// to the snapshot's own commit: the base-commit snapshot (same stable ID) must
/// not be reported as changed just because the head snapshot changed.
#[test]
fn test_changed_in_excludes_unchanged_same_id_snapshot() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    // Same stable ID at base and head; only the head snapshot is marked changed.
    let fa_base = file_node("src/lib.rs", "aaaaaaaa");
    let fa_head = file_node("src/lib.rs", "bbbbbbbb");
    let changed = changed_in_edge("src/lib.rs", "bbbbbbbb");

    let records = vec![c1, c2, e1, fa_base, fa_head, changed];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    assert_eq!(
        ctx.changed_files.len(),
        1,
        "only the head snapshot changed; the base snapshot must be excluded"
    );
    assert_eq!(ctx.changed_files[0].git_commit, "bbbbbbbb");
}

/// `CHANGED_IN` coverage is detected per range commit, not globally: a commit in
/// the range that carries no `CHANGED_IN` edges still falls back to commit
/// membership even though another commit in the store uses those edges.
#[test]
fn test_changed_in_scoped_per_commit_with_fallback() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let c3 = commit("cccccccc", &["bbbbbbbb"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let e2 = parent_edge("bbbbbbbb", "cccccccc");
    // bbbb carries CHANGED_IN coverage; cccc carries none.
    let fa = file_node("src/a.rs", "bbbbbbbb");
    let changed_a = changed_in_edge("src/a.rs", "bbbbbbbb");
    let fb = file_node("src/b.rs", "cccccccc");

    let records = vec![c1, c2, c3, e1, e2, fa, changed_a, fb];
    let ctx = changes_context(&records, "aaaa", "cccc", None).unwrap();

    let paths: std::collections::BTreeSet<&str> =
        ctx.changed_files.iter().map(|f| f.path).collect();
    assert!(
        paths.contains("src/a.rs"),
        "covered commit reports its change"
    );
    assert!(
        paths.contains("src/b.rs"),
        "uncovered commit falls back to commit membership"
    );
}

/// A relay (e.g. `ToolCall`) that cites a changed file via its own evidence
/// links must still be expanded so its forward `PRODUCED_EVIDENCE` edge reaches
/// the `CommandRun` it produced.
#[test]
fn test_relay_evidence_link_reaches_command_run() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let f1_id = f1.id().to_owned();

    let tc_id = "node:tool_call:tc1";
    let mut tc = GraphRecord::node(
        tc_id.to_owned(),
        NodeKind::ToolCall,
        None,
        None,
        None,
        "Tool call".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut evidence_links,
        ..
    } = tc
    {
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(f1_id),
            target_domain: "codegraph".to_owned(),
            relation: "TOUCHED_FILE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let cr_id = "node:command_run:cr1";
    let cr = GraphRecord::node(
        cr_id.to_owned(),
        NodeKind::CommandRun,
        None,
        None,
        None,
        "cargo test".to_owned(),
    );
    let edge = GraphRecord::edge(
        EdgeLabel::ProducedEvidence,
        tc_id.to_owned(),
        cr_id.to_owned(),
        None,
        "produced".to_owned(),
    );

    let records = vec![c1, c2, e1, f1, tc, cr, edge];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let ver_ids: std::collections::BTreeSet<&str> = ctx
        .verification_evidence
        .iter()
        .map(|v| v.record_id)
        .collect();
    assert!(
        ver_ids.contains(cr_id),
        "CommandRun produced by a relay citing the changed file must appear"
    );
}

/// The `unexplained` traversal must use the same relay rules as the output BFS:
/// an observation reachable from a changed file only through a non-relay node
/// (e.g. a `Repository`) is never emitted, so the change must stay unexplained
/// rather than be falsely marked explained.
#[test]
fn test_evidence_only_through_non_relay_is_unexplained() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let f1_id = f1.id().to_owned();

    let repo_id = "node:repository:repo1";
    let repo = GraphRecord::node(
        repo_id.to_owned(),
        NodeKind::Repository,
        None,
        None,
        None,
        "Repo".to_owned(),
    );
    let obs_id = agent_memory_stable_id(&["obs", "obs_nonrelay"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Obs".to_owned(),
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
        f1_id.clone(),
        repo_id.to_owned(),
        None,
        "e1".to_owned(),
    );
    let edge2 = GraphRecord::edge(
        EdgeLabel::RelatesTo,
        repo_id.to_owned(),
        obs_id.clone(),
        None,
        "e2".to_owned(),
    );

    let records = vec![c1, c2, e1, f1, repo, obs, edge1, edge2];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let unexplained_ids: std::collections::BTreeSet<&str> =
        ctx.unexplained.iter().map(|u| u.record_id).collect();
    assert!(
        unexplained_ids.contains(f1_id.as_str()),
        "a non-relay bridge must not explain the change"
    );
    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|o| o.record_id).collect();
    assert!(
        !obs_ids.contains(obs_id.as_str()),
        "an observation reachable only via a non-relay node must not be emitted"
    );
}

/// A triple-only evidence link (path citation, no resolved `target_record_id`)
/// that targets a changed path must be surfaced in `unresolved` even when its
/// source node is otherwise disconnected from the change.
#[test]
fn test_triple_only_evidence_link_surfaced_for_changed_path() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let f1 = file_node("src/lib.rs", "bbbbbbbb");

    let obs_id = agent_memory_stable_id(&["obs", "obs_triple"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Obs".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: None,
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: Some("src/lib.rs".to_owned()),
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let records = vec![c1, c2, e1, f1, obs];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    assert!(
        ctx.unresolved
            .iter()
            .any(|u| u.source_record_id == obs_id && u.target_handle.contains("src/lib.rs")),
        "triple-only citation to a changed path must be surfaced in unresolved"
    );
}

/// In a shared store two repositories can carry the same commit SHA. Scoping a
/// `query changes` to one repository must resolve only that repo's commit and
/// select only its changed files, never the sibling repo's.
#[test]
fn test_repo_scope_excludes_sibling_repo_with_shared_sha() {
    fn repo_node(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(id.to_owned()),
            format!("Repo {id}"),
        )
    }
    fn scoped_commit(repo: &str, sha: &str, parents: &[&str]) -> GraphRecord {
        GraphRecord::node(
            format!("node:commit:{repo}:{sha}"),
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
            valid_time_source: None,
        })
    }
    fn scoped_file(repo: &str, path: &str, sha: &str) -> GraphRecord {
        GraphRecord::node(
            format!("node:file:{repo}:{path}"),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("File {path}"),
        )
        .with_temporal(TemporalMetadata {
            git_commit: sha.to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        })
    }
    fn contains(repo: &str, child_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Contains,
            repo.to_owned(),
            child_id.to_owned(),
            None,
            "contains".to_owned(),
        )
    }

    let ra = "node:repository:A";
    let rb = "node:repository:B";
    // Both repos share SHAs a0aaaaaa -> a1aaaaaa.
    let records = vec![
        repo_node(ra),
        repo_node(rb),
        scoped_commit("A", "a0aaaaaa", &[]),
        scoped_commit("A", "a1aaaaaa", &["a0aaaaaa"]),
        scoped_commit("B", "a0aaaaaa", &[]),
        scoped_commit("B", "a1aaaaaa", &["a0aaaaaa"]),
        contains(ra, "node:commit:A:a0aaaaaa"),
        contains(ra, "node:commit:A:a1aaaaaa"),
        contains(rb, "node:commit:B:a0aaaaaa"),
        contains(rb, "node:commit:B:a1aaaaaa"),
        scoped_file("A", "src/a.rs", "a1aaaaaa"),
        scoped_file("B", "src/b.rs", "a1aaaaaa"),
        contains(ra, "node:file:A:src/a.rs"),
        contains(rb, "node:file:B:src/b.rs"),
    ];

    // Scoped to repo A: only A's file is reported.
    let ctx = changes_context(&records, "a0", "a1", Some(ra)).unwrap();
    let paths: std::collections::BTreeSet<&str> =
        ctx.changed_files.iter().map(|f| f.path).collect();
    assert_eq!(paths.len(), 1, "scope must select exactly one repo's files");
    assert!(paths.contains("src/a.rs"));
    assert!(!paths.contains("src/b.rs"));

    // Unscoped: the shared SHA collapses and both repos' files appear.
    let ctx_all = changes_context(&records, "a0", "a1", None).unwrap();
    let all_paths: std::collections::BTreeSet<&str> =
        ctx_all.changed_files.iter().map(|f| f.path).collect();
    assert!(all_paths.contains("src/a.rs"));
    assert!(all_paths.contains("src/b.rs"));
}
