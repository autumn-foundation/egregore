//! TDD tests for typed evidence write workflows (issue #44).
//!
//! RED → GREEN → REFACTOR:
//! These tests drive the `evidence` module implementation.

#![allow(missing_docs)]

use aletheia_egregore::{
    evidence::{
        ArtifactRequest, CommandEvidenceRequest, EvidenceProvenance, ObservationRequest,
        VerificationRequest, build_artifact_records, build_command_evidence_records,
        build_observation_records, build_verification_records,
    },
    ir::{Domain, EdgeLabel, EvidenceLink, NodeKind},
};

// ── Shared fixture helpers ──────────────────────────────────────────────────

fn valid_provenance() -> EvidenceProvenance {
    EvidenceProvenance {
        agent_id: "agent-test-001".to_owned(),
        agent_kind: "other".to_owned(),
        session_id: "session-test-001".to_owned(),
        observed_at: "2026-05-30T10:00:00Z".to_owned(),
        source_handle: Some("tests/fixtures/rust_basic/src/lib.rs:abc123".to_owned()),
    }
}

fn dummy_evidence_link(target_id: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target_id.to_owned()),
        target_domain: Domain::CodeGraph.as_str().to_owned(),
        relation: EdgeLabel::Observes.as_str().to_owned(),
        confidence: "0.9".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

// ── AC2: Provenance validation — missing fields produce machine-readable errors ─

#[test]
fn observation_rejects_empty_agent_id() {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            agent_id: String::new(),
            ..valid_provenance()
        },
        text: "test observation".to_owned(),
        confidence: 0.9,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abc123")],
    };
    let err = build_observation_records(&req).expect_err("empty agent_id must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "agent_id");
    // Error message must not echo the payload
    let msg = err.to_string();
    assert!(
        !msg.contains("test observation"),
        "error must not echo payload text"
    );
}

#[test]
fn observation_rejects_empty_session_id() {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            session_id: String::new(),
            ..valid_provenance()
        },
        text: "session check".to_owned(),
        confidence: 0.8,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abc123")],
    };
    let err = build_observation_records(&req).expect_err("empty session_id must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "session_id");
    let msg = err.to_string();
    assert!(
        !msg.contains("session check"),
        "error must not echo payload text"
    );
}

#[test]
fn observation_rejects_empty_observed_at() {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            observed_at: String::new(),
            ..valid_provenance()
        },
        text: "time check".to_owned(),
        confidence: 0.7,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abc123")],
    };
    let err = build_observation_records(&req).expect_err("empty observed_at must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "observed_at");
}

#[test]
fn observation_rejects_missing_source_handle() {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            source_handle: None,
            ..valid_provenance()
        },
        text: "source check".to_owned(),
        confidence: 0.6,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abc123")],
    };
    let err = build_observation_records(&req).expect_err("missing source_handle must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "source_handle");
    let msg = err.to_string();
    assert!(
        !msg.contains("source check"),
        "error must not echo payload text"
    );
}

