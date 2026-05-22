//! `rust-swe-agent` `.traj` importer — M2 agent-memory source (issue #9).
//!
//! Parses a trajectory JSON file (format `mini-swe-agent-1.2`) and emits typed
//! agent-memory graph records following the same JSONL conventions as `scan` and
//! `scan-history`.
//!
//! Every emitted record carries: `domain`, `schema_version`, `importer_id`,
//! `importer_version`, `source_artifact_path`, and `source_artifact_hash` (BLAKE3
//! of the raw `.traj` bytes). The raw artifact body is never inlined into a
//! queryable graph field; it is preserved by handle (path + hash) on every record.
//!
//! # Redaction
//!
//! All free-text fields (command text, stdout/stderr excerpts, task descriptions)
//! pass through a caller-supplied redaction closure before being stored.  The
//! default [`ImportOptions`] uses a pass-through closure.
//! TODO: replace with #4 policy at the single call site in [`redact`].
//!
//! # Idempotency
//!
//! The `AgentSession` ID is derived from the BLAKE3 hash of the raw `.traj` bytes
//! plus the importer version string.  Re-importing the same file always produces
//! the same `AgentSession` ID regardless of when or how many times import is run.

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
pub const IMPORTER_ID: &str = "traj-importer";
/// Importer version embedded in every emitted record and used for idempotency.
pub const IMPORTER_VERSION: &str = "0.1.0";
/// Domain value carried on every agent-memory record.
pub const DOMAIN: &str = "agent_memory";
const DEFAULT_TRAJ_TIMESTAMP: &str = "1970-01-01T00:00:00Z";
const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

// ── Import options ────────────────────────────────────────────────────────────

/// Options controlling `.traj` import behaviour.
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

/// Apply the redaction closure to a free-text value.
///
/// This is the **single call site** for redaction in the traj importer.
/// TODO: replace the closure dispatch with the #4 policy once that lands.
#[inline]
fn redact(value: &str, opts: &ImportOptions) -> String {
    (opts.redact)(value)
}

// ── .traj JSON parsing types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TrajFile {
    trajectory_format: String,
    #[allow(dead_code)]
    #[serde(default)]
    schema_version: TrajSchemaVersion,
    info: TrajInfo,
    #[serde(default)]
    messages: Vec<TrajMessage>,
}

#[derive(Debug, Default, Deserialize)]
struct TrajSchemaVersion {
    #[allow(dead_code)]
    major: u32,
    #[allow(dead_code)]
    minor: u32,
}

#[derive(Debug, Deserialize)]
struct TrajInfo {
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    exit_reason: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    // Fields present in trajectories but not used by the importer yet.
    #[allow(dead_code)]
    #[serde(default)]
    task: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    ended_at: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    verification_status: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    steps: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TrajMessage {
    role: String,
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default)]
    extra: Option<TrajExtra>,
}

