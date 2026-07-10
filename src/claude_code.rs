//! Claude Code transcript JSONL importer — M4 agent-memory source (issue #52).
//!
//! Parses a Claude Code session transcript JSONL file (format `claude-code-1.0`) and
//! emits typed agent-memory graph records following the same JSONL conventions as
//! `scan`, `scan-history`, `import-traj`, and `import-codex`.
//!
//! Every emitted record carries: `domain`, `schema_version`, `importer_id`,
//! `importer_version`, `source_artifact_path`, `source_artifact_hash` (BLAKE3
//! of the raw JSONL bytes), and `source_format_version`. The raw artifact body is
//! never inlined into a queryable graph field; it is preserved by handle (path +
//! hash) on every record.
//!
//! # Event Types
//!
//! - **user**: Human turn or tool result payload. Tool results are content blocks
//!   with `type: "tool_result"` correlating back to a `tool_use_id`.
//! - **assistant**: Model response turn. Tool calls are content blocks with
//!   `type: "tool_use"`. Usage is per assistant event.
//! - **hook**: Lifecycle event (`PreToolUse`, `PostToolUse`, `Stop`, etc.).
//!   Emits `Diagnostic` records.
//! - **summary**: Condensed session summary. Treated as unknown → Diagnostic.
//!
//! # Tool Call / Result Correlation
//!
//! Tool calls in assistant events carry a `tool_use_id`. Matching tool results
//! appear in subsequent user events as `tool_result` content blocks referencing
//! the same `tool_use_id`. The importer correlates them into a single slot.
//!
//! # Verification Trust Rule
//!
//! A `Verification` record is only emitted when a `Bash` tool call whose command
//! matches a known-test-command pattern completes with `is_error: false`.
//! Assistant prose alone (e.g. "Tests pass.") never promotes to a `Verification`
//! record. The known-test-command pattern list is owned by engineering; see
//! `TEST_COMMAND_PATTERNS` below.
//!
//! # Idempotency
//!
//! The `AgentSession` ID is derived from the BLAKE3 hash of the raw JSONL bytes
//! plus the importer version string. Re-importing the same file always produces
//! the same `AgentSession` ID regardless of when or how many times import is run.
//!
//! # Redaction
//!
//! All free-text fields (command arguments, tool outputs, assistant prose) pass
//! through a caller-supplied redaction closure before being stored. The default
//! [`ImportOptions`] applies the v1 redaction policy via
//! [`crate::redaction::redact_value`]. Pass-through requires an explicit
//! [`ImportOptions::passthrough`].

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::{
    error::Result,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, Graph, GraphRecord, NodeKind, OutputHandle,
        Producer, ProducerKind, VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id,
        verification_stable_id,
    },
};

// ── Importer identity ─────────────────────────────────────────────────────────

/// Stable importer identifier embedded in every emitted record.
pub const IMPORTER_ID: &str = "claude-code-jsonl";
/// Importer version embedded in every emitted record and used for idempotency.
pub const IMPORTER_VERSION: &str = "0.1.0";
/// Domain value carried on every agent-memory record.
pub const DOMAIN: &str = "agent_memory";

/// Pinned Claude Code format version this importer targets.
pub const SOURCE_FORMAT_VERSION: &str = "claude-code-1.0";
/// Timestamp used when no timestamp is available.
const DEFAULT_TIMESTAMP: &str = "1970-01-01T00:00:00Z";
/// Maximum bytes to inline in a handle field.
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

/// Wall-clock time captured when the first import in this process begins.
static IMPORTER_STARTED_AT: LazyLock<String> =
    LazyLock::new(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

// ── Import options ────────────────────────────────────────────────────────────

/// Options controlling Claude Code transcript JSONL import behaviour.
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

// ── Claude Code JSONL event types ─────────────────────────────────────────────

/// A single content block inside a message.
#[derive(Debug, Clone, Deserialize)]
struct ContentBlock {
    /// Block type: `"text"`, `"tool_use"`, `"tool_result"`.
    #[serde(rename = "type", default)]
    block_type: String,
    /// Text payload (for `"text"` blocks).
    #[serde(default)]
    #[allow(dead_code)]
    text: Option<String>,
    /// Tool use identifier (for `"tool_use"` and `"tool_result"` blocks).
    #[serde(default)]
    id: Option<String>,
    /// Tool use correlation id (for `"tool_result"` blocks).
    #[serde(default)]
    tool_use_id: Option<String>,
    /// Tool name (for `"tool_use"` blocks).
    #[serde(default)]
    name: Option<String>,
    /// Tool input arguments (for `"tool_use"` blocks).
    #[serde(default)]
    input: Option<serde_json::Value>,
    /// Tool result content (for `"tool_result"` blocks — can be text or array).
    #[serde(default)]
    content: Option<serde_json::Value>,
    /// Whether the tool result was an error (for `"tool_result"` blocks).
    #[serde(default)]
    is_error: bool,
}

/// Message envelope inside a user or assistant event.
#[derive(Debug, Clone, Default, Deserialize)]
struct MessageEnvelope {
    /// Role: `"user"` or `"assistant"`.
    #[serde(default)]
    #[allow(dead_code)]
    role: String,
    /// Content blocks (`text`, `tool_use`, `tool_result`).
    ///
    /// Claude Code transcripts may represent user prompts as a bare string
    /// rather than a block array; the custom deserializer normalises both forms.
    #[serde(default, deserialize_with = "deserialize_content_blocks")]
    content: Vec<ContentBlock>,
    /// Token usage when carried inside the message object (best-effort; some
    /// Claude Code versions embed usage here rather than at the event top level).
    #[serde(default)]
    usage: Option<ClaudeCodeUsage>,
}

/// Deserialise `content` from either a JSON string or a `ContentBlock` array.
///
/// A bare string is normalised to a single `text` block so the rest of the
/// importer can treat both forms uniformly.
fn deserialize_content_blocks<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ContentBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => Ok(vec![ContentBlock {
            block_type: "text".to_owned(),
            text: Some(s),
            id: None,
            tool_use_id: None,
            name: None,
            input: None,
            content: None,
            is_error: false,
        }]),
        other => serde_json::from_value(other).map_err(serde::de::Error::custom),
    }
}

