//! Integration tests for `eg link-logs` — linking error signatures to the
//! agent runs and tasks that preceded them (issue #323).
//!
//! Seeds a union graph of real `scan-logs` output plus agent-memory /
//! verification / project records and asserts each acceptance criterion:
//! `content_hash_join` (confidence 1.0), `temporal_correlation` (confidence
//! 0.5, one edge per overlapping run), the `uncorrelated` tally, cross-repository
//! rejection, `REFERENCES_TASK` reuse, dual-representation agreement,
//! 5-run byte-identical idempotency with zero duplicate edges, and no raw
//! payload leak.

#![allow(missing_docs)]

use std::{collections::BTreeSet, fs, path::Path};

use aletheia_egregore::{
    Graph,
    ir::{GraphRecord, LogPayload, NodeKind, OutputHandle},
    log_graph,
};
use assert_cmd::Command;
use serde_json::Value;

const FIXED_TIME: &str = "2026-07-01T00:00:00Z";
const ANCHOR: &str = "codegraph:v1:repo_main";
const FOREIGN_REPO: &str = "codegraph:v1:repo_other";

const RUN_IN: &str = "agent_memory:v1:run_in";
const RUN_OVERLAP: &str = "agent_memory:v1:run_overlap";
const RUN_OUT: &str = "agent_memory:v1:run_out";
const CMD: &str = "verification:v1:cmd_hash";
const TASK: &str = "project:v1:task_alpha";

const RAW_SECRET: &str = "SUPERSECRETtokenValue1234567890";

const SIG_TIME: &str = "2026-07-01T12:00:00Z";

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

/// Scans one log file through the real capture path and returns its records.
fn scan_log(dir: &Path, repo_id: &str, filename: &str, content: &str) -> Vec<GraphRecord> {
    let path = dir.join(filename);
    fs::write(&path, content).expect("write log fixture");
    let scan = log_graph::scan_log_records(&path, dir, repo_id, FIXED_TIME, false)
        .expect("scan should succeed");
    let producer = log_graph::log_importer_producer(scan.source_format_version, FIXED_TIME);
    let mut g = Graph::new();
    for record in scan.records {
        g.push(record);
    }
    g.stamp_producer(&producer).records().to_vec()
}

/// Returns the `source_artifact_hash` of the single `LogSource` in `records`.
fn log_source_hash(records: &[GraphRecord]) -> String {
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::LogSource,
            log: Some(payload),
            ..
        } = r
            && let LogPayload::LogSource(p) = payload.as_ref()
        {
            return p.source_artifact_hash.clone();
        }
    }
    panic!("no LogSource in scanned records");
}

fn node(id: &str, kind: NodeKind, summary: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        Some(summary.to_owned()),
        summary.to_owned(),
    )
}

/// A node stamped with a specific (non-code-graph) domain schema version, so it
/// round-trips through the JSONL parser's `(domain, kind, schema_version)` gate.
fn dom_node(id: &str, kind: NodeKind, summary: &str, schema_version: u32) -> GraphRecord {
    let mut n = node(id, kind, summary);
    if let GraphRecord::Node {
        schema_version: sv, ..
    } = &mut n
    {
        *sv = schema_version;
    }
    n
}

fn agent_run(id: &str, start: &str, end: &str) -> GraphRecord {
    let mut n = dom_node(id, NodeKind::AgentRun, "agent run", 1);
    if let GraphRecord::Node {
        started_at,
        finished_at,
        ..
    } = &mut n
    {
        *started_at = Some(start.to_owned());
        *finished_at = Some(end.to_owned());
    }
    n
}

fn command_run(id: &str, stderr_hash: &str) -> GraphRecord {
    let mut n = dom_node(id, NodeKind::CommandRun, "command run", 1);
    if let GraphRecord::Node { stderr_handle, .. } = &mut n {
        *stderr_handle = Some(Box::new(OutputHandle {
            inline: None,
            hash: stderr_hash.to_owned(),
            bytes: 42,
        }));
    }
    n
}

