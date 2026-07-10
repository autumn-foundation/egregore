use super::*;

/// Builds the claim view, never presenting it as source truth (AC3).
pub(crate) fn audit_claim(record: &GraphRecord) -> Option<AuditClaim<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        text,
        confidence,
        superseded_by,
        redaction_policy_version,
        ..
    } = record
    else {
        return None;
    };
    let redacted = redaction_policy_version.is_some()
        || text.as_deref().is_some_and(|t| t.contains("<REDACTED:"));
    let text_hash = text
        .as_deref()
        .map(|t| format!("blake3:{}", blake3::hash(t.as_bytes()).to_hex()));
    let (summary, summary_hash) = safe_summary(record);
    Some(AuditClaim {
        record_id: id,
        kind: kind.as_str(),
        trust_class: "agent_authored",
        summary,
        summary_hash,
        text_hash,
        confidence: confidence.as_deref(),
        superseded_by: superseded_by.as_deref(),
        redacted,
    })
}

/// Serializes one evidence item to a bounded, payload-free view (AC9).
#[allow(clippy::too_many_lines)]
pub(crate) fn audit_item<'a>(item: &query::MemoryEvidenceItem<'a>) -> AuditItem<'a> {
    let record = item.record;
    let handle = citable_handle(record);
    let trust = trust_class_for(record);
    let (summary, summary_hash) = safe_summary(record);
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        repo_relative_path,
        span,
        status,
        verification_kind,
        exit_code,
        source_artifact_path,
        source_artifact_hash,
        stdout_handle,
        stderr_handle,
        patch_status,
        patch_bytes_hash,
        body_handle,
        author,
        agent_id,
        session_id,
        observed_at,
        confidence,
        patch_handle,
        diff_hunk_handle,
        arguments_handle,
        result_handle,
        ..
    } = record
    else {
        // Edges/tombstones never reach here; produce a minimal safe item.
        return AuditItem {
            record_id: record.id(),
            kind: "Unknown",
            trust_class: "other",
            relation: item.relation.clone(),
            citable_handle: handle,
            summary,
            summary_hash,
            name: None,
            title: None,
            repo_relative_path: None,
            span: None,
            status: None,
            verification_kind: None,
            exit_code: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            stdout_hash: None,
            stderr_hash: None,
            patch_status: None,
            patch_bytes_hash: None,
            body_handle_hash: None,
            diff_hunk_hash: None,
            author: None,
            agent_id: None,
            session_id: None,
            observed_at: None,
            confidence: None,
            protected: false,
        };
    };
    let protected = patch_handle.is_some()
        || stdout_handle.as_ref().is_some_and(|o| o.bytes > 0)
        || stderr_handle.as_ref().is_some_and(|o| o.bytes > 0)
        || body_handle.is_some()
        || diff_hunk_handle.is_some()
        || arguments_handle.is_some()
        || result_handle.is_some();
    AuditItem {
        record_id: id,
        kind: kind.as_str(),
        trust_class: trust,
        relation: item.relation.clone(),
        citable_handle: handle,
        summary,
        summary_hash,
        name: name.as_deref(),
        title: title.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        exit_code: *exit_code,
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        stdout_hash: stdout_handle.as_ref().map(|o| o.hash.as_str()),
        stderr_hash: stderr_handle.as_ref().map(|o| o.hash.as_str()),
        patch_status: patch_status.as_deref(),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        body_handle_hash: body_handle.as_ref().map(|o| o.hash.as_str()),
        diff_hunk_hash: diff_hunk_handle.as_ref().map(|o| o.hash.as_str()),
        author: author.as_deref(),
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        protected,
    }
}

/// Emits a `protected_payload` diagnostic for each withheld raw payload (AC6).
pub(crate) fn protected_payload_diagnostics<'a>(
    record: &'a GraphRecord,
    out: &mut Vec<AuditDiagnostic<'a>>,
) {
    let GraphRecord::Node {
        id,
        stdout_handle,
        stderr_handle,
        patch_bytes_hash,
        patch_handle,
        body_handle,
        diff_hunk_handle,
        arguments_handle,
        result_handle,
        ..
    } = record
    else {
        return;
    };
    if let Some(o) = stdout_handle.as_ref().filter(|o| o.bytes > 0) {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "stdout",
            target_domain: "verification",
        });
    }
    if let Some(o) = stderr_handle.as_ref().filter(|o| o.bytes > 0) {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "stderr",
            target_domain: "verification",
        });
    }
    if patch_handle.is_some()
        && let Some(h) = patch_bytes_hash.as_deref()
    {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: h,
            relation: "patch_bytes",
            target_domain: "artifact",
        });
    }
    if let Some(o) = body_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "body",
            target_domain: "project",
        });
    }
    if let Some(o) = diff_hunk_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "diff_hunk",
            target_domain: "project",
        });
    }
    if let Some(o) = arguments_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "tool_arguments",
            target_domain: "agent_memory",
        });
    }
    if let Some(o) = result_handle.as_ref() {
        out.push(AuditDiagnostic {
            code: "protected_payload",
            source_record_id: id,
            target_handle: &o.hash,
            relation: "tool_result",
            target_domain: "agent_memory",
        });
    }
}

pub(crate) fn query_audit_cmd(
    records: &[GraphRecord],
    durable_id: &str,
    format: OutputFormat,
) -> Result<()> {
    let durable = records
        .iter()
        .rfind(|r| r.id() == durable_id)
        .ok_or_else(|| anyhow::anyhow!("Durable record '{durable_id}' not found"))?;
    match crate::query::audit_trail(records, durable) {
        Ok(chain) => {
            match format {
                OutputFormat::Json => {
                    let out = serde_json::json!({
                        "ok": true,
                        "audit_chain": chain,
                    });
                    println!("{}", serde_json::to_string(&out)?);
                }
                OutputFormat::Text => {
                    println!("Audit trail for policy record: {durable_id}");
                    for (i, rec) in chain.iter().enumerate() {
                        if let GraphRecord::Node { id, kind, .. } = rec {
                            println!("  Step {i}: [{kind:?}] {id}");
                        }
                    }
                }
            }
            Ok(())
        }
        Err(e) => {
            match format {
                OutputFormat::Json => {
                    let out = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": "audit_trail_failed",
                            "message": e,
                        }
                    });
                    println!("{}", serde_json::to_string(&out)?);
                }
                OutputFormat::Text => {
                    eprintln!("error: audit trail failed: {e}");
                }
            }
            anyhow::bail!("audit trail failed: {e}")
        }
    }
}
