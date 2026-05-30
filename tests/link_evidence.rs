//! Tests for the evidence-to-code-graph linker (issue #43).
//!
//! RED-first: all tests are written before the implementation exists.
//! They drive the exact contract specified in the issue acceptance criteria.

use std::path::Path;

use aletheia_egregore::{
    import_traj,
    ir::{EdgeLabel, GraphRecord, NodeKind, agent_memory_stable_id, stable_id},
    link_evidence::{self, DiagnosticReason, LinkOptions},
    scan_repository,
    traj::ImportOptions,
};
use assert_cmd::Command;
use tempfile::tempdir;

const CODE_GRAPH_FIXTURE: &str = "tests/fixtures/rust_basic";
const TRAJ_FIXTURE: &str = "tests/fixtures/agent_memory/link_evidence_basic/trajectory.traj";

// ── helpers ───────────────────────────────────────────────────────────────────

fn rust_basic_graph() -> Vec<GraphRecord> {
    scan_repository(Path::new(CODE_GRAPH_FIXTURE))
        .expect("rust_basic must scan")
        .records()
        .to_vec()
}

fn link_evidence_agent_session() -> Vec<GraphRecord> {
    import_traj(Path::new(TRAJ_FIXTURE), &ImportOptions::passthrough())
        .expect("link_evidence_basic trajectory must import")
        .records()
        .to_vec()
}

const fn is_touched_file_edge(r: &GraphRecord) -> bool {
    matches!(r, GraphRecord::Edge { label: EdgeLabel::TouchedFile, .. })
}