/// Token usage metadata for an assistant event (best-effort tier).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Default, Deserialize)]
struct ClaudeCodeUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl ClaudeCodeUsage {
    fn input(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
    }
    fn output(&self) -> u64 {
        self.output_tokens.unwrap_or(0)
    }
    fn cache_creation(&self) -> u64 {
        self.cache_creation_input_tokens.unwrap_or(0)
    }
    fn cache_read(&self) -> u64 {
        self.cache_read_input_tokens.unwrap_or(0)
    }
}

/// Condensed session summary event (`type: "summary"`).
#[derive(Debug, Clone, Default, Deserialize)]
struct SummaryEvent {
    /// The condensed context summary produced by the model.
    #[serde(default)]
    summary: Option<String>,
    /// Event timestamp RFC 3339 (best-effort).
    #[serde(default)]
    timestamp: Option<String>,
    /// Session identifier (best-effort).
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
}

/// A raw event line from a Claude Code transcript JSONL.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeCodeEvent {
    /// Human turn or tool-result payload.
    User(UserEvent),
    /// Model response turn.
    Assistant(AssistantEvent),
    /// Lifecycle hook event.
    Hook(HookEvent),
    /// Condensed session summary — stored as a Diagnostic with summary text.
    Summary(SummaryEvent),
    /// Any event kind not listed above — degraded to Diagnostic.
    #[serde(other)]
    Unknown,
}

/// User event (human message or tool results).
#[derive(Debug, Clone, Default, Deserialize)]
struct UserEvent {
    /// Message envelope containing content blocks.
    #[serde(default)]
    message: MessageEnvelope,
    /// Event timestamp RFC 3339 (best-effort).
    #[serde(default)]
    timestamp: Option<String>,
    /// Session identifier (best-effort).
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
}

/// Assistant event (model response turn).
#[derive(Debug, Clone, Default, Deserialize)]
struct AssistantEvent {
    /// Message envelope containing content blocks.
    #[serde(default)]
    message: MessageEnvelope,
    /// Token usage for this turn (best-effort).
    #[serde(default)]
    usage: Option<ClaudeCodeUsage>,
    /// Event timestamp RFC 3339 (best-effort).
    #[serde(default)]
    timestamp: Option<String>,
    /// Session identifier (best-effort).
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
}

/// Hook lifecycle event (`PreToolUse`, `PostToolUse`, `Stop`, etc.).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Default, Deserialize)]
struct HookEvent {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
}

// ── Turn grouping data structures ─────────────────────────────────────────────

/// One tool call slot within a turn.
struct ToolCallSlot {
    #[allow(dead_code)]
    tool_use_id: String,
    tool_name: String,
    input: serde_json::Value,
    result: Option<ToolCallResult>,
    /// Timestamp from the assistant event that issued the tool call (`started_at`).
    start_timestamp: Option<String>,
    /// Timestamp from the user event that delivered the tool result (`finished_at`).
    result_timestamp: Option<String>,
}

/// The result of a tool call (from the matching `tool_result` block).
struct ToolCallResult {
    content: Option<String>,
    is_error: bool,
}

/// One logical agent turn: an assistant event with its tool calls.
struct TurnData {
    turn_index: u64,
    timestamp: Option<String>,
    usage: Option<ClaudeCodeUsage>,
    /// Text content blocks from the assistant turn, concatenated.
    prose: Option<String>,
    /// Preceding plain user-prompt text (not a tool-result block).
    user_prompt: Option<String>,
    tool_calls: Vec<ToolCallSlot>,
    /// Maps `tool_use_id` → index in `tool_calls` for result correlation.
    id_to_slot: HashMap<String, usize>,
}

struct GroupedEvents {
    turns: Vec<TurnData>,
    hook_events: Vec<HookEvent>,
    summary_events: Vec<(usize, SummaryEvent)>,
    unknown_indices: Vec<usize>,
}

/// Returns a mutable reference to the active turn: current if Some, else last done.
///
/// Must be defined before `group_events` at module level (not nested inside it).
const fn active_turn_mut<'a>(
    current: &'a mut Option<TurnData>,
    done: &'a mut [TurnData],
) -> Option<&'a mut TurnData> {
    if current.is_some() {
        current.as_mut()
    } else {
        done.last_mut()
    }
}

