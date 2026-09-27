//! MCP tool I/O contract freeze — issue #194.
//!
//! RED phase: these tests pin the frozen contract. They fail (compile-time,
//! missing `mcp_contract` module) until the contract module publishes the
//! per-tool JSON Schemas and the server advertises `mcp_contract_version`.
//!
//! Conformance rule: 100% of `eg mcp` tool success responses validate against
//! their published per-tool schema, and `mcp_contract_version` changes only
//! when a documented breaking-change class is introduced (see
//! `docs/schema/mcp.md`).

#![allow(missing_docs)]
#![allow(clippy::doc_markdown)]
#![cfg(feature = "embedded-aletheiadb")]

use std::path::Path;

use aletheia_egregore::{
    EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{AGENT_MEMORY_SCHEMA_VERSION, SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION},
    mcp::{
        EgregoreMcpServer, SymbolContextArgs, missing_argument_error, stamp_freshness_on_payload,
        tool_failure_history_from_records, tool_freshness_stamp, tool_inspect_store_from_records,
        tool_search_code_from_verb_result, tool_symbol_context_from_records,
        tool_task_evidence_from_records,
    },
    mcp_contract::{MCP_CONTRACT_TOOLS, MCP_CONTRACT_VERSION, error_schema, response_schema},
};
use rmcp::ServerHandler as _;
use rmcp::handler::server::wrapper::Parameters;
use serde_json::Value;

// ── Fixtures ──────────────────────────────────────────────────────────────────

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
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

/// Canonical task ID accepted by `resolve_task_ids` (`project:v1:<64 hex>`).
const TASK_ID: &str = "project:v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn make_task() -> GraphRecord {
    GraphRecord::node(
        TASK_ID.to_owned(),
        NodeKind::Task,
        None,
        None,
        Some("Freeze the MCP contract".to_owned()),
        "Freeze and version the MCP tool I/O contract".to_owned(),
    )
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
        ref mut session_id,
        ref mut observed_at,
        ref mut confidence,
        ref mut evidence_links,
        ..
    } = obs
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("test-agent".to_owned());
        *session_id = Some("sess-001".to_owned());
        *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        *confidence = Some("0.9".to_owned());
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
    vec![
        make_symbol("sym-1", "alpha_fn", "src/lib.rs"),
        make_symbol("sym-2", "beta_fn", "src/util.rs"),
        make_observation("obs-1", "alpha_fn looks correct", "sym-1"),
        make_task(),
    ]
}

/// Records for the `failure_history` conformance fixture: one symbol with a
/// superseded agent failure and a later passing verification, plus one
/// failure-free symbol.
fn failure_history_records() -> Vec<GraphRecord> {
    let mut records = vec![
        make_symbol("fh-sym-1", "fh_parse", "src/fh.rs"),
        make_symbol("fh-sym-2", "fh_quiet", "src/fh.rs"),
    ];

    let mut failure = GraphRecord::node(
        "fh-fail-1".to_owned(),
        NodeKind::Failure,
        None,
        None,
        None,
        "agent failure on fh_parse".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        agent_id,
        session_id,
        observed_at,
        evidence_links,
        ..
    } = &mut failure
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *agent_id = Some("test-agent".to_owned());
        *session_id = Some("sess-001".to_owned());
        *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some("fh-sym-1".to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "FAILED_ON".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }
    records.push(failure);

    let mut pass = GraphRecord::node(
        "fh-pass-1".to_owned(),
        NodeKind::TestRun,
        None,
        None,
        None,
        "passing test run for fh_parse".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version,
        status,
        verification_kind,
        executed_at,
        evidence_links,
        ..
    } = &mut pass
    {
        *schema_version = VERIFICATION_SCHEMA_VERSION;
        *status = Some("pass".to_owned());
        *verification_kind = Some("test_run".to_owned());
        *executed_at = Some("2026-02-01T00:00:00Z".to_owned());
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some("fh-sym-1".to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "VALIDATED_BY".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
    }
    records.push(pass);

    records
}

// ── Schema validation helper ────────────────────────────────────────────────

fn assert_valid(schema: &Value, instance: &Value, what: &str) {
    let validator =
        jsonschema::validator_for(schema).expect("published schema must be a valid JSON Schema");
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{what} failed schema validation:\n  - {}\ninstance: {instance}",
        errors.join("\n  - ")
    );
}

fn assert_invalid(schema: &Value, instance: &Value, what: &str) {
    let validator =
        jsonschema::validator_for(schema).expect("published schema must be a valid JSON Schema");
    assert!(
        !validator.is_valid(instance),
        "{what}: schema unexpectedly accepted {instance}"
    );
}

