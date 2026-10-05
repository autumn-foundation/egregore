//! Frozen MCP tool I/O contract — issue #194.
//!
//! The `eg mcp` server is the surface coding agents actually connect to, but
//! its tool payloads were previously shaped only by implementation accident.
//! This module freezes them:
//!
//! - [`MCP_CONTRACT_VERSION`] — the contract counter, advertised to agents in
//!   the server info (`initialize` response) so they can read it without
//!   inspecting source. Distinct from the rmcp protocol version negotiated in
//!   `initialize` and from record `schema_version`.
//! - [`response_schema`] — the published JSON Schema (Draft 2020-12) for each
//!   shipped tool's *success* response. A conformance test
//!   (`tests/integration/mcp_contract.rs`) asserts that the real output of
//!   every shipped tool validates against its schema.
//! - [`error_schema`] — the published schema for the stable error envelope,
//!   so an agent can deterministically distinguish "symbol/task not found"
//!   from "store missing or unreadable".
//!
//! The human-readable contract — per-tool inputs, per-field stability tiers
//! (`stable` vs `provisional`), the breaking-vs-additive definition, and the
//! release gate for new tools — lives in `docs/schema/mcp.md`; this module is
//! its machine-readable twin and must be updated in the same commit.
//!
//! Versioning rule: `MCP_CONTRACT_VERSION` bumps **only** when a documented
//! breaking-change class lands (field removal, rename, type change, or
//! semantic change of an existing field, or an error-code identifier change).
//! Additive changes — new tools, new optional input parameters, new response
//! fields, new error codes — never bump it, and the schemas below declare
//! `additionalProperties: true` at every object level so additive fields
//! validate against the frozen schema.

use serde_json::{Value, json};

/// The frozen MCP tool I/O contract version.
///
/// Starts at 1. Bumps **only** on a breaking-change class (see the module
/// docs and `docs/schema/mcp.md`): field removal, field rename, field type
/// change, semantic change of an existing field, or an error-code identifier
/// change. Additive changes (new tools, new optional parameters, new response
/// fields, new error codes) never bump it.
///
/// This is deliberately a plain integer counter, not a semver triple: it is
/// distinct from the crate/protocol version and from record `schema_version`.
pub const MCP_CONTRACT_VERSION: u32 = 1;

/// Tool names covered by the frozen contract, in `tool_router` registration
/// order. Any new MCP tool (e.g. #181, #182) must extend this
/// list and register its schema here before it ships.
pub const MCP_CONTRACT_TOOLS: [&str; 8] = [
    "inspect_store",
    "symbol_context",
    "task_evidence",
    "store_freshness",
    "failure_history",
    "record_observation",
    "search_code",
    "symbol_at",
];

/// Returns the published JSON Schema (Draft 2020-12) for a shipped tool's
/// success response (`ok: true`), or `None` for an unknown tool name.
///
/// Over the wire, success responses also carry the `freshness` object from
/// issue #220 (the store-freshness verdict stamp); the schemas below include
/// it, and the conformance test stamps it exactly the way the tool methods
/// do before validating.
///
/// Every object level declares `additionalProperties: true`: additive
/// response fields validate against the frozen schema, so an additive-only
/// tool addition never breaks conformance and never bumps
/// [`MCP_CONTRACT_VERSION`].
#[must_use]
pub fn response_schema(tool_name: &str) -> Option<Value> {
    match tool_name {
        "inspect_store" => Some(inspect_store_schema()),
        "symbol_context" => Some(symbol_context_schema()),
        "task_evidence" => Some(task_evidence_schema()),
        "store_freshness" => Some(store_freshness_schema()),
        "failure_history" => Some(failure_history_schema()),
        "record_observation" => Some(record_observation_schema()),
        "search_code" => Some(search_code_schema()),
        "symbol_at" => Some(symbol_at_schema()),
        _ => None,
    }
}