#[test]
fn observation_rejects_empty_text() {
    let req = ObservationRequest {
        provenance: valid_provenance(),
        text: String::new(),
        confidence: 0.9,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abc123")],
    };
    let err = build_observation_records(&req).expect_err("empty text must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "text");
}

#[test]
fn observation_rejects_empty_evidence_links() {
    let req = ObservationRequest {
        provenance: valid_provenance(),
        text: "valid observation".to_owned(),
        confidence: 0.9,
        evidence_links: vec![],
    };
    let err = build_observation_records(&req).expect_err("empty evidence_links must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "evidence_links");
}

// ── AC1 + AC3: Accepted writes produce correct domain-typed records ──────────

#[test]
fn accepted_observation_produces_observation_node() {
    let req = ObservationRequest {
        provenance: valid_provenance(),
        text: "the function has high cyclomatic complexity".to_owned(),
        confidence: 0.9,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:deadbeef")],
    };
    let outcome = build_observation_records(&req).expect("valid observation must succeed");

    // Primary record is an Observation node in agent_memory domain
    assert!(
        outcome.record_id.starts_with("agent_memory:v1:"),
        "observation record_id must use agent_memory:v1: prefix, got {}",
        outcome.record_id
    );
    assert_eq!(
        outcome.evidence_handle, outcome.record_id,
        "evidence_handle must equal record_id"
    );

    // Must include Agent, AgentSession, and Observation nodes
    let node_kinds: Vec<NodeKind> = outcome
        .records
        .iter()
        .filter_map(|r| {
            if let aletheia_egregore::ir::GraphRecord::Node { kind, .. } = r {
                Some(*kind)
            } else {
                None
            }
        })
        .collect();

    assert!(
        node_kinds.contains(&NodeKind::Agent),
        "records must include Agent node"
    );
    assert!(
        node_kinds.contains(&NodeKind::AgentSession),
        "records must include AgentSession node"
    );
    assert!(
        node_kinds.contains(&NodeKind::Observation),
        "records must include Observation node"
    );
}

#[test]
fn accepted_observation_carries_required_provenance() {
    let prov = valid_provenance();
    let req = ObservationRequest {
        provenance: prov,
        text: "provenance check".to_owned(),
        confidence: 0.85,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:feed1234")],
    };
    let outcome = build_observation_records(&req).expect("valid observation must succeed");

    let obs_record = outcome
        .records
        .iter()
        .find(|r| {
            matches!(
                r,
                aletheia_egregore::ir::GraphRecord::Node {
                    kind: NodeKind::Observation,
                    ..
                }
            )
        })
        .expect("observation node must be present");

    if let aletheia_egregore::ir::GraphRecord::Node {
        agent_id,
        session_id,
        observed_at,
        source_handle,
        evidence_links,
        confidence,
        text,
        domain,
        ..
    } = obs_record
    {
        assert_eq!(
            agent_id.as_deref(),
            Some("agent-test-001"),
            "agent_id must match"
        );
        assert_eq!(
            session_id.as_deref(),
            Some("session-test-001"),
            "session_id must match"
        );
        assert_eq!(
            observed_at.as_deref(),
            Some("2026-05-30T10:00:00Z"),
            "observed_at must match"
        );
        assert!(source_handle.is_some(), "source_handle must be present");
        assert!(
            evidence_links.as_ref().is_some_and(|l| !l.is_empty()),
            "evidence_links must be present"
        );
        assert!(confidence.is_some(), "confidence must be present");
        assert!(text.is_some(), "text must be present");
        assert_eq!(
            domain.as_deref(),
            Some("agent_memory"),
            "domain must be agent_memory"
        );
    } else {
        panic!("expected Node record");
    }
}

#[test]
fn accepted_command_evidence_produces_command_run_node() {
    let req = CommandEvidenceRequest {
        provenance: valid_provenance(),
        executed_at: "2026-05-30T10:01:00Z".to_owned(),
        exit_code: 0,
        stdout: Some("test output\n".to_owned()),
        stderr: None,
        evidence_quality: "verbatim".to_owned(),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:abc123".to_owned(),
    };
    let outcome =
        build_command_evidence_records(&req).expect("valid command evidence must succeed");

    assert!(
        outcome.record_id.starts_with("verification:v1:"),
        "command evidence record_id must use verification:v1: prefix, got {}",
        outcome.record_id
    );

    let has_command_run = outcome.records.iter().any(|r| {
        matches!(
            r,
            aletheia_egregore::ir::GraphRecord::Node {
                kind: NodeKind::CommandRun,
                ..
            }
        )
    });
    assert!(has_command_run, "records must include CommandRun node");
}

#[test]
fn accepted_command_evidence_carries_required_provenance() {
    let req = CommandEvidenceRequest {
        provenance: valid_provenance(),
        executed_at: "2026-05-30T10:01:00Z".to_owned(),
        exit_code: 1,
        stdout: None,
        stderr: Some("error output".to_owned()),
        evidence_quality: "summarized".to_owned(),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:def456".to_owned(),
    };
    let outcome =
        build_command_evidence_records(&req).expect("valid command evidence must succeed");

    let cmd_record = outcome
        .records
        .iter()
        .find(|r| {
            matches!(
                r,
                aletheia_egregore::ir::GraphRecord::Node {
                    kind: NodeKind::CommandRun,
                    ..
                }
            )
        })
        .expect("CommandRun node must be present");

    if let aletheia_egregore::ir::GraphRecord::Node {
        agent_id,
        session_id,
        source_artifact_path,
        source_artifact_hash,
        executed_at,
        exit_code,
        ..
    } = cmd_record
    {
        assert_eq!(
            agent_id.as_deref(),
            Some("agent-test-001"),
            "agent_id must match"
        );
        assert_eq!(
            session_id.as_deref(),
            Some("session-test-001"),
            "session_id must match"
        );
        assert!(
            source_artifact_path.is_some(),
            "source_artifact_path must be present"
        );
        assert!(
            source_artifact_hash.is_some(),
            "source_artifact_hash must be present"
        );
        assert!(executed_at.is_some(), "executed_at must be present");
        assert_eq!(*exit_code, Some(1i64), "exit_code must match");
    } else {
        panic!("expected Node record");
    }
}

#[test]
fn command_evidence_rejects_missing_source_artifact() {
    let req = CommandEvidenceRequest {
        provenance: EvidenceProvenance {
            source_handle: None,
            ..valid_provenance()
        },
        executed_at: "2026-05-30T10:01:00Z".to_owned(),
        exit_code: 0,
        stdout: None,
        stderr: None,
        evidence_quality: "verbatim".to_owned(),
        source_artifact_path: String::new(),
        source_artifact_hash: String::new(),
    };
    let err =
        build_command_evidence_records(&req).expect_err("missing source artifact must be rejected");
    assert_eq!(err.code, "missing_field");
    // Either source_artifact_path or source_artifact_hash must be named
    assert!(
        err.field.contains("source_artifact"),
        "error field must name source_artifact field, got: {}",
        err.field
    );
}

#[test]
fn accepted_artifact_produces_patch_artifact_node() {
    let req = ArtifactRequest {
        provenance: valid_provenance(),
        patch_bytes: b"--- a/src/lib.rs\n+++ b/src/lib.rs\n".to_vec(),
        target_files: vec!["src/lib.rs".to_owned()],
        patch_status: "unverified".to_owned(),
        base_commit: Some("abc123def456".to_owned()),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:abc123".to_owned(),
    };
    let outcome = build_artifact_records(&req).expect("valid artifact must succeed");

    assert!(
        outcome.record_id.starts_with("artifact:v1:"),
        "artifact record_id must use artifact:v1: prefix, got {}",
        outcome.record_id
    );

    let has_patch_artifact = outcome.records.iter().any(|r| {
        matches!(
            r,
            aletheia_egregore::ir::GraphRecord::Node {
                kind: NodeKind::PatchArtifact,
                ..
            }
        )
    });
    assert!(
        has_patch_artifact,
        "records must include PatchArtifact node"
    );
}

#[test]
fn accepted_verification_produces_verification_node() {
    let req = VerificationRequest {
        provenance: valid_provenance(),
        executed_at: "2026-05-30T10:02:00Z".to_owned(),
        status: "pass".to_owned(),
        verification_kind: "test_run".to_owned(),
        stdout: Some("test ok - 5 passed, 0 failed".to_owned()),
        evidence_quality: "verbatim".to_owned(),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:abc123".to_owned(),
        linked_command_evidence_id: None,
    };
    let outcome = build_verification_records(&req).expect("valid verification must succeed");

    assert!(
        outcome.record_id.starts_with("verification:v1:"),
        "verification record_id must use verification:v1: prefix, got {}",
        outcome.record_id
    );

    let has_verification = outcome.records.iter().any(|r| {
        matches!(
            r,
            aletheia_egregore::ir::GraphRecord::Node {
                kind: NodeKind::Verification,
                ..
            }
        )
    });
    assert!(has_verification, "records must include Verification node");
}

// ── AC3: Separation — observations are never auto-verified ──────────────────

#[test]
fn observation_without_verification_link_is_not_marked_verified() {
    let req = ObservationRequest {
        provenance: valid_provenance(),
        text: "unverified claim about complexity".to_owned(),
        confidence: 0.7,
        // Evidence link points to a codegraph entity (code fact), not a verification record
        evidence_links: vec![dummy_evidence_link("codegraph:v4:deadbeef")],
    };
    let outcome = build_observation_records(&req).expect("valid observation must succeed");

    let obs_record = outcome
        .records
        .iter()
        .find(|r| {
            matches!(
                r,
                aletheia_egregore::ir::GraphRecord::Node {
                    kind: NodeKind::Observation,
                    ..
                }
            )
        })
        .expect("observation node must be present");

    // The Observation node must not have a VALIDATED_BY edge target
    if let aletheia_egregore::ir::GraphRecord::Node { evidence_links, .. } = obs_record {
        let has_validated_by = evidence_links.as_ref().is_some_and(|links| {
            links
                .iter()
                .any(|l| l.relation == EdgeLabel::ValidatedBy.as_str())
        });
        assert!(
            !has_validated_by,
            "observation without verification evidence must not carry VALIDATED_BY link"
        );
    }

    // Must not include any edge with VALIDATED_BY label
    let has_validated_by_edge = outcome.records.iter().any(|r| {
        matches!(
            r,
            aletheia_egregore::ir::GraphRecord::Edge {
                label: EdgeLabel::ValidatedBy,
                ..
            }
        )
    });
    assert!(
        !has_validated_by_edge,
        "records must not include VALIDATED_BY edge for unverified observation"
    );
}

#[test]
fn observation_with_verification_link_carries_validated_by() {
    let req = ObservationRequest {
        provenance: valid_provenance(),
        text: "function passes all tests".to_owned(),
        confidence: 0.95,
        evidence_links: vec![
            // Cross-domain link to code graph
            dummy_evidence_link("codegraph:v4:deadbeef"),
            // Link to verification evidence
            EvidenceLink {
                target_record_id: Some("verification:v1:abc123".to_owned()),
                target_domain: Domain::Verification.as_str().to_owned(),
                relation: EdgeLabel::ValidatedBy.as_str().to_owned(),
                confidence: "0.95".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            },
        ],
    };
    let outcome =
        build_observation_records(&req).expect("valid observation with verification must succeed");

    let obs_record = outcome
        .records
        .iter()
        .find(|r| {
            matches!(
                r,
                aletheia_egregore::ir::GraphRecord::Node {
                    kind: NodeKind::Observation,
                    ..
                }
            )
        })
        .expect("observation node must be present");

    if let aletheia_egregore::ir::GraphRecord::Node { evidence_links, .. } = obs_record {
        let has_validated_by = evidence_links.as_ref().is_some_and(|links| {
            links
                .iter()
                .any(|l| l.relation == EdgeLabel::ValidatedBy.as_str())
        });
        assert!(
            has_validated_by,
            "observation with verification evidence must carry VALIDATED_BY link"
        );
    }
}

// ── AC8: Determinism — five identical runs produce same IDs ─────────────────

#[test]
fn five_identical_observation_writes_produce_same_record_ids() {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            agent_id: "agent-determinism-001".to_owned(),
            agent_kind: "other".to_owned(),
            session_id: "session-determinism-001".to_owned(),
            observed_at: "2026-05-30T12:00:00Z".to_owned(),
            source_handle: Some("src/lib.rs:sha256:deadbeef".to_owned()),
        },
        text: "determinism test observation".to_owned(),
        confidence: 0.9,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:feed5678")],
    };

    let run0 = build_observation_records(&req).expect("run 0 must succeed");
    let run1 = build_observation_records(&req).expect("run 1 must succeed");
    let run2 = build_observation_records(&req).expect("run 2 must succeed");
    let run3 = build_observation_records(&req).expect("run 3 must succeed");
    let run4 = build_observation_records(&req).expect("run 4 must succeed");

    let ids: Vec<_> = [&run0, &run1, &run2, &run3, &run4]
        .iter()
        .map(|o| o.record_id.clone())
        .collect();
    let first = &ids[0];
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(id, first, "run {i} record_id differs from run 0");
    }

    // Also verify that record counts and link labels are identical
    let counts: Vec<usize> = [&run0, &run1, &run2, &run3, &run4]
        .iter()
        .map(|o| o.records.len())
        .collect();
    let first_count = counts[0];
    for (i, count) in counts.iter().enumerate() {
        assert_eq!(
            *count, first_count,
            "run {i} record count differs from run 0"
        );
    }
}

