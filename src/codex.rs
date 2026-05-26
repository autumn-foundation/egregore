//! Codex session/rollout JSONL importer — M3 agent-memory source (issue #21).
//!
//! Parses a Codex session or rollout JSONL file (format `codex-cli-1.0`) and
//! emits typed agent-memory graph records following the same JSONL conventions as
//! `scan`, `scan-history`, and `import-traj`.
//!
//! Every emitted record carries: `domain`, `schema_version`, `importer_id`,
//! `importer_version`, `source_artifact_path`, `source_artifact_hash` (BLAKE3
//! of the raw JSONL bytes), and `source_format_version` (the Codex format version
//! inferred from the first event). The raw artifact body is never inlined into a
//! queryable graph field; it is preserved by handle (path + hash) on every record.
//!
//! # Flavors
//!
//! - **Session**: first line is `{"type":"session",...}`. Session ID comes from
//!   the `id` field on the header event.
//! - **Rollout**: first line is `{"type":"rollout",...}`. Run ID and session ID
//!   come from `run_id` and `session_id` on the header event.
//!
//! # Field-Stability Tiers
//!
//! See `docs/adr/codex-field-stability-tiers.md` for the authoritative table.
//! Summary:
//! - `required`: `type` on every event; `call_id` on function_call/output; `role`
//!   on message; `name` on function_call. Importer returns an error if absent.
//! - `expected`: `content`, `arguments`, `output`, `id`, `status` on message,
//!   `exit_code`/`stdout`/`stderr` inside parsed output. Emits `Diagnostic` if absent.
//! - `best-effort`: `usage`, `model`, timestamps, `reason` on interrupted. Silently
//!   degrades to absent or `unknown` metadata.
//! - `opaque`: raw stdout/stderr above `INLINE_PAYLOAD_CEILING` is stored as a
//!   hash-addressed blob handle; large patch bodies are similarly opaqued.
//!
//! # Redaction
//!
//! All free-text fields (command arguments, stdout/stderr excerpts, assistant prose)
//! pass through a caller-supplied redaction closure before being stored.
//! TODO: replace with #4 policy at the single call site in [`redact`].
//!
//! # Idempotency
//!
//! The `AgentSession` ID is derived from the BLAKE3 hash of the raw JSONL bytes
//! plus the importer version string. Re-importing the same file always produces
//! the same `AgentSession` ID regardless of when or how many times import is run.
//!
//! # Verification Trust Rule
//!
//! A `Verification` record is only emitted when a tool call whose command matches a
//! known-test-command pattern returns exit code 0. Assistant prose alone (e.g.
//! "tests pass") never promotes to a `Verification` record. The known-test-command
//! pattern list is owned by engineering; see `TEST_COMMAND_PATTERNS` below.
//!
//! # Upgrade Contract
//!
//! A new Codex version that adds fields is non-breaking (best-effort tier absorbs
//! them). A Codex version that renames or removes a `required` field requires a new
//! importer version and a JSONL `schema_version` bump on all emitted records.
//! See `docs/adr/codex-field-stability-tiers.md §Upgrade Contract`.

use std::path::Path;

use serde::Deserialize;

use crate::{
    error::Result,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, Graph, GraphRecord, NodeKind, OutputHandle,
        agent_memory_stable_id,
    },
};

// ── Importer identity ─────────────────────────────────────────────────────────

/// Stable importer identifier embedded in every emitted record.
pub const IMPORTER_ID: &str = "codex-jsonl";
/// Importer version embedded in every emitted record and used for idempotency.
pub const IMPORTER_VERSION: &str = "0.1.0";
/// Domain value carried on every agent-memory record.
pub const DOMAIN: &str = "agent_memory";

/// Pinned Codex format version this importer targets.
/// Embedded in `AgentRun` and `AgentSession` summaries; reused by M4+ importers.
#[allow(dead_code)]
pub const SOURCE_FORMAT_VERSION: &str = "codex-cli-1.0";
/// Timestamp used when no timestamp is available.
const DEFAULT_TIMESTAMP: &str = "1970-01-01T00:00:00Z";
/// Maximum bytes to inline in a handle field.
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

// ── Import options ────────────────────────────────────────────────────────────

/// Options controlling Codex JSONL import behaviour.
pub struct ImportOptions {
    /// Redaction closure applied to every free-text field before storage.
    ///
    /// TODO: replace with #4 policy at the single [`redact`] call site.
    pub redact: Box<dyn Fn(&str) -> String + Send + Sync>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            // TODO: replace with #4 policy
            redact: Box::new(|s: &str| s.to_owned()),
        }
    }
}

#[inline]
fn redact(value: &str, opts: &ImportOptions) -> String {
    (opts.redact)(value)
}

// ── Codex JSONL event types ───────────────────────────────────────────────────

/// An event line from a Codex session or rollout JSONL.
///
/// Field-stability tiers are documented in `docs/adr/codex-field-stability-tiers.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexEvent {
    /// Session flavor header (required on session files).
    Session(CodexSessionHeader),
    /// Rollout flavor header (required on rollout files).
    Rollout(CodexRolloutHeader),
    /// User or assistant message turn.
    Message(CodexMessage),
    /// Tool invocation (function call).
    FunctionCall(CodexFunctionCall),
    /// Tool output (function call result).
    FunctionCallOutput(CodexFunctionCallOutput),
    /// Session interruption marker.
    Interrupted(CodexInterrupted),
    /// Any event kind not listed above — degraded to Diagnostic.
    #[serde(other)]
    Unknown,
}

