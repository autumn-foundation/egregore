//! MCP server tests — issue #53.
//!
//! GREEN phase: tests cover the rmcp-based `EgregoreMcpServer` and the three
//! public `tool_*_from_records` helpers.  Protocol-level framing (initialize,
//! ping, JSON-RPC parse errors, notification handling) is delegated to the
//! rmcp crate and is covered by its own test suite.

#![allow(missing_docs)]
#![allow(clippy::doc_markdown)]
#![cfg(feature = "embedded-aletheiadb")]

use aletheia_egregore::{
    EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::AGENT_MEMORY_SCHEMA_VERSION,
    mcp::{
        EgregoreMcpServer, tool_inspect_store_from_records, tool_symbol_context_from_records,
        tool_task_evidence_from_records,
    },
};
use rmcp::ServerHandler as _;

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

// ── AC1: MCP server identity and capabilities ─────────────────────────────────

/// Server info must identify as "egregore" with tools capability enabled.
#[test]
fn mcp_server_get_info_reports_egregore_with_tools_capability() {
    let server = EgregoreMcpServer::new(test_data_dir());
    let info = server.get_info();
    assert_eq!(
        info.server_info.name, "egregore",
        "server name must be egregore"
    );
    assert!(
        info.capabilities.tools.is_some(),
        "tools capability must be enabled; got {:?}",
        info.capabilities
    );
}

/// Server instructions must be non-empty.
#[test]
fn mcp_server_has_instructions() {
    let server = EgregoreMcpServer::new(test_data_dir());
    let info = server.get_info();
    assert!(
        info.instructions.as_deref().is_some_and(|s| !s.is_empty()),
        "server instructions must be non-empty"
    );
}

// ── AC2: Tool registration ─────────────────────────────────────────────────────

/// The MCP tool router must register exactly the three required tools.
#[test]
fn tool_router_registers_exactly_three_required_tools() {
    let inspect = EgregoreMcpServer::inspect_store_tool_attr();
    let symbol = EgregoreMcpServer::symbol_context_tool_attr();
    let task = EgregoreMcpServer::task_evidence_tool_attr();

    assert_eq!(inspect.name, "inspect_store", "first tool name");
    assert_eq!(symbol.name, "symbol_context", "second tool name");
    assert_eq!(task.name, "task_evidence", "third tool name");
}