#[test]
fn five_identical_command_evidence_writes_produce_same_record_ids() {
    let req = CommandEvidenceRequest {
        provenance: EvidenceProvenance {
            agent_id: "agent-cmd-001".to_owned(),
            agent_kind: "other".to_owned(),
            session_id: "session-cmd-001".to_owned(),
            observed_at: "2026-05-30T12:00:00Z".to_owned(),
            source_handle: Some("src/lib.rs:sha256:deadbeef".to_owned()),
        },
        executed_at: "2026-05-30T12:01:00Z".to_owned(),
        exit_code: 0,
        stdout: Some("all tests passed".to_owned()),
        stderr: None,
        evidence_quality: "verbatim".to_owned(),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:abc123".to_owned(),
    };

    let ids: Vec<_> = (0..5)
        .map(|i| {
            build_command_evidence_records(&req)
                .unwrap_or_else(|e| panic!("run {i} failed: {e}"))
                .record_id
        })
        .collect();
    let first = &ids[0];
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            id, first,
            "run {i} command evidence record_id differs from run 0"
        );
    }
}

// ── AC1: Full workflow — all four evidence types in one pass ─────────────────

#[test]
fn full_workflow_writes_all_four_evidence_types() {
    let prov = valid_provenance();

    // 1. Observation
    let obs_req = ObservationRequest {
        provenance: prov.clone(),
        text: "complex function detected".to_owned(),
        confidence: 0.88,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:deadbeef01")],
    };
    let obs = build_observation_records(&obs_req).expect("observation must succeed");
    assert!(obs.record_id.starts_with("agent_memory:v1:"));

    // 2. Command evidence
    let cmd_req = CommandEvidenceRequest {
        provenance: prov.clone(),
        executed_at: "2026-05-30T10:01:00Z".to_owned(),
        exit_code: 0,
        stdout: Some("cargo test -- --test-threads=4\ntest result: ok".to_owned()),
        stderr: None,
        evidence_quality: "verbatim".to_owned(),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:fixture001".to_owned(),
    };
    let cmd = build_command_evidence_records(&cmd_req).expect("command evidence must succeed");
    assert!(cmd.record_id.starts_with("verification:v1:"));

    // 3. Artifact
    let art_req = ArtifactRequest {
        provenance: prov.clone(),
        patch_bytes: b"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n-pub fn f() {}\n+pub fn f() { todo!() }".to_vec(),
        target_files: vec!["src/lib.rs".to_owned()],
        patch_status: "unverified".to_owned(),
        base_commit: Some("abc123def456".to_owned()),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:fixture001".to_owned(),
    };
    let art = build_artifact_records(&art_req).expect("artifact must succeed");
    assert!(art.record_id.starts_with("artifact:v1:"));

    // 4. Verification result
    let ver_req = VerificationRequest {
        provenance: prov,
        executed_at: "2026-05-30T10:03:00Z".to_owned(),
        status: "pass".to_owned(),
        verification_kind: "test_run".to_owned(),
        stdout: Some("test result: ok. 3 passed".to_owned()),
        evidence_quality: "verbatim".to_owned(),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:fixture001".to_owned(),
        linked_command_evidence_id: Some(cmd.record_id.clone()),
    };
    let ver = build_verification_records(&ver_req).expect("verification must succeed");
    assert!(ver.record_id.starts_with("verification:v1:"));

    // All four IDs are distinct
    let ids = [
        &obs.record_id,
        &cmd.record_id,
        &art.record_id,
        &ver.record_id,
    ];
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        4,
        "all four evidence handles must be distinct"
    );

    // All records belong to correct domain types
    for record in obs
        .records
        .iter()
        .chain(cmd.records.iter())
        .chain(art.records.iter())
        .chain(ver.records.iter())
    {
        if let aletheia_egregore::ir::GraphRecord::Node { id, .. } = record {
            // Each node ID must use a known domain prefix
            assert!(
                id.starts_with("agent_memory:v1:")
                    || id.starts_with("verification:v1:")
                    || id.starts_with("artifact:v1:")
                    || id.starts_with("codegraph:"),
                "unexpected domain prefix in id: {id}"
            );
        }
    }
}