/// Builds the combined graph JSONL for the whole fixture.
fn fixture_jsonl(dir: &Path) -> String {
    // In-repo primary log → S1 (content-hash + temporal). Timestamp is inside
    // run_in and run_overlap windows.
    let file_a = scan_log(
        dir,
        ANCHOR,
        "app_a.log",
        &format!("{SIG_TIME} [ERROR] primary boom failure alpha\n"),
    );
    let hash_a = log_source_hash(&file_a);

    // In-repo secondary log → S2. Timestamp is outside every window and its hash
    // matches no command; carries a secret that must be redacted.
    let file_b = scan_log(
        dir,
        ANCHOR,
        "app_b.log",
        &format!("2026-07-09T00:00:00Z [ERROR] auth failed API_KEY={RAW_SECRET} denied beta\n"),
    );

    // Foreign-repo log → S_F. Same timestamp as S1 (inside run_in) but scanned
    // under a different repository id, so it must be repository-rejected.
    let file_f = scan_log(
        dir,
        FOREIGN_REPO,
        "foreign.log",
        &format!("{SIG_TIME} [ERROR] foreign boom failure gamma\n"),
    );

    let mut g = Graph::new();
    for r in file_a.into_iter().chain(file_b).chain(file_f) {
        g.push(r);
    }
    // Repository anchor (single → temporal enabled).
    g.push(node(ANCHOR, NodeKind::Repository, "repo main"));
    // Agent runs: two overlap S1, one does not.
    g.push(agent_run(
        RUN_IN,
        "2026-07-01T11:00:00Z",
        "2026-07-01T13:00:00Z",
    ));
    g.push(agent_run(
        RUN_OVERLAP,
        "2026-07-01T11:30:00Z",
        "2026-07-01T12:30:00Z",
    ));
    g.push(agent_run(
        RUN_OUT,
        "2026-07-20T11:00:00Z",
        "2026-07-20T13:00:00Z",
    ));
    // A command whose stderr hash equals the primary log's artifact hash.
    g.push(command_run(CMD, &hash_a));
    // A task the in-window run already references.
    g.push(dom_node(TASK, NodeKind::Task, "task alpha", 1));
    g.push(GraphRecord::agent_memory_edge(
        aletheia_egregore::ir::EdgeLabel::ReferencesTask,
        RUN_IN.to_owned(),
        TASK.to_owned(),
        None,
        "run references task".to_owned(),
    ));

    g.to_jsonl().expect("serialize fixture graph")
}

fn parse_records(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSON line"))
        .collect()
}

/// Runs link-logs and returns (out JSONL, stdout envelope).
fn run_link(dir: &Path) -> (String, Value) {
    let graph_path = dir.join("union.graph.jsonl");
    fs::write(&graph_path, fixture_jsonl(dir)).expect("write fixture");
    let out_path = dir.join("linked.jsonl");

    let assert = egregore()
        .arg("link-logs")
        .arg("--graph")
        .arg(&graph_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("envelope JSON");
    let out = fs::read_to_string(&out_path).expect("read out");
    (out, envelope)
}

fn emitted_edges(records: &[Value]) -> Vec<&Value> {
    records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(Value::as_str) == Some("edge")
                && r.get("label").and_then(Value::as_str) == Some("EMITTED_DURING")
        })
        .collect()
}

#[test]
fn content_hash_join_and_temporal_bases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (out, envelope) = run_link(dir.path());
    let records = parse_records(&out);
    let edges = emitted_edges(&records);

    // Exactly three EMITTED_DURING edges: 1 content-hash + 2 temporal.
    let hash_edges: Vec<&&Value> = edges
        .iter()
        .filter(|e| e.get("basis").and_then(Value::as_str) == Some("content_hash_join"))
        .collect();
    let temporal_edges: Vec<&&Value> = edges
        .iter()
        .filter(|e| e.get("basis").and_then(Value::as_str) == Some("temporal_correlation"))
        .collect();

    assert_eq!(hash_edges.len(), 1, "one content_hash_join edge");
    assert_eq!(
        hash_edges[0].get("target").and_then(Value::as_str),
        Some(CMD)
    );
    assert_eq!(
        hash_edges[0].get("confidence").and_then(Value::as_str),
        Some("1.0"),
        "content_hash_join carries confidence 1.0"
    );

    assert_eq!(temporal_edges.len(), 2, "one edge per overlapping run");
    let mut targets: Vec<&str> = temporal_edges
        .iter()
        .filter_map(|e| e.get("target").and_then(Value::as_str))
        .collect();
    targets.sort_unstable();
    assert_eq!(
        targets,
        vec![RUN_IN, RUN_OVERLAP],
        "both overlapping runs get an edge; no silent winner"
    );
    for e in &temporal_edges {
        assert_eq!(
            e.get("confidence").and_then(Value::as_str),
            Some("0.5"),
            "temporal_correlation carries lower confidence 0.5"
        );
    }

    // No edge ever lacks a basis.
    for e in &edges {
        assert!(
            e.get("basis").and_then(Value::as_str).is_some(),
            "every EMITTED_DURING edge carries a basis"
        );
    }

    let totals = &envelope["totals"];
    assert_eq!(totals["content_hash_join_edges"].as_u64(), Some(1));
    assert_eq!(totals["temporal_correlation_edges"].as_u64(), Some(2));
    assert_eq!(totals["temporal_correlation_enabled"].as_bool(), Some(true));
    assert_eq!(totals["tolerance_seconds"].as_u64(), Some(0));
}

