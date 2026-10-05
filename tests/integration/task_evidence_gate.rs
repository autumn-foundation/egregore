//! CLI end-to-end tests for `eg query task-evidence-gate` (issue #147).
//!
//! Written RED-first per SPEC-PROOF-RED-GREEN-REFACTOR. These drive the real
//! binary against a hand-built project graph: JSON envelope shape (top-level
//! `ready` boolean, per-criterion evidence rows with pass/fail + freshness),
//! the exit-code contract (0 verdict incl. ready:false / 2 no such task),
//! `--format text` readability, and byte-identical repeat runs.

#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
        project_stable_id, verification_stable_id,
    },
};
use assert_cmd::Command;
use serde_json::Value;

fn eg() -> Command {
    Command::cargo_bin("eg").expect("eg binary builds")
}

fn task_node(id: &str, title: &str, status: &str) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::Task,
        None,
        None,
        Some("t1".to_owned()),
        format!("Task: {title}"),
    );
    if let GraphRecord::Node {
        schema_version,
        title: t,
        status: s,
        ..
    } = &mut rec
    {
        // The project domain accepts its own schema version (issue #166).
        *schema_version = PROJECT_SCHEMA_VERSION;
        *t = Some(title.to_owned());
        *s = Some(status.to_owned());
    }
    rec
}

fn ac_node(id: &str, task_id: &str, ordinal: u32, text: &str) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::AcceptanceCriterion,
        None,
        None,
        Some("ac1".to_owned()),
        format!("AcceptanceCriterion: {text}"),
    );
    if let GraphRecord::Node {
        schema_version,
        parent_task_id: p,
        ordinal: o,
        text: t,
        ..
    } = &mut rec
    {
        // The project domain accepts its own schema version (issue #166).
        *schema_version = PROJECT_SCHEMA_VERSION;
        *p = Some(task_id.to_owned());
        *o = Some(ordinal);
        *t = Some(text.to_owned());
    }
    rec
}

fn ver_run(slug: &str, status: &str, executed_at: Option<&str>) -> (String, GraphRecord) {
    let id = verification_stable_id(&["verification", slug]);
    let mut rec = GraphRecord::node(
        id.clone(),
        NodeKind::TestRun,
        None,
        None,
        None,
        format!("verification {slug}"),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        status: st,
        executed_at: ea,
        ..
    } = &mut rec
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *domain = Some("verification".to_owned());
        *st = Some(status.to_owned());
        *ea = executed_at.map(str::to_owned);
    }
    (id, rec)
}

fn closes_edge(ac_id: &str, ver_id: &str) -> GraphRecord {
    let mut rec = GraphRecord::edge(
        EdgeLabel::ClosesAcceptanceCriterion,
        ac_id.to_owned(),
        ver_id.to_owned(),
        None,
        "closes".to_owned(),
    );
    if let GraphRecord::Edge { schema_version, .. } = &mut rec {
        // Cross-domain edges file under the agent_memory domain (issue #166).
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
    }
    rec
}

/// A task with one criterion closed by one passing, citation-free `TestRun`:
/// fresh by vacuity, so the gate opens.
fn ready_graph() -> (String, Vec<GraphRecord>) {
    let task_id = project_stable_id(&["project", "Task", "cli-gate-ready"]);
    let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "cli-gate-ready", "1"]);
    let (ver_id, ver) = ver_run("cli-gate-pass", "passed", Some("2026-01-01T00:00:00Z"));
    let records = vec![
        task_node(&task_id, "Gate the thing", "open"),
        ac_node(&ac_id, &task_id, 1, "the widget renders"),
        ver,
        closes_edge(&ac_id, &ver_id),
    ];
    (task_id, records)
}

/// A task with one criterion and no verification evidence at all.
fn missing_evidence_graph() -> (String, Vec<GraphRecord>) {
    let task_id = project_stable_id(&["project", "Task", "cli-gate-missing"]);
    let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "cli-gate-missing", "1"]);
    let records = vec![
        task_node(&task_id, "Gate the thing", "open"),
        ac_node(&ac_id, &task_id, 1, "the widget renders"),
    ];
    (task_id, records)
}

fn write_graph(records: Vec<GraphRecord>) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("task-evidence-gate.graph.jsonl");
    let mut graph = Graph::new();
    for record in records {
        graph.push(record);
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write graph");
    (temp, path)
}

fn gate_json(graph: &PathBuf, task_id: &str, extra: &[&str]) -> assert_cmd::assert::Assert {
    eg().arg("query")
        .arg("task-evidence-gate")
        .arg(task_id)
        .arg("--graph")
        .arg(graph)
        .args(extra)
        .assert()
}

fn gate_json_ok(graph: &PathBuf, task_id: &str, extra: &[&str]) -> Value {
    let output = gate_json(graph, task_id, extra)
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("valid JSON envelope")
}