/// Session flavor header event (field-stability tier: best-effort).
#[derive(Debug, Clone, Default, Deserialize)]
struct CodexSessionHeader {
    /// Session identifier (best-effort: retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    id: Option<String>,
    /// Model identifier (best-effort).
    #[serde(default)]
    model: Option<String>,
    /// Session creation timestamp RFC 3339 (best-effort).
    #[serde(default)]
    created_at: Option<String>,
    /// System instructions (best-effort; opaque: never stored in queryable fields).
    #[serde(default)]
    #[allow(dead_code)]
    instructions: Option<String>,
}

/// Rollout flavor header event (field-stability tier: best-effort).
#[derive(Debug, Clone, Default, Deserialize)]
struct CodexRolloutHeader {
    /// Run identifier (best-effort; retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    run_id: Option<String>,
    /// Session identifier (best-effort; retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
    /// Model identifier (best-effort).
    #[serde(default)]
    model: Option<String>,
    /// Run start timestamp RFC 3339 (best-effort).
    #[serde(default)]
    started_at: Option<String>,
}

/// Message event for user or assistant turns (field-stability tier: mixed).
#[derive(Debug, Clone, Deserialize)]
struct CodexMessage {
    /// Message role — `"user"` or `"assistant"` (required tier).
    role: String,
    /// Message content blocks (expected tier; retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    content: Vec<ContentBlock>,
    /// Message identifier (expected tier; retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    id: Option<String>,
    /// Completion status (expected tier: `"completed"` | `"incomplete"` | absent).
    #[serde(default)]
    status: Option<String>,
    /// Token usage (best-effort tier: absent → no `CostUsage` record emitted).
    #[serde(default)]
    usage: Option<CodexUsage>,
}

/// A content block within a message.
#[derive(Debug, Clone, Deserialize)]
struct ContentBlock {
    /// Block type: `"input_text"`, `"output_text"`, etc. (retained for format documentation).
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    block_type: String,
    /// Block text payload (retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    text: Option<String>,
}

/// Token usage metadata for an assistant message (best-effort tier).
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(clippy::struct_field_names)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

impl CodexUsage {
    fn input(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
    }
    fn output(&self) -> u64 {
        self.output_tokens.unwrap_or(0)
    }
    fn total(&self) -> u64 {
        self.total_tokens.unwrap_or(0)
    }
}

/// Function call event — tool invocation (field-stability tier: mixed).
#[derive(Debug, Clone, Deserialize)]
struct CodexFunctionCall {
    /// Correlation identifier linking this call to its output (required tier).
    call_id: String,
    /// Tool name, e.g. `"shell"` (required tier).
    name: String,
    /// Raw arguments JSON string (expected tier).
    #[serde(default)]
    arguments: Option<String>,
    /// Function call identifier (retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    id: Option<String>,
    /// Completion status (retained for format documentation).
    #[serde(default)]
    #[allow(dead_code)]
    status: Option<String>,
}

/// Function call output event — tool result (field-stability tier: mixed).
#[derive(Debug, Clone, Deserialize)]
struct CodexFunctionCallOutput {
    /// Correlation identifier matching the `function_call` (required tier).
    call_id: String,
    /// Raw output payload — may be JSON string or plain text (expected tier).
    #[serde(default)]
    output: Option<String>,
}

/// Interruption marker event (field-stability tier: best-effort).
#[derive(Debug, Clone, Default, Deserialize)]
struct CodexInterrupted {
    /// Interruption reason (best-effort: `"user"`, `"wallclock_timeout"`, etc.).
    #[serde(default)]
    reason: Option<String>,
    /// Interruption timestamp RFC 3339 (best-effort).
    #[serde(default)]
    at: Option<String>,
}

// ── Parsed command output ─────────────────────────────────────────────────────

struct ParsedOutput {
    exit_code: Option<i64>,
    stdout: Option<String>,
    stderr: Option<String>,
}

fn parse_output_field(raw: &str) -> ParsedOutput {
    serde_json::from_str::<serde_json::Value>(raw).map_or_else(
        |_| ParsedOutput {
            exit_code: None,
            stdout: Some(raw.to_owned()),
            stderr: None,
        },
        |val| ParsedOutput {
            exit_code: val.get("exit_code").and_then(serde_json::Value::as_i64),
            stdout: val
                .get("stdout")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            stderr: val
                .get("stderr")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        },
    )
}

// ── Argument parsing ──────────────────────────────────────────────────────────

/// Extract the shell command string from a function call's `arguments` JSON.
///
/// The `shell` tool passes arguments as a JSON string containing `{"cmd": [...]}`.
/// Returns the joined command parts, or the raw arguments string if parsing fails.
fn extract_command_from_arguments(arguments: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(arguments)
        && let Some(cmd_arr) = val.get("cmd").and_then(|v| v.as_array())
    {
        let parts: Vec<&str> = cmd_arr.iter().filter_map(|v| v.as_str()).collect();
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    arguments.to_owned()
}

/// Extract individual command parts array from arguments JSON.
fn extract_cmd_parts(arguments: &str) -> Vec<String> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(arguments)
        && let Some(cmd_arr) = val.get("cmd").and_then(|v| v.as_array())
    {
        return cmd_arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
    }
    vec![arguments.to_owned()]
}

// ── Command classification ────────────────────────────────────────────────────

/// Known test command patterns (engineering-owned list per spec).
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

fn is_patch_command_parts(parts: &[String]) -> bool {
    let first = parts.first().map_or("", String::as_str);
    match first {
        "patch" => true,
        "git" => parts.get(1).map(String::as_str) == Some("apply"),
        _ => false,
    }
}

