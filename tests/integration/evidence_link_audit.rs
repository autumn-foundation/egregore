#![allow(missing_docs, clippy::too_many_lines)]

//! Integration tests for `eg audit evidence-links` (issue #217): exit codes,
//! JSON field stability, redaction safety, read-only behavior, and determinism.

use std::collections::BTreeSet;
use std::path::Path;

use aletheia_egregore::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, GraphRecord, NodeKind,
    PROJECT_SCHEMA_VERSION, SourceSpan,
};
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn mk_span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 42,
        start_line,
        end_line,
    }
}

fn link(target: &str, target_domain: &str, relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: target_domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

fn observation(id: &str, links: Vec<EvidenceLink>) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "observation".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        session_id,
        text,
        evidence_links,
        ..
    } = &mut rec
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("agent_1".to_owned());
        *session_id = Some("session_1".to_owned());
        *text = Some("SENTINEL_TRANSCRIPT_TEXT".to_owned());
        if !links.is_empty() {
            *evidence_links = Some(links);
        }
    }
    rec
}

fn project_node(id: &str, kind: NodeKind) -> GraphRecord {
    let mut rec = GraphRecord::node(id.to_owned(), kind, None, None, None, "project".to_owned());
    if let GraphRecord::Node {
        schema_version,
        domain,
        ..
    } = &mut rec
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *domain = Some("project".to_owned());
    }
    rec
}

fn symbol(id: &str, path: &str, span: SourceSpan) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span),
        Some("sym".to_owned()),
        "symbol".to_owned(),
    )
}

fn edge(id: &str, label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::Edge {
        id: id.to_owned(),
        schema_version: 1,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence: None,
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        basis: None,
        temporal: None,
        summary: "SENTINEL_EDGE_SUMMARY".to_owned(),
        producer: None,
    }
}

fn tombstone(id: &str, deleted_id: &str) -> GraphRecord {
    GraphRecord::Tombstone {
        id: id.to_owned(),
        schema_version: 1,
        deleted_id: deleted_id.to_owned(),
        summary: "removed".to_owned(),
        producer: None,
    }
}

/// A seeded broken store spanning >=3 source domains and both target-absence
/// cases (issue #217 AC1): agent_memory OBSERVES a tombstoned code symbol,
/// agent_memory VALIDATED_BY an absent verification record, and a project
/// CLOSES_ACCEPTANCE_CRITERION edge to an absent record.
fn broken_records() -> Vec<GraphRecord> {
    vec![
        // Tombstoned code symbol (path/span recoverable).
        symbol("codegraph:v1:sym_dead", "src/lib.rs", mk_span(10, 20)),
        tombstone("codegraph:v1:tomb_sym", "codegraph:v1:sym_dead"),
        // agent_memory -> tombstoned code symbol (OBSERVES) + absent verification.
        observation(
            "agent_memory:v1:obs1",
            vec![
                link("codegraph:v1:sym_dead", "codegraph", "OBSERVES"),
                link("verification:v1:ghost", "verification", "VALIDATED_BY"),
            ],
        ),
        // project AcceptanceCriterion -> absent verification via a standalone edge.
        project_node("project:v1:ac1", NodeKind::AcceptanceCriterion),
        edge(
            "project:v1:edge_closes",
            EdgeLabel::ClosesAcceptanceCriterion,
            "project:v1:ac1",
            "verification:v1:ghost_ac",
        ),
    ]
}

/// A clean store: every checked evidence edge resolves to a live target.
fn clean_records() -> Vec<GraphRecord> {
    vec![
        symbol("codegraph:v1:sym_live", "src/a.rs", mk_span(1, 5)),
        observation(
            "agent_memory:v1:obs_clean",
            vec![link("codegraph:v1:sym_live", "codegraph", "OBSERVES")],
        ),
    ]
}

fn write_graph(dir: &Path, name: &str, records: &[GraphRecord]) -> std::path::PathBuf {
    let path = dir.join(name);
    let jsonl: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, jsonl).unwrap();
    path
}

// AC1/AC2/AC3/AC5: a seeded broken store reports exactly the planted defects
// with citable handles, distinguishes absent from tombstoned, and exits 1.
#[test]
fn broken_store_reports_defects_and_exits_1() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = write_graph(tmp.path(), "broken.jsonl", &broken_records());

    let output = egregore()
        .args([
            "audit",
            "evidence-links",
            "--graph",
            graph.to_str().unwrap(),
        ])
        .assert()
        .code(1);
    let report: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();

    assert_eq!(report["ok"], false);
    assert_eq!(report["broken_edge_count"], 3);
    assert_eq!(report["checked_edge_count"], 3);

    let broken = report["broken_edges"].as_array().unwrap();
    // The tombstoned code symbol row recovers path + span and is labeled tombstoned.
    let tomb_row = broken
        .iter()
        .find(|b| b["target_record_id"] == "codegraph:v1:sym_dead")
        .expect("tombstoned OBSERVES row");
    assert_eq!(tomb_row["case"], "tombstoned");
    assert_eq!(tomb_row["source_domain"], "agent_memory");
    assert_eq!(tomb_row["edge_label"], "OBSERVES");
    assert_eq!(tomb_row["representation"], "inline_evidence_link");
    assert_eq!(tomb_row["target_repo_relative_path"], "src/lib.rs");
    assert_eq!(tomb_row["target_span"]["start_line"], 10);

    // The absent verification target is labeled absent, with no recovered path.
    let absent_row = broken
        .iter()
        .find(|b| b["target_record_id"] == "verification:v1:ghost")
        .expect("absent VALIDATED_BY row");
    assert_eq!(absent_row["case"], "absent");
    assert!(absent_row.get("target_repo_relative_path").is_none());

    // The standalone project edge row cites the edge record id.
    let edge_row = broken
        .iter()
        .find(|b| b["target_record_id"] == "verification:v1:ghost_ac")
        .expect("absent CLOSES edge row");
    assert_eq!(edge_row["representation"], "edge_record");
    assert_eq!(edge_row["edge_record_id"], "project:v1:edge_closes");
    assert_eq!(edge_row["source_domain"], "project");

    // Per-domain and per-edge-label counts are present (AC7).
    assert_eq!(report["by_source_domain"]["agent_memory"], 2);
    assert_eq!(report["by_source_domain"]["project"], 1);
    assert_eq!(report["by_case"]["absent"], 2);
    assert_eq!(report["by_case"]["tombstoned"], 1);

    // Partition disclosure (AC).
    let checked = report["checked_edge_labels"].as_array().unwrap();
    assert!(checked.iter().any(|v| v == "OBSERVES"));
    let excluded = report["excluded_edge_labels"].as_array().unwrap();
    assert!(excluded.iter().any(|v| v == "CALLS"));
}