fn file_node_ids(records: &[GraphRecord]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { id, kind: NodeKind::File, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn file_edit_ids(records: &[GraphRecord]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { id, kind: NodeKind::FileEdit, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn verification_ids(records: &[GraphRecord]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { id, kind: NodeKind::Verification, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

/// Build a minimal Failure node with an optional `repo_relative_path`.
fn make_failure_node(id: String, repo_relative_path: Option<String>) -> GraphRecord {
    make_agent_memory_node(id, NodeKind::Failure, repo_relative_path, None, None)
}

/// Build a minimal Observation node with an optional `name`.
fn make_observation_node(id: String, name: Option<String>) -> GraphRecord {
    make_agent_memory_node(id, NodeKind::Observation, None, name, None)
}

#[allow(clippy::too_many_lines)]
fn make_agent_memory_node(
    id: String,
    kind: NodeKind,
    repo_relative_path: Option<String>,
    name: Option<String>,
    text: Option<String>,
) -> GraphRecord {
    GraphRecord::Node {
        id,
        kind,
        schema_version: aletheia_egregore::AGENT_MEMORY_SCHEMA_VERSION,
        repo_relative_path,
        span: None,
        name,
        language: None,
        symbol_kind: None,
        disambiguator: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        repository_identity: None,
        text,
        superseded_by: None,
        agent_id: Some("test-agent".to_string()),
        agent_kind: Some("test".to_string()),
        session_id: Some("test-session".to_string()),
        observed_at: Some("2026-05-20T09:00:00Z".to_string()),
        ingested_at: Some("2026-05-20T09:00:00Z".to_string()),
        confidence: None,
        source_handle: Some("fixture:deadbeef".to_string()),
        redaction_policy_version: None,
        valid_time: None,
        valid_time_source: None,
        entity_id: None,
        title: None,
        body_handle: None,
        source_kind: None,
        source_external_link_id: None,
        assignees: None,
        labels: None,
        priority: None,
        parent_task_id: None,
        ordinal: None,
        verification_link_id: None,
        system: None,
        url: None,
        system_native_id: None,
        repository_remote: None,
        discovered_at: None,
        transaction_time: None,
        summary: format!("{kind:?} test node"),
        domain: Some("agent_memory".to_string()),
        importer_id: Some("test".to_string()),
        importer_version: Some("0.0.0".to_string()),
        source_artifact_path: Some("fixture".to_string()),
        source_artifact_hash: Some("deadbeef".to_string()),
        patch_status: None,
        base_commit: None,
        unknown_base_reason: None,
        target_files: None,
        patch_bytes_hash: None,
        patch_bytes_size: None,
        patch_handle: None,
        validation_summary: None,
        producer_session_id: None,
        edit_kind: None,
        before_hash: None,
        after_hash: None,
        rename_to: None,
        hunk_count: None,
        linked_patch_id: None,
        linked_turn_id: None,
        tool_name: None,
        tool_kind: None,
        arguments_summary: None,
        arguments_handle: None,
        result_handle: None,
        produced_evidence_id: None,
        started_at: None,
        finished_at: None,
        failure_kind: None,
        exit_code: None,
        turn_index: None,
        stdout_handle: None,
        stderr_handle: None,
        evidence_quality: None,
        executed_at: None,
        verification_kind: None,
        status: None,
        user_context: aletheia_egregore::UserContextFields::empty(),
        producer: None,
    }
}

fn make_file_node(id: String, path: &str) -> GraphRecord {
    GraphRecord::node(
        id,
        NodeKind::File,
        Some(path.to_string()),
        None,
        Some(path.to_string()),
        format!("File {path}"),
    )
}

fn make_symbol_node(id: String, path: &str, name: &str) -> GraphRecord {
    GraphRecord::node(
        id,
        NodeKind::Symbol,
        Some(path.to_string()),
        None,
        Some(name.to_string()),
        format!("Symbol {name} in {path}"),
    )
}

// ── AC: fixture existence ─────────────────────────────────────────────────────

#[test]
fn fixture_trajectory_exists() {
    assert!(
        Path::new(TRAJ_FIXTURE).exists(),
        "fixture trajectory not found at {TRAJ_FIXTURE}"
    );
}

#[test]
fn fixture_readme_exists() {
    assert!(
        Path::new("tests/fixtures/agent_memory/link_evidence_basic/README.md").exists(),
        "fixture README not found"
    );
}

// ── AC1 + AC2: TOUCHED_FILE edges for resolved file handles ───────────────────

#[test]
fn resolved_file_edit_emits_touched_file_edge() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    assert!(
        output.edges.iter().any(is_touched_file_edge),
        "expected at least one TOUCHED_FILE edge for src/lib.rs"
    );
}

#[test]
fn touched_file_edge_target_is_file_node_from_code_graph() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();
    let file_ids = file_node_ids(&code_graph);

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    for edge in &output.edges {
        if let GraphRecord::Edge { label: EdgeLabel::TouchedFile, target, .. } = edge {
            assert!(
                file_ids.contains(&target.as_str()),
                "TOUCHED_FILE target {target} is not a File node from the code graph"
            );
        }
    }
}

#[test]
fn touched_file_edge_source_preserves_evidence_record_id() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();
    let edit_ids = file_edit_ids(&evidence);

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    let any_match = output.edges.iter().any(|r| {
        matches!(r, GraphRecord::Edge { label: EdgeLabel::TouchedFile, source, .. } if edit_ids.contains(&source.as_str()))
    });

    assert!(
        any_match,
        "no TOUCHED_FILE edge source matched a FileEdit record"
    );
}

// ── AC5: diagnostics for unresolved file handles ──────────────────────────────

#[test]
fn missing_file_produces_diagnostic() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    assert!(
        output
            .diagnostics
            .iter()
            .any(|d| matches!(d.reason, DiagnosticReason::MissingFile)),
        "expected at least one missing_file diagnostic for src/missing_helper.rs"
    );
}

#[test]
fn missing_file_diagnostic_carries_repo_relative_path() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    for diag in &output.diagnostics {
        if matches!(diag.reason, DiagnosticReason::MissingFile) {
            assert!(
                diag.repo_relative_path.is_some(),
                "missing_file diagnostic must carry repo_relative_path"
            );
        }
    }
}

#[test]
fn missing_file_diagnostic_carries_source_record_id() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    for diag in &output.diagnostics {
        if matches!(diag.reason, DiagnosticReason::MissingFile) {
            assert!(
                !diag.source_record_id.is_empty(),
                "missing_file diagnostic must carry a non-empty source_record_id"
            );
        }
    }
}

// ── AC3: FAILED_ON emitted only for unambiguous file handles ──────────────────

#[test]
fn failure_node_with_repo_relative_path_emits_failed_on() {
    let code_graph = rust_basic_graph();
    let failure_id = agent_memory_stable_id(&["test", "failure", "src/lib.rs"]);
    let evidence = vec![make_failure_node(
        failure_id.clone(),
        Some("src/lib.rs".to_string()),
    )];

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    let has_failed_on = output.edges.iter().any(|r| {
        matches!(r, GraphRecord::Edge { label: EdgeLabel::FailedOn, source, .. } if source == &failure_id)
    });

    assert!(
        has_failed_on,
        "expected FAILED_ON edge from Failure node with repo_relative_path set"
    );
}

#[test]
fn failure_node_without_file_handle_emits_no_failed_on_cross_domain_edge() {
    let code_graph = rust_basic_graph();
    let failure_id = agent_memory_stable_id(&["test", "failure", "no_path"]);
    let evidence = vec![make_failure_node(failure_id.clone(), None)];

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    let has_cross_domain_failed_on = output.edges.iter().any(|r| {
        matches!(r, GraphRecord::Edge { label: EdgeLabel::FailedOn, source, target, .. }
            if source == &failure_id && target.starts_with("codegraph:"))
    });

    assert!(
        !has_cross_domain_failed_on,
        "must not emit cross-domain FAILED_ON for Failure with no file handle"
    );
}

