//! Integration tests for `eg query subsystem <prefix>` (issue #83).
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, TemporalMetadata,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION,
        SEMANTIC_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        artifact_stable_id, project_stable_id, semantic_stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

const fn span(start_line: usize, end_line: usize) -> aletheia_egregore::SourceSpan {
    aletheia_egregore::SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn temporal(commit: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: vec![],
        valid_time: "2026-01-01T00:00:00Z".to_owned(),
        author_time: None,
        observed_at: "2026-01-01T00:00:00Z".to_owned(),
        valid_time_source: None,
    }
}

/// Build a seeded JSONL fixture with:
/// - `src/alpha/` and `src/beta/` each with two symbols
/// - `src/alphabet/` with one symbol (for bleed tests)
/// - one `Observation`, `Task`, `Artifact`, `Verification`, `SemanticDrift` — all under `src/alpha/`
///
/// Returns (`TempDir`, `graph_path`). Caller must keep `TempDir` alive.
#[allow(clippy::too_many_lines)]
fn fixture_subsystem_seeded() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("subsystem.jsonl");

    // src/alpha/ records
    let alpha_file_id = "file:sub:alpha_a".to_owned();
    let alpha_sym1_id = "codegraph:v4:intsub_alpha_sym001".to_owned();
    let alpha_sym2_id = "codegraph:v4:intsub_alpha_sym002".to_owned();

    let alpha_file = GraphRecord::node(
        alpha_file_id,
        NodeKind::File,
        Some("src/alpha/a.rs".to_owned()),
        None,
        Some("src/alpha/a.rs".to_owned()),
        "file src/alpha/a.rs".to_owned(),
    );
    let alpha_sym1 = GraphRecord::symbol(
        alpha_sym1_id.clone(),
        "fn",
        "src/alpha/a.rs".to_owned(),
        span(1, 10),
        "alpha_fn1".to_owned(),
        "fn alpha_fn1 in src/alpha/a.rs".to_owned(),
    );
    let alpha_sym2 = GraphRecord::symbol(
        alpha_sym2_id,
        "fn",
        "src/alpha/b.rs".to_owned(),
        span(1, 10),
        "alpha_fn2".to_owned(),
        "fn alpha_fn2 in src/alpha/b.rs".to_owned(),
    );

    // src/beta/ records
    let beta_file_id = "file:sub:beta_a".to_owned();
    let beta_sym1_id = "codegraph:v4:intsub_beta_sym001".to_owned();
    let beta_sym2_id = "codegraph:v4:intsub_beta_sym002".to_owned();

    let beta_file = GraphRecord::node(
        beta_file_id,
        NodeKind::File,
        Some("src/beta/a.rs".to_owned()),
        None,
        Some("src/beta/a.rs".to_owned()),
        "file src/beta/a.rs".to_owned(),
    );
    let beta_sym1 = GraphRecord::symbol(
        beta_sym1_id,
        "fn",
        "src/beta/a.rs".to_owned(),
        span(1, 10),
        "beta_fn1".to_owned(),
        "fn beta_fn1 in src/beta/a.rs".to_owned(),
    );
    let beta_sym2 = GraphRecord::symbol(
        beta_sym2_id,
        "fn",
        "src/beta/b.rs".to_owned(),
        span(1, 10),
        "beta_fn2".to_owned(),
        "fn beta_fn2 in src/beta/b.rs".to_owned(),
    );

    // src/alphabet/ — sibling for bleed test
    let alphabet_sym_id = "codegraph:v4:intsub_alphabet_sym001".to_owned();
    let alphabet_sym = GraphRecord::symbol(
        alphabet_sym_id,
        "fn",
        "src/alphabet/a.rs".to_owned(),
        span(1, 5),
        "alphabet_fn".to_owned(),
        "fn alphabet_fn in src/alphabet/a.rs".to_owned(),
    );

    // Cross-domain records linked to src/alpha
    let obs_id = agent_memory_stable_id(&["obs", "intsub_obs001"]);
    let mut obs = GraphRecord::node(
        obs_id,
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation about alpha_fn1".to_owned(),
    );
    if let GraphRecord::Node {
        evidence_links: ref mut el,
        schema_version: ref mut sv,
        ..
    } = obs
    {
        *el = Some(vec![EvidenceLink {
            target_record_id: Some(alpha_sym1_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
        *sv = AGENT_MEMORY_SCHEMA_VERSION;
    }

    let task_id = project_stable_id(&["task", "intsub_t001"]);
    let mut task = GraphRecord::node(
        task_id,
        NodeKind::Task,
        None,
        None,
        Some("Refactor alpha_fn1".to_owned()),
        "Task: Refactor alpha_fn1".to_owned(),
    );
    if let GraphRecord::Node {
        evidence_links: ref mut el,
        schema_version: ref mut sv,
        ..
    } = task
    {
        *sv = PROJECT_SCHEMA_VERSION;
        *el = Some(vec![EvidenceLink {
            target_record_id: Some(alpha_sym1_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "REFERENCES_TASK".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let artifact_id = artifact_stable_id(&["artifact", "intsub_a001"]);
    let mut artifact = GraphRecord::node(
        artifact_id,
        NodeKind::Artifact,
        None,
        None,
        Some("alpha patch".to_owned()),
        "Artifact: alpha patch".to_owned(),
    );
    if let GraphRecord::Node {
        evidence_links: ref mut el,
        schema_version: ref mut sv,
        ..
    } = artifact
    {
        *sv = ARTIFACT_SCHEMA_VERSION;
        *el = Some(vec![EvidenceLink {
            target_record_id: Some(alpha_sym1_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "RELATES_TO".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let ver_id = verification_stable_id(&["intsub_ver001"]);
    let mut verification = GraphRecord::node(
        ver_id,
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification for alpha_fn1".to_owned(),
    );
    if let GraphRecord::Node {
        evidence_links: ref mut el,
        schema_version: ref mut sv,
        ..
    } = verification
    {
        *el = Some(vec![EvidenceLink {
            target_record_id: Some(alpha_sym1_id.clone()),
            target_domain: "codegraph".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
        *sv = VERIFICATION_SCHEMA_VERSION;
    }

    let drift_id = semantic_stable_id(&["drift", "intsub_drift001"]);
    let drift_node = {
        let mut n = GraphRecord::node(
            drift_id.clone(),
            NodeKind::SemanticDrift,
            Some("src/alpha/a.rs".to_owned()),
            None,
            None,
            "Semantic drift for alpha_fn1".to_owned(),
        )
        .with_temporal(temporal("aabbccdd"))
        .with_semantic_drift(SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "test".to_owned(),
                name: "model".to_owned(),
                version: "v1".to_owned(),
                dim: 384,
                content_hash: "hash".to_owned(),
            },
            target_record_id: alpha_sym1_id.clone(),
            prior_record_id: alpha_sym1_id.clone(),
            before_git_commit: "00000000".to_owned(),
            after_git_commit: "aabbccdd".to_owned(),
            before_valid_time: "2025-12-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-01-01T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.55,
            selection_threshold: 0.2,
            selection_basis: SelectionBasis::ThresholdOnly,
        });
        if let GraphRecord::Node {
            schema_version: ref mut sv,
            ..
        } = n
        {
            *sv = SEMANTIC_SCHEMA_VERSION;
        }
        n
    };
    let drift_edge = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id,
        alpha_sym1_id,
        Some("1.0".to_owned()),
        "drifts from alpha_fn1".to_owned(),
    );

    let mut graph = Graph::new();
    for r in vec![
        alpha_file,
        alpha_sym1,
        alpha_sym2,
        beta_file,
        beta_sym1,
        beta_sym2,
        alphabet_sym,
        obs,
        task,
        artifact,
        verification,
        drift_node,
        drift_edge,
    ] {
        graph.push(r);
    }

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[test]
fn query_subsystem_exits_0_and_returns_json_for_known_prefix() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON output");
    assert_eq!(parsed["ok"], true, "ok must be true on success");
}

#[test]
fn query_subsystem_source_facts_contain_alpha_not_beta() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let facts = parsed["source_facts"]
        .as_array()
        .expect("source_facts array");
    assert!(!facts.is_empty(), "source_facts must not be empty");

    // All file paths must be under src/alpha/
    for fact in facts {
        if let Some(path) = fact["repo_relative_path"].as_str() {
            assert!(
                path.starts_with("src/alpha"),
                "source_fact path {path:?} must be under src/alpha"
            );
        }
    }

    // Beta and alphabet must not appear
    let stdout_lower = stdout.to_lowercase();
    assert!(
        !stdout_lower.contains("beta_fn"),
        "beta symbols must not appear in src/alpha subsystem output"
    );
    assert!(
        !stdout_lower.contains("src/beta"),
        "src/beta paths must not appear in src/alpha subsystem output"
    );
}

#[test]
fn query_subsystem_no_prefix_bleed_to_alphabet_sibling() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    // src/alphabet must not appear at all
    assert!(
        !stdout.contains("src/alphabet"),
        "src/alphabet must not appear in src/alpha results (no prefix bleed, AC3)"
    );
}

#[test]
fn query_subsystem_trailing_slash_equals_bare_form() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let out_bare = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let out_slash = egregore()
        .args(["query", "subsystem", "src/alpha/", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        out_bare, out_slash,
        "trailing-slash form must produce byte-identical output (AC2)"
    );
}

#[test]
fn query_subsystem_unknown_prefix_exits_2_with_no_match_json() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "src/nonexistent", "--graph"])
        .arg(&graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("no_match must be valid JSON");
    assert_eq!(parsed["ok"], false, "ok must be false for no-match");
    assert_eq!(
        parsed["error"]["code"], "no_match",
        "error code must be no_match"
    );
}

#[test]
fn query_subsystem_empty_prefix_exits_1_with_malformed_json() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("malformed must be valid JSON");
    assert_eq!(parsed["ok"], false, "ok must be false for malformed prefix");
    assert_eq!(
        parsed["error"]["code"], "malformed_prefix",
        "error code must be malformed_prefix"
    );
}

#[test]
fn query_subsystem_slash_only_prefix_exits_1_with_malformed_json() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "/", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("malformed must be valid JSON");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"], "malformed_prefix");
}

#[test]
fn query_subsystem_output_includes_all_trust_sections() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert!(
        parsed["source_facts"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "source_facts must be present and non-empty"
    );
    assert!(
        parsed["observations"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "observations must be present and non-empty"
    );
    assert!(
        parsed["project_state"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "project_state must be present and non-empty"
    );
    assert!(
        parsed["artifacts"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "artifacts must be present and non-empty"
    );
    assert!(
        parsed["verification_evidence"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "verification_evidence must be present and non-empty"
    );
    assert!(
        parsed["semantic_drift"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "semantic_drift must be present and non-empty (AC5)"
    );
}

#[test]
fn query_subsystem_source_facts_carry_record_id_and_path() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let facts = parsed["source_facts"]
        .as_array()
        .expect("source_facts array");
    for fact in facts {
        assert!(
            fact["record_id"].as_str().is_some(),
            "every source_fact must carry record_id (AC6)"
        );
        // Must have at least one of path or span/commit
        let has_path = fact["repo_relative_path"].as_str().is_some();
        let has_commit = fact["git_commit"].as_str().is_some();
        assert!(
            has_path || has_commit,
            "every source_fact must carry repo_relative_path or git_commit (AC6)"
        );
    }
}

#[test]
fn query_subsystem_observations_carry_provenance() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let output = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    let observations = parsed["observations"]
        .as_array()
        .expect("observations array");
    assert!(
        !observations.is_empty(),
        "must have at least one observation"
    );
    for obs in observations {
        assert!(
            obs["record_id"].as_str().is_some(),
            "every observation must carry record_id (AC6)"
        );
    }
}

#[test]
fn query_subsystem_output_is_deterministic() {
    let (_temp, graph) = fixture_subsystem_seeded();

    let out_a = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let out_b = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        out_a, out_b,
        "identical query must produce byte-identical output (AC7)"
    );
}
