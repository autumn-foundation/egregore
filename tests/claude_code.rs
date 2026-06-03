//! Integration tests for the Claude Code transcript JSONL importer.

use aletheia_egregore::claude_code::{
    DOMAIN, IMPORTER_ID, IMPORTER_VERSION, ImportOptions, import_claude_code,
};
use aletheia_egregore::ir::{GraphRecord, NodeKind};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent_memory")
        .join(name)
        .join("transcript.jsonl")
}

fn node_kinds(records: &[GraphRecord]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node { kind, .. } = r {
                Some(kind.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn nodes_of_kind<'a>(records: &'a [GraphRecord], target: &str) -> Vec<&'a GraphRecord> {
    records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node { kind, .. } = r {
                kind.as_str() == target
            } else {
                false
            }
        })
        .collect()
}

// ── Basic session import ───────────────────────────────────────────────────────

#[test]
fn basic_session_import_produces_expected_record_kinds() {
    let path = fixture("claude_code_session");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let kinds = node_kinds(records);

    // Must contain at least one of each core record type.
    assert!(
        kinds.contains(&"AgentSession"),
        "expected AgentSession; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"AgentRun"),
        "expected AgentRun; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"AgentTurn"),
        "expected AgentTurn; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"ToolCall"),
        "expected ToolCall; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"CommandRun"),
        "expected CommandRun; got: {kinds:?}"
    );
}

#[test]
fn basic_session_has_file_edit_record() {
    let path = fixture("claude_code_session");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let file_edits = nodes_of_kind(records, "FileEdit");
    assert!(
        !file_edits.is_empty(),
        "expected at least one FileEdit; got none"
    );

    // The FileEdit should reference calc.py.
    let has_calc = file_edits.iter().any(|r| {
        if let GraphRecord::Node {
            repo_relative_path: Some(p),
            ..
        } = r
        {
            p.as_str().contains("calc.py")
        } else {
            false
        }
    });
    assert!(has_calc, "expected FileEdit for calc.py");
}

#[test]
fn basic_session_has_verification_for_test_bash() {
    let path = fixture("claude_code_session");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let verifications = nodes_of_kind(records, "Verification");
    assert!(
        !verifications.is_empty(),
        "expected at least one Verification for cargo test success"
    );
}

// ── Record metadata fields ─────────────────────────────────────────────────────

#[test]
fn records_carry_correct_importer_metadata() {
    let path = fixture("claude_code_session");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");

    for record in graph.records() {
        if let GraphRecord::Node {
            importer_id,
            importer_version,
            domain,
            source_artifact_hash,
            ..
        } = record
        {
            assert_eq!(
                importer_id.as_deref(),
                Some(IMPORTER_ID),
                "wrong importer_id"
            );
            assert_eq!(
                importer_version.as_deref(),
                Some(IMPORTER_VERSION),
                "wrong importer_version"
            );
            assert_eq!(domain.as_deref(), Some(DOMAIN), "wrong domain");
            assert!(
                source_artifact_hash.is_some(),
                "source_artifact_hash must be present"
            );
        }
    }
}

#[test]
fn all_nodes_have_non_empty_source_artifact_hash() {
    let path = fixture("claude_code_session");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");

    for record in graph.records() {
        if let GraphRecord::Node {
            source_artifact_hash,
            ..
        } = record
        {
            let hash = source_artifact_hash
                .as_deref()
                .expect("source_artifact_hash must be Some");
            assert!(!hash.is_empty(), "source_artifact_hash must be non-empty");
            // BLAKE3 hex is 64 chars.
            assert_eq!(hash.len(), 64, "BLAKE3 hex should be 64 chars");
        }
    }
}

// ── Verification trust boundary ────────────────────────────────────────────────

#[test]
fn prose_alone_does_not_produce_verification() {
    // The last assistant turn is a text-only "Tests pass. The edit is complete."
    // This must NOT produce a Verification record.
    // We count Verification records: they may only come from Bash+test commands.
    let path = fixture("claude_code_session");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let verifications = nodes_of_kind(records, "Verification");
    // We expect exactly one Verification (from the cargo test --all Bash call).
    // The prose "Tests pass." in the final assistant turn must not add more.
    assert_eq!(
        verifications.len(),
        1,
        "expected exactly 1 Verification (from Bash test), got {}",
        verifications.len()
    );
}

// ── Hook events → Diagnostic ──────────────────────────────────────────────────

#[test]
fn hook_events_produce_diagnostic_records() {
    let path = fixture("claude_code_hook_rich");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let diagnostics = nodes_of_kind(records, "Diagnostic");
    assert!(
        !diagnostics.is_empty(),
        "expected Diagnostic records from hook events"
    );

    // Fixture has 3 hooks: PreToolUse, PostToolUse, Stop.
    // Each should produce at least one Diagnostic.
    let hook_diag_count = diagnostics
        .iter()
        .filter(|r| {
            if let GraphRecord::Node { summary, .. } = r {
                summary.starts_with("Hook ")
            } else {
                false
            }
        })
        .count();
    assert_eq!(hook_diag_count, 3, "expected 3 Hook Diagnostic records");
}

