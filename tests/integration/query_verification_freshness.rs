#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::needless_collect
)]

//! End-to-end tests for `eg query verification-freshness` (issue #111): flag
//! verification records (`TestRun`/`CIStatus`/`BenchmarkRun`/`CoverageReport`/
//! `ProofResult`) whose cited code has drifted since the record's anchor
//! (`temporal.git_commit` or `executed_at`). Distinct from `eg query
//! evidence-freshness` (issue #85), which ages agent-memory observations, not
//! verification evidence.

use std::{fs, path::PathBuf};

use aletheia_egregore::{
    EdgeLabel, EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis,
    SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    ir::{Graph, VERIFICATION_SCHEMA_VERSION, stable_id, verification_stable_id},
};
use assert_cmd::Command;
use serde_json::Value;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn temporal(commit: &str, valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: Vec::new(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

/// Sentinel that must never leak into the redaction-safe output (AC10).
const RAW_SECRET_SENTINEL: &str = "RAW_SECRET_SHOULD_NOT_LEAK";

fn symbol_version(
    sym_id: &str,
    path: &str,
    name: &str,
    sym_span: SourceSpan,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        sym_id.to_owned(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(sym_span),
        Some(name.to_owned()),
        format!("Rust fn {name}\nSource:\n{body}"),
    )
    .with_temporal(temporal(commit, valid_time))
}

fn file_version(
    file_id: &str,
    path: &str,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    GraphRecord::node(
        file_id.to_owned(),
        NodeKind::File,
        Some(path.to_owned()),
        Some(span(1, 100)),
        Some(path.to_owned()),
        format!("Source file {path}\n{body}"),
    )
    .with_temporal(temporal(commit, valid_time))
}

/// A verification-domain node, carrying a redaction sentinel in its (never
/// emitted) summary so leakage tests have something to catch.
#[allow(clippy::too_many_arguments)]
fn ver_node(
    slug: &str,
    kind: NodeKind,
    verification_kind: Option<&str>,
    status: &str,
    executed_at: Option<&str>,
    git_commit: Option<&str>,
    source_artifact_path: Option<&str>,
    source_artifact_hash: Option<&str>,
) -> (String, GraphRecord) {
    let id = verification_stable_id(&["verification", slug]);
    let mut rec = GraphRecord::node(
        id.clone(),
        kind,
        None,
        None,
        None,
        format!("verification {slug} {RAW_SECRET_SENTINEL}"),
    );
    if let GraphRecord::Node {
        schema_version,
        domain,
        verification_kind: vk,
        status: st,
        executed_at: ea,
        source_artifact_path: sap,
        source_artifact_hash: sah,
        temporal,
        ..
    } = &mut rec
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *domain = Some("verification".to_owned());
        *vk = verification_kind.map(str::to_owned);
        *st = Some(status.to_owned());
        *ea = executed_at.map(str::to_owned);
        *sap = source_artifact_path.map(str::to_owned);
        *sah = source_artifact_hash.map(str::to_owned);
        if let Some(commit) = git_commit {
            *temporal = Some(temporal_at(
                commit,
                executed_at.unwrap_or("2026-01-01T00:00:00Z"),
            ));
        }
    }
    (id, rec)
}

fn temporal_at(commit: &str, valid_time: &str) -> TemporalMetadata {
    temporal(commit, valid_time)
}

fn cite_edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        label,
        source.to_owned(),
        target.to_owned(),
        None,
        "cites".to_owned(),
    )
}

fn repo_node(name: &str) -> (String, GraphRecord) {
    let id = stable_id(&["repository", "operator-override", name]);
    let rec = GraphRecord::node(
        id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some(name.to_owned()),
        format!("Repository {name}"),
    );
    (id, rec)
}

fn contains(source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Contains,
        source.to_owned(),
        target.to_owned(),
        None,
        "contains".to_owned(),
    )
}

fn defines(source: &str, target: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Defines,
        source.to_owned(),
        target.to_owned(),
        None,
        "defines".to_owned(),
    )
}

fn plain_file(path: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "File", path]);
    let rec = GraphRecord::node(
        id.clone(),
        NodeKind::File,
        Some(path.to_owned()),
        Some(span(1, 100)),
        Some(path.to_owned()),
        format!("Source file {path}"),
    );
    (id, rec)
}

fn plain_symbol(path: &str, name: &str) -> (String, GraphRecord) {
    let id = stable_id(&["node", "Symbol", path, name]);
    let rec = GraphRecord::node(
        id.clone(),
        NodeKind::Symbol,
        Some(path.to_owned()),
        Some(span(1, 5)),
        Some(name.to_owned()),
        format!("Symbol {name}"),
    );
    (id, rec)
}

fn tombstone_of(deleted_id: &str, marker: &str) -> GraphRecord {
    GraphRecord::Tombstone {
        id: stable_id(&["tombstone", deleted_id, marker]),
        schema_version: aletheia_egregore::SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: "deleted".to_owned(),
        producer: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn drift_record(
    drift_id: &str,
    prior_id: &str,
    target_id: &str,
    before_commit: &str,
    after_commit: &str,
    before_vt: &str,
    after_vt: &str,
) -> GraphRecord {
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
        before_git_commit: before_commit.to_owned(),
        after_git_commit: after_commit.to_owned(),
        before_valid_time: before_vt.to_owned(),
        after_valid_time: after_vt.to_owned(),
        metric_kind: MetricKind::CosineDistance,
        score: 0.7,
        selection_threshold: 0.2,
        selection_basis: SelectionBasis::ThresholdOnly,
    };
    GraphRecord::node(
        drift_id.to_owned(),
        NodeKind::SemanticDrift,
        None,
        None,
        None,
        "semantic drift".to_owned(),
    )
    .with_domain("semantic", aletheia_egregore::ir::SEMANTIC_SCHEMA_VERSION)
    .with_semantic_drift(drift)
}

fn write_graph(records: Vec<GraphRecord>) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("verification.graph.jsonl");
    let mut graph = Graph::new();
    for record in records {
        graph.push(record);
    }
    fs::write(&path, graph.to_jsonl().expect("serialize")).expect("write");
    (temp, path)
}