// AC5: a clean store reports a stable "no broken links" signal and exits 0.
#[test]
fn clean_store_exits_0() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = write_graph(tmp.path(), "clean.jsonl", &clean_records());

    let output = egregore()
        .args([
            "audit",
            "evidence-links",
            "--graph",
            graph.to_str().unwrap(),
        ])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["broken_edge_count"], 0);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "no_broken_evidence_links")
    );
}

// AC8: raw source/transcript/edge text never appears in the output.
#[test]
fn no_raw_payloads_in_output() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = write_graph(tmp.path(), "broken.jsonl", &broken_records());

    let output = egregore()
        .args([
            "audit",
            "evidence-links",
            "--graph",
            graph.to_str().unwrap(),
        ])
        .assert()
        .code(1);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("SENTINEL_TRANSCRIPT_TEXT"));
    assert!(!stdout.contains("SENTINEL_EDGE_SUMMARY"));
}

// AC9: five consecutive runs over an unchanged graph are byte-identical.
#[test]
fn output_byte_identical_across_five_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = write_graph(tmp.path(), "broken.jsonl", &broken_records());

    let first = egregore()
        .args([
            "audit",
            "evidence-links",
            "--graph",
            graph.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    for _ in 0..4 {
        let again = egregore()
            .args([
                "audit",
                "evidence-links",
                "--graph",
                graph.to_str().unwrap(),
            ])
            .assert()
            .code(1)
            .get_output()
            .stdout
            .clone();
        assert_eq!(first, again, "byte-identical across runs");
    }
}

// A human-readable text form is available (AC7).
#[test]
fn text_format_renders() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = write_graph(tmp.path(), "broken.jsonl", &broken_records());

    let output = egregore()
        .args([
            "audit",
            "evidence-links",
            "--graph",
            graph.to_str().unwrap(),
            "--format",
            "text",
        ])
        .assert()
        .code(1);
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("broken evidence edges: 3"));
    assert!(stdout.contains("by source domain:"));
    assert!(stdout.contains("by edge label:"));
}

// AC5: usage/load errors exit 2.
#[test]
fn usage_and_load_errors_exit_2() {
    let tmp = tempfile::tempdir().unwrap();

    // Missing both flags.
    egregore()
        .args(["audit", "evidence-links"])
        .assert()
        .code(2);

    // Nonexistent graph path.
    egregore()
        .args(["audit", "evidence-links", "--graph", "does_not_exist.jsonl"])
        .assert()
        .code(2);

    // Empty graph is a load error naming the path (not a false clean pass).
    let empty = tmp.path().join("empty.jsonl");
    std::fs::write(&empty, "").unwrap();
    let output = egregore()
        .args([
            "audit",
            "evidence-links",
            "--graph",
            empty.to_str().unwrap(),
        ])
        .assert()
        .code(2);
    let stderr = String::from_utf8(output.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("empty_evidence_input"));

    // Both flags together.
    let graph = write_graph(tmp.path(), "clean.jsonl", &clean_records());
    egregore()
        .args([
            "audit",
            "evidence-links",
            "--graph",
            graph.to_str().unwrap(),
            "--data-dir",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

/// Recursively snapshots (relative path, bytes) of every file under `dir`.
fn snapshot_dir(dir: &Path) -> BTreeSet<(String, Vec<u8>)> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                out.insert((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    out
}

// AC6: running over a `--data-dir` store mutates zero files. The read-only
// guarantee is independent of the verdict, so this uses a clean ingestable store
// (broken-edge behavior is covered by the `--graph` tests above; the embedded
// write path validates referential integrity, so a dangling store is seeded
// through `--graph`, not `ingest`).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_store_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = write_graph(tmp.path(), "clean.jsonl", &clean_records());
    let store = tmp.path().join("store");

    // Ingest the seeded records into an embedded store.
    egregore()
        .args([
            "ingest",
            graph.to_str().unwrap(),
            "--adapter",
            "embedded",
            "--data-dir",
            store.to_str().unwrap(),
        ])
        .assert()
        .success();

    let before = snapshot_dir(&store);
    egregore()
        .args([
            "audit",
            "evidence-links",
            "--data-dir",
            store.to_str().unwrap(),
        ])
        .assert()
        .success();
    let after = snapshot_dir(&store);
    assert_eq!(before, after, "the audit must not mutate the store");
}
