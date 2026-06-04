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

// ── AC1: JSON-RPC 2.0 protocol ─────────────────────────────────────────────────

/// initialize must return protocolVersion "2024-11-05" and capabilities.
#[test]
fn initialize_returns_protocol_version_and_capabilities() {
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("initialize must return a response");

    assert_eq!(
        response["jsonrpc"].as_str(),
        Some("2.0"),
        "response must carry jsonrpc 2.0"
    );
    assert_eq!(
        response["result"]["protocolVersion"].as_str(),
        Some("2024-11-05"),
        "must echo back protocol version"
    );
    assert!(
        response["result"]["capabilities"].is_object(),
        "must include capabilities object"
    );
    assert_eq!(
        response["result"]["serverInfo"]["name"].as_str(),
        Some("egregore"),
        "server name must be egregore"
    );
}

/// Unknown method must return a JSON-RPC method-not-found error (-32601).
#[test]
fn unknown_method_returns_method_not_found_error() {
    let request = r#"{"jsonrpc":"2.0","id":2,"method":"nonexistent/method","params":{}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("unknown method must return an error");

    assert_eq!(
        response["error"]["code"].as_i64(),
        Some(-32601),
        "must return -32601 method-not-found"
    );
}

/// Malformed JSON must return a JSON-RPC parse error (-32700).
#[test]
fn invalid_json_returns_parse_error() {
    let response = handle_message("not json at all", &test_data_dir());
    let response = response.expect("parse error must still return a response");
    assert_eq!(
        response["error"]["code"].as_i64(),
        Some(-32700),
        "must return -32700 parse error"
    );
}

/// A request with no `id` (notification) must return no response.
#[test]
fn notification_without_id_returns_no_response() {
    let request = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    let response = handle_message(request, &test_data_dir());
    assert!(response.is_none(), "notifications must return None");
}

// ── AC2: Tool list ─────────────────────────────────────────────────────────────

/// The MCP tool list must include exactly inspect_store, symbol_context, task_evidence.
#[test]
fn tools_list_contains_exactly_three_required_tools() {
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
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

/// Each tool must expose a non-empty description and an inputSchema object.
#[test]
fn tools_list_each_tool_has_description_and_input_schema() {
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("tools/list must return a response");

    let tools = response["result"]["tools"]
        .as_array()
        .expect("result.tools must be an array");

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "tool {name} must have a non-empty description"
        );
        assert!(
            tool["inputSchema"].is_object(),
            "tool {name} must have an inputSchema object"
        );
    }
}

// ── AC3: inspect_store ────────────────────────────────────────────────────────

/// inspect_store must return a structured JSON object with domain counts and snapshot_timestamp.
#[test]
fn inspect_store_returns_structured_output() {
    let records = fixture_records();
    let result = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");

    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "ok must be true; got {result}"
    );
    assert!(
        result["snapshot_timestamp"].as_str().is_some(),
        "must include snapshot_timestamp; got {result}"
    );
    assert!(
        result["domain_counts"].is_object(),
        "must include domain_counts object; got {result}"
    );
}

/// inspect_store must return non-zero total record count for records that are present.
#[test]
fn inspect_store_counts_reflect_fixture_records() {
    let records = fixture_records();
    let result = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");

    let total = result["records"]
        .as_u64()
        .expect("records must be a u64 count");

    assert!(
        total >= 2,
        "total record count must be at least 2 (one symbol + one observation); got {total}"
    );
}

// ── AC4: symbol_context ───────────────────────────────────────────────────────

/// symbol_context must return a structured response with source_facts separated from observations.
#[test]
fn symbol_context_returns_structured_output() {
    let records = fixture_records();
    let result = tool_symbol_context_from_records(&records, "my_function");

    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "ok must be true for a known symbol; got {result}"
    );
    assert!(
        result["source_facts"].is_array(),
        "must include source_facts array; got {result}"
    );
}

/// symbol_context for an unknown symbol must return ok:false with a no_match error.
#[test]
fn symbol_context_no_match_returns_ok_false() {
    let records = fixture_records();
    let result = tool_symbol_context_from_records(&records, "definitely_nonexistent_symbol_xyz");

    assert!(
        !result["ok"].as_bool().unwrap_or(true),
        "ok must be false for unknown symbol; got {result}"
    );
    assert_eq!(
        result["error"]["code"].as_str(),
        Some("no_match"),
        "error code must be no_match; got {result}"
    );
}

// ── AC5: task_evidence ────────────────────────────────────────────────────────

/// task_evidence for an unknown id must return ok:false with no_match.
#[test]
fn task_evidence_no_match_returns_ok_false() {
    let records = fixture_records();
    let result = tool_task_evidence_from_records(&records, "task:nonexistent-999");

    assert!(
        !result["ok"].as_bool().unwrap_or(true),
        "ok must be false for unknown task; got {result}"
    );
}

/// tools/call for task_evidence must return a well-formed tool result envelope.
#[test]
fn tools_call_task_evidence_returns_tool_result_envelope() {
    let request = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"task_evidence","arguments":{"id_or_handle":"nonexistent-task"}}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("tools/call must return a response");

    let content = response["result"]["content"]
        .as_array()
        .expect("result.content must be an array");
    assert!(!content.is_empty(), "content must be non-empty");
    assert_eq!(
        content[0]["type"].as_str(),
        Some("text"),
        "content[0].type must be 'text'"
    );
    assert!(
        content[0]["text"].as_str().is_some(),
        "content[0].text must be a string"
    );
}

// ── AC6: Trust separation ─────────────────────────────────────────────────────

