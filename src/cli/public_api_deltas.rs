use super::*;

/// `eg query public-api-deltas` (issue #157): classified public-API surface
/// changes between two commit handles.
///
/// Exit codes follow the `eg query deltas` convention: `0` on success
/// (including a resolved range with no surface changes), `2` when a commit
/// handle resolves to nothing or the history is empty (no match), and `1`
/// for the remaining stable diagnostics (ambiguous prefix, identical
/// endpoints, reversed range, no ancestor path).
pub(crate) fn query_public_api_deltas_cmd(
    records: &[GraphRecord],
    base: &str,
    head: &str,
    repo: Option<&str>,
    options: query::PublicApiDeltasOptions,
    format: OutputFormat,
) -> Result<()> {
    let index = query::RepositoryIndex::build(records);
    let repo_scope = resolve_repo_scope(&index, repo);
    match query::public_api_deltas(records, base, head, repo_scope.as_deref(), options) {
        Ok(report) => {
            match format {
                OutputFormat::Json => {
                    #[derive(Debug, Clone, serde::Serialize)]
                    struct PublicApiDeltasResponse<'a> {
                        ok: bool,
                        #[serde(flatten)]
                        report: query::PublicApiDeltas<'a>,
                    }
                    let response = PublicApiDeltasResponse { ok: true, report };
                    let output = serde_json::to_string_pretty(&response)
                        .context("failed to serialize public-api deltas")?;
                    println!("{output}");
                }
                OutputFormat::Text => print!("{}", render_public_api_deltas_text(&report)),
            }
            Ok(())
        }
        Err(err) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct PublicApiDeltasErrorResponse {
                ok: bool,
                error: query::RangeDeltasError,
            }
            let response = PublicApiDeltasErrorResponse {
                ok: false,
                error: err.clone(),
            };
            let output = serde_json::to_string(&response)
                .context("failed to serialize public-api-deltas error")?;
            println!("{output}");
            let exit_code = match err {
                query::RangeDeltasError::MissingCommit { .. }
                | query::RangeDeltasError::EmptyHistory => 2,
                _ => 1,
            };
            std::process::exit(exit_code);
        }
    }
}

/// Deterministic human-readable rendering of a public-api-deltas report.
pub(crate) fn render_public_api_deltas_text(report: &query::PublicApiDeltas<'_>) -> String {
    use std::fmt::Write as _;

    fn write_rows(out: &mut String, label: &str, rows: &[query::PublicApiDeltaRow<'_>]) {
        let _ = writeln!(out, "{label} ({}):", rows.len());
        for row in rows {
            let line = row
                .span
                .map_or_else(String::new, |span| format!(":{}", span.start_line));
            let breaking = if row.potentially_breaking {
                " [potentially breaking]"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "- {} [{}] {}{} commit {} ({}){}",
                row.name,
                row.symbol_kind.unwrap_or("symbol"),
                row.repo_relative_path,
                line,
                row.commit,
                row.valid_time.unwrap_or("valid time unrecorded"),
                breaking
            );
            if let (Some(before), Some(after)) = (row.before_visibility, row.after_visibility)
                && before != after
            {
                let _ = writeln!(out, "  visibility: {before} -> {after}");
            }
            if let Some(before) = row.before_signature
                && row.change_class != "added"
            {
                let _ = writeln!(out, "  before: {before}");
            }
            if let Some(after) = row.after_signature
                && row.change_class != "removed"
            {
                let _ = writeln!(out, "  after:  {after}");
            }
            if let Some(callers) = &row.internal_callers {
                let _ = writeln!(out, "  internal callers ({}):", callers.len());
                for caller in callers {
                    let _ = writeln!(
                        out,
                        "  - {} ({})",
                        caller.name.unwrap_or("<unresolved>"),
                        caller.record_id
                    );
                }
            }
        }
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "public-api-deltas {}..{} ({} range commits)",
        report.base, report.head, report.range_commit_count
    );
    let _ = writeln!(out, "disclaimer: {}", report.disclaimer);
    write_rows(&mut out, "added", &report.added);
    write_rows(&mut out, "removed", &report.removed);
    write_rows(&mut out, "signature_changed", &report.signature_changed);
    write_rows(&mut out, "visibility_narrowed", &report.visibility_narrowed);
    write_rows(&mut out, "visibility_widened", &report.visibility_widened);
    if let Some(internal) = &report.internal {
        let _ = writeln!(out, "internal [{}]:", internal.label);
        write_rows(&mut out, "internal rows", &internal.rows);
    }
    let counts = &report.counts;
    let _ = writeln!(
        out,
        "counts: added={} removed={} signature_changed={} visibility_narrowed={} \
         visibility_widened={} exported_body_only_modified={} internal_changes={}",
        counts.added,
        counts.removed,
        counts.signature_changed,
        counts.visibility_narrowed,
        counts.visibility_widened,
        counts.exported_body_only_modified,
        counts.internal_changes
    );
    let _ = writeln!(out, "diagnostics ({}):", report.diagnostics.len());
    for diagnostic in &report.diagnostics {
        let _ = writeln!(
            out,
            "- {}{}: {}",
            diagnostic.code,
            diagnostic
                .record_id
                .as_deref()
                .map_or_else(String::new, |id| format!(" [{id}]")),
            diagnostic.detail
        );
    }
    out
}