fn is_file_edit_command_parts(parts: &[String]) -> bool {
    let first = parts.first().map_or("", String::as_str);
    match first {
        "sed" => parts.contains(&"-i".to_owned()),
        "tee" => true,
        "cat" => {
            // cat > file or cat >> file (redirect)
            parts.windows(2).any(|w| w[0] == ">" || w[0] == ">>")
        }
        _ => false,
    }
}

// ── Turn grouping ─────────────────────────────────────────────────────────────

/// One logical agent turn: an assistant message with its tool calls.
struct TurnData {
    turn_index: u64,
    message: CodexMessage,
    tool_calls: Vec<(CodexFunctionCall, Option<CodexFunctionCallOutput>)>,
}

/// Header metadata from the first event of the file.
enum SessionFlavor {
    Session(CodexSessionHeader),
    Rollout(CodexRolloutHeader),
    None,
}

struct GroupedEvents {
    flavor: SessionFlavor,
    turns: Vec<TurnData>,
    unknown_indices: Vec<usize>,
    interruptions: Vec<CodexInterrupted>,
}

fn group_events(events: Vec<(usize, CodexEvent)>) -> GroupedEvents {
    let mut flavor = SessionFlavor::None;
    let mut turns: Vec<TurnData> = Vec::new();
    let mut current_turn: Option<TurnData> = None;
    let mut pending_call: Option<CodexFunctionCall> = None;
    let mut unknown_indices: Vec<usize> = Vec::new();
    let mut interruptions: Vec<CodexInterrupted> = Vec::new();

    for (line_idx, event) in events {
        match event {
            CodexEvent::Session(h) => {
                flavor = SessionFlavor::Session(h);
            }
            CodexEvent::Rollout(h) => {
                flavor = SessionFlavor::Rollout(h);
            }
            CodexEvent::Message(m) if m.role == "assistant" => {
                if let Some(turn) = current_turn.take() {
                    turns.push(turn);
                }
                // Flush any pending unmatched call
                if let Some(orphan) = pending_call.take()
                    && let Some(turn) = turns.last_mut()
                {
                    turn.tool_calls.push((orphan, None));
                }
                current_turn = Some(TurnData {
                    turn_index: turns.len() as u64,
                    message: m,
                    tool_calls: Vec::new(),
                });
            }
            CodexEvent::Message(_) => {
                // User message or other role — skip (context only)
            }
            CodexEvent::FunctionCall(fc) => {
                // Flush previous pending call without output
                if let Some(orphan) = pending_call.take()
                    && let Some(turn) = current_turn.as_mut()
                {
                    turn.tool_calls.push((orphan, None));
                }
                pending_call = Some(fc);
            }
            CodexEvent::FunctionCallOutput(fco) => {
                if let Some(pending) = pending_call.take() {
                    let output = if pending.call_id == fco.call_id {
                        Some(fco)
                    } else {
                        // Mismatched call_id — emit output as unmatched
                        unknown_indices.push(line_idx);
                        None
                    };
                    if let Some(turn) = current_turn.as_mut() {
                        turn.tool_calls.push((pending, output));
                    } else if let Some(turn) = turns.last_mut() {
                        turn.tool_calls.push((pending, output));
                    }
                } else {
                    // Orphaned output
                    unknown_indices.push(line_idx);
                }
            }
            CodexEvent::Interrupted(interrupted) => {
                interruptions.push(interrupted);
            }
            CodexEvent::Unknown => {
                unknown_indices.push(line_idx);
            }
        }
    }

    // Flush final pending call
    if let Some(orphan) = pending_call.take()
        && let Some(turn) = current_turn.as_mut()
    {
        turn.tool_calls.push((orphan, None));
    }

    if let Some(turn) = current_turn {
        turns.push(turn);
    }

    GroupedEvents {
        flavor,
        turns,
        unknown_indices,
        interruptions,
    }
}

// ── Public import entry point ─────────────────────────────────────────────────

