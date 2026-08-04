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
