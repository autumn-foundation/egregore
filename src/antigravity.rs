//! Antigravity transcript JSONL importer — M5 agent-memory source.
//!
//! Parses an Antigravity session transcript JSONL file and emits typed
//! agent-memory graph records.

use serde::Deserialize;
use std::path::Path;

use crate::{
    error::Result,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, Graph, GraphRecord, NodeKind, OutputHandle,
        VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, verification_stable_id,
    },
};

/// Stable importer identifier embedded in every emitted record.
pub const IMPORTER_ID: &str = "antigravity-jsonl";
/// Importer version embedded in every emitted record and used for idempotency.
pub const IMPORTER_VERSION: &str = "0.1.0";
/// Domain value carried on every agent-memory record.
pub const DOMAIN: &str = "agent_memory";
/// Pinned Antigravity format version this importer targets.
pub const SOURCE_FORMAT_VERSION: &str = "antigravity-1.0";

const DEFAULT_TIMESTAMP: &str = "1970-01-01T00:00:00Z";
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

// ── Import options ────────────────────────────────────────────────────────────

/// Options controlling Antigravity transcript JSONL import behaviour.
pub struct ImportOptions {
    /// Redaction closure applied to every free-text field before storage.
    pub redact: Box<dyn Fn(&str) -> String + Send + Sync>,
    /// Policy version stamped on every emitted node record.
    pub policy_version: Option<&'static str>,
    /// Stable session ID override.
    pub session_id_override: Option<String>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            redact: Box::new(crate::redaction::redact_value),
            policy_version: Some(crate::redaction::REDACTION_POLICY_VERSION),
            session_id_override: None,
        }
    }
}

impl ImportOptions {
    /// Returns an `ImportOptions` with a pass-through redaction closure.
    #[must_use]
    pub fn passthrough() -> Self {
        Self {
            redact: Box::new(|s: &str| s.to_owned()),
            policy_version: None,
            session_id_override: None,
        }
    }
}

#[inline]
fn redact(value: &str, opts: &ImportOptions) -> String {
    (opts.redact)(value)
}

// ── Deserialization models ────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
struct AntigravityStep {
    step_index: u64,
    source: String,
    #[serde(rename = "type")]
    step_type: String,
    status: String,
    created_at: String,
    content: Option<String>,
    thinking: Option<String>,
    tool_calls: Option<Vec<AntigravityToolCall>>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
struct AntigravityToolCall {
    name: String,
    args: serde_json::Value,
}

// ── Grouping structures ───────────────────────────────────────────────────────

struct ToolResultSlot {
    content: Option<String>,
    is_error: bool,
    timestamp: String,
}

struct ToolCallSlot {
    tool_name: String,
    args: serde_json::Value,
    result: Option<ToolResultSlot>,
}

struct TurnData {
    turn_index: u64,
    timestamp: String,
    user_prompt: Option<String>,
    prose: Option<String>,
    thinking: Option<String>,
    tool_calls: Vec<ToolCallSlot>,
}

// ── Command classification ────────────────────────────────────────────────────

const TEST_COMMAND_PATTERNS: &[&str] = &[
    "pytest",
    "python -m pytest",
    "python3 -m pytest",
    "cargo test",
    "npm test",
    "npm run test",
    "go test",
    "make test",
    "./gradlew test",
    "mvn test",
];

fn is_test_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    TEST_COMMAND_PATTERNS
        .iter()
        .any(|pat| cmd == *pat || cmd.starts_with(&format!("{pat} ")))
}

// ── Importer core ─────────────────────────────────────────────────────────────

struct ImportCtx {
    session_id: String,
    default_timestamp: String,
    source_artifact_path: String,
    source_artifact_hash: String,
    redaction_policy_version: Option<String>,
}

#[derive(Default)]
struct NodeExtra {
    repo_relative_path: Option<String>,
    text: Option<String>,
    observed_at: Option<String>,
    turn_index: Option<u64>,
    patch_status: Option<String>,
    edit_kind: Option<String>,
    hunk_count: Option<u32>,
    linked_turn_id: Option<String>,
    tool_name: Option<String>,
    tool_kind: Option<String>,
    arguments_summary: Option<String>,
    arguments_handle: Option<OutputHandle>,
    result_handle: Option<OutputHandle>,
    produced_evidence_id: Option<String>,
    finished_at: Option<String>,
    failure_kind: Option<String>,
    exit_code: Option<i64>,
    schema_version_override: Option<u32>,
    status: Option<String>,
    domain_override: Option<&'static str>,
}

