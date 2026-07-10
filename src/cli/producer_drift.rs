use super::*;

// ---------------------------------------------------------------------------
// producer-drift query (issue #234)
// ---------------------------------------------------------------------------

pub(crate) const PRODUCER_DRIFT_DISCLAIMER: &str = "Read-only audit of stored producer identity against the running binary. Drift means \
     re-extraction with this binary could emit different spans or edges for the same source; \
     it is never proof the recorded facts are wrong, and nothing is re-extracted or mutated.";

/// `eg query producer-drift` (issue #234): report code-graph records whose
/// producer identity differs from the running binary, in deterministic
/// JSON or text form.
pub(crate) fn query_producer_drift_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    store_record_parents: &BTreeMap<String, String>,
    format: OutputFormat,
) -> Result<()> {
    let current = query::CurrentProducerIdentity::of_running_binary();
    let report = query::producer_drift(records, index, repo_scope, store_record_parents, &current);

    match format {
        OutputFormat::Json => {
            #[derive(Serialize)]
            struct ProducerDriftResponse<'a> {
                ok: bool,
                #[serde(skip_serializing_if = "Option::is_none")]
                repo_scope: Option<&'a str>,
                disclaimer: &'static str,
                #[serde(flatten)]
                report: query::ProducerDriftReport<'a>,
            }
            let response = ProducerDriftResponse {
                ok: true,
                repo_scope,
                disclaimer: PRODUCER_DRIFT_DISCLAIMER,
                report,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize producer-drift report")?;
            println!("{output}");
        }
        OutputFormat::Text => print!("{}", render_producer_drift_text(&report, repo_scope)),
    }
    Ok(())
}

/// Deterministic human-readable rendering of a producer-drift report.
pub(crate) fn render_producer_drift_text(
    report: &query::ProducerDriftReport<'_>,
    repo_scope: Option<&str>,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let counts = &report.counts;
    let _ = writeln!(
        out,
        "producer-drift: {} drifted, {} current, {} non-code, {} legacy_pre_v1 ({} records)",
        counts.drifted,
        counts.current,
        counts.non_code_producer,
        counts.legacy_pre_v1,
        counts.total
    );
    if let Some(scope) = repo_scope {
        let _ = writeln!(out, "repo scope: {scope}");
    }
    let _ = writeln!(
        out,
        "current egregore_version: {}",
        report.current_producer.egregore_version
    );
    for (key, version) in &report.current_producer.producer_components {
        let _ = writeln!(out, "current component {key}: {version}");
    }
    for group in &report.groups {
        let _ = write!(
            out,
            "group {} {}",
            group.bucket.as_str(),
            group.producer_kind
        );
        if let Some(version) = group.egregore_version {
            let _ = write!(out, " egregore_version {version}");
        }
        let _ = writeln!(out, " ({} records)", group.record_count);
        for mismatch in group.mismatches.iter().flatten() {
            let _ = writeln!(
                out,
                "  mismatch {}: recorded {}, current {}",
                mismatch.field,
                mismatch.recorded,
                mismatch
                    .current
                    .as_deref()
                    .unwrap_or("<component unknown to this binary>")
            );
        }
        for record in group.records.iter().flatten() {
            let _ = write!(out, "  record {} {}", record.record_id, record.record_type);
            if let Some(path) = record.repo_relative_path {
                let _ = write!(out, " {path}");
                if let Some(span) = record.span {
                    let _ = write!(out, ":{}-{}", span.start_line, span.end_line);
                }
            }
            let _ = writeln!(out);
        }
    }
    for diagnostic in &report.diagnostics {
        let _ = writeln!(out, "diagnostic {}: {}", diagnostic.code, diagnostic.detail);
    }
    out
}
