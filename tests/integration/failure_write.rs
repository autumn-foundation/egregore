//! TDD tests for `eg write failure` — live-authored typed `Failure` records
//! (issue #264).
//!
//! RED → GREEN → REFACTOR:
//! These tests drive the `evidence` module's failure builder plus the
//! `eg write failure` CLI surface, mirroring the existing `eg write observation`
//! provenance contract (AC1) and feeding the already-shipped
//! `eg query failures` read lane (AC4).

#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;

use aletheia_egregore::evidence::{EvidenceProvenance, FailureRequest, build_failure_records};
use aletheia_egregore::ir::{EdgeLabel, GraphRecord, NodeKind};
use assert_cmd::Command;
use predicates::prelude::*;

const SYMBOL_TARGET: &str =
    "codegraph:v9:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const FILE_TARGET: &str =
    "codegraph:v9:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const TASK_TARGET: &str =
    "project:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SECRET_TEXT: &str = "attempt failed; API_KEY=\"supersecretvalue12345\" leaked in output";

fn valid_provenance() -> EvidenceProvenance {
    EvidenceProvenance {
        agent_id: "agent-fail-001".to_owned(),
        agent_kind: "other".to_owned(),
        session_id: "session-fail-001".to_owned(),
        observed_at: "2026-05-30T10:00:00Z".to_owned(),
        source_handle: Some("src/lib.rs:sha256:abc123".to_owned()),
    }
}

fn valid_request() -> FailureRequest {
    FailureRequest {
        provenance: valid_provenance(),
        failure_kind: "command_failure".to_owned(),
        text: "cargo test failed on the new parser edge case".to_owned(),
        exit_code: Some(1),
        failed_on: vec![SYMBOL_TARGET.to_owned()],
        references_task: vec![],
    }
}

fn failure_node(records: &[GraphRecord]) -> &GraphRecord {
    records
        .iter()
        .find(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Failure,
                    ..
                }
            )
        })
        .expect("Failure node must be present")
}

// ── AC1: provenance contract — missing fields rejected with field-naming errors ─

