use super::*;

#[allow(clippy::too_many_lines)]
pub(crate) fn query_memory_cmd(
    records: &[GraphRecord],
    id_or_handle: &str,
    verified_only: bool,
) -> Result<()> {
    let resolved = match query::resolve_memory_ids(records, id_or_handle) {
        Ok(res) => res,
        Err(
            err @ (query::MemoryResolveError::Ambiguous { .. }
            | query::MemoryResolveError::Unsupported { .. }),
        ) => {
            eprintln!("{}", serde_json::to_string(&err)?);
            std::process::exit(1);
        }
    };

    if resolved.matched.is_empty() {
        // The handle named only deleted records — either the canonical ID is a
        // tombstone target, or a source/session handle matched a now-tombstoned
        // claim (`tombstoned_only`). Either way it is stale, not missing (AC6).
        let is_tombstoned = resolved.tombstoned_only
            || records.iter().any(|r| {
                matches!(r, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == id_or_handle)
            });
        let code = if is_tombstoned {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "memory_handle": id_or_handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    // `resolve_memory_ids` has already dropped tombstoned (deleted) IDs, so a
    // resolved ID is always a live claim.
    let memory_id = resolved.matched.iter().next().expect("non-empty");

    let ctx = query::memory_audit_context(records, memory_id, verified_only);
    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "memory_handle": id_or_handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let memory_claim: Vec<AuditClaim<'_>> = ctx
        .memory_claim
        .iter()
        .filter_map(|r| audit_claim(r))
        .collect();

    // Direct provenance from the first claim node.
    let provenance = ctx.memory_claim.first().map_or_else(
        || AuditProvenance {
            provenance_handle: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            source_handle: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            redaction_policy_version: None,
            agent_session_ids: Vec::new(),
            agent_ids: Vec::new(),
        },
        |claim| {
            let GraphRecord::Node {
                agent_id,
                agent_kind,
                session_id,
                observed_at,
                ingested_at,
                source_handle,
                source_artifact_path,
                source_artifact_hash,
                redaction_policy_version,
                ..
            } = claim
            else {
                unreachable!("claim is a node");
            };
            let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
                (Some(a), Some(s)) => Some(format!("{a}:{s}")),
                (Some(a), None) => Some(a.to_owned()),
                _ => None,
            };
            AuditProvenance {
                provenance_handle,
                agent_id: agent_id.as_deref(),
                agent_kind: agent_kind.as_deref(),
                session_id: session_id.as_deref(),
                observed_at: observed_at.as_deref(),
                ingested_at: ingested_at.as_deref(),
                source_handle: source_handle.as_deref(),
                source_artifact_path: source_artifact_path.as_deref(),
                source_artifact_hash: source_artifact_hash.as_deref(),
                redaction_policy_version: redaction_policy_version.as_deref(),
                agent_session_ids: ctx.agent_sessions.iter().map(|r| r.id()).collect(),
                agent_ids: ctx.agents.iter().map(|r| r.id()).collect(),
            }
        },
    );

    let supporting_evidence: Vec<AuditItem<'_>> =
        ctx.supporting_evidence.iter().map(audit_item).collect();
    let contradicting_evidence: Vec<AuditItem<'_>> =
        ctx.contradicting_evidence.iter().map(audit_item).collect();
    let superseding_records: Vec<AuditItem<'_>> =
        ctx.superseding_records.iter().map(audit_item).collect();
    let related_code_handles: Vec<AuditItem<'_>> =
        ctx.related_code_handles.iter().map(audit_item).collect();
    let related_project_handles: Vec<AuditItem<'_>> =
        ctx.related_project_handles.iter().map(audit_item).collect();
    let verification_evidence: Vec<AuditItem<'_>> =
        ctx.verification_evidence.iter().map(audit_item).collect();

    // Diagnostics: context (unresolved links) + protected payloads + redaction.
    let mut diagnostics: Vec<AuditDiagnostic<'_>> = ctx
        .diagnostics
        .iter()
        .map(|d| AuditDiagnostic {
            code: &d.code,
            source_record_id: &d.source_record_id,
            target_handle: &d.target_handle,
            relation: &d.relation,
            target_domain: &d.target_domain,
        })
        .collect();
    for claim in &ctx.memory_claim {
        protected_payload_diagnostics(claim, &mut diagnostics);
        if let GraphRecord::Node {
            id,
            redaction_policy_version: Some(ver),
            ..
        } = claim
        {
            diagnostics.push(AuditDiagnostic {
                code: "redacted_payload",
                source_record_id: id,
                target_handle: ver,
                relation: "redaction_policy_version",
                target_domain: "agent_memory",
            });
        }
    }
    for item in ctx
        .supporting_evidence
        .iter()
        .chain(&ctx.contradicting_evidence)
        .chain(&ctx.superseding_records)
        .chain(&ctx.related_project_handles)
        .chain(&ctx.verification_evidence)
    {
        protected_payload_diagnostics(item.record, &mut diagnostics);
    }
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.source_record_id.cmp(b.source_record_id))
            .then_with(|| a.target_handle.cmp(b.target_handle))
            .then_with(|| a.relation.cmp(b.relation))
            .then_with(|| a.target_domain.cmp(b.target_domain))
    });
    // Keep `target_domain` in the dedup key: two unresolved links sharing source,
    // handle, and relation but pointing at different domains are distinct
    // unresolved facts and must both be surfaced (AC6).
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
            && a.target_domain == b.target_domain
    });

    let excluded: Vec<AuditExcluded<'_>> = ctx
        .excluded
        .iter()
        .filter_map(|item| {
            let GraphRecord::Node {
                id,
                kind,
                source_handle,
                ..
            } = item.record
            else {
                return None;
            };
            Some(AuditExcluded {
                record_id: id,
                kind: kind.as_str(),
                reason: "unverified_observation",
                source_handle: source_handle.as_deref(),
            })
        })
        .collect();

    let returned = memory_claim.len()
        + supporting_evidence.len()
        + contradicting_evidence.len()
        + superseding_records.len()
        + related_code_handles.len()
        + related_project_handles.len()
        + verification_evidence.len();

    let response = MemoryAuditResponse {
        ok: true,
        memory_id,
        verified_only,
        memory_claim,
        direct_provenance: provenance,
        supporting_evidence,
        contradicting_evidence,
        superseding_records,
        related_code_handles,
        related_project_handles,
        verification_evidence,
        diagnostics,
        excluded,
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned,
        },
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize memory audit")?;
    println!("{output}");
    Ok(())
}
