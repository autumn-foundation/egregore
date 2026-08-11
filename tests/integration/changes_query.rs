#![allow(missing_docs)]

use aletheia_egregore::{
    ChangesError, EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind,
    SelectionBasis, SemanticDriftMetadata, TemporalMetadata, changes_context,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, OutputHandle, agent_memory_stable_id, stable_id},
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

    // Corpus disclosure (issue #427): this hand-built fixture carries no
    // Repository source_snapshot, so this range lane discloses
    // `single_snapshot` (the range is the analysis window).
    assert_eq!(ctx.corpus_mode, "single_snapshot");
    assert_eq!(ctx.corpus_mode_source, "default");

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

/// When the range uses `CHANGED_IN` edges, a commit that only re-emits a
/// snapshot without a `CHANGED_IN` edge (e.g. a doc/config-only commit) must not
/// have that file reported as changed — only the commits that actually carry the
/// edge do.
#[test]
fn test_doc_only_commit_does_not_report_reemitted_snapshot() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let c3 = commit("cccccccc", &["bbbbbbbb"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let e2 = parent_edge("bbbbbbbb", "cccccccc");
    // src/a.rs actually changes at bbbb (CHANGED_IN edge). At cccc it is only
    // re-emitted (a doc-only commit) with no CHANGED_IN edge.
    let fa_b = file_node("src/a.rs", "bbbbbbbb");
    let changed_a = changed_in_edge("src/a.rs", "bbbbbbbb");
    let fa_c = file_node("src/a.rs", "cccccccc");

    let records = vec![c1, c2, c3, e1, e2, fa_b, changed_a, fa_c];
    let ctx = changes_context(&records, "aaaa", "cccc", None).unwrap();

    assert_eq!(
        ctx.changed_files.len(),
        1,
        "only the commit with a CHANGED_IN edge reports the file"
    );
    assert_eq!(ctx.changed_files[0].git_commit, "bbbbbbbb");
}

/// When the range carries no `CHANGED_IN` edges at all (history written without
/// them), selection falls back to commit membership for every range commit.
#[test]
fn test_no_changed_in_edges_falls_back_to_commit_membership() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let c3 = commit("cccccccc", &["bbbbbbbb"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let e2 = parent_edge("bbbbbbbb", "cccccccc");
    let fa = file_node("src/a.rs", "bbbbbbbb");
    let fb = file_node("src/b.rs", "cccccccc");

    let records = vec![c1, c2, c3, e1, e2, fa, fb];
    let ctx = changes_context(&records, "aaaa", "cccc", None).unwrap();

    let paths: std::collections::BTreeSet<&str> =
        ctx.changed_files.iter().map(|f| f.path).collect();
    assert!(paths.contains("src/a.rs"));
    assert!(paths.contains("src/b.rs"));
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

/// The redacted changes response must not leak captured command output: an
/// `OutputHandle` carrying inline stdout/stderr bytes is reduced to hash/size
/// metadata before serialization.
#[test]
fn test_redacted_changes_evidence_strips_inline_output() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let f1_id = f1.id().to_owned();

    // A CommandRun with captured stdout inline, linked to the changed file.
    let cr_id = agent_memory_stable_id(&["verification", "cr_inline"]);
    let mut cr = GraphRecord::node(
        cr_id,
        NodeKind::CommandRun,
        None,
        None,
        None,
        "cargo test".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut stdout_handle,
        ref mut evidence_links,
        ..
    } = cr
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *stdout_handle = Some(Box::new(OutputHandle {
            inline: Some("SECRET stdout bytes".to_owned()),
            hash: "blake3hash".to_owned(),
            bytes: 19,
        }));
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(f1_id),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let records = vec![c1, c2, e1, f1, cr];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    assert_eq!(ctx.verification_evidence.len(), 1);
    let v = &ctx.verification_evidence[0];
    let handle = v.stdout_handle.as_ref().expect("stdout handle present");
    assert_eq!(
        handle.inline, None,
        "inline output must be stripped from redacted changes evidence"
    );
    assert_eq!(handle.hash, "blake3hash");
    assert_eq!(handle.bytes, 19);
}

/// `EXPLAINS_CHANGE` evidence that targets the per-commit `Change` node (not the
/// File/Symbol) must still be discovered, even though the `Change` is de-duped
/// from `changed_files` when a File snapshot already covers it.
#[test]
fn test_explains_change_evidence_on_change_node_is_seeded() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    // Normal Rust modification: File snapshot + CHANGED_IN edge at bbbb.
    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let changed = changed_in_edge("src/lib.rs", "bbbbbbbb");

    // The per-commit Change node for the same (path, commit).
    let change_id = "node:change:repo_test:bbbbbbbb:M:src/lib.rs";
    let change = GraphRecord::node(
        change_id.to_owned(),
        NodeKind::Change,
        Some("src/lib.rs".to_owned()),
        None,
        Some("M src/lib.rs".to_owned()),
        "Git change M to src/lib.rs".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "bbbbbbbb".to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    // Observation explains the Change node only (no link to the File/Symbol).
    let obs_id = agent_memory_stable_id(&["obs", "explains_change"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Explains the change".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    let explains = GraphRecord::edge(
        EdgeLabel::ExplainsChange,
        obs_id.clone(),
        change_id.to_owned(),
        None,
        "explains".to_owned(),
    );

    let records = vec![c1, c2, e1, f1, changed, change, obs, explains];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|o| o.record_id).collect();
    assert!(
        obs_ids.contains(obs_id.as_str()),
        "EXPLAINS_CHANGE evidence on the Change node must be surfaced"
    );
}

/// A triple-only citation anchored to a commit outside the queried range must
/// not be surfaced as context for a later change to the same path.
#[test]
fn test_triple_only_citation_out_of_range_commit_is_filtered() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let f1 = file_node("src/lib.rs", "bbbbbbbb");

    let triple = |id_seed: &str, anchor: &str| {
        let id = agent_memory_stable_id(&["obs", id_seed]);
        let mut obs = GraphRecord::node(
            id.clone(),
            NodeKind::Observation,
            None,
            None,
            None,
            "cite".to_owned(),
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
                target_git_commit: Some(anchor.to_owned()),
            }]);
        }
        (id, obs)
    };

    let (fresh_id, fresh) = triple("fresh", "bbbbbbbb"); // in range
    let (stale_id, stale) = triple("stale", "zzzzzzzz"); // out of range

    let records = vec![c1, c2, e1, f1, fresh, stale];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let sources: std::collections::BTreeSet<&str> = ctx
        .unresolved
        .iter()
        .map(|u| u.source_record_id.as_str())
        .collect();
    assert!(
        sources.contains(fresh_id.as_str()),
        "in-range triple citation should surface"
    );
    assert!(
        !sources.contains(stale_id.as_str()),
        "out-of-range triple citation must be filtered"
    );
}

