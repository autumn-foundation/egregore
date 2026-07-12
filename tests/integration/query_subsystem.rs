//! Integration tests for `eg query subsystem <prefix>` (issue #83).
#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, ErrorSignaturePayload, EvidenceLink, GraphRecord,
    LOG_SCHEMA_VERSION, LogPayload, MetricKind, NodeKind, SCHEMA_VERSION, SelectionBasis,
    SemanticDriftMetadata, TemporalMetadata,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, FrameResolution, Graph,
        PROJECT_SCHEMA_VERSION, SEMANTIC_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
        agent_memory_stable_id, artifact_stable_id, project_stable_id, semantic_stable_id,
        verification_stable_id,
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

// ---------------------------------------------------------------------------
// log_signatures section (issue #325)
// ---------------------------------------------------------------------------

/// Build an `ErrorSignature` log node.
fn error_signature(
    id: &str,
    severity: &str,
    occurrence_count: u64,
    first_seen: &str,
    last_seen: &str,
) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::ErrorSignature,
        None,
        None,
        Some(format!("{severity} signature")),
        format!("Error signature ({severity}) x{occurrence_count}"),
    )
    .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
        fingerprint_algorithm: "template-v1".to_owned(),
        template_excerpt: format!("redacted {severity} template for {id}"),
        severity: severity.to_owned(),
        occurrence_count,
        first_seen: first_seen.to_owned(),
        last_seen: last_seen.to_owned(),
        frames: None,
    }))
    .with_domain("log", LOG_SCHEMA_VERSION)
}

/// A `FRAME_RESOLVES_TO` edge carrying a resolution class and frame index.
fn frame_edge(
    signature_id: &str,
    target_id: &str,
    resolution: FrameResolution,
    frame_index: u32,
) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::FrameResolvesTo,
        signature_id.to_owned(),
        target_id.to_owned(),
        Some("1.0".to_owned()),
        format!("frame {frame_index} resolves ({}) ", resolution.as_str()),
    )
    .with_frame_resolution(resolution)
    .with_frame_index(frame_index)
}

/// Build a fixture with runtime log signatures resolving into `src/alpha`,
/// a sibling `src/alphabet`, and an `unresolved`-frames-only signature.
///
/// Returns (`TempDir`, `graph_path`). Caller must keep `TempDir` alive.
fn fixture_subsystem_with_logs() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("subsystem-logs.jsonl");

    // Code seeds under src/alpha and the src/alphabet bleed sibling.
    let alpha_sym_id = "codegraph:v4:intsublog_alpha_sym001".to_owned();
    let alpha_file = GraphRecord::node(
        "file:sublog:alpha_a".to_owned(),
        NodeKind::File,
        Some("src/alpha/a.rs".to_owned()),
        None,
        Some("src/alpha/a.rs".to_owned()),
        "file src/alpha/a.rs".to_owned(),
    );
    let alpha_sym = GraphRecord::symbol(
        alpha_sym_id.clone(),
        "fn",
        "src/alpha/a.rs".to_owned(),
        span(1, 20),
        "alpha_handler".to_owned(),
        "fn alpha_handler in src/alpha/a.rs".to_owned(),
    );
    let alphabet_sym_id = "codegraph:v4:intsublog_alphabet_sym001".to_owned();
    let alphabet_sym = GraphRecord::symbol(
        alphabet_sym_id.clone(),
        "fn",
        "src/alphabet/a.rs".to_owned(),
        span(1, 5),
        "alphabet_handler".to_owned(),
        "fn alphabet_handler in src/alphabet/a.rs".to_owned(),
    );

    // (a) In-prefix resolved signature, with a duplicate record to exercise
    // stable-ID coalescing (earliest first_seen, latest last_seen, summed count).
    let sig_alpha_id = "log:v1:sig_alpha";
    let sig_alpha_1 = error_signature(
        sig_alpha_id,
        "fatal",
        7,
        "2026-01-02T00:00:00Z",
        "2026-01-03T00:00:00Z",
    );
    let sig_alpha_2 = error_signature(
        sig_alpha_id,
        "fatal",
        5,
        "2026-01-01T00:00:00Z",
        "2026-01-04T00:00:00Z",
    );
    let sig_alpha_edge = frame_edge(sig_alpha_id, &alpha_sym_id, FrameResolution::Resolved, 0);

    // (b) Sibling signature resolving into src/alphabet (bleed guard).
    let sig_alphabet_id = "log:v1:sig_alphabet";
    let sig_alphabet = error_signature(
        sig_alphabet_id,
        "error",
        3,
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00Z",
    );
    let sig_alphabet_edge = frame_edge(
        sig_alphabet_id,
        &alphabet_sym_id,
        FrameResolution::Resolved,
        0,
    );

    // (c) Unresolved-frames-only signature: its dangling diagnostic target sits
    // under src/alpha and must surface in `unresolved`, never in log_signatures.
    let diag_id = "log:v1:diag_alpha_missing".to_owned();
    let diag = GraphRecord::node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        Some("src/alpha/missing.rs".to_owned()),
        None,
        None,
        "unresolved frame diagnostic".to_owned(),
    )
    .with_domain("log", LOG_SCHEMA_VERSION);
    let sig_unres_id = "log:v1:sig_unresolved";
    let sig_unres = error_signature(
        sig_unres_id,
        "warn",
        2,
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00Z",
    );
    let sig_unres_edge = frame_edge(sig_unres_id, &diag_id, FrameResolution::Unresolved, 0);

    let mut graph = Graph::new();
    for r in vec![
        alpha_file,
        alpha_sym,
        alphabet_sym,
        sig_alpha_1,
        sig_alpha_2,
        sig_alpha_edge,
        sig_alphabet,
        sig_alphabet_edge,
        diag,
        sig_unres,
        sig_unres_edge,
    ] {
        graph.push(r);
    }

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