/// Returns the published JSON Schema (Draft 2020-12) for the stable error
/// envelope (`ok: false`).
///
/// Every MCP tool error — `no_match`, `stale_handle`, `ambiguous_handle`,
/// `unsupported_handle`, `missing_argument`, `daemon_not_running`,
/// `daemon_stale` — shares this envelope: `{"ok": false, "error": {"code":
/// ..., ...}}`. The `code` is stable; code-specific detail fields ride
/// alongside it under `error`.
#[must_use]
pub fn error_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Egregore MCP tool error envelope",
        "description": "Stable error shape shared by every eg mcp tool (issue #194).",
        "type": "object",
        "additionalProperties": true,
        "required": ["ok", "error"],
        "properties": {
            "ok": { "const": false },
            "error": {
                "type": "object",
                "additionalProperties": true,
                "required": ["code"],
                "properties": {
                    "code": { "type": "string" },
                    "message": { "type": "string" },
                    "field": { "type": "string" },
                    "symbol_name": { "type": "string" },
                    "id_or_handle": { "type": "string" },
                    "handle": { "type": "string" },
                    // `ambiguous_handle` carries plain handle strings;
                    // `ambiguous_symbol` (issue #192) carries one object per
                    // distinct identity (record_id + file:span handle).
                    "candidates": { "type": "array", "items": { "type": ["string", "object"] } },
                    // `ambiguous_commit_prefix` (issue #181) carries the
                    // offending prefix and the distinct-SHA count.
                    "commit_prefix": { "type": "string" },
                    "matching_commits": { "type": "integer" },
                    // `ambiguous_repository` (issue #67 / #181) carries the
                    // repository handles and whether unattributed rows were
                    // among the colliding candidates.
                    "repositories": { "type": "array", "items": { "type": "string" } },
                    "includes_unattributed_rows": { "type": "boolean" }
                }
            }
        }
    })
}

// ── Shared fragments ────────────────────────────────────────────────────────

fn str_or_null() -> Value {
    json!({ "type": ["string", "null"] })
}

fn int_or_null() -> Value {
    json!({ "type": ["integer", "null"] })
}

fn bool_schema() -> Value {
    json!({ "type": "boolean" })
}

fn trust_schema() -> Value {
    json!({
        "type": "string",
        "enum": [
            "source_derived", "verification_evidence", "agent_verified",
            "agent_unverified", "agent_contradicted", "project_state",
            "artifact", "runtime_observation", "other"
        ]
    })
}

fn span_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "additionalProperties": true,
        "required": ["start_byte", "end_byte", "start_line", "end_line"],
        "properties": {
            "start_byte": { "type": "integer" },
            "end_byte": { "type": "integer" },
            "start_line": { "type": "integer" },
            "end_line": { "type": "integer" },
            "start_column": int_or_null(),
            "end_column": int_or_null()
        }
    })
}

fn evidence_link_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "target_record_id": str_or_null(),
            "target_domain": { "type": "string" },
            "relation": { "type": "string" },
            "confidence": { "type": "string" }
        }
    })
}

fn evidence_links_schema() -> Value {
    json!({ "type": "array", "items": evidence_link_schema() })
}

/// Every returned record carries `record_id` (the stable, evidence-cited
/// handle) and a `trust` class from the closed vocabulary the CLI and daemon
/// use (`crate::query::TrustClass`).
fn record_header_properties() -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([
        ("record_id".to_owned(), json!({ "type": "string" })),
        ("kind".to_owned(), json!({ "type": "string" })),
        ("trust".to_owned(), trust_schema()),
    ])
}

fn extend_props(
    props: &mut serde_json::Map<String, Value>,
    extra: impl IntoIterator<Item = (&'static str, Value)>,
) {
    props.extend(extra.into_iter().map(|(k, v)| (k.to_owned(), v)));
}

fn source_fact_schema() -> Value {
    let mut props = record_header_properties();
    extend_props(
        &mut props,
        [
            ("name", str_or_null()),
            ("repo_relative_path", str_or_null()),
            ("span", span_schema()),
            ("git_commit", str_or_null()),
            ("valid_time", str_or_null()),
            ("language", str_or_null()),
            ("symbol_kind", str_or_null()),
        ],
    );
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "kind", "trust"],
        "properties": props
    })
}