// ── AC: contract version advertisement ──────────────────────────────────────

/// The server must advertise `mcp_contract_version` in its server info so a
/// connected agent can read it without inspecting source (no tool call, no
/// daemon, no store needed — it rides on MCP `initialize`).
#[test]
fn contract_version_is_advertised_in_server_info() {
    let server = EgregoreMcpServer::new(Path::new(".").to_path_buf());
    let info = server.get_info();
    let instructions = info.instructions.as_deref().unwrap_or_default();
    let marker = format!("mcp_contract_version={MCP_CONTRACT_VERSION}");
    assert!(
        instructions.contains(&marker),
        "server instructions must advertise `{marker}`; got: {instructions}"
    );
}

/// `mcp_contract_version` is its own counter: distinct from the rmcp protocol
/// version negotiated in `initialize` and from record `schema_version`.
#[test]
fn contract_version_is_distinct_from_protocol_and_record_versions() {
    // It is a plain integer counter, not a semver triple like the crate /
    // protocol version — a bump means exactly one thing: a breaking-change
    // class landed in a tool payload.
    assert_ne!(
        MCP_CONTRACT_VERSION.to_string(),
        env!("CARGO_PKG_VERSION"),
        "mcp_contract_version must not track the crate/protocol version"
    );
    assert_eq!(
        MCP_CONTRACT_VERSION, 1,
        "contract starts at 1; a bump requires a documented breaking-change class"
    );
}

// ── AC: published schemas ───────────────────────────────────────────────────

/// Every shipped tool has a published success-response schema, and unknown
/// tool names resolve to nothing (no silent default schema).
#[test]
fn every_shipped_tool_has_a_published_schema() {
    assert_eq!(
        MCP_CONTRACT_TOOLS,
        [
            "inspect_store",
            "symbol_context",
            "task_evidence",
            "store_freshness",
            "failure_history",
            "record_observation",
            "search_code"
        ],
        "contract covers exactly the shipped tools in registration order"
    );
    for tool in MCP_CONTRACT_TOOLS {
        assert!(
            response_schema(tool).is_some(),
            "shipped tool `{tool}` must have a published success-response schema"
        );
    }
    assert!(
        response_schema("no_such_tool").is_none(),
        "unknown tool names must not resolve to a schema"
    );
}

/// The error envelope has a published schema of its own.
#[test]
fn error_envelope_has_a_published_schema() {
    let schema = error_schema();
    assert_eq!(schema["type"], Value::from("object"));
    assert!(schema["properties"]["error"].is_object());
}

// ── AC: conformance — success payloads ──────────────────────────────────────

/// The validator is not vacuous: it must reject a payload that violates the
/// schema, so a green conformance run means something.
#[test]
fn schema_validator_rejects_malformed_payloads() {
    let schema = response_schema("inspect_store").expect("schema must exist");
    assert_invalid(
        &schema,
        &serde_json::json!({"ok": "yes", "records": "many"}),
        "inspect_store schema",
    );
    assert_invalid(
        &schema,
        &serde_json::json!({"ok": true}),
        "inspect_store schema (missing required fields)",
    );
}

#[test]
fn inspect_store_success_conforms_to_published_schema() {
    let records = fixture_records();
    let mut payload = tool_inspect_store_from_records(&records, &[], "2026-01-01T00:00:00Z");
    stamp_freshness_on_payload(&mut payload, &records, Path::new("."), None);
    let schema = response_schema("inspect_store").expect("schema must exist");
    assert_valid(&schema, &payload, "inspect_store success");
}

#[test]
fn symbol_context_success_conforms_to_published_schema() {
    let records = fixture_records();
    let mut payload = tool_symbol_context_from_records(&records, "alpha_fn");
    assert_eq!(payload["ok"], Value::from(true));
    stamp_freshness_on_payload(&mut payload, &records, Path::new("."), None);
    let schema = response_schema("symbol_context").expect("schema must exist");
    assert_valid(&schema, &payload, "symbol_context success");
}

#[test]
fn task_evidence_success_conforms_to_published_schema() {
    let records = fixture_records();
    let mut payload = tool_task_evidence_from_records(&records, TASK_ID);
    assert_eq!(payload["ok"], Value::from(true));
    stamp_freshness_on_payload(&mut payload, &records, Path::new("."), None);
    let schema = response_schema("task_evidence").expect("schema must exist");
    assert_valid(&schema, &payload, "task_evidence success");
}