fn run(path: &PathBuf, extra: &[&str]) -> Value {
    let output = egregore()
        .args(["query", "verification-freshness", "--graph"])
        .arg(path)
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("valid JSON envelope")
}

fn run_raw(path: &PathBuf, extra: &[&str]) -> assert_cmd::assert::Assert {
    egregore()
        .args(["query", "verification-freshness", "--graph"])
        .arg(path)
        .args(extra)
        .assert()
}

fn verdicts_for<'a>(report: &'a Value, record_id: &str) -> Vec<&'a Value> {
    report["verdicts"]
        .as_array()
        .expect("verdicts array")
        .iter()
        .filter(|v| v["verification_record_id"] == record_id)
        .collect()
}

// ---------------------------------------------------------------------------
// Verdict basics
// ---------------------------------------------------------------------------

#[test]
fn unchanged_cited_symbol_is_current() {
    let (sym_id, symbol) = (
        stable_id(&["node", "Symbol", "src/a.rs", "widget"]),
        symbol_version(
            &stable_id(&["node", "Symbol", "src/a.rs", "widget"]),
            "src/a.rs",
            "widget",
            span(10, 20),
            "fn widget() {}",
            "c1",
            "2026-01-01T00:00:00Z",
        ),
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, ver, edge]);

    let report = run(&path, &[]);
    assert_eq!(report["ok"], true);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1, "one citation row expected");
    assert_eq!(rows[0]["verdict"], "current");
    assert!(rows[0]["freshness_lead"].is_null());
    assert!(rows[0]["triggering_handle"].is_null());
    assert_eq!(rows[0]["cited_handle"]["relation"], "MENTIONS_SYMBOL");
    assert_eq!(rows[0]["cited_handle"]["target_record_id"], sym_id);
}

#[test]
fn later_content_change_is_stale_with_content_change_trigger() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let v_early = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 1 }",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let v_later = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 2 }",
        "c2",
        "2026-01-05T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![v_early, v_later, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "stale");
    assert!(!rows[0]["freshness_lead"].is_null());
    assert_eq!(rows[0]["triggering_handle"]["kind"], "content_change");
    assert_eq!(rows[0]["triggering_handle"]["after_git_commit"], "c2");
}

#[test]
fn drift_record_after_anchor_is_stale_with_drift_trigger() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let drift_id = "semantic:v1:drift1".to_owned();
    let drift = drift_record(
        &drift_id,
        &sym_id,
        &sym_id,
        "c1",
        "c2",
        "2026-01-01T00:00:00Z",
        "2026-01-10T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::ProofResult,
        Some("proof_result"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, drift, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "stale");
    assert_eq!(rows[0]["triggering_handle"]["kind"], "drift_record");
    assert_eq!(rows[0]["triggering_handle"]["drift_record_id"], drift_id);
}

#[test]
fn drifts_prior_edge_recovers_drift_with_stale_prior_record_id_field() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let unrelated_id = stable_id(&["node", "Symbol", "src/other.rs", "unrelated"]);
    let drift_id = "semantic:v1:drift2".to_owned();
    // The metadata's own `prior_record_id` is stale/mismatched -- points at
    // an unrelated symbol -- so only the DRIFTS_PRIOR edge (drift -> widget)
    // can recover this as a trigger for `widget`'s citation.
    let drift = drift_record(
        &drift_id,
        &unrelated_id,
        &sym_id,
        "c1",
        "c2",
        "2026-01-01T00:00:00Z",
        "2026-01-10T00:00:00Z",
    );
    let drifts_prior_edge = GraphRecord::edge(
        EdgeLabel::DriftsPrior,
        drift_id.clone(),
        sym_id.clone(),
        None,
        "drifts prior".to_owned(),
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, drift, drifts_prior_edge, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["verdict"], "stale",
        "the DRIFTS_PRIOR edge must recover the trigger even though \
         prior_record_id points elsewhere"
    );
    assert_eq!(rows[0]["triggering_handle"]["kind"], "drift_record");
    assert_eq!(rows[0]["triggering_handle"]["drift_record_id"], drift_id);
}

#[test]
fn retracted_supersedes_source_never_hides_a_still_live_target() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let superseder_id = "agent_memory:v1:superseder".to_owned();
    let supersedes_edge = GraphRecord::edge(
        EdgeLabel::Supersedes,
        superseder_id.clone(),
        sym_id.clone(),
        None,
        "supersedes".to_owned(),
    );
    // The superseding node itself is later retracted -- its SUPERSEDES claim
    // must not survive it, so `widget` stays live.
    let retraction = tombstone_of(&superseder_id, "retracted");
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, supersedes_edge, retraction, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["verdict"], "current",
        "a retracted SUPERSEDES source must not hide a still-live target as unresolved"
    );
}

#[test]
fn tombstoned_target_is_unresolved_with_handle_removed_trigger() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let tomb = tombstone_of(&sym_id, "removed");
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "fail",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::FailedOn, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, tomb, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "unresolved");
    assert_eq!(rows[0]["triggering_handle"]["kind"], "handle_removed");
}

#[test]
fn absent_target_is_unresolved_with_handle_absent_trigger() {
    let missing_id = stable_id(&["node", "Symbol", "src/gone.rs", "ghost"]);
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "fail",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &missing_id);
    let (_temp, path) = write_graph(vec![ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "unresolved");
    assert_eq!(rows[0]["triggering_handle"]["kind"], "handle_absent");
}

#[test]
fn no_git_commit_or_executed_at_is_unanchored() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        None,
        None,
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::TouchedFile, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "unanchored");
    assert!(rows[0]["freshness_lead"].is_null());
    assert!(rows[0]["triggering_handle"].is_null());
}

// ---------------------------------------------------------------------------
// AC3 / AC4: freshness lead only, trust separation
// ---------------------------------------------------------------------------

