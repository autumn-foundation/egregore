//! RED tests for issue #184: supersede / contradict at write time.
//!
//! A live agent must be able to author a `SUPERSEDES` (or `CONTRADICTS`) edge
//! from a new `eg write observation` to a prior observation-class record, with
//! the target validated at write time against a local store (`--graph` JSONL
//! or embedded `--data-dir`).

#![allow(missing_docs)]

use std::fs;

use aletheia_egregore::{
    evidence::{EvidenceProvenance, ObservationRequest, build_observation_records},
    ir::{EdgeLabel, EvidenceLink, GraphRecord, NodeKind},
    query::memory_audit_context,
    supersede_write::{
        SupersessionError, SupersessionTarget, build_supersession_edge,
        check_supersession_conflict, validate_supersession_target,
    },
    temporal_status::TemporalResolver,
};
use assert_cmd::Command;

// ── Shared fixtures ─────────────────────────────────────────────────────────

fn valid_provenance() -> EvidenceProvenance {
    EvidenceProvenance {
        agent_id: "agent-1".to_owned(),
        agent_kind: "other".to_owned(),
        session_id: "sess-1".to_owned(),
        observed_at: "2026-09-26T12:00:00Z".to_owned(),
        source_handle: Some("src/lib.rs:sha256:abc123".to_owned()),
    }
}

fn dummy_evidence_link(target_id: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target_id.to_owned()),
        target_domain: "codegraph".to_owned(),
        relation: EdgeLabel::Observes.as_str().to_owned(),
        confidence: "0.9".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

fn observation_request(text: &str) -> ObservationRequest {
    ObservationRequest {
        provenance: valid_provenance(),
        text: text.to_owned(),
        confidence: 0.9,
        evidence_links: vec![dummy_evidence_link("codegraph:v1:deadbeef")],
        supersession: None,
    }
}

/// Minimal node record deserialized through the JSONL contract.
fn node_record(id: &str, kind: &str) -> GraphRecord {
    serde_json::from_value(serde_json::json!({
        "record_type": "node",
        "id": id,
        "kind": kind,
        "schema_version": 1,
        "summary": "test node",
    }))
    .expect("node should deserialize")
}

/// A real prior observation minted by the production builder.
fn prior_observation() -> aletheia_egregore::evidence::EvidenceWriteOutcome {
    build_observation_records(&observation_request("blocking IO here"))
        .expect("prior observation should build")
}

fn prior_observation_id() -> String {
    prior_observation().record_id
}

// ── Builder: edge authorship ────────────────────────────────────────────────

#[test]
fn supersede_write_authors_supersedes_edge() {
    let target = prior_observation_id();
    let mut req = observation_request("blocking IO deadlocked, use async");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Supersedes,
    });
    let outcome = build_observation_records(&req).expect("supersede write should build");

    let edges: Vec<&GraphRecord> = outcome
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::Supersedes,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(edges.len(), 1, "exactly one SUPERSEDES edge expected");
    let GraphRecord::Edge {
        id,
        source,
        target: edge_target,
        ..
    } = edges[0]
    else {
        unreachable!()
    };
    assert_eq!(source, &outcome.record_id);
    assert_eq!(edge_target, &target);
    assert!(
        id.starts_with("agent_memory:v1:"),
        "edge id must live in the agent_memory domain, got {id}"
    );
}

#[test]
fn contradict_write_authors_contradicts_edge_with_confidence() {
    let target = prior_observation_id();
    let mut req = observation_request("actually blocking IO is fine here");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Contradicts,
    });
    let outcome = build_observation_records(&req).expect("contradict write should build");

    let edges: Vec<&GraphRecord> = outcome
        .records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Edge {
                    label: EdgeLabel::Contradicts,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(edges.len(), 1, "exactly one CONTRADICTS edge expected");
    let GraphRecord::Edge {
        source,
        target: edge_target,
        confidence,
        ..
    } = edges[0]
    else {
        unreachable!()
    };
    assert_eq!(source, &outcome.record_id);
    assert_eq!(edge_target, &target);
    // Schema registry: CONTRADICTS requires confidence.
    assert_eq!(confidence.as_deref(), Some("0.9"));
}

