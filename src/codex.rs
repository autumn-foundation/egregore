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
//! pass through a caller-supplied redaction closure before being stored. The
//! default [`ImportOptions`] applies the v1 redaction policy via
//! [`crate::redaction::redact_value`]. Pass-through requires an explicit
//! [`ImportOptions::passthrough`].
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

use std::collections::HashMap;
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
    /// The default closure is [`crate::redaction::redact_value`], which applies
    /// the v1 redaction policy from `docs/schema/redaction.md`. Pass-through
    /// (no redaction) is only permitted on explicit dry-run or test paths via
    /// [`ImportOptions::passthrough`].
    pub redact: Box<dyn Fn(&str) -> String + Send + Sync>,
    /// Policy version stamped on every emitted node record, or `None` for passthrough.
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
    ///
    /// Only use this for dry-run or test invocations where redaction is not required.
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
    /// Completion status — used to derive tool status when output is absent (expected tier).
    #[serde(default)]
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

/// Coerce a JSON value to text, falling back to JSON serialization for non-strings.
/// Returns `None` only for JSON `null`.
fn json_value_to_text(v: &serde_json::Value) -> Option<String> {
    if v.is_null() {
        None
    } else {
        Some(v.as_str().map_or_else(|| v.to_string(), str::to_owned))
    }
}