#[test]
fn stale_verdict_never_rewrites_recorded_status_and_carries_lead_text_only() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let v_early = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 1 }",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let v_later = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 2 }",
        "c2",
        "2026-01-05T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![v_early, v_later, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    // Recorded `status` is echoed verbatim -- a `pass` stays `pass` even under
    // a `stale` freshness verdict (AC4: never re-judged).
    assert_eq!(rows[0]["status"], "pass");
    let lead = rows[0]["freshness_lead"].as_str().expect("lead text");
    assert!(lead.contains("reason to re-verify"));
    // The lead may name "wrong" only to explicitly DISCLAIM it -- it must
    // never assert the result is wrong outright.
    assert!(lead.contains("not proof"));
    assert!(!lead.to_lowercase().contains("broken"));
}

// ---------------------------------------------------------------------------
// AC5: no false staleness from neighbors
// ---------------------------------------------------------------------------

#[test]
fn sibling_symbol_change_never_flags_an_unrelated_citation() {
    let a_id = stable_id(&["node", "Symbol", "src/a.rs", "alpha"]);
    let b_id = stable_id(&["node", "Symbol", "src/a.rs", "beta"]);
    let a1 = symbol_version(
        &a_id,
        "src/a.rs",
        "alpha",
        span(1, 5),
        "fn alpha() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    // `beta` changes after the anchor; `alpha` never does.
    let b1 = symbol_version(
        &b_id,
        "src/a.rs",
        "beta",
        span(10, 15),
        "fn beta() { 1 }",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let b2 = symbol_version(
        &b_id,
        "src/a.rs",
        "beta",
        span(10, 15),
        "fn beta() { 2 }",
        "c2",
        "2026-01-05T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &a_id);
    let (_temp, path) = write_graph(vec![a1, b1, b2, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["verdict"], "current",
        "beta's drift must not leak onto alpha's citation"
    );
}

#[test]
fn touched_file_citation_flags_only_when_the_cited_file_itself_changes() {
    let file_id = stable_id(&["node", "File", "src/a.rs"]);
    let f1 = file_version(
        &file_id,
        "src/a.rs",
        "v1 body",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let f2 = file_version(
        &file_id,
        "src/a.rs",
        "v2 body",
        "c2",
        "2026-01-05T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::TouchedFile, &v1, &file_id);
    let (_temp, path) = write_graph(vec![f1, f2, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "stale");
    assert_eq!(rows[0]["cited_handle"]["relation"], "TOUCHED_FILE");
}

// ---------------------------------------------------------------------------
// AC6: full handle disclosure on stale/unresolved rows
// ---------------------------------------------------------------------------

#[test]
fn stale_row_carries_record_kind_status_cited_handle_and_triggering_handle() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let v_early = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 1 }",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let v_later = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 2 }",
        "c2",
        "2026-01-05T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![v_early, v_later, ver, edge]);

    let report = run(&path, &[]);
    let row = &verdicts_for(&report, &v1)[0];
    assert_eq!(row["verification_record_id"], v1);
    assert_eq!(row["verification_kind"], "test_run");
    assert_eq!(row["status"], "pass");
    assert_eq!(row["cited_handle"]["target_record_id"], sym_id);
    assert_eq!(row["cited_handle"]["repo_relative_path"], "src/a.rs");
    assert!(row["cited_handle"]["span"].is_object());
    assert_eq!(row["cited_handle"]["anchor_commit"], "c1");
    assert!(row["triggering_handle"]["after_git_commit"].is_string());
}

// ---------------------------------------------------------------------------
// AC2: source_artifact_hash mismatch forces at-minimum stale
// ---------------------------------------------------------------------------

#[test]
fn artifact_hash_mismatch_with_repo_path_is_stale() {
    let temp_repo = tempfile::tempdir().expect("repo dir");
    let artifact_rel = "ci/config.yml";
    let artifact_abs = temp_repo.path().join(artifact_rel);
    fs::create_dir_all(artifact_abs.parent().unwrap()).unwrap();
    fs::write(&artifact_abs, b"current bytes").unwrap();
    let recorded_hash = blake3::hash(b"old bytes").to_hex().to_string();

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some(artifact_rel),
        Some(&recorded_hash),
    );
    let (_temp, path) = write_graph(vec![ver]);

    let report = run(&path, &["--repo-path", temp_repo.path().to_str().unwrap()]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1, "artifact-hash citation row expected");
    assert_eq!(rows[0]["verdict"], "stale");
    assert_eq!(
        rows[0]["triggering_handle"]["kind"],
        "artifact_hash_changed"
    );
    assert_eq!(rows[0]["cited_handle"]["relation"], "source_artifact");
}

#[test]
fn artifact_hash_match_with_repo_path_is_current() {
    let temp_repo = tempfile::tempdir().expect("repo dir");
    let artifact_rel = "ci/config.yml";
    let artifact_abs = temp_repo.path().join(artifact_rel);
    fs::create_dir_all(artifact_abs.parent().unwrap()).unwrap();
    fs::write(&artifact_abs, b"same bytes").unwrap();
    let recorded_hash = blake3::hash(b"same bytes").to_hex().to_string();

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some(artifact_rel),
        Some(&recorded_hash),
    );
    let (_temp, path) = write_graph(vec![ver]);

    let report = run(&path, &["--repo-path", temp_repo.path().to_str().unwrap()]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "current");
}

#[test]
fn artifact_hash_never_checked_without_repo_path() {
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some("ci/config.yml"),
        Some(&"a".repeat(64)),
    );
    let (_temp, path) = write_graph(vec![ver]);

    // No code citations and no --repo-path: nothing to classify, so this
    // record contributes zero rows -- never a fabricated verdict.
    let report = run(&path, &[]);
    assert_eq!(verdicts_for(&report, &v1).len(), 0);
}

#[test]
fn artifact_path_escaping_repo_root_via_dotdot_is_never_read() {
    let repo_root = tempfile::tempdir().expect("repo root");
    // A file OUTSIDE repo_root, as a sibling directory entry.
    let secret_name = format!("secret-{}.txt", std::process::id());
    let secret_path = repo_root.path().parent().unwrap().join(&secret_name);
    fs::write(&secret_path, b"outside-secret-bytes").unwrap();
    let recorded_hash = blake3::hash(b"outside-secret-bytes").to_hex().to_string();

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some(&format!("../{secret_name}")),
        Some(&recorded_hash),
    );
    let (_temp, path) = write_graph(vec![ver]);

    let report = run(&path, &["--repo-path", repo_root.path().to_str().unwrap()]);
    // The escape must be rejected outright -- no row at all, never a
    // fabricated `current`/`stale` verdict built from a file outside the
    // repo root.
    assert_eq!(
        verdicts_for(&report, &v1).len(),
        0,
        "a source_artifact_path escaping --repo-path via .. must never be read"
    );
    let _ = fs::remove_file(&secret_path);
}

#[test]
fn artifact_path_as_absolute_path_is_never_read() {
    let repo_root = tempfile::tempdir().expect("repo root");
    let secret_name = format!("secret-abs-{}.txt", std::process::id());
    let secret_path = repo_root.path().parent().unwrap().join(&secret_name);
    fs::write(&secret_path, b"outside-secret-bytes-abs").unwrap();
    let recorded_hash = blake3::hash(b"outside-secret-bytes-abs")
        .to_hex()
        .to_string();

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some(secret_path.to_str().unwrap()),
        Some(&recorded_hash),
    );
    let (_temp, path) = write_graph(vec![ver]);

    let report = run(&path, &["--repo-path", repo_root.path().to_str().unwrap()]);
    // `Path::join` discards the base for an absolute second operand -- must
    // still be rejected, not silently read.
    assert_eq!(
        verdicts_for(&report, &v1).len(),
        0,
        "an absolute source_artifact_path must never be read, even though \
         Path::join would otherwise honor it verbatim"
    );
    let _ = fs::remove_file(&secret_path);
}

