//! Unit tests for the `link-logs` correlation core (issue #323).

use super::*;
use crate::ir::{ErrorSignaturePayload, LogSourcePayload, OutputHandle};

const ANCHOR: &str = "codegraph:v1:repo_main";
const FINGERPRINT: &str = "template-v1";

fn repository(id: &str) -> GraphRecord {
    GraphRecord::node(
        id.to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("repo".to_owned()),
        "repository".to_owned(),
    )
}

fn log_source(anchor: &str, path: &str, hash: &str) -> (String, GraphRecord) {
    let id = log_stable_id(&["log_source", anchor, path, hash]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::LogSource,
        Some(path.to_owned()),
        None,
        Some(path.to_owned()),
        "log source".to_owned(),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::LogSource(LogSourcePayload {
        source_relative_path: path.to_owned(),
        source_format_version: "plain-v1".to_owned(),
        source_artifact_hash: hash.to_owned(),
        line_count: 1,
    }));
    (id, node)
}

fn error_signature(tag: &str, last_seen: &str) -> (String, GraphRecord) {
    let id = log_stable_id(&["error_signature", ANCHOR, FINGERPRINT, tag, "error"]);
    let node = GraphRecord::node(
        id.clone(),
        NodeKind::ErrorSignature,
        None,
        None,
        Some("error signature".to_owned()),
        "error signature".to_owned(),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
    .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
        fingerprint_algorithm: FINGERPRINT.to_owned(),
        template_excerpt: "boom".to_owned(),
        severity: "error".to_owned(),
        occurrence_count: 1,
        first_seen: last_seen.to_owned(),
        last_seen: last_seen.to_owned(),
        frames: None,
    }))
    .with_valid_time(last_seen, "log_event_timestamp");
    (id, node)
}

fn captured_from(sig: &str, src: &str) -> GraphRecord {
    let mut e = GraphRecord::edge(
        EdgeLabel::CapturedFrom,
        sig.to_owned(),
        src.to_owned(),
        None,
        "captured".to_owned(),
    );
    // Re-key onto the log domain like scan-logs does; the linker keys off
    // label/source/target, not the edge ID, so this is cosmetic.
    if let GraphRecord::Edge { schema_version, .. } = &mut e {
        *schema_version = LOG_SCHEMA_VERSION;
    }
    e
}

fn command_run(id: &str, stderr_hash: &str) -> GraphRecord {
    let mut n = GraphRecord::node(
        format!("verification:v1:{id}"),
        NodeKind::CommandRun,
        None,
        None,
        Some("cmd".to_owned()),
        "command run".to_owned(),
    );
    if let GraphRecord::Node { stderr_handle, .. } = &mut n {
        *stderr_handle = Some(Box::new(OutputHandle {
            inline: None,
            hash: stderr_hash.to_owned(),
            bytes: 10,
        }));
    }
    n
}

fn agent_run(id: &str, start: &str, end: &str) -> GraphRecord {
    let mut n = GraphRecord::node(
        format!("agent_memory:v1:{id}"),
        NodeKind::AgentRun,
        None,
        None,
        Some("run".to_owned()),
        "agent run".to_owned(),
    );
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

fn task(id: &str) -> GraphRecord {
    GraphRecord::node(
        format!("project:v1:{id}"),
        NodeKind::Task,
        None,
        None,
        Some("task".to_owned()),
        "task".to_owned(),
    )
}

fn references_task(source: &str, target: &str) -> GraphRecord {
    GraphRecord::agent_memory_edge(
        EdgeLabel::ReferencesTask,
        source.to_owned(),
        target.to_owned(),
        None,
        "references task".to_owned(),
    )
}

fn emitted_edges(result: &LinkLogsResult) -> Vec<&GraphRecord> {
    result
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::EmittedDuring,
                    ..
                }
            )
        })
        .collect()
}

#[test]
fn content_hash_join_emits_confidence_one() {
    let hash = "deadbeefhash";
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let (sig_id, sig) = error_signature("boom", "2026-07-01T00:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        command_run("cmd1", hash),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());
    let edges = emitted_edges(&result);
    assert_eq!(edges.len(), 1, "one content-hash-join edge");
    let GraphRecord::Edge {
        confidence,
        basis,
        target,
        ..
    } = edges[0]
    else {
        panic!("edge");
    };
    assert_eq!(basis, &Some(CorrelationBasis::ContentHashJoin));
    assert_eq!(confidence.as_deref(), Some("1.0"));
    assert_eq!(target, "verification:v1:cmd1");
    assert_eq!(result.totals.content_hash_join_edges, 1);
    assert_eq!(result.totals.uncorrelated, 0);
}

#[test]
fn temporal_correlation_in_window_emits_confidence_half() {
    let (src_id, src) = log_source(ANCHOR, "app.log", "h1");
    let (sig_id, sig) = error_signature("boom", "2026-07-01T12:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_in", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
        agent_run("run_out", "2026-07-02T11:00:00Z", "2026-07-02T13:00:00Z"),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());
    let edges = emitted_edges(&result);
    assert_eq!(edges.len(), 1, "only the in-window run correlates");
    let GraphRecord::Edge {
        confidence,
        basis,
        target,
        ..
    } = edges[0]
    else {
        panic!("edge");
    };
    assert_eq!(basis, &Some(CorrelationBasis::TemporalCorrelation));
    assert_eq!(confidence.as_deref(), Some("0.5"));
    assert_eq!(target, "agent_memory:v1:run_in");
    assert_eq!(result.totals.temporal_correlation_edges, 1);
    assert!(result.totals.temporal_correlation_enabled);
}

#[test]
fn overlapping_runs_each_get_one_edge() {
    let (src_id, src) = log_source(ANCHOR, "app.log", "h1");
    let (sig_id, sig) = error_signature("boom", "2026-07-01T12:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_a", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
        agent_run("run_b", "2026-07-01T11:30:00Z", "2026-07-01T12:30:00Z"),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());
    assert_eq!(
        emitted_edges(&result).len(),
        2,
        "both overlapping runs get an edge; no silent winner"
    );
    assert_eq!(result.totals.temporal_correlation_edges, 2);
}

#[test]
fn out_of_window_no_hash_is_uncorrelated() {
    let (src_id, src) = log_source(ANCHOR, "app.log", "h1");
    let (sig_id, sig) = error_signature("boom", "2026-07-01T00:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_out", "2026-07-05T11:00:00Z", "2026-07-05T13:00:00Z"),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());
    assert!(emitted_edges(&result).is_empty());
    assert_eq!(result.totals.uncorrelated, 1);
    assert!(result.signatures[0].uncorrelated);
    assert_eq!(result.totals.cross_repo_rejected, 0);
}