fn observation_schema() -> Value {
    let mut props = record_header_properties();
    extend_props(
        &mut props,
        [
            ("summary", str_or_null()),
            ("text", str_or_null()),
            ("provenance_handle", str_or_null()),
            ("agent_id", str_or_null()),
            ("session_id", str_or_null()),
            ("observed_at", str_or_null()),
            ("confidence", str_or_null()),
            ("failure_kind", str_or_null()),
            ("exit_code", int_or_null()),
            ("evidence_links", evidence_links_schema()),
        ],
    );
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "kind", "trust"],
        "properties": props
    })
}

/// One row of the `decisions` section (issue #191): agent-authored decision
/// with `decision_text`, non-empty `rationale_summary`, provenance,
/// optional `confidence`, and resolved decision evidence handles.
///
/// The property names mirror the emitted [`query::ContextDecision`] row
/// exactly: `observed_at` (the record's timestamp, as on every
/// agent-memory row) and `evidence_handles` (resolved `EXPLAINS_CHANGE` /
/// `REFERENCES_TASK` handles). Targets absent from the recalled slice are
/// omitted here and reported in the answer-level `unresolved` array —
/// never invented.
fn decision_schema() -> Value {
    let mut props = record_header_properties();
    extend_props(
        &mut props,
        [
            ("summary", json!({ "type": "string" })),
            ("decision_text", str_or_null()),
            ("rationale_summary", str_or_null()),
            ("provenance_handle", str_or_null()),
            ("agent_id", str_or_null()),
            ("session_id", str_or_null()),
            ("observed_at", str_or_null()),
            ("confidence", str_or_null()),
            (
                "evidence_handles",
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": true,
                        "required": ["relation", "target_record_id", "target_kind"],
                        "properties": {
                            "relation": { "type": "string" },
                            "target_record_id": { "type": "string" },
                            "target_kind": { "type": "string" }
                        }
                    }
                }),
            ),
        ],
    );
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "kind", "trust", "decision_text", "rationale_summary"],
        "properties": props
    })
}

fn output_handle_citation_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "additionalProperties": true,
        "properties": {
            "hash": { "type": "string" },
            "bytes": { "type": "integer" }
        }
    })
}

fn patch_handle_citation_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "additionalProperties": true,
        "properties": { "path": { "type": "string" } }
    })
}

/// The generic linked-item row (tasks, acceptance criteria, artifacts,
/// verification evidence, reviews, external links). Citation metadata only —
/// never inlined payloads (redaction, issue #53).
fn linked_item_schema() -> Value {
    let mut props = record_header_properties();
    extend_props(
        &mut props,
        [
            ("summary", str_or_null()),
            ("title", str_or_null()),
            ("name", str_or_null()),
            ("text", str_or_null()),
            ("status", str_or_null()),
            ("verification_kind", str_or_null()),
            ("exit_code", int_or_null()),
            ("executed_at", str_or_null()),
            ("evidence_quality", str_or_null()),
            ("source_artifact_path", str_or_null()),
            ("source_artifact_hash", str_or_null()),
            ("stdout_handle", output_handle_citation_schema()),
            ("stderr_handle", output_handle_citation_schema()),
            ("repo_relative_path", str_or_null()),
            ("edit_kind", str_or_null()),
            ("patch_status", str_or_null()),
            ("patch_handle", patch_handle_citation_schema()),
            ("patch_bytes_hash", str_or_null()),
            ("patch_bytes_size", int_or_null()),
            (
                "target_files",
                json!({ "type": ["array", "null"], "items": { "type": "string" } }),
            ),
            ("validation_summary", str_or_null()),
            ("base_commit", str_or_null()),
            ("producer_session_id", str_or_null()),
            ("body_handle", output_handle_citation_schema()),
            ("author", str_or_null()),
            ("url", str_or_null()),
            ("system_native_id", str_or_null()),
            ("evidence_links", evidence_links_schema()),
        ],
    );
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "kind", "trust"],
        "properties": props
    })
}

/// An acceptance-criterion row is a linked item that may carry its resolved
/// verification record when its status is `verified`.
fn acceptance_criterion_schema() -> Value {
    let mut schema = linked_item_schema();
    schema["properties"]["verification_record"] = json!({
        "type": "object",
        "additionalProperties": true
    });
    schema
}