// ---------------------------------------------------------------------------
// AC7: stale-only mode + empty-result diagnostics
// ---------------------------------------------------------------------------

#[test]
fn zero_verification_records_in_store_yields_explicit_diagnostic() {
    let file_id = stable_id(&["node", "File", "src/a.rs"]);
    let f = file_version(&file_id, "src/a.rs", "body", "c1", "2026-01-01T00:00:00Z");
    let (_temp, path) = write_graph(vec![f]);

    let report = run(&path, &[]);
    assert_eq!(report["ok"], true);
    assert_eq!(verdicts_for(&report, "anything").len(), 0);
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"no_verification_records_in_store"));
}

#[test]
fn stale_only_mode_reports_no_stale_diagnostic_when_all_current() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, ver, edge]);

    let report = run(&path, &["--stale-only"]);
    assert_eq!(report["stale_only"], true);
    assert_eq!(verdicts_for(&report, &v1).len(), 0);
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"no_stale_verification_records"));
}

#[test]
fn stale_only_mode_returns_only_stale_and_unresolved_rows() {
    let cur_id = stable_id(&["node", "Symbol", "src/a.rs", "current_one"]);
    let cur = symbol_version(
        &cur_id,
        "src/a.rs",
        "current_one",
        span(1, 5),
        "fn current_one() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let missing_id = stable_id(&["node", "Symbol", "src/gone.rs", "ghost"]);

    let (v1, ver1) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let (v2, ver2) = ver_node(
        "v2",
        NodeKind::TestRun,
        Some("test_run"),
        "fail",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let e1 = cite_edge(EdgeLabel::MentionsSymbol, &v1, &cur_id);
    let e2 = cite_edge(EdgeLabel::FailedOn, &v2, &missing_id);
    let (_temp, path) = write_graph(vec![cur, ver1, ver2, e1, e2]);

    let report = run(&path, &["--stale-only"]);
    let ids: Vec<&str> = report["verdicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["verification_record_id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&v1.as_str()));
    assert!(ids.contains(&v2.as_str()));
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"stale_verification_records_present"));
}

// ---------------------------------------------------------------------------
// AC8: diagnostics for limits, ambiguity, unresolved scope
// ---------------------------------------------------------------------------

#[test]
fn invalid_limit_fails_fast_before_touching_the_store() {
    let (_temp, path) = write_graph(vec![]);
    run_raw(&path, &["--limit", "0"]).failure().code(1);
    run_raw(&path, &["--limit", "100000"]).failure().code(1);
}

#[test]
fn truncation_beyond_limit_is_disclosed_with_counts() {
    let mut records = Vec::new();
    for i in 0..5 {
        let sym_id = stable_id(&["node", "Symbol", "src/a.rs", &format!("sym{i}")]);
        records.push(symbol_version(
            &sym_id,
            "src/a.rs",
            &format!("sym{i}"),
            span(i * 10 + 1, i * 10 + 5),
            "fn f() {}",
            "c1",
            "2026-01-01T00:00:00Z",
        ));
        let (vid, ver) = ver_node(
            &format!("v{i}"),
            NodeKind::TestRun,
            Some("test_run"),
            "pass",
            Some("2026-01-02T00:00:00Z"),
            Some("c1"),
            None,
            None,
        );
        records.push(ver);
        records.push(cite_edge(EdgeLabel::MentionsSymbol, &vid, &sym_id));
    }
    let (_temp, path) = write_graph(records);

    let report = run(&path, &["--limit", "2"]);
    assert_eq!(report["verdicts"].as_array().unwrap().len(), 2);
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"results_truncated"));
}

#[test]
fn unsupported_scope_handle_exits_two() {
    let (_temp, path) = write_graph(vec![]);
    run_raw(&path, &["nonexistent-thing-xyz"]).failure().code(2);
}

// ---------------------------------------------------------------------------
// AC9: strictly read-only + deterministic
// ---------------------------------------------------------------------------

