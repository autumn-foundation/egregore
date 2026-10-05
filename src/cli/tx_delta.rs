use super::*;

/// One record on the transaction-time delta feed.
///
/// Carries only redaction-safe handles: record ID, domain, kind, event class,
/// the transaction-time handle, and citable source handles (name,
/// repo-relative path, span). No summaries, bodies, or raw payloads are
/// emitted.
#[derive(Serialize)]
struct SinceRow<'a> {
    record_id: &'a str,
    domain: &'a str,
    kind: &'a str,
    event: query::TxDeltaEvent,
    transaction_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

/// Response envelope for `eg query since`.
#[derive(Serialize)]
struct SinceEnvelope<'a> {
    ok: bool,
    verb: &'static str,
    tx_after: &'a str,
    domains: &'a [String],
    next_cursor: String,
    up_to_date: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_reason: Option<&'static str>,
    total_matching: usize,
    returned: usize,
    truncated: bool,
    records: Vec<SinceRow<'a>>,
    diagnostics: Vec<query::TxDiagnostic>,
}

/// Builds a redaction-safe delta row from a selected record.
fn since_row<'a>(item: &'a query::TxDeltaItem<'a>) -> SinceRow<'a> {
    let (name, repo_relative_path, span) = match item.record {
        GraphRecord::Node {
            name,
            repo_relative_path,
            span,
            ..
        } => (name.as_deref(), repo_relative_path.as_deref(), *span),
        // Only nodes carry a transaction-time handle, so the feed never
        // selects edges or tombstones; the arm exists for exhaustiveness.
        GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => (None, None, None),
    };
    SinceRow {
        record_id: item.record.id(),
        domain: item.domain.as_str(),
        kind: item.kind,
        event: item.event,
        transaction_time: item
            .transaction_time
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        superseded_by: item.superseded_by,
        name,
        repo_relative_path,
        span,
    }
}

/// Runs `eg query since` (issue #197): the transaction-time delta feed.
///
/// Returns every record whose transaction-time handle is strictly after
/// `tx_after`, ordered by `(transaction_time, record_id)`, with a
/// `next_cursor` for gap-free resumption. A malformed cursor, unknown domain,
/// or bad limit exits 1 with the standard machine-readable error envelope.
pub(crate) fn query_since_cmd(
    records: &[GraphRecord],
    tx_after: &str,
    domains: &[String],
    repo_id: Option<&str>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let result = match query::transaction_delta(records, tx_after, domains, repo_id, limit) {
        Ok(result) => result,
        Err(err) => {
            print_tx_error(&err.code, &err.message)?;
            std::process::exit(1);
        }
    };
    let rows: Vec<SinceRow<'_>> = result.items.iter().map(since_row).collect();
    let envelope = SinceEnvelope {
        ok: true,
        verb: "since",
        tx_after,
        domains,
        next_cursor: result.next_cursor.clone(),
        up_to_date: result.up_to_date,
        empty_reason: result.up_to_date.then_some(query::SINCE_EMPTY_REASON),
        total_matching: result.total_matching,
        returned: result.returned,
        truncated: result.truncated,
        records: rows,
        diagnostics: result.diagnostics,
    };
    match format {
        OutputFormat::Json => {
            // Single-line JSON object per the `docs/cli/query.md` contract.
            println!("{}", serde_json::to_string(&envelope)?);
        }
        OutputFormat::Text => {
            if envelope.up_to_date {
                println!("Up to date: no records with transaction_time after {tx_after}.");
            }
            for row in &envelope.records {
                let name = row.name.unwrap_or("(unnamed)");
                let path = row.repo_relative_path.unwrap_or("(unknown)");
                let line = row
                    .span
                    .map_or(String::new(), |s| format!(":{}", s.start_line));
                let via = row
                    .superseded_by
                    .map_or(String::new(), |s| format!(" superseded_by={s}"));
                println!(
                    "{} {} {} {} {} {name} @ {path}{line}{via}",
                    row.transaction_time,
                    row.domain,
                    row.kind,
                    row.record_id,
                    row.event.as_str(),
                );
            }
            if envelope.truncated {
                println!(
                    "# truncated: showing {} of {} (raise --limit, max {})",
                    envelope.returned,
                    envelope.total_matching,
                    query::SINCE_MAX_LIMIT
                );
            }
            println!("# next_cursor: {}", envelope.next_cursor);
            for diag in &envelope.diagnostics {
                println!("# {}: {}", diag.code, diag.message);
            }
        }
    }
    Ok(())
}
