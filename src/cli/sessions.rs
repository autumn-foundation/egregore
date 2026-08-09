use super::*;

// ---------------------------------------------------------------------------
// sessions query (issue #112)
// ---------------------------------------------------------------------------

/// `eg query sessions <REPO>` (issue #112): print the repo-scoped,
/// recency-ordered digest of recent agent sessions.
///
/// The envelope carries the resolved repository identity, the standing
/// disclaimer, the disclosed unsupported count kinds, the ordered rows, and the
/// sorted diagnostics. Rows and diagnostics are serialized from the SAME core
/// values the daemon verb `agent_sessions_for_repo` returns, so the two
/// transports cannot drift.
pub(crate) fn query_sessions_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repository_id: &str,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let digest = query::sessions_for_repo(records, index, repository_id, limit);
    let repository = index.display_of(repository_id);

    match format {
        OutputFormat::Json => {
            #[derive(Serialize)]
            struct SessionsResponse<'a> {
                ok: bool,
                lane: &'static str,
                repository_id: &'a str,
                repository: Option<&'a str>,
                disclaimer: &'static str,
                unsupported_count_kinds: &'static [&'static str],
                sessions: &'a [query::SessionRow],
                diagnostics: &'a [query::SessionsDiagnostic],
            }
            let response = SessionsResponse {
                ok: true,
                lane: "sessions",
                repository_id,
                repository,
                disclaimer: query::SESSIONS_DISCLAIMER,
                unsupported_count_kinds: query::SESSIONS_UNSUPPORTED_COUNT_KINDS,
                sessions: &digest.sessions,
                diagnostics: &digest.diagnostics,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize sessions digest")?;
            println!("{output}");
        }
        OutputFormat::Text => print!(
            "{}",
            render_sessions_text(&digest, repository_id, repository)
        ),
    }
    Ok(())
}

/// Deterministic human-readable rendering of a sessions digest.
pub(crate) fn render_sessions_text(
    digest: &query::SessionsDigest,
    repository_id: &str,
    repository: Option<&str>,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "sessions: {} row(s) for {} ({repository_id})",
        digest.sessions.len(),
        repository.unwrap_or("<unnamed repository>")
    );
    let _ = writeln!(out, "disclaimer: {}", query::SESSIONS_DISCLAIMER);
    let _ = writeln!(
        out,
        "unsupported count kinds: {}",
        query::SESSIONS_UNSUPPORTED_COUNT_KINDS.join(", ")
    );
    for row in &digest.sessions {
        let _ = writeln!(
            out,
            "session {} [{}] agent_id {} session_id {}",
            row.session_record_id,
            row.trust_class,
            row.agent_id.as_deref().unwrap_or("<absent>"),
            row.session_id.as_deref().unwrap_or("<absent>")
        );
        let _ = writeln!(
            out,
            "  activity {} .. {} ({} source(s), {})",
            row.first_activity.as_deref().unwrap_or("<absent>"),
            row.last_activity.as_deref().unwrap_or("<absent>"),
            row.time_source_count,
            row.time_basis
        );
        let _ = writeln!(
            out,
            "  scope {} via {}",
            row.repository_scope.join(", "),
            row.scope_basis.join(", ")
        );
        let _ = writeln!(out, "  run_status {}", row.run_status);
        for run in &row.runs {
            let _ = writeln!(
                out,
                "    run {} outcome {} exit_reason {} at {}",
                run.run_record_id,
                run.outcome.as_deref().unwrap_or("<unrecorded>"),
                run.exit_reason.as_deref().unwrap_or("<unrecorded>"),
                run.observed_at.as_deref().unwrap_or("<absent>")
            );
        }
        for task in &row.tasks {
            let _ = writeln!(
                out,
                "    task {} status {} (recorded {}, {})",
                task.record_id, task.status, task.status_recorded, task.trust_class
            );
        }
        let _ = writeln!(
            out,
            "  counts observation {} decision {} failure {} lesson {}",
            row.record_counts.observation,
            row.record_counts.decision,
            row.record_counts.failure,
            row.record_counts
                .lesson
                .map_or_else(|| "<unsupported>".to_owned(), |n| n.to_string())
        );
    }
    for diagnostic in &digest.diagnostics {
        let _ = writeln!(
            out,
            "diagnostic {}{}{}",
            diagnostic.code,
            diagnostic
                .session_record_id
                .as_deref()
                .map(|id| format!(" session {id}"))
                .unwrap_or_default(),
            diagnostic
                .run_record_id
                .as_deref()
                .map(|id| format!(" run {id}"))
                .unwrap_or_default()
        );
    }
    out
}