/// The `search_code` success payload validates against its published schema
/// (issue #182): the shaped verb result plus the freshness stamp every read
/// tool carries.
#[test]
fn search_code_success_conforms_to_published_schema() {
    let records = fixture_records();
    let verb_result = serde_json::json!({
        "verb": "semantic_search",
        "records": [
            {
                "record_id": "codegraph:v4:sym",
                "name": "alpha_fn",
                "repo_relative_path": "src/lib.rs",
                "score": 0.9,
                "span": {
                    "start_byte": 0, "end_byte": 10,
                    "start_line": 1, "end_line": 1,
                    "start_column": null, "end_column": null,
                },
            }
        ],
        "embedding_provenance": { "metric": "cosine" },
        "confidence": { "verdict": "strong" },
    });
    let mut payload = tool_search_code_from_verb_result("alpha", 10, &verb_result);
    assert_eq!(payload["ok"], Value::from(true));
    stamp_freshness_on_payload(&mut payload, &records, Path::new("."), None);
    let schema = response_schema("search_code").expect("schema must exist");
    assert_valid(&schema, &payload, "search_code success");
}

#[test]
fn store_freshness_success_conforms_to_published_schema() {
    let records = fixture_records();
    let payload = serde_json::json!({
        "ok": true,
        "freshness": tool_freshness_stamp(&records, Path::new("."), None),
    });
    let schema = response_schema("store_freshness").expect("schema must exist");
    assert_valid(&schema, &payload, "store_freshness success");
}

/// The `failure_history` success payload — one superseded agent failure and
/// its superseding pass, with the freshness stamp applied exactly the way
/// the tool method does — validates against the published schema.
#[test]
fn failure_history_success_conforms_to_published_schema() {
    let records = failure_history_records();
    let mut payload = tool_failure_history_from_records(&records, "fh_parse");
    assert_eq!(payload["ok"], Value::from(true), "got {payload}");
    assert_eq!(payload["agent_failures"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        payload["superseding_successes"].as_array().map(Vec::len),
        Some(1)
    );
    stamp_freshness_on_payload(&mut payload, &records, Path::new("."), None);
    let schema = response_schema("failure_history").expect("schema must exist");
    assert_valid(&schema, &payload, "failure_history success");
}

/// The resolved-but-empty `failure_history` answer (a target with no
/// recorded failures) is a distinct success shape — explicit empty sections
/// and a `safety_note` — and still validates.
#[test]
fn failure_history_empty_target_conforms_to_published_schema() {
    let records = failure_history_records();
    let mut payload = tool_failure_history_from_records(&records, "fh_quiet");
    assert_eq!(payload["ok"], Value::from(true), "got {payload}");
    stamp_freshness_on_payload(&mut payload, &records, Path::new("."), None);
    let schema = response_schema("failure_history").expect("schema must exist");
    assert_valid(&schema, &payload, "failure_history empty-target success");
}

// ── AC: conformance — error and empty results ───────────────────────────────

/// Agents must deterministically distinguish "symbol/task not found"
/// (`no_match`) from "store missing or unreadable" (`daemon_not_running` /
/// `daemon_stale`) and from caller mistakes (`missing_argument`) — all share
/// one stable envelope.
#[test]
fn error_shapes_conform_to_published_error_schema() {
    let records = fixture_records();
    let schema = error_schema();

    let no_match = tool_symbol_context_from_records(&records, "no_such_symbol");
    assert_eq!(no_match["ok"], Value::from(false));
    assert_eq!(no_match["error"]["code"], Value::from("no_match"));
    assert_valid(&schema, &no_match, "symbol_context no_match");

    let task_no_match = tool_task_evidence_from_records(
        &records,
        "project:v1:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    );
    assert_eq!(task_no_match["error"]["code"], Value::from("no_match"));
    assert_valid(&schema, &task_no_match, "task_evidence no_match");

    let missing = missing_argument_error("symbol_name");
    assert_eq!(missing["error"]["code"], Value::from("missing_argument"));
    assert_eq!(missing["error"]["field"], Value::from("symbol_name"));
    assert_valid(&schema, &missing, "missing_argument");
}

/// Every `failure_history` error code shares the stable error envelope.
#[test]
fn failure_history_error_shapes_conform_to_published_error_schema() {
    let records = failure_history_records();
    let schema = error_schema();

    let no_match = tool_failure_history_from_records(&records, "no_such_target");
    assert_eq!(no_match["error"]["code"], Value::from("no_match"));
    assert_valid(&schema, &no_match, "failure_history no_match");

    let unsupported = tool_failure_history_from_records(&records, "codegraph:not-a-valid-id");
    assert_eq!(
        unsupported["error"]["code"],
        Value::from("unsupported_handle")
    );
    assert_valid(&schema, &unsupported, "failure_history unsupported_handle");

    let mut stale_records = records;
    stale_records.push(GraphRecord::Tombstone {
        id: "fh-tombstone".to_owned(),
        schema_version: SCHEMA_VERSION,
        deleted_id: "fh-sym-1".to_owned(),
        summary: "deleted".to_owned(),
        producer: None,
    });
    let stale = tool_failure_history_from_records(&stale_records, "fh-sym-1");
    assert_eq!(stale["error"]["code"], Value::from("stale_handle"));
    assert_valid(&schema, &stale, "failure_history stale_handle");
}

/// A store/daemon failure (`daemon_not_running` / `daemon_stale`) shares the
/// stable error envelope but carries a different code from entity absence
/// (`no_match`): agents must not conflate "not found" with "store
/// unreachable". This exercises the real `symbol_context` tool method — not
/// the from-records helper — against a data directory that cannot exist.
#[test]
fn daemon_failure_shape_conforms_and_differs_from_no_match() {
    let server =
        EgregoreMcpServer::new(Path::new("/nonexistent-egregore-data-dir-issue-194").to_path_buf());
    let raw = server.symbol_context(Parameters(SymbolContextArgs {
        symbol_name: "alpha_fn".to_owned(),
        candidate: None,
        data_dir: None,
        repo_path: None,
    }));
    let payload: Value =
        serde_json::from_str(&raw).expect("symbol_context must return a JSON payload");
    assert_eq!(payload["ok"], Value::from(false));
    let code = payload["error"]["code"].as_str().unwrap_or_default();
    assert!(
        code == "daemon_not_running" || code == "daemon_stale",
        "expected a daemon failure code, got `{code}`: {payload}"
    );
    assert_ne!(
        code, "no_match",
        "daemon failure must not masquerade as entity absence"
    );
    assert_valid(&error_schema(), &payload, "daemon failure");
}

// ── AC: published schema files ──────────────────────────────────────────────

/// The schemas are published as checked-in files under `docs/schema/mcp/`
/// (generated by `cargo run --example gen_mcp_schemas`) so agent clients that
/// cannot call into Rust still get the machine-readable contract. These files
/// must never drift from the registered schemas: the Rust functions in
/// `mcp_contract` are the single source of truth.
#[test]
fn published_schema_files_match_registered_schemas() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/schema/mcp");
    for tool in MCP_CONTRACT_TOOLS {
        let path = dir.join(format!("{tool}.schema.json"));
        let file_contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("published schema file missing: {}: {e}", path.display()));
        let from_file: Value = serde_json::from_str(&file_contents)
            .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", path.display()));
        let registered = response_schema(tool).expect("shipped tool must have a schema");
        assert_eq!(
            from_file, registered,
            "checked-in {tool}.schema.json drifted from mcp_contract::response_schema"
        );
    }
    let path = dir.join("error.schema.json");
    let file_contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("published schema file missing: {}: {e}", path.display()));
    let from_file: Value = serde_json::from_str(&file_contents)
        .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", path.display()));
    assert_eq!(
        from_file,
        error_schema(),
        "checked-in error.schema.json drifted from mcp_contract::error_schema"
    );
}

