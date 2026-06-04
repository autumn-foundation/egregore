//! MCP server for Egregore read-only tools — issue #53.
//!
//! Implements the Model Context Protocol (JSON-RPC 2.0 over stdio) and exposes
//! three read-only tools backed by the existing daemon query, symbol-context, and
//! task-evidence contracts:
//!
//! - **`inspect_store`** — store-inspection summary (record counts, domain breakdown).
//! - **`symbol_context`** — evidence-backed symbol context, trust-separated by domain.
//! - **`task_evidence`** — evidence-backed task context, trust-separated by domain.
//!
//! All tool responses carry machine-readable structured output with record IDs and
//! citation handles. No write tools ship in this slice.
//!
//! ## Transport
//!
//! [`run_stdio`] processes newline-delimited JSON-RPC 2.0 messages from stdin and
//! writes responses to stdout. One message per line; one response per request.
//! Notifications (no `id` field) produce no response.
//!
//! ## Daemon discovery
//!
//! Each tool call discovers the running daemon from the `data_dir` argument
//! (default `.egregore`) using the existing `DaemonClient::from_data_dir`
//! contract. When the daemon is missing or stale the tool returns a stable
//! machine-readable error rather than falling back to shell commands or
//! embedded direct reads.
//!
//! ## Redaction
//!
//! Tool output never includes raw transcript text, patch hunks, issue bodies,
//! bearer tokens, or other protected artifact payloads. All structured output
//! is produced using the same field-level filtering applied by the CLI query
//! commands.

use std::{
    collections::BTreeMap,
    io::{BufRead, Write as _},
    path::Path,
};

use serde_json::{Value, json};

use crate::{
    GraphRecord, NodeKind,
    daemon::DaemonClient,
    ir::EdgeLabel,
    query,
    schema_version::{UnknownSchemaVersion, record_version, validate_record_version},
};

// ── Protocol constants ────────────────────────────────────────────────────────

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "egregore";

// ── JSON-RPC error codes ──────────────────────────────────────────────────────

const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

// ── Public API ────────────────────────────────────────────────────────────────

/// Runs the MCP stdio server, processing JSON-RPC 2.0 messages from stdin.
///
/// Blocks until stdin is closed. Each non-empty line is treated as one message.
/// Responses are written to stdout, one JSON object per line.
///
/// # Errors
///
/// Returns an error if stdin/stdout IO fails.
pub fn run_stdio(default_data_dir: &Path) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_message(&line, default_data_dir) {
            let encoded = serde_json::to_string(&response)?;
            writeln!(stdout, "{encoded}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Processes a single JSON-RPC 2.0 message line.
///
/// Returns `Some(response)` for requests that require a reply.
/// Returns `None` for notifications (no `id` field) and silently-ignored inputs.
///
/// This function is `pub` so tests can call it without going through the stdio
/// transport.
#[must_use]
pub fn handle_message(line: &str, data_dir: &Path) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": PARSE_ERROR, "message": "Parse error" }
            }));
        }
    };

    // Notifications have no `id` — do not reply.
    let id = match msg.get("id") {
        Some(id) => id.clone(),
        None => return None,
    };

    let method = msg["method"].as_str().unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    let (result_key, result_val) = match method {
        "initialize" => ("result", initialize_result()),
        "tools/list" => ("result", tools_list_result()),
        "tools/call" => match dispatch_tool_call(&params, data_dir) {
            Ok(v) => ("result", v),
            Err(err) => ("error", err),
        },
        // ping is a standard MCP utility — must reply with an empty result.
        "ping" => ("result", json!({})),
        _ => (
            "error",
            json!({ "code": METHOD_NOT_FOUND, "message": format!("Method not found: {method}") }),
        ),
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, result_key: result_val }))
}

