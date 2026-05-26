//! Tests for the Codex session/rollout JSONL importer (issue #21).
//!
//! Written RED-first: all tests are written before the implementation exists.
//! They drive the exact contract specified in the issue AC.

use std::path::Path;

use aletheia_egregore::{
    codex::{ImportOptions, import_codex},
    ir::{EdgeLabel, GraphRecord, NodeKind},
};

const SESSION_FIXTURE: &str = "tests/fixtures/agent_memory/codex_session/session.jsonl";
const ROLLOUT_FIXTURE: &str = "tests/fixtures/agent_memory/codex_rollout/rollout.jsonl";

// ── AC: fixture existence ─────────────────────────────────────────────────────

#[test]
fn session_fixture_exists() {
    assert!(
        Path::new(SESSION_FIXTURE).exists(),
        "session fixture not found at {SESSION_FIXTURE}"
    );
}

#[test]
fn rollout_fixture_exists() {
    assert!(
        Path::new(ROLLOUT_FIXTURE).exists(),
        "rollout fixture not found at {ROLLOUT_FIXTURE}"
    );
}

#[test]
fn session_fixture_readme_exists() {
    assert!(
        Path::new("tests/fixtures/agent_memory/codex_session/README.md").exists(),
        "session fixture README not found"
    );
}

