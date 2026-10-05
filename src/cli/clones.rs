use super::*;

/// Runs `eg query clones` (issue #216): group exact-duplicate Rust symbol
/// bodies into citable clone classes.
///
/// The JSON envelope is emitted as a single line so machine consumers keep the
/// one-JSON-object-per-line contract from `docs/cli/query.md`, and classes are
/// byte-identical across repeated runs on an unchanged store.
pub(crate) fn query_clones_cmd(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    min_size: usize,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let report = query::clone_classes(records, repo_id, min_size, limit);
    print_clone_classes_report(&report, format)
}

/// Prints a [`query::CloneClassReport`] in the requested format. Shared by the
/// local `--graph` / `--data-dir` path and the `--daemon` path (which
/// round-trips the daemon-computed report through the same struct), so the two
/// transports emit the same envelope.
pub(crate) fn print_clone_classes_report(
    report: &query::CloneClassReport,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let envelope = serde_json::json!({
                "ok": true,
                "result": report,
            });
            println!("{}", serde_json::to_string(&envelope)?);
        }
        OutputFormat::Text => {
            if report.classes.is_empty() {
                println!(
                    "No clone classes ({}).",
                    report.empty_reason.as_deref().unwrap_or("empty")
                );
            }
            for class in &report.classes {
                println!("clone class {}: {} members", class.content_hash, class.size);
                for member in &class.members {
                    println!(
                        "  {} {} @ {}:{}-{}",
                        member.record_id,
                        member.qualified_name,
                        member.repo_relative_path,
                        member.span.start_line,
                        member.span.end_line
                    );
                }
            }
            if report.truncated {
                println!(
                    "truncated: showing {} of {} classes (raise --limit, max {})",
                    report.returned_class_count,
                    report.total_class_count,
                    query::CLONES_MAX_LIMIT
                );
            }
        }
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn query_clones_via_daemon(
    data_dir: &Path,
    min_size: usize,
    limit: usize,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut params = serde_json::json!({ "limit": limit as u64, "min_size": min_size as u64 });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let result = client
        .query_verb_raw("clone_classes", &params, None)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;
    // The daemon returns the full report under `result.report` (plus the
    // classes as `result.records` for row-oriented consumers); deserialize it
    // into the shared report struct so the daemon transport prints the same
    // envelope as the local path.
    let report: query::CloneClassReport = serde_json::from_value(result["report"].clone())
        .context("daemon clone_classes response missing report")?;
    print_clone_classes_report(&report, format)
}