#[test]
fn supersede_write_keeps_write_time_invariants() {
    let target = prior_observation_id();
    let mut req = observation_request("newer correction");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target,
        label: EdgeLabel::Supersedes,
    });
    let outcome = build_observation_records(&req).expect("supersede write should build");
    let obs = outcome
        .records
        .iter()
        .find(|r| r.id() == outcome.record_id)
        .expect("observation node present");
    let GraphRecord::Node {
        evidence_links,
        text,
        confidence,
        ..
    } = obs
    else {
        panic!("observation should be a node")
    };
    assert!(
        evidence_links.as_ref().is_some_and(|l| !l.is_empty()),
        "evidence_links still required"
    );
    assert!(text.is_some(), "text still required");
    assert_eq!(confidence.as_deref(), Some("0.9"));
}

#[test]
fn supersede_write_does_not_mutate_prior_record() {
    let prior = prior_observation();
    let target = prior.record_id;
    let mut req = observation_request("correction");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Supersedes,
    });
    let outcome = build_observation_records(&req).expect("supersede write should build");
    // No emitted record may carry the prior record's ID: the write is purely
    // additive; provenance fields are never rewritten in place.
    assert!(
        outcome.records.iter().all(|r| r.id() != target),
        "write batch must not contain the prior record's ID"
    );
}

#[test]
fn supersede_write_rejects_unsupported_label() {
    let mut req = observation_request("correction");
    req.supersession = Some(SupersessionTarget {
        target_record_id: prior_observation_id(),
        label: EdgeLabel::Observes,
    });
    let err = build_observation_records(&req).expect_err("unsupported label must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "supersession");
}

#[test]
fn supersede_write_rejects_empty_target() {
    let mut req = observation_request("correction");
    req.supersession = Some(SupersessionTarget {
        target_record_id: String::new(),
        label: EdgeLabel::Supersedes,
    });
    let err = build_observation_records(&req).expect_err("empty target must be rejected");
    assert_eq!(err.code, "missing_field");
}

#[test]
fn supersession_edge_is_deterministic() {
    let edge_a = build_supersession_edge(
        "agent_memory:v1:aaa",
        &SupersessionTarget {
            target_record_id: "agent_memory:v1:bbb".to_owned(),
            label: EdgeLabel::Supersedes,
        },
        "0.9",
    );
    let edge_b = build_supersession_edge(
        "agent_memory:v1:aaa",
        &SupersessionTarget {
            target_record_id: "agent_memory:v1:bbb".to_owned(),
            label: EdgeLabel::Supersedes,
        },
        "0.9",
    );
    assert_eq!(edge_a.id(), edge_b.id());
}

// ── Pure target validation ──────────────────────────────────────────────────

#[test]
fn validate_accepts_observation_target() {
    let target = prior_observation_id();
    let records: Vec<GraphRecord> = prior_observation()
        .records
        .into_iter()
        .filter(|r| r.id() == target)
        .collect();
    let kind = validate_supersession_target(&records, &target)
        .expect("observation target should validate");
    assert_eq!(kind, NodeKind::Observation);
}

#[test]
fn validate_rejects_unknown_target() {
    let err = validate_supersession_target(&[], "agent_memory:v1:nope")
        .expect_err("unknown target must be rejected");
    assert!(matches!(err, SupersessionError::UnknownTarget { .. }));
    assert_eq!(err.code(), "unknown_supersession_target");
    assert_eq!(err.exit_code(), 2);
    // The diagnostic never echoes payload values — only the handle.
    let json = err.to_json("supersedes").to_string();
    assert!(!json.contains("blocking IO"));
}

#[test]
fn validate_rejects_codegraph_fact_target() {
    let symbol = node_record("codegraph:v1:symbol1", "Symbol");
    let err = validate_supersession_target(&[symbol], "codegraph:v1:symbol1")
        .expect_err("codegraph fact must be rejected");
    assert!(matches!(err, SupersessionError::CodegraphFact { .. }));
    assert_eq!(err.code(), "supersede_codegraph_fact");
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn validate_rejects_non_observation_class_target() {
    let agent = node_record("agent_memory:v1:agent1", "Agent");
    let err = validate_supersession_target(&[agent], "agent_memory:v1:agent1")
        .expect_err("non-observation-class target must be rejected");
    assert!(matches!(
        err,
        SupersessionError::NonObservationTarget { .. }
    ));
    assert_eq!(err.code(), "supersede_non_observation_target");
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn validate_rejects_codegraph_edge_target() {
    let edge: GraphRecord = serde_json::from_value(serde_json::json!({
        "record_type": "edge",
        "id": "codegraph:v1:edge1",
        "schema_version": 1,
        "label": "CALLS",
        "source": "codegraph:v1:s1",
        "target": "codegraph:v1:s2",
        "summary": "call edge",
    }))
    .expect("edge should deserialize");
    let err = validate_supersession_target(&[edge], "codegraph:v1:edge1")
        .expect_err("codegraph edge target must be rejected");
    assert_eq!(err.code(), "supersede_codegraph_fact");
}

// ── Idempotency conflict ────────────────────────────────────────────────────

#[test]
fn identical_rerun_is_not_a_conflict() {
    let prior = prior_observation();
    let target = prior.record_id.clone();
    let mut req = observation_request("correction");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Supersedes,
    });
    let outcome = build_observation_records(&req).expect("write should build");
    let mut records = prior.records;
    records.extend(outcome.records.iter().cloned());
    check_supersession_conflict(&records, &outcome.record_id, EdgeLabel::Supersedes, &target)
        .expect("identical re-run must not conflict");
}

#[test]
fn same_key_different_target_is_a_conflict() {
    let prior = prior_observation();
    let target = prior.record_id.clone();
    let mut req = observation_request("correction");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Supersedes,
    });
    let outcome = build_observation_records(&req).expect("write should build");
    let mut records = prior.records;
    records.extend(outcome.records.iter().cloned());
    // A genuinely different observation: different text → different ID.
    let other = build_observation_records(&observation_request("a different prior note"))
        .expect("other observation should build")
        .record_id;
    assert_ne!(other, target, "test setup: targets must differ");
    let err =
        check_supersession_conflict(&records, &outcome.record_id, EdgeLabel::Supersedes, &other)
            .expect_err("different target for the same key must conflict");
    assert!(matches!(err, SupersessionError::Conflict { .. }));
    assert_eq!(err.code(), "supersession_conflict");
    assert_eq!(err.exit_code(), 1);
}

