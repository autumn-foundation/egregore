//! Issue #191: `Decision` records must surface with rationale in a dedicated
//! `decisions` section of the `symbol_context` (`query context`) and
//! `task_evidence` (`query task`) answers — never flattened into
//! `observations`.
//!
//! Fixture store (AC6): one decision linked to a symbol (via inline
//! `evidence_links`, with an `EXPLAINS_CHANGE` handle to a commit), one
//! decision linked to a task (via a `REFERENCES_TASK` edge), plus one
//! observation and one failure proving the section split in both directions.
//!
//! Success metric asserted here: 100% of the fixture's Decision records
//! surface in `decisions` with non-empty `rationale_summary`; 0% appear in
//! `observations`.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION, SCHEMA_VERSION},
};
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const SYM_ID: &str = "codegraph:v5:sym-decide-fn";
const SYM_NAME: &str = "decide_fn";
const TASK_ID: &str = "project:v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const COMMIT_ID: &str = "codegraph:v5:commit-abc123";
const DEC_SYM_ID: &str = "agent_memory:v1:dec-linked-to-symbol";
const DEC_TASK_ID: &str = "agent_memory:v1:dec-linked-to-task";
const OBS_ID: &str = "agent_memory:v1:obs-dec-191";
const FAIL_ID: &str = "agent_memory:v1:fail-dec-191";

const DEC_SYM_TEXT: &str = "Use BTreeMap for the routing table";
const DEC_SYM_RATIONALE: &str =
    "HashMap iteration order made the routing snapshot tests flaky on CI";
const DEC_TASK_TEXT: &str = "Ship the fix behind the existing feature flag";
const DEC_TASK_RATIONALE: &str =
    "The flag already gates the old path, so rollout is reversible without a revert";

fn evidence_link(target: &str, relation: &str, domain: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

fn stamp_memory(node: &mut GraphRecord, links: Vec<EvidenceLink>) {
    if let GraphRecord::Node {
        agent_id,
        session_id,
        observed_at,
        confidence,
        evidence_links,
        source_handle,
        ..
    } = node
    {
        *agent_id = Some("agent-1".to_owned());
        *session_id = Some("sess-1".to_owned());
        *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *source_handle = Some("traj-abc123".to_owned());
        *evidence_links = Some(links);
    }
}

/// The two fixture decisions: one linked to the symbol via inline
/// `evidence_links` (carrying an `EXPLAINS_CHANGE` handle to the commit),
/// one linked to the task via a `REFERENCES_TASK` graph edge. Returns the
/// symbol-linked decision, the task-linked decision, and its edge.
fn decision_records() -> (GraphRecord, GraphRecord, GraphRecord) {
    let mut dec_sym = GraphRecord::node(
        DEC_SYM_ID.to_owned(),
        NodeKind::Decision,
        None,
        None,
        None,
        "decision summary".to_owned(),
    )
    .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION)
    .with_decision_text(DEC_SYM_TEXT)
    .with_rationale_summary(DEC_SYM_RATIONALE);
    stamp_memory(
        &mut dec_sym,
        vec![
            evidence_link(SYM_ID, "OBSERVES", "codegraph"),
            evidence_link(COMMIT_ID, "EXPLAINS_CHANGE", "codegraph"),
        ],
    );

    let mut dec_task = GraphRecord::node(
        DEC_TASK_ID.to_owned(),
        NodeKind::Decision,
        None,
        None,
        None,
        "decision summary".to_owned(),
    )
    .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION)
    .with_decision_text(DEC_TASK_TEXT)
    .with_rationale_summary(DEC_TASK_RATIONALE);
    stamp_memory(&mut dec_task, vec![]);
    let dec_task_edge = GraphRecord::edge(
        EdgeLabel::ReferencesTask,
        DEC_TASK_ID.to_owned(),
        TASK_ID.to_owned(),
        Some("1.0".to_owned()),
        "decision references task".to_owned(),
    );
    (dec_sym, dec_task, dec_task_edge)
}

