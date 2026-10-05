#![allow(missing_docs)]

//! Integration tests for `eg audit memory-evidence-health` (issue #185):
//! store-wide classification of agent-memory evidence links into
//! `resolves_live` / `drifted` / `dangling`, plus denormalized-array vs
//! stored-edge integrity violations. Exit codes, NDJSON field contract,
//! determinism, and the `--format text` human view.

use std::fs;

use aletheia_egregore::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, MetricKind,
    NodeKind, SEMANTIC_SCHEMA_VERSION, SelectionBasis, SemanticDriftMetadata, SourceSpan,
};
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
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

const fn mk_span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 42,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

fn memory_node(id: &str, kind: NodeKind, links: Vec<EvidenceLink>) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        kind,
        None,
        None,
        None,
        "memory node".to_owned(),
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

fn symbol(id: &str, path: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(mk_span(10, 20)),
        Some("sym".to_owned()),
        "symbol".to_owned(),
    )
}

fn edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        label,
        source.to_owned(),
        target.to_owned(),
        None,
        "SENTINEL_EDGE_SUMMARY".to_owned(),
    )
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

fn drift_record(drift_id: &str, prior_id: &str, target_id: &str) -> Vec<GraphRecord> {
    let drift = SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider: "p".to_owned(),
            name: "m".to_owned(),
            version: "v".to_owned(),
            dim: 8,
            content_hash: "h".to_owned(),
        },
        target_record_id: target_id.to_owned(),
        prior_record_id: prior_id.to_owned(),
        before_git_commit: "aaa".to_owned(),
        after_git_commit: "bbb".to_owned(),
        before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
        after_valid_time: "2026-02-01T00:00:00Z".to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.7,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    };
    let node = GraphRecord::node(
        drift_id.to_owned(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "Drift".to_owned(),
    )
    .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
    .with_semantic_drift(drift);
    let edge = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id.to_owned(),
        target_id.to_owned(),
        None,
        "drifts from".to_owned(),
    );
    vec![node, edge]
}

/// Fixture store for issue #185:
/// - `sym_live`: live symbol, no drift.
/// - `sym_drifted`: live symbol with a live `SemanticDrift` record whose
///   `prior_record_id` is the symbol (drift applies).
/// - `sym_dead`: symbol covered by a tombstone (dangling target).
/// - `sym_absent`: never present (dangling target).
/// - `obs1` (Observation): one link per bucket target + the two dangling cases.
/// - `obs2` (Observation): one healthy link, one link with no stored edge
///   (`array_without_edge`), and one stored edge with no array entry
///   (`edge_without_array`).
/// - `dec1` (Decision): healthy link (proves Decision-class coverage).
/// - `fail1` (Failure): healthy link (proves Failure-class coverage).
fn fixture_records() -> Vec<GraphRecord> {
    let mut records = vec![
        symbol("codegraph:v1:sym_live", "src/lib.rs"),
        symbol("codegraph:v1:sym_drifted", "src/drift.rs"),
        symbol("codegraph:v1:sym_dead", "src/dead.rs"),
        tombstone("codegraph:v1:tomb_sym", "codegraph:v1:sym_dead"),
    ];
    records.extend(drift_record(
        "semantic:v1:drift1",
        "codegraph:v1:sym_drifted",
        "codegraph:v1:sym_drifted_after",
    ));
    records.push(memory_node(
        "agent_memory:v1:obs1",
        NodeKind::Observation,
        vec![
            link("codegraph:v1:sym_live", "codegraph", "OBSERVES"),
            link("codegraph:v1:sym_drifted", "codegraph", "OBSERVES"),
            link("codegraph:v1:sym_absent", "codegraph", "OBSERVES"),
            link("codegraph:v1:sym_dead", "codegraph", "OBSERVES"),
        ],
    ));
    records.push(memory_node(
        "agent_memory:v1:obs2",
        NodeKind::Observation,
        vec![
            link("codegraph:v1:sym_live", "codegraph", "MENTIONS_SYMBOL"),
            // No stored edge for this link: array_without_edge.
            link("codegraph:v1:sym_live", "codegraph", "OBSERVES"),
        ],
    ));
    records.push(memory_node(
        "agent_memory:v1:dec1",
        NodeKind::Decision,
        vec![link("codegraph:v1:sym_live", "codegraph", "OBSERVES")],
    ));
    records.push(memory_node(
        "agent_memory:v1:fail1",
        NodeKind::Failure,
        vec![link("codegraph:v1:sym_live", "codegraph", "FAILED_ON")],
    ));
    // Stored edges for every array entry except obs2's dangling OBSERVES link,
    // plus one edge (VALIDATED_BY from obs2) with no array entry.
    for (source, relation, target) in [
        (
            "agent_memory:v1:obs1",
            EdgeLabel::Observes,
            "codegraph:v1:sym_live",
        ),
        (
            "agent_memory:v1:obs1",
            EdgeLabel::Observes,
            "codegraph:v1:sym_drifted",
        ),
        (
            "agent_memory:v1:obs1",
            EdgeLabel::Observes,
            "codegraph:v1:sym_absent",
        ),
        (
            "agent_memory:v1:obs1",
            EdgeLabel::Observes,
            "codegraph:v1:sym_dead",
        ),
        (
            "agent_memory:v1:obs2",
            EdgeLabel::MentionsSymbol,
            "codegraph:v1:sym_live",
        ),
        (
            "agent_memory:v1:dec1",
            EdgeLabel::Observes,
            "codegraph:v1:sym_live",
        ),
        (
            "agent_memory:v1:fail1",
            EdgeLabel::FailedOn,
            "codegraph:v1:sym_live",
        ),
        // Edge with no array entry: edge_without_array.
        (
            "agent_memory:v1:obs2",
            EdgeLabel::ValidatedBy,
            "codegraph:v1:sym_live",
        ),
    ] {
        records.push(edge(relation, source, target));
    }
    records
}