#[allow(clippy::too_many_lines)]
fn group_events(events: Vec<(usize, ClaudeCodeEvent)>) -> GroupedEvents {
    let mut turns: Vec<TurnData> = Vec::new();
    let mut current_turn: Option<TurnData> = None;
    let mut hook_events: Vec<HookEvent> = Vec::new();
    let mut summary_events: Vec<(usize, SummaryEvent)> = Vec::new();
    let mut unknown_indices: Vec<usize> = Vec::new();
    let mut pending_user_prompt: Option<String> = None;

    for (line_idx, event) in events {
        match event {
            ClaudeCodeEvent::Assistant(ae) => {
                // Start a new turn. Push any in-progress turn to done first.
                if let Some(t) = current_turn.take() {
                    turns.push(t);
                }
                let turn_index = turns.len() as u64;
                let usage = ae.usage.or_else(|| ae.message.usage.clone());
                let prose_parts: Vec<&str> = ae
                    .message
                    .content
                    .iter()
                    .filter(|b| b.block_type == "text")
                    .filter_map(|b| b.text.as_deref())
                    .filter(|s| !s.trim().is_empty())
                    .collect();
                let prose = if prose_parts.is_empty() {
                    None
                } else {
                    Some(prose_parts.join("\n"))
                };
                let mut turn = TurnData {
                    turn_index,
                    timestamp: ae.timestamp.clone(),
                    usage,
                    prose,
                    user_prompt: pending_user_prompt.take(),
                    tool_calls: Vec::new(),
                    id_to_slot: HashMap::new(),
                };
                for block in &ae.message.content {
                    if block.block_type == "tool_use"
                        && let Some(ref id) = block.id
                    {
                        let slot_idx = turn.tool_calls.len();
                        turn.id_to_slot.insert(id.clone(), slot_idx);
                        turn.tool_calls.push(ToolCallSlot {
                            tool_use_id: id.clone(),
                            tool_name: block.name.clone().unwrap_or_default(),
                            input: block.input.clone().unwrap_or(serde_json::Value::Null),
                            result: None,
                            start_timestamp: ae.timestamp.clone(),
                            result_timestamp: None,
                        });
                    }
                }
                current_turn = Some(turn);
            }
            ClaudeCodeEvent::User(ue) => {
                // Check if this user event contains tool_result blocks.
                let has_tool_results = ue
                    .message
                    .content
                    .iter()
                    .any(|b| b.block_type == "tool_result");

                if has_tool_results {
                    // Fill in results in the active turn.
                    if let Some(active) = active_turn_mut(&mut current_turn, &mut turns) {
                        for block in &ue.message.content {
                            if block.block_type == "tool_result" {
                                let id = block.tool_use_id.as_deref().unwrap_or("").to_owned();
                                if let Some(&slot_idx) = active.id_to_slot.get(&id) {
                                    let content_text = extract_content_text(block.content.as_ref());
                                    active.tool_calls[slot_idx].result = Some(ToolCallResult {
                                        content: content_text,
                                        is_error: block.is_error,
                                    });
                                    active.tool_calls[slot_idx]
                                        .result_timestamp
                                        .clone_from(&ue.timestamp);
                                }
                            }
                        }
                    }
                } else {
                    // Plain human message — flush current turn and capture prompt text.
                    if let Some(t) = current_turn.take() {
                        turns.push(t);
                    }
                    let text_parts: Vec<&str> = ue
                        .message
                        .content
                        .iter()
                        .filter(|b| b.block_type == "text")
                        .filter_map(|b| b.text.as_deref())
                        .filter(|s| !s.trim().is_empty())
                        .collect();
                    if !text_parts.is_empty() {
                        pending_user_prompt = Some(text_parts.join("\n"));
                    }
                }
            }
            ClaudeCodeEvent::Hook(he) => {
                hook_events.push(he);
            }
            ClaudeCodeEvent::Summary(se) => {
                summary_events.push((line_idx, se));
            }
            ClaudeCodeEvent::Unknown => {
                unknown_indices.push(line_idx);
            }
        }
    }

    if let Some(t) = current_turn {
        turns.push(t);
    }

    GroupedEvents {
        turns,
        hook_events,
        summary_events,
        unknown_indices,
    }
}

