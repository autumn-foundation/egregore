use super::*;

/// Runs `eg query churn` (issue #128): rank files by distinct-commit change
/// frequency over a `scan-history` temporal store.
///
/// The JSON envelope is emitted as a single line so machine consumers keep the
/// one-JSON-object-per-line contract from `docs/cli/query.md`, and the ranking
/// is byte-identical across repeated runs on unchanged history.
pub(crate) fn query_churn_cmd(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match query::file_churn(records, repo_id, limit) {
        Ok(report) => {
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": true,
                        "result": report,
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    println!(
                        "File churn ranking: distinct commits that modified each file, highest first"
                    );
                    for range in &report.commit_ranges {
                        let scope = range
                            .repository
                            .as_deref()
                            .or(range.repository_id.as_deref())
                            .unwrap_or("(unattributed)");
                        println!(
                            "range {}..{} ({} commits) [{scope}]",
                            range.first_commit, range.last_commit, range.commit_count
                        );
                    }
                    for file in &report.files {
                        println!(
                            "{}. {} commits={} ({})",
                            file.rank,
                            file.repo_relative_path,
                            file.commit_count,
                            file.file_record_id
                        );
                    }
                    if report.truncated {
                        println!(
                            "truncated: showing {} of {} files (raise --limit, max {})",
                            report.returned_file_count,
                            report.total_file_count,
                            query::CHURN_MAX_LIMIT
                        );
                    }
                    println!("corpus: {}", report.corpus_mode);
                }
            }
            Ok(())
        }
        Err(err @ query::FileChurnError::NoHistory) => churn_error_exit(&err, "no_history", format),
        Err(err @ query::FileChurnError::NoMatch) => churn_error_exit(&err, "no_match", format),
    }
}

/// Prints the stable churn error envelope and exits 2 (nothing to rank).
pub(crate) fn churn_error_exit(err: &query::FileChurnError, code: &str, format: OutputFormat) -> ! {
    match format {
        OutputFormat::Json => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": code,
                    "message": err.to_string(),
                }
            });
            println!("{envelope}");
        }
        OutputFormat::Text => {
            eprintln!("Error: {err}");
        }
    }
    std::process::exit(2);
}