fn topology_edge_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "trust", "label", "source_id", "target_id"],
        "properties": {
            "record_id": { "type": "string" },
            "trust": trust_schema(),
            "label": { "type": "string" },
            "source_id": { "type": "string" },
            "target_id": { "type": "string" },
            "summary": str_or_null(),
            "git_commit": str_or_null(),
            "valid_time": str_or_null()
        }
    })
}

/// One `drift_history` row: the same citable handle the CLI and daemon
/// render, plus the closed-vocabulary trust class.
fn drift_row_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "record_id", "trust", "score", "before_commit", "after_commit",
            "before_valid_time", "after_valid_time", "embedding_model"
        ],
        "properties": {
            "record_id": { "type": "string" },
            "trust": trust_schema(),
            "score": { "type": "number" },
            "before_commit": { "type": "string" },
            "after_commit": { "type": "string" },
            "before_valid_time": { "type": "string" },
            "after_valid_time": { "type": "string" },
            "embedding_model": { "type": "string" },
            "repo_relative_path": { "type": "string" },
            "span": span_schema()
        }
    })
}

/// One active approved policy row folded into `symbol_context` (issue #169).
///
/// Mirrors the payload built in `mcp::symbol_context_payload`: the record ID,
/// durable kind, human-readable body, the approval-decision handle that
/// materialized the record, the activation timestamp, the
/// supersession/revocation status (always `active` — the fold never surfaces
/// superseded or revoked rows), the #114 trust class (always `other` for
/// policy rows), and the record's own scope echoed for auditability.
fn policy_row_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "record_id", "kind", "body", "approval_decision_id",
            "active_from", "status", "trust", "scope"
        ],
        "properties": {
            "record_id": { "type": "string" },
            "kind": { "type": "string" },
            "body": { "type": "string" },
            "approval_decision_id": { "type": "string" },
            "active_from": { "type": "string" },
            "status": { "type": "string" },
            "trust": trust_schema(),
            "scope": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "repo": { "type": ["string", "null"] },
                    "path_glob": { "type": ["string", "null"] },
                    "language": { "type": ["string", "null"] },
                    "lifecycle_phase": { "type": ["string", "null"] }
                }
            }
        }
    })
}

fn unresolved_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "source_record_id", "target_handle", "relation",
            "target_domain", "verification_status"
        ],
        "properties": {
            "source_record_id": { "type": "string" },
            "target_handle": { "type": "string" },
            "relation": { "type": "string" },
            "target_domain": { "type": "string" },
            "verification_status": { "const": "unresolved" }
        }
    })
}

/// Store-level domain presence (issue #196): `false` + empty section means
/// the domain is absent from the store; `true` + empty section means the
/// entity has no records in that domain.
fn store_coverage_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["code_graph", "agent_memory", "project", "artifact", "verification"],
        "properties": {
            "code_graph": bool_schema(),
            "agent_memory": bool_schema(),
            "project": bool_schema(),
            "artifact": bool_schema(),
            "verification": bool_schema()
        }
    })
}

/// The `freshness` trust-signal object stamped onto every successful tool
/// response (issue #220). Reuses the #186 contract vocabulary — no new
/// vocabulary is introduced here.
fn freshness_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "verdict", "fresh", "repository_id", "repo_path",
            "stored_snapshot", "current_head", "current_dirty", "message"
        ],
        "properties": {
            "verdict": {
                "type": "string",
                "enum": ["fresh", "stale_head", "stale_dirty", "unknown"]
            },
            "fresh": bool_schema(),
            "repository_id": { "type": "string" },
            "repo_path": { "type": "string" },
            "stored_snapshot": {
                "anyOf": [
                    {
                        "type": "object",
                        "additionalProperties": true,
                        "required": ["head", "dirty", "repository_id", "scanned_at"],
                        "properties": {
                            "head": snapshot_head_schema(),
                            "dirty": bool_schema(),
                            "repository_id": { "type": "string" },
                            "scanned_at": { "type": "string" }
                        }
                    },
                    {
                        "description": "Explicit marker for a pre-stamping store — never a silent absence, never a false `fresh`.",
                        "type": "object",
                        "required": ["state"],
                        "properties": { "state": { "const": "pre_stamping" } }
                    }
                ]
            },
            "current_head": snapshot_head_schema(),
            "current_dirty": bool_schema(),
            "message": { "type": "string" }
        }
    })
}

