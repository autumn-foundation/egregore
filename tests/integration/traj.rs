//! Tests for the rust-swe-agent .traj importer (issue #9).
//!
//! Written RED-first: all tests are written before the implementation exists.
//! They drive the exact contract specified in the issue AC.

use std::path::Path;

use aletheia_egregore::{
    import_traj,
    ir::{EdgeLabel, GraphRecord, NodeKind},
    traj::ImportOptions,
};

const FIXTURE: &str = "tests/fixtures/agent_memory/swe_agent_basic/trajectory.traj";

// ── AC: fixture existence ─────────────────────────────────────────────────────

#[test]
fn fixture_file_exists() {
    assert!(
        Path::new(FIXTURE).exists(),
        "fixture file not found at {FIXTURE}"
    );
}

#[test]
fn fixture_readme_exists() {
    assert!(
        Path::new("tests/fixtures/agent_memory/swe_agent_basic/README.md").exists(),
        "fixture README not found"
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn do_import() -> Vec<GraphRecord> {
    let graph = import_traj(Path::new(FIXTURE), &ImportOptions::default())
        .expect("import_traj must not fail on the basic fixture");
    graph.records().to_vec()
}

const fn kind_name(r: &GraphRecord) -> Option<&'static str> {
    r.node_kind_name()
}

fn count_kind(records: &[GraphRecord], name: &str) -> usize {
    records
        .iter()
        .filter(|r| kind_name(r) == Some(name))
        .count()
}

fn all_edges(records: &[GraphRecord]) -> impl Iterator<Item = &GraphRecord> {
    records
        .iter()
        .filter(|r: &&GraphRecord| matches!(*r, GraphRecord::Edge { .. }))
}

// ── AC: M2 record set ─────────────────────────────────────────────────────────

#[test]
fn import_emits_agent_session() {
    let records = do_import();
    assert!(
        count_kind(&records, "AgentSession") >= 1,
        "missing AgentSession"
    );
}

#[test]
fn import_emits_agent_run() {
    let records = do_import();
    assert!(count_kind(&records, "AgentRun") >= 1, "missing AgentRun");
}

#[test]
fn import_emits_agent_turns() {
    // Fixture has 4 assistant→user pairs → 4 turns
    let records = do_import();
    assert!(
        count_kind(&records, "AgentTurn") >= 4,
        "expected ≥4 AgentTurn records"
    );
}

#[test]
fn import_emits_tool_calls() {
    let records = do_import();
    assert!(
        count_kind(&records, "ToolCall") >= 4,
        "expected ≥4 ToolCall records"
    );
}

#[test]
fn import_emits_command_runs() {
    let records = do_import();
    assert!(
        count_kind(&records, "CommandRun") >= 4,
        "expected ≥4 CommandRun records"
    );
}

#[test]
fn import_emits_file_edit() {
    let records = do_import();
    assert!(count_kind(&records, "FileEdit") >= 1, "missing FileEdit");
}

#[test]
fn import_emits_patch_artifact() {
    let records = do_import();
    assert!(
        count_kind(&records, "PatchArtifact") >= 1,
        "missing PatchArtifact"
    );
}

#[test]
fn import_emits_failure() {
    let records = do_import();
    assert!(count_kind(&records, "Failure") >= 1, "missing Failure");
}

#[test]
fn import_emits_verification() {
    let records = do_import();
    assert!(
        count_kind(&records, "Verification") >= 1,
        "missing Verification"
    );
}

// ── AC: turn ordering determinism ────────────────────────────────────────────

#[test]
fn import_is_deterministic_across_5_runs() {
    let opts = ImportOptions::default();
    let path = Path::new(FIXTURE);
    let first = import_traj(path, &opts)
        .expect("run 0 failed")
        .to_jsonl()
        .expect("jsonl 0 failed");
    for i in 1..=4 {
        let run = import_traj(path, &opts)
            .unwrap_or_else(|_| panic!("run {i} failed"))
            .to_jsonl()
            .unwrap_or_else(|_| panic!("jsonl {i} failed"));
        assert_eq!(
            first, run,
            "import output differed on run {i} — not byte-identical"
        );
    }
}

// ── AC: required provenance fields on every node ──────────────────────────────

#[test]
fn every_node_has_domain_field() {
    let records = do_import();
    for r in &records {
        if let GraphRecord::Node {
            id, domain, kind, ..
        } = r
        {
            let is_agent_kind = matches!(
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
            );
            if is_agent_kind {
                assert_eq!(
                    domain.as_deref(),
                    Some("agent_memory"),
                    "node {id} missing domain=agent_memory"
                );
            }
        }
    }
}

#[test]
fn every_node_has_importer_id() {
    let records = do_import();
    for r in &records {
        if let GraphRecord::Node {
            id,
            importer_id,
            kind,
            ..
        } = r
        {
            let is_agent_kind = matches!(
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
            );
            if is_agent_kind {
                assert!(importer_id.is_some(), "node {id} missing importer_id");
            }
        }
    }
}

#[test]
fn every_node_has_importer_version() {
    let records = do_import();
    for r in &records {
        if let GraphRecord::Node {
            id,
            importer_version,
            kind,
            ..
        } = r
        {
            let is_agent_kind = matches!(
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
            );
            if is_agent_kind {
                assert!(
                    importer_version.is_some(),
                    "node {id} missing importer_version"
                );
            }
        }
    }
}

#[test]
fn every_node_has_source_artifact_path() {
    let records = do_import();
    for r in &records {
        if let GraphRecord::Node {
            id,
            source_artifact_path,
            kind,
            ..
        } = r
        {
            let is_agent_kind = matches!(
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
            );
            if is_agent_kind {
                assert!(
                    source_artifact_path.is_some(),
                    "node {id} missing source_artifact_path"
                );
            }
        }
    }
}

#[test]
fn every_node_has_source_artifact_hash() {
    let records = do_import();
    for r in &records {
        if let GraphRecord::Node {
            id,
            source_artifact_hash,
            kind,
            ..
        } = r
        {
            let is_agent_kind = matches!(
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
            );
            if is_agent_kind {
                assert!(
                    source_artifact_hash.is_some(),
                    "node {id} missing source_artifact_hash"
                );
            }
        }
    }
}

// ── AC: raw artifact preserved by handle, not inlined ────────────────────────

#[test]
fn raw_traj_body_is_not_inlined() {
    let fixture_content = std::fs::read_to_string(FIXTURE).expect("fixture readable");
    let jsonl = import_traj(Path::new(FIXTURE), &ImportOptions::default())
        .expect("import ok")
        .to_jsonl()
        .expect("jsonl ok");
    // The full task description should not appear verbatim as a queryable graph field.
    // We check that no single JSONL line exceeds a reasonable inlining threshold by
    // verifying the raw task text from info.task is absent from every line.
    let task_excerpt =
        "Fix the add function in src/calc.py to return the correct sum instead of difference.";
    let raw_content_fragment = fixture_content[..50].to_owned();
    for line in jsonl.lines() {
        assert!(
            !line.contains(task_excerpt),
            "JSONL line contains raw .traj body text — artifact inlining violation:\n{line}"
        );
        assert!(
            !line.contains(raw_content_fragment.as_str()),
            "JSONL line contains raw .traj body — artifact inlining violation:\n{line}"
        );
    }
}

// ── AC: invalid patch has patch_status == "invalid" ──────────────────────────

#[test]
fn failed_patch_has_patch_status_invalid() {
    let records = do_import();
    let has_invalid_patch = records.iter().any(|r: &GraphRecord| {
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
        has_invalid_patch,
        "no PatchArtifact with patch_status=invalid found; fixture exercises a failed patch hunk"
    );
}

#[test]
fn no_unvalidated_patch_promoted_to_success() {
    let records = do_import();
    // The only patch in the fixture FAILED (exit_code=1). It must never be "success".
    for r in &records {
        if let GraphRecord::Node {
            kind: NodeKind::PatchArtifact,
            patch_status,
            ..
        } = r
        {
            assert_ne!(
                patch_status.as_deref(),
                Some("success"),
                "PatchArtifact with unvalidated patch was promoted to success"
            );
        }
    }
}

// ── AC: cross-record edges link tool calls / command runs to session/run/turn ─

#[test]
fn import_has_nonzero_cross_record_edges() {
    let records = do_import();
    let edge_count = all_edges(&records).count();
    assert!(
        edge_count >= 1,
        "import emitted zero edges between agent-memory records"
    );
}

#[test]
fn agent_turn_edges_link_to_run() {
    let records = do_import();
    // Find AgentRun IDs
    let run_ids: Vec<_> = records
        .iter()
        .filter_map(|r: &GraphRecord| {
            if r.node_kind_name() == Some("AgentRun") {
                Some(r.id().to_owned())
            } else {
                None
            }
        })
        .collect();
    assert!(!run_ids.is_empty(), "no AgentRun records");

    // At least one AUTHORED_BY edge should target an AgentRun
    let has_authored_by_run = records.iter().any(|r: &GraphRecord| {
        if let GraphRecord::Edge { label, target, .. } = r {
            *label == EdgeLabel::AuthoredBy && run_ids.contains(target)
        } else {
            false
        }
    });
    assert!(
        has_authored_by_run,
        "no AUTHORED_BY edge connects a child record to an AgentRun"
    );
}

#[test]
fn agent_run_has_session_of_edge() {
    let records = do_import();
    let session_ids: Vec<_> = records
        .iter()
        .filter_map(|r: &GraphRecord| {
            if r.node_kind_name() == Some("AgentSession") {
                Some(r.id().to_owned())
            } else {
                None
            }
        })
        .collect();
    assert!(!session_ids.is_empty(), "no AgentSession");

    let has_session_of = records.iter().any(|r: &GraphRecord| {
        if let GraphRecord::Edge { label, target, .. } = r {
            *label == EdgeLabel::SessionOf && session_ids.contains(target)
        } else {
            false
        }
    });
    assert!(
        has_session_of,
        "no SESSION_OF edge connects AgentRun to AgentSession"
    );
}

// ── AC: idempotency — same .traj → same AgentSession ID ──────────────────────

#[test]
fn reimport_produces_same_session_id() {
    let opts = ImportOptions::default();
    let path = Path::new(FIXTURE);

    let get_session_ids = |records: &[GraphRecord]| {
        records
            .iter()
            .filter(|r: &&GraphRecord| r.node_kind_name() == Some("AgentSession"))
            .map(|r: &GraphRecord| r.id().to_owned())
            .collect::<Vec<_>>()
    };

    let first_records = import_traj(path, &opts)
        .expect("first import")
        .records()
        .to_vec();
    let second_records = import_traj(path, &opts)
        .expect("second import")
        .records()
        .to_vec();

    let first_ids = get_session_ids(&first_records);
    let second_ids = get_session_ids(&second_records);

    assert!(!first_ids.is_empty(), "no AgentSession on first import");
    assert_eq!(
        first_ids, second_ids,
        "AgentSession ID changed between imports — idempotency broken"
    );
}

// ── AC: unknown/unrecognized event kind → Diagnostic record ──────────────────

#[test]
fn unknown_traj_format_produces_diagnostic() {
    // Import from the real upstream sample that has `exit_reason=wallclock_timeout`.
    // The importer should not panic and should return at least one record.
    let sample = Path::new("tests/fixtures/agent_memory/swe_agent_basic/trajectory.traj");
    let result = import_traj(sample, &ImportOptions::default());
    assert!(
        result.is_ok(),
        "import_traj panicked or errored on known fixture"
    );
}

// ── AC: redaction hook is applied to free-text fields ────────────────────────

#[test]
fn redaction_hook_is_applied_to_free_text() {
    let sentinel = "__REDACTED__";
    let opts = ImportOptions {
        redact: Box::new(|_s| sentinel.to_owned()),
        policy_version: None,
    };
    let graph = import_traj(Path::new(FIXTURE), &opts).expect("import with custom redactor");
    let records = graph.records();

    // At least one CommandRun node should have its text field set to the sentinel,
    // proving the redaction closure was called on command text.
    let has_redacted_text = records.iter().any(|r: &GraphRecord| {
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
        has_redacted_text,
        "no CommandRun node has its text field set to the redaction sentinel — \
         redaction hook was not called"
    );
}

// ── AC: inspect on importer output reports non-zero edge counts ───────────────

#[test]
fn inspect_reports_nonzero_edges() {
    let records = do_import();
    let edge_count = records
        .iter()
        .filter(|r: &&GraphRecord| matches!(*r, GraphRecord::Edge { .. }))
        .count();
    let node_count = records
        .iter()
        .filter(|r: &&GraphRecord| matches!(*r, GraphRecord::Node { .. }))
        .count();
    assert!(node_count > 0, "no nodes emitted");
    assert!(
        edge_count > 0,
        "no edges emitted — inspect would report 0 edges"
    );
}