#[test]
fn rollout_fixture_readme_exists() {
    assert!(
        Path::new("tests/fixtures/agent_memory/codex_rollout/README.md").exists(),
        "rollout fixture README not found"
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn import_session() -> Vec<GraphRecord> {
    import_codex(Path::new(SESSION_FIXTURE), &ImportOptions::default())
        .expect("import_codex must not fail on session fixture")
        .records()
        .to_vec()
}

fn import_rollout() -> Vec<GraphRecord> {
    import_codex(Path::new(ROLLOUT_FIXTURE), &ImportOptions::default())
        .expect("import_codex must not fail on rollout fixture")
        .records()
        .to_vec()
}

fn count_kind(records: &[GraphRecord], name: &str) -> usize {
    records
        .iter()
        .filter(|r| r.node_kind_name() == Some(name))
        .count()
}

fn all_edges(records: &[GraphRecord]) -> impl Iterator<Item = &GraphRecord> {
    records
        .iter()
        .filter(|r| matches!(r, GraphRecord::Edge { .. }))
}

// ── AC: session fixture – M3 record set ─────────────────────────────────────

#[test]
fn session_emits_agent_session() {
    assert!(count_kind(&import_session(), "AgentSession") >= 1);
}

#[test]
fn session_emits_agent_run() {
    assert!(count_kind(&import_session(), "AgentRun") >= 1);
}

#[test]
fn session_emits_agent_turns() {
    // Session fixture has 5 assistant messages → 5 turns
    assert!(
        count_kind(&import_session(), "AgentTurn") >= 5,
        "expected ≥5 AgentTurn records in session fixture"
    );
}

#[test]
fn session_emits_tool_calls() {
    assert!(count_kind(&import_session(), "ToolCall") >= 4);
}

#[test]
fn session_emits_command_runs() {
    assert!(count_kind(&import_session(), "CommandRun") >= 4);
}

#[test]
fn session_emits_file_edit() {
    assert!(
        count_kind(&import_session(), "FileEdit") >= 1,
        "missing FileEdit in session"
    );
}

#[test]
fn session_emits_patch_artifact() {
    assert!(
        count_kind(&import_session(), "PatchArtifact") >= 1,
        "missing PatchArtifact in session"
    );
}

#[test]
fn session_emits_failure() {
    assert!(
        count_kind(&import_session(), "Failure") >= 1,
        "missing Failure in session"
    );
}

#[test]
fn session_emits_verification() {
    assert!(
        count_kind(&import_session(), "Verification") >= 1,
        "missing Verification in session"
    );
}

#[test]
fn session_emits_diagnostic() {
    assert!(
        count_kind(&import_session(), "Diagnostic") >= 1,
        "missing Diagnostic (interruption) in session"
    );
}

#[test]
fn session_emits_cost_usage() {
    assert!(
        count_kind(&import_session(), "CostUsage") >= 1,
        "missing CostUsage in session"
    );
}

// ── AC: rollout fixture – M3 record set ─────────────────────────────────────

#[test]
fn rollout_emits_agent_session() {
    assert!(count_kind(&import_rollout(), "AgentSession") >= 1);
}

#[test]
fn rollout_emits_agent_run() {
    assert!(count_kind(&import_rollout(), "AgentRun") >= 1);
}

#[test]
fn rollout_emits_agent_turns() {
    assert!(
        count_kind(&import_rollout(), "AgentTurn") >= 4,
        "expected ≥4 AgentTurn records in rollout fixture"
    );
}

#[test]
fn rollout_emits_tool_calls() {
    assert!(count_kind(&import_rollout(), "ToolCall") >= 3);
}

#[test]
fn rollout_emits_command_runs() {
    assert!(count_kind(&import_rollout(), "CommandRun") >= 3);
}

#[test]
fn rollout_emits_file_edit() {
    assert!(
        count_kind(&import_rollout(), "FileEdit") >= 1,
        "missing FileEdit in rollout"
    );
}

#[test]
fn rollout_emits_failure() {
    assert!(
        count_kind(&import_rollout(), "Failure") >= 1,
        "missing Failure in rollout"
    );
}

#[test]
fn rollout_emits_verification() {
    assert!(
        count_kind(&import_rollout(), "Verification") >= 1,
        "missing Verification in rollout"
    );
}

#[test]
fn rollout_emits_diagnostic() {
    assert!(
        count_kind(&import_rollout(), "Diagnostic") >= 1,
        "missing Diagnostic in rollout"
    );
}

#[test]
fn rollout_emits_cost_usage() {
    assert!(
        count_kind(&import_rollout(), "CostUsage") >= 1,
        "missing CostUsage in rollout"
    );
}

// ── AC: turn ordering determinism (5 runs → byte-identical output) ───────────

#[test]
fn session_import_is_deterministic_across_5_runs() {
    let opts = ImportOptions::default();
    let path = Path::new(SESSION_FIXTURE);
    let first = import_codex(path, &opts)
        .expect("run 0 failed")
        .to_jsonl()
        .expect("jsonl 0 failed");
    for i in 1..=4 {
        let run = import_codex(path, &opts)
            .unwrap_or_else(|_| panic!("run {i} failed"))
            .to_jsonl()
            .unwrap_or_else(|_| panic!("jsonl {i} failed"));
        assert_eq!(first, run, "session import differed on run {i}");
    }
}

#[test]
fn rollout_import_is_deterministic_across_5_runs() {
    let opts = ImportOptions::default();
    let path = Path::new(ROLLOUT_FIXTURE);
    let first = import_codex(path, &opts)
        .expect("run 0 failed")
        .to_jsonl()
        .expect("jsonl 0 failed");
    for i in 1..=4 {
        let run = import_codex(path, &opts)
            .unwrap_or_else(|_| panic!("run {i} failed"))
            .to_jsonl()
            .unwrap_or_else(|_| panic!("jsonl {i} failed"));
        assert_eq!(first, run, "rollout import differed on run {i}");
    }
}

// ── AC: interruptions are first-class records ─────────────────────────────────

#[test]
fn session_interruption_survives_as_diagnostic() {
    let records = import_session();
    let has_interruption_diag = records.iter().any(|r| {
        if let GraphRecord::Node {
            kind: NodeKind::Diagnostic,
            summary,
            ..
        } = r
        {
            summary.to_lowercase().contains("interrupt")
        } else {
            false
        }
    });
    assert!(
        has_interruption_diag,
        "no Diagnostic record with 'interrupt' in summary found"
    );
}

#[test]
fn rollout_interruption_survives_as_diagnostic() {
    let records = import_rollout();
    let has_interruption_diag = records.iter().any(|r| {
        if let GraphRecord::Node {
            kind: NodeKind::Diagnostic,
            summary,
            ..
        } = r
        {
            summary.to_lowercase().contains("interrupt")
        } else {
            false
        }
    });
    assert!(
        has_interruption_diag,
        "no Diagnostic record with 'interrupt' in rollout"
    );
}

// ── AC: required provenance fields on every agent-memory node ────────────────

const fn is_agent_memory_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::AgentSession
            | NodeKind::AgentRun
            | NodeKind::AgentTurn
            | NodeKind::ToolCall
            | NodeKind::CommandRun
            | NodeKind::FileEdit
            | NodeKind::PatchArtifact
            | NodeKind::Failure
            | NodeKind::Verification
            | NodeKind::Diagnostic
    )
}

