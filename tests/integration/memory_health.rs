#![allow(
    missing_docs,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::doc_markdown
)]

use aletheia_egregore::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, GraphRecord, NodeKind,
    VERIFICATION_SCHEMA_VERSION,
};
use aletheia_egregore::memory_health::{
    MemoryHealthConfig, MemoryHealthCounts, run_memory_health_audit,
};
use assert_cmd::Command;
use serde_json::Value;

fn link(target: &str, domain: &str, relation: &str) -> EvidenceLink {
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

fn build_observation(
    id: &str,
    agent_id: Option<&str>,
    session_id: Option<&str>,
    source_handle: Option<&str>,
    observed_at: Option<&str>,
    links: Vec<EvidenceLink>,
    superseded_by: Option<&str>,
) -> GraphRecord {
    let mut rec = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id: aid,
        agent_kind,
        session_id: sid,
        observed_at: obs_at,
        source_handle: sh,
        evidence_links,
        superseded_by: sup_by,
        ..
    } = &mut rec
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = agent_id.map(String::from);
        *agent_kind = Some("claude-code".to_owned());
        *sid = session_id.map(String::from);
        *obs_at = observed_at.map(String::from);
        *sh = source_handle.map(String::from);
        if !links.is_empty() {
            *evidence_links = Some(links);
        }
        *sup_by = superseded_by.map(String::from);
    }
    rec
}

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn build_fixture_records() -> Vec<GraphRecord> {
    let mut records = Vec::new();

    // 1. Verification node (to verify the verified observation)
    let mut v_node = GraphRecord::node(
        "verification_1".to_owned(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification 1".to_owned(),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut v_node {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
    }
    records.push(v_node);

    // 2. Verified observation (V)
    // Has full provenance, verification link
    let obs_verified = build_observation(
        "obs_verified",
        Some("agent_1"),
        Some("session_1"),
        Some("source_1.jsonl"),
        Some("2026-06-03T12:00:00Z"),
        vec![link("verification_1", "verification", "VALIDATED_BY")],
        None,
    );
    records.push(obs_verified);

    // 3. Unverified observation (U)
    // Has full provenance, but no evidence links
    let obs_unverified = build_observation(
        "obs_unverified",
        Some("agent_1"),
        Some("session_1"),
        Some("source_1.jsonl"),
        Some("2026-06-03T12:10:00Z"),
        vec![],
        None,
    );
    records.push(obs_unverified);

    // 4. Superseded observation (S)
    // Has full provenance, superseded_by set
    let obs_superseded = build_observation(
        "obs_superseded",
        Some("agent_1"),
        Some("session_1"),
        Some("source_2.jsonl"),
        Some("2026-06-03T12:20:00Z"),
        vec![],
        Some("obs_verified"),
    );
    records.push(obs_superseded);

    // 5. Contradicted observation (X)
    // Has full provenance, contradicted edge to obs_verified
    let obs_contradicted = build_observation(
        "obs_contradicted",
        Some("agent_2"),
        Some("session_2"),
        Some("source_2.jsonl"),
        Some("2026-06-03T12:30:00Z"),
        vec![],
        None,
    );
    records.push(obs_contradicted);
    records.push(GraphRecord::Edge {
        id: "edge_contradicts".to_owned(),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        label: EdgeLabel::Contradicts,
        source: "obs_contradicted".to_owned(),
        target: "obs_verified".to_owned(),
        confidence: None,
        resolution: None,
        temporal: None,
        summary: "Contradiction edge".to_owned(),
        producer: None,
    });

    // 6. Dangling evidence observation (D)
    // Has source_handle but evidence link target does not exist
    let obs_dangling = build_observation(
        "obs_dangling",
        Some("agent_2"),
        Some("session_2"),
        Some("source_3.jsonl"),
        Some("2026-06-03T12:40:00Z"),
        vec![link("nonexistent_target", "codegraph", "OBSERVES")],
        None,
    );
    records.push(obs_dangling);

    // 7. Missing provenance observation (orphaned)
    // Lacks agent_id, session_id, observed_at, and source_handle
    let obs_no_provenance =
        build_observation("obs_no_provenance", None, None, None, None, vec![], None);
    records.push(obs_no_provenance);

    // 8. Tombstoned observation (deleted from current state)
    // Should be excluded from the base universe of active observations
    let obs_deleted = build_observation(
        "obs_deleted",
        Some("agent_1"),
        Some("session_1"),
        Some("source_1.jsonl"),
        Some("2026-06-03T12:50:00Z"),
        vec![],
        None,
    );
    records.push(obs_deleted);
    records.push(GraphRecord::Tombstone {
        id: "tombstone_deleted".to_owned(),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: "obs_deleted".to_owned(),
        summary: "Deleted observation".to_owned(),
        producer: None,
    });

    records
}

#[test]
fn test_memory_health_calculations() {
    let records = build_fixture_records();

    let config = MemoryHealthConfig {
        min_provenance_coverage: 0.8,
        max_dangling_evidence: 0.2,
        max_unverified: None,
        max_current_guidance_contamination: None,
    };

    let report = run_memory_health_audit(&records, &config);

    // active observations: obs_verified, obs_unverified, obs_superseded, obs_contradicted, obs_dangling, obs_no_provenance
    assert_eq!(report.total_observations, 6);

    // provenance_coverage: 5 of 6 (all except obs_no_provenance)
    assert_eq!(
        report.provenance_coverage,
        MemoryHealthCounts {
            numerator: 5,
            denominator: 6,
            ratio: 5.0 / 6.0,
        }
    );

    // unverified: 5 of 6 (all except obs_verified)
    assert_eq!(report.unverified.numerator, 5);
    assert_eq!(report.unverified.denominator, 6);

    // superseded: 1 of 6 (obs_superseded)
    assert_eq!(report.superseded.numerator, 1);

    // contradicted: 2 of 6 (obs_contradicted, obs_verified)
    assert_eq!(report.contradicted.numerator, 2);

    // dangling_evidence: 1 of 6 (obs_dangling)
    assert_eq!(report.dangling_evidence.numerator, 1);

    // missing_provenance: 1 of 6 (obs_no_provenance)
    assert_eq!(report.missing_provenance.numerator, 1);

    // weak_provenance: 1 of 6 (obs_dangling has source_handle but is dangling)
    assert_eq!(report.weak_provenance.numerator, 1);

    // unratified_memory: 4 of 6 (obs_unverified, obs_superseded, obs_contradicted, obs_dangling)
    assert_eq!(report.unratified_memory.numerator, 4);

    // stale_or_contradicted_memory: 3 of 6 (obs_superseded, obs_contradicted, obs_verified)
    assert_eq!(report.stale_or_contradicted_memory.numerator, 3);

    // current_guidance_contamination: 5 of 5 (obs_superseded is excluded from denominator)
    assert_eq!(report.current_guidance_contamination.numerator, 5);
    assert_eq!(report.current_guidance_contamination.denominator, 5);

    // concentration: source_1: 2, source_2: 2, source_3: 1
    assert_eq!(
        report.source_transcript_concentration.get("source_1.jsonl"),
        Some(&2)
    );
    assert_eq!(
        report.source_transcript_concentration.get("source_2.jsonl"),
        Some(&2)
    );
    assert_eq!(
        report.source_transcript_concentration.get("source_3.jsonl"),
        Some(&1)
    );

    // age distribution: oldest = 2026-06-03T12:00:00Z, newest = 2026-06-03T12:40:00Z (ignoring obs_deleted)
    assert_eq!(
        report.age_distribution.oldest,
        Some("2026-06-03T12:00:00Z".to_owned())
    );
    assert_eq!(
        report.age_distribution.newest,
        Some("2026-06-03T12:40:00Z".to_owned())
    );
    assert_eq!(report.age_distribution.buckets.get("2026-06"), Some(&5));

    // Thresholds:
    // min_provenance_coverage = 0.8: observed is 0.8333 -> ok
    // max_dangling_evidence = 0.2: observed is 0.1666 -> ok
    assert!(report.ok);
    assert!(report.diagnostics.is_empty());
}

#[test]
fn test_memory_health_cli() {
    let records = build_fixture_records();
    let tmp = tempfile::tempdir().expect("temp dir");
    let graph_path = tmp.path().join("memory_health_seeded.jsonl");

    // Write records to JSONL file
    let jsonl: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&graph_path, jsonl).unwrap();

    // 1. Run with customized thresholds that should pass
    let output = egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            graph_path.to_str().unwrap(),
            "--min-provenance-coverage",
            "0.8",
            "--max-dangling-evidence",
            "0.2",
        ])
        .assert()
        .success();

    let report: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["total_observations"], 6);
    assert!(report["diagnostics"].as_array().unwrap().is_empty());

    // 2. Run with default thresholds (coverage 1.0, dangling 0.0) which should trip the gate (exit 1)
    let output_fail = egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            graph_path.to_str().unwrap(),
        ])
        .assert()
        .code(1);

    let report_fail: Value = serde_json::from_slice(&output_fail.get_output().stdout).unwrap();
    assert_eq!(report_fail["ok"], false);
    let diags = report_fail["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 2);

    let has_cov_diag = diags
        .iter()
        .any(|d| d["code"] == "provenance_coverage_below_threshold");
    let has_dang_diag = diags
        .iter()
        .any(|d| d["code"] == "dangling_evidence_above_threshold");
    assert!(has_cov_diag, "Should have provenance coverage failure");
    assert!(has_dang_diag, "Should have dangling evidence failure");

    // 3. Run with max-unverified threshold that fails
    let output_unverified_fail = egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            graph_path.to_str().unwrap(),
            "--min-provenance-coverage",
            "0.8",
            "--max-dangling-evidence",
            "0.2",
            "--max-unverified",
            "0.5",
        ])
        .assert()
        .code(1);
    let report_unverified_fail: Value =
        serde_json::from_slice(&output_unverified_fail.get_output().stdout).unwrap();
    assert_eq!(report_unverified_fail["ok"], false);
    assert!(
        report_unverified_fail["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "unverified_above_threshold")
    );

    // 4. Run with max-contamination threshold that fails
    let output_contamination_fail = egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            graph_path.to_str().unwrap(),
            "--min-provenance-coverage",
            "0.8",
            "--max-dangling-evidence",
            "0.2",
            "--max-current-guidance-contamination",
            "0.5",
        ])
        .assert()
        .code(1);
    let report_contamination_fail: Value =
        serde_json::from_slice(&output_contamination_fail.get_output().stdout).unwrap();
    assert_eq!(report_contamination_fail["ok"], false);
    assert!(
        report_contamination_fail["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "current_guidance_contamination_above_threshold")
    );

    // 5. Usage error on missing graph path (exit 2)
    egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            "nonexistent_file.jsonl",
        ])
        .assert()
        .code(2);

    // 6. Determinism check: verify 5 consecutive runs yield byte-identical stdout
    let first = egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            graph_path.to_str().unwrap(),
            "--min-provenance-coverage",
            "0.8",
            "--max-dangling-evidence",
            "0.2",
        ])
        .assert()
        .get_output()
        .stdout
        .clone();

    for _ in 0..4 {
        let again = egregore()
            .args([
                "audit",
                "memory-health",
                "--graph",
                graph_path.to_str().unwrap(),
                "--min-provenance-coverage",
                "0.8",
                "--max-dangling-evidence",
                "0.2",
            ])
            .assert()
            .get_output()
            .stdout
            .clone();
        assert_eq!(first, again, "Output must be byte-identical across runs");
    }
}