// ── AC4: Content-addressed idempotency ──────────────────────────────────────

#[test]
fn same_inputs_produce_same_evidence_handles() {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            agent_id: "agent-idem-001".to_owned(),
            agent_kind: "claude-code".to_owned(),
            session_id: "session-idem-001".to_owned(),
            observed_at: "2026-05-30T10:00:00Z".to_owned(),
            source_handle: Some("src/main.rs:sha256:cafe".to_owned()),
        },
        text: "idempotency test".to_owned(),
        confidence: 0.75,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abcdef")],
    };

    let first = build_observation_records(&req).expect("first write must succeed");
    let second = build_observation_records(&req).expect("second write must succeed");

    assert_eq!(
        first.record_id, second.record_id,
        "same inputs must produce same record_id"
    );
    assert_eq!(
        first.evidence_handle, second.evidence_handle,
        "same inputs must produce same evidence_handle"
    );
    assert_eq!(
        first.records.len(),
        second.records.len(),
        "same inputs must produce same number of records"
    );

    // Verify all record IDs are identical across both runs
    let first_ids: Vec<_> = first.records.iter().map(aletheia_egregore::GraphRecord::id).collect();
    let second_ids: Vec<_> = second.records.iter().map(aletheia_egregore::GraphRecord::id).collect();
    assert_eq!(
        first_ids, second_ids,
        "all record IDs must be identical across two identical writes"
    );
}