fn write_graph(records: &[GraphRecord]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("temp graph file");
    let mut out = String::new();
    for record in records {
        out.push_str(&serde_json::to_string(record).expect("record serializes"));
        out.push('\n');
    }
    fs::write(file.path(), out).expect("write graph");
    file
}

fn run_audit(graph: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = egregore();
    cmd.arg("audit")
        .arg("memory-evidence-health")
        .arg("--graph")
        .arg(graph);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.output().expect("run eg")
}

fn ndjson_lines(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8(stdout.to_vec()).expect("stdout is utf8");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("line is valid JSON"))
        .collect()
}

fn link_row<'a>(lines: &'a [Value], source: &str, target: &str) -> &'a Value {
    lines
        .iter()
        .find(|line| {
            line["type"] == "link"
                && line["source_record_id"] == source
                && line["target_record_id"] == target
        })
        .unwrap_or_else(|| panic!("missing link row {source} -> {target}"))
}

#[test]
fn buckets_classify_every_link() {
    let graph = write_graph(&fixture_records());
    let output = run_audit(graph.path(), &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "findings (dangling + integrity violations) exit 1: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = ndjson_lines(&output.stdout);

    // AC2: every link lands in exactly one bucket.
    let row = link_row(&lines, "agent_memory:v1:obs1", "codegraph:v1:sym_live");
    assert_eq!(row["bucket"], "resolves_live");
    assert_eq!(row["source_kind"], "Observation");
    assert_eq!(row["relation"], "OBSERVES");

    let row = link_row(&lines, "agent_memory:v1:obs1", "codegraph:v1:sym_drifted");
    assert_eq!(row["bucket"], "drifted");
    assert_eq!(row["drift_record_id"], "semantic:v1:drift1");

    let row = link_row(&lines, "agent_memory:v1:obs1", "codegraph:v1:sym_absent");
    assert_eq!(row["bucket"], "dangling");
    assert_eq!(row["tombstoned"], false);

    let row = link_row(&lines, "agent_memory:v1:obs1", "codegraph:v1:sym_dead");
    assert_eq!(row["bucket"], "dangling");
    assert_eq!(row["tombstoned"], true);

    // Observation-class coverage: Decision and Failure rows are emitted too.
    let row = link_row(&lines, "agent_memory:v1:dec1", "codegraph:v1:sym_live");
    assert_eq!(row["bucket"], "resolves_live");
    assert_eq!(row["source_kind"], "Decision");
    let row = link_row(&lines, "agent_memory:v1:fail1", "codegraph:v1:sym_live");
    assert_eq!(row["bucket"], "resolves_live");
    assert_eq!(row["source_kind"], "Failure");

    // A link with no stored edge still classifies by target health.
    let rows: Vec<&Value> = lines
        .iter()
        .filter(|line| {
            line["type"] == "link"
                && line["source_record_id"] == "agent_memory:v1:obs2"
                && line["relation"] == "OBSERVES"
        })
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["bucket"], "resolves_live");
}

