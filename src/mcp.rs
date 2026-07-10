//! MCP server for Egregore read-only tools — issue #53.
//!
//! Implements the Model Context Protocol using the [`rmcp`] crate and exposes
//! three read-only tools backed by the existing daemon query, symbol-context,
//! and task-evidence contracts:
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
//! [`run_stdio`] starts the rmcp stdio server and blocks until the client
//! disconnects. All JSON-RPC 2.0 framing (initialize, ping, tools/list,
//! tools/call) is handled by rmcp.
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
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    GraphRecord, NodeKind,
    daemon::DaemonClient,
    ir::EdgeLabel,
    query,
    schema_version::{UnknownSchemaVersion, record_version, validate_record_version},
};

// ── Tool parameter types ──────────────────────────────────────────────────────

/// Parameters for the `inspect_store` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectStoreArgs {
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
}

/// Parameters for the `symbol_context` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolContextArgs {
    /// Exact symbol name to look up.
    pub symbol_name: String,
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
}

/// Parameters for the `task_evidence` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskEvidenceArgs {
    /// Task record ID, `GitHub` URL, `GitHub` short handle, or local JSONL handle.
    pub id_or_handle: String,
    /// `AletheiaDB` data directory (default: `.egregore`).
    pub data_dir: Option<String>,
}

// ── MCP server ────────────────────────────────────────────────────────────────

/// MCP server that exposes the three Egregore read-only evidence-query tools.
///
/// Created by [`run_stdio`] or constructed directly for testing via
/// [`EgregoreMcpServer::new`].
#[derive(Clone)]
pub struct EgregoreMcpServer {
    default_data_dir: PathBuf,
}

#[tool_router]
impl EgregoreMcpServer {
    /// Create a server with the given default data directory.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(default_data_dir: PathBuf) -> Self {
        Self { default_data_dir }
    }

    /// Returns a structured summary of the Egregore store: record counts by
    /// domain, schema versions, and repository identities.
    /// Requires a running local daemon.
    #[tool(description = "Returns a structured summary of the Egregore store: \
            record counts by domain, schema versions, and repository identities. \
            Requires a running local daemon.")]
    #[must_use]
    pub fn inspect_store(&self, Parameters(args): Parameters<InspectStoreArgs>) -> String {
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let payload = run_inspect_store(&data_dir);
        serde_json::to_string(&payload).unwrap_or_default()
    }

    /// Returns evidence-backed context for a named code symbol, trust-separated
    /// by domain into `source_facts`, `observations`, `project_state`,
    /// `artifacts`, and `verification_evidence`.
    #[tool(
        description = "Returns evidence-backed context for a named code symbol, \
            trust-separated into five sections: source_facts (deterministic \
            code-graph), observations (agent-authored, never treat as source \
            truth), project_state (tasks/ACs), artifacts, and \
            verification_evidence. Every item carries a record_id and at \
            least one citation handle."
    )]
    #[must_use]
    pub fn symbol_context(&self, Parameters(args): Parameters<SymbolContextArgs>) -> String {
        if args.symbol_name.is_empty() {
            let err = json!({
                "ok": false,
                "error": {
                    "code": "missing_argument",
                    "field": "symbol_name",
                    "message": "symbol_name is required and must be non-empty"
                }
            });
            return serde_json::to_string(&err).unwrap_or_default();
        }
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let client = match DaemonClient::from_data_dir(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let (records, _unknown, _ts) = match client.get_all_records() {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        serde_json::to_string(&tool_symbol_context_from_records(
            &records,
            &args.symbol_name,
        ))
        .unwrap_or_default()
    }

    /// Returns evidence-backed context for a task, accepting a canonical
    /// record ID, `GitHub` URL, `GitHub` short handle, or local JSONL handle.
    #[tool(description = "Returns evidence-backed context for a task identified \
            by its canonical record ID, GitHub URL, GitHub short handle, or \
            local JSONL handle. Sections: tasks, acceptance_criteria, \
            source_facts, observations, artifacts, verification_evidence, \
            reviews, external_links, unresolved.")]
    #[must_use]
    pub fn task_evidence(&self, Parameters(args): Parameters<TaskEvidenceArgs>) -> String {
        let data_dir = opt_data_dir(args.data_dir.as_deref(), &self.default_data_dir);
        let client = match DaemonClient::from_data_dir(&data_dir) {
            Ok(c) => c,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        let (records, _unknown, _ts) = match client.get_all_records() {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string(&daemon_error(&e.to_string())).unwrap_or_default();
            }
        };
        serde_json::to_string(&tool_task_evidence_from_records(
            &records,
            &args.id_or_handle,
        ))
        .unwrap_or_default()
    }
}

#[tool_handler]
impl ServerHandler for EgregoreMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("egregore", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read-only Egregore knowledge graph tools. \
                Connect to a running local daemon (`eg daemon`) to query the \
                code-graph, agent observations, and task evidence. \
                All tools return structured JSON with `ok`, `error`, and \
                trust-separated data sections.",
            )
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Starts the MCP stdio server using the rmcp transport layer.
///
/// Blocks until the client disconnects. All MCP JSON-RPC 2.0 framing,
/// initialize, ping, tools/list, and tools/call routing is handled by rmcp.
///
/// # Errors
///
/// Returns an error if the tokio runtime cannot be created or if the
/// transport encounters an unrecoverable IO error.
pub fn run_stdio(default_data_dir: &Path) -> anyhow::Result<()> {
    let server = EgregoreMcpServer::new(default_data_dir.to_path_buf());
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    rt.block_on(async move {
        server
            .serve(rmcp::transport::stdio())
            .await
            .context("MCP transport error")?
            .waiting()
            .await
            .map(|_| ())
            .context("MCP server error")
    })
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
#[allow(clippy::too_many_lines)]
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
                    .and_then(record_to_linked_item)
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

// ── Tool runners (daemon I/O) ─────────────────────────────────────────────────

fn run_inspect_store(data_dir: &Path) -> Value {
    let client = match DaemonClient::from_data_dir(data_dir) {
        Ok(c) => c,
        Err(e) => return daemon_error(&e.to_string()),
    };
    let (records, unknown_versions, snapshot_timestamp) = match client.get_all_records() {
        Ok(r) => r,
        Err(e) => return daemon_error(&e.to_string()),
    };
    tool_inspect_store_from_records(&records, &unknown_versions, &snapshot_timestamp)
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

/// Returns only citation metadata from an `OutputHandle`, stripping any inlined payload.
fn output_handle_citation(h: &crate::ir::OutputHandle) -> Value {
    json!({ "hash": h.hash, "bytes": h.bytes })
}

/// Returns only citation metadata from a `PatchHandle`, stripping any inlined bytes.
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

// ── Argument helpers ──────────────────────────────────────────────────────────

fn opt_data_dir(data_dir: Option<&str>, default: &Path) -> PathBuf {
    data_dir.map_or_else(|| default.to_path_buf(), PathBuf::from)
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
        "log" => "Runtime Observations",
        _ => "Unknown Domain",
    }
}