/// Import a Codex session or rollout JSONL file and return a [`Graph`] of
/// agent-memory records.
///
/// The flavor (session vs rollout) is auto-detected from the first event type.
/// Every node record carries `domain`, `importer_id`, `importer_version`,
/// `source_artifact_path`, and `source_artifact_hash` (BLAKE3 of raw bytes).
/// Free-text fields are passed through `opts.redact` before storage.
///
/// # Errors
///
/// Returns an error when the file cannot be read or contains no parseable events.
pub fn import_codex(path: &Path, opts: &ImportOptions) -> Result<Graph> {
    let raw_bytes = std::fs::read(path).map_err(|e| crate::CodegraphError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    })?;

    let source_artifact_hash = blake3_hex(&raw_bytes);
    let source_artifact_path = path.to_string_lossy().into_owned();

    // Parse all lines. Lines that fail to parse as CodexEvent are treated as Unknown.
    // Use lossy UTF-8 conversion — Codex files should be UTF-8; mojibake is treated
    // as a best-effort parse rather than a hard failure.
    let raw_str = String::from_utf8_lossy(&raw_bytes);

    let parsed: Vec<(usize, CodexEvent)> = raw_str
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            let event = serde_json::from_str::<CodexEvent>(line).unwrap_or(CodexEvent::Unknown);
            (i, event)
        })
        .collect();

    // Derive stable session ID from artifact hash + importer identity.
    let session_id = agent_memory_stable_id(&[
        "node",
        "agent_session",
        IMPORTER_ID,
        IMPORTER_VERSION,
        &source_artifact_hash,
    ]);

    let grouped = group_events(parsed);

    // Determine metadata from flavor header.
    let (header_model, header_timestamp) = match &grouped.flavor {
        SessionFlavor::Session(h) => (h.model.clone(), h.created_at.clone()),
        SessionFlavor::Rollout(h) => (h.model.clone(), h.started_at.clone()),
        SessionFlavor::None => (None, None),
    };

    let flavor_name = match &grouped.flavor {
        SessionFlavor::Session(_) => "session",
        SessionFlavor::Rollout(_) => "rollout",
        SessionFlavor::None => "unknown",
    };

    let default_timestamp = header_timestamp
        .clone()
        .unwrap_or_else(|| DEFAULT_TIMESTAMP.to_owned());

    let ctx = ImportCtx {
        source_artifact_path,
        source_artifact_hash,
        session_id: session_id.clone(),
        default_timestamp,
    };

    let mut graph = Graph::new();

    let run_id = agent_memory_stable_id(&["node", "agent_run", &session_id, "run-0"]);

    // ── AgentSession ──────────────────────────────────────────────────────────
    graph.push(make_node(
        session_id.clone(),
        NodeKind::AgentSession,
        format!(
            "AgentSession codex-{} {}",
            flavor_name,
            &ctx.source_artifact_hash[..16]
        ),
        &ctx,
        NodeExtra {
            observed_at: header_timestamp.clone(),
            agent_kind: Some("codex".to_owned()),
            ..Default::default()
        },
    ));

    // ── AgentRun ──────────────────────────────────────────────────────────────
    graph.push(make_node(
        run_id.clone(),
        NodeKind::AgentRun,
        format!(
            "AgentRun codex-{} model={}",
            flavor_name,
            header_model.as_deref().unwrap_or("unknown")
        ),
        &ctx,
        NodeExtra {
            observed_at: header_timestamp,
            agent_kind: Some("codex".to_owned()),
            ..Default::default()
        },
    ));

    // AgentRun -[SESSION_OF]-> AgentSession
    graph.push(make_edge(
        EdgeLabel::SessionOf,
        run_id.clone(),
        session_id,
        "AgentRun belongs to AgentSession",
        &ctx,
    ));

    // ── Turns ─────────────────────────────────────────────────────────────────
    for turn in &grouped.turns {
        emit_turn(&mut graph, turn, &run_id, &ctx, opts);
    }

    // ── Interruptions → Diagnostic ────────────────────────────────────────────
    for (i, interrupted) in grouped.interruptions.iter().enumerate() {
        emit_interruption_diagnostic(&mut graph, i, interrupted, &run_id, &ctx);
    }

    // ── Unknown event kinds → Diagnostic ─────────────────────────────────────
    for &line_idx in &grouped.unknown_indices {
        emit_unknown_event_diagnostic(&mut graph, line_idx, &run_id, &ctx);
    }

    Ok(graph)
}

// ── Turn emission ─────────────────────────────────────────────────────────────

fn emit_turn(
    graph: &mut Graph,
    turn: &TurnData,
    run_id: &str,
    ctx: &ImportCtx,
    opts: &ImportOptions,
) {
    let turn_index = turn.turn_index;
    let msg = &turn.message;

    // Determine turn timestamp from first tool call output or default.
    let turn_timestamp = ctx.default_timestamp.clone();

    let turn_id = agent_memory_stable_id(&["node", "agent_turn", run_id, &turn_index.to_string()]);

    // Aborted / incomplete turn status
    let is_incomplete = msg.status.as_deref() == Some("incomplete");

    // ── AgentTurn ─────────────────────────────────────────────────────────────
    graph.push(make_node(
        turn_id.clone(),
        NodeKind::AgentTurn,
        format!("AgentTurn {turn_index}"),
        ctx,
        NodeExtra {
            observed_at: Some(turn_timestamp.clone()),
            turn_index: Some(turn_index),
            agent_kind: Some("codex".to_owned()),
            status: msg.status.clone(),
            ..Default::default()
        },
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        turn_id.clone(),
        run_id.to_owned(),
        &format!("AgentTurn {turn_index} belongs to AgentRun"),
        ctx,
    ));

    // ── CostUsage (best-effort: emitted only when usage is present) ───────────
    if let Some(usage) = &msg.usage {
        emit_cost_usage(graph, turn_index, &turn_id, usage, &turn_timestamp, ctx);
    }

    // ── Failure for incomplete/aborted turns ──────────────────────────────────
    if is_incomplete && turn.tool_calls.is_empty() {
        let failure_id = agent_memory_stable_id(&["node", "failure", "aborted_turn", &turn_id]);
        graph.push(make_node(
            failure_id.clone(),
            NodeKind::Failure,
            format!("Failure aborted_turn turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: Some(turn_timestamp.clone()),
                failure_kind: Some("aborted_turn".to_owned()),
                agent_kind: Some("codex".to_owned()),
                ..Default::default()
            },
        ));
        graph.push(make_edge(
            EdgeLabel::AuthoredBy,
            failure_id,
            turn_id.clone(),
            "Failure belongs to AgentTurn",
            ctx,
        ));
    }

    // ── Tool calls ────────────────────────────────────────────────────────────
    for (action_idx, (fc, fco)) in turn.tool_calls.iter().enumerate() {
        emit_tool_action(
            graph,
            action_idx,
            &turn_id,
            turn_index,
            fc,
            fco.as_ref(),
            run_id,
            ctx,
            opts,
        );
    }
}

