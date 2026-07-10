use super::*;

pub(crate) fn query_lifeline_cmd(
    records: &[GraphRecord],
    symbol: &str,
    repo_id: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    /// Prints a stable machine-readable lifeline diagnostic and exits.
    fn fail(
        code: &str,
        msg: &str,
        candidates: Option<&[String]>,
        format: OutputFormat,
        exit_code: i32,
    ) -> ! {
        match format {
            OutputFormat::Json => {
                let mut error = serde_json::json!({
                    "code": code,
                    "message": msg
                });
                if let Some(candidates) = candidates {
                    error["candidates"] = serde_json::json!(candidates);
                }
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": error
                });
                println!(
                    "{}",
                    serde_json::to_string(&envelope).expect("diagnostic envelope must serialize")
                );
            }
            OutputFormat::Text => match candidates {
                Some(candidates) => {
                    eprintln!("Error: {msg}. Candidates: {}", candidates.join(", "));
                }
                None => eprintln!("Error: {msg}"),
            },
        }
        std::process::exit(exit_code);
    }

    match query::symbol_lifeline(records, symbol, repo_id) {
        Ok(events) => {
            if events.is_empty() {
                let msg = format!("symbol matched but has no commit-linked history: {symbol}");
                fail("no_history", &msg, None, format, 2);
            }
            match format {
                OutputFormat::Json => {
                    // Newline-delimited JSON: one standalone event object per
                    // line, chronologically ordered (issue #215).
                    for ev in &events {
                        println!("{}", serde_json::to_string(ev)?);
                    }
                }
                OutputFormat::Text => {
                    println!("Advisory temporal facts: where and when this symbol changed");
                    for ev in &events {
                        let citation = match (&ev.repo_relative_path, &ev.span) {
                            (Some(path), Some(span)) => {
                                format!(" @ {path}:{}-{}", span.start_line, span.end_line)
                            }
                            (Some(path), None) => {
                                let reason = ev
                                    .absent_span_reason
                                    .as_ref()
                                    .map_or_else(String::new, |r| format!(" [{r}]"));
                                format!(" @ {path}{reason}")
                            }
                            (None, _) => ev
                                .absent_span_reason
                                .as_ref()
                                .map_or_else(String::new, |reason| format!(" [{reason}]")),
                        };
                        let drift = match (ev.drift_score, &ev.drift_record_id) {
                            (Some(score), Some(id)) => format!(" drift={score:.4} ({id})"),
                            _ => " drift=absent".to_string(),
                        };
                        println!(
                            "[{}] commit={} valid_time={} record_id={}{}{}",
                            ev.event_type, ev.commit, ev.valid_time, ev.record_id, citation, drift
                        );
                    }
                }
            }
            Ok(())
        }
        Err(query::LifelineError::UnknownSymbol { query }) => {
            let msg = format!("symbol not found in the graph: {query}");
            fail("unknown_symbol", &msg, None, format, 2);
        }
        Err(query::LifelineError::AmbiguousSymbol { query, candidates }) => {
            let msg = format!("ambiguous symbol name '{query}' matches multiple symbols");
            fail("ambiguous_symbol", &msg, Some(&candidates), format, 6);
        }
    }
}
