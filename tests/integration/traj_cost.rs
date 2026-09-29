//! Tests for issue #132: capture and query agent run cost from imported trajectories.
//!
//! Written RED-first: these tests define the contract before the implementation
//! exists. The importer half exercises `import_traj` against the real
//! `swe_agent_basic` fixture (whose `info` block carries a fully populated cost
//! section) plus crafted minimal trajectories. The query half exercises the
//! `eg query cost` core (`query::cost_rollup`) over imported records.

use std::path::{Path, PathBuf};

use aletheia_egregore::{
    import_traj,
    ir::{EdgeLabel, GraphRecord},
    query::{CostFilters, CostVerificationOutcome, cost_rollup},
    traj::ImportOptions,
};

const FIXTURE: &str = "tests/fixtures/agent_memory/swe_agent_basic/trajectory.traj";

// ── helpers ─────────────────────────────────────────────────────────────────

fn do_import() -> Vec<GraphRecord> {
    import_traj(Path::new(FIXTURE), &ImportOptions::default())
        .expect("import_traj must not fail on the basic fixture")
        .records()
        .to_vec()
}

fn cost_usage_nodes(records: &[GraphRecord]) -> Vec<&GraphRecord> {
    records
        .iter()
        .filter(|r| r.node_kind_name() == Some("CostUsage"))
        .collect()
}

fn node_text(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node { text, .. } => text.as_deref(),
        _ => None,
    }
}

fn node_field<'a>(record: &'a GraphRecord, field: &str) -> Option<&'a str> {
    match record {
        GraphRecord::Node {
            id,
            session_id,
            source_handle,
            source_artifact_path,
            source_artifact_hash,
            importer_version,
            ..
        } => match field {
            "id" => Some(id.as_str()),
            "session_id" => session_id.as_deref(),
            "source_handle" => source_handle.as_deref(),
            "source_artifact_path" => source_artifact_path.as_deref(),
            "source_artifact_hash" => source_artifact_hash.as_deref(),
            "importer_version" => importer_version.as_deref(),
            _ => None,
        },
        _ => None,
    }
}

fn payload_of(node: &GraphRecord) -> serde_json::Value {
    let text = node_text(node).expect("CostUsage node must carry a text payload");
    serde_json::from_str(text).expect("CostUsage text payload must be valid JSON")
}

fn run_id_of(records: &[GraphRecord]) -> String {
    records
        .iter()
        .find(|r| r.node_kind_name() == Some("AgentRun"))
        .and_then(|r| node_field(r, "id"))
        .expect("import must emit an AgentRun")
        .to_owned()
}

fn authored_by_edges(records: &[GraphRecord]) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for r in records {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = r
            && *label == EdgeLabel::AuthoredBy
        {
            out.push((source.as_str(), target.as_str()));
        }
    }
    out
}

