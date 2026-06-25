//! Integration tests for `eg query lifeline` (issue #96).
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, TemporalMetadata,
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
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

fn temporal_with_parents(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|&s| s.to_owned()).collect(),
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
    .with_domain("semantic", 1)
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

#[allow(clippy::too_many_lines)]
fn fixture_graph() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let graph_path = temp.path().join("graph.jsonl");

    let repo_id = "repo:test";
    let symbol_id = "symbol:answer";

    let repo_node = GraphRecord::node(
        repo_id.to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("test-repo".to_owned()),
        "Test repository".to_owned(),
    );

    let c1 = GraphRecord::node(
        "commit:1".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("c1".to_owned()),
        "commit 1".to_owned(),
    )
    .with_temporal(temporal("c1", "2026-01-01T00:00:00Z"));
    let c2 = GraphRecord::node(
        "commit:2".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("c2".to_owned()),
        "commit 2".to_owned(),
    )
    .with_temporal(temporal_with_parents("c2", &["c1"], "2026-01-02T00:00:00Z"));
    let c3 = GraphRecord::node(
        "commit:3".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("c3".to_owned()),
        "commit 3".to_owned(),
    )
    .with_temporal(temporal_with_parents("c3", &["c2"], "2026-01-03T00:00:00Z"));
    let c4 = GraphRecord::node(
        "commit:4".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("c4".to_owned()),
        "commit 4".to_owned(),
    )
    .with_temporal(temporal_with_parents("c4", &["c3"], "2026-01-04T00:00:00Z"));
    let c5 = GraphRecord::node(
        "commit:5".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("c5".to_owned()),
        "commit 5".to_owned(),
    )
    .with_temporal(temporal_with_parents("c5", &["c4"], "2026-01-05T00:00:00Z"));
    let c6 = GraphRecord::node(
        "commit:6".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("c6".to_owned()),
        "commit 6".to_owned(),
    )
    .with_temporal(temporal_with_parents("c6", &["c5"], "2026-01-06T00:00:00Z"));
    let c7 = GraphRecord::node(
        "commit:7".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("c7".to_owned()),
        "commit 7".to_owned(),
    )
    .with_temporal(temporal_with_parents("c7", &["c6"], "2026-01-07T00:00:00Z"));

    let e_repo_c1 = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        "commit:1".to_owned(),
        None,
        "repo contains c1".to_owned(),
    );
    let e_repo_c2 = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        "commit:2".to_owned(),
        None,
        "repo contains c2".to_owned(),
    );
    let e_repo_c3 = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        "commit:3".to_owned(),
        None,
        "repo contains c3".to_owned(),
    );
    let e_repo_c4 = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        "commit:4".to_owned(),
        None,
        "repo contains c4".to_owned(),
    );
    let e_repo_c5 = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        "commit:5".to_owned(),
        None,
        "repo contains c5".to_owned(),
    );
    let e_repo_c6 = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        "commit:6".to_owned(),
        None,
        "repo contains c6".to_owned(),
    );
    let e_repo_c7 = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.to_owned(),
        "commit:7".to_owned(),
        None,
        "repo contains c7".to_owned(),
    );

    // C1: introduced
    let s1 = GraphRecord::node(
        symbol_id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "answer body 1".to_owned(),
    )
    .with_temporal(temporal("c1", "2026-01-01T00:00:00Z"));
    let e_c1_s1 = GraphRecord::edge(
        EdgeLabel::Contains,
        "commit:1".to_owned(),
        symbol_id.to_owned(),
        None,
        String::new(),
    );

    // C2: modified
    let s2 = GraphRecord::node(
        symbol_id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "answer body 2".to_owned(),
    )
    .with_temporal(temporal_with_parents("c2", &["c1"], "2026-01-02T00:00:00Z"));
    let e_c2_s2 = GraphRecord::edge(
        EdgeLabel::Contains,
        "commit:2".to_owned(),
        symbol_id.to_owned(),
        None,
        String::new(),
    );

    // C3: modified (drift)
    let s3 = GraphRecord::node(
        symbol_id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "answer body 2".to_owned(),
    )
    .with_temporal(temporal_with_parents("c3", &["c2"], "2026-01-03T00:00:00Z"));
    let e_c3_s3 = GraphRecord::edge(
        EdgeLabel::Contains,
        "commit:3".to_owned(),
        symbol_id.to_owned(),
        None,
        String::new(),
    );
    let drift_node = drift("drift:c3", "answer", "c2", "c3", 0.75);

    // C4: unchanged
    let s4 = GraphRecord::node(
        symbol_id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "answer body 2".to_owned(),
    )
    .with_temporal(temporal_with_parents("c4", &["c3"], "2026-01-04T00:00:00Z"));
    let e_c4_s4 = GraphRecord::edge(
        EdgeLabel::Contains,
        "commit:4".to_owned(),
        symbol_id.to_owned(),
        None,
        String::new(),
    );

    // C5: removed

    // C7: reintroduced
    let s7 = GraphRecord::node(
        symbol_id.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(5, 8)),
        Some("answer".to_owned()),
        "answer body 3".to_owned(),
    )
    .with_temporal(temporal_with_parents("c7", &["c6"], "2026-01-07T00:00:00Z"));
    let e_c7_s7 = GraphRecord::edge(
        EdgeLabel::Contains,
        "commit:7".to_owned(),
        symbol_id.to_owned(),
        None,
        String::new(),
    );

    // Add unrelated same-name symbol in different module/file to verify zero bleed
    let unrelated_id = "symbol:other_answer";
    let unrelated_c2 = GraphRecord::node(
        unrelated_id.to_owned(),
        NodeKind::Symbol,
        Some("src/other.rs".to_owned()),
        Some(span(1, 2)),
        Some("answer".to_owned()),
        "other body".to_owned(),
    )
    .with_temporal(temporal_with_parents("c2", &["c1"], "2026-01-02T00:00:00Z"));
    let e_c2_unrelated = GraphRecord::edge(
        EdgeLabel::Contains,
        "commit:2".to_owned(),
        unrelated_id.to_owned(),
        None,
        String::new(),
    );

    // Sibling Repository with same-name symbol
    let repo_sibling_id = "repo:sibling";
    let repo_sibling = GraphRecord::node(
        repo_sibling_id.to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("sibling-repo".to_owned()),
        "Sibling repository".to_owned(),
    );
    let sib_commit_c1 = GraphRecord::node(
        "commit:sib1".to_owned(),
        NodeKind::Commit,
        None,
        None,
        Some("sib1".to_owned()),
        "sibling commit 1".to_owned(),
    )
    .with_temporal(temporal("sib1", "2026-01-01T00:00:00Z"));
    let e_repo_sib = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_sibling_id.to_owned(),
        "commit:sib1".to_owned(),
        None,
        String::new(),
    );
    let sib_symbol_s1 = GraphRecord::node(
        "symbol:sibling_answer".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        Some(span(1, 4)),
        Some("answer".to_owned()),
        "sibling body".to_owned(),
    )
    .with_temporal(temporal("sib1", "2026-01-01T00:00:00Z"));
    let e_sib_c1_s1 = GraphRecord::edge(
        EdgeLabel::Contains,
        "commit:sib1".to_owned(),
        "symbol:sibling_answer".to_owned(),
        None,
        String::new(),
    );

    let mut obs_node = GraphRecord::node(
        "obs:test_ignored".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Test observation to be ignored".to_owned(),
    );
    if let GraphRecord::Node { ref mut schema_version, .. } = obs_node {
        *schema_version = 1;
    }
    let mut task_node = GraphRecord::node(
        "task:test_ignored".to_owned(),
        NodeKind::Task,
        None,
        None,
        None,
        "Test task to be ignored".to_owned(),
    );
    if let GraphRecord::Node { ref mut schema_version, .. } = task_node {
        *schema_version = 1;
    }

    let records = vec![
        repo_node,
        c1,
        c2,
        c3,
        c4,
        c5,
        c6,
        c7,
        e_repo_c1,
        e_repo_c2,
        e_repo_c3,
        e_repo_c4,
        e_repo_c5,
        e_repo_c6,
        e_repo_c7,
        s1,
        e_c1_s1,
        s2,
        e_c2_s2,
        s3,
        e_c3_s3,
        drift_node,
        s4,
        e_c4_s4,
        s7,
        e_c7_s7,
        unrelated_c2,
        e_c2_unrelated,
        repo_sibling,
        sib_commit_c1,
        e_repo_sib,
        sib_symbol_s1,
        e_sib_c1_s1,
        obs_node,
        task_node,
    ];

    let content = records
        .into_iter()
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&graph_path, content).unwrap();

    (temp, graph_path)
}