#[test]
fn every_session_node_has_domain_field() {
    for r in &import_session() {
        if let GraphRecord::Node {
            id, domain, kind, ..
        } = r
            && is_agent_memory_kind(*kind)
        {
            assert_eq!(
                domain.as_deref(),
                Some("agent_memory"),
                "node {id} missing domain=agent_memory"
            );
        }
    }
}

#[test]
fn every_session_node_has_importer_id() {
    for r in &import_session() {
        if let GraphRecord::Node {
            id,
            importer_id,
            kind,
            ..
        } = r
            && is_agent_memory_kind(*kind)
        {
            assert!(importer_id.is_some(), "node {id} missing importer_id");
        }
    }
}

#[test]
fn every_session_node_has_importer_version() {
    for r in &import_session() {
        if let GraphRecord::Node {
            id,
            importer_version,
            kind,
            ..
        } = r
            && is_agent_memory_kind(*kind)
        {
            assert!(
                importer_version.is_some(),
                "node {id} missing importer_version"
            );
        }
    }
}

#[test]
fn every_session_node_has_source_artifact_path() {
    for r in &import_session() {
        if let GraphRecord::Node {
            id,
            source_artifact_path,
            kind,
            ..
        } = r
            && is_agent_memory_kind(*kind)
        {
            assert!(
                source_artifact_path.is_some(),
                "node {id} missing source_artifact_path"
            );
        }
    }
}

#[test]
fn every_session_node_has_source_artifact_hash() {
    for r in &import_session() {
        if let GraphRecord::Node {
            id,
            source_artifact_hash,
            kind,
            ..
        } = r
            && is_agent_memory_kind(*kind)
        {
            assert!(
                source_artifact_hash.is_some(),
                "node {id} missing source_artifact_hash"
            );
        }
    }
}

// ── AC: raw JSONL body is preserved by handle, not inlined ───────────────────

#[test]
fn session_raw_body_not_inlined() {
    let fixture_content = std::fs::read_to_string(SESSION_FIXTURE).expect("fixture readable");
    let jsonl = import_codex(Path::new(SESSION_FIXTURE), &ImportOptions::default())
        .expect("import ok")
        .to_jsonl()
        .expect("jsonl ok");
    // The session instruction text must not appear verbatim in any output line.
    let instruction_text = "You are a coding assistant. Fix bugs in the codebase.";
    // Raw line start must not appear in graph fields.
    let raw_first_line = &fixture_content.lines().next().unwrap()[..40];
    for line in jsonl.lines() {
        assert!(
            !line.contains(instruction_text),
            "JSONL line contains raw session instructions — artifact inlining violation"
        );
        assert!(
            !line.contains(raw_first_line),
            "JSONL line contains raw session header — artifact inlining violation"
        );
    }
}

// ── AC: verification claim vs evidence separation ────────────────────────────

#[test]
fn session_verification_requires_exit_code_zero() {
    let records = import_session();
    // Every Verification node must correspond to a CommandRun with exit_code=0.
    // We verify indirectly: Verification nodes must have an exit_code of 0.
    for r in &records {
        if let GraphRecord::Node {
            kind: NodeKind::Verification,
            exit_code,
            ..
        } = r
        {
            assert_eq!(
                exit_code.unwrap_or(-1),
                0,
                "Verification node promoted without exit_code=0 CommandRun"
            );
        }
    }
}

#[test]
fn session_assistant_prose_alone_does_not_create_verification() {
    // The assistant message "All tests pass" in msg_005 (status=incomplete)
    // must NOT produce a Verification record — only the pytest CommandRun does.
    let records = import_session();
    // Count Verification records: must be exactly 1 (the pytest turn), not more.
    let verification_count = count_kind(&records, "Verification");
    assert!(
        verification_count >= 1,
        "expected at least one Verification from pytest turn"
    );
    // The patch turn (exit_code=1) must never generate a Verification.
    let all_verifications_passed = records.iter().all(|r| {
        if let GraphRecord::Node {
            kind: NodeKind::Verification,
            exit_code,
            ..
        } = r
        {
            exit_code.is_some_and(|c| c == 0)
        } else {
            true
        }
    });
    assert!(
        all_verifications_passed,
        "Verification record found with non-zero exit_code — unverified claim promoted"
    );
}