/// A `Symbol` snapshot carrying an explicit body, used to drive the symbol-level
/// change gate: the `summary` stands in for the normalized source body that the
/// real scanner emits.
fn symbol_node_body(name: &str, path: &str, commit: &str, body: &str) -> GraphRecord {
    let id = stable_id(&["node", "symbol", "repo_test", path, name]);
    GraphRecord::node(
        id,
        NodeKind::Symbol,
        Some(path.to_owned()),
        None,
        Some(name.to_owned()),
        body.to_owned(),
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

/// A `CHANGED_IN` edge whose source is a `Symbol` stable ID, mirroring the
/// per-symbol edges `scan-history` writes for every symbol in a touched file.
fn symbol_changed_in_edge(name: &str, path: &str, commit: &str) -> GraphRecord {
    let src = stable_id(&["node", "symbol", "repo_test", path, name]);
    let tgt = stable_id(&["node", "commit", "repo_test", commit]);
    GraphRecord::edge(
        EdgeLabel::ChangedIn,
        src,
        tgt,
        None,
        format!("symbol {name} changed in {commit}"),
    )
}

/// `scan-history` adds a `CHANGED_IN` edge for every `Symbol` snapshot in a
/// touched file, not only the symbol whose body the commit edited. A symbol whose
/// body is identical to its parent-commit snapshot must be excluded from
/// `changed_symbols`, so agents are not sent to inspect unchanged code.
#[test]
fn test_unchanged_symbol_in_touched_file_excluded() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    // Both symbols live in the touched file and carry a CHANGED_IN edge at bbbb,
    // but only `edited` actually changed body between aaaa and bbbb.
    let edited_base = symbol_node_body("edited", "src/lib.rs", "aaaaaaaa", "fn edited() { 1 }");
    let edited_head = symbol_node_body("edited", "src/lib.rs", "bbbbbbbb", "fn edited() { 2 }");
    let kept_base = symbol_node_body("untouched", "src/lib.rs", "aaaaaaaa", "fn untouched() {}");
    let kept_head = symbol_node_body("untouched", "src/lib.rs", "bbbbbbbb", "fn untouched() {}");
    let edited_changed = symbol_changed_in_edge("edited", "src/lib.rs", "bbbbbbbb");
    let kept_changed = symbol_changed_in_edge("untouched", "src/lib.rs", "bbbbbbbb");

    let records = vec![
        c1,
        c2,
        e1,
        edited_base,
        edited_head,
        kept_base,
        kept_head,
        edited_changed,
        kept_changed,
    ];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let names: Vec<&str> = ctx.changed_symbols.iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        vec!["edited"],
        "only the symbol whose body changed should be reported"
    );
}