#[allow(clippy::too_many_lines)]
fn make_node(
    id: String,
    kind: NodeKind,
    summary: String,
    ctx: &ImportCtx,
    extra: NodeExtra,
) -> GraphRecord {
    let name = if kind == NodeKind::AgentSession {
        Some(summary.clone())
    } else {
        None
    };
    GraphRecord::Node {
        id,
        kind,
        schema_version: extra
            .schema_version_override
            .unwrap_or(AGENT_MEMORY_SCHEMA_VERSION),
        repo_relative_path: extra.repo_relative_path,
        span: None,
        name,
        language: None,
        symbol_kind: None,
        disambiguator: None,
        visibility: None,
        signature: None,
        doc: None,
        call_context: None,
        note: None,
        content_signature: None,
        route: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
        text: extra.text,
        superseded_by: None,
        agent_id: Some(IMPORTER_ID.to_owned()),
        agent_kind: Some("antigravity".to_owned()),
        session_id: Some(ctx.session_id.clone()),
        observed_at: extra
            .observed_at
            .clone()
            .or_else(|| Some(ctx.default_timestamp.clone())),
        ingested_at: Some(ctx.default_timestamp.clone()),
        confidence: None,
        source_handle: Some(format!(
            "{}:{}",
            ctx.source_artifact_path, ctx.source_artifact_hash
        )),
        redaction_policy_version: ctx.redaction_policy_version.clone(),
        summary,
        domain: Some(extra.domain_override.unwrap_or(DOMAIN).to_owned()),
        importer_id: Some(IMPORTER_ID.to_owned()),
        importer_version: Some(IMPORTER_VERSION.to_owned()),
        source_artifact_path: Some(ctx.source_artifact_path.clone()),
        source_artifact_hash: Some(ctx.source_artifact_hash.clone()),
        patch_status: extra.patch_status,
        base_commit: None,
        unknown_base_reason: None,
        target_files: None,
        patch_bytes_hash: None,
        patch_bytes_size: None,
        patch_handle: None,
        validation_summary: None,
        producer_session_id: None,
        edit_kind: extra.edit_kind,
        before_hash: None,
        after_hash: None,
        rename_to: None,
        hunk_count: extra.hunk_count,
        linked_patch_id: None,
        linked_turn_id: extra.linked_turn_id,
        tool_name: extra.tool_name,
        tool_kind: extra.tool_kind,
        arguments_summary: extra.arguments_summary,
        arguments_handle: extra.arguments_handle.map(Box::new),
        result_handle: extra.result_handle.map(Box::new),
        produced_evidence_id: extra.produced_evidence_id,
        started_at: extra
            .observed_at
            .clone()
            .or_else(|| Some(ctx.default_timestamp.clone())),
        finished_at: extra.finished_at,
        failure_kind: extra.failure_kind,
        exit_code: extra.exit_code,
        turn_index: extra.turn_index,
        repository_identity: None,
        source_snapshot: None,
        valid_time: None,
        valid_time_source: None,
        entity_id: None,
        title: None,
        body_handle: None,
        source_kind: None,
        source_external_link_id: None,
        assignees: None,
        labels: None,
        priority: None,
        parent_task_id: None,
        ordinal: None,
        verification_link_id: None,
        head_sha: None,
        head_ref: None,
        base_ref: None,
        merge_commit_sha: None,
        merged_at: None,
        draft: None,
        system: None,
        url: None,
        system_native_id: None,
        repository_remote: None,
        discovered_at: None,
        transaction_time: None,
        stdout_handle: None,
        stderr_handle: None,
        evidence_quality: None,
        executed_at: None,
        verification_kind: None,
        status: extra.status,
        review_kind: None,
        review_state: None,
        in_reply_to_id: None,
        author: None,
        diff_hunk_handle: None,
        review_side: None,
        review_commit_sha: None,
        identity_system: None,
        transition_kind: None,
        dependency: None,
        log: None,
        scan_coverage: None,
        embedding_model: None,
        user_context: crate::ir::UserContextFields::empty(),
        producer: None,
    }
}

fn make_edge(label: EdgeLabel, source: String, target: String, summary: &str) -> GraphRecord {
    let id = agent_memory_stable_id(&["edge", label.as_str(), &source, &target]);
    GraphRecord::Edge {
        id,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        label,
        source,
        target,
        confidence: None,
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        basis: None,
        is_exhaustive: None,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
    }
}