fn snapshot_head_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["state"],
        "properties": {
            "state": { "type": "string", "enum": ["commit", "no_git", "unborn_head"] },
            "sha": { "type": "string" }
        }
    })
}

fn schema_head(tool: &str, description: &str) -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": format!("Egregore MCP tool response: {tool}"),
        "description": description,
    })
}

/// Merges two JSON objects. Both arguments are always objects by construction;
/// a non-object `extra` is ignored rather than panicking.
fn merge_objects(mut base: Value, extra: Value) -> Value {
    let Value::Object(extra_map) = extra else {
        return base;
    };
    if let Value::Object(base_map) = &mut base {
        base_map.extend(extra_map);
    }
    base
}

// ── Per-tool success schemas ────────────────────────────────────────────────

fn inspect_store_schema() -> Value {
    let schema = schema_head(
        "inspect_store",
        "Structured store-inspection summary (issue #194).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "ok", "records", "nodes", "edges", "tombstones", "diagnostics",
            "domain_counts", "schema_versions", "unknown_schema_versions",
            "repositories", "freshness"
        ],
        "properties": {
            "ok": { "const": true },
            "snapshot_timestamp": str_or_null(),
            "records": { "type": "integer" },
            "nodes": { "type": "integer" },
            "edges": { "type": "integer" },
            "tombstones": { "type": "integer" },
            "diagnostics": { "type": "integer" },
            "domain_counts": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": { "type": "integer" }
                }
            },
            "schema_versions": {
                "type": "object",
                "additionalProperties": { "type": "integer" }
            },
            "unknown_schema_versions": {
                "type": "object",
                "additionalProperties": { "type": "integer" }
            },
            "repositories": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": true,
                    "required": ["id", "identity_summary"],
                    "properties": {
                        "id": { "type": "string" },
                        "identity_summary": { "type": "string" }
                    }
                }
            },
            "freshness": freshness_schema()
        }
    });
    merge_objects(schema, body)
}

fn symbol_context_schema() -> Value {
    let schema = schema_head(
        "symbol_context",
        "Evidence-backed symbol context, trust-separated by domain (issue #194).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "ok", "symbol_name", "source_facts", "topology_edges",
            "observations", "decisions", "project_state", "artifacts",
            "verification_evidence", "drift_history", "unresolved",
            "policy", "store_coverage", "freshness"
        ],
        "properties": {
            "ok": { "const": true },
            "symbol_name": { "type": "string" },
            "source_facts": { "type": "array", "items": source_fact_schema() },
            "topology_edges": { "type": "array", "items": topology_edge_schema() },
            "observations": { "type": "array", "items": observation_schema() },
            "decisions": { "type": "array", "items": decision_schema() },
            "project_state": { "type": "array", "items": linked_item_schema() },
            "artifacts": { "type": "array", "items": linked_item_schema() },
            "verification_evidence": { "type": "array", "items": linked_item_schema() },
            "drift_history": { "type": "array", "items": drift_row_schema() },
            "unresolved": { "type": "array", "items": unresolved_schema() },
            "policy": {
                "type": "array",
                "items": policy_row_schema(),
                "description": "Active approved user-context policy whose scope applies to the symbol (issue #169). Always present; empty when no in-scope policy applies."
            },
            "store_coverage": store_coverage_schema(),
            "freshness": freshness_schema()
        }
    });
    merge_objects(schema, body)
}