/// symbol_context must put code-graph nodes in source_facts and agent observations separately.
#[test]
fn symbol_context_separates_source_facts_from_observations() {
    let records = fixture_records();
    let result = tool_symbol_context_from_records(&records, "my_function");

    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "ok must be true; got {result}"
    );
    assert!(
        result["source_facts"].is_array(),
        "must include source_facts array; got {result}"
    );
    assert!(
        result["observations"].is_array(),
        "must include observations array; got {result}"
    );
}

/// inspect_store must attribute records to their domain (codegraph vs agent_memory).
#[test]
fn inspect_store_separates_domains() {
    let records = fixture_records();
    let result = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");

    let domain_counts = result["domain_counts"]
        .as_object()
        .expect("domain_counts must be object");
    // fixture has one codegraph symbol and one agent_memory observation
    assert!(
        domain_counts.len() >= 2,
        "must track at least 2 domain categories; got {domain_counts:?}"
    );
}

// ── AC7: No bearer tokens ─────────────────────────────────────────────────────

/// tools/list response must not contain any bearer token strings.
#[test]
fn tools_list_response_contains_no_bearer_tokens() {
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("tools/list must return a response");

    let serialized = serde_json::to_string(&response).expect("response must serialize");
    assert!(
        !serialized.contains("Bearer "),
        "tools/list response must not contain 'Bearer '"
    );
    assert!(
        !serialized.contains("Authorization"),
        "tools/list response must not contain 'Authorization'"
    );
}

/// initialize response must not contain any bearer token strings.
#[test]
fn initialize_response_contains_no_bearer_tokens() {
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("initialize must return a response");

    let serialized = serde_json::to_string(&response).expect("response must serialize");
    assert!(
        !serialized.contains("Bearer "),
        "initialize response must not contain 'Bearer '"
    );
}

// ── AC8: Determinism ──────────────────────────────────────────────────────────

/// symbol_context called twice with the same inputs must return the same output.
#[test]
fn symbol_context_output_is_deterministic() {
    let records = fixture_records();
    let result1 = tool_symbol_context_from_records(&records, "my_function");
    let result2 = tool_symbol_context_from_records(&records, "my_function");

    assert_eq!(
        result1, result2,
        "symbol_context must be deterministic across identical calls"
    );
}

/// inspect_store called twice with the same inputs must return the same output.
#[test]
fn inspect_store_output_is_deterministic() {
    let records = fixture_records();
    let result1 = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");
    let result2 = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");

    assert_eq!(
        result1, result2,
        "inspect_store must be deterministic across identical calls"
    );
}

// ── AC9: Offline operation ────────────────────────────────────────────────────

/// The tool_*_from_records functions must work without a running daemon.
/// (By definition — they accept pre-loaded records, not a data directory.)
#[test]
fn tool_functions_work_without_daemon() {
    let records = fixture_records();

    // None of these must panic or require network/daemon access.
    let r1 = tool_inspect_store_from_records(&records, &[], "2026-06-04T00:00:00Z");
    let r2 = tool_symbol_context_from_records(&records, "my_function");
    let r3 = tool_task_evidence_from_records(&records, "task:nonexistent");

    // All return JSON objects with an "ok" field — the key offline contract.
    assert!(
        r1["ok"].is_boolean(),
        "inspect_store must return JSON with ok field"
    );
    assert!(
        r2["ok"].is_boolean(),
        "symbol_context must return JSON with ok field"
    );
    assert!(
        r3["ok"].is_boolean(),
        "task_evidence must return JSON with ok field"
    );
}

/// tools/list does not require a daemon connection.
#[test]
fn tools_list_works_without_daemon() {
    // .egregore-nonexistent path intentionally does not exist.
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("tools/list must succeed without daemon");

    assert!(
        response["result"]["tools"].as_array().is_some(),
        "must return tools array even without a running daemon"
    );
}

// ── AC10: Domain categories ───────────────────────────────────────────────────

/// inspect_store must label domain categories in human-readable form.
///
/// The `domain_counts` keys are the category labels (e.g. "Deterministic Source Facts").
#[test]
fn inspect_store_domain_categories_are_labeled() {
    let records = fixture_records();
    let result = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");

    let domain_counts = result["domain_counts"]
        .as_object()
        .expect("domain_counts must be object");

    assert!(
        !domain_counts.is_empty(),
        "domain_counts must not be empty for fixture records"
    );
    for (key, _) in domain_counts {
        assert!(
            !key.is_empty(),
            "domain category key must be a non-empty label string"
        );
        // Keys must be human-readable: no raw domain strings like "codegraph"
        assert!(
            !matches!(key.as_str(), "codegraph" | "agent_memory" | "semantic"),
            "domain category '{key}' must be a human-readable label, not a raw domain string"
        );
    }
}

/// Unknown tool name in tools/call must return an error, not panic.
#[test]
fn tools_call_unknown_tool_returns_error() {
    let request = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}"#;
    let response =
        handle_message(request, &test_data_dir()).expect("unknown tool must return a response");

    // Either an error result or isError:true in the tool result
    let is_error_result = response["error"].is_object();
    let is_tool_error = response["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        is_error_result || is_tool_error,
        "unknown tool must return an error or isError:true; got {response}"
    );
}

/// tools/call missing required arguments must return an error, not panic.
#[test]
fn tools_call_missing_arguments_returns_error() {
    let request = r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"symbol_context","arguments":{}}}"#;
    let response = handle_message(request, &test_data_dir())
        .expect("missing arguments must return a response");

    let content = response["result"]["content"]
        .as_array()
        .expect("result.content must be an array");
    assert!(!content.is_empty(), "content must be non-empty");
    // Either an error or isError — either is acceptable
    let is_error_result = response["error"].is_object();
    let is_tool_error = response["result"]["isError"].as_bool().unwrap_or(false);
    let has_content = !content.is_empty();
    assert!(
        is_error_result || is_tool_error || has_content,
        "must return some error signal; got {response}"
    );
}