#[test]
fn integrity_violations_flag_array_edge_disagreement() {
    let graph = write_graph(&fixture_records());
    let output = run_audit(graph.path(), &[]);
    assert_eq!(output.status.code(), Some(1));
    let lines = ndjson_lines(&output.stdout);
    let violations: Vec<&Value> = lines
        .iter()
        .filter(|line| line["type"] == "integrity_violation")
        .collect();
    assert_eq!(violations.len(), 2);

    let missing_edge = violations
        .iter()
        .find(|v| v["violation"] == "array_without_edge")
        .expect("array_without_edge row");
    assert_eq!(missing_edge["source_record_id"], "agent_memory:v1:obs2");
    assert_eq!(missing_edge["target_record_id"], "codegraph:v1:sym_live");
    assert_eq!(missing_edge["relation"], "OBSERVES");

    let missing_array = violations
        .iter()
        .find(|v| v["violation"] == "edge_without_array")
        .expect("edge_without_array row");
    assert_eq!(missing_array["source_record_id"], "agent_memory:v1:obs2");
    assert_eq!(missing_array["target_record_id"], "codegraph:v1:sym_live");
    assert_eq!(missing_array["relation"], "VALIDATED_BY");
}

#[test]
fn summary_reports_counts_per_bucket() {
    let graph = write_graph(&fixture_records());
    let output = run_audit(graph.path(), &[]);
    let lines = ndjson_lines(&output.stdout);
    let summary = lines
        .iter()
        .find(|line| line["type"] == "summary")
        .expect("summary line");
    assert_eq!(summary["ok"], false);
    assert_eq!(summary["sources_checked"], 4);
    assert_eq!(summary["links_checked"], 8);
    assert_eq!(summary["resolves_live"], 5);
    assert_eq!(summary["drifted"], 1);
    assert_eq!(summary["dangling"], 2);
    assert_eq!(summary["integrity_violations"], 2);
}

#[test]
fn sweep_is_deterministic_across_runs() {
    let graph = write_graph(&fixture_records());
    let first = run_audit(graph.path(), &[]);
    let second = run_audit(graph.path(), &[]);
    assert_eq!(
        first.stdout, second.stdout,
        "byte-for-byte identical output"
    );
}

#[test]
fn clean_store_reports_ok_and_exits_zero() {
    let records = vec![
        symbol("codegraph:v1:sym_live", "src/lib.rs"),
        memory_node(
            "agent_memory:v1:obs1",
            NodeKind::Observation,
            vec![link("codegraph:v1:sym_live", "codegraph", "OBSERVES")],
        ),
        edge(
            EdgeLabel::Observes,
            "agent_memory:v1:obs1",
            "codegraph:v1:sym_live",
        ),
    ];
    let graph = write_graph(&records);
    let output = run_audit(graph.path(), &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "clean store exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = ndjson_lines(&output.stdout);
    let summary = lines
        .iter()
        .find(|line| line["type"] == "summary")
        .expect("summary line");
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["dangling"], 0);
    assert_eq!(summary["integrity_violations"], 0);
}

#[test]
fn text_format_renders_human_view() {
    let graph = write_graph(&fixture_records());
    let output = run_audit(graph.path(), &["--format", "text"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(
        text.contains("dangling: 2"),
        "summary counts in text view: {text}"
    );
    assert!(
        text.contains("drifted: 1"),
        "summary counts in text view: {text}"
    );
    assert!(
        text.contains("agent_memory:v1:obs1"),
        "source handle in text view: {text}"
    );
}

#[test]
fn empty_graph_is_a_load_error() {
    let graph = write_graph(&[]);
    let output = run_audit(graph.path(), &[]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(
        stderr.contains("empty_memory_evidence_input") || stderr.contains("zero records"),
        "diagnostic names the empty input: {stderr}"
    );
}
