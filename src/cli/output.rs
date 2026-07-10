use super::*;

/// Returns a redaction-safe summary plus an optional hash of the stored one.
///
/// For agent-authored records the stored summary embeds a prefix of the
/// observation text (see `build_observation_records`), so it is never forwarded
/// verbatim. We synthesize a structured label from typed fields and expose the
/// original only as a BLAKE3 hash (AC9). Structured records (code, verification,
/// project, artifact) keep their templated summary, which carries no free text.
pub(crate) fn safe_summary(record: &GraphRecord) -> (String, Option<String>) {
    let GraphRecord::Node {
        kind,
        summary,
        agent_id,
        session_id,
        ..
    } = record
    else {
        return (String::new(), None);
    };
    if trust_class_for(record) == "agent_authored" {
        let who = match (agent_id.as_deref(), session_id.as_deref()) {
            (Some(a), Some(s)) => format!("{a}:{s}"),
            (Some(a), None) => a.to_owned(),
            _ => "unknown".to_owned(),
        };
        let label = format!("{} by {who}", kind.as_str());
        let hash = format!("blake3:{}", blake3::hash(summary.as_bytes()).to_hex());
        (label, Some(hash))
    } else {
        (summary.clone(), None)
    }
}

/// Computes a non-empty citable handle for an item, guaranteeing AC4.
pub(crate) fn citable_handle(record: &GraphRecord) -> String {
    let GraphRecord::Node {
        repo_relative_path,
        span,
        source_artifact_path,
        source_artifact_hash,
        stdout_handle,
        stderr_handle,
        patch_bytes_hash,
        body_handle,
        url,
        source_handle,
        agent_id,
        session_id,
        verification_kind,
        title,
        name,
        id,
        ..
    } = record
    else {
        return record.id().to_owned();
    };
    if let Some(path) = repo_relative_path.as_deref() {
        return span.map_or_else(
            || path.to_owned(),
            |s| format!("{path}:{}-{}", s.start_line, s.end_line),
        );
    }
    if let Some(p) = source_artifact_path.as_deref() {
        return p.to_owned();
    }
    if let Some(h) = source_artifact_hash.as_deref() {
        return h.to_owned();
    }
    if let Some(h) = stdout_handle.as_ref().map(|o| o.hash.as_str()) {
        return h.to_owned();
    }
    if let Some(h) = stderr_handle.as_ref().map(|o| o.hash.as_str()) {
        return h.to_owned();
    }
    if let Some(h) = patch_bytes_hash.as_deref() {
        return h.to_owned();
    }
    if let Some(h) = body_handle.as_ref().map(|o| o.hash.as_str()) {
        return h.to_owned();
    }
    if let Some(u) = url.as_deref() {
        return u.to_owned();
    }
    if let Some(s) = source_handle.as_deref() {
        return s.to_owned();
    }
    match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => return format!("{a}:{s}"),
        (Some(a), None) => return a.to_owned(),
        _ => {}
    }
    if let Some(v) = verification_kind.as_deref() {
        return v.to_owned();
    }
    title
        .as_deref()
        .or(name.as_deref())
        .map_or_else(|| id.clone(), ToOwned::to_owned)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

pub(crate) fn print_result<T: Serialize + PrintText>(
    result: &T,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let line = serde_json::to_string(result).context("failed to serialize query result")?;
            println!("{line}");
        }
        OutputFormat::Text => {
            println!("{}", result.as_text());
        }
    }
    Ok(())
}

pub(crate) trait PrintText {
    fn as_text(&self) -> String;
}

// ---------------------------------------------------------------------------

pub(crate) fn domain_category(domain: &str) -> &'static str {
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