impl TrajMessage {
    const fn content_str(&self) -> &str {
        match &self.content {
            serde_json::Value::String(s) => s.as_str(),
            _ => "",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct TrajExtra {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    actions: Option<Vec<String>>,
    #[serde(default)]
    run_result: Option<TrajRunResult>,
    // Present in trajectories but unused by the importer (captured for Diagnostic emit).
    #[allow(dead_code)]
    #[serde(default)]
    tool_use_blocked: Option<bool>,
    #[allow(dead_code)]
    #[serde(default)]
    observation_truncated: Option<bool>,
    /// Unrecognized extra fields — walked to emit Diagnostic records.
    #[serde(flatten)]
    unknown: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct TrajRunResult {
    exit_code: i64,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
}

// ── Public import entry point ─────────────────────────────────────────────────

/// Import a `rust-swe-agent` `.traj` file and return a [`Graph`] of agent-memory records.
///
/// Every node record carries `domain`, `importer_id`, `importer_version`,
/// `source_artifact_path`, and `source_artifact_hash` (BLAKE3 of raw bytes).
/// Free-text fields are passed through `opts.redact` before storage.
///
/// # Errors
///
/// Returns an error when the file cannot be read or is not valid trajectory JSON.
#[allow(clippy::too_many_lines)]
pub fn import_traj(path: &Path, opts: &ImportOptions) -> Result<Graph> {
    let raw_bytes = std::fs::read(path).map_err(|e| crate::CodegraphError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    })?;

    let source_artifact_hash = blake3_hex(&raw_bytes);
    let source_artifact_path = path.to_string_lossy().into_owned();

    let traj: TrajFile = serde_json::from_slice(&raw_bytes)?;

    // Derive stable session ID from artifact hash + importer version.
    // Identical input bytes → identical session ID (idempotency guarantee).
    let session_id = agent_memory_stable_id(&[
        "node",
        "agent_session",
        IMPORTER_ID,
        IMPORTER_VERSION,
        &source_artifact_hash,
    ]);

    let ctx = ImportCtx {
        source_artifact_path,
        source_artifact_hash,
        traj_format: traj.trajectory_format,
        session_id: session_id.clone(),
        default_timestamp: traj
            .info
            .started_at
            .clone()
            .unwrap_or_else(|| DEFAULT_TRAJ_TIMESTAMP.to_owned()),
    };

    let mut graph = Graph::new();

    let run_id = agent_memory_stable_id(&["node", "agent_run", &session_id, "run-0"]);

    // ── AgentSession ──────────────────────────────────────────────────────────
    graph.push(make_node(
        session_id.clone(),
        NodeKind::AgentSession,
        format!(
            "AgentSession for {} trajectory ({})",
            traj.info.model_name.as_deref().unwrap_or("unknown"),
            &ctx.source_artifact_hash[..16]
        ),
        &ctx,
        NodeExtra::default(),
    ));

    // ── AgentRun ──────────────────────────────────────────────────────────────
    let outcome = traj.info.outcome.as_deref().unwrap_or("unknown");
    let exit_reason = traj.info.exit_reason.as_deref().unwrap_or("unknown");
    graph.push(make_node(
        run_id.clone(),
        NodeKind::AgentRun,
        format!("AgentRun outcome={outcome} exit_reason={exit_reason}"),
        &ctx,
        NodeExtra {
            observed_at: traj.info.started_at,
            agent_kind: Some("rust-swe-agent".to_owned()),
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

    // ── Diagnostic for unrecognized trajectory format ─────────────────────────
    emit_format_diagnostic(&mut graph, &run_id, &ctx);

    // ── Parse messages into turns ─────────────────────────────────────────────
    let mut turn_index: u64 = 0;
    let mut i = 0;
    while i < traj.messages.len() {
        let msg = &traj.messages[i];
        if msg.role == "assistant" {
            let user_msg = traj.messages.get(i + 1).filter(|m| m.role == "user");
            emit_turn(&mut graph, turn_index, msg, user_msg, &run_id, &ctx, opts);
            turn_index += 1;
            i += 2;
        } else {
            i += 1;
        }
    }

    // ── Diagnostics for unrecognized extra keys ───────────────────────────────
    emit_unknown_key_diagnostics(&mut graph, &traj.messages, &run_id, &ctx);

    Ok(graph)
}

// ── Diagnostic helpers ────────────────────────────────────────────────────────

fn emit_format_diagnostic(graph: &mut Graph, run_id: &str, ctx: &ImportCtx) {
    const RECOGNIZED: &[&str] = &["mini-swe-agent-1.2", "mini-swe-agent-1.0", "swe-agent-1.0"];
    if RECOGNIZED.contains(&ctx.traj_format.as_str()) {
        return;
    }
    let diag_id = agent_memory_stable_id(&[
        "node",
        "diagnostic",
        "unknown_format",
        &ctx.traj_format,
        run_id,
    ]);
    graph.push(make_node(
        diag_id.clone(),
        NodeKind::Diagnostic,
        format!(
            "Unrecognized trajectory format '{}' — imported with best-effort mapping",
            ctx.traj_format
        ),
        ctx,
        NodeExtra::default(),
    ));
    graph.push(make_edge(
        EdgeLabel::AuthoredBy,
        diag_id,
        run_id.to_owned(),
        "Diagnostic about unrecognized format",
        ctx,
    ));
}

const KNOWN_EXTRA_KEYS: &[&str] = &[
    "actions",
    "timestamp",
    "run_result",
    "tool_latency_ms",
    "model_latency_ms",
    "harness_overhead_ms",
    "observation_truncated",
    "output_bytes_omitted",
    "stdout_bytes_omitted",
    "stderr_bytes_omitted",
    "post_tool_use_hooks",
    "pre_tool_use_hooks",
    "tool_use_blocked",
    "sampling",
    "response",
    "model_call",
    "wallclock_deadline_warning",
    "harness_advisory",
];

fn emit_unknown_key_diagnostics(
    graph: &mut Graph,
    messages: &[TrajMessage],
    run_id: &str,
    ctx: &ImportCtx,
) {
    for (mi, msg) in messages.iter().enumerate() {
        let Some(extra) = &msg.extra else { continue };
        for unknown_key in extra.unknown.keys() {
            if KNOWN_EXTRA_KEYS.contains(&unknown_key.as_str()) {
                continue;
            }
            let diag_id = agent_memory_stable_id(&[
                "node",
                "diagnostic",
                "unknown_extra_key",
                unknown_key,
                &mi.to_string(),
                run_id,
            ]);
            graph.push(make_node(
                diag_id.clone(),
                NodeKind::Diagnostic,
                format!("Unrecognized .traj extra key '{unknown_key}' at message {mi}"),
                ctx,
                NodeExtra::default(),
            ));
            graph.push(make_edge(
                EdgeLabel::AuthoredBy,
                diag_id,
                run_id.to_owned(),
                "Diagnostic for unknown extra key",
                ctx,
            ));
        }
    }
}

// ── Turn emission ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
#[allow(clippy::option_if_let_else)]
fn emit_turn(
    graph: &mut Graph,
    turn_index: u64,
    assistant_msg: &TrajMessage,
    user_msg: Option<&TrajMessage>,
    run_id: &str,
    ctx: &ImportCtx,
    opts: &ImportOptions,
) {
    let timestamp = assistant_msg
        .extra
        .as_ref()
        .and_then(|e| e.timestamp.clone());

    let turn_id = agent_memory_stable_id(&["node", "agent_turn", run_id, &turn_index.to_string()]);

    // ── AgentTurn ─────────────────────────────────────────────────────────────
    graph.push(make_node(
        turn_id.clone(),
        NodeKind::AgentTurn,
        format!("AgentTurn {turn_index}"),
        ctx,
        NodeExtra {
            observed_at: timestamp.clone(),
            turn_index: Some(turn_index),
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

    // ── Extract run_result ────────────────────────────────────────────────────
    let run_result = user_msg.and_then(|m| m.extra.as_ref()?.run_result.as_ref());
    let exit_code = run_result.map(|r| r.exit_code);
    let stdout = run_result.and_then(|r| r.stdout.as_deref());
    let stderr = run_result.and_then(|r| r.stderr.as_deref());

    // ── Resolve effective actions ─────────────────────────────────────────────
    let fallback: Vec<String>;
    let effective_actions: &[String] = if let Some(actions) = assistant_msg
        .extra
        .as_ref()
        .and_then(|e| e.actions.as_deref())
        .filter(|a| !a.is_empty())
    {
        actions
    } else if let Some(cmd) = extract_bash_command(assistant_msg.content_str()) {
        fallback = vec![cmd];
        &fallback
    } else {
        // Prose-only turn (e.g. final submit sentinel) — AgentTurn already emitted,
        // but no bash block means no real tool action to record.
        return;
    };

    let num_actions = effective_actions.len();
    for (action_idx, command) in effective_actions.iter().enumerate() {
        let redacted_cmd = redact(command, opts);

        // exit code is attributed to the last action only
        let this_exit = if action_idx == num_actions - 1 {
            exit_code
        } else {
            Some(0)
        };
        let output_summary = build_output_summary(stdout, stderr, opts);

        emit_command_action(
            graph,
            action_idx,
            &turn_id,
            command,
            &redacted_cmd,
            timestamp.as_deref(),
            this_exit,
            output_summary,
            run_id,
            ctx,
            turn_index,
        );
    }
}

/// Emit `ToolCall`, `CommandRun`, and derived records for one action.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn emit_command_action(
    graph: &mut Graph,
    action_idx: usize,
    turn_id: &str,
    raw_cmd: &str,
    redacted_cmd: &str,
    timestamp: Option<&str>,
    this_exit: Option<i64>,
    output_summary: String,
    run_id: &str,
    ctx: &ImportCtx,
    turn_index: u64,
) {
    let tool_call_id =
        agent_memory_stable_id(&["node", "tool_call", turn_id, &action_idx.to_string()]);
    let cmd_run_id =
        agent_memory_stable_id(&["node", "command_run", turn_id, &action_idx.to_string()]);
    let status = tool_status(this_exit);
    let action_timestamp = timestamp.unwrap_or(&ctx.default_timestamp);

    // ── ToolCall ──────────────────────────────────────────────────────────────
    graph.push(make_node(
        tool_call_id.clone(),
        NodeKind::ToolCall,
        format!("ToolCall bash turn={turn_index} action={action_idx}"),
        ctx,
        NodeExtra {
            observed_at: timestamp.map(str::to_owned),
            text: Some(redacted_cmd.to_owned()),
            linked_turn_id: Some(turn_id.to_owned()),
            tool_name: Some("Bash".to_owned()),
            tool_kind: Some("bash".to_owned()),
            arguments_summary: Some(redacted_cmd.to_owned()),
            arguments_handle: Some(Box::new(output_handle(redacted_cmd))),
            started_at: timestamp.map(str::to_owned),
            finished_at: matches!(status, "succeeded" | "failed")
                .then(|| action_timestamp.to_owned()),
            status: Some(status.to_owned()),
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
    graph.push(make_node(
        cmd_run_id.clone(),
        NodeKind::CommandRun,
        format!(
            "CommandRun exit={} turn={turn_index}",
            this_exit.map_or_else(|| "?".to_owned(), |c| c.to_string())
        ),
        ctx,
        NodeExtra {
            observed_at: timestamp.map(str::to_owned),
            text: Some(redacted_cmd.to_owned()),
            exit_code: this_exit,
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
    if is_file_edit_command(raw_cmd) {
        let file_edit_id =
            agent_memory_stable_id(&["node", "file_edit", turn_id, &action_idx.to_string()]);
        let target = extract_target_file(raw_cmd).unwrap_or("unknown");
        graph.push(make_node(
            file_edit_id.clone(),
            NodeKind::FileEdit,
            format!("FileEdit {target} turn={turn_index}"),
            ctx,
            NodeExtra {
                observed_at: timestamp.map(str::to_owned),
                text: Some(redacted_cmd.to_owned()),
                repo_relative_path: Some(target.to_owned()),
                edit_kind: Some("modify".to_owned()),
                before_hash: Some(file_edit_surrogate_hash(
                    ctx,
                    target,
                    "before",
                    redacted_cmd,
                )),
                after_hash: Some(file_edit_surrogate_hash(ctx, target, "after", redacted_cmd)),
                hunk_count: Some(1),
                linked_turn_id: Some(turn_id.to_owned()),
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
    if is_patch_command(raw_cmd) {
        emit_patch_action(
            graph,
            action_idx,
            turn_id,
            redacted_cmd,
            timestamp,
            this_exit,
            run_id,
            ctx,
            turn_index,
            &output_summary,
        );
    } else if this_exit.is_some_and(|c| c != 0) {
        emit_command_failure(
            graph,
            action_idx,
            turn_id,
            timestamp,
            this_exit,
            &cmd_run_id,
            &output_summary,
            ctx,
            turn_index,
        );
    }

    // ── Verification ──────────────────────────────────────────────────────────
    if is_test_command(raw_cmd) {
        let verification_id =
            agent_memory_stable_id(&["node", "verification", turn_id, &action_idx.to_string()]);
        let verified = this_exit.is_some_and(|c| c == 0);
        graph.push(make_node(
            verification_id.clone(),
            NodeKind::Verification,
            format!(
                "Verification {} turn={turn_index}",
                if verified { "passed" } else { "failed" }
            ),
            ctx,
            NodeExtra {
                observed_at: timestamp.map(str::to_owned),
                text: Some(output_summary),
                exit_code: this_exit,
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

#[allow(clippy::too_many_arguments)]
fn emit_patch_action(
    graph: &mut Graph,
    action_idx: usize,
    turn_id: &str,
    redacted_cmd: &str,
    timestamp: Option<&str>,
    this_exit: Option<i64>,
    run_id: &str,
    ctx: &ImportCtx,
    turn_index: u64,
    output_summary: &str,
) {
    let failed = this_exit.is_some_and(|c| c != 0);
    let patch_status = if failed { "invalid" } else { "unverified" };

    let patch_id =
        agent_memory_stable_id(&["node", "patch_artifact", turn_id, &action_idx.to_string()]);
    graph.push(make_node(
        patch_id.clone(),
        NodeKind::PatchArtifact,
        format!("PatchArtifact status={patch_status} turn={turn_index}"),
        ctx,
        NodeExtra {
            observed_at: timestamp.map(str::to_owned),
            text: Some(redacted_cmd.to_owned()),
            patch_status: Some(patch_status.to_owned()),
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
        // Use the combined stdout+stderr summary; it already has stderr and redaction applied.
        let error_text = output_summary.to_owned();
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
                observed_at: timestamp.map(str::to_owned),
                text: Some(error_text),
                failure_kind: Some("patch_invalid".to_owned()),
                exit_code: this_exit,
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
}

#[allow(clippy::too_many_arguments)]
fn emit_command_failure(
    graph: &mut Graph,
    action_idx: usize,
    turn_id: &str,
    timestamp: Option<&str>,
    this_exit: Option<i64>,
    cmd_run_id: &str,
    output_summary: &str,
    ctx: &ImportCtx,
    turn_index: u64,
) {
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
            this_exit.unwrap_or(-1)
        ),
        ctx,
        NodeExtra {
            observed_at: timestamp.map(str::to_owned),
            text: Some(output_summary.to_owned()),
            failure_kind: Some("command_failure".to_owned()),
            exit_code: this_exit,
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
        cmd_run_id.to_owned(),
        "Failure describes failed CommandRun",
        ctx,
    ));
}

// ── Command classification helpers ────────────────────────────────────────────

fn is_file_edit_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    (cmd.starts_with("sed") && cmd.contains(" -i"))
        || cmd.starts_with("tee ")
        || (cmd.contains(" > ") && !cmd.starts_with("cat "))
        || cmd.starts_with("cat > ")
        || (cmd.starts_with("printf ") && cmd.contains(" > "))
}

fn is_patch_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    cmd.starts_with("patch ")
        || cmd.starts_with("patch<")
        || cmd.contains("git apply")
        || cmd.contains("patch -p")
}

fn is_test_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    cmd.starts_with("pytest")
        || cmd.starts_with("python -m pytest")
        || cmd.starts_with("python3 -m pytest")
        || cmd.starts_with("cargo test")
        || cmd.starts_with("npm test")
        || cmd.starts_with("go test")
        || cmd.starts_with("make test")
        || cmd.starts_with("./gradlew test")
        || cmd.starts_with("mvn test")
}

fn extract_bash_command(content: &str) -> Option<String> {
    let start = content.find("```bash\n")?;
    let after = &content[start + 8..];
    let end = after.find("```")?;
    Some(after[..end].trim().to_owned())
}

fn extract_target_file(cmd: &str) -> Option<&str> {
    cmd.split_whitespace()
        .find(|p| p.contains('/') || p.contains('.'))
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

/// Truncate `s` to at most `max_bytes` bytes while keeping valid UTF-8.
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

// ── Node / edge construction helpers ─────────────────────────────────────────

/// Context carried through the whole import for provenance fields.
struct ImportCtx {
    source_artifact_path: String,
    source_artifact_hash: String,
    traj_format: String,
    session_id: String,
    default_timestamp: String,
}

/// Optional extra fields for a single node emit call.
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
}

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
        agent_kind: extra
            .agent_kind
            .or_else(|| Some("rust-swe-agent".to_owned())),
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
        stdout_handle: None,
        stderr_handle: None,
        evidence_quality: None,
        executed_at: None,
        verification_kind: None,
        status: extra.status,
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

// Current .traj records do not carry file snapshots, so legacy FileEdit nodes
// use deterministic provenance hashes until the importer can emit real file hashes.
fn file_edit_surrogate_hash(
    ctx: &ImportCtx,
    target: &str,
    phase: &str,
    redacted_command: &str,
) -> String {
    blake3_hex(
        format!(
            "traj-importer-v1\0{}\0{target}\0{phase}\0{redacted_command}",
            ctx.source_artifact_hash
        )
        .as_bytes(),
    )
}

const fn tool_status(exit_code: Option<i64>) -> &'static str {
    match exit_code {
        Some(0) => "succeeded",
        Some(_) => "failed",
        None => "unknown",
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
    fn patch_command_detection() {
        assert!(is_patch_command("patch -p1 < /tmp/fix.patch"));
        assert!(is_patch_command("git apply /tmp/x.patch"));
        assert!(!is_patch_command("cat foo.txt"));
    }

    #[test]
    fn file_edit_command_detection() {
        assert!(is_file_edit_command("sed -i 's/a/b/' foo.py"));
        assert!(is_file_edit_command("echo 'x' > foo.py"));
        assert!(!is_file_edit_command("cat foo.py"));
    }

    #[test]
    fn test_command_detection() {
        assert!(is_test_command("python -m pytest tests/test_calc.py"));
        assert!(is_test_command("cargo test"));
        assert!(!is_test_command("cat foo.txt"));
    }

    #[test]
    fn bash_extraction_from_fenced_block() {
        let content = "Let me look.\n\n```bash\ncat foo.py\n```";
        assert_eq!(extract_bash_command(content), Some("cat foo.py".to_owned()));
        assert_eq!(extract_bash_command("Just prose, no code block."), None);
    }

    #[test]
    fn utf8_truncation_stays_on_boundary() {
        let multibyte = "a".repeat(499) + "é"; // é is 2 bytes — would panic at [..500]
        let result = build_output_summary(
            Some(&multibyte),
            None,
            &crate::traj::ImportOptions::default(),
        );
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn classify_before_redact() {
        // A redactor that blanks everything should not suppress FileEdit classification.
        let opts = crate::traj::ImportOptions {
            redact: Box::new(|_| "[REDACTED]".to_owned()),
        };
        assert!(is_file_edit_command("sed -i 's/a/b/' foo.py"));
        // Confirm the redactor would destroy classification signal.
        let redacted = (opts.redact)("sed -i 's/a/b/' foo.py");
        assert!(!is_file_edit_command(&redacted));
    }

    #[test]
    fn blake3_is_stable() {
        let a = blake3_hex(b"hello");
        let b = blake3_hex(b"hello");
        assert_eq!(a, b);
        assert_ne!(blake3_hex(b"hello"), blake3_hex(b"world"));
    }
}