// ── Determinism ─────────────────────────────────────────────────────────────

#[test]
fn five_identical_supersede_writes_are_byte_identical() {
    let target = prior_observation_id();
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let mut req = observation_request("blocking IO deadlocked, use async");
        req.supersession = Some(SupersessionTarget {
            target_record_id: target.clone(),
            label: EdgeLabel::Supersedes,
        });
        let outcome = build_observation_records(&req).expect("write should build");
        let batch: Vec<String> = outcome
            .records
            .iter()
            .map(|r| serde_json::to_string(r).expect("record serializes"))
            .collect();
        outputs.push((outcome.record_id.clone(), batch));
    }
    for (id, batch) in &outputs[1..] {
        assert_eq!(id, &outputs[0].0, "record IDs must be byte-identical");
        assert_eq!(
            batch, &outputs[0].1,
            "record batches must be byte-identical"
        );
    }
}

// ── Read-side consumption ───────────────────────────────────────────────────

#[test]
fn temporal_resolver_flags_superseded_with_forward_handle() {
    let prior = prior_observation();
    let target = prior.record_id.clone();
    let mut req = observation_request("blocking IO deadlocked, use async");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Supersedes,
    });
    let outcome = build_observation_records(&req).expect("write should build");

    let mut records = prior.records;
    records.extend(outcome.records.iter().cloned());
    let resolver = TemporalResolver::build(&records);
    let (status, _, _) = resolver.resolve_status(&target);
    assert_eq!(status, "superseded");
    let audit = memory_audit_context(&records, &target, false);
    assert!(
        audit
            .superseding_records
            .iter()
            .any(|item| item.record.id() == outcome.record_id),
        "audit must surface the new observation as the forward handle"
    );
}

#[test]
fn temporal_resolver_flags_contradicted_mutually() {
    let prior = prior_observation();
    let target = prior.record_id.clone();
    let mut req = observation_request("actually blocking IO is fine");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Contradicts,
    });
    let outcome = build_observation_records(&req).expect("write should build");

    let mut records = prior.records;
    records.extend(outcome.records.iter().cloned());
    let resolver = TemporalResolver::build(&records);
    let (status, _, _) = resolver.resolve_status(&target);
    assert_eq!(status, "contradicted");
    // The dispute is mutual, not a winner declaration: the new record is also
    // reachable and flagged.
    let (new_status, _, _) = resolver.resolve_status(&outcome.record_id);
    assert_eq!(new_status, "contradicted");
    let audit = memory_audit_context(&records, &target, false);
    assert!(
        audit
            .contradicting_evidence
            .iter()
            .any(|item| item.record.id() == outcome.record_id),
        "audit must list the new observation as contradicting evidence"
    );
}