#[test]
fn output_is_byte_identical_across_five_runs() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, ver, edge]);

    let first = egregore()
        .args(["query", "verification-freshness", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    for _ in 0..4 {
        let again = egregore()
            .args(["query", "verification-freshness", "--graph"])
            .arg(&path)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(first, again, "output must be byte-identical across runs");
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn dir_fingerprint(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let ft = entry.file_type().unwrap();
            let path = entry.path();
            if ft.is_dir() {
                walk(&path, base, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn data_dir_is_never_mutated() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, ver, edge]);

    let scratch = tempfile::tempdir().expect("scratch");
    let data_dir = scratch.path().join("store");
    egregore()
        .args(["ingest"])
        .arg(&path)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let before = dir_fingerprint(&data_dir);
    egregore()
        .args(["query", "verification-freshness", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    let after = dir_fingerprint(&data_dir);
    assert_eq!(
        before, after,
        "verification-freshness must never mutate the store"
    );
}

// ---------------------------------------------------------------------------
// AC10: no raw payload leakage
// ---------------------------------------------------------------------------

#[test]
fn raw_summary_text_never_leaks_into_output() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![symbol, ver, edge]);

    let output = egregore()
        .args(["query", "verification-freshness", "--graph"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(!text.contains(RAW_SECRET_SENTINEL));
}

// ---------------------------------------------------------------------------
// Multiple citations per record stay independently classified
// ---------------------------------------------------------------------------

#[test]
fn a_record_with_multiple_citations_gets_one_row_per_citation() {
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let file_id = stable_id(&["node", "File", "src/a.rs"]);
    let f1 = file_version(
        &file_id,
        "src/a.rs",
        "v1 body",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let f2 = file_version(
        &file_id,
        "src/a.rs",
        "v2 body",
        "c2",
        "2026-01-05T00:00:00Z",
    );

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let e1 = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let e2 = cite_edge(EdgeLabel::TouchedFile, &v1, &file_id);
    let (_temp, path) = write_graph(vec![symbol, f1, f2, ver, e1, e2]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 2, "one row per citation, never collapsed");
    let verdicts: Vec<&str> = rows
        .iter()
        .map(|r| r["verdict"].as_str().unwrap())
        .collect();
    assert!(
        verdicts.contains(&"current"),
        "the unchanged symbol citation stays current"
    );
    assert!(
        verdicts.contains(&"stale"),
        "the changed file citation goes stale"
    );
}

// ---------------------------------------------------------------------------
// Mutual exclusivity / usage errors
// ---------------------------------------------------------------------------

#[test]
fn graph_and_data_dir_are_mutually_exclusive() {
    let (_temp, path) = write_graph(vec![]);
    egregore()
        .args(["query", "verification-freshness", "--graph"])
        .arg(&path)
        .args(["--data-dir"])
        .arg(&path)
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// External review regressions (Codex review on PR #498)
// ---------------------------------------------------------------------------

#[test]
fn symbol_absent_from_newest_scan_without_tombstone_is_unresolved() {
    // A repeated current-tree `scan`/`refresh` never tombstones a deleted
    // symbol; it simply stops appearing in the newest scan. `widget` exists
    // only in the OLDER (non-temporal) snapshot; a later `Repository`
    // snapshot proves a newer scan happened without re-emitting `widget`.
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let widget_at_older_scan = GraphRecord::node(
        sym_id.clone(),
        NodeKind::Symbol,
        Some("src/a.rs".to_owned()),
        Some(span(10, 20)),
        Some("widget".to_owned()),
        "fn widget() {}".to_owned(),
    )
    .with_valid_time("2026-01-01T00:00:00Z", "test");
    let (repo_id, _) = repo_node("solo");
    let repo_at_newer_scan = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("solo".to_owned()),
        "Repository solo".to_owned(),
    )
    .with_valid_time("2026-02-01T00:00:00Z", "test");
    // A real `eg scan` always wires Repository -> File/Symbol via CONTAINS;
    // without it the symbol would be genuinely unattributable (a different,
    // already-covered case) rather than "attributed to a repository whose
    // newest scan moved past it".
    let contains_widget = contains(&repo_id, &sym_id);

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![
        widget_at_older_scan,
        repo_at_newer_scan,
        contains_widget,
        ver,
        edge,
    ]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["verdict"], "unresolved",
        "a symbol missing from the newest scan (no tombstone) must not be reported current/stale"
    );
}

#[test]
fn symbol_present_in_newest_scan_stays_resolvable() {
    // Companion to the above: `widget` survives into the newer scan too, so
    // the frontier logic must not over-prune a still-present symbol.
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let widget_at_older_scan = GraphRecord::node(
        sym_id.clone(),
        NodeKind::Symbol,
        Some("src/a.rs".to_owned()),
        Some(span(10, 20)),
        Some("widget".to_owned()),
        "fn widget() {}".to_owned(),
    )
    .with_valid_time("2026-01-01T00:00:00Z", "test");
    let widget_at_newer_scan = GraphRecord::node(
        sym_id.clone(),
        NodeKind::Symbol,
        Some("src/a.rs".to_owned()),
        Some(span(10, 20)),
        Some("widget".to_owned()),
        "fn widget() {}".to_owned(),
    )
    .with_valid_time("2026-02-01T00:00:00Z", "test");

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![widget_at_older_scan, widget_at_newer_scan, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "current");
}

#[test]
fn artifact_row_excluded_from_repo_scope_without_attributable_citation() {
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let (file_b, file_b_node) = plain_file("src/b.rs");

    let temp_repo = tempfile::tempdir().expect("repo dir");
    let artifact_rel = "ci/config.yml";
    let artifact_abs = temp_repo.path().join(artifact_rel);
    fs::create_dir_all(artifact_abs.parent().unwrap()).unwrap();
    fs::write(&artifact_abs, b"bytes").unwrap();
    let recorded_hash = blake3::hash(b"bytes").to_hex().to_string();

    // v1 belongs to repo-b (its only real citation targets file_b, which
    // repo-b contains) and also carries a source_artifact_hash/path.
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some(artifact_rel),
        Some(&recorded_hash),
    );
    let touched = cite_edge(EdgeLabel::TouchedFile, &v1, &file_b);
    let (_temp, path) = write_graph(vec![
        repo_a_node,
        repo_b_node,
        file_b_node,
        contains(&repo_b, &file_b),
        ver,
        touched,
    ]);

    // Scoped to repo-a (the WRONG repository, but the one whose checkout is
    // supplied via --repo-path): the artifact row must NOT appear -- it
    // must never hash repo-a's filesystem for a TestRun that belongs to
    // repo-b.
    let report_a = run(
        &path,
        &[
            "--repo",
            &repo_a,
            "--repo-path",
            temp_repo.path().to_str().unwrap(),
        ],
    );
    let artifact_rows_a: Vec<&Value> = verdicts_for(&report_a, &v1)
        .into_iter()
        .filter(|r| r["cited_handle"]["relation"] == "source_artifact")
        .collect();
    assert_eq!(
        artifact_rows_a.len(),
        0,
        "an artifact row must never surface under an unrelated repository's scope"
    );

    // Scoped to repo-b (the record's real repository): the artifact row is
    // correctly included.
    let report_b = run(
        &path,
        &[
            "--repo",
            &repo_b,
            "--repo-path",
            temp_repo.path().to_str().unwrap(),
        ],
    );
    let artifact_rows_b: Vec<&Value> = verdicts_for(&report_b, &v1)
        .into_iter()
        .filter(|r| r["cited_handle"]["relation"] == "source_artifact")
        .collect();
    assert_eq!(artifact_rows_b.len(), 1);
}

#[test]
fn scope_resolution_is_confined_to_the_selected_repository() {
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let (file_a, file_a_node) = plain_file("src/a.rs");
    let (file_b, file_b_node) = plain_file("src/b.rs");
    // Same symbol NAME in both repositories.
    let (sym_a, sym_a_node) = plain_symbol("src/a.rs", "run");
    let (sym_b, sym_b_node) = plain_symbol("src/b.rs", "run");

    let (v1, ver1) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let e1 = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_a);
    let (v2, ver2) = ver_node(
        "v2",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let e2 = cite_edge(EdgeLabel::MentionsSymbol, &v2, &sym_b);

    let (_temp, path) = write_graph(vec![
        repo_a_node,
        repo_b_node,
        file_a_node,
        file_b_node,
        sym_a_node,
        sym_b_node,
        contains(&repo_a, &file_a),
        defines(&file_a, &sym_a),
        contains(&repo_b, &file_b),
        defines(&file_b, &sym_b),
        ver1,
        e1,
        ver2,
        e2,
    ]);

    // Without a --repo scope, "run" is genuinely ambiguous (exists in both
    // repositories).
    run_raw(&path, &["run"]).failure().code(1);

    // Scoped to repo-a, "run" is UNIQUE within that repository and must
    // resolve -- not be rejected as ambiguous just because a same-named
    // symbol exists elsewhere.
    let report = run(&path, &["run", "--repo", &repo_a]);
    let rows: Vec<&Value> = report["verdicts"].as_array().unwrap().iter().collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verification_record_id"], v1);
}

#[test]
fn repositories_scanned_at_different_times_do_not_cross_prune() {
    // Repo A's newest scan is LATER than repo B's newest (and only) scan.
    // A store-wide frontier would prune B's still-current symbol as if it
    // had been deleted in a scan that never touched B at all.
    let (_repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let repo_a_node = repo_a_node.with_valid_time("2026-03-01T00:00:00Z", "test");
    let repo_b_node = repo_b_node.with_valid_time("2026-01-01T00:00:00Z", "test");

    let sym_b = stable_id(&["node", "Symbol", "src/b.rs", "widget"]);
    let widget_b = GraphRecord::node(
        sym_b.clone(),
        NodeKind::Symbol,
        Some("src/b.rs".to_owned()),
        Some(span(10, 20)),
        Some("widget".to_owned()),
        "fn widget() {}".to_owned(),
    )
    .with_valid_time("2026-01-01T00:00:00Z", "test");

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_b);
    let (_temp, path) = write_graph(vec![
        repo_a_node,
        repo_b_node,
        widget_b,
        contains(&repo_b, &sym_b),
        ver,
        edge,
    ]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["verdict"], "current",
        "repo B's own (only) scan must not be pruned by repo A's LATER scan"
    );
}

#[test]
fn artifact_row_excluded_when_record_cites_multiple_repositories() {
    // v1 cites code in BOTH repo A and repo B and also carries a
    // source_artifact_hash/path. Ownership is genuinely ambiguous for the
    // artifact row -- it must not resolve under EITHER repository's scope.
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let (file_a, file_a_node) = plain_file("src/a.rs");
    let (file_b, file_b_node) = plain_file("src/b.rs");

    let temp_repo = tempfile::tempdir().expect("repo dir");
    let artifact_rel = "ci/config.yml";
    let artifact_abs = temp_repo.path().join(artifact_rel);
    fs::create_dir_all(artifact_abs.parent().unwrap()).unwrap();
    fs::write(&artifact_abs, b"bytes").unwrap();
    let recorded_hash = blake3::hash(b"bytes").to_hex().to_string();

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some(artifact_rel),
        Some(&recorded_hash),
    );
    let touched_a = cite_edge(EdgeLabel::TouchedFile, &v1, &file_a);
    let touched_b = cite_edge(EdgeLabel::TouchedFile, &v1, &file_b);
    let (_temp, path) = write_graph(vec![
        repo_a_node,
        repo_b_node,
        file_a_node,
        file_b_node,
        contains(&repo_a, &file_a),
        contains(&repo_b, &file_b),
        ver,
        touched_a,
        touched_b,
    ]);

    for repo in [&repo_a, &repo_b] {
        let report = run(
            &path,
            &[
                "--repo",
                repo,
                "--repo-path",
                temp_repo.path().to_str().unwrap(),
            ],
        );
        let artifact_rows: Vec<&Value> = verdicts_for(&report, &v1)
            .into_iter()
            .filter(|r| r["cited_handle"]["relation"] == "source_artifact")
            .collect();
        assert_eq!(
            artifact_rows.len(),
            0,
            "a record citing two repositories must not resolve its ambiguous \
             artifact row under either repository's scope"
        );
        // The record's real, unambiguous citation rows still appear.
        let real_rows: Vec<&Value> = verdicts_for(&report, &v1)
            .into_iter()
            .filter(|r| r["cited_handle"]["relation"] == "TOUCHED_FILE")
            .collect();
        assert_eq!(real_rows.len(), 1);
    }
}

#[test]
fn unresolved_row_with_no_owner_excluded_from_unrelated_repo_scope() {
    // v1 cites a live symbol in repo B AND a target that is entirely absent
    // from the store (unresolved, no owner of its own). Scoped to an
    // UNRELATED repo A, only B's row is truly in scope; the ownerless
    // unresolved row must not be attributed to repo A just because neither
    // side has a direct RepositoryIndex owner.
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let (file_b, file_b_node) = plain_file("src/b.rs");
    let missing_id = stable_id(&["node", "Symbol", "src/gone.rs", "ghost"]);

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "fail",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let live_edge = cite_edge(EdgeLabel::TouchedFile, &v1, &file_b);
    let dangling_edge = cite_edge(EdgeLabel::FailedOn, &v1, &missing_id);
    let (_temp, path) = write_graph(vec![
        repo_a_node,
        repo_b_node,
        file_b_node,
        contains(&repo_b, &file_b),
        ver,
        live_edge,
        dangling_edge,
    ]);

    let report_a = run(&path, &["--repo", &repo_a]);
    let rows_a = verdicts_for(&report_a, &v1);
    assert_eq!(
        rows_a.len(),
        0,
        "neither of repo B's citations (live or dangling) belongs under repo A's scope"
    );

    let report_b = run(&path, &["--repo", &repo_b]);
    let rows_b = verdicts_for(&report_b, &v1);
    assert_eq!(
        rows_b.len(),
        2,
        "both of repo B's own citations (live and dangling) belong under repo B's scope"
    );
}

#[test]
fn retracted_same_named_symbol_never_causes_false_ambiguity() {
    // A history-inclusive store retains a tombstoned "run" (in file A)
    // alongside a live "run" (in file B). Scoping to the live one by name
    // must resolve, not be rejected as ambiguous by the dead one.
    let dead_id = stable_id(&["node", "Symbol", "src/a.rs", "run"]);
    let dead_run = symbol_version(
        &dead_id,
        "src/a.rs",
        "run",
        span(1, 5),
        "fn run() { 1 }",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let dead_tombstone = tombstone_of(&dead_id, "removed");

    let live_id = stable_id(&["node", "Symbol", "src/b.rs", "run"]);
    let live_run = symbol_version(
        &live_id,
        "src/b.rs",
        "run",
        span(1, 5),
        "fn run() { 2 }",
        "c1",
        "2026-01-01T00:00:00Z",
    );

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &live_id);
    let (_temp, path) = write_graph(vec![dead_run, dead_tombstone, live_run, ver, edge]);

    // Scoping by the live name must succeed, not report ambiguous_scope.
    let report = run(&path, &["run"]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cited_handle"]["target_record_id"], live_id);

    // An explicit reference to the DEAD record ID still resolves directly
    // (never filtered by liveness) -- it simply matches nothing here since
    // no citation targets it, an ordinary well-formed empty result.
    run_raw(&path, &[&dead_id]).success();
}

#[test]
fn repeated_verification_write_is_classified_once() {
    // Simulates a re-ingested / history-inclusive-store-duplicated physical
    // copy of the SAME TestRun and the SAME citation edge (both keyed by
    // stable ID, so two byte-identical physical rows share one ID). Must
    // classify to exactly one row, never two.
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let symbol = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() {}",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    // The same TestRun node and the same citation edge, each written TWICE.
    let (_temp, path) = write_graph(vec![symbol, ver.clone(), ver, edge.clone(), edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(
        rows.len(),
        1,
        "a repeated physical write of the same record/edge must classify once, not per copy"
    );
}

#[test]
fn artifact_only_record_excluded_from_every_repo_scope() {
    // v1 has ZERO code citations (e.g. `capture-tests` run without
    // `--graph`) -- only a source_artifact_hash/path. With no attributable
    // citation anywhere, and the verification record itself unattributed,
    // the artifact row must be excluded from a scoped response entirely --
    // never passed through under an unrelated repository's --repo-path.
    let (repo_a, repo_a_node) = repo_node("repo-a");

    let temp_repo = tempfile::tempdir().expect("repo dir");
    let artifact_rel = "ci/config.yml";
    let artifact_abs = temp_repo.path().join(artifact_rel);
    fs::create_dir_all(artifact_abs.parent().unwrap()).unwrap();
    fs::write(&artifact_abs, b"bytes").unwrap();
    let recorded_hash = blake3::hash(b"bytes").to_hex().to_string();

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some(artifact_rel),
        Some(&recorded_hash),
    );
    let (_temp, path) = write_graph(vec![repo_a_node, ver]);

    // Unscoped: the artifact row appears normally.
    let report = run(&path, &["--repo-path", temp_repo.path().to_str().unwrap()]);
    assert_eq!(verdicts_for(&report, &v1).len(), 1);

    // Scoped to repo-a (a repository this record has no relationship to
    // whatsoever): the artifact row must be excluded, not passed through.
    let scoped = run(
        &path,
        &[
            "--repo",
            &repo_a,
            "--repo-path",
            temp_repo.path().to_str().unwrap(),
        ],
    );
    assert_eq!(
        verdicts_for(&scoped, &v1).len(),
        0,
        "an artifact-only record with no attributable citation must never \
         surface under any --repo scope"
    );
}

#[test]
fn content_change_then_revert_is_still_detected_as_stale() {
    // widget changes after the anchor (c1 -> c2) and then reverts back to
    // byte-identical content at c3. Comparing only the anchor against the
    // LATEST version would miss this (c1 == c3), but the code genuinely
    // drifted at c2 -- the documented trigger is ANY later differing
    // version, not just the final one.
    let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "widget"]);
    let v_anchor = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 1 }",
        "c1",
        "2026-01-01T00:00:00Z",
    );
    let v_changed = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 2 }",
        "c2",
        "2026-01-05T00:00:00Z",
    );
    let v_reverted = symbol_version(
        &sym_id,
        "src/a.rs",
        "widget",
        span(10, 20),
        "fn widget() { 1 }",
        "c3",
        "2026-01-10T00:00:00Z",
    );
    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        Some("c1"),
        None,
        None,
    );
    let edge = cite_edge(EdgeLabel::MentionsSymbol, &v1, &sym_id);
    let (_temp, path) = write_graph(vec![v_anchor, v_changed, v_reverted, ver, edge]);

    let report = run(&path, &[]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["verdict"], "stale",
        "a change-then-revert must still be detected -- the code drifted at \
         c2 even though the LATEST version happens to match the anchor again"
    );
    assert_eq!(rows[0]["triggering_handle"]["kind"], "content_change");
    assert_eq!(rows[0]["triggering_handle"]["after_git_commit"], "c2");
}