fn run_subsystem(prefix: &str, graph: &PathBuf) -> serde_json::Value {
    let output = egregore()
        .args(["query", "subsystem", prefix, "--graph"])
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    serde_json::from_str(stdout.trim()).expect("valid JSON")
}

#[test]
fn query_subsystem_log_signatures_in_prefix_carries_full_citation() {
    let (_temp, graph) = fixture_subsystem_with_logs();
    let parsed = run_subsystem("src/alpha", &graph);

    let sigs = parsed["log_signatures"]
        .as_array()
        .expect("log_signatures array");
    assert_eq!(
        sigs.len(),
        1,
        "exactly one in-prefix signature (coalesced) must appear, got {sigs:?}"
    );
    let sig = &sigs[0];

    assert_eq!(sig["record_id"], "log:v1:sig_alpha");
    assert_eq!(sig["kind"], "ErrorSignature");
    assert_eq!(sig["trust_class"], "runtime_observation");
    assert_eq!(sig["schema_version"], LOG_SCHEMA_VERSION);
    assert_eq!(sig["severity"], "fatal");
    // Coalesced: 7 + 5 summed across the two stable-ID records.
    assert_eq!(sig["occurrence_count"], 12);
    // Earliest first_seen, latest last_seen across the group.
    assert_eq!(sig["first_seen_valid_time"], "2026-01-01T00:00:00Z");
    assert_eq!(sig["last_seen_valid_time"], "2026-01-04T00:00:00Z");

    let frames = sig["resolved_frames"]
        .as_array()
        .expect("resolved_frames array");
    assert_eq!(frames.len(), 1, "one in-prefix resolved frame");
    let frame = &frames[0];
    assert_eq!(frame["frame_index"], 0);
    assert_eq!(frame["frame_resolution"], "resolved");
    assert_eq!(frame["target_repo_relative_path"], "src/alpha/a.rs");
    assert!(
        frame["target_span"].is_object(),
        "resolved symbol frame must carry a span"
    );
}

#[test]
fn query_subsystem_log_signatures_no_bleed_to_alphabet() {
    let (_temp, graph) = fixture_subsystem_with_logs();
    let output = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");

    assert!(
        !stdout.contains("sig_alphabet"),
        "src/alphabet signature must never appear for src/alpha (no bleed, AC2)"
    );
    assert!(
        !stdout.contains("src/alphabet"),
        "src/alphabet paths must never appear for src/alpha (no bleed, AC2)"
    );
}