fn parse_output_field(raw: &str) -> ParsedOutput {
    // Only treat as structured output when valid JSON *and* an object.
    // Arrays, strings, and other JSON scalars are stored as raw stdout text.
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .filter(serde_json::Value::is_object)
        .map_or_else(
            || ParsedOutput {
                exit_code: None,
                stdout: Some(raw.to_owned()),
                stderr: None,
            },
            |val| {
                let exit_code = val.get("exit_code").and_then(serde_json::Value::as_i64);
                let stdout = val
                    .get("stdout")
                    .and_then(json_value_to_text)
                    .filter(|s| !s.is_empty());
                let stderr = val
                    .get("stderr")
                    .and_then(json_value_to_text)
                    .filter(|s| !s.is_empty());
                // If none of the expected shell-output keys are present, the object
                // is a non-shell tool payload: preserve the raw text so downstream
                // records (CommandRun, Failure) still have meaningful content.
                if exit_code.is_none() && stdout.is_none() && stderr.is_none() {
                    ParsedOutput {
                        exit_code: None,
                        stdout: Some(raw.to_owned()),
                        stderr: None,
                    }
                } else {
                    ParsedOutput {
                        exit_code,
                        stdout,
                        stderr,
                    }
                }
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
        "sed" => parts.iter().any(|p| {
            p == "-i" || p.starts_with("-i") || p == "--in-place" || p.starts_with("--in-place=")
        }),
        // tee is a file edit only when it has a real file operand (not just piping to stdout).
        "tee" => extract_target_file_from_parts(parts).is_some(),
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
    // Maps call_id → index in the target turn's tool_calls for output correlation.
    // Multiple calls can be in flight simultaneously; cleared on each new assistant turn.
    let mut pending_call_ids: HashMap<String, usize> = HashMap::new();
    let mut unknown_indices: Vec<usize> = Vec::new();
    let mut interruptions: Vec<CodexInterrupted> = Vec::new();

    for (evt_idx, (line_idx, event)) in events.into_iter().enumerate() {
        match event {
            // Flavor is determined exclusively from the first parsed event (index 0).
            // A header at any later position is silently ignored per the ADR contract.
            CodexEvent::Session(h) if evt_idx == 0 => {
                flavor = SessionFlavor::Session(h);
            }
            CodexEvent::Rollout(h) if evt_idx == 0 => {
                flavor = SessionFlavor::Rollout(h);
            }
            CodexEvent::Message(m) if m.role == "assistant" => {
                if let Some(turn) = current_turn.take() {
                    turns.push(turn);
                }
                pending_call_ids.clear();
                current_turn = Some(TurnData {
                    turn_index: turns.len() as u64,
                    message: m,
                    tool_calls: Vec::new(),
                });
            }
            // Late/duplicate headers and non-assistant messages are context-only; skip.
            CodexEvent::Session(_) | CodexEvent::Rollout(_) | CodexEvent::Message(_) => {}
            CodexEvent::FunctionCall(fc) => {
                // Pre-allocate a slot in the turn with no output yet.
                // When the matching output arrives it fills in the slot by index.
                let call_id = fc.call_id.clone();
                if let Some(turn) = current_turn.as_mut() {
                    let idx = turn.tool_calls.len();
                    pending_call_ids.insert(call_id, idx);
                    turn.tool_calls.push((fc, None));
                } else if let Some(turn) = turns.last_mut() {
                    let idx = turn.tool_calls.len();
                    pending_call_ids.insert(call_id, idx);
                    turn.tool_calls.push((fc, None));
                } else {
                    // No turn context at all — treat as unrecognized
                    unknown_indices.push(line_idx);
                }
            }
            CodexEvent::FunctionCallOutput(fco) => {
                if let Some(&idx) = pending_call_ids.get(&fco.call_id) {
                    // Fill the pre-allocated slot for this call_id.
                    if let Some(turn) = current_turn.as_mut() {
                        turn.tool_calls[idx].1 = Some(fco);
                    } else if let Some(turn) = turns.last_mut() {
                        turn.tool_calls[idx].1 = Some(fco);
                    }
                } else {
                    // No matching function_call for this output — orphaned
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
#[allow(clippy::too_many_lines)]
pub fn import_codex(path: &Path, opts: &ImportOptions) -> Result<Graph> {
    let raw_bytes = std::fs::read(path).map_err(|e| crate::CodegraphError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    })?;

    let source_artifact_hash = blake3_hex(&raw_bytes);
    let source_artifact_path = path.to_string_lossy().into_owned();

    // Parse all lines. Lines that fail JSON parsing are counted as malformed and
    // excluded from grouping. Lines with valid JSON but an unrecognized `type`
    // become CodexEvent::Unknown (via #[serde(other)]) and are grouped as future-
    // format events, producing Diagnostic records instead of a hard failure.
    // Use lossy UTF-8 — Codex files should be UTF-8; mojibake is best-effort.
    let raw_str = String::from_utf8_lossy(&raw_bytes);

    let mut malformed_count = 0usize;
    let mut parsed: Vec<(usize, CodexEvent)> = Vec::new();
    for (i, line) in raw_str.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<CodexEvent>(line) {
            Ok(event) => parsed.push((i, event)),
            Err(_) => {
                // Distinguish between structurally invalid JSON (truly malformed) and
                // valid JSON that simply doesn't match the CodexEvent schema (e.g. a
                // recognized event type missing a required field). The latter is valid
                // JSON and should degrade to CodexEvent::Unknown → Diagnostic, not be
                // silently dropped into malformed_count.
                if let Ok(serde_json::Value::Object(_)) =
                    serde_json::from_str::<serde_json::Value>(line)
                {
                    parsed.push((i, CodexEvent::Unknown));
                } else {
                    malformed_count += 1;
                }
            }
        }
    }

    // Derive stable session ID from artifact hash + importer identity.
    let session_id = opts.session_id_override.clone().unwrap_or_else(|| {
        agent_memory_stable_id(&[
            "node",
            "agent_session",
            IMPORTER_ID,
            IMPORTER_VERSION,
            &source_artifact_hash,
        ])
    });

    let grouped = group_events(parsed);

    // Reject truly empty or fully malformed files. A file containing only
    // valid-JSON lines with unrecognised event types is NOT rejected here — those
    // events produce Diagnostic records, preserving forward-compat with future
    // Codex CLI versions that add new event types.
    // A file with a recognised session/rollout header but no assistant turns is also
    // NOT rejected: the session identity is known, so at minimum an AgentSession node
    // can be emitted (common for truncated captures or header-only recordings).
    if grouped.turns.is_empty()
        && grouped.interruptions.is_empty()
        && grouped.unknown_indices.is_empty()
        && matches!(grouped.flavor, SessionFlavor::None)
    {
        return Err(crate::CodegraphError::EmptyImport {
            path: path.to_path_buf(),
        });
    }

    // Determine metadata from flavor header.
    // Timestamps are validated before use; malformed values fall back to DEFAULT_TIMESTAMP
    // so that a bad header field never propagates invalid dates into daemon-visible records.
    let (header_model, header_timestamp) = match &grouped.flavor {
        SessionFlavor::Session(h) => (
            h.model.clone(),
            h.created_at.as_deref().and_then(sanitize_rfc3339),
        ),
        SessionFlavor::Rollout(h) => (
            h.model.clone(),
            h.started_at.as_deref().and_then(sanitize_rfc3339),
        ),
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
        redaction_policy_version: opts.policy_version.map(str::to_owned),
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

    // ── Malformed-lines Diagnostic (if any lines failed JSON parsing) ─────────
    if malformed_count > 0 {
        emit_malformed_lines_diagnostic(&mut graph, malformed_count, &run_id, &ctx);
    }

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

    // ── CostUsage (best-effort: emit only when at least one non-zero token count) ──
    if let Some(usage) = &msg.usage
        && (usage.input() > 0 || usage.output() > 0 || usage.total() > 0)
    {
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
    // Redact before computing handles so secrets never appear in inline payloads or hashes.
    let stdout = parsed_output
        .as_ref()
        .and_then(|p| p.stdout.as_deref())
        .map(|s| redact(s, opts));
    let stderr = parsed_output
        .as_ref()
        .and_then(|p| p.stderr.as_deref())
        .map(|s| redact(s, opts));

    let action_timestamp = ctx.default_timestamp.clone();

    let tool_call_id =
        agent_memory_stable_id(&["node", "tool_call", turn_id, &action_idx.to_string()]);
    let cmd_run_id =
        agent_memory_stable_id(&["node", "command_run", turn_id, &action_idx.to_string()]);

    let status_str = derive_tool_status(exit_code, fc.status.as_deref());

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
    let output_summary = build_output_summary(stdout.as_deref(), stderr.as_deref());
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
    // Only emit when provenance (a target file path) can be inferred.
    // Commands like `tee` with no file argument are skipped entirely.
    if fc.name == "shell"
        && is_file_edit_command_parts(&cmd_parts)
        && let Some(target) = extract_target_file_from_parts(&cmd_parts)
    {
        let file_edit_id =
            agent_memory_stable_id(&["node", "file_edit", turn_id, &action_idx.to_string()]);
        let before_hash = surrogate_hash(ctx, &target, "before", &redacted_cmd);
        let after_hash = surrogate_hash(ctx, &target, "after", &redacted_cmd);
        graph.push(make_node(
            file_edit_id.clone(),
            NodeKind::FileEdit,
            format!("FileEdit {target} turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp.clone()),
                text: Some(redacted_cmd.clone()),
                repo_relative_path: Some(target),
                edit_kind: Some("modify".to_owned()),
                before_hash: Some(before_hash),
                after_hash: Some(after_hash),
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
                text: Some(build_output_summary(stdout.as_deref(), stderr.as_deref())),
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
        .as_deref()
        .and_then(sanitize_rfc3339)
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

fn emit_malformed_lines_diagnostic(graph: &mut Graph, count: usize, run_id: &str, ctx: &ImportCtx) {
    let diag_id = agent_memory_stable_id(&[
        "node",
        "diagnostic",
        "malformed_lines",
        &count.to_string(),
        run_id,
    ]);
    graph.push(make_node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        format!("{count} malformed line(s) skipped during import"),
        ctx,
        NodeExtra::default(),
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        diag_id,
        run_id.to_owned(),
        "Malformed-lines Diagnostic belongs to AgentRun",
        ctx,
    ));
}

// ── Command helpers ───────────────────────────────────────────────────────────

fn tool_kind_for(tool_name: &str, cmd_parts: &[String]) -> String {
    // Values must be in daemon's TOOL_KIND_VALUES:
    // bash | file_edit | file_read | search | network_request | code_execution | other
    match tool_name {
        "shell" => {
            if is_patch_command_parts(cmd_parts) {
                "other".to_owned()
            } else if is_file_edit_command_parts(cmd_parts) {
                "file_edit".to_owned()
            } else {
                "bash".to_owned()
            }
        }
        _ => "other".to_owned(),
    }
}

fn extract_target_file_from_parts(parts: &[String]) -> Option<String> {
    // Skip parts[0] (the command token itself) — we want an operand, not the
    // command name. Without this guard, `tee` with no file argument would
    // return "tee" as the repo_relative_path.
    // We intentionally omit any '/' or '.' requirement so extension-less
    // targets like Makefile, Dockerfile, and LICENSE are captured.
    parts
        .get(1..)
        .unwrap_or(&[])
        .iter()
        .rev()
        .find(|p| !p.starts_with('-') && !is_sed_expression(p))
        .cloned()
}

fn is_sed_expression(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 4 || b[0] != b's' {
        return false;
    }
    let delim = b[1];
    // Exclude alphanumeric, underscore, and '.' delimiters.
    // '.' is used in filenames (s.conf, s.yaml) but is never a practical sed delimiter.
    if delim.is_ascii_alphanumeric() || delim == b'_' || delim == b'.' {
        return false;
    }
    // A valid sed substitution is s<d>pattern<d>replacement[<d>flags].
    // Find the first occurrence of delim in b[2..]; there must be a second one after it.
    let rest = &b[2..];
    rest.iter()
        .position(|&c| c == delim)
        .is_some_and(|pos| rest[pos + 1..].contains(&delim))
}

fn build_output_summary(stdout: Option<&str>, stderr: Option<&str>) -> String {
    const MAX_LEN: usize = 500;
    let combined = match (stdout, stderr) {
        (Some(o), Some(e)) if !e.is_empty() => format!("{o}\n{e}"),
        (Some(o), _) => o.to_owned(),
        (_, Some(e)) => e.to_owned(),
        (None, None) => String::new(),
    };
    if combined.len() > MAX_LEN {
        format!("{}…", safe_truncate(&combined, MAX_LEN))
    } else {
        combined
    }
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

/// Derive `ToolCall` status from `exit_code` and the `function_call` event's own `status` field.
/// When no output arrived (`exit_code` is `None`), the call's status field reveals whether
/// it was interrupted/cancelled rather than simply missing its output.
fn derive_tool_status(exit_code: Option<i64>, fc_status: Option<&str>) -> &'static str {
    match exit_code {
        Some(0) => "succeeded",
        Some(_) => "failed",
        // When no output payload arrived we cannot confirm success — "completed"
        // only signals lifecycle end, not command success, so it stays "unknown".
        None => match fc_status {
            Some("incomplete" | "cancelled" | "interrupted") => "interrupted",
            _ => "unknown",
        },
    }
}

/// Validate a timestamp string as plausible RFC3339.
///
/// Checks structural positions, numeric ranges, timezone designator (including the
/// full offset for `+`/`-` forms), and calendar validity (including leap years).
/// Values like `2025-99-99T99:99:99Z`, `2025-02-31T12:00:00Z`,
/// `2025-01-01T00:00:00BAD`, and `2025-01-01T00:00:00+24:99` are all rejected.
/// Returns `None` for any malformed input so callers fall back to `DEFAULT_TIMESTAMP`.
fn sanitize_rfc3339(ts: &str) -> Option<String> {
    let b = ts.as_bytes();
    if b.len() < 20 {
        return None;
    }
    // Structural separators: YYYY-MM-DDTHH:MM:SS
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    // All digit positions must be ASCII digits.
    for &pos in &[0usize, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
        if !b[pos].is_ascii_digit() {
            return None;
        }
    }
    let month = (b[5] - b'0') * 10 + (b[6] - b'0');
    let day = (b[8] - b'0') * 10 + (b[9] - b'0');
    let hour = (b[11] - b'0') * 10 + (b[12] - b'0');
    let minute = (b[14] - b'0') * 10 + (b[15] - b'0');
    let second = (b[17] - b'0') * 10 + (b[18] - b'0');
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Validate day against actual days-in-month including leap-year February.
    let year = u16::from(b[0] - b'0') * 1000
        + u16::from(b[1] - b'0') * 100
        + u16::from(b[2] - b'0') * 10
        + u16::from(b[3] - b'0');
    let days_in_month: u8 = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let is_leap =
                (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
            if is_leap { 29 } else { 28 }
        }
        _ => return None,
    };
    if day < 1 || day > days_in_month {
        return None;
    }
    // Validate the complete timezone tail starting at byte 19.
    // Each branch validates both structure and value ranges for the respective form.
    match b[19] {
        b'Z' => {
            // Z must be the final character; trailing garbage (e.g. "ZZZZ") is not RFC3339.
            if b.len() != 20 {
                return None;
            }
        }
        b'+' | b'-' => {
            // Numeric offset: sign + HH:MM = exactly 6 bytes → total length 25.
            if b.len() != 25
                || !b[20].is_ascii_digit()
                || !b[21].is_ascii_digit()
                || b[22] != b':'
                || !b[23].is_ascii_digit()
                || !b[24].is_ascii_digit()
            {
                return None;
            }
            let off_h = (b[20] - b'0') * 10 + (b[21] - b'0');
            let off_m = (b[23] - b'0') * 10 + (b[24] - b'0');
            if off_h > 23 || off_m > 59 {
                return None;
            }
        }
        b'.' => {
            // Fractional seconds: one or more ASCII digits, then Z or a numeric offset.
            let mut i = 20usize;
            if i >= b.len() || !b[i].is_ascii_digit() {
                return None;
            }
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            match b.get(i) {
                Some(&b'Z') => {
                    if i + 1 != b.len() {
                        return None;
                    }
                }
                Some(&(b'+' | b'-')) => {
                    let rem = &b[i..];
                    // sign + HH:MM = exactly 6 bytes
                    if rem.len() != 6
                        || !rem[1].is_ascii_digit()
                        || !rem[2].is_ascii_digit()
                        || rem[3] != b':'
                        || !rem[4].is_ascii_digit()
                        || !rem[5].is_ascii_digit()
                    {
                        return None;
                    }
                    let off_h = (rem[1] - b'0') * 10 + (rem[2] - b'0');
                    let off_m = (rem[4] - b'0') * 10 + (rem[5] - b'0');
                    if off_h > 23 || off_m > 59 {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        // Not a valid RFC3339 timezone designator (e.g. 'B' from "BAD").
        _ => return None,
    }
    Some(ts.to_owned())
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
    redaction_policy_version: Option<String>,
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
        visibility: None,
        signature: None,
        doc: None,
        call_context: None,
        note: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
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
        redaction_policy_version: ctx.redaction_policy_version.clone(),
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
        stdout_handle: extra.stdout_handle,
        stderr_handle: extra.stderr_handle,
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
        dependency: None,
        log: None,
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
        resolution: None,
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

    #[test]
    fn flavor_pinned_to_first_header_event() {
        // A session header followed by a rollout header: flavor must stay Session.
        let events = vec![
            (
                0,
                CodexEvent::Session(CodexSessionHeader {
                    id: Some("sess_1".to_owned()),
                    model: Some("model-1".to_owned()),
                    created_at: Some("2025-01-01T00:00:00Z".to_owned()),
                    ..Default::default()
                }),
            ),
            (
                1,
                CodexEvent::Rollout(CodexRolloutHeader {
                    run_id: Some("run_2".to_owned()),
                    model: Some("model-2".to_owned()),
                    ..Default::default()
                }),
            ),
            (
                2,
                CodexEvent::Message(CodexMessage {
                    role: "assistant".to_owned(),
                    content: vec![],
                    id: None,
                    status: None,
                    usage: None,
                }),
            ),
        ];
        let grouped = group_events(events);
        assert!(
            matches!(grouped.flavor, SessionFlavor::Session(ref h) if h.model.as_deref() == Some("model-1")),
            "later Rollout header must not overwrite first Session header"
        );
    }

    #[test]
    fn late_header_leaves_flavor_none() {
        // First event is a user message (not a header); session header comes later.
        // Per ADR contract, flavor must remain None.
        let events = vec![
            (
                0,
                CodexEvent::Message(CodexMessage {
                    role: "user".to_owned(),
                    content: vec![],
                    id: None,
                    status: None,
                    usage: None,
                }),
            ),
            (
                1,
                CodexEvent::Session(CodexSessionHeader {
                    id: Some("late_sess".to_owned()),
                    model: Some("late-model".to_owned()),
                    ..Default::default()
                }),
            ),
            (
                2,
                CodexEvent::Message(CodexMessage {
                    role: "assistant".to_owned(),
                    content: vec![],
                    id: None,
                    status: None,
                    usage: None,
                }),
            ),
        ];
        let grouped = group_events(events);
        assert!(
            matches!(grouped.flavor, SessionFlavor::None),
            "session header after a non-header first event must not set flavor"
        );
    }

    #[test]
    fn derive_tool_status_incomplete_fc_is_interrupted() {
        assert_eq!(derive_tool_status(None, Some("incomplete")), "interrupted");
        assert_eq!(derive_tool_status(None, Some("cancelled")), "interrupted");
        assert_eq!(derive_tool_status(None, Some("interrupted")), "interrupted");
    }

    #[test]
    fn derive_tool_status_completed_without_output_is_unknown() {
        // completed only signals lifecycle end; without exit_code we cannot confirm success.
        assert_eq!(derive_tool_status(None, Some("completed")), "unknown");
        assert_eq!(derive_tool_status(None, None), "unknown");
    }

    #[test]
    fn derive_tool_status_exit_code_overrides_fc_status() {
        assert_eq!(derive_tool_status(Some(0), Some("incomplete")), "succeeded");
        assert_eq!(derive_tool_status(Some(1), Some("completed")), "failed");
    }

    #[test]
    fn sanitize_rfc3339_accepts_valid() {
        assert!(sanitize_rfc3339("2025-01-01T00:00:00Z").is_some());
        assert!(sanitize_rfc3339("2025-05-26T12:34:56+00:00").is_some());
        assert!(sanitize_rfc3339("1970-01-01T00:00:00Z").is_some());
        assert!(sanitize_rfc3339("2024-02-29T00:00:00Z").is_some()); // leap year Feb 29
        assert!(sanitize_rfc3339("2025-01-01T00:00:00.123Z").is_some()); // fractional seconds
    }

    #[test]
    fn sanitize_rfc3339_rejects_malformed() {
        assert!(sanitize_rfc3339("not-a-timestamp").is_none());
        assert!(sanitize_rfc3339("2025-01-01").is_none()); // no time part
        assert!(sanitize_rfc3339("").is_none());
        // Out-of-range calendar/time values must be rejected.
        assert!(sanitize_rfc3339("2025-99-99T99:99:99Z").is_none()); // month 99
        assert!(sanitize_rfc3339("2025-13-01T00:00:00Z").is_none()); // month 13
        assert!(sanitize_rfc3339("2025-01-01T24:00:00Z").is_none()); // hour 24
        assert!(sanitize_rfc3339("2025-01-01T00:60:00Z").is_none()); // minute 60
        // Invalid timezone tail.
        assert!(sanitize_rfc3339("2025-01-01T00:00:00BAD").is_none()); // non-tz tail
        // Impossible calendar dates (days exceeding month maximum).
        assert!(sanitize_rfc3339("2025-02-31T12:00:00Z").is_none()); // Feb 31 impossible
        assert!(sanitize_rfc3339("2025-04-31T12:00:00Z").is_none()); // April 31 impossible
        assert!(sanitize_rfc3339("2025-02-29T12:00:00Z").is_none()); // Feb 29 in non-leap year
    }

    #[test]
    fn is_sed_expression_accepts_real_sed_scripts() {
        assert!(is_sed_expression("s/foo/bar/"));
        assert!(is_sed_expression("s|foo|bar|g"));
        assert!(is_sed_expression("s@old@new@"));
    }

    #[test]
    fn is_sed_expression_rejects_filenames() {
        assert!(!is_sed_expression("s.conf")); // dot delimiter excluded
        assert!(!is_sed_expression("s.yaml"));
        assert!(!is_sed_expression("setup.py")); // starts with 's', but 'e' is alphanumeric
        assert!(!is_sed_expression("src/main.rs")); // path, not a sed script
    }

    #[test]
    fn is_sed_expression_requires_min_length() {
        assert!(!is_sed_expression("s/a")); // fewer than 4 chars (s + d + 1 more d = needs 4)
        assert!(!is_sed_expression("s//"));
    }

    #[test]
    fn parse_output_field_non_string_stdout_preserved() {
        // When stdout is a JSON object, it must be serialized to a string rather than dropped.
        let raw = r#"{"exit_code":0,"stdout":{"nested":"data"},"stderr":""}"#;
        let p = parse_output_field(raw);
        assert_eq!(p.exit_code, Some(0));
        assert!(
            p.stdout.is_some(),
            "non-string stdout value was dropped instead of serialized"
        );
        let out = p.stdout.unwrap();
        assert!(
            out.contains("nested") && out.contains("data"),
            "serialized stdout should contain original content; got: {out}"
        );
    }

    #[test]
    fn json_value_to_text_null_returns_none() {
        assert!(json_value_to_text(&serde_json::Value::Null).is_none());
    }

    #[test]
    fn json_value_to_text_string_returns_string() {
        let v = serde_json::Value::String("hello".to_owned());
        assert_eq!(json_value_to_text(&v).as_deref(), Some("hello"));
    }

    #[test]
    fn json_value_to_text_object_returns_json() {
        let v = serde_json::json!({"k":"v"});
        let result = json_value_to_text(&v).expect("should be Some");
        assert!(result.contains('k') && result.contains('v'));
    }

    // ── sanitize_rfc3339: full timezone-suffix validation ──────────────────────

    #[test]
    fn sanitize_rfc3339_rejects_invalid_offset() {
        // Offset hours must be 00-23, minutes 00-59.
        assert!(sanitize_rfc3339("2025-01-01T00:00:00+24:99").is_none()); // hours > 23
        assert!(sanitize_rfc3339("2025-01-01T00:00:00-00:60").is_none()); // minutes > 59
        assert!(sanitize_rfc3339("2025-01-01T00:00:00+").is_none()); // incomplete offset
        assert!(sanitize_rfc3339("2025-01-01T00:00:00+5:30").is_none()); // not zero-padded → wrong len
        assert!(sanitize_rfc3339("2025-01-01T00:00:00+05:3").is_none()); // truncated
    }

    #[test]
    fn sanitize_rfc3339_accepts_valid_offsets() {
        assert!(sanitize_rfc3339("2025-01-01T00:00:00+05:30").is_some());
        assert!(sanitize_rfc3339("2025-01-01T00:00:00-07:00").is_some());
        assert!(sanitize_rfc3339("2025-01-01T00:00:00+23:59").is_some()); // boundary values
        assert!(sanitize_rfc3339("2025-01-01T00:00:00.456+05:30").is_some()); // fractional + offset
    }

    // ── is_file_edit_command_parts: sed --in-place=<suffix> ────────────────────

    #[test]
    fn sed_in_place_equals_suffix_is_file_edit() {
        // GNU sed --in-place=<suffix> must be detected as a file-edit command.
        let parts: Vec<String> = vec![
            "sed".to_owned(),
            "--in-place=.bak".to_owned(),
            "s/foo/bar/g".to_owned(),
            "file.py".to_owned(),
        ];
        assert!(
            is_file_edit_command_parts(&parts),
            "sed --in-place=.bak must be classified as a file-edit command"
        );
    }

    // ── is_file_edit_command_parts: tee without file arg ───────────────────────

    #[test]
    fn tee_without_file_arg_is_not_file_edit() {
        // tee with no operand (piping to stdout only) must not be classified
        // as a file-edit command — there is no file to record.
        let parts: Vec<String> = vec!["tee".to_owned()];
        assert!(
            !is_file_edit_command_parts(&parts),
            "tee with no file arg must not be a file-edit command"
        );
    }

    #[test]
    fn tee_with_file_arg_is_file_edit() {
        // tee file.log IS a file-edit; must still classify correctly.
        let parts: Vec<String> = vec!["tee".to_owned(), "file.log".to_owned()];
        assert!(
            is_file_edit_command_parts(&parts),
            "tee with a file arg must be a file-edit command"
        );
    }
}