/// A newly introduced symbol (no parent snapshot of the same id) is a real change
/// and must be reported even though the gate cannot compare it to a parent.
#[test]
fn test_new_symbol_in_touched_file_reported() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let added_head = symbol_node_body("added", "src/lib.rs", "bbbbbbbb", "fn added() {}");
    let added_changed = symbol_changed_in_edge("added", "src/lib.rs", "bbbbbbbb");

    let records = vec![c1, c2, e1, added_head, added_changed];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let names: Vec<&str> = ctx.changed_symbols.iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        vec!["added"],
        "a symbol with no parent snapshot is a real change"
    );
}

/// When the only explanation targets the per-commit `Change` node, the matching
/// `File` fact for the same `(path, commit)` must not be reported as unexplained:
/// the output BFS already surfaces the observation via the seeded `Change`, so
/// reporting the file as unexplained would contradict the emitted evidence.
#[test]
fn test_change_evidence_marks_file_explained() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let f1_id = f1.id().to_owned();
    let changed = changed_in_edge("src/lib.rs", "bbbbbbbb");

    let change_id = "node:change:repo_test:bbbbbbbb:M:src/lib.rs";
    let change = GraphRecord::node(
        change_id.to_owned(),
        NodeKind::Change,
        Some("src/lib.rs".to_owned()),
        None,
        Some("M src/lib.rs".to_owned()),
        "Git change M to src/lib.rs".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "bbbbbbbb".to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let obs_id = agent_memory_stable_id(&["obs", "explains_change_unexplained"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Explains the change".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    let explains = GraphRecord::edge(
        EdgeLabel::ExplainsChange,
        obs_id.clone(),
        change_id.to_owned(),
        None,
        "explains".to_owned(),
    );

    let records = vec![c1, c2, e1, f1, changed, change, obs, explains];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let unexplained: std::collections::BTreeSet<&str> =
        ctx.unexplained.iter().map(|u| u.record_id).collect();
    assert!(
        !unexplained.contains(f1_id.as_str()),
        "file explained via its Change node must not be listed as unexplained"
    );

    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|o| o.record_id).collect();
    assert!(
        obs_ids.contains(obs_id.as_str()),
        "the explaining observation must still be surfaced"
    );
}

