use super::*;

// ---------------------------------------------------------------------------
// cost query (issue #132)
// ---------------------------------------------------------------------------

/// `eg query cost` (issue #132): per-run agent cost rollup from imported
/// trajectory `CostUsage` records, plus aggregate sums over the filtered set.
///
/// The envelope carries the standing epistemic disclaimer, the per-run rows
/// (record IDs, run/session/source handles, verbatim transcript values), the
/// totals, and the sorted diagnostics. Rows and totals are serialized from
/// the SAME core values [`query::cost_rollup`] returns, so the CLI and any
/// future daemon verb cannot drift.
pub(crate) fn query_cost_cmd(
    records: &[GraphRecord],
    filters: &query::CostFilters,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let rollup = query::cost_rollup(records, filters, limit);
    match format {
        OutputFormat::Json => {
            #[derive(Serialize)]
            struct CostResponse<'a> {
                ok: bool,
                lane: &'static str,
                disclaimer: &'static str,
                rows: &'a [query::CostRow],
                totals: &'a query::CostTotals,
                diagnostics: &'a [query::CostDiagnostic],
            }
            let response = CostResponse {
                ok: true,
                lane: "cost",
                disclaimer: rollup.disclaimer,
                rows: &rollup.rows,
                totals: &rollup.totals,
                diagnostics: &rollup.diagnostics,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize cost rollup")?;
            println!("{output}");
        }
        OutputFormat::Text => print!("{}", render_cost_text(&rollup)),
    }
    Ok(())
}

/// Deterministic human-readable rendering of a cost rollup.
///
/// Every free-text field that reaches rendered output (model name, baseline
/// model, task handle) passes through [`query::bounded_cost_text`] first: an
/// embedded newline or ANSI escape would otherwise forge output lines or
/// drive the reader's terminal. The JSON transport needs no such step (serde
/// escapes control characters) and keeps the raw values.
pub(crate) fn render_cost_text(rollup: &query::CostRollup) -> String {
    use std::fmt::Write as _;

    fn money(value: Option<f64>) -> String {
        value.map_or_else(|| "unknown".to_owned(), |v| format!("{v:.2}"))
    }
    fn count(value: Option<u64>) -> String {
        value.map_or_else(|| "unknown".to_owned(), |v| v.to_string())
    }
    fn secs(value: Option<f64>) -> String {
        value.map_or_else(|| "unknown".to_owned(), |v| format!("{v:.1}s"))
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "cost: {} row(s) ({} matching)",
        rollup.rows.len(),
        rollup.totals.matching_rows
    );
    for row in &rollup.rows {
        let _ = writeln!(
            out,
            "- {} run={} session={} actual_usd={} total_usd={} baseline_usd={} \
             prompt={} cache_read={} completion={} duration={} model={} verified={}",
            row.record_id,
            row.run_id.as_deref().unwrap_or("unknown"),
            row.session_id.as_deref().unwrap_or("unknown"),
            money(row.actual_cost_usd),
            money(row.total_cost_usd),
            money(row.baseline_cost_usd),
            count(row.prompt_tokens),
            count(row.cache_read_tokens),
            count(row.completion_tokens),
            secs(row.duration_secs),
            query::bounded_cost_text(row.model_name.as_deref().unwrap_or("unknown")),
            row.verification_outcome.as_deref().unwrap_or("unknown"),
        );
    }
    let t = &rollup.totals;
    let _ = writeln!(
        out,
        "totals: actual_usd={} total_usd={} baseline_usd={} prompt={} cache_read={} \
         completion={} duration={}",
        money(t.actual_cost_usd),
        money(t.total_cost_usd),
        money(t.baseline_cost_usd),
        count(t.prompt_tokens),
        count(t.cache_read_tokens),
        count(t.completion_tokens),
        secs(t.duration_secs),
    );
    if t.rows_truncated {
        let _ = writeln!(
            out,
            "note: rows truncated to the requested limit; totals cover all {} matching rows",
            t.matching_rows
        );
    }
    for d in &rollup.diagnostics {
        let _ = writeln!(out, "diagnostic {}: {}", d.code, d.record_id);
    }
    let _ = writeln!(out, "disclaimer: {}", rollup.disclaimer);
    out
}
