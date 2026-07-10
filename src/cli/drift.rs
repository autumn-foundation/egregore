use super::*;

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn query_drift_via_daemon(
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut params = serde_json::json!({ "limit": limit as u64 });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let records = client
        .query_verb("drift_top_n", &params, None)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;
    if records.is_empty() {
        eprintln!("error: no match found — no SemanticDrift nodes in graph");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_drift_record(rec, format)?;
    }
    Ok(())
}

/// Prints a daemon drift record (`serde_json::Value`) in the requested format.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn print_daemon_drift_record(
    rec: &serde_json::Value,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let name = rec["name"].as_str().unwrap_or("(unknown)");
            // score may be a JSON string ("0.92") or a JSON number (0.92);
            // accept both so text output is correct regardless of serialisation path.
            let score_owned = rec["score"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| rec["score"].as_f64().map(|f| f.to_string()))
                .unwrap_or_else(|| "?".to_owned());
            let score = score_owned.as_str();
            let before = rec["before_commit"].as_str().unwrap_or("?");
            let after = rec["after_commit"].as_str().unwrap_or("?");
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            println!("{name} score={score} {before}..{after} @ {path}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query drift
// ---------------------------------------------------------------------------

pub(crate) fn query_drift(
    records: &[GraphRecord],
    limit: usize,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
) -> Result<()> {
    // Rank first, then apply the repository scope, then truncate: the limit
    // must bound the scoped result set, not pre-empt it.
    let mut drifts = query::largest_semantic_drifts(records, usize::MAX);
    if let Some(repo) = selected_repo {
        drifts.retain(|r| index.owner_of(r.id()) == Some(repo));
    }
    drifts.truncate(limit);

    if drifts.is_empty() {
        eprintln!("error: no match found — no SemanticDrift nodes in graph");
        std::process::exit(2);
    }

    for record in drifts {
        let GraphRecord::Node {
            id,
            schema_version,
            semantic_drift: Some(drift),
            repo_relative_path: drift_path,
            name: drift_name,
            ..
        } = record
        else {
            continue;
        };

        let (resolved_path, resolved_name, resolved_span) = query::resolve_drift_target(
            records,
            id,
            drift,
            drift_path.as_deref(),
            drift_name.as_deref(),
        );

        let repository_id = index.owner_of(id);
        let result = DriftResult {
            record_id: id,
            schema_version: *schema_version,
            before_commit: &drift.before_git_commit,
            after_commit: &drift.after_git_commit,
            before_valid_time: &drift.before_valid_time,
            after_valid_time: &drift.after_valid_time,
            embedding_model_provider: &drift.embedding_model.provider,
            embedding_model_name: &drift.embedding_model.name,
            embedding_model_version: &drift.embedding_model.version,
            embedding_model_dim: drift.embedding_model.dim,
            embedding_model_content_hash: &drift.embedding_model.content_hash,
            metric_kind: drift.metric_kind.as_str(),
            prior_record_id: &drift.prior_record_id,
            target_record_id: &drift.target_record_id,
            score: drift.score,
            selection_threshold: drift.selection_threshold,
            selection_basis: drift.selection_basis.as_str(),
            repo_relative_path: resolved_path,
            name: resolved_name,
            span: resolved_span,
            repository_id,
            repository: repository_id.and_then(|repo| index.display_of(repo)),
            status: "drift is a lead, not proof",
        };
        print_result(&result, format)?;
    }
    Ok(())
}

impl PrintText for DriftResult<'_> {
    fn as_text(&self) -> String {
        let name = self.name.unwrap_or("(unknown)");
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        format!(
            "{name} score={:.6} {}..{} @ {path}",
            self.score, self.before_commit, self.after_commit
        )
    }
}