/// Changed-fact items must serialize bounded identity/path/commit metadata, never
/// the raw `GraphRecord` whose `summary` embeds normalized source bodies.
#[test]
fn test_changed_facts_serialize_bounded_metadata() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    // File and symbol summaries carry a sentinel "source body" that must not leak.
    let mut f1 = file_node("src/lib.rs", "bbbbbbbb");
    if let GraphRecord::Node {
        ref mut summary, ..
    } = f1
    {
        *summary = "Source: SENTINEL_FILE_BODY".to_owned();
    }
    let f1_id = f1.id().to_owned();
    let file_changed = changed_in_edge("src/lib.rs", "bbbbbbbb");

    let sym = symbol_node_body(
        "added",
        "src/lib.rs",
        "bbbbbbbb",
        "Source: SENTINEL_SYMBOL_BODY",
    );
    let sym_id = sym.id().to_owned();
    let sym_changed = symbol_changed_in_edge("added", "src/lib.rs", "bbbbbbbb");

    let records = vec![c1, c2, e1, f1, file_changed, sym, sym_changed];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let json = serde_json::to_string(&ctx).expect("changes context serializes");
    assert!(
        !json.contains("SENTINEL_FILE_BODY") && !json.contains("SENTINEL_SYMBOL_BODY"),
        "raw source bodies must not appear in the serialized output: {json}"
    );

    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let file = &value["changed_files"][0];
    assert_eq!(file["record_id"], serde_json::json!(f1_id));
    assert_eq!(file["path"], serde_json::json!("src/lib.rs"));
    assert_eq!(file["git_commit"], serde_json::json!("bbbbbbbb"));
    assert!(
        file.get("record").is_none(),
        "the raw record must not be serialized"
    );

    let symbol = &value["changed_symbols"][0];
    assert_eq!(symbol["record_id"], serde_json::json!(sym_id));
    assert_eq!(symbol["name"], serde_json::json!("added"));
    assert_eq!(symbol["git_commit"], serde_json::json!("bbbbbbbb"));
}

/// A `CHANGED_IN` edge retracted by an active tombstone must not mark its fact as
/// changed. The output BFS already skips tombstoned edges, so trusting the same
/// edge here would report a fact as changed whose change marker has been revoked.
/// A sibling live `CHANGED_IN` edge keeps the range on the edge-trusting path so
/// the test isolates tombstone handling rather than the commit-membership fallback.
#[test]
fn test_tombstoned_changed_in_edge_excludes_fact() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    // `src/live.rs` has a live CHANGED_IN edge; `src/dead.rs` has one retracted by
    // a tombstone. Only `src/live.rs` should be reported.
    let live = file_node("src/live.rs", "bbbbbbbb");
    let live_changed = changed_in_edge("src/live.rs", "bbbbbbbb");

    let dead = file_node("src/dead.rs", "bbbbbbbb");
    let dead_changed = changed_in_edge("src/dead.rs", "bbbbbbbb");
    let dead_changed_id = dead_changed.id().to_owned();
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:dead_changed".to_owned(),
        schema_version: 4,
        deleted_id: dead_changed_id,
        summary: "Retracted CHANGED_IN edge for src/dead.rs".to_owned(),
        producer: None,
    };

    let records = vec![
        c1,
        c2,
        e1,
        live,
        live_changed,
        dead,
        dead_changed,
        tombstone,
    ];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let paths: Vec<&str> = ctx.changed_files.iter().map(|f| f.path).collect();
    assert_eq!(
        paths,
        vec!["src/live.rs"],
        "a fact whose only CHANGED_IN edge is tombstoned must not be reported"
    );
}

/// Under a `--repo` scope, a sibling repository can carry the same path and the
/// same commit SHA. A triple-only citation (path/commit, no resolved target id)
/// authored by a record owned by the sibling repo must not be surfaced as
/// unresolved context for the selected repo's change.
#[test]
#[allow(clippy::too_many_lines)]
fn test_triple_only_citation_scoped_to_repository() {
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
    // Both repos carry `src/shared.rs` and share SHAs a0aaaaaa -> a1aaaaaa.
    let mut sibling_obs = GraphRecord::node(
        agent_memory_stable_id(&["obs", "sibling_repo_triple"]),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation from sibling repo B".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = sibling_obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: None,
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_FILE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: Some("src/shared.rs".to_owned()),
            target_span: None,
            target_git_commit: Some("a1aaaaaa".to_owned()),
        }]);
    }
    let sibling_obs_id = sibling_obs.id().to_owned();

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
        scoped_file("A", "src/shared.rs", "a1aaaaaa"),
        contains(ra, "node:file:A:src/shared.rs"),
        sibling_obs,
        contains(rb, &sibling_obs_id),
    ];

    let ctx = changes_context(&records, "a0", "a1", Some(ra)).unwrap();
    assert!(
        ctx.unresolved
            .iter()
            .all(|u| u.source_record_id != sibling_obs_id),
        "a triple-only citation owned by a sibling repo must not be surfaced under repo scope"
    );
}