/// Builds a structured store-inspection summary from a record slice.
///
/// Used by the `inspect_store` tool after fetching records from the daemon, and
/// directly by tests that supply a fixture slice.
///
/// The `snapshot_timestamp` field is forwarded verbatim; pass an RFC 3339 string.
#[must_use]
pub fn tool_inspect_store_from_records(
    records: &[GraphRecord],
    unknown_versions: &[UnknownSchemaVersion],
    snapshot_timestamp: &str,
) -> Value {
    let mut total = 0_usize;
    let mut nodes = 0_usize;
    let mut edges = 0_usize;
    let mut tombstones = 0_usize;
    let mut diagnostics = 0_usize;
    let mut schema_versions: BTreeMap<String, usize> = BTreeMap::new();
    let mut unknown_schema_map: BTreeMap<String, usize> = BTreeMap::new();
    let mut domain_counts: BTreeMap<&'static str, BTreeMap<String, usize>> = BTreeMap::new();
    let mut repositories: Vec<Value> = Vec::new();

    for uv in unknown_versions {
        total += 1;
        let key = format!(
            "{}:{}:{}",
            uv.version.domain, uv.version.kind, uv.version.version
        );
        *unknown_schema_map.entry(key).or_default() += 1;
    }

    for record in records {
        total += 1;

        if let Err(uv) = validate_record_version(record) {
            let key = format!(
                "{}:{}:{}",
                uv.version.domain, uv.version.kind, uv.version.version
            );
            *unknown_schema_map.entry(key).or_default() += 1;
            continue;
        }

        let rv = record_version(record);
        let sv_key = format!("{}:{}:{}", rv.domain, rv.kind, rv.version);
        *schema_versions.entry(sv_key.clone()).or_default() += 1;

        let category = domain_category(&rv.domain);
        let kind_key = format!("{} v{}", rv.kind, rv.version);
        *domain_counts
            .entry(category)
            .or_default()
            .entry(kind_key)
            .or_default() += 1;

        match record {
            GraphRecord::Node {
                kind,
                id,
                repository_identity,
                ..
            } => {
                nodes += 1;
                if *kind == NodeKind::Diagnostic {
                    diagnostics += 1;
                }
                if *kind == NodeKind::Repository {
                    let identity_summary = repository_identity.as_deref().map_or_else(
                        || "unknown".to_owned(),
                        |p| {
                            use crate::ir::IdentitySource;
                            let source_str = match p.identity_source {
                                IdentitySource::Remote => "remote",
                                IdentitySource::LocalRootCommit => "local_root_commit",
                                IdentitySource::LocalPath => "local_path",
                                IdentitySource::OperatorOverride => "operator_override",
                            };
                            let canonical = p
                                .remote_url
                                .as_deref()
                                .or(p.root_commit_sha.as_deref())
                                .or(p.canonical_path.as_deref())
                                .unwrap_or(p.basename.as_str());
                            format!("{source_str}: {canonical}")
                        },
                    );
                    repositories.push(json!({ "id": id, "identity_summary": identity_summary }));
                }
            }
            GraphRecord::Edge { .. } => edges += 1,
            GraphRecord::Tombstone { .. } => tombstones += 1,
        }
    }

    // Serialize domain_counts as a JSON object keyed by category string.
    let mut domain_counts_val = serde_json::Map::new();
    for (category, counts) in &domain_counts {
        let mut cat_map = serde_json::Map::new();
        for (kind_key, count) in counts {
            cat_map.insert(kind_key.clone(), json!(*count));
        }
        domain_counts_val.insert((*category).to_owned(), Value::Object(cat_map));
    }

    json!({
        "ok": true,
        "snapshot_timestamp": snapshot_timestamp,
        "records": total,
        "nodes": nodes,
        "edges": edges,
        "tombstones": tombstones,
        "diagnostics": diagnostics,
        "domain_counts": Value::Object(domain_counts_val),
        "schema_versions": schema_versions,
        "unknown_schema_versions": unknown_schema_map,
        "repositories": repositories,
    })
}