/// Fixture records: 1 symbol, 1 task, 1 commit, 2 decisions (one per linkage
/// mechanism), 1 observation, 1 failure.
fn fixture_records() -> Vec<GraphRecord> {
    let symbol = GraphRecord::node(
        SYM_ID.to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some(SYM_NAME.to_owned()),
        format!("symbol {SYM_NAME}"),
    )
    .with_domain("codegraph", SCHEMA_VERSION);

    let task = GraphRecord::node(
        TASK_ID.to_owned(),
        NodeKind::Task,
        None,
        None,
        Some("decide the rollout".to_owned()),
        "Task: decide the rollout".to_owned(),
    )
    .with_domain("project", PROJECT_SCHEMA_VERSION);

    let commit = GraphRecord::node(
        COMMIT_ID.to_owned(),
        NodeKind::Commit,
        None,
        None,
        None,
        "commit abc123".to_owned(),
    )
    .with_domain("codegraph", SCHEMA_VERSION);

    let (dec_sym, dec_task, dec_task_edge) = decision_records();

    // An observation and a failure: they must stay in `observations`
    // (section contract: Observation + Failure only).
    let mut obs = GraphRecord::node(
        OBS_ID.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation text".to_owned(),
    )
    .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION);
    stamp_memory(
        &mut obs,
        vec![evidence_link(SYM_ID, "OBSERVES", "codegraph")],
    );

    let mut fail = GraphRecord::node(
        FAIL_ID.to_owned(),
        NodeKind::Failure,
        None,
        None,
        None,
        "failure text".to_owned(),
    )
    .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION);
    stamp_memory(
        &mut fail,
        vec![evidence_link(TASK_ID, "FAILED_ON", "project")],
    );

    vec![
        symbol,
        task,
        commit,
        dec_sym,
        dec_task,
        dec_task_edge,
        obs,
        fail,
    ]
}

/// Fixture store: the fixture records serialized as JSONL in a temp dir.
fn fixture_store() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("decisions.jsonl");

    let mut graph = Graph::new();
    for record in fixture_records() {
        graph.push(record);
    }
    let jsonl = graph.to_jsonl().expect("fixture serializes");
    fs::write(&path, jsonl).expect("write fixture");
    (temp, path)
}

fn run_query(args: &[&str], graph: &PathBuf) -> Value {
    let output = egregore()
        .args(["query"])
        .args(args)
        .arg("--graph")
        .arg(graph)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(std::str::from_utf8(&output).expect("utf8").trim())
        .expect("stdout must be valid JSON")
}

fn decisions(parsed: &Value) -> Vec<&Value> {
    parsed["decisions"]
        .as_array()
        .expect("decisions section")
        .iter()
        .collect()
}

fn observation_ids(parsed: &Value) -> Vec<&str> {
    parsed["observations"]
        .as_array()
        .expect("observations section")
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect()
}

/// AC1/AC3 (symbol lane): the symbol-linked decision surfaces in
/// `decisions` with its rationale, never in `observations`.
#[test]
fn query_context_surfaces_symbol_linked_decision_with_rationale() {
    let (_temp, graph) = fixture_store();
    let parsed = run_query(&["context", SYM_NAME], &graph);

    assert_eq!(parsed["ok"], true);
    let decs = decisions(&parsed);
    let row = decs
        .iter()
        .find(|r| r["record_id"].as_str() == Some(DEC_SYM_ID))
        .unwrap_or_else(|| panic!("decisions must contain {DEC_SYM_ID}: {decs:?}"));

    // AC2: the row carries decision_text, rationale_summary, record_id,
    // confidence, and a provenance handle.
    assert_eq!(row["decision_text"], DEC_SYM_TEXT);
    let rationale = row["rationale_summary"]
        .as_str()
        .expect("rationale_summary");
    assert!(!rationale.is_empty(), "rationale_summary must be non-empty");
    assert_eq!(rationale, DEC_SYM_RATIONALE);
    assert_eq!(row["record_id"], DEC_SYM_ID);
    assert_eq!(row["confidence"], "0.9");
    assert!(
        row["provenance_handle"].as_str().is_some(),
        "decision row must carry a provenance handle: {row:?}"
    );

    // Resolved EXPLAINS_CHANGE evidence handle to the commit.
    let handles = row["evidence_handles"]
        .as_array()
        .expect("evidence_handles");
    assert!(
        handles.iter().any(|h| h["relation"] == "EXPLAINS_CHANGE"
            && h["target_record_id"] == COMMIT_ID
            && h["target_kind"] == "Commit"),
        "EXPLAINS_CHANGE handle to the commit must resolve: {handles:?}"
    );

    // AC3: no Decision record inside `observations`; the plain observation
    // stays there (section contract).
    let obs_ids = observation_ids(&parsed);
    assert!(
        !obs_ids.contains(&DEC_SYM_ID),
        "decision must not appear in observations: {obs_ids:?}"
    );
    assert!(
        obs_ids.contains(&OBS_ID),
        "observation must stay in observations: {obs_ids:?}"
    );
}