fn emit_cost_usage(
    graph: &mut Graph,
    turn_index: u64,
    turn_id: &str,
    usage: &CodexUsage,
    timestamp: &str,
    ctx: &ImportCtx,
) {
    let cost_id = agent_memory_stable_id(&["node", "cost_usage", turn_id, &turn_index.to_string()]);
    let summary_text = format!(
        "CostUsage turn={turn_index} input={} output={} total={}",
        usage.input(),
        usage.output(),
        usage.total(),
    );
    // Store usage as compact JSON in text field (opaque tier: not parsed downstream).
    let text_payload = serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
    })
    .to_string();

    graph.push(make_node(
        cost_id.clone(),
        NodeKind::CostUsage,
        summary_text,
        ctx,
        NodeExtra {
            observed_at: Some(timestamp.to_owned()),
            text: Some(text_payload),
            linked_turn_id: Some(turn_id.to_owned()),
            agent_kind: Some("codex".to_owned()),
            ..Default::default()
        },
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        cost_id,
        turn_id.to_owned(),
        "CostUsage belongs to AgentTurn",
        ctx,
    ));
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn emit_tool_action(
    graph: &mut Graph,
    action_idx: usize,
    turn_id: &str,
    turn_index: u64,
    fc: &CodexFunctionCall,
    fco: Option<&CodexFunctionCallOutput>,
    run_id: &str,
    ctx: &ImportCtx,
    opts: &ImportOptions,
) {
    let args_str = fc.arguments.as_deref().unwrap_or("");
    let cmd_parts = extract_cmd_parts(args_str);
    let cmd_joined = extract_command_from_arguments(args_str);
    let redacted_cmd = redact(&cmd_joined, opts);

    let parsed_output = fco
        .and_then(|o| o.output.as_deref())
        .map(parse_output_field);

    let exit_code = parsed_output.as_ref().and_then(|p| p.exit_code);
    let stdout = parsed_output.as_ref().and_then(|p| p.stdout.clone());
    let stderr = parsed_output.as_ref().and_then(|p| p.stderr.clone());

    let action_timestamp = ctx.default_timestamp.clone();

    let tool_call_id =
        agent_memory_stable_id(&["node", "tool_call", turn_id, &action_idx.to_string()]);
    let cmd_run_id =
        agent_memory_stable_id(&["node", "command_run", turn_id, &action_idx.to_string()]);

    let status_str = tool_status(exit_code);

    // ── ToolCall ──────────────────────────────────────────────────────────────
    graph.push(make_node(
        tool_call_id.clone(),
        NodeKind::ToolCall,
        format!("ToolCall {} turn={turn_index} action={action_idx}", fc.name),
        ctx,
        NodeExtra {
            observed_at: Some(action_timestamp.clone()),
            text: Some(redacted_cmd.clone()),
            linked_turn_id: Some(turn_id.to_owned()),
            tool_name: Some(fc.name.clone()),
            tool_kind: Some(tool_kind_for(&fc.name, &cmd_parts)),
            arguments_summary: Some(redacted_cmd.clone()),
            arguments_handle: Some(Box::new(output_handle(&redacted_cmd))),
            started_at: Some(action_timestamp.clone()),
            finished_at: matches!(status_str, "succeeded" | "failed")
                .then(|| action_timestamp.clone()),
            status: Some(status_str.to_owned()),
            agent_kind: Some("codex".to_owned()),
            ..Default::default()
        },
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        tool_call_id,
        turn_id.to_owned(),
        "ToolCall belongs to AgentTurn",
        ctx,
    ));

    // ── CommandRun ────────────────────────────────────────────────────────────
    let output_summary = build_output_summary(stdout.as_deref(), stderr.as_deref(), opts);
    graph.push(make_node(
        cmd_run_id.clone(),
        NodeKind::CommandRun,
        format!(
            "CommandRun exit={} turn={turn_index}",
            exit_code.map_or_else(|| "?".to_owned(), |c| c.to_string())
        ),
        ctx,
        NodeExtra {
            observed_at: Some(action_timestamp.clone()),
            text: Some(redacted_cmd.clone()),
            exit_code,
            stdout_handle: stdout.as_deref().map(|s| Box::new(output_handle(s))),
            stderr_handle: stderr.as_deref().map(|s| Box::new(output_handle(s))),
            agent_kind: Some("codex".to_owned()),
            ..Default::default()
        },
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        cmd_run_id.clone(),
        turn_id.to_owned(),
        "CommandRun belongs to AgentTurn",
        ctx,
    ));

    // ── FileEdit ──────────────────────────────────────────────────────────────
    if fc.name == "shell" && is_file_edit_command_parts(&cmd_parts) {
        let file_edit_id =
            agent_memory_stable_id(&["node", "file_edit", turn_id, &action_idx.to_string()]);
        let target = extract_target_file_from_parts(&cmd_parts);
        graph.push(make_node(
            file_edit_id.clone(),
            NodeKind::FileEdit,
            format!(
                "FileEdit {} turn={turn_index}",
                target.as_deref().unwrap_or("unknown")
            ),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp.clone()),
                text: Some(redacted_cmd.clone()),
                repo_relative_path: target.clone(),
                edit_kind: Some("modify".to_owned()),
                before_hash: Some(surrogate_hash(
                    ctx,
                    target.as_deref().unwrap_or(""),
                    "before",
                    &redacted_cmd,
                )),
                after_hash: Some(surrogate_hash(
                    ctx,
                    target.as_deref().unwrap_or(""),
                    "after",
                    &redacted_cmd,
                )),
                hunk_count: Some(1),
                linked_turn_id: Some(turn_id.to_owned()),
                agent_kind: Some("codex".to_owned()),
                ..Default::default()
            },
        ));
        graph.push(make_edge(
            EdgeLabel::AuthoredBy,
            file_edit_id,
            turn_id.to_owned(),
            "FileEdit belongs to AgentTurn",
            ctx,
        ));
    }

    // ── PatchArtifact / Failure ───────────────────────────────────────────────
    if fc.name == "shell" && is_patch_command_parts(&cmd_parts) {
        let failed = exit_code.is_some_and(|c| c != 0);
        let patch_status = if failed { "invalid" } else { "unverified" };
        let patch_id =
            agent_memory_stable_id(&["node", "patch_artifact", turn_id, &action_idx.to_string()]);
        graph.push(make_node(
            patch_id.clone(),
            NodeKind::PatchArtifact,
            format!("PatchArtifact status={patch_status} turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp.clone()),
                text: Some(redacted_cmd),
                patch_status: Some(patch_status.to_owned()),
                agent_kind: Some("codex".to_owned()),
                ..Default::default()
            },
        ));
        graph.push(make_edge(
            EdgeLabel::AuthoredBy,
            patch_id.clone(),
            turn_id.to_owned(),
            "PatchArtifact belongs to AgentTurn",
            ctx,
        ));
        graph.push(make_edge(
            EdgeLabel::ProducedPatch,
            run_id.to_owned(),
            patch_id.clone(),
            "AgentRun produced patch artifact",
            ctx,
        ));

        if failed {
            let failure_id = agent_memory_stable_id(&[
                "node",
                "failure",
                "patch_invalid",
                turn_id,
                &action_idx.to_string(),
            ]);
            graph.push(make_node(
                failure_id.clone(),
                NodeKind::Failure,
                format!("Failure patch_invalid turn={turn_index}"),
                ctx,
                NodeExtra {
                    observed_at: Some(action_timestamp.clone()),
                    text: Some(output_summary),
                    failure_kind: Some("patch_invalid".to_owned()),
                    exit_code,
                    agent_kind: Some("codex".to_owned()),
                    ..Default::default()
                },
            ));
            graph.push(make_edge(
                EdgeLabel::AuthoredBy,
                failure_id.clone(),
                turn_id.to_owned(),
                "Failure belongs to AgentTurn",
                ctx,
            ));
            graph.push(make_edge(
                EdgeLabel::FailedOn,
                failure_id,
                patch_id,
                "Failure describes invalid PatchArtifact",
                ctx,
            ));
        }
    } else if exit_code.is_some_and(|c| c != 0) && !is_patch_command_parts(&cmd_parts) {
        // Non-patch command that failed
        let failure_id = agent_memory_stable_id(&[
            "node",
            "failure",
            "command_failure",
            turn_id,
            &action_idx.to_string(),
        ]);
        graph.push(make_node(
            failure_id.clone(),
            NodeKind::Failure,
            format!(
                "Failure command_failure exit={} turn={turn_index}",
                exit_code.unwrap_or(-1)
            ),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp.clone()),
                text: Some(output_summary),
                failure_kind: Some("command_failure".to_owned()),
                exit_code,
                agent_kind: Some("codex".to_owned()),
                ..Default::default()
            },
        ));
        graph.push(make_edge(
            EdgeLabel::AuthoredBy,
            failure_id.clone(),
            turn_id.to_owned(),
            "Failure belongs to AgentTurn",
            ctx,
        ));
        graph.push(make_edge(
            EdgeLabel::FailedOn,
            failure_id,
            cmd_run_id,
            "Failure describes failed CommandRun",
            ctx,
        ));
    }

    // ── Verification (only when test command + exit_code == 0) ───────────────
    if fc.name == "shell" && is_test_command(&cmd_joined) && exit_code == Some(0) {
        let verification_id =
            agent_memory_stable_id(&["node", "verification", turn_id, &action_idx.to_string()]);
        graph.push(make_node(
            verification_id.clone(),
            NodeKind::Verification,
            format!("Verification passed turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp),
                text: Some(redact(
                    &build_output_summary(stdout.as_deref(), stderr.as_deref(), opts),
                    opts,
                )),
                exit_code,
                agent_kind: Some("codex".to_owned()),
                ..Default::default()
            },
        ));
        graph.push(make_edge(
            EdgeLabel::AuthoredBy,
            verification_id.clone(),
            turn_id.to_owned(),
            "Verification belongs to AgentTurn",
            ctx,
        ));
        graph.push(make_edge(
            EdgeLabel::ValidatedBy,
            run_id.to_owned(),
            verification_id,
            "AgentRun validated by test result",
            ctx,
        ));
    }
}