/// AC1 + AC6: passing non-drifted evidence yields a top-level `ready: true`
/// verdict with exit code 0.
#[test]
fn passing_fresh_evidence_yields_ready_true() {
    let (task_id, records) = ready_graph();
    let (_temp, graph) = write_graph(records);

    let body = gate_json_ok(&graph, &task_id, &[]);
    assert_eq!(body["ok"], true, "envelope ok, got {body}");
    assert_eq!(body["lane"], "task-evidence-gate");
    assert_eq!(body["ready"], true, "top-level ready boolean, got {body}");
    let criteria = body["criteria"].as_array().expect("criteria array");
    assert_eq!(criteria.len(), 1);
    let criterion_id = criteria[0]["record_id"].as_str().unwrap();
    assert!(
        criterion_id.starts_with("project:v1:")
            && criterion_id.len() == "project:v1:".len() + 64
            && criterion_id["project:v1:".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
        "canonical project record ID, got {criterion_id}"
    );
    assert_eq!(criteria[0]["text"], "the widget renders");
    assert_eq!(criteria[0]["satisfied"], true);
    assert!(criteria[0].get("not_ready_reason").is_none());
    let evidence = criteria[0]["evidence"].as_array().expect("evidence array");
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0]["pass"], true);
    assert_eq!(evidence[0]["freshness"], "current");
    assert_eq!(body["counts"]["criteria"], 1);
    assert_eq!(body["counts"]["satisfied"], 1);
}

/// AC3: a criterion with no linked verification evidence yields
/// `ready: false` naming the criterion with reason `missing_evidence` —
/// still exit 0, since a verdict was produced.
#[test]
fn missing_evidence_yields_ready_false_with_reason() {
    let (task_id, records) = missing_evidence_graph();
    let (_temp, graph) = write_graph(records);

    let body = gate_json_ok(&graph, &task_id, &[]);
    assert_eq!(body["ok"], true);
    assert_eq!(body["ready"], false);
    let criteria = body["criteria"].as_array().expect("criteria array");
    assert_eq!(criteria.len(), 1);
    assert_eq!(criteria[0]["satisfied"], false);
    assert_eq!(criteria[0]["not_ready_reason"], "missing_evidence");
    assert_eq!(
        criteria[0]["evidence"]
            .as_array()
            .expect("evidence array")
            .len(),
        0
    );
}

/// AC9: no such task exits 2 with a structured no-match envelope.
#[test]
fn no_such_task_exits_2() {
    let (_task_id, records) = ready_graph();
    let (_temp, graph) = write_graph(records);

    let output = gate_json(
        &graph,
        "project:v1:0000000000000000000000000000000000000000000000000000000000000000",
        &[],
    );
    let asserted = output.code(2);
    let stdout = String::from_utf8_lossy(&asserted.get_output().stdout);
    let body: Value = serde_json::from_str(&stdout).expect("JSON envelope on stdout");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "no_match");
}

/// AC8: `--format text` is human-readable only — it names the verdict, the
/// criterion text, and the blocking reason without JSON.
#[test]
fn text_format_is_human_readable() {
    let (task_id, records) = missing_evidence_graph();
    let (_temp, graph) = write_graph(records);

    let output = gate_json(&graph, &task_id, &["--format", "text"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8 text output");
    assert!(
        text.contains("NOT READY") || text.contains("not ready"),
        "text names the verdict, got:\n{text}"
    );
    assert!(
        text.contains("the widget renders"),
        "text names the criterion, got:\n{text}"
    );
    assert!(
        text.contains("missing_evidence"),
        "text names the reason, got:\n{text}"
    );
    assert!(
        !text.trim_start().starts_with('{'),
        "text format must not be JSON, got:\n{text}"
    );
}

/// AC7: repeat runs over an unchanged store are byte-identical.
#[test]
fn repeat_runs_are_byte_identical() {
    let (task_id, records) = ready_graph();
    let (_temp, graph) = write_graph(records);

    let first = gate_json(&graph, &task_id, &[])
        .success()
        .get_output()
        .stdout
        .clone();
    let second = gate_json(&graph, &task_id, &[])
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(first, second, "deterministic for unchanged store");
}

/// AC5 (CLI surface): a failing verification record blocks with
/// `failing_evidence`.
#[test]
fn failing_evidence_yields_ready_false_cli() {
    let task_id = project_stable_id(&["project", "Task", "cli-gate-fail"]);
    let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "cli-gate-fail", "1"]);
    let (ver_id, ver) = ver_run("cli-gate-fail-run", "failed", Some("2026-01-01T00:00:00Z"));
    let records = vec![
        task_node(&task_id, "Gate the thing", "open"),
        ac_node(&ac_id, &task_id, 1, "the widget renders"),
        ver,
        closes_edge(&ac_id, &ver_id),
    ];
    let (_temp, graph) = write_graph(records);

    let body = gate_json_ok(&graph, &task_id, &[]);
    assert_eq!(body["ready"], false);
    assert_eq!(body["criteria"][0]["not_ready_reason"], "failing_evidence");
    assert_eq!(body["criteria"][0]["evidence"][0]["pass"], false);
}