fn task_evidence_schema() -> Value {
    let schema = schema_head(
        "task_evidence",
        "Evidence-backed task context, trust-separated by domain (issue #194).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "ok", "task_id", "tasks", "acceptance_criteria", "source_facts",
            "observations", "decisions", "artifacts", "verification_evidence", "reviews",
            "external_links", "unresolved", "store_coverage", "freshness"
        ],
        "properties": {
            "ok": { "const": true },
            "task_id": { "type": "string" },
            "tasks": { "type": "array", "items": linked_item_schema() },
            "acceptance_criteria": {
                "type": "array",
                "items": acceptance_criterion_schema()
            },
            "source_facts": { "type": "array", "items": source_fact_schema() },
            "observations": { "type": "array", "items": observation_schema() },
            "decisions": { "type": "array", "items": decision_schema() },
            "artifacts": { "type": "array", "items": linked_item_schema() },
            "verification_evidence": { "type": "array", "items": linked_item_schema() },
            "reviews": { "type": "array", "items": linked_item_schema() },
            "external_links": { "type": "array", "items": linked_item_schema() },
            "unresolved": { "type": "array", "items": unresolved_schema() },
            "store_coverage": store_coverage_schema(),
            "freshness": freshness_schema()
        }
    });
    merge_objects(schema, body)
}

fn store_freshness_schema() -> Value {
    let schema = schema_head(
        "store_freshness",
        "Store-freshness verdict for the whole store (issue #194).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["ok", "freshness"],
        "properties": {
            "ok": { "const": true },
            "freshness": freshness_schema()
        }
    });
    merge_objects(schema, body)
}

/// The audit-vocabulary trust class used by failure-history rows
/// (`crate::cli::trust_class_for`), distinct from the symbol-context
/// `TrustClass` vocabulary pinned above.
fn audit_trust_class_schema() -> Value {
    json!({
        "type": "string",
        "enum": [
            "agent_authored",
            "verification_evidence",
            "source_fact",
            "project_state",
            "artifact",
            "runtime_observation",
            "other"
        ]
    })
}

/// One redaction-safe audit row (`superseding_successes`, `patch_artifacts`):
/// stable record ID, citable handle, trust class, bounded summary, and
/// hash/handle citations — never raw payload bytes.
fn audit_item_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "kind", "trust_class", "relation", "citable_handle", "summary"],
        "properties": {
            "record_id": { "type": "string" },
            "kind": { "type": "string" },
            "trust_class": audit_trust_class_schema(),
            "relation": { "type": "string" },
            "citable_handle": { "type": "string" },
            "summary": { "type": "string" },
            "summary_hash": str_or_null(),
            "name": str_or_null(),
            "title": str_or_null(),
            "repo_relative_path": str_or_null(),
            "span": { "type": ["object", "null"], "additionalProperties": true },
            "status": str_or_null(),
            "verification_kind": str_or_null(),
            "exit_code": { "type": ["integer", "null"] },
            "source_artifact_path": str_or_null(),
            "source_artifact_hash": str_or_null(),
            "stdout_hash": str_or_null(),
            "stderr_hash": str_or_null(),
            "patch_status": str_or_null(),
            "patch_bytes_hash": str_or_null(),
            "body_handle_hash": str_or_null(),
            "diff_hunk_hash": str_or_null(),
            "author": str_or_null(),
            "agent_id": str_or_null(),
            "session_id": str_or_null(),
            "observed_at": str_or_null(),
            "confidence": str_or_null(),
            "protected": bool_schema()
        }
    })
}

/// One failure-attempt row (`runtime_failures`, `agent_failures`): the audit
/// row plus the read-time `resolution_status` computed at tool-call time
/// (`still_failing` | `since_resolved`) and its `resolved_by` pointer.
fn failure_attempt_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "kind", "trust_class", "relation", "citable_handle", "summary", "resolution_status"],
        "properties": {
            "record_id": { "type": "string" },
            "kind": { "type": "string" },
            "trust_class": audit_trust_class_schema(),
            "relation": { "type": "string" },
            "citable_handle": { "type": "string" },
            "summary": { "type": "string" },
            "summary_hash": str_or_null(),
            "name": str_or_null(),
            "title": str_or_null(),
            "repo_relative_path": str_or_null(),
            "span": { "type": ["object", "null"], "additionalProperties": true },
            "status": str_or_null(),
            "verification_kind": str_or_null(),
            "failure_kind": str_or_null(),
            "executed_at": str_or_null(),
            "exit_code": { "type": ["integer", "null"] },
            "source_artifact_path": str_or_null(),
            "source_artifact_hash": str_or_null(),
            "stdout_hash": str_or_null(),
            "stderr_hash": str_or_null(),
            "patch_status": str_or_null(),
            "patch_bytes_hash": str_or_null(),
            "body_handle_hash": str_or_null(),
            "diff_hunk_hash": str_or_null(),
            "author": str_or_null(),
            "agent_id": str_or_null(),
            "session_id": str_or_null(),
            "observed_at": str_or_null(),
            "confidence": str_or_null(),
            "protected": bool_schema(),
            "resolution_status": { "type": "string", "enum": ["still_failing", "since_resolved"] },
            "resolved_by": str_or_null(),
            "matched_target": { "type": "string" }
        }
    })
}

