use super::*;

/// Runs `eg query recency` (issue #219): rank a repository's indexed symbols by
/// least-recent last change — most dormant first — over a `scan-history`
/// temporal store.
///
/// The JSON envelope is emitted as a single line so machine consumers keep the
/// one-JSON-object-per-line contract from `docs/cli/query.md`, and the ranking
/// is byte-identical across repeated runs on unchanged history.
pub(crate) fn query_recency_cmd(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match query::symbol_recency(records, repo_id, limit) {
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
                        "Symbol recency ranking: least-recent last change first (dormancy \
                         measured against the newest indexed commit, never wall-clock)"
                    );
                    for anchor in &report.anchors {
                        let scope = anchor
                            .repository
                            .as_deref()
                            .or(anchor.repository_id.as_deref())
                            .unwrap_or("(unattributed)");
                        println!(
                            "anchor {} @ {} [{scope}]",
                            anchor.commit_sha, anchor.valid_time
                        );
                    }
                    for symbol in &report.symbols {
                        let span = symbol.span.map_or_else(
                            || {
                                symbol
                                    .absent_span_reason
                                    .clone()
                                    .unwrap_or_else(|| "no-span".to_owned())
                            },
                            |s| format!("{}-{}", s.start_line, s.end_line),
                        );
                        println!(
                            "{}. {}:{} {} dormant={}d last_change={} ({})",
                            symbol.rank,
                            symbol.repo_relative_path,
                            span,
                            symbol.symbol_name,
                            symbol.dormancy_days,
                            symbol.last_change_commit,
                            symbol.record_id
                        );
                    }
                    if report.truncated {
                        println!(
                            "truncated: showing {} of {} symbols (raise --limit, max {})",
                            report.returned_symbol_count,
                            report.total_symbol_count,
                            query::RECENCY_MAX_LIMIT
                        );
                    }
                    println!("corpus: {}", report.corpus_mode);
                }
            }
            Ok(())
        }
        Err(err @ query::RecencyError::NoHistory) => recency_error_exit(&err, "no_history", format),
        Err(err @ query::RecencyError::NoMatch) => recency_error_exit(&err, "no_match", format),
    }
}

/// Prints the stable recency error envelope and exits 2 (nothing to rank).
pub(crate) fn recency_error_exit(err: &query::RecencyError, code: &str, format: OutputFormat) -> ! {
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