#[test]
fn dangling_only_citation_with_no_attribution_excluded_from_every_repo_scope() {
    // v1's ONLY citation targets an ID absent from the store entirely (no
    // owner anywhere), and v1 itself carries no other citation to borrow
    // attribution from. Under a --repo scope this row must never surface
    // under ANY repository -- there is no positive evidence connecting it
    // to one, so guessing "it belongs to whichever repo was asked for"
    // would attribute the same dangling reference to every repository.
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let missing_id = stable_id(&["node", "Symbol", "src/gone.rs", "ghost"]);

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "fail",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let dangling_edge = cite_edge(EdgeLabel::FailedOn, &v1, &missing_id);
    let (_temp, path) = write_graph(vec![repo_a_node, ver, dangling_edge]);

    // Unscoped: the dangling citation appears normally.
    let unscoped = run(&path, &[]);
    assert_eq!(verdicts_for(&unscoped, &v1).len(), 1);

    // Scoped to repo-a, with which this record has no relationship at all.
    let scoped = run(&path, &["--repo", &repo_a]);
    assert_eq!(
        verdicts_for(&scoped, &v1).len(),
        0,
        "a dangling citation with zero attribution anywhere must never \
         surface under any --repo scope"
    );
}

#[test]
fn artifact_hash_match_without_anchor_is_unanchored_not_current() {
    // A CIStatus with a matching source_artifact_hash but NEITHER
    // temporal.git_commit NOR executed_at. A hash MATCH proves only
    // "unchanged right now" -- it says nothing about "since the anchor"
    // when there is no anchor to compare against, so this must stay
    // unanchored, not be overstated as current.
    let temp_repo = tempfile::tempdir().expect("repo dir");
    let artifact_rel = "ci/config.yml";
    let artifact_abs = temp_repo.path().join(artifact_rel);
    fs::create_dir_all(artifact_abs.parent().unwrap()).unwrap();
    fs::write(&artifact_abs, b"same bytes").unwrap();
    let recorded_hash = blake3::hash(b"same bytes").to_hex().to_string();

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        None, // no executed_at
        None, // no git_commit
        Some(artifact_rel),
        Some(&recorded_hash),
    );
    let (_temp, path) = write_graph(vec![ver]);

    let report = run(&path, &["--repo-path", temp_repo.path().to_str().unwrap()]);
    let rows = verdicts_for(&report, &v1);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["verdict"], "unanchored",
        "a matching hash on an anchorless record must not be overstated as current"
    );
}