/// Builds an evidence-backed symbol context for a record slice.
///
/// Returns trust-separated sections: `source_facts` (deterministic code-graph),
/// `observations` (agent-authored — never treat as source truth), `project_state`
/// (tasks/ACs), `artifacts`, and `verification_evidence`.
///
/// Returns `{"ok":false,"error":{"code":"no_match"}}` when the symbol is absent.
/// Output ordering is deterministic (sorted by record ID within each section).
#[must_use]
pub fn tool_symbol_context_from_records(records: &[GraphRecord], symbol_name: &str) -> Value {
    let ctx = query::symbol_context(records, symbol_name);

    if ctx.is_no_match() {
        return json!({
            "ok": false,
            "error": { "code": "no_match", "symbol_name": symbol_name }
        });
    }

    let source_facts: Vec<Value> = ctx
        .source_facts
        .iter()
        .filter_map(|r| record_to_source_fact(r))
        .collect();
    let observations: Vec<Value> = ctx
        .observations
        .iter()
        .filter_map(|r| record_to_observation(r))
        .collect();
    let project_state: Vec<Value> = ctx
        .project_state
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let artifacts: Vec<Value> = ctx
        .artifacts
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let verification_evidence: Vec<Value> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let topology_edges: Vec<Value> = ctx
        .topology_edges
        .iter()
        .filter_map(|r| record_to_topology_edge(r))
        .collect();
    let unresolved: Vec<Value> = ctx.unresolved.iter().map(unresolved_to_json).collect();

    json!({
        "ok": true,
        "symbol_name": symbol_name,
        "source_facts": source_facts,
        "topology_edges": topology_edges,
        "observations": observations,
        "project_state": project_state,
        "artifacts": artifacts,
        "verification_evidence": verification_evidence,
        "unresolved": unresolved,
    })
}