// ── AC5: Unresolved handles produce diagnostics ──────────────────────────────

#[test]
fn observation_with_invalid_confidence_is_rejected() {
    let req = ObservationRequest {
        provenance: valid_provenance(),
        text: "invalid confidence".to_owned(),
        confidence: 1.5, // out of [0.0, 1.0]
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abc123")],
    };
    let err = build_observation_records(&req).expect_err("confidence > 1.0 must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "confidence");
}

#[test]
fn artifact_rejects_unknown_patch_status() {
    let req = ArtifactRequest {
        provenance: valid_provenance(),
        patch_bytes: b"diff content".to_vec(),
        target_files: vec!["src/lib.rs".to_owned()],
        patch_status: "totally-invalid-status".to_owned(),
        base_commit: Some("abc123".to_owned()),
        source_artifact_path: "tests/fixtures/rust_basic".to_owned(),
        source_artifact_hash: "sha256:abc123".to_owned(),
    };
    let err = build_artifact_records(&req).expect_err("unknown patch_status must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "patch_status");
}

// ── AC9: Uses existing schema — no new node kinds or edge labels ─────────────

#[test]
fn evidence_writer_uses_only_existing_node_kinds() {
    let prov = valid_provenance();

    let obs_kinds: Vec<NodeKind> = build_observation_records(&ObservationRequest {
        provenance: prov,
        text: "schema check".to_owned(),
        confidence: 0.9,
        evidence_links: vec![dummy_evidence_link("codegraph:v4:abc")],
    })
    .expect("observation must succeed")
    .records
    .into_iter()
    .filter_map(|r| {
        if let aletheia_egregore::ir::GraphRecord::Node { kind, .. } = r {
            Some(kind)
        } else {
            None
        }
    })
    .collect();

    // All kinds must be in the existing NodeKind enum
    for kind in &obs_kinds {
        let name = kind.as_str();
        // If it's a valid NodeKind we can serialize it — just check known domain kinds
        assert!(
            matches!(
                kind,
                NodeKind::Agent
                    | NodeKind::AgentSession
                    | NodeKind::Observation
                    | NodeKind::CommandRun
                    | NodeKind::PatchArtifact
                    | NodeKind::Verification
                    | NodeKind::TestRun
            ),
            "unexpected node kind: {name}"
        );
    }
}