#[test]
fn prior_record_stays_explicitly_retrievable_inside_its_window() {
    let prior = prior_observation();
    let target = prior.record_id.clone();
    let mut req = observation_request("blocking IO deadlocked, use async");
    req.supersession = Some(SupersessionTarget {
        target_record_id: target.clone(),
        label: EdgeLabel::Supersedes,
    });
    let outcome = build_observation_records(&req).expect("write should build");

    let mut records = prior.records;
    records.extend(outcome.records.iter().cloned());
    let audit = memory_audit_context(&records, &target, false);
    // The prior claim is still present and addressed directly, inside its
    // original window.
    assert_eq!(audit.memory_id, target.as_str());
    let prior_node = records
        .iter()
        .find(|r| r.id() == target)
        .expect("prior record still present");
    let GraphRecord::Node {
        agent_id,
        observed_at,
        source_handle,
        ..
    } = prior_node
    else {
        panic!("prior record should be a node")
    };
    assert_eq!(agent_id.as_deref(), Some("agent-1"));
    assert_eq!(observed_at.as_deref(), Some("2026-09-26T12:00:00Z"));
    assert_eq!(source_handle.as_deref(), Some("src/lib.rs:sha256:abc123"));
    // And the read side flags it as superseded rather than silently current.
    assert!(
        !audit.superseding_records.is_empty(),
        "audit should flag the supersession"
    );
}

// ── CLI: accepted write ─────────────────────────────────────────────────────

fn write_prior_observation(dir: &std::path::Path) -> (String, std::path::PathBuf) {
    let prior_out = dir.join("prior.jsonl");
    let mut cmd = Command::cargo_bin("eg").expect("eg binary should build");
    let assert = cmd
        .arg("write")
        .arg("observation")
        .arg("--agent-id")
        .arg("agent-1")
        .arg("--agent-kind")
        .arg("other")
        .arg("--session-id")
        .arg("sess-1")
        .arg("--observed-at")
        .arg("2026-09-26T12:00:00Z")
        .arg("--source-handle")
        .arg("src/lib.rs:sha256:abc123")
        .arg("--text")
        .arg("blocking IO here")
        .arg("--confidence")
        .arg("0.9")
        .arg("--evidence-target")
        .arg("codegraph:v1:deadbeef")
        .arg("--out")
        .arg(&prior_out)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let handle: String = stdout
        .trim()
        .strip_prefix(r#"{"ok":true,"evidence_handle":""#)
        .and_then(|rest| rest.split('"').next())
        .expect("stdout should carry the evidence handle")
        .to_owned();
    (handle, prior_out)
}

fn base_write_args(cmd: &mut Command, dir: &std::path::Path, out: &std::path::Path) {
    cmd.arg("write")
        .arg("observation")
        .arg("--agent-id")
        .arg("agent-1")
        .arg("--agent-kind")
        .arg("other")
        .arg("--session-id")
        .arg("sess-1")
        .arg("--observed-at")
        .arg("2026-09-26T12:00:00Z")
        .arg("--source-handle")
        .arg("src/lib.rs:sha256:abc123")
        .arg("--text")
        .arg("blocking IO deadlocked, use async")
        .arg("--confidence")
        .arg("0.9")
        .arg("--evidence-target")
        .arg("codegraph:v1:deadbeef")
        .arg("--out")
        .arg(out);
    let _ = dir;
}

#[test]
fn cli_supersede_write_authors_edge_and_leaves_seed_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (prior_handle, prior_out) = write_prior_observation(dir.path());
    let seed_bytes = fs::read(&prior_out).expect("seed should be readable");

    let new_out = dir.path().join("new.jsonl");
    let mut cmd = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut cmd, dir.path(), &new_out);
    let assert = cmd
        .arg("--supersedes")
        .arg(&prior_handle)
        .arg("--graph")
        .arg(&prior_out)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("\"supersession_edge\""),
        "stdout should carry the edge handle, got: {stdout}"
    );

    // The seed file is never rewritten by the correction workflow.
    assert_eq!(
        fs::read(&prior_out).expect("seed should be readable"),
        seed_bytes,
        "seed JSONL must be byte-identical after the supersede write"
    );

    // The new batch carries exactly one SUPERSEDES edge from the new
    // observation to the prior one, and no record with the prior's ID.
    let new_jsonl = fs::read_to_string(&new_out).expect("new batch should be readable");
    let mut edge_id = None;
    let mut new_handle = None;
    for line in new_jsonl.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSONL");
        if v["record_type"] == "edge" && v["label"] == "SUPERSEDES" {
            assert_eq!(v["target"], prior_handle);
            edge_id = Some(v["id"].as_str().expect("edge id").to_owned());
            new_handle = Some(v["source"].as_str().expect("edge source").to_owned());
        }
        assert_ne!(
            v["id"].as_str().unwrap_or_default(),
            prior_handle,
            "no emitted record may reuse the prior record's ID"
        );
    }
    let edge_id = edge_id.expect("exactly one SUPERSEDES edge expected");
    assert!(
        edge_id.starts_with("agent_memory:v1:"),
        "edge id must live in the agent_memory domain"
    );
    assert!(
        stdout.contains(&edge_id),
        "stdout edge handle must match the emitted edge"
    );

    // Read-back: the prior record is flagged superseded and the audit
    // supplies the new observation as the forward handle.
    let mut combined = seed_bytes;
    combined.extend_from_slice(new_jsonl.as_bytes());
    let combined_path = dir.path().join("combined.jsonl");
    fs::write(&combined_path, &combined).expect("combined should write");
    let mut qcmd = Command::cargo_bin("eg").expect("eg binary should build");
    let qassert = qcmd
        .arg("query")
        .arg("memory")
        .arg(&prior_handle)
        .arg("--graph")
        .arg(&combined_path)
        .assert()
        .success();
    let qstdout = String::from_utf8_lossy(&qassert.get_output().stdout).to_string();
    let new_handle = new_handle.expect("edge source");
    assert!(
        qstdout.contains(&new_handle),
        "audit should surface the new observation as the forward handle"
    );
    assert!(
        qstdout.contains("SUPERSEDES"),
        "audit should name the SUPERSEDES relation"
    );
}