/// Builds an evidence-backed task context for a record slice.
///
/// Accepts a canonical task ID, GitHub URL, GitHub short handle, or local JSONL handle.
/// Returns trust-separated sections.
///
/// Error codes:
/// - `no_match` — no task with the given handle exists
/// - `ambiguous_handle` — the handle matched more than one task
/// - `unsupported_handle` — the handle format is unrecognized
#[must_use]
pub fn tool_task_evidence_from_records(records: &[GraphRecord], id_or_handle: &str) -> Value {
    let resolved_ids = match query::resolve_task_ids(records, id_or_handle) {
        Ok(ids) => ids,
        Err(query::TaskResolveError::Ambiguous { handle, candidates }) => {
            return json!({
                "ok": false,
                "error": {
                    "code": "ambiguous_handle",
                    "handle": handle,
                    "candidates": candidates,
                }
            });
        }
        Err(query::TaskResolveError::Unsupported { handle, message }) => {
            return json!({
                "ok": false,
                "error": {
                    "code": "unsupported_handle",
                    "handle": handle,
                    "message": message,
                }
            });
        }
    };

    let Some(task_id) = resolved_ids.iter().next() else {
        return json!({
            "ok": false,
            "error": { "code": "no_match", "id_or_handle": id_or_handle }
        });
    };
    let ctx = query::task_evidence_context(records, task_id);

    if ctx.is_no_match() {
        return json!({
            "ok": false,
            "error": { "code": "no_match", "id_or_handle": id_or_handle }
        });
    }

    let tasks: Vec<Value> = ctx
        .tasks
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let acceptance_criteria: Vec<Value> = ctx
        .acceptance_criteria
        .iter()
        .filter_map(|r| {
            let mut item = record_to_linked_item(r)?;
            // For verified ACs, attach the closing verification record so the
            // evidence that closed it is visible without a second tool call.
            if item["status"].as_str() == Some("verified") {
                let GraphRecord::Node {
                    verification_link_id,
                    ..
                } = r
                else {
                    return Some(item);
                };
                let ver_id = verification_link_id.as_deref().or_else(|| {
                    records.iter().find_map(|edge| {
                        if let GraphRecord::Edge {
                            label: EdgeLabel::ClosesAcceptanceCriterion,
                            source,
                            target,
                            ..
                        } = edge
                        {
                            if source == r.id() {
                                Some(target.as_str())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                });
                if let Some(ver) = ver_id
                    .and_then(|vid| records.iter().find(|c| c.id() == vid))
                    .and_then(|rec| record_to_linked_item(rec))
                {
                    item["verification_record"] = ver;
                }
            }
            Some(item)
        })
        .collect();
    let source_facts: Vec<Value> = ctx
        .source_facts
        .iter()
        .filter_map(|r| record_to_source_fact(r))
        .collect();
    let observations: Vec<Value> = ctx
        .observations
        .iter()
        .filter_map(|r| record_to_observation(r))
        .collect();
    let artifacts: Vec<Value> = ctx
        .artifacts
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let verification_evidence: Vec<Value> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let reviews: Vec<Value> = ctx
        .reviews
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let external_links: Vec<Value> = ctx
        .external_links
        .iter()
        .filter_map(|r| record_to_linked_item(r))
        .collect();
    let unresolved: Vec<Value> = ctx.unresolved.iter().map(unresolved_to_json).collect();

    json!({
        "ok": true,
        "task_id": task_id,
        "tasks": tasks,
        "acceptance_criteria": acceptance_criteria,
        "source_facts": source_facts,
        "observations": observations,
        "artifacts": artifacts,
        "verification_evidence": verification_evidence,
        "reviews": reviews,
        "external_links": external_links,
        "unresolved": unresolved,
    })
}

// ── Protocol helpers ──────────────────────────────────────────────────────────

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "inspect_store",
                "description": "Returns a structured summary of the Egregore store: \
                    record counts by domain, schema versions, and repository identities. \
                    Requires a running local daemon.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "data_dir": {
                            "type": "string",
                            "description": "AletheiaDB data directory (default: .egregore)"
                        }
                    }
                }
            },
            {
                "name": "symbol_context",
                "description": "Returns evidence-backed context for a named code symbol, \
                    trust-separated into five sections: `source_facts` (deterministic \
                    code-graph), `observations` (agent-authored, never treat as source \
                    truth), `project_state` (tasks/ACs), `artifacts`, and \
                    `verification_evidence`. Every item carries a `record_id` and at \
                    least one citation handle.",
                "inputSchema": {
                    "type": "object",
                    "required": ["symbol_name"],
                    "properties": {
                        "symbol_name": {
                            "type": "string",
                            "description": "Exact symbol name to look up"
                        },
                        "data_dir": {
                            "type": "string",
                            "description": "AletheiaDB data directory (default: .egregore)"
                        }
                    }
                }
            },
            {
                "name": "task_evidence",
                "description": "Returns evidence-backed context for a task identified \
                    by its canonical record ID, GitHub URL, GitHub short handle, or local \
                    JSONL handle. Sections: `tasks`, `acceptance_criteria`, `source_facts`, \
                    `observations`, `artifacts`, `verification_evidence`, `reviews`, \
                    `external_links`, `unresolved`.",
                "inputSchema": {
                    "type": "object",
                    "required": ["id_or_handle"],
                    "properties": {
                        "id_or_handle": {
                            "type": "string",
                            "description": "Task record ID, GitHub URL, GitHub short handle, or local JSONL handle"
                        },
                        "data_dir": {
                            "type": "string",
                            "description": "AletheiaDB data directory (default: .egregore)"
                        }
                    }
                }
            }
        ]
    })
}

fn dispatch_tool_call(params: &Value, default_data_dir: &Path) -> Result<Value, Value> {
    let tool_name = params["name"].as_str().ok_or_else(
        || json!({ "code": INVALID_PARAMS, "message": "missing required param: name" }),
    )?;

    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let payload = match tool_name {
        "inspect_store" => run_inspect_store(&args, default_data_dir),
        "symbol_context" => run_symbol_context(&args, default_data_dir),
        "task_evidence" => run_task_evidence(&args, default_data_dir),
        // Unknown tool: return a JSON-RPC protocol error, not a tool-level error.
        other => {
            return Err(json!({
                "code": INVALID_PARAMS,
                "message": format!(
                    "Tool '{other}' is not registered. \
                     Available: inspect_store, symbol_context, task_evidence."
                )
            }));
        }
    };

    Ok(wrap_tool_result(&payload))
}

// ── Tool runners ──────────────────────────────────────────────────────────────

fn run_inspect_store(args: &Value, default_data_dir: &Path) -> Value {
    let data_dir = resolve_data_dir(args, default_data_dir);

    let client = match DaemonClient::from_data_dir(&data_dir) {
        Ok(c) => c,
        Err(e) => return daemon_error(&e.to_string()),
    };

    let (records, unknown_versions, snapshot_timestamp) = match client.get_all_records() {
        Ok(r) => r,
        Err(e) => return daemon_error(&e.to_string()),
    };

    tool_inspect_store_from_records(&records, &unknown_versions, &snapshot_timestamp)
}

