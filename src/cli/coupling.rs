use super::*;

/// `eg query coupling` (issue #153): ranked historical co-change partners
/// for one target file over a `scan-history` temporal store.
///
/// Exit codes follow the history-query convention: `0` on success (including
/// an explicit empty partner set), `2` when the target or a commit handle
/// resolves to nothing (`unknown_file`, `missing_commit`, `empty_history`,
/// `no_commit_at_or_before`), and `1` for malformed or ambiguous input
/// (paths, selectors, thresholds, identical endpoints, reversed ranges).
pub(crate) fn query_coupling_cmd(
    records: &[GraphRecord],
    path: &str,
    repo_scope: Option<&str>,
    options: &query::CoChangeCouplingOptions<'_>,
    format: OutputFormat,
) -> Result<()> {
    match query::co_change_coupling(records, path, repo_scope, options) {
        Ok(report) => {
            match format {
                OutputFormat::Json => {
                    #[derive(Debug, Clone, serde::Serialize)]
                    struct CouplingResponse<'a> {
                        ok: bool,
                        #[serde(flatten)]
                        report: query::CoChangeCoupling<'a>,
                    }
                    let response = CouplingResponse { ok: true, report };
                    let output = serde_json::to_string(&response)
                        .context("failed to serialize co-change coupling")?;
                    println!("{output}");
                }
                OutputFormat::Text => print!("{}", render_coupling_text(&report)),
            }
            Ok(())
        }
        Err(err) => {
            #[derive(Debug, Clone, serde::Serialize)]
            struct CouplingErrorResponse {
                ok: bool,
                error: query::CoChangeCouplingError,
            }
            let response = CouplingErrorResponse {
                ok: false,
                error: err.clone(),
            };
            let output = serde_json::to_string(&response)
                .context("failed to serialize co-change coupling error")?;
            println!("{output}");
            let exit_code = match err {
                query::CoChangeCouplingError::UnknownFile { .. }
                | query::CoChangeCouplingError::MissingCommit { .. }
                | query::CoChangeCouplingError::EmptyHistory
                | query::CoChangeCouplingError::NoCommitAtOrBefore { .. } => 2,
                _ => 1,
            };
            std::process::exit(exit_code);
        }
    }
}

/// Deterministic human-readable rendering of a co-change coupling report.
pub(crate) fn render_coupling_text(report: &query::CoChangeCoupling<'_>) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let scope_detail = match report.scope.selector {
        "commit_range" => format!(
            " range {}..{}",
            report.scope.base.unwrap_or("?"),
            report.scope.head.unwrap_or("?")
        ),
        "at_commit" => format!(" at {}", report.scope.at.unwrap_or("?")),
        "as_of" => format!(" as of {}", report.scope.as_of.unwrap_or("?")),
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "co-change coupling for {} — {} in-scope change(s) across {} commit(s) [{}{}]",
        report.target.repo_relative_path,
        report.target.change_count,
        report.scope.commit_count,
        report.scope.selector,
        scope_detail,
    );
    let _ = writeln!(
        out,
        "min support {}; metric {}; showing {} of {} partner(s){}",
        report.min_support,
        report.coupling_metric,
        report.partners.len(),
        report.total_partners,
        if report.truncated { " (truncated)" } else { "" },
    );
    for partner in &report.partners {
        let short_sha =
            &partner.last_co_change_commit[..partner.last_co_change_commit.len().min(12)];
        let _ = writeln!(
            out,
            "  {}  co_changes={}  partner_changes={}  coupling={:.4}  confidence={:.4}  last={}",
            partner.repo_relative_path,
            partner.co_change_count,
            partner.partner_change_count,
            partner.coupling,
            partner.confidence,
            short_sha,
        );
    }
    for diagnostic in &report.diagnostics {
        let _ = writeln!(out, "  [{}] {}", diagnostic.code, diagnostic.detail);
    }
    let _ = writeln!(out, "note: {}", report.disclaimer);
    out
}