// ── AC3: MENTIONS_SYMBOL disambiguation ──────────────────────────────────────

#[test]
fn ambiguous_symbol_name_produces_diagnostic_not_edge() {
    let sym_a = stable_id(&["File", "src/a.rs", "Symbol", "fn", "helper", "0"]);
    let sym_b = stable_id(&["File", "src/b.rs", "Symbol", "fn", "helper", "0"]);
    let file_a = stable_id(&["File", "src/a.rs"]);
    let file_b = stable_id(&["File", "src/b.rs"]);

    let code_graph = vec![
        make_file_node(file_a, "src/a.rs"),
        make_file_node(file_b, "src/b.rs"),
        make_symbol_node(sym_a, "src/a.rs", "helper"),
        make_symbol_node(sym_b, "src/b.rs", "helper"),
    ];

    let obs_id = agent_memory_stable_id(&["test", "obs", "ambiguous"]);
    let evidence = vec![make_observation_node(obs_id, Some("helper".to_string()))];

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    assert!(
        output
            .diagnostics
            .iter()
            .any(|d| matches!(d.reason, DiagnosticReason::AmbiguousSymbol)),
        "expected ambiguous_symbol diagnostic for 'helper' which matches two symbols"
    );

    assert!(
        output
            .edges
            .iter()
            .all(|r| !matches!(r, GraphRecord::Edge { label: EdgeLabel::MentionsSymbol, .. })),
        "must not emit MENTIONS_SYMBOL edge for ambiguous name"
    );
}

#[test]
fn unambiguous_symbol_name_emits_mentions_symbol_edge() {
    let sym_id = stable_id(&["File", "src/lib.rs", "Symbol", "fn", "unique_fn", "0"]);
    let file_id = stable_id(&["File", "src/lib.rs"]);

    let code_graph = vec![
        make_file_node(file_id, "src/lib.rs"),
        make_symbol_node(sym_id, "src/lib.rs", "unique_fn"),
    ];

    let obs_id = agent_memory_stable_id(&["test", "obs", "unambiguous"]);
    let evidence = vec![make_observation_node(obs_id, Some("unique_fn".to_string()))];

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    let count = output
        .edges
        .iter()
        .filter(|r| matches!(r, GraphRecord::Edge { label: EdgeLabel::MentionsSymbol, .. }))
        .count();

    assert_eq!(
        count, 1,
        "expected exactly one MENTIONS_SYMBOL edge for unambiguous symbol"
    );
}

// ── AC4: verification trust boundary ─────────────────────────────────────────

#[test]
fn verification_node_does_not_get_touched_file_link() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();
    let ver_ids = verification_ids(&evidence);

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    for edge in &output.edges {
        if let GraphRecord::Edge { label: EdgeLabel::TouchedFile, source, .. } = edge {
            assert!(
                !ver_ids.contains(&source.as_str()),
                "Verification node {source} must not get a TOUCHED_FILE link"
            );
        }
    }
}

// ── AC5: 100% of unresolved handles emit diagnostics ─────────────────────────

#[test]
fn all_unresolvable_file_edits_emit_diagnostics() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let resolved_paths: std::collections::BTreeSet<&str> = code_graph
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { kind: NodeKind::File, repo_relative_path: Some(p), .. } => {
                Some(p.as_str())
            }
            _ => None,
        })
        .collect();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    for r in &evidence {
        if let GraphRecord::Node {
            kind: NodeKind::FileEdit,
            repo_relative_path: Some(path),
            id,
            ..
        } = r
        {
            if resolved_paths.contains(path.as_str()) {
                continue;
            }
            let has_diag = output.diagnostics.iter().any(|d| {
                d.source_record_id == *id
                    && matches!(d.reason, DiagnosticReason::MissingFile)
                    && d.repo_relative_path.as_deref() == Some(path.as_str())
            });
            assert!(has_diag, "FileEdit {id} for path {path} has no missing_file diagnostic");
        }
    }
}

// ── AC6: determinism – 5 identical runs ──────────────────────────────────────

#[test]
fn link_evidence_is_deterministic_across_five_runs() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let serialize_output = |o: link_evidence::LinkOutput| {
        let mut lines: Vec<String> = o
            .edges
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        lines.extend(o.diagnostics.iter().map(|d| serde_json::to_string(d).unwrap()));
        lines
    };

    let first = serialize_output(link_evidence::link_evidence(
        &code_graph,
        &evidence,
        &LinkOptions::default(),
    ));

    for run in 2..=5 {
        let current = serialize_output(link_evidence::link_evidence(
            &code_graph,
            &evidence,
            &LinkOptions::default(),
        ));
        assert_eq!(first, current, "link_evidence output differed on run {run}");
    }
}

