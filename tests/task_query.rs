#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
        agent_memory_stable_id, project_stable_id, stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

/// Helper to generate a seeded graph JSONL fixture.
#[allow(clippy::too_many_lines)]
fn fixture_task_query_seeded() -> (tempfile::TempDir, PathBuf, String) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("task_query_seeded.jsonl");

    // Seed: 1 Task, 2 Acceptance Criteria (one verified, one unverified),
    // 2 code handles (touched file / mentions symbol), 1 agent observation,
    // 1 artifact, 1 verification record, 1 external link.
    let task_id = project_stable_id(&["task", "task_1"]);

    let ext_link_id = stable_id(&[
        "node",
        "ExternalLink",
        "https://github.com/madmax983/egregore/issues/48",
    ]);
    let mut ext_link = GraphRecord::node(
        ext_link_id.clone(),
        NodeKind::ExternalLink,
        None,
        None,
        None,
        "GitHub Issue #48 Link".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut url,
        ref mut system_native_id,
        ..
    } = ext_link
    {
        *url = Some("https://github.com/madmax983/egregore/issues/48".to_owned());
        *system_native_id = Some("madmax983/egregore#48".to_owned());
    }

    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Implement task evidence query".to_owned()),
        "Task #48 implementation".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut schema_version,
        ref mut source_external_link_id,
        ..
    } = task
    {
        *title = Some("Implement task evidence query".to_owned());
        *schema_version = PROJECT_SCHEMA_VERSION;
        *source_external_link_id = Some(ext_link_id);
    }

    // Acceptance Criterion 1: verified
    let ac_1_id = project_stable_id(&["acceptance_criterion", "ac_1"]);
    let mut ac_1 = GraphRecord::node(
        ac_1_id.clone(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("Query returns deterministic JSON output".to_owned()),
        "AC 1: JSON output".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut status,
        ref mut parent_task_id,
        ref mut schema_version,
        ..
    } = ac_1
    {
        *status = Some("verified".to_owned());
        *parent_task_id = Some(task_id.clone());
        *schema_version = PROJECT_SCHEMA_VERSION;
    }

    // Acceptance Criterion 2: unverified
    let ac_2_id = project_stable_id(&["acceptance_criterion", "ac_2"]);
    let mut ac_2 = GraphRecord::node(
        ac_2_id,
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("Query behaves local-first without network".to_owned()),
        "AC 2: Local-first".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut status,
        ref mut parent_task_id,
        ref mut schema_version,
        ..
    } = ac_2
    {
        *status = Some("pending".to_owned());
        *parent_task_id = Some(task_id.clone());
        *schema_version = PROJECT_SCHEMA_VERSION;
    }

    // CommandRun verification that closes AC 1
    let ver_id = verification_stable_id(&["verification", "ver_1"]);
    let mut ver = GraphRecord::node(
        ver_id.clone(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification pass".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut status,
        ref mut verification_kind,
        ..
    } = ver
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("command_run".to_owned());
    }

    // Edge connecting AC 1 to Verification (CLOSES_ACCEPTANCE_CRITERION)
    let ac_ver_edge = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ac_1_id,
        ver_id,
        Some("1.0".to_owned()),
        "AC 1 closed by Verification".to_owned(),
    );

    // Code facts
    let file_id = stable_id(&["node", "File", "src/query.rs"]);
    let file = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        "src/query.rs".to_owned(),
        span(1, 100),
        "query.rs".to_owned(),
        "rust",
        "Source file query.rs".to_owned(),
    );

    // Edges connecting Task to File (TouchesFile)
    let task_file_edge = GraphRecord::edge(
        EdgeLabel::TouchesFile,
        task_id.clone(),
        file_id,
        Some("1.0".to_owned()),
        "Task touches query.rs".to_owned(),
    );

    // Agent Observation referencing Task
    let obs_id = agent_memory_stable_id(&["obs", "obs_1"]);
    let mut obs = GraphRecord::node(
        obs_id,
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation on task 1".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut text,
        ref mut agent_id,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = obs
    {
        *text = Some("Implemented resolving logic".to_owned());
        *agent_id = Some("agent_1".to_owned());
        *session_id = Some("sess_1".to_owned());
        *observed_at = Some("2026-06-03T12:00:00Z".to_owned());
        *confidence = Some("1.0".to_owned());
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(task_id.clone()),
            target_domain: "project".to_owned(),
            relation: "REFERENCES_TASK".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    // Artifact referencing Task
    let art_id = agent_memory_stable_id(&["artifact", "art_1"]);
    let mut art = GraphRecord::node(
        art_id,
        NodeKind::Artifact,
        None,
        None,
        None,
        "Implementation artifact".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut evidence_links,
        ..
    } = art
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(task_id.clone()),
            target_domain: "project".to_owned(),
            relation: "REFERENCES_TASK".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }

    let mut graph = Graph::new();
    graph.push(ext_link);
    graph.push(task);
    graph.push(ac_1);
    graph.push(ac_2);
    graph.push(ver);
    graph.push(ac_ver_edge);
    graph.push(file);
    graph.push(task_file_edge);
    graph.push(obs);
    graph.push(art);

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    (temp, path, task_id)
}

