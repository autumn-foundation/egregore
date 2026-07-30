use super::*;

/// Builds one redaction-safe failed-attempt view from a context attempt.
pub(crate) fn failure_attempt_json<'a>(
    attempt: &query::FailureAttempt<'a>,
) -> FailureAttemptJson<'a> {
    let item = audit_item(&attempt.item);
    let (failure_kind, executed_at) = match attempt.item.record {
        GraphRecord::Node {
            failure_kind,
            executed_at,
            ..
        } => (failure_kind.as_deref(), executed_at.as_deref()),
        _ => (None, None),
    };
    FailureAttemptJson {
        item,
        resolution_status: attempt.status.as_str(),
        resolved_by: attempt.resolved_by,
        matched_target: attempt.matched_target,
        failure_kind,
        executed_at,
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn query_failures_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at_head: bool,
    all_history: bool,
) -> Result<()> {
    // Corpus-mode selection (issue #456): head-anchor by default over a
    // scan-history store; `--all-history` opts into the union. Pre-filter drops
    // off-HEAD records BEFORE traversal.
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, index, false, at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    let target = match query::resolve_failure_handle(records, handle, index, repo_scope) {
        Ok(t) => t,
        Err(
            err @ (query::FailureHandleError::Ambiguous { .. }
            | query::FailureHandleError::Unsupported { .. }),
        ) => {
            eprintln!("{}", serde_json::to_string(&err)?);
            std::process::exit(1);
        }
    };

    // A handle that resolved to nothing live in the store is a no-match (or a
    // stale handle when it named a tombstoned record). This is distinct from a
    // resolved target that simply has no recorded failures, which is a real
    // exit-0 empty answer below (AC6).
    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let ctx = query::failure_history_context(records, &target);

    let runtime_failures: Vec<FailureAttemptJson<'_>> = ctx
        .runtime_failures
        .iter()
        .map(failure_attempt_json)
        .collect();
    let agent_failures: Vec<FailureAttemptJson<'_>> = ctx
        .agent_failures
        .iter()
        .map(failure_attempt_json)
        .collect();
    let superseding_successes: Vec<AuditItem<'_>> =
        ctx.superseding_successes.iter().map(audit_item).collect();
    let patch_artifacts: Vec<AuditItem<'_>> = ctx.patch_artifacts.iter().map(audit_item).collect();

    // Diagnostics: context diagnostics + protected-payload + redaction markers
    // for every reached record, exactly as the memory audit (AC6, AC8).
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
    for attempt in ctx.agent_failures.iter().chain(&ctx.runtime_failures) {
        protected_payload_diagnostics(attempt.item.record, &mut diagnostics);
        if let GraphRecord::Node {
            id,
            redaction_policy_version: Some(ver),
            ..
        } = attempt.item.record
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
    for item in ctx.superseding_successes.iter().chain(&ctx.patch_artifacts) {
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
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
            && a.target_domain == b.target_domain
    });

    let returned = runtime_failures.len()
        + agent_failures.len()
        + superseding_successes.len()
        + patch_artifacts.len();

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    let response = FailureHistoryResponse {
        ok: true,
        target_handle: handle,
        target_type: ctx.target_kind,
        target_ids: ctx.target_ids.iter().map(String::as_str).collect(),
        runtime_failures,
        agent_failures,
        superseding_successes,
        patch_artifacts,
        agent_sessions: ctx.agent_sessions.iter().map(|r| r.id()).collect(),
        agents: ctx.agents.iter().map(|r| r.id()).collect(),
        diagnostics,
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned,
        },
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize failure history")?;
    println!("{output}");
    Ok(())
}