/// When the only in-range `CHANGED_IN` marker for a commit is retracted by a
/// tombstone, the range must still count as using `CHANGED_IN` so the legacy
/// commit-membership fallback stays off. Otherwise the tombstoned File/Symbol
/// snapshot would be reported as changed via the fallback anyway.
#[test]
fn test_tombstoned_only_changed_in_does_not_fall_back() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let f1 = file_node("src/only.rs", "bbbbbbbb");
    let f1_changed = changed_in_edge("src/only.rs", "bbbbbbbb");
    let f1_changed_id = f1_changed.id().to_owned();
    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:only_changed".to_owned(),
        schema_version: 4,
        deleted_id: f1_changed_id,
        summary: "Retracted the only CHANGED_IN edge for src/only.rs".to_owned(),
        producer: None,
    };

    let records = vec![c1, c2, e1, f1, f1_changed, tombstone];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();
    assert!(
        ctx.changed_files.is_empty(),
        "a retracted sole CHANGED_IN edge must not re-enable the commit-membership fallback: {:?}",
        ctx.changed_files.iter().map(|f| f.path).collect::<Vec<_>>()
    );
}

/// A direct evidence link that cites a changed File/Symbol but is anchored to a
/// commit outside the queried range is stale context, not an explanation: history
/// reuses the same stable id across commits. Such a citation must neither be
/// surfaced nor mark the in-range change explained.
#[test]
fn test_direct_evidence_out_of_range_anchor_does_not_explain() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let f1_id = f1.id().to_owned();
    let f1_changed = changed_in_edge("src/lib.rs", "bbbbbbbb");

    let obs_id = agent_memory_stable_id(&["obs", "stale_anchor"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation anchored to an out-of-range commit".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        // Anchored to the base commit aaaaaaaa, which is out of the queried range.
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(f1_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_FILE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: Some("aaaaaaaa".to_owned()),
        }]);
    }

    let records = vec![c1, c2, e1, f1, f1_changed, obs];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let unexplained: std::collections::BTreeSet<&str> =
        ctx.unexplained.iter().map(|u| u.record_id).collect();
    assert!(
        unexplained.contains(f1_id.as_str()),
        "a change explained only by out-of-range-anchored evidence must remain unexplained"
    );
    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|o| o.record_id).collect();
    assert!(
        !obs_ids.contains(obs_id.as_str()),
        "an observation anchored to an out-of-range commit must not be surfaced"
    );
}

/// A Rust-file deletion has no File/Symbol snapshot at the deleted commit and no
/// `CHANGED_IN` edge, so evidence usually cites the deleted file's stable id from an
/// earlier snapshot. The deletion's explaining observation must still be surfaced
/// by bridging the prior path-backed code id into the evidence traversal.
#[test]
fn test_deletion_evidence_surfaced_via_prior_code_id() {
    fn change_node(status: &str, path: &str, commit: &str) -> GraphRecord {
        let id = stable_id(&["node", "change", "repo_test", commit, status, path]);
        GraphRecord::node(
            id,
            NodeKind::Change,
            Some(path.to_owned()),
            None,
            Some(format!("{status} {path}")),
            format!("Git change {status} to {path}"),
        )
        .with_temporal(TemporalMetadata {
            git_commit: commit.to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        })
    }

    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    // Prior snapshot of the file that is deleted at bbbb (no snapshot at bbbb).
    let gone = file_node("src/gone.rs", "aaaaaaaa");
    let gone_id = gone.id().to_owned();
    let deletion = change_node("D", "src/gone.rs", "bbbbbbbb");

    let obs_id = agent_memory_stable_id(&["obs", "explains_deletion"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Explains why src/gone.rs was deleted".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(gone_id),
            target_domain: "codegraph".to_owned(),
            relation: "EXPLAINS_CHANGE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let records = vec![c1, c2, e1, gone, deletion, obs];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    assert!(
        ctx.changed_files.iter().any(|f| f.path == "src/gone.rs"),
        "the deletion must be reported as a changed file"
    );
    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|o| o.record_id).collect();
    assert!(
        obs_ids.contains(obs_id.as_str()),
        "evidence citing the deleted file's prior code id must be surfaced for the deletion"
    );
}