// ── Failure on git push error ─────────────────────────────────────────────────

#[test]
fn bash_error_produces_failure_record() {
    let path = fixture("claude_code_hook_rich");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let failures = nodes_of_kind(records, "Failure");
    assert!(
        !failures.is_empty(),
        "expected Failure record for git push error"
    );
}

// ── Malformed JSON lines → Diagnostic ─────────────────────────────────────────

#[test]
fn malformed_json_lines_produce_diagnostic() {
    use std::io::Write;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("malformed.jsonl");

    {
        let mut f = std::fs::File::create(&path).expect("create file");
        // One valid user event followed by two malformed lines.
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"timestamp":"2025-01-01T00:00:00Z","session_id":"s1"}}"#
        )
        .unwrap();
        writeln!(f, "not valid json at all!!!").unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"hello"}}]}},"usage":{{"input_tokens":10,"output_tokens":5}},"timestamp":"2025-01-01T00:00:01Z","session_id":"s1"}}"#
        )
        .unwrap();
        writeln!(f, "{{{{broken").unwrap();
    }

    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let diagnostics = nodes_of_kind(records, "Diagnostic");
    let malformed_diag = diagnostics.iter().any(|r| {
        if let GraphRecord::Node { summary, .. } = r {
            summary.contains("malformed")
        } else {
            false
        }
    });
    assert!(malformed_diag, "expected malformed-lines Diagnostic");
}

// ── Empty file errors ──────────────────────────────────────────────────────────

#[test]
fn empty_file_returns_empty_import_error() {
    use std::io::Write;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("empty.jsonl");
    std::fs::File::create(&path)
        .expect("create file")
        .write_all(b"")
        .unwrap();

    let opts = ImportOptions::passthrough();
    let result = import_claude_code(&path, &opts);
    assert!(result.is_err(), "empty file must return an error");
}

#[test]
fn whitespace_only_file_returns_empty_import_error() {
    use std::io::Write;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("whitespace.jsonl");
    std::fs::File::create(&path)
        .expect("create file")
        .write_all(b"   \n\n   \n")
        .unwrap();

    let opts = ImportOptions::passthrough();
    let result = import_claude_code(&path, &opts);
    assert!(result.is_err(), "whitespace-only file must return an error");
}

// ── Hook-rich fixture: Verification from test bash ────────────────────────────

#[test]
fn hook_rich_fixture_has_verification_for_test_bash() {
    let path = fixture("claude_code_hook_rich");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let verifications = nodes_of_kind(records, "Verification");
    assert!(
        !verifications.is_empty(),
        "expected Verification for cargo test success in hook-rich fixture"
    );
}

// ── Source artifact hash stability ────────────────────────────────────────────

#[test]
fn source_artifact_hash_is_stable_across_imports() {
    let path = fixture("claude_code_session");
    let opts1 = ImportOptions::passthrough();
    let opts2 = ImportOptions::passthrough();
    let graph1 = import_claude_code(&path, &opts1).expect("first import");
    let graph2 = import_claude_code(&path, &opts2).expect("second import");

    let hash1 = graph1
        .records()
        .iter()
        .find_map(|r| {
            if let GraphRecord::Node {
                kind,
                source_artifact_hash,
                ..
            } = r
            {
                if *kind == NodeKind::AgentSession {
                    source_artifact_hash.clone()
                } else {
                    None
                }
            } else {
                None
            }
        })
        .expect("AgentSession node must exist");

    let hash2 = graph2
        .records()
        .iter()
        .find_map(|r| {
            if let GraphRecord::Node {
                kind,
                source_artifact_hash,
                ..
            } = r
            {
                if *kind == NodeKind::AgentSession {
                    source_artifact_hash.clone()
                } else {
                    None
                }
            } else {
                None
            }
        })
        .expect("AgentSession node must exist");

    assert_eq!(hash1, hash2, "source_artifact_hash must be stable");
}

// ── Agent subagent tool → "other" tool kind ───────────────────────────────────

#[test]
fn agent_subagent_tool_gets_other_kind() {
    let path = fixture("claude_code_hook_rich");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let has_agent_tool_call = records.iter().any(|r| {
        if let GraphRecord::Node {
            kind,
            tool_name,
            tool_kind,
            ..
        } = r
        {
            *kind == NodeKind::ToolCall
                && tool_name.as_deref() == Some("Agent")
                && tool_kind.as_deref() == Some("other")
        } else {
            false
        }
    });

    assert!(
        has_agent_tool_call,
        "expected at least one ToolCall with tool_name=Agent and tool_kind=other"
    );
}

// ── CostUsage records ─────────────────────────────────────────────────────────

#[test]
fn cost_usage_emitted_for_turns_with_tokens() {
    let path = fixture("claude_code_session");
    let opts = ImportOptions::passthrough();
    let graph = import_claude_code(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let cost_usage = nodes_of_kind(records, "CostUsage");
    assert!(
        !cost_usage.is_empty(),
        "expected CostUsage records for turns with non-zero token counts"
    );
}