// ── AC7: linked edges are ingestible ─────────────────────────────────────────

#[test]
fn linked_edges_round_trip_through_dry_run_ingest() {
    use aletheia_egregore::adapters::{DryRunSink, GraphSink};

    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    let mut sink = DryRunSink::default();
    for edge in &output.edges {
        sink.write_record(edge)
            .expect("linked edge must be accepted by DryRunSink");
    }
}

#[test]
fn linked_edges_have_valid_agent_memory_ids() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    for edge in &output.edges {
        let eid: &str = edge.id();
        assert!(
            eid.starts_with("agent_memory:v1:"),
            "linked edge ID must use agent_memory:v1: prefix, got: {eid}"
        );
    }
}

// ── AC9: no new edge labels ───────────────────────────────────────────────────

#[test]
fn linked_edges_use_only_registered_evidence_link_labels() {
    let code_graph = rust_basic_graph();
    let evidence = link_evidence_agent_session();

    let output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    for edge in &output.edges {
        if let GraphRecord::Edge { label, .. } = edge {
            assert!(
                label.is_evidence_link_label(),
                "linked edge uses non-evidence label {label:?}"
            );
        }
    }
}

// ── CLI: eg link-evidence command ────────────────────────────────────────────

#[test]
fn cli_link_evidence_command_succeeds_and_writes_jsonl() {
    let tmp = tempdir().expect("temp dir");
    let code_graph_path = tmp.path().join("code_graph.jsonl");
    let evidence_path = tmp.path().join("evidence.jsonl");
    let out_path = tmp.path().join("linked.jsonl");

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("scan")
        .arg(CODE_GRAPH_FIXTURE)
        .arg("--out")
        .arg(&code_graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("import-traj")
        .arg(TRAJ_FIXTURE)
        .arg("--out")
        .arg(&evidence_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("link-evidence")
        .arg("--code-graph")
        .arg(&code_graph_path)
        .arg("--evidence")
        .arg(&evidence_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();

    let content = std::fs::read_to_string(&out_path).expect("output file must exist");
    assert!(
        !content.trim().is_empty(),
        "link-evidence output must not be empty"
    );

    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("output line is not valid JSON: {e}\nLine: {line}"));
        assert!(
            parsed.get("record_type").is_some(),
            "output line missing record_type: {line}"
        );
    }
}

#[test]
fn cli_link_evidence_output_contains_touched_file_edges() {
    let tmp = tempdir().expect("temp dir");
    let code_graph_path = tmp.path().join("code_graph.jsonl");
    let evidence_path = tmp.path().join("evidence.jsonl");
    let out_path = tmp.path().join("linked.jsonl");

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("scan")
        .arg(CODE_GRAPH_FIXTURE)
        .arg("--out")
        .arg(&code_graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("import-traj")
        .arg(TRAJ_FIXTURE)
        .arg("--out")
        .arg(&evidence_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("link-evidence")
        .arg("--code-graph")
        .arg(&code_graph_path)
        .arg("--evidence")
        .arg(&evidence_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();

    let content = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        content.contains("TOUCHED_FILE"),
        "CLI output must contain TOUCHED_FILE edges"
    );
}

#[test]
fn cli_link_evidence_is_idempotent_on_unchanged_inputs() {
    let tmp = tempdir().expect("temp dir");
    let code_graph_path = tmp.path().join("code_graph.jsonl");
    let evidence_path = tmp.path().join("evidence.jsonl");
    let out1 = tmp.path().join("linked1.jsonl");
    let out2 = tmp.path().join("linked2.jsonl");

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("scan")
        .arg(CODE_GRAPH_FIXTURE)
        .arg("--out")
        .arg(&code_graph_path)
        .assert()
        .success();

    Command::cargo_bin("egregore")
        .unwrap()
        .arg("import-traj")
        .arg(TRAJ_FIXTURE)
        .arg("--out")
        .arg(&evidence_path)
        .assert()
        .success();

    for out in [&out1, &out2] {
        Command::cargo_bin("egregore")
            .unwrap()
            .arg("link-evidence")
            .arg("--code-graph")
            .arg(&code_graph_path)
            .arg("--evidence")
            .arg(&evidence_path)
            .arg("--out")
            .arg(out)
            .assert()
            .success();
    }

    let content1 = std::fs::read_to_string(&out1).unwrap();
    let content2 = std::fs::read_to_string(&out2).unwrap();
    assert_eq!(content1, content2, "link-evidence must be idempotent");
}