/// Extract text content from a `tool_result` `content` field.
/// The field may be a string, an array of content blocks, or absent.
fn extract_content_text(content: Option<&serde_json::Value>) -> Option<String> {
    let v = content?;
    if let Some(s) = v.as_str() {
        return Some(s.to_owned());
    }
    if let Some(arr) = v.as_array() {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    None
}

// ── Argument / command helpers ────────────────────────────────────────────────

/// Extract the shell command string from a Bash tool's `input` JSON.
fn extract_bash_command(input: &serde_json::Value) -> String {
    input
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_default()
}

/// Extract the file path from a file tool's `input` JSON.
///
/// `NotebookEdit` and `NotebookRead` use `notebook_path`; all other file tools
/// use `file_path`. Falls back to `file_path` for notebook tools as a safety net.
fn extract_file_path(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let primary_key = if matches!(tool_name, "NotebookEdit" | "NotebookRead") {
        "notebook_path"
    } else {
        "file_path"
    };
    input
        .get(primary_key)
        .or_else(|| input.get("file_path"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
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

fn is_patch_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    cmd == "patch"
        || cmd.starts_with("patch ")
        || cmd == "git apply"
        || cmd.starts_with("git apply ")
}

/// Determine `tool_kind` for the daemon's `TOOL_KIND_VALUES` set.
fn tool_kind_for(tool_name: &str, cmd: &str) -> &'static str {
    match tool_name {
        "Bash" => {
            if is_patch_command(cmd) {
                "other"
            } else {
                "bash"
            }
        }
        "Read" | "NotebookRead" => "file_read",
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "file_edit",
        "WebFetch" | "WebSearch" => "network_request",
        "Grep" | "Glob" | "GlobSearch" => "search",
        _ => "other",
    }
}

fn build_output_summary(text: Option<&str>) -> String {
    const MAX_LEN: usize = 500;
    let s = text.unwrap_or("");
    if s.len() > MAX_LEN {
        format!("{}…", safe_truncate(s, MAX_LEN))
    } else {
        s.to_owned()
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

// ── Public import entry point ─────────────────────────────────────────────────

/// Import a Claude Code transcript JSONL file and return a [`Graph`] of
/// agent-memory records.
///
/// Every node record carries `domain`, `importer_id`, `importer_version`,
fn claude_code_producer() -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::ClaudeCodeImporter,
        producer_components: BTreeMap::from([
            (
                "importer_schema_version".to_owned(),
                IMPORTER_VERSION.to_owned(),
            ),
            (
                "source_format_version".to_owned(),
                SOURCE_FORMAT_VERSION.to_owned(),
            ),
        ]),
        producer_started_at: IMPORTER_STARTED_AT.clone(),
    }
}

/// `source_artifact_path`, and `source_artifact_hash` (BLAKE3 of raw bytes).
/// Free-text fields are passed through `opts.redact` before storage.
///
/// # Errors
///
/// Returns an error when the file cannot be read or contains no parseable events.
#[allow(clippy::too_many_lines)]
pub fn import_claude_code(path: &Path, opts: &ImportOptions) -> Result<Graph> {
    let raw_bytes = std::fs::read(path).map_err(|e| crate::CodegraphError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    })?;

    let source_artifact_hash = blake3_hex(&raw_bytes);
    let source_artifact_path = path.to_string_lossy().into_owned();

    // Parse all lines. Lines that fail JSON parsing are counted as malformed.
    // Lines with valid JSON but an unrecognized `type` become Unknown and
    // produce Diagnostic records (forward-compat with future Claude Code versions).
    let raw_str = String::from_utf8_lossy(&raw_bytes);

    let mut malformed_count = 0usize;
    let mut parsed: Vec<(usize, ClaudeCodeEvent)> = Vec::new();
    for (i, line) in raw_str.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<ClaudeCodeEvent>(line) {
            Ok(event) => parsed.push((i, event)),
            Err(_) => {
                if let Ok(serde_json::Value::Object(_)) =
                    serde_json::from_str::<serde_json::Value>(line)
                {
                    parsed.push((i, ClaudeCodeEvent::Unknown));
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

    // Reject empty or fully-malformed files (no parseable events at all).
    // A file where every line fails JSON parsing leaves all grouped collections
    // empty; that must be treated as EmptyImport rather than emitting a bare
    // AgentSession/AgentRun around a single malformed-lines Diagnostic.
    if grouped.turns.is_empty()
        && grouped.hook_events.is_empty()
        && grouped.summary_events.is_empty()
        && grouped.unknown_indices.is_empty()
    {
        return Err(crate::CodegraphError::EmptyImport {
            path: path.to_path_buf(),
        });
    }

    let default_timestamp = DEFAULT_TIMESTAMP.to_owned();

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
            "AgentSession claude-code {}",
            &ctx.source_artifact_hash[..16]
        ),
        &ctx,
        NodeExtra {
            observed_at: None,
            agent_kind: Some("claude-code".to_owned()),
            ..Default::default()
        },
    ));

    // ── AgentRun ──────────────────────────────────────────────────────────────
    graph.push(make_node(
        run_id.clone(),
        NodeKind::AgentRun,
        "AgentRun claude-code".to_owned(),
        &ctx,
        NodeExtra {
            observed_at: None,
            agent_kind: Some("claude-code".to_owned()),
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

    // ── Hook events → Diagnostic ──────────────────────────────────────────────
    for (i, hook) in grouped.hook_events.iter().enumerate() {
        emit_hook_diagnostic(&mut graph, i, hook, &run_id, &ctx);
    }

    // ── Summary events → Diagnostic ───────────────────────────────────────────
    for (i, (line_idx, se)) in grouped.summary_events.iter().enumerate() {
        emit_summary_diagnostic(&mut graph, i, *line_idx, se, &run_id, &ctx, opts);
    }

    // ── Unknown event kinds → Diagnostic ─────────────────────────────────────
    for &line_idx in &grouped.unknown_indices {
        emit_unknown_event_diagnostic(&mut graph, line_idx, &run_id, &ctx);
    }

    Ok(graph.stamp_producer(&claude_code_producer()))
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

    let turn_timestamp = turn
        .timestamp
        .as_deref()
        .and_then(sanitize_rfc3339)
        .unwrap_or_else(|| ctx.default_timestamp.clone());

    let turn_id = agent_memory_stable_id(&["node", "agent_turn", run_id, &turn_index.to_string()]);

    // ── AgentTurn ─────────────────────────────────────────────────────────────
    let turn_text = {
        let user_part = turn.user_prompt.as_deref().map(|p| redact(p, opts));
        let prose_part = turn.prose.as_deref().map(|p| redact(p, opts));
        match (user_part, prose_part) {
            (Some(u), Some(p)) => Some(format!("User: {u}\n\nAssistant: {p}")),
            (Some(u), None) => Some(format!("User: {u}")),
            (None, Some(p)) => Some(p),
            (None, None) => None,
        }
    };
    graph.push(make_node(
        turn_id.clone(),
        NodeKind::AgentTurn,
        format!("AgentTurn {turn_index}"),
        ctx,
        NodeExtra {
            observed_at: Some(turn_timestamp.clone()),
            turn_index: Some(turn_index),
            agent_kind: Some("claude-code".to_owned()),
            text: turn_text,
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

    // ── CostUsage (best-effort: emit when any token count is non-zero) ──────────
    if let Some(usage) = &turn.usage
        && (usage.input() > 0
            || usage.output() > 0
            || usage.cache_creation() > 0
            || usage.cache_read() > 0)
    {
        emit_cost_usage(graph, turn_index, &turn_id, usage, &turn_timestamp, ctx);
    }

    // ── Tool calls ────────────────────────────────────────────────────────────
    for (action_idx, slot) in turn.tool_calls.iter().enumerate() {
        emit_tool_action(
            graph, action_idx, &turn_id, turn_index, slot, run_id, ctx, opts,
        );
    }
}

fn emit_cost_usage(
    graph: &mut Graph,
    turn_index: u64,
    turn_id: &str,
    usage: &ClaudeCodeUsage,
    timestamp: &str,
    ctx: &ImportCtx,
) {
    let cost_id = agent_memory_stable_id(&["node", "cost_usage", turn_id, &turn_index.to_string()]);
    let summary_text = format!(
        "CostUsage turn={turn_index} input={} output={} cache_creation={}",
        usage.input(),
        usage.output(),
        usage.cache_creation(),
    );
    let text_payload = serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        "cache_read_input_tokens": usage.cache_read_input_tokens,
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
            agent_kind: Some("claude-code".to_owned()),
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
    slot: &ToolCallSlot,
    run_id: &str,
    ctx: &ImportCtx,
    opts: &ImportOptions,
) {
    let tool_name = &slot.tool_name;
    let cmd = if tool_name == "Bash" {
        extract_bash_command(&slot.input)
    } else {
        String::new()
    };
    let redacted_cmd = if tool_name == "Bash" {
        redact(&cmd, opts)
    } else {
        String::new()
    };

    // exit_code is Some(0) on success, None on failure (actual exit code is not
    // available from the is_error boolean alone — synthesising 1 would be misleading).
    let is_error = slot.result.as_ref().is_some_and(|r| r.is_error);
    let exit_code: Option<i64> = slot
        .result
        .as_ref()
        .and_then(|r| if r.is_error { None } else { Some(0) });
    let result_text = slot
        .result
        .as_ref()
        .and_then(|r| r.content.as_deref())
        .map(|s| redact(s, opts));

    let action_timestamp = slot
        .start_timestamp
        .as_deref()
        .and_then(sanitize_rfc3339)
        .unwrap_or_else(|| ctx.default_timestamp.clone());
    let finished_at = slot
        .result_timestamp
        .as_deref()
        .and_then(sanitize_rfc3339)
        .or_else(|| slot.result.as_ref().map(|_| action_timestamp.clone()));

    let tool_call_id =
        agent_memory_stable_id(&["node", "tool_call", turn_id, &action_idx.to_string()]);
    // CommandRun is emitted in the verification domain so PRODUCED_EVIDENCE
    // edges from ToolCall can satisfy the verification:v1: target prefix
    // requirement enforced by validate_agent_memory_edge_endpoints.
    let cmd_run_id =
        verification_stable_id(&["node", "command_run", turn_id, &action_idx.to_string()]);

    let status_str = match exit_code {
        Some(0) => "succeeded",
        Some(_) => "failed",
        None if is_error => "failed",
        None => "unknown",
    };

    // Choose a human-readable command summary for the tool call.
    let cmd_summary = if tool_name == "Bash" {
        redacted_cmd
    } else if let Some(path) = extract_file_path(tool_name, &slot.input) {
        // File-targeting tools: redact and use the path as the summary.
        redact(&path, opts)
    } else {
        // Other tools (Grep, Glob, WebFetch, Agent, …): serialize the tool input
        // as a compact JSON summary so real arguments are recoverable from audits.
        let raw = serde_json::to_string(&slot.input).unwrap_or_default();
        redact(&raw, opts)
    };

    let tool_kind = tool_kind_for(tool_name, &cmd);

    // result_handle carries the tool output for non-Bash tools (Bash uses CommandRun.stdout_handle).
    let tool_result_handle = if tool_name == "Bash" {
        None
    } else {
        result_text.as_deref().map(|s| Box::new(output_handle(s)))
    };
    // Only link produced_evidence_id when a result has actually been observed.
    let has_result = slot.result.is_some();

    // ── ToolCall ──────────────────────────────────────────────────────────────
    graph.push(make_node(
        tool_call_id.clone(),
        NodeKind::ToolCall,
        format!("ToolCall {tool_name} turn={turn_index} action={action_idx}"),
        ctx,
        NodeExtra {
            observed_at: Some(action_timestamp.clone()),
            text: Some(cmd_summary.clone()),
            linked_turn_id: Some(turn_id.to_owned()),
            tool_name: Some(tool_name.clone()),
            tool_kind: Some(tool_kind.to_owned()),
            arguments_summary: Some(cmd_summary.clone()),
            arguments_handle: Some(Box::new(output_handle(&cmd_summary))),
            started_at: Some(action_timestamp.clone()),
            finished_at,
            status: Some(status_str.to_owned()),
            agent_kind: Some("claude-code".to_owned()),
            result_handle: tool_result_handle,
            produced_evidence_id: (tool_name == "Bash" && has_result).then(|| cmd_run_id.clone()),
            ..Default::default()
        },
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        tool_call_id.clone(),
        turn_id.to_owned(),
        "ToolCall belongs to AgentTurn",
        ctx,
    ));

    // ── CommandRun (Bash only, verification domain, result required) ─────────
    // Only emit CommandRun when a tool_result has been observed (has_result).
    // Truncated transcripts where the Bash call has no matching tool_result must
    // not produce command evidence, since no execution outcome is known.
    // CommandRun is placed in the verification domain so that the PRODUCED_EVIDENCE
    // edge from ToolCall satisfies the verification:v1: target-prefix constraint
    // enforced by validate_agent_memory_edge_endpoints.
    if tool_name == "Bash" && has_result {
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
                text: Some(cmd_summary.clone()),
                exit_code,
                stdout_handle: result_text.as_deref().map(|s| Box::new(output_handle(s))),
                agent_kind: Some("claude-code".to_owned()),
                schema_version_override: Some(VERIFICATION_SCHEMA_VERSION),
                domain_override: Some("verification"),
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
        // ToolCall --PRODUCED_EVIDENCE--> CommandRun (verification domain)
        graph.push(make_edge(
            EdgeLabel::ProducedEvidence,
            tool_call_id,
            cmd_run_id,
            "ToolCall produced CommandRun evidence",
            ctx,
        ));
    }

    // ── FileEdit ──────────────────────────────────────────────────────────────
    // Emitted for Edit/Write/MultiEdit/NotebookEdit tools when a successful result
    // has been observed and a file path is known. Gating on has_result prevents
    // truncated transcripts (tool_use with no matching tool_result) from recording
    // an unconfirmed operation as a real file modification.
    // Write creates a new file; other edit tools modify an existing file.
    if has_result
        && !is_error
        && matches!(
            tool_name.as_str(),
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit"
        )
        && let Some(target) = extract_file_path(tool_name, &slot.input)
    {
        let file_edit_id =
            agent_memory_stable_id(&["node", "file_edit", turn_id, &action_idx.to_string()]);
        let after_hash = surrogate_hash(ctx, &target, "after", action_idx);
        let before_hash = Some(surrogate_hash(ctx, &target, "before", action_idx));
        let hunk_count: u32 = if tool_name == "MultiEdit" {
            slot.input
                .get("edits")
                .and_then(|e| e.as_array())
                .map_or(1, |a| u32::try_from(a.len()).unwrap_or(u32::MAX))
        } else {
            1
        };
        graph.push(make_node(
            file_edit_id.clone(),
            NodeKind::FileEdit,
            format!("FileEdit {target} turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp.clone()),
                text: Some(cmd_summary),
                repo_relative_path: Some(target),
                edit_kind: Some("modify".to_owned()),
                before_hash,
                after_hash: Some(after_hash),
                hunk_count: Some(hunk_count),
                linked_turn_id: Some(turn_id.to_owned()),
                agent_kind: Some("claude-code".to_owned()),
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

    // ── Failure ───────────────────────────────────────────────────────────────
    // PatchArtifact is not emitted: required artifact fields (target_files,
    // patch_bytes_hash, patch_handle, etc.) cannot be derived from a JSONL
    // transcript. The command failure is captured by the Failure node instead.
    // FailedOn edges are not emitted: the valid target kinds for FAILED_ON are
    // Symbol, File, and PatchArtifact only (validate_evidence_endpoint_constraints).
    if tool_name == "Bash" && is_error {
        let failure_kind = if is_patch_command(&cmd) {
            "patch_invalid"
        } else {
            "command_failure"
        };
        let failure_id = agent_memory_stable_id(&[
            "node",
            "failure",
            failure_kind,
            turn_id,
            &action_idx.to_string(),
        ]);
        graph.push(make_node(
            failure_id.clone(),
            NodeKind::Failure,
            format!("Failure {failure_kind} turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp.clone()),
                text: Some(build_output_summary(result_text.as_deref())),
                failure_kind: Some(failure_kind.to_owned()),
                exit_code,
                agent_kind: Some("claude-code".to_owned()),
                ..Default::default()
            },
        ));
        graph.push(make_edge(
            EdgeLabel::AuthoredBy,
            failure_id,
            turn_id.to_owned(),
            "Failure belongs to AgentTurn",
            ctx,
        ));
    }

    // ── Verification (only when Bash test command + exit_code == 0) ──────────
    // Trust rule: prose alone never promotes to Verification.
    if tool_name == "Bash" && is_test_command(&cmd) && exit_code == Some(0) {
        let verification_id =
            verification_stable_id(&["node", "verification", turn_id, &action_idx.to_string()]);
        graph.push(make_node(
            verification_id.clone(),
            NodeKind::Verification,
            format!("Verification passed turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: Some(action_timestamp),
                text: Some(build_output_summary(result_text.as_deref())),
                exit_code,
                agent_kind: Some("claude-code".to_owned()),
                schema_version_override: Some(VERIFICATION_SCHEMA_VERSION),
                domain_override: Some("verification"),
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

fn emit_hook_diagnostic(
    graph: &mut Graph,
    idx: usize,
    hook: &HookEvent,
    run_id: &str,
    ctx: &ImportCtx,
) {
    let hook_name = hook.hook_event_name.as_deref().unwrap_or("unknown");
    let timestamp = hook
        .timestamp
        .as_deref()
        .and_then(sanitize_rfc3339)
        .unwrap_or_else(|| ctx.default_timestamp.clone());

    let diag_id = agent_memory_stable_id(&[
        "node",
        "diagnostic",
        "hook",
        hook_name,
        &idx.to_string(),
        run_id,
    ]);
    let summary = hook.tool_name.as_deref().map_or_else(
        || format!("Hook {hook_name}"),
        |tool| format!("Hook {hook_name} tool={tool}"),
    );
    graph.push(make_node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        summary,
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
        "Hook Diagnostic belongs to AgentRun",
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

fn emit_summary_diagnostic(
    graph: &mut Graph,
    idx: usize,
    line_idx: usize,
    se: &SummaryEvent,
    run_id: &str,
    ctx: &ImportCtx,
    opts: &ImportOptions,
) {
    let timestamp = se
        .timestamp
        .as_deref()
        .and_then(sanitize_rfc3339)
        .unwrap_or_else(|| ctx.default_timestamp.clone());
    let diag_id = agent_memory_stable_id(&[
        "node",
        "diagnostic",
        "summary_event",
        &idx.to_string(),
        run_id,
    ]);
    let redacted_text = se.summary.as_deref().map(|s| redact(s, opts));
    let label_excerpt = redacted_text.as_deref().unwrap_or("(empty summary)");
    let label = format!(
        "Summary event at line {line_idx}: {}",
        safe_truncate(label_excerpt, 120)
    );
    graph.push(make_node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        label,
        ctx,
        NodeExtra {
            observed_at: Some(timestamp),
            text: redacted_text,
            ..Default::default()
        },
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        diag_id,
        run_id.to_owned(),
        "Summary Diagnostic belongs to AgentRun",
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

// ── RFC 3339 timestamp sanitizer ──────────────────────────────────────────────

/// Validate a timestamp string as plausible RFC3339.
///
/// Checks structural positions, numeric ranges, timezone designator (including
/// the full offset for `+`/`-` forms), and calendar validity (including leap
/// years). Returns `None` for any malformed input so callers fall back to
/// `DEFAULT_TIMESTAMP`.
fn sanitize_rfc3339(ts: &str) -> Option<String> {
    let b = ts.as_bytes();
    if b.len() < 20 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
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
    match b[19] {
        b'Z' => {
            if b.len() != 20 {
                return None;
            }
        }
        b'+' | b'-' => {
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
        _ => return None,
    }
    Some(ts.to_owned())
}

fn surrogate_hash(ctx: &ImportCtx, target: &str, phase: &str, action_idx: usize) -> String {
    blake3_hex(
        format!(
            "claude-code-importer-v1\0{}\0{target}\0{phase}\0{action_idx}",
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
    result_handle: Option<Box<OutputHandle>>,
    /// Override the `schema_version` field (default: `AGENT_MEMORY_SCHEMA_VERSION`).
    schema_version_override: Option<u32>,
    /// Override the `domain` field (default: `DOMAIN` = `"agent_memory"`).
    domain_override: Option<&'static str>,
    /// The ID of the `CommandRun` or `TestRun` this `ToolCall` produced (for `PRODUCED_EVIDENCE`).
    produced_evidence_id: Option<String>,
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
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
        text: extra.text,
        superseded_by: None,
        agent_id: Some(IMPORTER_ID.to_owned()),
        agent_kind: extra.agent_kind.or_else(|| Some("claude-code".to_owned())),
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
        result_handle: extra.result_handle,
        produced_evidence_id: extra.produced_evidence_id,
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
        frame_resolution: None,
        frame_index: None,
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn extract_bash_command_with_command_field() {
        let input = serde_json::json!({"command": "cargo test --all"});
        assert_eq!(extract_bash_command(&input), "cargo test --all");
    }

    #[test]
    fn extract_bash_command_without_command_field() {
        let input = serde_json::json!({"file_path": "foo.py"});
        assert_eq!(extract_bash_command(&input), "");
    }

    #[test]
    fn tool_kind_for_bash() {
        assert_eq!(tool_kind_for("Bash", "cargo test"), "bash");
        assert_eq!(tool_kind_for("Bash", "patch -p1"), "other");
        assert_eq!(tool_kind_for("Bash", "git apply foo.patch"), "other");
    }

    #[test]
    fn tool_kind_for_file_tools() {
        assert_eq!(tool_kind_for("Read", ""), "file_read");
        assert_eq!(tool_kind_for("NotebookRead", ""), "file_read");
        assert_eq!(tool_kind_for("Edit", ""), "file_edit");
        assert_eq!(tool_kind_for("Write", ""), "file_edit");
        assert_eq!(tool_kind_for("MultiEdit", ""), "file_edit");
        assert_eq!(tool_kind_for("NotebookEdit", ""), "file_edit");
    }

    #[test]
    fn tool_kind_for_unknown() {
        assert_eq!(tool_kind_for("Agent", ""), "other");
        assert_eq!(tool_kind_for("SomeFutureTool", ""), "other");
    }

    #[test]
    fn is_test_command_detection() {
        assert!(is_test_command("cargo test"));
        assert!(is_test_command("cargo test --all"));
        assert!(is_test_command("cargo test --all-targets"));
        assert!(is_test_command("python -m pytest"));
        assert!(is_test_command("python -m pytest tests/"));
        assert!(!is_test_command("cat foo.py"));
        assert!(!is_test_command("git apply foo.patch"));
        assert!(!is_test_command("echo cargo test"));
    }

    #[test]
    fn is_patch_command_detection() {
        assert!(is_patch_command("patch"));
        assert!(is_patch_command("patch -p1"));
        assert!(is_patch_command("git apply"));
        assert!(is_patch_command("git apply foo.patch"));
        assert!(!is_patch_command("cargo test"));
        assert!(!is_patch_command("git commit"));
    }

    #[test]
    fn group_events_basic_session() {
        // Single assistant with one tool_use, then user with tool_result.
        let events = vec![
            (
                0,
                ClaudeCodeEvent::User(UserEvent {
                    message: MessageEnvelope {
                        role: "user".to_owned(),
                        content: vec![ContentBlock {
                            block_type: "text".to_owned(),
                            text: Some("Please read calc.py".to_owned()),
                            id: None,
                            tool_use_id: None,
                            name: None,
                            input: None,
                            content: None,
                            is_error: false,
                        }],
                        usage: None,
                    },
                    timestamp: Some("2025-01-01T00:00:00Z".to_owned()),
                    session_id: None,
                }),
            ),
            (
                1,
                ClaudeCodeEvent::Assistant(AssistantEvent {
                    message: MessageEnvelope {
                        role: "assistant".to_owned(),
                        content: vec![ContentBlock {
                            block_type: "tool_use".to_owned(),
                            text: None,
                            id: Some("toolu_r1".to_owned()),
                            tool_use_id: None,
                            name: Some("Read".to_owned()),
                            input: Some(serde_json::json!({"file_path": "calc.py"})),
                            content: None,
                            is_error: false,
                        }],
                        usage: None,
                    },
                    usage: None,
                    timestamp: Some("2025-01-01T00:00:01Z".to_owned()),
                    session_id: None,
                }),
            ),
            (
                2,
                ClaudeCodeEvent::User(UserEvent {
                    message: MessageEnvelope {
                        role: "user".to_owned(),
                        content: vec![ContentBlock {
                            block_type: "tool_result".to_owned(),
                            text: None,
                            id: None,
                            tool_use_id: Some("toolu_r1".to_owned()),
                            name: None,
                            input: None,
                            content: Some(serde_json::json!([{
                                "type": "text",
                                "text": "def add(a, b):\n    return a + b\n"
                            }])),
                            is_error: false,
                        }],
                        usage: None,
                    },
                    timestamp: Some("2025-01-01T00:00:02Z".to_owned()),
                    session_id: None,
                }),
            ),
        ];
        let grouped = group_events(events);
        assert_eq!(grouped.turns.len(), 1);
        assert_eq!(grouped.turns[0].tool_calls.len(), 1);
        assert!(grouped.turns[0].tool_calls[0].result.is_some());
        assert!(
            !grouped.turns[0].tool_calls[0]
                .result
                .as_ref()
                .unwrap()
                .is_error
        );
    }

    #[test]
    fn verification_trust_boundary() {
        // Prose "Tests pass." in an assistant text block must NOT produce a Verification.
        let events = vec![(
            0,
            ClaudeCodeEvent::Assistant(AssistantEvent {
                message: MessageEnvelope {
                    role: "assistant".to_owned(),
                    content: vec![ContentBlock {
                        block_type: "text".to_owned(),
                        text: Some("Tests pass. The edit is complete.".to_owned()),
                        id: None,
                        tool_use_id: None,
                        name: None,
                        input: None,
                        content: None,
                        is_error: false,
                    }],
                    usage: None,
                },
                usage: None,
                timestamp: Some("2025-01-01T00:00:07Z".to_owned()),
                session_id: None,
            }),
        )];
        let grouped = group_events(events);
        // One turn with zero tool calls — no Verification can be emitted.
        assert_eq!(grouped.turns.len(), 1);
        assert_eq!(grouped.turns[0].tool_calls.len(), 0);
    }

    #[test]
    fn hook_event_produces_diagnostic() {
        let events = vec![(
            0,
            ClaudeCodeEvent::Hook(HookEvent {
                hook_event_name: Some("PreToolUse".to_owned()),
                tool_name: Some("Bash".to_owned()),
                timestamp: Some("2025-01-01T00:00:01Z".to_owned()),
                session_id: Some("sess_001".to_owned()),
            }),
        )];
        let grouped = group_events(events);
        assert_eq!(grouped.hook_events.len(), 1);
        assert_eq!(
            grouped.hook_events[0].hook_event_name.as_deref(),
            Some("PreToolUse")
        );
    }

    #[test]
    fn blake3_hex_stability() {
        let a = blake3_hex(b"hello");
        let b = blake3_hex(b"hello");
        assert_eq!(a, b);
        assert_ne!(blake3_hex(b"hello"), blake3_hex(b"world"));
    }
}