/// Evidence explaining a deletion is normally anchored to the file's prior live
/// commit (the parent of the deletion commit), which is out of the queried range.
/// The in-range anchor filter must still admit such a citation for the bridged
/// deletion target, otherwise the deletion is reported with no evidence.
#[test]
fn test_deletion_evidence_anchored_to_prior_live_commit_surfaced() {
    fn change_node(status: &str, path: &str, commit: &str) -> GraphRecord {
        let id = stable_id(&["node", "change", "repo_test", commit, status, path]);
        GraphRecord::node(
            id,
            NodeKind::Change,
            Some(path.to_owned()),
            None,
            Some(format!("{status} {path}")),
            format!("Git change {status} to {path}"),
        )
        .with_temporal(TemporalMetadata {
            git_commit: commit.to_owned(),
            git_parent_commits: vec!["aaaaaaaa".to_owned()],
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        })
    }

    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let gone = file_node("src/gone.rs", "aaaaaaaa");
    let gone_id = gone.id().to_owned();
    let deletion = change_node("D", "src/gone.rs", "bbbbbbbb");

    let obs_id = agent_memory_stable_id(&["obs", "explains_deletion_anchored"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Explains why src/gone.rs was deleted".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        // Anchored to aaaaaaaa, the prior live commit (out of the range {bbbb}).
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(gone_id),
            target_domain: "codegraph".to_owned(),
            relation: "EXPLAINS_CHANGE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: Some("aaaaaaaa".to_owned()),
        }]);
    }

    let records = vec![c1, c2, e1, gone, deletion, obs];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|o| o.record_id).collect();
    assert!(
        obs_ids.contains(obs_id.as_str()),
        "deletion evidence anchored to the prior live commit must still be surfaced"
    );
}

/// A triple-only citation reached during the evidence BFS (because its source
/// node was already reached through a resolved link) must still be filtered by
/// the same commit-anchor check as the seed-path pass, so a stale citation to an
/// out-of-range version of a changed path is not re-emitted as unresolved.
#[test]
fn test_triple_only_stale_citation_filtered_in_bfs() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let f1_id = f1.id().to_owned();
    let f1_changed = changed_in_edge("src/lib.rs", "bbbbbbbb");

    let obs_id = agent_memory_stable_id(&["obs", "bfs_stale_triple"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Explains the in-range change but also carries a stale triple citation".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![
            // Resolved, in-range link: makes the BFS reach this observation.
            EvidenceLink {
                target_record_id: Some(f1_id),
                target_domain: "codegraph".to_owned(),
                relation: "EXPLAINS_CHANGE".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
            // Triple-only citation to the same path at an out-of-range commit.
            EvidenceLink {
                target_record_id: None,
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_FILE".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: Some("src/lib.rs".to_owned()),
                target_span: None,
                target_git_commit: Some("aaaaaaaa".to_owned()),
            },
        ]);
    }

    let records = vec![c1, c2, e1, f1, f1_changed, obs];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    assert!(
        ctx.unresolved.iter().all(|u| u.source_record_id != obs_id),
        "a stale out-of-range triple citation must not be emitted as unresolved from the BFS"
    );
}