#[test]
fn verification_record_directly_attributed_to_one_repo_excluded_from_anothers_scope() {
    // A hypothetical future writer directly attributes v1 to repo-a (via
    // CONTAINS -- no current writer does this, but the filter must not
    // assume it never will). v1 cites a symbol owned by repo-b. Both
    // endpoints must agree with the requested scope for a row to survive:
    // this is a genuinely cross-repository citation, so it must be excluded
    // from BOTH single-repository scopes -- never guessed into either one.
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let (file_b, file_b_node) = plain_file("src/b.rs");

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::TestRun,
        Some("test_run"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        None,
        None,
    );
    let touched = cite_edge(EdgeLabel::TouchedFile, &v1, &file_b);
    let (_temp, path) = write_graph(vec![
        repo_a_node,
        repo_b_node,
        file_b_node,
        contains(&repo_a, &v1), // v1 itself directly attributed to repo-a
        contains(&repo_b, &file_b),
        ver,
        touched,
    ]);

    let scoped_to_b = run(&path, &["--repo", &repo_b]);
    assert_eq!(
        verdicts_for(&scoped_to_b, &v1).len(),
        0,
        "a record attributed to repo-a must never surface under repo-b's scope, \
         even when its cited target belongs to repo-b"
    );

    let scoped_to_a = run(&path, &["--repo", &repo_a]);
    assert_eq!(
        verdicts_for(&scoped_to_a, &v1).len(),
        0,
        "a citation into repo-b must not surface under repo-a's scope either -- \
         both endpoints must agree, so a genuinely cross-repository citation is \
         excluded from every single-repository scope"
    );
}