#[test]
fn cli_contradict_write_and_read_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (prior_handle, prior_out) = write_prior_observation(dir.path());

    let new_out = dir.path().join("contra.jsonl");
    let mut cmd = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut cmd, dir.path(), &new_out);
    cmd.arg("--contradicts")
        .arg(&prior_handle)
        .arg("--graph")
        .arg(&prior_out)
        .assert()
        .success();

    let new_jsonl = fs::read_to_string(&new_out).expect("new batch should be readable");
    let edge_line = new_jsonl
        .lines()
        .find(|l| l.contains("\"CONTRADICTS\""))
        .expect("one CONTRADICTS edge expected");
    let edge: serde_json::Value = serde_json::from_str(edge_line).expect("valid JSONL");
    assert_eq!(edge["target"], prior_handle);
    assert!(
        edge["confidence"].is_string(),
        "CONTRADICTS requires confidence"
    );
}

// ── CLI: rejections ────────────────────────────────────────────────────────

#[test]
fn cli_rejects_missing_target_with_exit_2_and_no_payload_echo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_prior_handle, prior_out) = write_prior_observation(dir.path());

    let new_out = dir.path().join("new.jsonl");
    let mut cmd = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut cmd, dir.path(), &new_out);
    let assert = cmd
        .arg("--supersedes")
        .arg("agent_memory:v1:doesnotexist")
        .arg("--graph")
        .arg(&prior_out)
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("unknown_supersession_target"),
        "stderr should carry the stable code, got: {stderr}"
    );
    assert!(
        !stderr.contains("blocking IO"),
        "diagnostics must never echo observation text"
    );
    assert!(
        !new_out.exists(),
        "a rejected write must not produce an output batch"
    );
}

#[test]
fn cli_rejects_codegraph_fact_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Seed a deterministic code-graph Symbol alongside the observation.
    let seed_path = dir.path().join("seed.jsonl");
    fs::write(
        &seed_path,
        "{\"record_type\":\"node\",\"id\":\"codegraph:v1:symbol1\",\"kind\":\"Symbol\",\"schema_version\":1,\"summary\":\"probe symbol\"}\n",
    )
    .expect("seed should write");

    let new_out = dir.path().join("new.jsonl");
    let mut cmd = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut cmd, dir.path(), &new_out);
    let assert = cmd
        .arg("--supersedes")
        .arg("codegraph:v1:symbol1")
        .arg("--graph")
        .arg(&seed_path)
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("supersede_codegraph_fact"),
        "stderr should carry the stable code, got: {stderr}"
    );
}