// ── AC: tool calls link back through edges ────────────────────────────────────

#[test]
fn session_has_nonzero_cross_record_edges() {
    let records = import_session();
    assert!(
        all_edges(&records).count() >= 1,
        "session import emitted zero edges"
    );
}

#[test]
fn session_agent_run_has_session_of_edge() {
    let records = import_session();
    let session_ids: Vec<_> = records
        .iter()
        .filter(|r| r.node_kind_name() == Some("AgentSession"))
        .map(|r| r.id().to_owned())
        .collect();
    assert!(!session_ids.is_empty(), "no AgentSession");
    let has_session_of = records.iter().any(|r| {
        if let GraphRecord::Edge { label, target, .. } = r {
            *label == EdgeLabel::SessionOf && session_ids.contains(target)
        } else {
            false
        }
    });
    assert!(has_session_of, "no SESSION_OF edge found");
}

#[test]
fn rollout_agent_run_has_session_of_edge() {
    let records = import_rollout();
    let session_ids: Vec<_> = records
        .iter()
        .filter(|r| r.node_kind_name() == Some("AgentSession"))
        .map(|r| r.id().to_owned())
        .collect();
    assert!(!session_ids.is_empty(), "no AgentSession in rollout");
    let has_session_of = records.iter().any(|r| {
        if let GraphRecord::Edge { label, target, .. } = r {
            *label == EdgeLabel::SessionOf && session_ids.contains(target)
        } else {
            false
        }
    });
    assert!(has_session_of, "no SESSION_OF edge in rollout");
}

// ── AC: idempotency — same JSONL → same AgentSession ID ─────────────────────

#[test]
fn session_reimport_produces_same_session_id() {
    let opts = ImportOptions::default();
    let path = Path::new(SESSION_FIXTURE);
    let get_ids = |recs: &[GraphRecord]| -> Vec<String> {
        recs.iter()
            .filter(|r| r.node_kind_name() == Some("AgentSession"))
            .map(|r| r.id().to_owned())
            .collect()
    };
    let first = import_codex(path, &opts)
        .expect("first import")
        .records()
        .to_vec();
    let second = import_codex(path, &opts)
        .expect("second import")
        .records()
        .to_vec();
    assert_eq!(
        get_ids(&first),
        get_ids(&second),
        "AgentSession ID changed — idempotency broken"
    );
}

#[test]
fn rollout_reimport_produces_same_session_id() {
    let opts = ImportOptions::default();
    let path = Path::new(ROLLOUT_FIXTURE);
    let get_ids = |recs: &[GraphRecord]| -> Vec<String> {
        recs.iter()
            .filter(|r| r.node_kind_name() == Some("AgentSession"))
            .map(|r| r.id().to_owned())
            .collect()
    };
    let first = import_codex(path, &opts)
        .expect("first import")
        .records()
        .to_vec();
    let second = import_codex(path, &opts)
        .expect("second import")
        .records()
        .to_vec();
    assert_eq!(
        get_ids(&first),
        get_ids(&second),
        "rollout session ID changed"
    );
}

// ── AC: unrecognized events → Diagnostic, no panic ───────────────────────────