/// One stable failure-history diagnostic: the record IDs that named an
/// unresolved target, and the relation/domain that failed.
fn failure_diagnostic_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["code", "source_record_id", "target_handle"],
        "properties": {
            "code": { "type": "string" },
            "source_record_id": { "type": "string" },
            "target_handle": { "type": "string" },
            "relation": { "type": "string" },
            "target_domain": { "type": "string" }
        }
    })
}

fn failure_history_schema() -> Value {
    let schema = schema_head(
        "failure_history",
        "Prior failed attempts for a symbol, file, or task handle, trust-separated into runtime failures, agent failures, and superseding successes (issue #188).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "ok", "handle", "target_type", "target_ids",
            "runtime_failures", "agent_failures", "superseding_successes",
            "patch_artifacts", "diagnostics", "safety_note", "freshness"
        ],
        "properties": {
            "ok": { "const": true },
            "handle": { "type": "string" },
            "target_type": {
                "type": "string",
                "enum": ["symbol", "file", "task", "source"]
            },
            "target_ids": { "type": "array", "items": { "type": "string" } },
            "runtime_failures": { "type": "array", "items": failure_attempt_schema() },
            "agent_failures": { "type": "array", "items": failure_attempt_schema() },
            "superseding_successes": { "type": "array", "items": audit_item_schema() },
            "patch_artifacts": { "type": "array", "items": audit_item_schema() },
            "diagnostics": { "type": "array", "items": failure_diagnostic_schema() },
            "safety_note": { "type": "string" },
            "freshness": freshness_schema()
        }
    });
    merge_objects(schema, body)
}

fn record_observation_schema() -> Value {
    let schema = schema_head(
        "record_observation",
        "Evidence-backed agent observation recorded into the agent_memory domain (issue #183).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["ok", "record_id"],
        "properties": {
            "ok": { "const": true },
            "record_id": {
                "type": "string",
                "description": "Stable content-addressed ID of the new Observation node (agent_memory domain); the citable evidence handle."
            },
            "records_written": {
                "type": "integer",
                "description": "Records the daemon accepted in the write batch (Agent, AgentSession, Observation nodes plus SESSION_OF and AUTHORED_BY edges)."
            }
        }
    });
    merge_objects(schema, body)
}

/// One ranked semantic match row in a `search_code` success response.
///
/// The row fields are the stable `eg query semantic` per-match fields —
/// bounded handles only (no raw source text, summaries, or payload bytes) —
/// plus the per-row confidence rendering the CLI's `--daemon` path emits.
fn search_code_match_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["record_id", "score", "confidence_band", "selection_threshold", "selection_basis"],
        "properties": {
            "record_id": {
                "type": "string",
                "description": "Stable record ID of the matched graph record (byte-identical to `eg query semantic`)."
            },
            "score": {
                "type": "number",
                "description": "Cosine relevance score; rows are ranked score-descending, ties by record ID."
            },
            "name": {
                "type": "string",
                "description": "Symbol/file name (absent when the daemon row omits it)."
            },
            "repo_relative_path": {
                "type": "string",
                "description": "Repo-relative file handle (absent when the daemon row omits it)."
            },
            "span": {
                "type": "object",
                "description": "Source span handle (start/end byte, line, column; absent when the daemon row omits it)."
            },
            "repository_id": {
                "type": "string",
                "description": "Canonical ID of the owning repository (absent for unattributed records)."
            },
            "repository": {
                "type": "string",
                "description": "Repository short handle (absent for unattributed records)."
            },
            "confidence_band": {
                "type": "string",
                "enum": ["strong", "weak"],
                "description": "Relevance band from the calibrated confidence floor (`strong` clears it, `weak` does not)."
            },
            "selection_threshold": {
                "type": "number",
                "description": "The calibrated confident threshold the band was classified against."
            },
            "selection_basis": {
                "type": "string",
                "description": "How the threshold was derived (corpus-calibrated confidence floor)."
            }
        }
    })
}

