use super::*;

/// Non-empty author label for a conflict party: `agent_id[:session_id]` for
/// agent-authored and verification records, the deciding user for
/// user-context records, and a stable marker when nothing is recorded.
pub(crate) fn conflict_party_author(record: &GraphRecord) -> String {
    if let GraphRecord::Node {
        agent_id,
        session_id,
        user_context,
        ..
    } = record
    {
        match (agent_id.as_deref(), session_id.as_deref()) {
            (Some(agent), Some(session)) => return format!("{agent}:{session}"),
            (Some(agent), None) => return agent.to_owned(),
            _ => {}
        }
        if let Some(by) = user_context.decided_by.as_deref() {
            return by.to_owned();
        }
    }
    "unknown_agent".to_owned()
}

/// Observation time for a conflict party: the verification execution time for
/// verification records, the agent observation time otherwise, and a stable
/// marker when the record carries neither (never an invented timestamp).
pub(crate) fn conflict_party_observed_at(record: &GraphRecord) -> &str {
    if let GraphRecord::Node {
        executed_at: Some(at),
        ..
    } = record
    {
        return at;
    }
    if let GraphRecord::Node {
        observed_at: Some(at),
        ..
    } = record
    {
        return at;
    }
    "unknown_time"
}

/// Confidence string for a conflict party, or a stable marker when absent.
pub(crate) fn conflict_party_confidence(record: &GraphRecord) -> &str {
    if let GraphRecord::Node {
        confidence: Some(confidence),
        ..
    } = record
    {
        return confidence;
    }
    "unspecified"
}

/// Projects one conflict party to its bounded, payload-safe wire view.
pub(crate) fn conflict_party_json<'a>(party: &query::ConflictParty<'a>) -> ConflictPartyJson<'a> {
    let record = party.record;
    let (summary, summary_hash) = safe_summary(record);
    let GraphRecord::Node { kind, .. } = record else {
        // Parties are node records by construction; the fallback keeps the
        // projection total without inventing a kind label.
        return ConflictPartyJson {
            record_id: record.id(),
            kind: "Unknown",
            party_class: party.party_class.as_str(),
            trust_class: party.trust_class.as_str(),
            edge_role: party.edge_role.as_str(),
            citation_handles: party.citation_handles.clone(),
            author: conflict_party_author(record),
            observed_at: conflict_party_observed_at(record),
            confidence: conflict_party_confidence(record),
            target_handle: party.target_handle,
            summary,
            summary_hash,
        };
    };
    ConflictPartyJson {
        record_id: record.id(),
        kind: kind.as_str(),
        party_class: party.party_class.as_str(),
        trust_class: party.trust_class.as_str(),
        edge_role: party.edge_role.as_str(),
        citation_handles: party.citation_handles.clone(),
        author: conflict_party_author(record),
        observed_at: conflict_party_observed_at(record),
        confidence: conflict_party_confidence(record),
        target_handle: party.target_handle,
        summary,
        summary_hash,
    }
}

/// Projects one conflict to its wire view.
pub(crate) fn conflict_json<'a>(conflict: &query::Conflict<'a>) -> ConflictJson<'a> {
    ConflictJson {
        conflict_id: [conflict.key.0, conflict.key.1],
        edge_ids: conflict.edge_ids.clone(),
        parties: conflict.parties.iter().map(conflict_party_json).collect(),
        resolved: conflict.resolved,
        resolved_by: conflict.resolved_by.clone(),
    }
}

/// Emits the conflicts answer for a resolved scope (issue #232).
pub(crate) fn query_conflicts_cmd(
    records: &[GraphRecord],
    scope_handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    include_resolved: bool,
    format: OutputFormat,
) -> Result<()> {
    let scope = match query::resolve_conflict_scope(records, scope_handle, index, repo_scope) {
        Ok(scope) => scope,
        Err(err) => {
            eprintln!("{}", serde_json::to_string(&err)?);
            std::process::exit(1);
        }
    };

    // A scope that resolved to nothing live is a no-match (or a stale handle
    // when it named only tombstoned records). Distinct from a valid scope with
    // no recorded conflicts, which is a real exit-0 empty answer below.
    if scope.is_empty() {
        let code = if scope.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "handle": scope_handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let ctx = query::conflicts_context(records, &scope, include_resolved);
    let conflicts: Vec<ConflictJson<'_>> = ctx.conflicts.iter().map(conflict_json).collect();
    let count = conflicts.len();
    let response = ConflictsResponse {
        ok: true,
        scope: scope.handle.as_str(),
        scope_kind: scope.kind.as_str(),
        target_handle: scope.target_handle.as_str(),
        target_ids: scope.anchor_ids.iter().map(String::as_str).collect(),
        include_resolved,
        conflicts,
        count,
        counts: ConflictsCountsJson::from(&ctx.counts),
        diagnostics: ctx
            .diagnostics
            .iter()
            .map(|d| ConflictDiagnosticJson {
                code: d.code,
                detail: d.detail.as_str(),
            })
            .collect(),
    };
    print_result(&response, format)
}