#[test]
fn cli_rejects_non_observation_class_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    // An Agent node is agent-memory but not observation-class.
    let seed_path = dir.path().join("seed.jsonl");
    fs::write(
        &seed_path,
        "{\"record_type\":\"node\",\"id\":\"agent_memory:v1:agentx\",\"kind\":\"Agent\",\"schema_version\":1,\"summary\":\"an agent\"}\n",
    )
    .expect("seed should write");

    let new_out = dir.path().join("new.jsonl");
    let mut cmd = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut cmd, dir.path(), &new_out);
    let assert = cmd
        .arg("--supersedes")
        .arg("agent_memory:v1:agentx")
        .arg("--graph")
        .arg(&seed_path)
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("supersede_non_observation_target"),
        "stderr should carry the stable code, got: {stderr}"
    );
}

#[test]
fn cli_rejects_supersede_without_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let new_out = dir.path().join("new.jsonl");
    let mut cmd = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut cmd, dir.path(), &new_out);
    let assert = cmd
        .arg("--supersedes")
        .arg("agent_memory:v1:anything")
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("supersession_store_required"),
        "stderr should carry the stable code, got: {stderr}"
    );
}

// ── CLI: idempotency ────────────────────────────────────────────────────────

#[test]
fn cli_identical_rerun_returns_identical_batch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (prior_handle, prior_out) = write_prior_observation(dir.path());

    let first_out = dir.path().join("first.jsonl");
    let mut first = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut first, dir.path(), &first_out);
    first
        .arg("--supersedes")
        .arg(&prior_handle)
        .arg("--graph")
        .arg(&prior_out)
        .assert()
        .success();

    // The identical re-run validates against a store that already contains
    // the first write's edge: same key, same target → no conflict.
    let mut store = fs::read(&prior_out).expect("seed");
    store.extend_from_slice(&fs::read(&first_out).expect("first batch"));
    let store_path = dir.path().join("store.jsonl");
    fs::write(&store_path, &store).expect("store should write");

    let second_out = dir.path().join("second.jsonl");
    let mut second = Command::cargo_bin("eg").expect("eg binary should build");
    base_write_args(&mut second, dir.path(), &second_out);
    second
        .arg("--supersedes")
        .arg(&prior_handle)
        .arg("--graph")
        .arg(&store_path)
        .assert()
        .success();

    assert_eq!(
        fs::read(&first_out).expect("first batch"),
        fs::read(&second_out).expect("second batch"),
        "identical re-runs must be byte-identical"
    );
}
/// The `--data-dir` lane validates the target against a real embedded store
/// (issue #184). Seeds a store with a prior observation, then supersedes it
/// through the CLI pointing at the store directory.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn cli_write_supersede_with_data_dir() {
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("store");
    let prior = prior_observation();
    let target = prior.record_id.clone();

    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("store should open");
    for record in &prior.records {
        sink.write_record(record).expect("record should write");
    }
    sink.persist_indexes().expect("indexes should persist");
    drop(sink);

    let out = temp.path().join("corrected.jsonl");
    let assert = Command::cargo_bin("eg")
        .expect("eg binary should build")
        .arg("write")
        .arg("observation")
        .arg("--agent-id")
        .arg("cli-agent")
        .arg("--session-id")
        .arg("sess-1")
        .arg("--observed-at")
        .arg("2026-09-26T11:00:00Z")
        .arg("--source-handle")
        .arg("notes.md")
        .arg("--text")
        .arg("async all the way down")
        .arg("--evidence-target")
        .arg("codegraph:v4:serve")
        .arg("--supersedes")
        .arg(&target)
        .arg("--data-dir")
        .arg(data_dir.to_str().expect("path should be utf8"))
        .arg("--out")
        .arg(out.to_str().expect("path should be utf8"))
        .assert()
        .success();
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must be JSON");
    assert_eq!(parsed["ok"], true);
    assert!(
        parsed["supersession_edge"].as_str().is_some(),
        "success envelope must surface the edge handle"
    );

    let batch = fs::read_to_string(&out).expect("batch should be readable");
    let has_edge = batch.lines().any(|line| {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSONL");
        v["record_type"] == "edge"
            && v["label"] == "SUPERSEDES"
            && v["target"] == target.as_str()
            && v["source"]
                .as_str()
                .is_some_and(|s| s.starts_with("agent_memory:v1:"))
    });
    assert!(has_edge, "batch must carry the SUPERSEDES edge");
}