#[test]
fn uncorrelated_and_cross_repo_are_tallied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_out, envelope) = run_link(dir.path());
    let totals = &envelope["totals"];
    // S2 (out of window, no hash) and S_F (repo-rejected) both received zero
    // edges → uncorrelated; S_F is additionally repository-rejected.
    assert_eq!(
        totals["uncorrelated"].as_u64(),
        Some(2),
        "signatures with zero edges are reported, never hidden"
    );
    assert_eq!(
        totals["cross_repo_rejected"].as_u64(),
        Some(1),
        "the foreign-repository signature's in-window candidate is rejected"
    );
}

#[test]
fn task_link_reuses_references_task() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (out, envelope) = run_link(dir.path());
    let records = parse_records(&out);
    let task_edges: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(Value::as_str) == Some("edge")
                && r.get("label").and_then(Value::as_str) == Some("REFERENCES_TASK")
        })
        .collect();
    assert_eq!(task_edges.len(), 1, "one signature->task reference");
    assert_eq!(
        task_edges[0].get("target").and_then(Value::as_str),
        Some(TASK)
    );
    // REFERENCES_TASK carries no correlation basis.
    assert!(task_edges[0].get("basis").is_none());
    assert_eq!(envelope["totals"]["task_link_edges"].as_u64(), Some(1));
}

#[test]
fn node_evidence_links_agree_with_edges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (out, _envelope) = run_link(dir.path());
    let records = parse_records(&out);

    let mut edge_triples: Vec<(String, String, String)> = records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(Value::as_str) == Some("edge")
                && matches!(
                    r.get("label").and_then(Value::as_str),
                    Some("EMITTED_DURING" | "REFERENCES_TASK")
                )
        })
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_owned(),
                e["label"].as_str().unwrap().to_owned(),
                e["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    edge_triples.sort();

    let mut link_triples: Vec<(String, String, String)> = Vec::new();
    for r in &records {
        if r.get("kind").and_then(Value::as_str) == Some("ErrorSignature")
            && let Some(links) = r.get("evidence_links").and_then(Value::as_array)
        {
            let sig_id = r["id"].as_str().unwrap().to_owned();
            for link in links {
                link_triples.push((
                    sig_id.clone(),
                    link["relation"].as_str().unwrap().to_owned(),
                    link["target_record_id"].as_str().unwrap().to_owned(),
                ));
            }
        }
    }
    link_triples.sort();

    assert!(!edge_triples.is_empty(), "some link edges were minted");
    assert_eq!(
        edge_triples, link_triples,
        "node evidence links must agree with emitted edges (dual representation)"
    );
}

#[test]
fn idempotent_and_byte_identical_across_five_runs() {
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let dir = tempfile::tempdir().expect("tempdir");
        let (out, _env) = run_link(dir.path());
        // Zero duplicate edge ids within a run.
        let records = parse_records(&out);
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for r in &records {
            if r.get("record_type").and_then(Value::as_str) == Some("edge") {
                let id = r["id"].as_str().unwrap().to_owned();
                assert!(ids.insert(id), "no duplicate edge ids");
            }
        }
        outputs.push(out);
    }
    for w in outputs.windows(2) {
        assert_eq!(w[0], w[1], "link-logs output is byte-identical across runs");
    }
}

#[test]
fn output_contains_no_raw_payload_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (out, envelope) = run_link(dir.path());
    assert!(
        !out.contains(RAW_SECRET),
        "the raw secret must never appear in linked output"
    );
    assert!(
        !serde_json::to_string(&envelope)
            .unwrap()
            .contains(RAW_SECRET),
        "the raw secret must never appear in the envelope"
    );
}