/// Write a minimal synthetic `.traj` with the given `info` object and no
/// messages; returns the temp file path.
fn write_traj(info_json: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("egregore-traj-cost-tests");
    std::fs::create_dir_all(&dir).expect("temp dir creatable");
    let path = dir.join(format!(
        "traj-cost-{}.traj",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let body = format!(
        "{{\"trajectory_format\":\"mini-swe-agent-1.2\",\"info\":{info_json},\"messages\":[]}}"
    );
    std::fs::write(&path, body).expect("traj writable");
    path
}

// ── AC2: one CostUsage record per AgentRun with exact source values ───────────

#[test]
fn traj_import_emits_exactly_one_cost_usage_for_swe_agent_basic() {
    let records = do_import();
    let cost = cost_usage_nodes(&records);
    assert_eq!(
        cost.len(),
        1,
        "expected exactly one CostUsage record per AgentRun, got {}",
        cost.len()
    );
}

#[test]
fn cost_usage_values_equal_source_exactly() {
    let records = do_import();
    let cost = cost_usage_nodes(&records);
    let payload = payload_of(cost[0]);

    // 100% of present cost/token/duration fields persist with exact values.
    assert_eq!(payload["actual_cost_usd"], serde_json::json!(0.05));
    assert_eq!(payload["total_cost_usd"], serde_json::json!(0.05));
    assert_eq!(payload["baseline_cost_usd"], serde_json::json!(0.48));
    assert_eq!(
        payload["baseline_cost_model"],
        serde_json::json!("claude-3-5-sonnet")
    );
    assert_eq!(payload["prompt_tokens"], serde_json::json!(2100));
    // cache_read_tokens is present-as-zero in the source: preserved as 0,
    // never rewritten to unknown.
    assert_eq!(payload["cache_read_tokens"], serde_json::json!(0));
    assert_eq!(payload["completion_tokens"], serde_json::json!(410));
    assert_eq!(payload["duration_secs"], serde_json::json!(22.5));
    assert_eq!(
        payload["model_name"],
        serde_json::json!("claude-3-5-sonnet")
    );
}

#[test]
fn cost_usage_links_to_run_and_session() {
    let records = do_import();
    let cost = cost_usage_nodes(&records);
    let cost_id = node_field(cost[0], "id").expect("CostUsage has id");
    let run_id = run_id_of(&records);
    let session_id = node_field(cost[0], "session_id").expect("CostUsage has session_id");

    // CostUsage -[AuthoredBy]-> AgentRun
    let edges = authored_by_edges(&records);
    assert!(
        edges
            .iter()
            .any(|(from, to)| *from == cost_id && *to == run_id),
        "no AuthoredBy edge from CostUsage to its AgentRun"
    );

    // ... and the run reaches the session via the existing SessionOf edge.
    let session_edge = records.iter().any(|r| {
        matches!(r, GraphRecord::Edge { label, source, target, .. }
            if *label == EdgeLabel::SessionOf && source == &run_id && target == session_id)
    });
    assert!(
        session_edge,
        "AgentRun must keep its SessionOf edge to the AgentSession"
    );
}

// ── AC1/AC5: spec'd fields, provenance, derivation label ──────────────────────

#[test]
fn cost_usage_carries_full_provenance_and_derivation_label() {
    let records = do_import();
    let cost = cost_usage_nodes(&records);
    let node = cost[0];
    let payload = payload_of(node);

    // Derivation label: transcript-derived, never a deterministic code fact.
    assert_eq!(
        payload["derivation"],
        serde_json::json!("transcript_derived"),
        "CostUsage must be labeled transcript-derived"
    );

    // Full provenance on the node: source artifact path + content hash +
    // importer version, plus the denormalized source handle.
    for field in [
        "source_artifact_path",
        "source_artifact_hash",
        "importer_version",
        "source_handle",
        "session_id",
    ] {
        assert!(
            node_field(node, field).is_some(),
            "CostUsage node missing provenance field {field}"
        );
    }
    let handle = node_field(node, "source_handle").unwrap();
    assert!(
        handle.contains("trajectory.traj"),
        "source_handle should name the artifact, got {handle}"
    );
}

#[test]
fn cost_usage_passes_through_redaction_pipeline() {
    // A task text that carries a cloud-credential-shaped secret must not land
    // in the payload verbatim.
    let path =
        write_traj(r#"{"task":"AKIAIOSFODNN7EXAMPLE","actual_cost_usd":0.01,"model_name":"m"}"#);
    let records = import_traj(&path, &ImportOptions::default())
        .expect("import ok")
        .records()
        .to_vec();
    let cost = cost_usage_nodes(&records);
    assert_eq!(cost.len(), 1, "cost record should be emitted");
    let payload = payload_of(cost[0]);
    let task_handle = payload["task_handle"]
        .as_str()
        .expect("task_handle present");
    assert!(
        !task_handle.contains("AKIA"),
        "task_handle must be a hash, never raw task text"
    );
    std::fs::remove_file(&path).ok();
}

// ── AC3: absent fields degrade to explicit unknown, never zero ────────────────

#[test]
fn absent_fields_degrade_to_unknown_not_zero() {
    // Only actual_cost_usd present; everything else absent.
    let path = write_traj(r#"{"actual_cost_usd":0.02,"model_name":"some-model"}"#);
    let records = import_traj(&path, &ImportOptions::default())
        .expect("import ok")
        .records()
        .to_vec();
    let cost = cost_usage_nodes(&records);
    assert_eq!(cost.len(), 1, "one cost record expected");
    let payload = payload_of(cost[0]);

    assert_eq!(payload["actual_cost_usd"], serde_json::json!(0.02));
    // Explicit unknown markers: JSON null, never fabricated zero/defaults.
    for field in [
        "total_cost_usd",
        "baseline_cost_usd",
        "baseline_cost_model",
        "prompt_tokens",
        "cache_read_tokens",
        "completion_tokens",
        "duration_secs",
    ] {
        assert!(
            payload.get(field).is_some(),
            "payload must carry an explicit marker for absent field {field}"
        );
        assert!(
            payload[field].is_null(),
            "absent field {field} must be null (unknown), not fabricated: {}",
            payload[field]
        );
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn zero_token_count_is_present_data_not_unknown() {
    // A present zero must stay zero — presence-based, not non-zero-based.
    let path = write_traj(
        r#"{"token_usage":{"prompt_tokens":0,"completion_tokens":0},"duration_secs":1.0}"#,
    );
    let records = import_traj(&path, &ImportOptions::default())
        .expect("import ok")
        .records()
        .to_vec();
    let cost = cost_usage_nodes(&records);
    assert_eq!(cost.len(), 1, "zero-valued fields are real data: emit");
    let payload = payload_of(cost[0]);
    assert_eq!(payload["prompt_tokens"], serde_json::json!(0));
    assert_eq!(payload["completion_tokens"], serde_json::json!(0));
    assert!(payload["cache_read_tokens"].is_null());
    std::fs::remove_file(&path).ok();
}

// ── AC4: no cost data → no record ─────────────────────────────────────────────

#[test]
fn no_cost_data_means_no_cost_usage_record() {
    let path = write_traj(r#"{"model_name":"some-model","outcome":"success"}"#);
    let records = import_traj(&path, &ImportOptions::default())
        .expect("import ok")
        .records()
        .to_vec();
    assert_eq!(
        cost_usage_nodes(&records).len(),
        0,
        "no cost/token/duration data → no placeholder CostUsage record"
    );
    std::fs::remove_file(&path).ok();
}

// ── AC7: 5x re-import is byte-identical ───────────────────────────────────────

#[test]
fn reimport_5x_produces_byte_identical_cost_usage_jsonl() {
    let opts = ImportOptions::default();
    let path = Path::new(FIXTURE);
    let first = import_traj(path, &opts)
        .expect("run 0 failed")
        .to_jsonl()
        .expect("jsonl 0 failed");
    assert!(
        first.lines().any(|l| l.contains("\"kind\":\"CostUsage\"")),
        "CostUsage records must be part of the JSONL under test"
    );
    for i in 1..=4 {
        let run = import_traj(path, &opts)
            .unwrap_or_else(|_| panic!("run {i} failed"))
            .to_jsonl()
            .unwrap_or_else(|_| panic!("jsonl {i} failed"));
        assert_eq!(
            first, run,
            "import output differed on run {i} — CostUsage JSONL not byte-identical"
        );
    }
}

// ── AC6: eg query cost core ───────────────────────────────────────────────────

#[test]
fn query_cost_rollup_returns_row_and_exact_sums() {
    let records = do_import();
    let rollup = cost_rollup(&records, &CostFilters::default(), 100);

    assert_eq!(rollup.rows.len(), 1, "one row for the single imported run");
    let row = &rollup.rows[0];
    assert!(
        !row.record_id.is_empty(),
        "row carries the CostUsage record id"
    );
    assert_eq!(
        row.run_id.as_deref(),
        Some(run_id_of(&records).as_str()),
        "row carries the owning run id"
    );
    assert!(
        row.session_id.as_deref().is_some_and(|s| !s.is_empty()),
        "row carries the session id"
    );
    assert!(
        row.source_handle
            .as_deref()
            .is_some_and(|h| h.contains("trajectory.traj")),
        "row carries the source handle"
    );
    assert_eq!(row.actual_cost_usd, Some(0.05));
    assert_eq!(row.prompt_tokens, Some(2100));
    assert_eq!(row.completion_tokens, Some(410));
    assert_eq!(row.duration_secs, Some(22.5));

    // Aggregate sums match source values within float rounding.
    let t = &rollup.totals;
    assert_eq!(t.matching_rows, 1);
    assert!((t.actual_cost_usd.unwrap() - 0.05).abs() < 1e-9);
    assert!((t.total_cost_usd.unwrap() - 0.05).abs() < 1e-9);
    assert!((t.baseline_cost_usd.unwrap() - 0.48).abs() < 1e-9);
    assert_eq!(t.prompt_tokens, Some(2100));
    assert_eq!(t.cache_read_tokens, Some(0));
    assert_eq!(t.completion_tokens, Some(410));
    assert!((t.duration_secs.unwrap() - 22.5).abs() < 1e-9);

    // The rollup is labeled transcript-derived, never a code fact.
    assert!(
        !rollup.disclaimer.is_empty(),
        "rollup must carry the epistemic disclaimer"
    );
}

#[test]
fn query_cost_filters_by_session() {
    let records = do_import();
    let session_id = node_field(cost_usage_nodes(&records)[0], "session_id")
        .expect("session id")
        .to_owned();

    let matching = cost_rollup(
        &records,
        &CostFilters {
            session: Some(session_id[..16].to_owned()),
            ..Default::default()
        },
        100,
    );
    assert_eq!(matching.rows.len(), 1, "session prefix filter matches");

    let missing = cost_rollup(
        &records,
        &CostFilters {
            session: Some("deadbeef-no-such-session".to_owned()),
            ..Default::default()
        },
        100,
    );
    assert_eq!(
        missing.rows.len(),
        0,
        "unknown session filters everything out"
    );
    assert_eq!(missing.totals.matching_rows, 0);
    assert!(
        missing.totals.actual_cost_usd.is_none(),
        "sums over an empty set are unknown, not zero"
    );
}

#[test]
fn query_cost_filters_by_task_handle() {
    let records = do_import();
    let payload = payload_of(cost_usage_nodes(&records)[0]);
    let task_handle = payload["task_handle"]
        .as_str()
        .expect("fixture has a task")
        .to_owned();

    let matching = cost_rollup(
        &records,
        &CostFilters {
            task: Some(task_handle[..12].to_owned()),
            ..Default::default()
        },
        100,
    );
    assert_eq!(matching.rows.len(), 1, "task handle prefix filter matches");

    let missing = cost_rollup(
        &records,
        &CostFilters {
            task: Some("000000000000no-such-task".to_owned()),
            ..Default::default()
        },
        100,
    );
    assert_eq!(missing.rows.len(), 0);
}

#[test]
fn query_cost_filters_by_verification_outcome() {
    let records = do_import();
    // The fixture's info block carries "verification_status": "verified".
    let verified = cost_rollup(
        &records,
        &CostFilters {
            verification: Some(CostVerificationOutcome::Verified),
            ..Default::default()
        },
        100,
    );
    assert_eq!(
        verified.rows.len(),
        1,
        "verified filter matches the fixture run"
    );
    assert_eq!(
        verified.rows[0].verification_outcome.as_deref(),
        Some("verified")
    );

    let failed = cost_rollup(
        &records,
        &CostFilters {
            verification: Some(CostVerificationOutcome::Failed),
            ..Default::default()
        },
        100,
    );
    assert_eq!(
        failed.rows.len(),
        0,
        "failed filter excludes the verified run"
    );
}

#[test]
fn query_cost_failed_filter_matches_synthetic_failed_run() {
    let mut records = do_import();
    // Craft a second CostUsage whose payload reports a failed verification.
    let mut failed_node = cost_usage_nodes(&records)[0].clone();
    let GraphRecord::Node { id, text, .. } = &mut failed_node else {
        panic!("cost usage test template must be a node");
    };
    let mut payload: serde_json::Value = serde_json::from_str(
        text.as_deref()
            .expect("cost usage node carries a JSON payload"),
    )
    .expect("payload is JSON");
    payload["verification_outcome"] = serde_json::Value::from("failed");
    *text = Some(payload.to_string());
    id.push_str("-failed");
    records.push(failed_node);

    let failed = cost_rollup(
        &records,
        &CostFilters {
            verification: Some(CostVerificationOutcome::Failed),
            ..Default::default()
        },
        100,
    );
    assert_eq!(
        failed.rows.len(),
        1,
        "failed filter positively matches the synthetic failed run"
    );
    assert_eq!(
        failed.rows[0].verification_outcome.as_deref(),
        Some("failed")
    );

    let verified = cost_rollup(
        &records,
        &CostFilters {
            verification: Some(CostVerificationOutcome::Verified),
            ..Default::default()
        },
        100,
    );
    assert_eq!(
        verified.rows.len(),
        1,
        "verified filter excludes the synthetic failed run"
    );
}

#[test]
fn query_cost_rollup_is_deterministic() {
    let records = do_import();
    let first = serde_json::to_string(&cost_rollup(&records, &CostFilters::default(), 100))
        .expect("serializable");
    let second = serde_json::to_string(&cost_rollup(&records, &CostFilters::default(), 100))
        .expect("serializable");
    assert_eq!(first, second, "rollup must be deterministic across runs");
}

#[test]
fn query_cost_token_overflow_degrades_total_to_unknown() {
    let mut records = do_import();
    // Craft a second CostUsage whose prompt tokens push the aggregate past
    // u64::MAX when added to the fixture's 2100: clone the imported node and
    // rewrite just its id and payload text.
    let mut overflow_node = cost_usage_nodes(&records)[0].clone();
    let GraphRecord::Node { id, text, .. } = &mut overflow_node else {
        panic!("cost usage test template must be a node");
    };
    let mut payload: serde_json::Value = serde_json::from_str(
        text.as_deref()
            .expect("cost usage node carries a JSON payload"),
    )
    .expect("payload is JSON");
    payload["prompt_tokens"] = serde_json::Value::from(u64::MAX);
    *text = Some(payload.to_string());
    id.push_str("-overflow");
    records.push(overflow_node);

    let rollup = cost_rollup(&records, &CostFilters::default(), 100);
    assert_eq!(rollup.rows.len(), 2, "both runs appear as rows");
    assert_eq!(
        rollup.totals.prompt_tokens, None,
        "u64 overflow degrades the token total to unknown instead of silently saturating"
    );
    // Non-overflowing totals are unaffected.
    assert_eq!(
        rollup.totals.completion_tokens,
        Some(410 + 410),
        "totals that fit keep their exact sums"
    );
}