#[test]
fn test_memory_health_cli_boundary_errors() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let empty_graph_path = tmp.path().join("empty_graph.jsonl");
    std::fs::write(&empty_graph_path, "").unwrap();

    // 1. Empty graph triggers "no_memory" diagnostic and fails the gate (exit 1)
    let output_empty = egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            empty_graph_path.to_str().unwrap(),
        ])
        .assert()
        .code(1);

    let report_empty: Value = serde_json::from_slice(&output_empty.get_output().stdout).unwrap();
    assert_eq!(report_empty["ok"], false);
    assert_eq!(report_empty["total_observations"], 0);
    assert_eq!(report_empty["diagnostics"][0]["code"], "no_memory");

    // 2. Invalid parameter threshold above bounds (exit 2)
    egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            empty_graph_path.to_str().unwrap(),
            "--min-provenance-coverage",
            "1.5",
        ])
        .assert()
        .code(2);

    // 3. Invalid parameter threshold below bounds (exit 2)
    egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            empty_graph_path.to_str().unwrap(),
            "--max-dangling-evidence",
            "-0.1",
        ])
        .assert()
        .code(2);

    // 4. Missing both --graph and --data-dir (exit 2)
    egregore().args(["audit", "memory-health"]).assert().code(2);

    // 5. Specifying both --graph and --data-dir (exit 2)
    egregore()
        .args([
            "audit",
            "memory-health",
            "--graph",
            empty_graph_path.to_str().unwrap(),
            "--data-dir",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .code(2);
}