fn run_symbol_context(args: &Value, default_data_dir: &Path) -> Value {
    let symbol_name = match args["symbol_name"].as_str() {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => {
            return json!({
                "ok": false,
                "error": {
                    "code": "missing_argument",
                    "field": "symbol_name",
                    "message": "symbol_name is required"
                }
            });
        }
    };

    let data_dir = resolve_data_dir(args, default_data_dir);

    let client = match DaemonClient::from_data_dir(&data_dir) {
        Ok(c) => c,
        Err(e) => return daemon_error(&e.to_string()),
    };

    let (records, _unknown, _ts) = match client.get_all_records() {
        Ok(r) => r,
        Err(e) => return daemon_error(&e.to_string()),
    };

    tool_symbol_context_from_records(&records, &symbol_name)
}

fn run_task_evidence(args: &Value, default_data_dir: &Path) -> Value {
    let id_or_handle = match args["id_or_handle"].as_str() {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => {
            return json!({
                "ok": false,
                "error": {
                    "code": "missing_argument",
                    "field": "id_or_handle",
                    "message": "id_or_handle is required"
                }
            });
        }
    };

    let data_dir = resolve_data_dir(args, default_data_dir);

    let client = match DaemonClient::from_data_dir(&data_dir) {
        Ok(c) => c,
        Err(e) => return daemon_error(&e.to_string()),
    };

    let (records, _unknown, _ts) = match client.get_all_records() {
        Ok(r) => r,
        Err(e) => return daemon_error(&e.to_string()),
    };

    tool_task_evidence_from_records(&records, &id_or_handle)
}

// ── Record → JSON helpers ─────────────────────────────────────────────────────

fn record_to_source_fact(record: &GraphRecord) -> Option<Value> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        language,
        symbol_kind,
        ..
    } = record
    else {
        return None;
    };
    Some(json!({
        "record_id": id,
        "kind": kind.as_str(),
        "name": name,
        "repo_relative_path": repo_relative_path,
        "span": span,
        "git_commit": temporal.as_ref().map(|t| t.git_commit.as_str()),
        "valid_time": valid_time.as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        "language": language,
        "symbol_kind": symbol_kind,
    }))
}

fn record_to_observation(record: &GraphRecord) -> Option<Value> {
    let GraphRecord::Node {
        id,
        kind,
        summary,
        text,
        agent_id,
        session_id,
        observed_at,
        confidence,
        failure_kind,
        exit_code,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    // Serialize evidence_links, redacting raw token fields
    let links: Vec<Value> = evidence_links
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|l| {
            json!({
                "target_record_id": l.target_record_id,
                "target_domain": l.target_domain,
                "relation": l.relation,
                "confidence": l.confidence,
            })
        })
        .collect();
    Some(json!({
        "record_id": id,
        "kind": kind.as_str(),
        "summary": summary,
        "text": text,
        "provenance_handle": provenance_handle,
        "agent_id": agent_id,
        "session_id": session_id,
        "observed_at": observed_at,
        "confidence": confidence,
        "failure_kind": failure_kind,
        "exit_code": exit_code,
        "evidence_links": links,
    }))
}

/// Returns only citation metadata from an OutputHandle, stripping any inlined payload.
fn output_handle_citation(h: &crate::ir::OutputHandle) -> Value {
    json!({ "hash": h.hash, "bytes": h.bytes })
}

/// Returns only citation metadata from a PatchHandle, stripping any inlined bytes.
fn patch_handle_citation(h: &crate::ir::PatchHandle) -> Value {
    json!({ "path": h.path })
}

