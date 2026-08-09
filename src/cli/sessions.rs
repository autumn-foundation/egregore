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
///
/// Every free-text field — `agent_id`, `session_id`, the `summary_label` that
/// interpolates them, and the repository's own display name (a local basename
/// or operator override, itself unbounded importer/operator-supplied text) —
/// is passed through [`query::bounded_session_text`] first: an embedded
/// newline or ANSI escape would otherwise forge output lines or drive the
/// reader's terminal. The JSON transport needs no such step (serde escapes
/// control characters) and keeps the raw values.
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
        repository.map_or_else(
            || "<unnamed repository>".to_owned(),
            query::bounded_session_text
        )
    );
    let _ = writeln!(out, "disclaimer: {}", query::SESSIONS_DISCLAIMER);
    let _ = writeln!(
        out,
        "unsupported count kinds: {}",
        query::SESSIONS_UNSUPPORTED_COUNT_KINDS.join(", ")
    );
    for row in &digest.sessions {
        render_session_row_text(&mut out, row);
    }
    for diagnostic in &digest.diagnostics {
        render_diagnostic_text(&mut out, diagnostic);
    }
    out
}

/// Renders one session row block.
fn render_session_row_text(out: &mut String, row: &query::SessionRow) {
    use std::fmt::Write as _;

    let _ = writeln!(
        out,
        "session {} [{}] agent_record_id {} agent_id {} session_id {}",
        row.session_record_id,
        row.trust_class,
        // The citable, edge-derived handle — distinct from the uncitable
        // stamped `agent_id` string printed alongside it: a session with a
        // live `SESSION_OF -> Agent` edge and one with only a matching string
        // must not look identical in text output.
        row.agent_record_id.as_deref().unwrap_or("<absent>"),
        row.agent_id
            .as_deref()
            .map_or_else(|| "<absent>".to_owned(), query::bounded_session_text),
        row.session_id
            .as_deref()
            .map_or_else(|| "<absent>".to_owned(), query::bounded_session_text)
    );
    let _ = writeln!(
        out,
        "  summary {} ({})",
        query::bounded_session_text(&row.summary_label),
        row.summary_hash.as_deref().unwrap_or("<absent>")
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
        "  ingested {} .. {}",
        row.first_ingested_at.as_deref().unwrap_or("<absent>"),
        row.last_ingested_at.as_deref().unwrap_or("<absent>")
    );
    let _ = writeln!(
        out,
        "  scope {} via {}",
        row.repository_scope.join(", "),
        row.scope_basis.join(", ")
    );
    for (repository, bases) in &row.scope_basis_by_repository {
        let _ = writeln!(out, "    scope {repository} via {}", bases.join(", "));
    }
    let _ = writeln!(out, "  aggregation_scope {}", row.aggregation_scope);
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

/// Renders one diagnostic line, INCLUDING every payload field it carries: a
/// text reader must not have to switch to `--format json` to learn what a
/// truncation dropped or which sessions a set-valued diagnostic named.
fn render_diagnostic_text(out: &mut String, diagnostic: &query::SessionsDiagnostic) {
    use std::fmt::Write as _;

    let mut payload = String::new();
    for (field, value) in [
        ("count", diagnostic.count),
        ("matched", diagnostic.matched),
        ("returned", diagnostic.returned),
        ("limit", diagnostic.limit),
    ] {
        if let Some(value) = value {
            let _ = write!(payload, " {field} {value}");
        }
    }
    if let Some(ids) = diagnostic.session_record_ids.as_ref() {
        let _ = write!(payload, " sessions [{}]", ids.join(", "));
    }
    let _ = writeln!(
        out,
        "diagnostic {}{}{}{payload}",
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
