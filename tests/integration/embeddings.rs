#![allow(missing_docs)]

use std::path::PathBuf;

use aletheia_egregore::{
    GraphRecord, NodeKind, TemporalMetadata,
    embeddings::{
        CandidateVector, EmbeddingVectorKey, embedding_candidates, semantic_drift_records,
    },
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
                && record["domain"] == "semantic"
                && record["semantic_drift"]["embedding_model"]["name"] == "fake-code-model"
                && record["semantic_drift"]["embedding_model"]["dim"] == 384
                && record["semantic_drift"]["target_record_id"] == "file:src/lib.rs"
                && record["semantic_drift"]["prior_record_id"] == "file:src/lib.rs"
                && record["semantic_drift"]["before_git_commit"] == "aaaaaaaa"
                && record["semantic_drift"]["after_git_commit"] == "bbbbbbbb"
                && record["semantic_drift"]["metric_kind"] == "cosine_distance"
                && record["semantic_drift"]["score"] == 1.0
                && record["semantic_drift"]["selection_threshold"] == 0.5
                && record["semantic_drift"]["selection_basis"] == "threshold_only"
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
    assert!(
        json.iter().any(|record| {
            record["record_type"] == "edge"
                && record["label"] == "DRIFTS_PRIOR"
                && record["target"] == "file:src/lib.rs"
        }),
        "missing DRIFTS_PRIOR edge"
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

#[test]
fn embedding_vector_keys_preserve_per_commit_observations_for_stable_record_ids() {
    let before = GraphRecord::node(
        "symbol:stable".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("stable".to_owned()),
        "Rust function stable computes the old behavior".to_owned(),
    )
    .with_temporal(temporal("aaaaaaaa", "2026-01-01T00:00:00Z"));
    let after = GraphRecord::node(
        "symbol:stable".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("stable".to_owned()),
        "Rust function stable computes the new behavior".to_owned(),
    )
    .with_temporal(temporal("bbbbbbbb", "2026-01-02T00:00:00Z"));

    let candidates = embedding_candidates(&[before, after]);
    let keys = candidates
        .iter()
        .map(EmbeddingVectorKey::from_candidate)
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(
        keys.len(),
        2,
        "same stable record_id must still retain one embedding key per commit observation"
    );
}

// ---------------------------------------------------------------------------
// RED: issue #91 — memory-kind embedding candidates
// ---------------------------------------------------------------------------

#[test]
fn embedding_candidates_include_agent_memory_observations() {
    use aletheia_egregore::agent_memory_stable_id;
    use aletheia_egregore::ir::AGENT_MEMORY_SCHEMA_VERSION;
    let obs_id = agent_memory_stable_id(&["obs", "test_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation about error handling".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut text,
        ref mut schema_version,
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut observed_at,
        ..
    } = obs
    {
        *text = Some("prefer thiserror in libraries for structured errors".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some("agent_1".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-01T00:00:00Z".to_owned());
    }

    let candidates = embedding_candidates(&[obs]);
    assert!(
        candidates.iter().any(|c| c.record_id == obs_id),
        "Observation nodes must become embedding candidates (issue #91)"
    );
    assert!(
        candidates
            .iter()
            .all(|c| c.target != "file" || c.record_id != obs_id),
        "Observation must not be classified as a code 'file' target"
    );
}

#[test]
fn embedding_candidates_include_agent_memory_decisions() {
    use aletheia_egregore::agent_memory_stable_id;
    use aletheia_egregore::ir::AGENT_MEMORY_SCHEMA_VERSION;
    let dec_id = agent_memory_stable_id(&["decision", "test_dec"]);
    let mut dec = GraphRecord::node(
        dec_id.clone(),
        NodeKind::Decision,
        None,
        None,
        None,
        "Decision on crate structure".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut text,
        ref mut schema_version,
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut observed_at,
        ref mut confidence,
        ..
    } = dec
    {
        *text = Some("we decided to use a workspace layout with a single binary crate".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some("agent_1".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-01T00:00:00Z".to_owned());
        *confidence = Some("0.95".to_owned());
    }

    let candidates = embedding_candidates(&[dec]);
    assert!(
        candidates.iter().any(|c| c.record_id == dec_id),
        "Decision nodes must become embedding candidates (issue #91)"
    );
}

#[test]
fn embedding_candidates_include_agent_memory_failures() {
    use aletheia_egregore::agent_memory_stable_id;
    use aletheia_egregore::ir::AGENT_MEMORY_SCHEMA_VERSION;
    let fail_id = agent_memory_stable_id(&["failure", "test_fail"]);
    let mut fail_node = GraphRecord::node(
        fail_id.clone(),
        NodeKind::Failure,
        None,
        None,
        None,
        "Failure: parser panics on empty input".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut text,
        ref mut schema_version,
        agent_id: ref mut aid,
        session_id: ref mut sid,
        ref mut observed_at,
        ..
    } = fail_node
    {
        *text = Some("the JSON parser panics on empty input with index out of range".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some("agent_1".to_owned());
        *sid = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-01T00:00:00Z".to_owned());
    }

    let candidates = embedding_candidates(&[fail_node]);
    assert!(
        candidates.iter().any(|c| c.record_id == fail_id),
        "Failure nodes must become embedding candidates (issue #91)"
    );
}

#[test]
fn embedding_candidates_skip_memory_records_with_empty_text() {
    use aletheia_egregore::agent_memory_stable_id;
    use aletheia_egregore::ir::AGENT_MEMORY_SCHEMA_VERSION;
    let obs_id = agent_memory_stable_id(&["obs", "no_text_obs"]);
    let mut obs = GraphRecord::node(
        obs_id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation with no text body".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        agent_id: ref mut aid,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some("agent_1".to_owned());
        // text intentionally left as None
    }

    let candidates = embedding_candidates(&[obs]);
    assert!(
        candidates.iter().all(|c| c.record_id != obs_id),
        "Observation without text must not be an embedding candidate (no meaningful content)"
    );
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