// ── Diagnostic helpers ────────────────────────────────────────────────────────

fn emit_interruption_diagnostic(
    graph: &mut Graph,
    idx: usize,
    interrupted: &CodexInterrupted,
    run_id: &str,
    ctx: &ImportCtx,
) {
    let reason = interrupted.reason.as_deref().unwrap_or("unknown");
    let timestamp = interrupted
        .at
        .clone()
        .unwrap_or_else(|| ctx.default_timestamp.clone());

    let diag_id = agent_memory_stable_id(&[
        "node",
        "diagnostic",
        "interrupted",
        reason,
        &idx.to_string(),
        run_id,
    ]);
    graph.push(make_node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        format!("Interrupted: reason={reason}"),
        ctx,
        NodeExtra {
            observed_at: Some(timestamp),
            ..Default::default()
        },
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        diag_id,
        run_id.to_owned(),
        "Interrupted Diagnostic belongs to AgentRun",
        ctx,
    ));
}

fn emit_unknown_event_diagnostic(
    graph: &mut Graph,
    line_idx: usize,
    run_id: &str,
    ctx: &ImportCtx,
) {
    let diag_id = agent_memory_stable_id(&[
        "node",
        "diagnostic",
        "unrecognized_event",
        &line_idx.to_string(),
        run_id,
    ]);
    graph.push(make_node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        format!("Unrecognized or malformed event at line {line_idx}"),
        ctx,
        NodeExtra::default(),
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        diag_id,
        run_id.to_owned(),
        "Unrecognized event Diagnostic belongs to AgentRun",
        ctx,
    ));
}