/// Imports an Antigravity session transcript JSONL file and returns a `Graph`.
///
/// # Errors
///
/// Returns `CodegraphError::ReadFile` if reading the file fails.
#[allow(clippy::too_many_lines)]
#[allow(clippy::missing_errors_doc)]
#[allow(clippy::collapsible_if)]
pub fn import_antigravity(path: &Path, opts: &ImportOptions) -> Result<Graph> {
    let bytes = std::fs::read(path).map_err(|e| crate::CodegraphError::ReadFile {
        path: path.to_owned(),
        source: e,
    })?;
    let file_hash = blake3::hash(&bytes).to_hex().to_string();
    let session_id = opts.session_id_override.clone().unwrap_or_else(|| {
        agent_memory_stable_id(&["node", "agent_session", IMPORTER_ID, &file_hash])
    });
    let run_id = agent_memory_stable_id(&["node", "agent_run", &session_id, "run-0"]);

    let ctx = ImportCtx {
        session_id: session_id.clone(),
        default_timestamp: DEFAULT_TIMESTAMP.to_owned(),
        source_artifact_path: path.to_string_lossy().into_owned(),
        source_artifact_hash: file_hash.clone(),
        redaction_policy_version: opts.policy_version.map(String::from),
    };

    let mut steps = Vec::new();
    let content_str = String::from_utf8_lossy(&bytes);
    for line in content_str.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(step) = serde_json::from_str::<AntigravityStep>(line) {
            steps.push(step);
        }
    }

    // Group steps into turns
    let mut turns = Vec::new();
    let mut current_turn: Option<TurnData> = None;
    let mut turn_counter = 0;

    for step in steps {
        if step.step_type == "USER_INPUT" {
            if let Some(t) = current_turn.take() {
                turns.push(t);
            }
            current_turn = Some(TurnData {
                turn_index: turn_counter,
                timestamp: step.created_at.clone(),
                user_prompt: step.content.clone(),
                prose: None,
                thinking: None,
                tool_calls: Vec::new(),
            });
            turn_counter += 1;
        } else if step.step_type == "CONVERSATION_HISTORY" || step.step_type == "SYSTEM_MESSAGE" {
            if let Some(ref mut t) = current_turn {
                if let Some(c) = &step.content {
                    let old_prompt = t.user_prompt.take().unwrap_or_default();
                    t.user_prompt = Some(format!("{}\n\n[{}] {}", old_prompt, step.step_type, c));
                }
            }
        } else if step.step_type == "PLANNER_RESPONSE" {
            if current_turn.is_none() {
                current_turn = Some(TurnData {
                    turn_index: turn_counter,
                    timestamp: step.created_at.clone(),
                    user_prompt: None,
                    prose: None,
                    thinking: None,
                    tool_calls: Vec::new(),
                });
                turn_counter += 1;
            }
            if let Some(ref mut t) = current_turn {
                if let Some(c) = &step.content {
                    t.prose = Some(c.clone());
                }
                if let Some(th) = &step.thinking {
                    t.thinking = Some(th.clone());
                }
                if let Some(tcs) = &step.tool_calls {
                    for tc in tcs {
                        t.tool_calls.push(ToolCallSlot {
                            tool_name: tc.name.clone(),
                            args: tc.args.clone(),
                            result: None,
                        });
                    }
                }
            }
        } else {
            // Match as tool result to first pending slot
            if let Some(ref mut t) = current_turn {
                if let Some(slot) = t.tool_calls.iter_mut().find(|slot| slot.result.is_none()) {
                    slot.result = Some(ToolResultSlot {
                        content: step.content.clone(),
                        is_error: step.status == "ERROR" || step.step_type == "ERROR_MESSAGE",
                        timestamp: step.created_at.clone(),
                    });
                }
            }
        }
    }
    if let Some(t) = current_turn {
        turns.push(t);
    }

    let mut graph = Graph::new();

    // ── AgentSession ──────────────────────────────────────────────────────────
    graph.push(make_node(
        session_id.clone(),
        NodeKind::AgentSession,
        format!("AgentSession antigravity {}", &file_hash[..16]),
        &ctx,
        NodeExtra::default(),
    ));

    // ── AgentRun ──────────────────────────────────────────────────────────────
    graph.push(make_node(
        run_id.clone(),
        NodeKind::AgentRun,
        "AgentRun antigravity".to_owned(),
        &ctx,
        NodeExtra::default(),
    ));

    // Run -> Session Edge
    graph.push(make_edge(
        EdgeLabel::SessionOf,
        run_id.clone(),
        session_id,
        "AgentRun belongs to AgentSession",
    ));

    // ── Emit turns ────────────────────────────────────────────────────────────
    for turn in turns {
        let turn_id =
            agent_memory_stable_id(&["node", "agent_turn", &run_id, &turn.turn_index.to_string()]);

        let turn_text = match (&turn.user_prompt, &turn.prose, &turn.thinking) {
            (Some(u), Some(p), Some(th)) => Some(format!(
                "User: {}\n\nAssistant Thinking:\n{}\n\nAssistant: {}",
                redact(u, opts),
                redact(th, opts),
                redact(p, opts)
            )),
            (Some(u), Some(p), None) => Some(format!(
                "User: {}\n\nAssistant: {}",
                redact(u, opts),
                redact(p, opts)
            )),
            (Some(u), None, Some(th)) => Some(format!(
                "User: {}\n\nAssistant Thinking:\n{}",
                redact(u, opts),
                redact(th, opts)
            )),
            (Some(u), None, None) => Some(format!("User: {}", redact(u, opts))),
            (None, Some(p), Some(th)) => Some(format!(
                "Assistant Thinking:\n{}\n\nAssistant: {}",
                redact(th, opts),
                redact(p, opts)
            )),
            (None, Some(p), None) => Some(redact(p, opts)),
            (None, None, Some(th)) => Some(format!("Assistant Thinking:\n{}", redact(th, opts))),
            (None, None, None) => None,
        };

        graph.push(make_node(
            turn_id.clone(),
            NodeKind::AgentTurn,
            format!("AgentTurn {}", turn.turn_index),
            &ctx,
            NodeExtra {
                observed_at: Some(turn.timestamp.clone()),
                turn_index: Some(turn.turn_index),
                text: turn_text,
                ..NodeExtra::default()
            },
        ));

        // Turn -> Run Edge
        graph.push(make_edge(
            EdgeLabel::AuthoredBy,
            turn_id.clone(),
            run_id.clone(),
            &format!("AgentTurn {} belongs to AgentRun", turn.turn_index),
        ));

        // Emit tool calls and results
        for (idx, tc) in turn.tool_calls.iter().enumerate() {
            let tool_call_id =
                agent_memory_stable_id(&["node", "tool_call", &turn_id, &idx.to_string()]);
            let args_str = serde_json::to_string(&tc.args).unwrap_or_default();
            let redacted_args = redact(&args_str, opts);

            let (result_text, is_error, finished_at, is_test) =
                tc.result
                    .as_ref()
                    .map_or((None, false, None, false), |res| {
                        let text = res.content.as_deref().unwrap_or("");
                        let is_test = tc.tool_name == "run_command"
                            && is_test_command(tc.args["CommandLine"].as_str().unwrap_or(""));
                        (
                            Some(redact(text, opts)),
                            res.is_error,
                            Some(res.timestamp.clone()),
                            is_test,
                        )
                    });

            let args_len = redacted_args.len() as u64;
            let args_handle = Some(OutputHandle {
                inline: if args_len <= INLINE_PAYLOAD_CEILING {
                    Some(redacted_args.clone())
                } else {
                    None
                },
                hash: blake3::hash(redacted_args.as_bytes()).to_hex().to_string(),
                bytes: args_len,
            });

            let result_handle = result_text.as_ref().map(|t| {
                let len = t.len() as u64;
                OutputHandle {
                    inline: if len <= INLINE_PAYLOAD_CEILING {
                        Some(t.clone())
                    } else {
                        None
                    },
                    hash: blake3::hash(t.as_bytes()).to_hex().to_string(),
                    bytes: len,
                }
            });

            // ── ToolCall Node ─────────────────────────────────────────────────
            graph.push(make_node(
                tool_call_id.clone(),
                NodeKind::ToolCall,
                format!("ToolCall {} turn={}", tc.tool_name, turn.turn_index),
                &ctx,
                NodeExtra {
                    observed_at: Some(turn.timestamp.clone()),
                    tool_name: Some(tc.tool_name.clone()),
                    tool_kind: Some("agent_tool".to_owned()),
                    arguments_summary: Some(redacted_args),
                    arguments_handle: args_handle,
                    result_handle,
                    finished_at: finished_at.clone(),
                    failure_kind: if is_error {
                        Some("error".to_owned())
                    } else {
                        None
                    },
                    exit_code: if is_error { Some(1) } else { Some(0) },
                    turn_index: Some(turn.turn_index),
                    linked_turn_id: Some(turn_id.clone()),
                    ..NodeExtra::default()
                },
            ));

            // ToolCall -> Turn Edge
            graph.push(make_edge(
                EdgeLabel::AuthoredBy,
                tool_call_id.clone(),
                turn_id.clone(),
                "ToolCall belongs to AgentTurn",
            ));

            // ── CommandRun / Verification / FileEdit synthesis ────────────────
            if tc.tool_name == "run_command" {
                let cmd_run_id =
                    agent_memory_stable_id(&["node", "command_run", &turn_id, &idx.to_string()]);
                let cmd_str = tc.args["CommandLine"].as_str().unwrap_or("");
                let exit_code = i64::from(is_error);

                graph.push(make_node(
                    cmd_run_id.clone(),
                    NodeKind::CommandRun,
                    format!("CommandRun {} exit={}", redact(cmd_str, opts), exit_code),
                    &ctx,
                    NodeExtra {
                        observed_at: Some(turn.timestamp.clone()),
                        arguments_summary: Some(redact(cmd_str, opts)),
                        finished_at: finished_at.clone(),
                        failure_kind: if is_error {
                            Some("error".to_owned())
                        } else {
                            None
                        },
                        exit_code: Some(exit_code),
                        turn_index: Some(turn.turn_index),
                        linked_turn_id: Some(turn_id.clone()),
                        domain_override: Some("verification"),
                        schema_version_override: Some(VERIFICATION_SCHEMA_VERSION),
                        ..NodeExtra::default()
                    },
                ));

                // CommandRun -> Turn
                graph.push(make_edge(
                    EdgeLabel::AuthoredBy,
                    cmd_run_id.clone(),
                    turn_id.clone(),
                    "CommandRun belongs to AgentTurn",
                ));

                if is_test && !is_error {
                    // Create Verification node
                    let ver_id = verification_stable_id(&["node", "verification", &cmd_run_id]);
                    graph.push(make_node(
                        ver_id.clone(),
                        NodeKind::Verification,
                        format!("Verification via {}", redact(cmd_str, opts)),
                        &ctx,
                        NodeExtra {
                            observed_at: Some(turn.timestamp.clone()),
                            produced_evidence_id: Some(cmd_run_id),
                            finished_at: finished_at.clone(),
                            exit_code: Some(0),
                            turn_index: Some(turn.turn_index),
                            linked_turn_id: Some(turn_id.clone()),
                            domain_override: Some("verification"),
                            schema_version_override: Some(VERIFICATION_SCHEMA_VERSION),
                            ..NodeExtra::default()
                        },
                    ));

                    // Verification -> Turn Edge
                    graph.push(make_edge(
                        EdgeLabel::AuthoredBy,
                        ver_id.clone(),
                        turn_id.clone(),
                        "Verification belongs to AgentTurn",
                    ));
                }
            } else if tc.tool_name == "replace_file_content"
                || tc.tool_name == "multi_replace_file_content"
                || tc.tool_name == "write_to_file"
            {
                // FileEdit synthesis
                let file_edit_id =
                    agent_memory_stable_id(&["node", "file_edit", &turn_id, &idx.to_string()]);
                let target_file = tc.args["TargetFile"].as_str().unwrap_or("(unknown)");

                graph.push(make_node(
                    file_edit_id.clone(),
                    NodeKind::FileEdit,
                    format!("FileEdit {target_file} turn={}", turn.turn_index),
                    &ctx,
                    NodeExtra {
                        observed_at: Some(turn.timestamp.clone()),
                        text: Some(format!("Edited file {target_file}")),
                        repo_relative_path: Some(target_file.to_owned()),
                        edit_kind: Some("modify".to_owned()),
                        hunk_count: Some(1),
                        linked_turn_id: Some(turn_id.clone()),
                        turn_index: Some(turn.turn_index),
                        ..NodeExtra::default()
                    },
                ));

                // FileEdit -> Turn
                graph.push(make_edge(
                    EdgeLabel::AuthoredBy,
                    file_edit_id.clone(),
                    turn_id.clone(),
                    "FileEdit belongs to AgentTurn",
                ));
            }
        }
    }

    Ok(graph)
}