fn record_to_linked_item(record: &GraphRecord) -> Option<Value> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        text,
        summary,
        status,
        verification_kind,
        exit_code,
        executed_at,
        evidence_quality,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
        edit_kind,
        patch_status,
        patch_bytes_hash,
        patch_bytes_size,
        patch_handle,
        target_files,
        validation_summary,
        base_commit,
        producer_session_id,
        author,
        evidence_links,
        url,
        system_native_id,
        body_handle,
        stdout_handle,
        stderr_handle,
        ..
    } = record
    else {
        return None;
    };
    // Redact validation_summary per AC6 (may contain raw patch output)
    // Keep only a hash/handle reference when populated.
    let redacted_validation = validation_summary
        .as_deref()
        .map(|s| if s.is_empty() { s } else { "<summarized>" });
    let links: Vec<Value> = evidence_links
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|l| {
            json!({
                "target_record_id": l.target_record_id,
                "target_domain": l.target_domain,
                "relation": l.relation,
                "confidence": l.confidence,
            })
        })
        .collect();
    Some(json!({
        "record_id": id,
        "kind": kind.as_str(),
        "summary": summary,
        "title": title,
        "name": name,
        "text": text,
        "status": status,
        "verification_kind": verification_kind,
        "exit_code": exit_code,
        "executed_at": executed_at,
        "evidence_quality": evidence_quality,
        "source_artifact_path": source_artifact_path,
        "source_artifact_hash": source_artifact_hash,
        "stdout_handle": stdout_handle.as_deref().map(output_handle_citation),
        "stderr_handle": stderr_handle.as_deref().map(output_handle_citation),
        "repo_relative_path": repo_relative_path,
        "edit_kind": edit_kind,
        "patch_status": patch_status,
        "patch_handle": patch_handle.as_deref().map(patch_handle_citation),
        "patch_bytes_hash": patch_bytes_hash,
        "patch_bytes_size": patch_bytes_size,
        "target_files": target_files,
        "validation_summary": redacted_validation,
        "base_commit": base_commit,
        "producer_session_id": producer_session_id,
        "body_handle": body_handle.as_deref().map(output_handle_citation),
        "author": author,
        "url": url,
        "system_native_id": system_native_id,
        "evidence_links": links,
    }))
}

fn record_to_topology_edge(record: &GraphRecord) -> Option<Value> {
    let GraphRecord::Edge {
        id,
        label,
        source,
        target,
        summary,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    Some(json!({
        "record_id": id,
        "label": label.as_str(),
        "source_id": source,
        "target_id": target,
        "summary": summary,
        "git_commit": temporal.as_ref().map(|t| t.git_commit.as_str()),
        "valid_time": temporal.as_ref().map(|t| t.valid_time.as_str()),
    }))
}

fn unresolved_to_json(u: &query::UnresolvedRef) -> Value {
    json!({
        "source_record_id": u.source_record_id,
        "target_handle": u.target_handle,
        "relation": u.relation,
        "target_domain": u.target_domain,
        "verification_status": "unresolved",
    })
}

// ── Error helpers ─────────────────────────────────────────────────────────────

fn daemon_error(msg: &str) -> Value {
    let code = if msg.to_lowercase().contains("stale") || msg.to_lowercase().contains("metadata") {
        "daemon_stale"
    } else {
        "daemon_not_running"
    };
    json!({
        "ok": false,
        "error": { "code": code, "message": msg }
    })
}

fn wrap_tool_result(payload: &Value) -> Value {
    let is_error = payload.get("ok").and_then(Value::as_bool) == Some(false);
    let text = serde_json::to_string(payload).unwrap_or_default();
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

// ── Argument helpers ─────────────────────────────────────────────────────────

fn resolve_data_dir(args: &Value, default: &Path) -> std::path::PathBuf {
    args["data_dir"]
        .as_str()
        .map_or_else(|| default.to_path_buf(), std::path::PathBuf::from)
}

// ── Domain category mapping (matches the existing CLI inspect output) ─────────

#[allow(clippy::missing_const_for_fn)]
fn domain_category(domain: &str) -> &'static str {
    match domain {
        "codegraph" => "Deterministic Source Facts",
        "semantic" => "Derived Measurements",
        "agent_memory" => "Agent-Authored Claims",
        "project" => "Project/Work State",
        "artifact" => "Artifacts",
        "verification" => "Verification Evidence",
        "user_context" => "User Context",
        _ => "Unknown Domain",
    }
}