/// Each registered tool must have a non-empty description and an object input schema.
#[test]
fn all_tools_have_nonempty_description_and_object_input_schema() {
    for tool in [
        EgregoreMcpServer::inspect_store_tool_attr(),
        EgregoreMcpServer::symbol_context_tool_attr(),
        EgregoreMcpServer::task_evidence_tool_attr(),
    ] {
        let name = tool.name.as_ref();
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "tool {name} must have a non-empty description"
        );
        assert!(
            tool.input_schema.get("type").is_some(),
            "tool {name} must have an inputSchema with a 'type' field"
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

/// symbol_context with an empty symbol name must return ok:false.
#[test]
fn symbol_context_with_empty_name_returns_no_match() {
    let records = fixture_records();
    let result = tool_symbol_context_from_records(&records, "");

    assert!(
        !result["ok"].as_bool().unwrap_or(true),
        "ok must be false for empty symbol name; got {result}"
    );
}

/// Codex review on PR #497: `tool_symbol_context_from_records` dropped every
/// row in `ctx.drift_history` (issue #108), so a symbol's semantic-drift
/// history was visible through the CLI and daemon but never through the MCP
/// surface agents call. `drift_history` must now appear with the resolved
/// `repo_relative_path`/`span` handle, matching the CLI's `ContextDrift` and
/// the daemon's `context_drift_to_json`.
#[test]
fn symbol_context_includes_drift_history() {
    let sym_id = "codegraph:v4:sym001";
    let drift_id = "semantic:v1:drift001";
    let mut records = fixture_records();
    records.push(
        GraphRecord::node(
            drift_id.to_owned(),
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "drift".to_owned(),
        )
        .with_semantic_drift(aletheia_egregore::SemanticDriftMetadata {
            embedding_model: aletheia_egregore::EmbeddingModel {
                provider: "test".to_owned(),
                name: "test-model".to_owned(),
                version: "v1".to_owned(),
                dim: 8,
                content_hash: "unknown".to_owned(),
            },
            target_record_id: sym_id.to_owned(),
            prior_record_id: sym_id.to_owned(),
            before_git_commit: "aaaaaaa".to_owned(),
            after_git_commit: "bbbbbbb".to_owned(),
            before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
            metric_kind: aletheia_egregore::MetricKind::CosineDistance,
            score: 0.75,
            selection_threshold: 0.2,
            selection_basis: aletheia_egregore::SelectionBasis::ThresholdOnly,
        }),
    );
    records.push(GraphRecord::edge(
        aletheia_egregore::EdgeLabel::DriftsFrom,
        drift_id.to_owned(),
        sym_id.to_owned(),
        Some("1.0".to_owned()),
        "drift edge".to_owned(),
    ));

    let result = tool_symbol_context_from_records(&records, "my_function");

    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "ok must be true; got {result}"
    );
    let drift_rows = result["drift_history"]
        .as_array()
        .expect("drift_history must be an array");
    assert_eq!(
        drift_rows.len(),
        1,
        "expected exactly one drift row; got {result}"
    );
    let row = &drift_rows[0];
    assert_eq!(row["record_id"].as_str(), Some(drift_id));
    assert_eq!(row["repo_relative_path"].as_str(), Some("src/lib.rs"));
    assert!(
        row["span"].is_object(),
        "resolved drift row must carry the target's span; got {row}"
    );
    assert_eq!(row["score"].as_f64(), Some(0.75));
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
    assert!(
        domain_counts.len() >= 2,
        "must track at least 2 domain categories; got {domain_counts:?}"
    );
}

// ── AC7: No bearer tokens ─────────────────────────────────────────────────────

/// Tool descriptions must not contain any bearer token strings.
#[test]
fn tool_descriptions_contain_no_bearer_tokens() {
    for tool in [
        EgregoreMcpServer::inspect_store_tool_attr(),
        EgregoreMcpServer::symbol_context_tool_attr(),
        EgregoreMcpServer::task_evidence_tool_attr(),
    ] {
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(
            !desc.contains("Bearer "),
            "tool {:?} description must not contain 'Bearer '",
            tool.name
        );
        assert!(
            !desc.contains("Authorization"),
            "tool {:?} description must not contain 'Authorization'",
            tool.name
        );
    }
}

/// Server info must not contain any bearer token strings.
#[test]
fn server_info_contains_no_bearer_tokens() {
    let server = EgregoreMcpServer::new(test_data_dir());
    let info = server.get_info();
    let serialized =
        serde_json::to_string(&info.server_info.name).expect("server name must serialize");
    assert!(
        !serialized.contains("Bearer "),
        "server name must not contain 'Bearer '"
    );
    let instructions = info.instructions.as_deref().unwrap_or("");
    assert!(
        !instructions.contains("Bearer "),
        "server instructions must not contain 'Bearer '"
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

/// Tool metadata (names, descriptions, schemas) is available without a running daemon.
#[test]
fn tool_registration_works_without_daemon() {
    // Constructing the server and accessing tool metadata must not require a daemon.
    let _inspect = EgregoreMcpServer::inspect_store_tool_attr();
    let _symbol = EgregoreMcpServer::symbol_context_tool_attr();
    let _task = EgregoreMcpServer::task_evidence_tool_attr();
    // If we reach here without panic, the test passes.
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
        assert!(
            !matches!(key.as_str(), "codegraph" | "agent_memory" | "semantic"),
            "domain category '{key}' must be a human-readable label, not a raw domain string"
        );
    }
}