// ── Command helpers ───────────────────────────────────────────────────────────

fn tool_kind_for(tool_name: &str, cmd_parts: &[String]) -> String {
    match tool_name {
        "shell" => {
            let first = cmd_parts.first().map_or("", String::as_str);
            if is_patch_command_parts(cmd_parts) {
                "patch".to_owned()
            } else if is_file_edit_command_parts(cmd_parts) {
                "file_edit".to_owned()
            } else if TEST_COMMAND_PATTERNS
                .iter()
                .any(|p| first == p.split_whitespace().next().unwrap_or(""))
            {
                "test".to_owned()
            } else {
                "shell".to_owned()
            }
        }
        other => other.to_owned(),
    }
}

fn extract_target_file_from_parts(parts: &[String]) -> Option<String> {
    // For sed/tee, find the last argument that looks like a file path.
    parts
        .iter()
        .rev()
        .find(|p| {
            !p.starts_with('-') && (p.contains('/') || p.contains('.')) && !p.starts_with('s') // skip sed expression
        })
        .cloned()
}

fn build_output_summary(
    stdout: Option<&str>,
    stderr: Option<&str>,
    opts: &ImportOptions,
) -> String {
    const MAX_LEN: usize = 500;
    let combined = match (stdout, stderr) {
        (Some(o), Some(e)) if !e.is_empty() => format!("{o}\n{e}"),
        (Some(o), _) => o.to_owned(),
        (_, Some(e)) => e.to_owned(),
        (None, None) => String::new(),
    };
    let truncated = if combined.len() > MAX_LEN {
        format!("{}…", safe_truncate(&combined, MAX_LEN))
    } else {
        combined
    };
    redact(&truncated, opts)
}

fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

const fn tool_status(exit_code: Option<i64>) -> &'static str {
    match exit_code {
        Some(0) => "succeeded",
        Some(_) => "failed",
        None => "unknown",
    }
}

fn surrogate_hash(ctx: &ImportCtx, target: &str, phase: &str, cmd: &str) -> String {
    blake3_hex(
        format!(
            "codex-importer-v1\0{}\0{target}\0{phase}\0{cmd}",
            ctx.source_artifact_hash
        )
        .as_bytes(),
    )
}

// ── Node / edge construction helpers ─────────────────────────────────────────

struct ImportCtx {
    source_artifact_path: String,
    source_artifact_hash: String,
    session_id: String,
    default_timestamp: String,
}

#[derive(Default)]
struct NodeExtra {
    repo_relative_path: Option<String>,
    observed_at: Option<String>,
    agent_kind: Option<String>,
    text: Option<String>,
    patch_status: Option<String>,
    failure_kind: Option<String>,
    exit_code: Option<i64>,
    turn_index: Option<u64>,
    edit_kind: Option<String>,
    before_hash: Option<String>,
    after_hash: Option<String>,
    hunk_count: Option<u32>,
    linked_turn_id: Option<String>,
    tool_name: Option<String>,
    tool_kind: Option<String>,
    arguments_summary: Option<String>,
    arguments_handle: Option<Box<OutputHandle>>,
    started_at: Option<String>,
    finished_at: Option<String>,
    status: Option<String>,
    stdout_handle: Option<Box<OutputHandle>>,
    stderr_handle: Option<Box<OutputHandle>>,
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
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        repo_relative_path: extra.repo_relative_path,
        span: None,
        name,
        language: None,
        symbol_kind: None,
        disambiguator: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        text: extra.text,
        superseded_by: None,
        agent_id: Some(IMPORTER_ID.to_owned()),
        agent_kind: extra.agent_kind.or_else(|| Some("codex".to_owned())),
        session_id: Some(ctx.session_id.clone()),
        observed_at: extra
            .observed_at
            .or_else(|| Some(ctx.default_timestamp.clone())),
        ingested_at: Some(ctx.default_timestamp.clone()),
        confidence: None,
        source_handle: Some(format!(
            "{}:{}",
            ctx.source_artifact_path, ctx.source_artifact_hash
        )),
        redaction_policy_version: None,
        summary,
        domain: Some(DOMAIN.to_owned()),
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
        before_hash: extra.before_hash,
        after_hash: extra.after_hash,
        rename_to: None,
        hunk_count: extra.hunk_count,
        linked_patch_id: None,
        linked_turn_id: extra.linked_turn_id,
        tool_name: extra.tool_name,
        tool_kind: extra.tool_kind,
        arguments_summary: extra.arguments_summary,
        arguments_handle: extra.arguments_handle,
        result_handle: None,
        produced_evidence_id: None,
        started_at: extra
            .started_at
            .or_else(|| Some(ctx.default_timestamp.clone())),
        finished_at: extra.finished_at,
        failure_kind: extra.failure_kind,
        exit_code: extra.exit_code,
        turn_index: extra.turn_index,
        repository_identity: None,
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
        system: None,
        url: None,
        system_native_id: None,
        repository_remote: None,
        discovered_at: None,
        transaction_time: None,
        stdout_handle: extra.stdout_handle,
        stderr_handle: extra.stderr_handle,
        evidence_quality: None,
        executed_at: None,
        verification_kind: None,
        status: extra.status,
        user_context: crate::ir::UserContextFields::empty(),
        producer: None,
    }
}

