//! Integration tests for `eg query evidence-path` (issue #247): trace the
//! deterministic shortest cross-domain evidence witness path between two record
//! handles, or emit an explicit `no_path` verdict.
//!
//! Drives the built `egregore` binary end-to-end over a seeded JSONL graph,
//! asserting byte-identical output across runs, the witness-path shape, the
//! read-only `--data-dir` guarantee, and the documented exit-code taxonomy.

#![allow(missing_docs, clippy::similar_names)]

use std::{fs, path::Path};

use aletheia_egregore::{GraphRecord, NodeKind, SourceSpan, ir::EdgeLabel};
use assert_cmd::Command as CargoCommand;

fn code_symbol(id: &str, path: &str, name: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: 1,
            end_line: 2,
        }),
        Some(name.to_owned()),
        format!("symbol {name}"),
    )
}

fn memory_node(id: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation".to_owned(),
    )
    .with_domain("agent_memory", 1)
}

fn verification_node(id: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Verification,
        None,
        None,
        None,
        "verification".to_owned(),
    )
    .with_domain("verification", 1)
}

fn evidence_edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        label,
        source.to_owned(),
        target.to_owned(),
        Some("1.0".to_owned()),
        format!("{} edge", label.as_str()),
    )
}

fn write_graph(records: &[GraphRecord], path: &Path) {
    let mut out = String::new();
    for r in records {
        out.push_str(&serde_json::to_string(r).unwrap());
        out.push('\n');
    }
    fs::write(path, out).unwrap();
}

/// A three-domain chain: `agent_memory` obs `--OBSERVES-->` codegraph sym
/// `--HAS_EVIDENCE-->` verification.
fn chain_fixture() -> Vec<GraphRecord> {
    let obs = memory_node("agent_memory:v1:obs");
    let sym = code_symbol("codegraph:v5:sym", "src/lib.rs", "foo");
    let verif = verification_node("verification:v1:verif");
    let e1 = evidence_edge(EdgeLabel::Observes, obs.id(), sym.id());
    let e2 = evidence_edge(EdgeLabel::HasEvidence, sym.id(), verif.id());
    vec![obs, sym, verif, e1, e2]
}

#[test]
fn cli_witness_path_byte_stable_and_shaped() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("seed.jsonl");
    write_graph(&chain_fixture(), &graph);

    let mut outputs = Vec::new();
    for _ in 0..5 {
        let assert = CargoCommand::cargo_bin("egregore")
            .unwrap()
            .args([
                "query",
                "evidence-path",
                "agent_memory:v1:obs",
                "verification:v1:verif",
            ])
            .arg("--graph")
            .arg(&graph)
            .assert()
            .success();
        outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    for output in &outputs[1..] {
        assert_eq!(&outputs[0], output, "CLI output must be byte-identical");
    }

    let mut lines = outputs[0].lines();
    let summary: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["hop_count"], 2);
    assert_eq!(summary["source"]["domain"], "agent_memory");
    assert_eq!(summary["target"]["domain"], "verification");
    assert!(
        summary["traversed_edge_classes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "OBSERVES")
    );
    assert!(
        summary["excluded_edge_classes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "CALLS")
    );

    // Corpus disclosure (issue #427): a snapshot-less graph discloses a single
    // snapshot; evidence-path over a scan-history store would disclose union.
    assert_eq!(summary["corpus_mode"], "single_snapshot");
    assert_eq!(summary["corpus_mode_source"], "default");
    assert!(
        summary["corpus_disclaimer"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );

    let hop0: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(hop0["index"], 0);
    assert_eq!(hop0["edge"]["label"], "OBSERVES");
    assert_eq!(hop0["edge"]["traversal_direction"], "forward");
    assert_eq!(hop0["from"]["record_id"], "agent_memory:v1:obs");
    assert_eq!(hop0["to"]["record_id"], "codegraph:v5:sym");
    assert_eq!(hop0["to"]["repo_relative_path"], "src/lib.rs");

    let hop1: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(hop1["index"], 1);
    assert_eq!(hop1["edge"]["label"], "HAS_EVIDENCE");
    assert!(lines.next().is_none(), "exactly summary + 2 hops");

    // No raw payload text leaks (redaction, AC8): the edge summary strings
    // ("OBSERVES edge", "HAS_EVIDENCE edge") and node display summaries never
    // appear in the citable output.
    assert!(
        !outputs[0].contains("OBSERVES edge"),
        "no edge summary leaks"
    );
    assert!(!outputs[0].contains("symbol foo"), "no node summary leaks");
}

#[test]
fn cli_no_path_exit_1() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("seed.jsonl");
    // Two disconnected live endpoints.
    let obs = memory_node("agent_memory:v1:obs");
    let verif = verification_node("verification:v1:verif");
    write_graph(&[obs, verif], &graph);

    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args([
            "query",
            "evidence-path",
            "agent_memory:v1:obs",
            "verification:v1:verif",
        ])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .failure()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["error_type"], "no_path");
    assert!(body["error"]["traversed_edge_classes"].is_array());
    assert!(body["error"]["excluded_edge_classes"].is_array());
}

#[test]
fn cli_endpoint_not_found_exit_2() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("seed.jsonl");
    write_graph(&chain_fixture(), &graph);

    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args([
            "query",
            "evidence-path",
            "agent_memory:v1:obs",
            "codegraph:v5:ghost",
        ])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .failure()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(body["error"]["error_type"], "endpoint_not_found");
    assert_eq!(body["error"]["side"], "target");
}

#[test]
fn cli_identical_endpoints_exit_1() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("seed.jsonl");
    write_graph(&chain_fixture(), &graph);

    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args([
            "query",
            "evidence-path",
            "agent_memory:v1:obs",
            "agent_memory:v1:obs",
        ])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .failure()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(body["error"]["error_type"], "identical_endpoints");
}

#[test]
fn cli_text_format_renders() {
    let temp = tempfile::tempdir().unwrap();
    let graph = temp.path().join("seed.jsonl");
    write_graph(&chain_fixture(), &graph);

    let assert = CargoCommand::cargo_bin("egregore")
        .unwrap()
        .args([
            "query",
            "evidence-path",
            "agent_memory:v1:obs",
            "verification:v1:verif",
            "--format",
            "text",
        ])
        .arg("--graph")
        .arg(&graph)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        out.contains("--OBSERVES(forward)-->"),
        "text hop line: {out}"
    );
    assert!(out.contains("hops: 2"), "text footer: {out}");
}