// ── Issue #192: ambiguous_symbol error envelope ─────────────────────────────

/// The `ambiguous_symbol` disambiguation error shares the published error
/// envelope: it must validate against `error_schema()` exactly like
/// `no_match` and `ambiguous_handle` do.
#[test]
fn ambiguous_symbol_error_conforms_to_published_error_schema() {
    use aletheia_egregore::mcp::tool_symbol_context_from_records_with_candidate;

    let sym_a = GraphRecord::symbol(
        "codegraph:v4:aaaa0001build".to_owned(),
        "function",
        "src/a.rs".to_owned(),
        span(1, 9),
        "build".to_owned(),
        "Rust function build in a".to_owned(),
    )
    .with_valid_time_inferred("2026-01-01T00:00:00Z");
    let sym_b = GraphRecord::symbol(
        "codegraph:v4:bbbb0002build".to_owned(),
        "function",
        "src/b.rs".to_owned(),
        span(11, 19),
        "build".to_owned(),
        "Rust function build in b".to_owned(),
    )
    .with_valid_time_inferred("2026-01-01T00:00:00Z");
    let records = vec![sym_a, sym_b];

    let payload = tool_symbol_context_from_records_with_candidate(&records, "build", None);
    assert_eq!(payload["ok"], Value::from(false));
    assert_eq!(payload["error"]["code"], Value::from("ambiguous_symbol"));
    assert_valid(&error_schema(), &payload, "symbol_context ambiguous_symbol");

    // The code is stable and distinct from the sibling contracts.
    assert_ne!(payload["error"]["code"], Value::from("no_match"));
    assert_ne!(payload["error"]["code"], Value::from("ambiguous_handle"));
}