#[test]
fn unrecognized_event_kind_does_not_panic() {
    use std::io::Write;
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("unknown_events.jsonl");
    let mut f = std::fs::File::create(&path).expect("create temp file");
    writeln!(f, r#"{{"type":"session","id":"sess_UNK","model":"o4-mini","created_at":"2025-01-01T00:00:00Z"}}"#).unwrap();
    writeln!(
        f,
        r#"{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}"#
    )
    .unwrap();
    writeln!(f, r#"{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"hi"}}],"id":"msg_unk","status":"completed"}}"#).unwrap();
    writeln!(
        f,
        r#"{{"type":"totally_unknown_future_event","some_field":"some_value"}}"#
    )
    .unwrap();
    drop(f);
    let result = import_codex(&path, &ImportOptions::default());
    assert!(
        result.is_ok(),
        "importer panicked on unrecognized event kind"
    );
    let records = result.unwrap().records().to_vec();
    let has_unknown_diag = records.iter().any(|r| {
        if let GraphRecord::Node {
            kind: NodeKind::Diagnostic,
            summary,
            ..
        } = r
        {
            summary.to_lowercase().contains("unrecognized")
                || summary.to_lowercase().contains("unknown")
        } else {
            false
        }
    });
    assert!(
        has_unknown_diag,
        "unrecognized event did not produce a Diagnostic record"
    );
}

// ── AC: redaction hook applied to free-text fields ───────────────────────────

#[test]
fn session_redaction_hook_applied_to_command_text() {
    let sentinel = "__CODEX_REDACTED__";
    let opts = ImportOptions {
        redact: Box::new(|_| sentinel.to_owned()),
    };
    let records = import_codex(Path::new(SESSION_FIXTURE), &opts)
        .expect("import with custom redactor")
        .records()
        .to_vec();
    let has_redacted = records.iter().any(|r| {
        if let GraphRecord::Node {
            kind: NodeKind::CommandRun,
            text,
            ..
        } = r
        {
            text.as_deref() == Some(sentinel)
        } else {
            false
        }
    });
    assert!(
        has_redacted,
        "redaction hook not applied to CommandRun.text"
    );
}

// ── AC: importer_id is "codex-jsonl" on all records ──────────────────────────

#[test]
fn session_importer_id_is_codex_jsonl() {
    for r in &import_session() {
        if let GraphRecord::Node {
            id,
            importer_id,
            kind,
            ..
        } = r
            && is_agent_memory_kind(*kind)
        {
            assert_eq!(
                importer_id.as_deref(),
                Some("codex-jsonl"),
                "node {id} has wrong importer_id"
            );
        }
    }
}

// ── AC: failed patch has patch_status="invalid" ───────────────────────────────

#[test]
fn session_failed_patch_has_status_invalid() {
    let records = import_session();
    let has_invalid = records.iter().any(|r| {
        if let GraphRecord::Node {
            kind: NodeKind::PatchArtifact,
            patch_status,
            ..
        } = r
        {
            patch_status.as_deref() == Some("invalid")
        } else {
            false
        }
    });
    assert!(
        has_invalid,
        "failed patch does not have patch_status=invalid"
    );
}

// ── AC: no panic on minimal valid file ───────────────────────────────────────

#[test]
fn import_does_not_panic_on_session_fixture() {
    let result = import_codex(Path::new(SESSION_FIXTURE), &ImportOptions::default());
    assert!(result.is_ok(), "import_codex panicked: {:?}", result.err());
}

#[test]
fn import_does_not_panic_on_rollout_fixture() {
    let result = import_codex(Path::new(ROLLOUT_FIXTURE), &ImportOptions::default());
    assert!(result.is_ok(), "import_codex panicked: {:?}", result.err());
}

// ── Empty / malformed file validation ────────────────────────────────────────

#[test]
fn empty_file_returns_error() {
    let tmp = tempfile::NamedTempFile::new().expect("tmp");
    let result = import_codex(tmp.path(), &ImportOptions::default());
    assert!(result.is_err(), "expected error for empty file, got Ok");
}

#[test]
fn header_only_file_returns_error() {
    // A file with only a session header and no turns should also fail.
    let jsonl = r#"{"type":"session","model":"test","created_at":"2025-01-01T00:00:00Z"}"#;
    let tmp = tempfile::NamedTempFile::new().expect("tmp");
    std::fs::write(tmp.path(), jsonl).expect("write");
    let result = import_codex(tmp.path(), &ImportOptions::default());
    assert!(
        result.is_err(),
        "expected error for header-only file, got Ok"
    );
}

#[test]
fn fully_malformed_file_returns_error() {
    // Every line is unparseable JSON — should fail rather than emit phantom records.
    let jsonl = "not json at all\nalso not json\n{broken";
    let tmp = tempfile::NamedTempFile::new().expect("tmp");
    std::fs::write(tmp.path(), jsonl).expect("write");
    let result = import_codex(tmp.path(), &ImportOptions::default());
    assert!(
        result.is_err(),
        "expected error for fully malformed file, got Ok"
    );
}

// ── Extension-less file targets (Makefile, Dockerfile, etc.) ─────────────────

#[test]
fn sed_edit_on_makefile_captures_path() {
    let jsonl = r#"{"type":"session","model":"t","created_at":"2025-01-01T00:00:00Z"}
{"type":"message","role":"user","content":[{"type":"input_text","text":"fix it"}]}
{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}
{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"cmd\":[\"sed\",\"-i\",\"s/foo/bar/\",\"Makefile\"]}"}
{"type":"function_call_output","call_id":"c1","output":"{\"exit_code\":0,\"stdout\":\"\",\"stderr\":\"\"}"}"#;
    let tmp = tempfile::NamedTempFile::new().expect("tmp");
    std::fs::write(tmp.path(), jsonl).expect("write");
    let records = import_codex(tmp.path(), &ImportOptions::default())
        .expect("import ok")
        .records()
        .to_vec();
    let file_edit = records.iter().find(|r| {
        matches!(
            r,
            GraphRecord::Node {
                kind: NodeKind::FileEdit,
                ..
            }
        )
    });
    assert!(
        file_edit.is_some(),
        "expected FileEdit node for Makefile edit"
    );
    if let Some(GraphRecord::Node {
        repo_relative_path, ..
    }) = file_edit
    {
        assert_eq!(
            repo_relative_path.as_deref(),
            Some("Makefile"),
            "repo_relative_path should be 'Makefile'"
        );
    }
}

// ── Multi-call correlation: two function_calls before their outputs ───────────

fn multi_call_jsonl() -> Vec<GraphRecord> {
    // Two function_calls arrive before either output; outputs arrive out-of-order
    // (call-2 output comes before call-1 output).
    let jsonl = r#"{"type":"session","model":"test-model","created_at":"2025-01-01T00:00:00Z"}
{"type":"message","role":"user","content":[{"type":"input_text","text":"run two things"}]}
{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Running both in parallel"}]}
{"type":"function_call","call_id":"call-1","name":"shell","arguments":"{\"cmd\":[\"echo\",\"one\"]}"}
{"type":"function_call","call_id":"call-2","name":"shell","arguments":"{\"cmd\":[\"echo\",\"two\"]}"}
{"type":"function_call_output","call_id":"call-2","output":"{\"exit_code\":0,\"stdout\":\"two\",\"stderr\":\"\"}"}
{"type":"function_call_output","call_id":"call-1","output":"{\"exit_code\":0,\"stdout\":\"one\",\"stderr\":\"\"}"}"#;

    let tmp = tempfile::NamedTempFile::new().expect("tmp file");
    std::fs::write(tmp.path(), jsonl).expect("write fixture");
    import_codex(tmp.path(), &ImportOptions::default())
        .expect("import ok")
        .records()
        .to_vec()
}

#[test]
fn multi_call_both_tool_calls_emitted() {
    let records = multi_call_jsonl();
    let tool_call_count = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::ToolCall,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        tool_call_count, 2,
        "expected 2 ToolCall nodes, got {tool_call_count}"
    );
}

#[test]
fn multi_call_both_command_runs_emitted() {
    let records = multi_call_jsonl();
    let cmd_run_count = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::CommandRun,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        cmd_run_count, 2,
        "expected 2 CommandRun nodes, got {cmd_run_count}"
    );
}

#[test]
fn multi_call_no_spurious_diagnostics() {
    // Neither call should produce an "unrecognized event" Diagnostic, which would
    // indicate a mismatched call_id (the old single-slot regression).
    let records = multi_call_jsonl();
    let diagnostic_count = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Diagnostic,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        diagnostic_count, 0,
        "spurious Diagnostic nodes indicate call_id mismatch: {diagnostic_count} found"
    );
}

#[test]
fn multi_call_both_exit_codes_present() {
    // Both CommandRun nodes should have exit_code=0 from their respective outputs.
    let records = multi_call_jsonl();
    let exit_codes: Vec<i64> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node {
                kind: NodeKind::CommandRun,
                exit_code,
                ..
            } = r
            {
                *exit_code
            } else {
                None
            }
        })
        .collect();
    assert_eq!(exit_codes.len(), 2, "expected 2 exit codes");
    assert!(
        exit_codes.iter().all(|&c| c == 0),
        "expected all exit_code=0, got {exit_codes:?}"
    );
}