fn make_edge(
    label: EdgeLabel,
    source: String,
    target: String,
    summary: &str,
    _ctx: &ImportCtx,
) -> GraphRecord {
    let id = agent_memory_stable_id(&["edge", label.as_str(), &source, &target]);
    GraphRecord::Edge {
        id,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        label,
        source,
        target,
        confidence: None,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
    }
}

fn output_handle(content: &str) -> OutputHandle {
    let bytes = content.len() as u64;
    OutputHandle {
        inline: (bytes <= INLINE_PAYLOAD_CEILING).then(|| content.to_owned()),
        hash: blake3_hex(content.as_bytes()),
        bytes,
    }
}

// ── BLAKE3 helper ─────────────────────────────────────────────────────────────

fn blake3_hex(bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn extract_command_from_shell_args() {
        let args = r#"{"cmd":["cat","calc.py"]}"#;
        assert_eq!(extract_command_from_arguments(args), "cat calc.py");
    }

    #[test]
    fn extract_command_from_non_shell_args() {
        let args = "some plain string";
        assert_eq!(extract_command_from_arguments(args), "some plain string");
    }

    #[test]
    fn test_command_detection() {
        assert!(is_test_command("cargo test"));
        assert!(is_test_command("cargo test --all"));
        assert!(is_test_command("python -m pytest tests/"));
        assert!(!is_test_command("cat foo.py"));
        assert!(!is_test_command("git apply foo.patch"));
    }

    #[test]
    fn patch_command_detection() {
        let parts: Vec<String> = vec!["git".to_owned(), "apply".to_owned(), "foo.patch".to_owned()];
        assert!(is_patch_command_parts(&parts));
        let parts2: Vec<String> = vec!["patch".to_owned(), "-p1".to_owned()];
        assert!(is_patch_command_parts(&parts2));
        let parts3: Vec<String> = vec!["cat".to_owned(), "foo.py".to_owned()];
        assert!(!is_patch_command_parts(&parts3));
    }

    #[test]
    fn file_edit_command_detection() {
        let sed: Vec<String> = vec![
            "sed".to_owned(),
            "-i".to_owned(),
            "s/a/b/g".to_owned(),
            "file.py".to_owned(),
        ];
        assert!(is_file_edit_command_parts(&sed));
        let tee: Vec<String> = vec!["tee".to_owned(), "-a".to_owned(), "file.py".to_owned()];
        assert!(is_file_edit_command_parts(&tee));
        let cat: Vec<String> = vec!["cat".to_owned(), "file.py".to_owned()];
        assert!(!is_file_edit_command_parts(&cat));
    }

    #[test]
    fn parse_output_field_with_json() {
        let raw = r#"{"exit_code":0,"stdout":"ok\n","stderr":""}"#;
        let p = parse_output_field(raw);
        assert_eq!(p.exit_code, Some(0));
        assert_eq!(p.stdout.as_deref(), Some("ok\n"));
        assert!(p.stderr.is_none());
    }

    #[test]
    fn parse_output_field_plain_text() {
        let raw = "plain output";
        let p = parse_output_field(raw);
        assert_eq!(p.stdout.as_deref(), Some("plain output"));
        assert!(p.exit_code.is_none());
    }

    #[test]
    fn blake3_is_stable() {
        let a = blake3_hex(b"hello");
        let b = blake3_hex(b"hello");
        assert_eq!(a, b);
        assert_ne!(blake3_hex(b"hello"), blake3_hex(b"world"));
    }

    #[test]
    fn group_events_basic_session() {
        let events = vec![
            (
                0,
                CodexEvent::Session(CodexSessionHeader {
                    id: Some("sess_1".to_owned()),
                    model: Some("o4-mini".to_owned()),
                    ..Default::default()
                }),
            ),
            (
                1,
                CodexEvent::Message(CodexMessage {
                    role: "user".to_owned(),
                    content: vec![],
                    id: None,
                    status: None,
                    usage: None,
                }),
            ),
            (
                2,
                CodexEvent::Message(CodexMessage {
                    role: "assistant".to_owned(),
                    content: vec![],
                    id: Some("msg_1".to_owned()),
                    status: Some("completed".to_owned()),
                    usage: None,
                }),
            ),
            (
                3,
                CodexEvent::FunctionCall(CodexFunctionCall {
                    call_id: "call_1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: Some(r#"{"cmd":["cat","f.py"]}"#.to_owned()),
                    id: None,
                    status: None,
                }),
            ),
            (
                4,
                CodexEvent::FunctionCallOutput(CodexFunctionCallOutput {
                    call_id: "call_1".to_owned(),
                    output: Some(
                        r#"{"exit_code":0,"stdout":"def f():pass","stderr":""}"#.to_owned(),
                    ),
                }),
            ),
        ];
        let grouped = group_events(events);
        assert_eq!(grouped.turns.len(), 1);
        assert_eq!(grouped.turns[0].tool_calls.len(), 1);
        assert!(grouped.turns[0].tool_calls[0].1.is_some());
    }
}