#[test]
fn foreign_repo_signature_is_rejected_not_correlated() {
    // Foreign LogSource scanned under a different repo id: it will not recompute
    // to the anchor, so its in-window temporal candidate is suppressed.
    let (src_id, src) = log_source("codegraph:v1:repo_other", "app.log", "h1");
    let (sig_id, sig) = error_signature("boom", "2026-07-01T12:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_in", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());
    assert!(
        emitted_edges(&result).is_empty(),
        "no cross-repository temporal edge"
    );
    assert_eq!(result.totals.cross_repo_rejected, 1);
    assert_eq!(result.totals.temporal_correlation_edges, 0);
}

#[test]
fn multiple_anchors_disable_temporal() {
    let (src_id, src) = log_source(ANCHOR, "app.log", "h1");
    let (sig_id, sig) = error_signature("boom", "2026-07-01T12:00:00Z");
    let records = vec![
        repository(ANCHOR),
        repository("codegraph:v1:repo_second"),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_in", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());
    assert!(!result.totals.temporal_correlation_enabled);
    assert!(emitted_edges(&result).is_empty());
    assert_eq!(result.totals.cross_repo_rejected, 1);
}

#[test]
fn task_link_reuses_references_task() {
    let hash = "deadbeefhash";
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let (sig_id, sig) = error_signature("boom", "2026-07-01T12:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_in", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
        task("t1"),
        references_task("agent_memory:v1:run_in", "project:v1:t1"),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());
    let task_edges: Vec<&GraphRecord> = result
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::ReferencesTask,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(task_edges.len(), 1, "one signature->task reference");
    let GraphRecord::Edge {
        source,
        target,
        basis,
        ..
    } = task_edges[0]
    else {
        panic!("edge");
    };
    assert_eq!(source, &sig_id);
    assert_eq!(target, "project:v1:t1");
    assert!(
        basis.is_none(),
        "REFERENCES_TASK carries no correlation basis"
    );
    assert_eq!(result.totals.task_link_edges, 1);
}

#[test]
fn tolerance_widens_the_window() {
    let (src_id, src) = log_source(ANCHOR, "app.log", "h1");
    // 30s after the window end; only correlated with tolerance >= 30s.
    let (sig_id, sig) = error_signature("boom", "2026-07-01T13:00:30Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_in", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
    ];
    let strict = link_logs(&records, &LinkLogsOptions::default());
    assert!(
        emitted_edges(&strict).is_empty(),
        "strict window excludes it"
    );
    let loose = link_logs(
        &records,
        &LinkLogsOptions {
            tolerance_seconds: 60,
            at_commit: None,
        },
    );
    assert_eq!(emitted_edges(&loose).len(), 1, "tolerance admits it");
}

#[test]
fn dual_representation_agrees() {
    let hash = "deadbeefhash";
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let (sig_id, sig) = error_signature("boom", "2026-07-01T12:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_in", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
        command_run("cmd1", hash),
    ];
    let result = link_logs(&records, &LinkLogsOptions::default());

    let mut edge_pairs: Vec<(String, String, String)> = result
        .records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Edge {
                label,
                source,
                target,
                ..
            } if matches!(label, EdgeLabel::EmittedDuring | EdgeLabel::ReferencesTask) => {
                Some((source.clone(), label.as_str().to_owned(), target.clone()))
            }
            _ => None,
        })
        .collect();
    edge_pairs.sort();

    let mut link_pairs: Vec<(String, String, String)> = Vec::new();
    for r in &result.records {
        if let GraphRecord::Node {
            kind: NodeKind::ErrorSignature,
            id,
            evidence_links: Some(links),
            ..
        } = r
        {
            for l in links {
                link_pairs.push((
                    id.clone(),
                    l.relation.clone(),
                    l.target_record_id.clone().unwrap(),
                ));
            }
        }
    }
    link_pairs.sort();

    assert!(!edge_pairs.is_empty());
    assert_eq!(edge_pairs, link_pairs, "edges and evidence links agree");
}

#[test]
fn output_is_byte_identical_across_runs() {
    let hash = "deadbeefhash";
    let (src_id, src) = log_source(ANCHOR, "app.log", hash);
    let (sig_id, sig) = error_signature("boom", "2026-07-01T12:00:00Z");
    let records = vec![
        repository(ANCHOR),
        src,
        sig,
        captured_from(&sig_id, &src_id),
        agent_run("run_in", "2026-07-01T11:00:00Z", "2026-07-01T13:00:00Z"),
        command_run("cmd1", hash),
    ];
    let a = link_logs(&records, &LinkLogsOptions::default());
    let b = link_logs(&records, &LinkLogsOptions::default());
    let ja: Vec<String> = a
        .records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    let jb: Vec<String> = b
        .records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    assert_eq!(ja, jb);
}