#[test]
fn query_task_by_canonical_id_returns_structured_json() {
    let (_temp, graph, task_id) = fixture_task_query_seeded();

    let output = egregore()
        .args(["query", "task", &task_id, "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["task_id"], task_id);

    // Verify AC 1 is verified and carries the inlined verification_record
    let acs = parsed["acceptance_criteria"].as_array().expect("ac array");
    assert_eq!(acs.len(), 2);
    let ac_1_id = project_stable_id(&["acceptance_criterion", "ac_1"]);
    let ac1 = acs
        .iter()
        .find(|ac| ac["record_id"].as_str() == Some(&ac_1_id))
        .unwrap();
    assert_eq!(ac1["status"], "verified");
    assert_eq!(ac1["verification_record"]["kind"], "Verification");
    assert_eq!(ac1["verification_record"]["status"], "pass");

    // Verify source facts contains the file
    let source_facts = parsed["source_facts"].as_array().expect("source facts");
    assert!(
        source_facts
            .iter()
            .any(|f| f["repo_relative_path"] == "src/query.rs")
    );

    // Verify observations are populated
    let observations = parsed["observations"].as_array().expect("obs");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0]["agent_id"], "agent_1");

    // Verify artifacts are populated
    let artifacts = parsed["artifacts"].as_array().expect("artifacts");
    assert_eq!(artifacts.len(), 1);
}

#[test]
fn query_task_by_github_url_resolves() {
    let (_temp, graph, task_id) = fixture_task_query_seeded();

    let output = egregore()
        .args([
            "query",
            "task",
            "https://github.com/madmax983/egregore/issues/48",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["task_id"], task_id);
}

#[test]
fn query_task_by_github_short_handle_resolves() {
    let (_temp, graph, task_id) = fixture_task_query_seeded();

    let output = egregore()
        .args(["query", "task", "madmax983/egregore#48", "--graph"])
        .arg(&graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["task_id"], task_id);
}

#[test]
fn query_task_unsupported_handle_exits_1_with_json_error() {
    let (_temp, graph, _) = fixture_task_query_seeded();

    let output = egregore()
        .args(["query", "task", "invalid_handle_format", "--graph"])
        .arg(&graph)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let stderr = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).expect("valid JSON error");
    assert!(parsed.get("Unsupported").is_some());
    assert_eq!(parsed["Unsupported"]["handle"], "invalid_handle_format");
}

#[test]
fn query_task_no_match_exits_2_with_json_envelope() {
    let (_temp, graph, _) = fixture_task_query_seeded();

    let output = egregore()
        .args([
            "query",
            "task",
            "https://github.com/madmax983/egregore/issues/999",
            "--graph",
        ])
        .arg(&graph)
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("valid JSON envelope");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error"]["code"], "no_match");
}

#[test]
fn query_task_ambiguous_handle_exits_1_with_json_error() {
    let (temp, graph, _) = fixture_task_query_seeded();

    // We add another Task node matching the same issue to trigger ambiguity
    let task_id_2 = project_stable_id(&["task", "task_2"]);
    let ext_link_id = stable_id(&[
        "node",
        "ExternalLink",
        "https://github.com/madmax983/egregore/issues/48",
    ]);

    let mut task2 = GraphRecord::node(
        task_id_2,
        NodeKind::Task,
        None,
        None,
        Some("Second task for same issue".to_owned()),
        "Task 2".to_owned(),
    );
    if let GraphRecord::Node {
        ref mut title,
        ref mut schema_version,
        ref mut source_external_link_id,
        ..
    } = task2
    {
        *title = Some("Second task for same issue".to_owned());
        *schema_version = PROJECT_SCHEMA_VERSION;
        *source_external_link_id = Some(ext_link_id);
    }

    let content = fs::read_to_string(&graph).expect("read");
    let mut parsed_records: Vec<GraphRecord> = serde_json::from_str::<serde_json::Value>(&format!(
        "[{}]",
        content.trim().replace('\n', ",")
    ))
    .expect("parse jsonl")
    .as_array()
    .unwrap()
    .iter()
    .map(|v| serde_json::from_value(v.clone()).unwrap())
    .collect();

    parsed_records.push(task2);
    let mut new_graph = Graph::new();
    for r in parsed_records {
        new_graph.push(r);
    }
    let new_path = temp.path().join("task_query_ambiguous.jsonl");
    fs::write(&new_path, new_graph.to_jsonl().expect("serialize")).expect("write");

    let output = egregore()
        .args(["query", "task", "madmax983/egregore#48", "--graph"])
        .arg(&new_path)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();

    let stderr = String::from_utf8(output).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).expect("valid JSON error");
    assert!(parsed.get("Ambiguous").is_some());
    assert_eq!(parsed["Ambiguous"]["handle"], "madmax983/egregore#48");
    assert_eq!(
        parsed["Ambiguous"]["candidates"].as_array().unwrap().len(),
        2
    );
}