/// A materialized cross-domain evidence edge carries its commit anchor on the
/// edge's temporal metadata. An edge anchored outside the queried range is stale
/// (history reuses the same stable id across commits), so it must not be indexed
/// or traversed — otherwise the change is marked explained by stale evidence even
/// though direct `EvidenceLink` anchors are already filtered.
#[test]
fn test_temporal_evidence_edge_out_of_range_filtered() {
    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let f1_id = f1.id().to_owned();
    let f1_changed = changed_in_edge("src/lib.rs", "bbbbbbbb");

    let obs_id = agent_memory_stable_id(&["obs", "edge_stale_anchor"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation linked via a materialized edge anchored out of range".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    // EXPLAINS_CHANGE edge whose temporal anchor is the base commit (out of range).
    let explains = GraphRecord::edge(
        EdgeLabel::ExplainsChange,
        obs_id.clone(),
        f1_id.clone(),
        None,
        "explains".to_owned(),
    )
    .with_temporal(TemporalMetadata {
        git_commit: "aaaaaaaa".to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: Some("2026-01-01T00:00:00Z".to_owned()),
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    });

    let records = vec![c1, c2, e1, f1, f1_changed, obs, explains];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    let unexplained: std::collections::BTreeSet<&str> =
        ctx.unexplained.iter().map(|u| u.record_id).collect();
    assert!(
        unexplained.contains(f1_id.as_str()),
        "a change explained only by an out-of-range-anchored edge must remain unexplained"
    );
    let obs_ids: std::collections::BTreeSet<&str> =
        ctx.observations.iter().map(|o| o.record_id).collect();
    assert!(
        !obs_ids.contains(obs_id.as_str()),
        "an observation reached only via an out-of-range-anchored edge must not be surfaced"
    );
}

/// A triple-only citation explaining a deletion is anchored to the deleted file's
/// prior live commit (out of range). The seed-path pass must admit such an anchor
/// when the cited path is an in-range deletion path, mirroring the direct-link
/// deletion bridge, so the deletion's unresolved evidence is still surfaced.
#[test]
fn test_triple_only_deletion_prior_commit_citation_surfaced() {
    fn change_node(status: &str, path: &str, commit: &str) -> GraphRecord {
        let id = stable_id(&["node", "change", "repo_test", commit, status, path]);
        GraphRecord::node(
            id,
            NodeKind::Change,
            Some(path.to_owned()),
            None,
            Some(format!("{status} {path}")),
            format!("Git change {status} to {path}"),
        )
        .with_temporal(TemporalMetadata {
            git_commit: commit.to_owned(),
            git_parent_commits: vec!["aaaaaaaa".to_owned()],
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: Some("2026-01-01T00:00:00Z".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        })
    }

    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");

    let gone = file_node("src/gone.rs", "aaaaaaaa");
    let deletion = change_node("D", "src/gone.rs", "bbbbbbbb");

    let obs_id = agent_memory_stable_id(&["obs", "triple_deletion_prior"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Triple-only citation explaining the deletion at the prior live commit".to_owned(),
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
            relation: "EXPLAINS_CHANGE".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: Some("src/gone.rs".to_owned()),
            target_span: None,
            target_git_commit: Some("aaaaaaaa".to_owned()),
        }]);
    }

    let records = vec![c1, c2, e1, gone, deletion, obs];
    let ctx = changes_context(&records, "aaaa", "bbbb", None).unwrap();

    assert!(
        ctx.unresolved.iter().any(|u| u.source_record_id == obs_id),
        "a triple-only deletion citation anchored to the prior live commit must be surfaced"
    );
}

/// Issue #114: every record-shaped section of a `changes` response carries a
/// derived `trust` class from the closed vocabulary.
///
/// The lane's documented guarantee is that a consumer can require the field
/// UNIFORMLY across the whole envelope, so this asserts the serialized JSON —
/// not just the typed struct — for all ten record-shaped sections, including
/// the six (`changed_files`, `changed_symbols`, `commits`, `tombstones`,
/// `drift_records`, `unexplained`) that carry code-graph and semantic facts
/// rather than cross-domain evidence.
#[test]
#[allow(clippy::too_many_lines)]
fn changes_labels_every_record_shaped_section_with_a_trust_class() {
    /// The closed `trust` vocabulary, mirrored from `crate::query::TrustClass`.
    const TRUST_VOCABULARY: &[&str] = &[
        "source_derived",
        "verification_evidence",
        "agent_verified",
        "agent_unverified",
        "agent_contradicted",
        "project_state",
        "artifact",
        "runtime_observation",
        "other",
    ];

    let c1 = commit("aaaaaaaa", &[]);
    let c2 = commit("bbbbbbbb", &["aaaaaaaa"]);
    let c3 = commit("cccccccc", &["bbbbbbbb"]);
    let e1 = parent_edge("aaaaaaaa", "bbbbbbbb");
    let e2 = parent_edge("bbbbbbbb", "cccccccc");

    let f1 = file_node("src/lib.rs", "bbbbbbbb");
    let s1 = symbol_node("hello", "src/lib.rs", "bbbbbbbb");
    let f2 = file_node("src/main.rs", "cccccccc");
    let s2 = symbol_node("main", "src/main.rs", "cccccccc");
    let s1_id = s1.id().to_owned();
    let s2_id = s2.id().to_owned();

    let drift = GraphRecord::node(
        "drift:trust".to_owned(),
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
        target_record_id: s2_id,
        prior_record_id: s1_id.clone(),
        before_git_commit: "bbbbbbbb".to_owned(),
        after_git_commit: "cccccccc".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.5,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    });

    let tombstone = GraphRecord::Tombstone {
        id: "tombstone:trust".to_owned(),
        schema_version: 4,
        deleted_id: "node:symbol:repo_test:src/lib.rs:old_symbol".to_owned(),
        summary: "Invalidated stale cached record from src/lib.rs".to_owned(),
        producer: None,
    };

    // An agent observation citing `hello`, with no verification backing.
    let obs_id = agent_memory_stable_id(&["obs", "trust_obs"]);
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
        ref mut agent_id,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent:1".to_owned());
        *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        *confidence = Some("1.0".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(s1_id),
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
    let ctx = changes_context(&records, "aaaa", "cccc", None).expect("changes context");

    // Serialize: the guarantee is about the wire shape, not the typed struct.
    let json: serde_json::Value = serde_json::to_value(&ctx).expect("serialize changes context");

    let record_sections = [
        "changed_files",
        "changed_symbols",
        "commits",
        "tombstones",
        "drift_records",
        "unexplained",
        "observations",
        "project_state",
        "artifacts",
        "verification_evidence",
    ];
    let mut labelled = 0_usize;
    for section in record_sections {
        for row in json[section].as_array().expect("section is an array") {
            let trust = row["trust"].as_str().unwrap_or_else(|| {
                panic!("row in `{section}` carries no `trust` field: {row}");
            });
            assert!(
                TRUST_VOCABULARY.contains(&trust),
                "row in `{section}` carries `trust` outside the closed vocabulary: {trust}"
            );
            labelled += 1;
        }
    }
    assert!(labelled > 0, "fixture produced no rows to label");

    // The six newly-labelled sections must be non-empty, or this test would
    // pass vacuously on exactly the rows it exists to cover.
    for section in [
        "changed_files",
        "changed_symbols",
        "commits",
        "tombstones",
        "drift_records",
        "unexplained",
    ] {
        assert!(
            !json[section].as_array().expect("array").is_empty(),
            "`{section}` must be populated for this test to prove anything"
        );
    }

    // Classification, not just presence. Code-graph facts, commits, and
    // semantic drift are all deterministic source-derived records; a tombstone
    // is a deletion marker, which carries no truth-bearing class.
    for section in [
        "changed_files",
        "changed_symbols",
        "commits",
        "drift_records",
        "unexplained",
    ] {
        for row in json[section].as_array().expect("array") {
            assert_eq!(
                row["trust"].as_str(),
                Some("source_derived"),
                "`{section}` row should be source_derived: {row}"
            );
        }
    }
    for row in json["tombstones"].as_array().expect("array") {
        assert_eq!(
            row["trust"].as_str(),
            Some("other"),
            "a tombstone is a deletion marker, never a truth-bearing class: {row}"
        );
    }
    // The agent observation cites no verification record, so it is a hypothesis
    // — and must never be labelled with a non-agent class.
    let obs_row = json["observations"]
        .as_array()
        .expect("array")
        .iter()
        .find(|r| r["record_id"].as_str() == Some(obs_id.as_str()))
        .expect("observation present");
    assert_eq!(obs_row["trust"].as_str(), Some("agent_unverified"));
}
