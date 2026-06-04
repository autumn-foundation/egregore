//! MCP server tests — issue #53.
//!
//! RED phase: these tests are written against the not-yet-implemented public API
//! of `aletheia_egregore::mcp`. All tests in this file are expected to fail
//! until the GREEN phase implementation is complete.

#![allow(missing_docs)]
#![allow(clippy::doc_markdown)]
#![cfg(feature = "embedded-aletheiadb")]

use aletheia_egregore::{
    EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::AGENT_MEMORY_SCHEMA_VERSION,
    mcp::{
        handle_message, tool_inspect_store_from_records, tool_symbol_context_from_records,
        tool_task_evidence_from_records,
    },
};

// ── Fixtures ──────────────────────────────────────────────────────────────────

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
    }
}

fn make_symbol(id: &str, name: &str, path: &str) -> GraphRecord {
    GraphRecord::symbol(
        id.to_owned(),
        "function",
        path.to_owned(),
        span(1, 10),
        name.to_owned(),
        format!("Rust function {name}"),
    )
    .with_valid_time_inferred("2026-01-01T00:00:00Z")
}

fn make_observation(id: &str, text: &str, target_id: &str) -> GraphRecord {
    let mut obs = GraphRecord::node(
        id.to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        text.to_owned(),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut agent_id,
        ref mut agent_kind,
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut source_handle,
        text: ref mut text_field,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("test-agent".to_owned());
        *agent_kind = Some("other".to_owned());
        *session_id = Some("sess-001".to_owned());
        *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
        *source_handle = Some("src/lib.rs:sha256:abc".to_owned());
        *text_field = Some(text.to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(target_id.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "OBSERVES".to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }
    obs
}

fn fixture_records() -> Vec<GraphRecord> {
    let sym_id = "codegraph:v4:sym001";
    vec![
        make_symbol(sym_id, "my_function", "src/lib.rs"),
        make_observation(
            "agent_memory:v1:obs001",
            "my_function has high complexity",
            sym_id,
        ),
    ]
}

fn test_data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(".egregore-nonexistent-fixture-mcp-test")
}

// ── AC2: Tool list ─────────────────────────────────────────────────────────────

/// The MCP tool list must include exactly inspect_store, symbol_context, task_evidence.
#[test]
fn tools_list_contains_exactly_three_required_tools() {
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'"#;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'"#;
    let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}";
    let response =
        handle_message(request, &test_data_dir()).expect("tools/list must return a response");

    let tools = response["result"]["tools"]
        .as_array()
        .expect("result.tools must be an array");

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    assert!(
        names.contains(&"inspect_store"),
        "must include inspect_store; got {names:?}"
    );
    assert!(
        names.contains(&"symbol_context"),
        "must include symbol_context; got {names:?}"
    );
    assert!(
        names.contains(&"task_evidence"),
        "must include task_evidence; got {names:?}"
    );
    assert_eq!(names.len(), 3, "must have exactly 3 tools; got {names:?}");
}