#[test]
fn test_cli_lifeline_happy_path_json() {
    let (_temp, graph) = fixture_graph();

    let output = egregore()
        .args(["query", "lifeline", "--graph"])
        .arg(&graph)
        .arg("symbol:answer")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["ok"], true);

    let result = parsed["result"].as_array().unwrap();
    assert_eq!(result.len(), 5);

    assert_eq!(result[0]["event_type"], "introduced");
    assert_eq!(result[0]["commit"], "c1");

    assert_eq!(result[1]["event_type"], "modified");
    assert_eq!(result[1]["commit"], "c2");

    assert_eq!(result[2]["event_type"], "modified");
    assert_eq!(result[2]["commit"], "c3");
    assert_eq!(result[2]["drift_record_id"], "drift:c3");
    assert_eq!(result[2]["drift_score"], 0.75);

    assert_eq!(result[3]["event_type"], "removed");
    assert_eq!(result[3]["commit"], "c5");
    assert!(result[3]["repo_relative_path"].is_null());
    assert_eq!(result[3]["absent_span_reason"], "tombstone");

    assert_eq!(result[4]["event_type"], "reintroduced");
    assert_eq!(result[4]["commit"], "c7");
}

#[test]
fn test_cli_lifeline_happy_path_text() {
    let (_temp, graph) = fixture_graph();

    let output = egregore()
        .args(["query", "lifeline", "--graph"])
        .arg(&graph)
        .arg("symbol:answer")
        .args(["--format", "text"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    assert!(stdout.contains("Advisory temporal facts: where and when this symbol changed"));
    assert!(
        stdout.contains(
            "[introduced] commit=c1 record_id=symbol:answer @ src/lib.rs:1-4 drift=absent"
        )
    );
    assert!(
        stdout
            .contains("[modified] commit=c2 record_id=symbol:answer @ src/lib.rs:1-4 drift=absent")
    );
    assert!(stdout.contains(
        "[modified] commit=c3 record_id=symbol:answer @ src/lib.rs:1-4 drift=0.7500 (drift:c3)"
    ));
    assert!(stdout.contains("[removed] commit=c5 record_id="));
    assert!(stdout.contains(
        "[reintroduced] commit=c7 record_id=symbol:answer @ src/lib.rs:5-8 drift=absent"
    ));
}

#[test]
fn test_cli_lifeline_errors() {
    let (_temp, graph) = fixture_graph();

    // Unknown symbol (exit code 5)
    let output_unknown = egregore()
        .args(["query", "lifeline", "--graph"])
        .arg(&graph)
        .arg("nonexistent")
        .assert()
        .code(5)
        .get_output()
        .stdout
        .clone();

    let stdout_u = String::from_utf8(output_unknown).unwrap();
    let parsed_u: serde_json::Value = serde_json::from_str(stdout_u.trim()).unwrap();
    assert_eq!(parsed_u["ok"], false);
    assert_eq!(parsed_u["error"]["code"], "unknown_symbol");

    // Ambiguous symbol (exit code 6)
    let output_ambig = egregore()
        .args(["query", "lifeline", "--graph"])
        .arg(&graph)
        .arg("answer")
        .assert()
        .code(6)
        .get_output()
        .stdout
        .clone();

    let stdout_a = String::from_utf8(output_ambig).unwrap();
    let parsed_a: serde_json::Value = serde_json::from_str(stdout_a.trim()).unwrap();
    assert_eq!(parsed_a["ok"], false);
    assert_eq!(parsed_a["error"]["code"], "ambiguous_symbol");
    let candidates = parsed_a["error"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3); // symbol:answer, symbol:other_answer, symbol:sibling_answer
}

#[test]
fn test_cli_lifeline_repo_scoping() {
    let (_temp, graph) = fixture_graph();

    // Query ambiguous name "answer" scoped to repo sibling -> resolves to 1 candidate, happy path!
    let output = egregore()
        .args(["query", "lifeline", "--graph"])
        .arg(&graph)
        .arg("answer")
        .args(["--repo", "sibling-repo"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["ok"], true);

    let result = parsed["result"].as_array().unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["record_id"], "symbol:sibling_answer");
    assert_eq!(result[0]["commit"], "sib1");
}

#[test]
fn test_cli_lifeline_determinism() {
    let (_temp, graph) = fixture_graph();

    let run_query = || {
        egregore()
            .args(["query", "lifeline", "--graph"])
            .arg(&graph)
            .arg("symbol:answer")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    let first = run_query();
    for _ in 0..4 {
        let current = run_query();
        assert_eq!(
            first, current,
            "lifeline query output must be byte-identical across runs"
        );
    }
}