fn search_code_schema() -> Value {
    let schema = schema_head(
        "search_code",
        "Semantic code search over the embedded vector index: the same ranked matches as `eg query semantic`, with the embedding-provenance envelope and the confidence verdict (issue #182).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": [
            "ok", "query", "limit", "matches",
            "embedding_provenance", "confidence", "freshness"
        ],
        "properties": {
            "ok": { "const": true },
            "query": {
                "type": "string",
                "description": "The natural-language query, echoed verbatim."
            },
            "limit": {
                "type": "integer",
                "description": "The effective match limit (default 10, clamped to 100)."
            },
            "matches": {
                "type": "array",
                "description": "Ranked semantic matches, daemon order (score-descending, ties by record ID); empty with a `message` marker on no-match.",
                "items": search_code_match_schema()
            },
            "embedding_provenance": {
                "type": ["object", "null"],
                "description": "The issue #243 provenance envelope: query/index model identity, metric, index fingerprint, and the model-match verdict (forwarded verbatim from the daemon)."
            },
            "confidence": {
                "type": ["object", "null"],
                "description": "The issue #221 confidence verdict (verdict, best score, thresholds, selection basis; forwarded verbatim from the daemon)."
            },
            "compatibility_note": {
                "type": "string",
                "description": "Discloses the daemon-path embedding compatibility caveat (issue #104)."
            },
            "message": {
                "type": "string",
                "description": "Present on the explicit empty answer (`no_semantic_matches`), naming the stable marker."
            },
            "freshness": freshness_schema()
        }
    });
    merge_objects(schema, body)
}

/// The published success-response schema for `symbol_at` (issue #181).
///
/// The `symbol` row carries the citable fields — `record_id`, `name`,
/// `kind`, `repo_relative_path`, `span`, `git_commit`, `valid_time` — whose
/// CLI-emitted subset is byte-equal to `eg query symbol --at/--as-of` for
/// the same store and inputs.
fn symbol_at_schema() -> Value {
    let schema = schema_head(
        "symbol_at",
        "Temporal symbol lookup: the symbol's state at a Git commit (full SHA or unique prefix) or as of an RFC 3339 valid-time instant — the temporal counterpart to `symbol_context`, mirroring `eg query symbol --at` / `--as-of` (issue #181).",
    );
    let body = json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["ok", "symbol_name", "selector", "symbol", "freshness"],
        "properties": {
            "ok": { "const": true },
            "symbol_name": {
                "type": "string",
                "description": "The looked-up symbol name, echoed verbatim."
            },
            "selector": {
                "type": "object",
                "additionalProperties": true,
                "description": "The validated temporal selector, echoed verbatim.",
                "required": ["kind", "value"],
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["commit", "as_of"],
                        "description": "Which temporal axis answered."
                    },
                    "value": {
                        "type": "string",
                        "description": "The caller's commit SHA/prefix or RFC 3339 instant."
                    }
                }
            },
            "symbol": temporal_symbol_row_schema(),
            "freshness": freshness_schema()
        }
    });
    merge_objects(schema, body)
}

/// The citable symbol-version row `symbol_at` returns: bounded citation
/// handles only, never raw source text.
fn temporal_symbol_row_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "description": "The symbol's state at the selected commit/instant. `record_id`, `name`, `kind`, `repo_relative_path`, `span`, `git_commit`, and `valid_time` are byte-equal to `eg query symbol --at` / `--as-of` for the same store and inputs.",
        "required": [
            "record_id", "name", "kind", "repo_relative_path",
            "span", "git_commit", "valid_time"
        ],
        "properties": {
            "record_id": { "type": "string" },
            "name": { "type": "string" },
            "kind": { "type": "string", "const": "Symbol" },
            "repo_relative_path": str_or_null(),
            "span": span_schema(),
            "git_commit": str_or_null(),
            "valid_time": str_or_null()
        }
    })
}