/// AC1/AC3 (task lane): the task-linked decision surfaces in `decisions`
/// with its rationale, never in `observations`.
#[test]
fn query_task_surfaces_task_linked_decision_with_rationale() {
    let (_temp, graph) = fixture_store();
    let parsed = run_query(&["task", TASK_ID], &graph);

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["task_id"], TASK_ID);
    let decs = decisions(&parsed);
    let row = decs
        .iter()
        .find(|r| r["record_id"].as_str() == Some(DEC_TASK_ID))
        .unwrap_or_else(|| panic!("decisions must contain {DEC_TASK_ID}: {decs:?}"));

    assert_eq!(row["decision_text"], DEC_TASK_TEXT);
    let rationale = row["rationale_summary"]
        .as_str()
        .expect("rationale_summary");
    assert!(!rationale.is_empty(), "rationale_summary must be non-empty");
    assert_eq!(rationale, DEC_TASK_RATIONALE);
    assert_eq!(row["record_id"], DEC_TASK_ID);

    // Resolved REFERENCES_TASK evidence handle to the task.
    let handles = row["evidence_handles"]
        .as_array()
        .expect("evidence_handles");
    assert!(
        handles.iter().any(|h| h["relation"] == "REFERENCES_TASK"
            && h["target_record_id"] == TASK_ID
            && h["target_kind"] == "Task"),
        "REFERENCES_TASK handle to the task must resolve: {handles:?}"
    );

    // AC3 + AC4: no Decision record inside `observations`; the Failure stays
    // there per the section contract (documented, not silently flattened).
    let obs_ids = observation_ids(&parsed);
    assert!(
        !obs_ids.contains(&DEC_TASK_ID),
        "decision must not appear in observations: {obs_ids:?}"
    );
    assert!(
        obs_ids.contains(&FAIL_ID),
        "failure must stay in observations per the section contract: {obs_ids:?}"
    );
}

/// Success metric: in a store with N Decision records, 100% surface in
/// `decisions` with non-empty `rationale_summary`, 0% in `observations`.
#[test]
fn decision_section_split_metric_holds_across_both_lanes() {
    let (_temp, graph) = fixture_store();
    let decision_ids = [DEC_SYM_ID, DEC_TASK_ID];

    let symbol_parsed = run_query(&["context", SYM_NAME], &graph);
    let task_parsed = run_query(&["task", TASK_ID], &graph);

    let mut surfaced = 0;
    for (parsed, lane) in [(&symbol_parsed, "symbol"), (&task_parsed, "task")] {
        let decs = decisions(parsed);
        let obs_ids = observation_ids(parsed);
        for id in decision_ids {
            assert!(
                !obs_ids.contains(&id),
                "[{lane}] decision {id} leaked into observations: {obs_ids:?}"
            );
            if let Some(row) = decs.iter().find(|r| r["record_id"].as_str() == Some(id)) {
                let rationale = row["rationale_summary"].as_str().unwrap_or_default();
                assert!(
                    !rationale.is_empty(),
                    "[{lane}] decision {id} surfaced without rationale"
                );
                surfaced += 1;
            }
        }
    }
    // Each decision surfaces in its own lane: the symbol-linked one in
    // `query context`, the task-linked one in `query task`.
    assert_eq!(
        surfaced,
        decision_ids.len(),
        "100% of Decision records must surface in decisions with rationale"
    );
}