#[test]
fn query_subsystem_unresolved_only_signature_absent_but_target_in_unresolved() {
    let (_temp, graph) = fixture_subsystem_with_logs();
    let parsed = run_subsystem("src/alpha", &graph);

    // Never in log_signatures.
    let sigs = parsed["log_signatures"]
        .as_array()
        .expect("log_signatures array");
    assert!(
        sigs.iter()
            .all(|s| s["record_id"] != "log:v1:sig_unresolved"),
        "unresolved-only signature must not appear in log_signatures (AC4)"
    );

    // Its dangling frame target surfaces through the existing unresolved section.
    let unresolved = parsed["unresolved"].as_array().expect("unresolved array");
    assert!(
        unresolved.iter().any(|u| {
            u["source_record_id"] == "log:v1:sig_unresolved"
                && u["target_handle"] == "log:v1:diag_alpha_missing"
                && u["relation"] == "FRAME_RESOLVES_TO"
        }),
        "unresolved-only signature's dangling target must surface in unresolved (AC4): {unresolved:?}"
    );
}

#[test]
fn query_subsystem_zero_log_records_emits_empty_log_signatures_section() {
    // The base seeded fixture carries zero log-domain records.
    let (_temp, graph) = fixture_subsystem_seeded();
    let parsed = run_subsystem("src/alpha", &graph);

    // Always present, empty array (AC5 additive output).
    assert_eq!(
        parsed["log_signatures"],
        serde_json::json!([]),
        "log_signatures must be present and empty on a zero-log graph (AC5)"
    );

    // Every other section is unchanged / still populated.
    assert!(
        parsed["source_facts"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(
        parsed["observations"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(
        parsed["project_state"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(
        parsed["artifacts"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(
        parsed["verification_evidence"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert!(
        parsed["semantic_drift"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert_eq!(parsed["ok"], true);
}

#[test]
fn query_subsystem_log_signatures_output_is_deterministic_5x() {
    let (_temp, graph) = fixture_subsystem_with_logs();

    let baseline = egregore()
        .args(["query", "subsystem", "src/alpha", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    for _ in 0..4 {
        let again = egregore()
            .args(["query", "subsystem", "src/alpha", "--graph"])
            .arg(&graph)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(
            baseline, again,
            "log_signatures output must be byte-identical across 5 runs (AC6)"
        );
    }
}

/// Build a fixture mirroring [`fixture_subsystem_with_logs`] where the resolved
/// frame target (`alpha_sym`) is tombstoned while a live sibling (`alpha_file`)
/// keeps `src/alpha` matched. A tombstoned non-temporal code target is deleted,
/// so its signature must never surface in `log_signatures` (nor in `unresolved`).
fn fixture_subsystem_tombstoned_frame_target() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("subsystem-logs-tombstoned.jsonl");

    let alpha_sym_id = "codegraph:v4:intsublog_alpha_sym001".to_owned();
    // Live sibling File keeps the prefix matched after the symbol is deleted.
    let alpha_file = GraphRecord::node(
        "file:sublog:alpha_a".to_owned(),
        NodeKind::File,
        Some("src/alpha/a.rs".to_owned()),
        None,
        Some("src/alpha/a.rs".to_owned()),
        "file src/alpha/a.rs".to_owned(),
    );
    let alpha_sym = GraphRecord::symbol(
        alpha_sym_id.clone(),
        "fn",
        "src/alpha/a.rs".to_owned(),
        span(1, 20),
        "alpha_handler".to_owned(),
        "fn alpha_handler in src/alpha/a.rs".to_owned(),
    );
    // The tombstone that deletes the resolved frame target.
    let alpha_sym_tombstone = GraphRecord::Tombstone {
        id: "codegraph:v5:tombstone_alpha_sym".to_owned(),
        schema_version: SCHEMA_VERSION,
        deleted_id: alpha_sym_id.clone(),
        summary: "alpha_handler was deleted".to_owned(),
        producer: None,
    };

    let sig_alpha_id = "log:v1:sig_alpha";
    let sig_alpha = error_signature(
        sig_alpha_id,
        "fatal",
        7,
        "2026-01-02T00:00:00Z",
        "2026-01-03T00:00:00Z",
    );
    let sig_alpha_edge = frame_edge(sig_alpha_id, &alpha_sym_id, FrameResolution::Resolved, 0);

    let mut graph = Graph::new();
    for r in [
        alpha_file,
        alpha_sym,
        alpha_sym_tombstone,
        sig_alpha,
        sig_alpha_edge,
    ] {
        graph.push(r);
    }

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

#[test]
fn query_subsystem_tombstoned_frame_target_excluded_from_log_signatures() {
    let (_temp, graph) = fixture_subsystem_tombstoned_frame_target();
    let parsed = run_subsystem("src/alpha", &graph);

    // The signature's only resolved frame targets a tombstoned non-temporal code
    // node: the deletion gate must drop the frame, so no signature surfaces.
    let sigs = parsed["log_signatures"]
        .as_array()
        .expect("log_signatures array");
    assert!(
        sigs.is_empty(),
        "a signature whose only frame targets a tombstoned code node must not \
         appear in log_signatures, got {sigs:?}"
    );

    // The tombstoned target must not leak through the unresolved section either.
    let unresolved = parsed["unresolved"].as_array().expect("unresolved array");
    assert!(
        unresolved.iter().all(|u| {
            u["target_handle"] != "codegraph:v4:intsublog_alpha_sym001"
                && u["source_record_id"] != "log:v1:sig_alpha"
        }),
        "tombstoned frame target must not surface in unresolved: {unresolved:?}"
    );
}

/// Build a fixture mirroring [`fixture_subsystem_with_logs`] where a live
/// `ErrorSignature` (`sig_alpha`) and a live target symbol under `src/alpha` are
/// bound by a `FRAME_RESOLVES_TO` edge, but that EDGE record is tombstoned. The
/// binding is deleted, so no frame may be read from it: the signature must never
/// surface in `log_signatures`, and the (still-live) target must not leak into
/// `unresolved`.
fn fixture_subsystem_tombstoned_frame_edge() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("subsystem-logs-tombstoned-edge.jsonl");

    let alpha_sym_id = "codegraph:v4:intsublog_alpha_sym001".to_owned();
    let alpha_file = GraphRecord::node(
        "file:sublog:alpha_a".to_owned(),
        NodeKind::File,
        Some("src/alpha/a.rs".to_owned()),
        None,
        Some("src/alpha/a.rs".to_owned()),
        "file src/alpha/a.rs".to_owned(),
    );
    // Live target symbol — only the binding edge is deleted, not the symbol.
    let alpha_sym = GraphRecord::symbol(
        alpha_sym_id.clone(),
        "fn",
        "src/alpha/a.rs".to_owned(),
        span(1, 20),
        "alpha_handler".to_owned(),
        "fn alpha_handler in src/alpha/a.rs".to_owned(),
    );

    let sig_alpha_id = "log:v1:sig_alpha";
    let sig_alpha = error_signature(
        sig_alpha_id,
        "fatal",
        7,
        "2026-01-02T00:00:00Z",
        "2026-01-03T00:00:00Z",
    );
    let sig_alpha_edge = frame_edge(sig_alpha_id, &alpha_sym_id, FrameResolution::Resolved, 0);
    // The tombstone that deletes the frame-resolution EDGE by its own record id.
    let frame_edge_tombstone = GraphRecord::Tombstone {
        id: "codegraph:v5:tombstone_frame_edge".to_owned(),
        schema_version: SCHEMA_VERSION,
        deleted_id: sig_alpha_edge.id().to_owned(),
        summary: "frame resolution binding was retracted".to_owned(),
        producer: None,
    };

    let mut graph = Graph::new();
    for r in [
        alpha_file,
        alpha_sym,
        sig_alpha,
        sig_alpha_edge,
        frame_edge_tombstone,
    ] {
        graph.push(r);
    }

    let jsonl = graph.to_jsonl().expect("serialize graph");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

#[test]
fn query_subsystem_tombstoned_frame_edge_excluded_from_log_signatures() {
    let (_temp, graph) = fixture_subsystem_tombstoned_frame_edge();
    let parsed = run_subsystem("src/alpha", &graph);

    // The signature's only frame binding is a tombstoned FRAME_RESOLVES_TO edge:
    // the deletion gate must drop the frame, so no signature surfaces.
    let sigs = parsed["log_signatures"]
        .as_array()
        .expect("log_signatures array");
    assert!(
        sigs.is_empty(),
        "a signature whose only frame binding is a tombstoned FRAME_RESOLVES_TO \
         edge must not appear in log_signatures, got {sigs:?}"
    );

    // The (still-live) target must not leak through the unresolved section either.
    let unresolved = parsed["unresolved"].as_array().expect("unresolved array");
    assert!(
        unresolved.iter().all(|u| {
            u["target_handle"] != "codegraph:v4:intsublog_alpha_sym001"
                && u["source_record_id"] != "log:v1:sig_alpha"
        }),
        "tombstoned frame-edge binding must not surface in unresolved: {unresolved:?}"
    );
}