#[test]
fn failure_rejects_empty_agent_id() {
    let req = FailureRequest {
        provenance: EvidenceProvenance {
            agent_id: String::new(),
            ..valid_provenance()
        },
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("empty agent_id must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "agent_id");
    // The error envelope must never echo the failure text.
    assert!(
        !err.to_string().contains("parser edge case"),
        "error must not echo failure text"
    );
}

#[test]
fn failure_rejects_empty_session_id() {
    let req = FailureRequest {
        provenance: EvidenceProvenance {
            session_id: String::new(),
            ..valid_provenance()
        },
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("empty session_id must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "session_id");
}

#[test]
fn failure_rejects_empty_observed_at() {
    let req = FailureRequest {
        provenance: EvidenceProvenance {
            observed_at: String::new(),
            ..valid_provenance()
        },
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("empty observed_at must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "observed_at");
}

#[test]
fn failure_rejects_invalid_observed_at() {
    let req = FailureRequest {
        provenance: EvidenceProvenance {
            observed_at: "not-a-timestamp".to_owned(),
            ..valid_provenance()
        },
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("invalid observed_at must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "observed_at");
}

#[test]
fn failure_rejects_missing_source_handle() {
    let req = FailureRequest {
        provenance: EvidenceProvenance {
            source_handle: None,
            ..valid_provenance()
        },
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("missing source_handle must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "source_handle");
}

#[test]
fn failure_rejects_unknown_agent_kind() {
    let req = FailureRequest {
        provenance: EvidenceProvenance {
            agent_kind: "totally-unknown-bot".to_owned(),
            ..valid_provenance()
        },
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("unknown agent_kind must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "agent_kind");
}

#[test]
fn failure_rejects_empty_text() {
    let req = FailureRequest {
        text: String::new(),
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("empty text must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "text");
}

// ── AC2: closed failure_kind set; exit_code rules per kind ───────────────────

#[test]
fn failure_rejects_unknown_failure_kind() {
    let req = FailureRequest {
        failure_kind: "brain_meltdown".to_owned(),
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("unknown failure_kind must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "failure_kind");
}

#[test]
fn failure_accepts_every_reserved_failure_kind() {
    for kind in [
        "command_failure",
        "patch_invalid",
        "assumption_rejected",
        "workflow_blocked",
    ] {
        let req = FailureRequest {
            failure_kind: kind.to_owned(),
            exit_code: None,
            ..valid_request()
        };
        let outcome = build_failure_records(&req)
            .unwrap_or_else(|e| panic!("kind {kind} must be accepted: {e}"));
        let GraphRecord::Node { failure_kind, .. } = failure_node(&outcome.records) else {
            panic!("expected a Failure node");
        };
        assert_eq!(
            failure_kind.as_deref(),
            Some(kind),
            "stored failure_kind must round-trip"
        );
    }
}

#[test]
fn failure_command_kinds_keep_exit_code() {
    for (kind, code) in [("command_failure", 2i64), ("patch_invalid", 1i64)] {
        let req = FailureRequest {
            failure_kind: kind.to_owned(),
            exit_code: Some(code),
            ..valid_request()
        };
        let outcome = build_failure_records(&req)
            .unwrap_or_else(|e| panic!("kind {kind} must be accepted: {e}"));
        let GraphRecord::Node { exit_code, .. } = failure_node(&outcome.records) else {
            panic!("expected a Failure node");
        };
        assert_eq!(
            *exit_code,
            Some(code),
            "exit_code must be stored for {kind}"
        );
    }
}

#[test]
fn failure_non_command_kinds_round_trip_without_exit_code() {
    for kind in ["assumption_rejected", "workflow_blocked"] {
        let req = FailureRequest {
            failure_kind: kind.to_owned(),
            exit_code: None,
            ..valid_request()
        };
        let outcome = build_failure_records(&req)
            .unwrap_or_else(|e| panic!("kind {kind} must be accepted: {e}"));
        let GraphRecord::Node { exit_code, .. } = failure_node(&outcome.records) else {
            panic!("expected a Failure node");
        };
        assert_eq!(
            *exit_code, None,
            "non-command kind {kind} must round-trip with no exit_code"
        );
    }
}

#[test]
fn failure_rejects_exit_code_on_non_command_kind() {
    // The schema binds exit_code to command_failure/patch_invalid only; an
    // exit_code on a reasoning failure is a schema violation, not a silent drop.
    let req = FailureRequest {
        failure_kind: "assumption_rejected".to_owned(),
        exit_code: Some(1),
        ..valid_request()
    };
    let err =
        build_failure_records(&req).expect_err("exit_code on assumption_rejected must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "exit_code");
}

// ── AC3: linkage — at least one resolvable citable target ────────────────────

#[test]
fn failure_rejects_when_no_targets() {
    let req = FailureRequest {
        failed_on: vec![],
        references_task: vec![],
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("target-less write must be rejected");
    assert_eq!(err.code, "missing_field");
    assert_eq!(err.field, "failed_on");
}

#[test]
fn failure_accepts_task_only_linkage() {
    let req = FailureRequest {
        failed_on: vec![],
        references_task: vec![TASK_TARGET.to_owned()],
        ..valid_request()
    };
    let outcome = build_failure_records(&req).expect("task-only linkage must be accepted");
    let GraphRecord::Node { evidence_links, .. } = failure_node(&outcome.records) else {
        panic!("expected a Failure node");
    };
    let links = evidence_links
        .as_ref()
        .expect("evidence_links must be present");
    assert!(
        links
            .iter()
            .any(|l| l.relation == EdgeLabel::ReferencesTask.as_str()
                && l.target_record_id.as_deref() == Some(TASK_TARGET)
                && l.target_domain == "project"),
        "must carry a REFERENCES_TASK link to the project-domain task"
    );
}

#[test]
fn failure_rejects_malformed_failed_on_id() {
    let req = FailureRequest {
        failed_on: vec!["src/lib.rs".to_owned()],
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("non-ID failed_on must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "failed_on");
}

#[test]
fn failure_rejects_wrong_domain_failed_on_id() {
    let req = FailureRequest {
        failed_on: vec![TASK_TARGET.to_owned()],
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("project ID in failed_on must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "failed_on");
}

#[test]
fn failure_rejects_malformed_references_task_id() {
    let req = FailureRequest {
        references_task: vec!["owner/repo#42".to_owned()],
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("non-canonical task handle must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "references_task");
}

#[test]
fn failure_rejects_short_hex_canonical_id() {
    let req = FailureRequest {
        failed_on: vec!["codegraph:v1:abc123".to_owned()],
        ..valid_request()
    };
    let err = build_failure_records(&req).expect_err("short hex must be rejected");
    assert_eq!(err.code, "invalid_field");
    assert_eq!(err.field, "failed_on");
}

#[test]
fn failure_deduplicates_identical_targets() {
    let req = FailureRequest {
        failed_on: vec![SYMBOL_TARGET.to_owned(), SYMBOL_TARGET.to_owned()],
        references_task: vec![TASK_TARGET.to_owned(), TASK_TARGET.to_owned()],
        ..valid_request()
    };
    let outcome = build_failure_records(&req).expect("duplicate targets must be deduped");
    let single = build_failure_records(&FailureRequest {
        failed_on: vec![SYMBOL_TARGET.to_owned()],
        references_task: vec![TASK_TARGET.to_owned()],
        ..valid_request()
    })
    .expect("unique targets must succeed");
    assert_eq!(
        outcome.record_id, single.record_id,
        "duplicate targets must not alter the record ID"
    );
    let failure = failure_node(&outcome.records);
    if let GraphRecord::Node { evidence_links, .. } = failure {
        let links = evidence_links.as_deref().unwrap_or(&[]);
        let failed_on_count = links
            .iter()
            .filter(|l| l.relation.as_str() == "FAILED_ON")
            .count();
        let task_count = links
            .iter()
            .filter(|l| l.relation.as_str() == "REFERENCES_TASK")
            .count();
        assert_eq!(
            failed_on_count, 1,
            "duplicate FAILED_ON must collapse to one link"
        );
        assert_eq!(
            task_count, 1,
            "duplicate REFERENCES_TASK must collapse to one link"
        );
    } else {
        panic!("Failure node must be present");
    }
}

// ── Record shape: batch, provenance, relations ───────────────────────────────

#[test]
fn accepted_failure_produces_typed_batch() {
    let outcome = build_failure_records(&valid_request()).expect("valid failure must succeed");
    assert!(
        outcome.record_id.starts_with("agent_memory:v1:"),
        "failure record_id must use agent_memory:v1: prefix, got {}",
        outcome.record_id
    );
    assert_eq!(outcome.evidence_handle, outcome.record_id);

    let kinds: Vec<NodeKind> = outcome
        .records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node { kind, .. } = r {
                Some(*kind)
            } else {
                None
            }
        })
        .collect();
    for expected in [NodeKind::Agent, NodeKind::AgentSession, NodeKind::Failure] {
        assert!(
            kinds.contains(&expected),
            "batch must include {expected:?} node"
        );
    }

    let edge_labels: Vec<&str> = outcome
        .records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Edge { label, .. } = r {
                Some(label.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        edge_labels.contains(&"SESSION_OF"),
        "batch must include SESSION_OF edge"
    );
    assert!(
        edge_labels.contains(&"AUTHORED_BY"),
        "batch must include AUTHORED_BY edge"
    );
}

#[test]
fn accepted_failure_carries_provenance_and_links() {
    let req = FailureRequest {
        failed_on: vec![SYMBOL_TARGET.to_owned(), FILE_TARGET.to_owned()],
        references_task: vec![TASK_TARGET.to_owned()],
        ..valid_request()
    };
    let outcome = build_failure_records(&req).expect("valid failure must succeed");
    let GraphRecord::Node {
        agent_id,
        agent_kind,
        session_id,
        observed_at,
        source_handle,
        domain,
        evidence_links,
        summary,
        ..
    } = failure_node(&outcome.records)
    else {
        panic!("expected a Failure node");
    };
    assert_eq!(agent_id.as_deref(), Some("agent-fail-001"));
    assert_eq!(agent_kind.as_deref(), Some("other"));
    assert_eq!(session_id.as_deref(), Some("session-fail-001"));
    assert_eq!(observed_at.as_deref(), Some("2026-05-30T10:00:00Z"));
    assert!(source_handle.is_some(), "source_handle must be present");
    assert_eq!(domain.as_deref(), Some("agent_memory"));
    assert!(
        summary.contains("command_failure"),
        "summary must name the failure kind, got: {summary}"
    );

    let links = evidence_links
        .as_ref()
        .expect("evidence_links must be present");
    assert_eq!(links.len(), 3, "one link per target");
    let failed_on: Vec<_> = links
        .iter()
        .filter(|l| l.relation == EdgeLabel::FailedOn.as_str())
        .collect();
    assert_eq!(failed_on.len(), 2, "two FAILED_ON links");
    assert!(
        failed_on.iter().all(|l| l.target_domain == "codegraph"),
        "FAILED_ON links target the codegraph domain"
    );
    let task_links: Vec<_> = links
        .iter()
        .filter(|l| l.relation == EdgeLabel::ReferencesTask.as_str())
        .collect();
    assert_eq!(task_links.len(), 1, "one REFERENCES_TASK link");
    assert_eq!(task_links[0].target_domain, "project");
    assert_eq!(task_links[0].target_record_id.as_deref(), Some(TASK_TARGET));
}

// ── AC6: redacted excerpt bound ──────────────────────────────────────────────

#[test]
fn failure_text_is_redacted_and_bounded() {
    let long = format!("{SECRET_TEXT} {}", "x".repeat(2000));
    let req = FailureRequest {
        text: long,
        ..valid_request()
    };
    let outcome = build_failure_records(&req).expect("valid failure must succeed");
    let GraphRecord::Node {
        text,
        redaction_policy_version,
        ..
    } = failure_node(&outcome.records)
    else {
        panic!("expected a Failure node");
    };
    let text = text.as_deref().expect("text must be present");
    assert!(
        !text.contains("supersecretvalue12345"),
        "secret must be redacted from stored text"
    );
    assert!(
        text.contains("<REDACTED:"),
        "redaction marker must be present, got: {text}"
    );
    assert!(
        text.chars().count() <= 501,
        "stored text must stay within the 500-char excerpt bound, got {} chars",
        text.chars().count()
    );
    assert!(
        text.ends_with('…'),
        "truncated excerpt must carry the … marker"
    );
    assert!(
        redaction_policy_version.is_some(),
        "redaction_policy_version must be set when redaction fired"
    );
}

#[test]
fn failure_short_text_is_stored_verbatim_without_marker() {
    let req = FailureRequest {
        text: "tried bumping the version; still red".to_owned(),
        ..valid_request()
    };
    let outcome = build_failure_records(&req).expect("valid failure must succeed");
    let GraphRecord::Node { text, .. } = failure_node(&outcome.records) else {
        panic!("expected a Failure node");
    };
    assert_eq!(
        text.as_deref(),
        Some("tried bumping the version; still red"),
        "short text must be stored verbatim"
    );
}

// ── AC5: determinism — identical identity inputs converge ────────────────────

#[test]
fn five_identical_failure_writes_produce_same_record_ids() {
    let ids: Vec<_> = (0..5)
        .map(|i| {
            build_failure_records(&valid_request())
                .unwrap_or_else(|e| panic!("run {i} must succeed: {e}"))
                .record_id
        })
        .collect();
    let first = &ids[0];
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(id, first, "run {i} record_id differs from run 0");
    }
}

#[test]
fn link_order_does_not_change_failure_id() {
    let req_a = FailureRequest {
        failed_on: vec![SYMBOL_TARGET.to_owned(), FILE_TARGET.to_owned()],
        ..valid_request()
    };
    let req_b = FailureRequest {
        failed_on: vec![FILE_TARGET.to_owned(), SYMBOL_TARGET.to_owned()],
        ..valid_request()
    };
    let id_a = build_failure_records(&req_a)
        .expect("a must succeed")
        .record_id;
    let id_b = build_failure_records(&req_b)
        .expect("b must succeed")
        .record_id;
    assert_eq!(
        id_a, id_b,
        "link submission order must not change the record ID"
    );
}

#[test]
fn distinct_text_produces_distinct_failure_ids() {
    let id_a = build_failure_records(&valid_request())
        .expect("a must succeed")
        .record_id;
    let req_b = FailureRequest {
        text: "a different dead end on the same symbol".to_owned(),
        ..valid_request()
    };
    let id_b = build_failure_records(&req_b)
        .expect("b must succeed")
        .record_id;
    assert_ne!(
        id_a, id_b,
        "different text must produce different record IDs"
    );
}

#[test]
fn live_failure_id_cannot_collide_with_traj_importer_ids() {
    // The traj importer mints ["node", "failure", kind, turn_id, action_idx];
    // live writes carry an explicit "live" discriminator so the two keyspaces
    // can never mint the same ID for the same kind.
    let outcome = build_failure_records(&valid_request()).expect("valid failure must succeed");
    let id = &outcome.record_id;
    assert!(
        id.starts_with("agent_memory:v1:"),
        "live failure must use the agent_memory prefix"
    );
    // The ID must differ from the importer shape for the same kind string.
    let importer_shaped = aletheia_egregore::ir::agent_memory_stable_id(&[
        "node",
        "failure",
        "command_failure",
        "agent-fail-001",
        "session-fail-001",
    ]);
    assert_ne!(
        id, &importer_shaped,
        "live-authored ID must not collide with the importer keyspace"
    );
}

// ── CLI surface ──────────────────────────────────────────────────────────────

fn eg() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

#[test]
fn cli_write_failure_missing_agent_id_emits_json_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("out.jsonl");
    eg().args([
        "write",
        "failure",
        "--session-id",
        "s1",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "src/lib.rs:sha256:abc",
        "--failure-kind",
        "command_failure",
        "--text",
        "SECRET_FAILURE_TEXT_MUST_NOT_LEAK",
        "--failed-on",
        SYMBOL_TARGET,
        "--out",
        out.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(r#""code":"missing_field""#))
    .stderr(predicate::str::contains(r#""field":"agent_id""#))
    .stderr(predicate::str::contains("SECRET_FAILURE_TEXT_MUST_NOT_LEAK").not());
    assert!(
        !out.exists(),
        "rejected write must not create the output file"
    );
}

#[test]
fn cli_write_failure_missing_agent_kind_emits_json_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("out.jsonl");
    eg().args([
        "write",
        "failure",
        "--agent-id",
        "a1",
        "--session-id",
        "s1",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "src/lib.rs:sha256:abc",
        "--failure-kind",
        "command_failure",
        "--text",
        "SECRET_FAILURE_TEXT_MUST_NOT_LEAK",
        "--failed-on",
        SYMBOL_TARGET,
        "--out",
        out.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(r#""code":"missing_field""#))
    .stderr(predicate::str::contains(r#""field":"agent_kind""#))
    .stderr(predicate::str::contains("SECRET_FAILURE_TEXT_MUST_NOT_LEAK").not());
    assert!(
        !out.exists(),
        "rejected write must not create the output file"
    );
}

#[test]
fn cli_write_failure_without_targets_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("out.jsonl");
    eg().args([
        "write",
        "failure",
        "--agent-id",
        "a1",
        "--agent-kind",
        "other",
        "--session-id",
        "s1",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "src/lib.rs:sha256:abc",
        "--failure-kind",
        "workflow_blocked",
        "--text",
        "blocked on a design question",
        "--out",
        out.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(r#""code":"missing_field""#))
    .stderr(predicate::str::contains(r#""field":"failed_on""#));
    assert!(
        !out.exists(),
        "rejected write must not create the output file"
    );
}

#[test]
fn cli_write_failure_happy_path_writes_typed_jsonl() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("out.jsonl");
    eg().args([
        "write",
        "failure",
        "--agent-id",
        "a1",
        "--agent-kind",
        "other",
        "--session-id",
        "s1",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "src/lib.rs:sha256:abc",
        "--failure-kind",
        "assumption_rejected",
        "--text",
        "assumption about the config schema was wrong",
        "--failed-on",
        SYMBOL_TARGET,
        "--references-task",
        TASK_TARGET,
        "--out",
        out.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(r#""ok":true"#));

    let jsonl = fs::read_to_string(&out).expect("out.jsonl must exist");
    let failure_line = jsonl
        .lines()
        .find(|l| l.contains(r#""kind":"Failure""#))
        .expect("JSONL must contain a Failure node");
    let v: serde_json::Value =
        serde_json::from_str(failure_line).expect("Failure line must be valid JSON");
    assert_eq!(v["failure_kind"], "assumption_rejected");
    assert!(
        v["id"]
            .as_str()
            .unwrap_or("")
            .starts_with("agent_memory:v1:"),
        "Failure ID must use the agent_memory prefix"
    );
    assert!(
        v.get("exit_code").is_none() || v["exit_code"].is_null(),
        "assumption_rejected must carry no exit_code"
    );
}

#[test]
fn cli_write_failure_rerun_is_byte_identical() {
    let temp = tempfile::tempdir().expect("temp dir");
    let base_args = |out: &PathBuf| {
        vec![
            "write".to_owned(),
            "failure".to_owned(),
            "--agent-id".to_owned(),
            "a1".to_owned(),
            "--agent-kind".to_owned(),
            "other".to_owned(),
            "--session-id".to_owned(),
            "s1".to_owned(),
            "--observed-at".to_owned(),
            "2026-05-30T10:00:00Z".to_owned(),
            "--source-handle".to_owned(),
            "src/lib.rs:sha256:abc".to_owned(),
            "--failure-kind".to_owned(),
            "command_failure".to_owned(),
            "--exit-code".to_owned(),
            "1".to_owned(),
            "--text".to_owned(),
            "same dead end twice".to_owned(),
            "--failed-on".to_owned(),
            SYMBOL_TARGET.to_owned(),
            "--out".to_owned(),
            out.to_str().unwrap().to_owned(),
        ]
    };
    let out_a = temp.path().join("a.jsonl");
    let out_b = temp.path().join("b.jsonl");
    eg().args(base_args(&out_a)).assert().success();
    eg().args(base_args(&out_b)).assert().success();
    let a = fs::read_to_string(&out_a).expect("a.jsonl must exist");
    let b = fs::read_to_string(&out_b).expect("b.jsonl must exist");
    assert_eq!(
        a, b,
        "re-running with identical inputs must be byte-identical"
    );
}

// ── AC4: end-to-end — the read lane returns the live-authored failure ────────

fn seed_code_graph(
    path: &std::path::Path,
    file_path: &str,
    symbol: &str,
) -> (String, String, String) {
    use aletheia_egregore::ir::{Graph, project_stable_id, stable_id};
    let mut graph = Graph::new();
    let repo_id = stable_id(&["node", "Repository", "repo-a"]);
    graph.push(GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repository repo-a".to_owned(),
    ));
    let file_id = stable_id(&["node", "File", file_path]);
    let mut file = GraphRecord::syntax_node(
        file_id.clone(),
        NodeKind::File,
        file_path.to_owned(),
        aletheia_egregore::SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: 1,
            end_line: 100,
            start_column: None,
            end_column: None,
        },
        "lib.rs".to_owned(),
        "rust",
        "Source file lib.rs".to_owned(),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut file {
        *schema_version = aletheia_egregore::ir::SCHEMA_VERSION;
    }
    graph.push(file);
    let symbol_id = stable_id(&["node", "Symbol", file_path, symbol]);
    let mut sym = GraphRecord::syntax_node(
        symbol_id.clone(),
        NodeKind::Symbol,
        file_path.to_owned(),
        aletheia_egregore::SourceSpan {
            start_byte: 0,
            end_byte: 50,
            start_line: 10,
            end_line: 20,
            start_column: None,
            end_column: None,
        },
        symbol.to_owned(),
        "rust",
        format!("Symbol {symbol}"),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut sym {
        *schema_version = aletheia_egregore::ir::SCHEMA_VERSION;
    }
    graph.push(sym);
    graph.push(GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Contains,
        repo_id,
        file_id.clone(),
        None,
        "repo contains file".to_owned(),
    ));
    graph.push(GraphRecord::edge(
        aletheia_egregore::EdgeLabel::Defines,
        file_id.clone(),
        symbol_id.clone(),
        None,
        "file defines symbol".to_owned(),
    ));
    // A canonical project Task node: the `eg query failures <task-handle>` lane
    // resolves canonical `project:` IDs to Task nodes, so the seeded task makes
    // the references_task link queryable end to end.
    let task_id = project_stable_id(&["task", "task-264"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Task #264".to_owned()),
        "Expose prior failed attempts".to_owned(),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut task {
        *schema_version = aletheia_egregore::ir::PROJECT_SCHEMA_VERSION;
    }
    graph.push(task);
    let jsonl = graph.to_jsonl().expect("seed graph must serialize");
    fs::write(path, jsonl).expect("seed graph must be written");
    (file_id, symbol_id, task_id)
}

#[test]
fn query_failures_returns_live_authored_failure() {
    let temp = tempfile::tempdir().expect("temp dir");
    let seed_path = temp.path().join("seed.jsonl");
    let (file_id, symbol_id, task_id) = seed_code_graph(&seed_path, "src/lib.rs", "foo");

    // Author the failure live against the symbol ID and a task handle.
    let failure_out = temp.path().join("failure.jsonl");
    eg().args([
        "write",
        "failure",
        "--agent-id",
        "live-agent",
        "--agent-kind",
        "other",
        "--session-id",
        "live-session",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "src/lib.rs:sha256:abc",
        "--failure-kind",
        "command_failure",
        "--exit-code",
        "1",
        "--text",
        "cargo test foo failed with an assertion",
        "--failed-on",
        &symbol_id,
        "--references-task",
        &task_id,
        "--out",
        failure_out.to_str().unwrap(),
    ])
    .assert()
    .success();

    // Merge the authored records into the code graph (the operator ingest path).
    let combined = temp.path().join("combined.jsonl");
    let mut merged = fs::read_to_string(&seed_path).expect("seed must read");
    merged.push_str(&fs::read_to_string(&failure_out).expect("failure must read"));
    fs::write(&combined, merged).expect("combined must write");

    // Symbol ID, exact symbol name, and task handle must each surface the
    // live-authored failure under agent_failures with trust class agent_authored.
    for (handle, expected_target) in [
        (symbol_id.as_str(), symbol_id.as_str()),
        ("foo", symbol_id.as_str()),
        (task_id.as_str(), task_id.as_str()),
    ] {
        let assert = eg()
            .args([
                "query",
                "failures",
                handle,
                "--graph",
                combined.to_str().unwrap(),
            ])
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
        let v: serde_json::Value =
            serde_json::from_str(&stdout).expect("query output must be JSON");
        assert_eq!(v["ok"], true, "query must succeed for handle {handle}");
        let agent_failures = v["agent_failures"]
            .as_array()
            .expect("agent_failures must be an array");
        assert_eq!(
            agent_failures.len(),
            1,
            "exactly one live-authored failure must be returned for handle {handle}"
        );
        let item = &agent_failures[0];
        assert_eq!(item["trust_class"], "agent_authored");
        assert_eq!(item["failure_kind"], "command_failure");
        assert_eq!(item["matched_target"], expected_target);
    }
    let _ = file_id;
}

#[test]
fn query_failures_by_file_path_returns_live_authored_failure() {
    // A failure authored against a File target is reachable through the file
    // path handle, exactly like a symbol target through its symbol handles.
    let temp = tempfile::tempdir().expect("temp dir");
    let seed_path = temp.path().join("seed.jsonl");
    let (file_id, _symbol_id, _task_id) = seed_code_graph(&seed_path, "src/lib.rs", "foo");

    let failure_out = temp.path().join("failure.jsonl");
    eg().args([
        "write",
        "failure",
        "--agent-id",
        "live-agent",
        "--agent-kind",
        "other",
        "--session-id",
        "live-session",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "src/lib.rs:sha256:abc",
        "--failure-kind",
        "patch_invalid",
        "--text",
        "the patch did not apply to src/lib.rs",
        "--failed-on",
        &file_id,
        "--out",
        failure_out.to_str().unwrap(),
    ])
    .assert()
    .success();

    let combined = temp.path().join("combined.jsonl");
    let mut merged = fs::read_to_string(&seed_path).expect("seed must read");
    merged.push_str(&fs::read_to_string(&failure_out).expect("failure must read"));
    fs::write(&combined, merged).expect("combined must write");

    let assert = eg()
        .args([
            "query",
            "failures",
            "src/lib.rs",
            "--graph",
            combined.to_str().unwrap(),
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("query output must be JSON");
    let agent_failures = v["agent_failures"]
        .as_array()
        .expect("agent_failures must be an array");
    assert_eq!(
        agent_failures.len(),
        1,
        "the file-path query must return the live-authored failure"
    );
    assert_eq!(agent_failures[0]["trust_class"], "agent_authored");
    assert_eq!(agent_failures[0]["failure_kind"], "patch_invalid");
    assert_eq!(agent_failures[0]["matched_target"], file_id);
}

#[test]
fn query_failures_by_source_handle_returns_live_authored_failure() {
    // The source-handle read lane (handle type #5) must also reach the failure:
    // the authored source_handle is itself a citable handle.
    let temp = tempfile::tempdir().expect("temp dir");
    let seed_path = temp.path().join("seed.jsonl");
    let (_file_id, symbol_id, _task_id) = seed_code_graph(&seed_path, "src/lib.rs", "foo");

    let failure_out = temp.path().join("failure.jsonl");
    eg().args([
        "write",
        "failure",
        "--agent-id",
        "live-agent",
        "--agent-kind",
        "other",
        "--session-id",
        "live-session",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "live-session-handle-xyz",
        "--failure-kind",
        "workflow_blocked",
        "--text",
        "blocked waiting on a design decision",
        "--failed-on",
        &symbol_id,
        "--out",
        failure_out.to_str().unwrap(),
    ])
    .assert()
    .success();

    let combined = temp.path().join("combined.jsonl");
    let mut merged = fs::read_to_string(&seed_path).expect("seed must read");
    merged.push_str(&fs::read_to_string(&failure_out).expect("failure must read"));
    fs::write(&combined, merged).expect("combined must write");

    let assert = eg()
        .args([
            "query",
            "failures",
            "live-session-handle-xyz",
            "--graph",
            combined.to_str().unwrap(),
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("query output must be JSON");
    let agent_failures = v["agent_failures"]
        .as_array()
        .expect("agent_failures must be an array");
    assert_eq!(
        agent_failures.len(),
        1,
        "the source-handle query must return the live-authored failure"
    );
    assert_eq!(agent_failures[0]["failure_kind"], "workflow_blocked");
}