#[cfg(unix)]
#[test]
fn out_of_scope_repository_artifact_fifo_is_never_read() {
    // v1's source_artifact_path points at a FIFO with no writer -- opening
    // it for reading would block forever. v1 has a live citation into
    // repo-b and none into repo-a. Scoped to the UNRELATED repo-a, the
    // query must complete promptly, proving the artifact file is never even
    // opened for an out-of-scope record.
    let (repo_a, repo_a_node) = repo_node("repo-a");
    let (repo_b, repo_b_node) = repo_node("repo-b");
    let (file_b, file_b_node) = plain_file("src/b.rs");

    let temp_repo = tempfile::tempdir().expect("repo dir");
    let fifo_path = temp_repo.path().join("blocking.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("mkfifo command must be available on Unix");
    assert!(status.success(), "mkfifo failed");

    let (v1, ver) = ver_node(
        "v1",
        NodeKind::CIStatus,
        Some("ci_status"),
        "pass",
        Some("2026-01-02T00:00:00Z"),
        None,
        Some("blocking.fifo"),
        Some(&"a".repeat(64)),
    );
    let touched = cite_edge(EdgeLabel::TouchedFile, &v1, &file_b);
    let (_temp, path) = write_graph(vec![
        repo_a_node,
        repo_b_node,
        file_b_node,
        contains(&repo_b, &file_b),
        ver,
        touched,
    ]);

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("egregore"))
        .args(["query", "verification-freshness", "--graph"])
        .arg(&path)
        .args(["--repo", &repo_a, "--repo-path"])
        .arg(temp_repo.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn egregore");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "query hung -- an out-of-scope repository's artifact FIFO was read \
                 before the repo-scope filter excluded its row"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
