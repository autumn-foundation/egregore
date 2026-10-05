use super::*;

/// Runs `eg query complexity` (issue #162): rank Rust callable symbols by
/// deterministic structural complexity, highest first.
///
/// The JSON envelope is emitted as a single line so machine consumers keep the
/// one-JSON-object-per-line contract from `docs/cli/query.md`, and the ranking
/// is byte-identical across repeated runs on an unchanged store.
pub(crate) fn query_complexity_cmd(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match query::symbol_complexity_ranking(records, repo_id, limit) {
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
                        "Symbol complexity ranking: structural complexity (1 + decision points), highest first"
                    );
                    for symbol in &report.symbols {
                        println!(
                            "{}. {} [{}] complexity={} ({})",
                            symbol.rank,
                            symbol.name,
                            symbol.symbol_kind,
                            symbol.complexity,
                            symbol.symbol_record_id
                        );
                    }
                    if report.truncated {
                        println!(
                            "truncated: showing {} of {} symbols (raise --limit, max {})",
                            report.returned_symbol_count,
                            report.total_symbol_count,
                            query::COMPLEXITY_MAX_LIMIT
                        );
                    }
                    println!("corpus: {}", report.corpus_mode);
                }
            }
            Ok(())
        }
        Err(err @ query::SymbolComplexityError::NoMatch) => {
            complexity_error_exit(&err, "no_match", format)
        }
    }
}

/// Prints the stable complexity error envelope and exits 2 (nothing to rank).
pub(crate) fn complexity_error_exit(
    err: &query::SymbolComplexityError,
    code: &str,
    format: OutputFormat,
) -> ! {
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
