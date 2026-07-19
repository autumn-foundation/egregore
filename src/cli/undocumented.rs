use super::*;

// ---------------------------------------------------------------------------
// undocumented public API lane (issue #257)
// ---------------------------------------------------------------------------

/// One undocumented-symbol row in the undocumented response.
#[derive(Serialize)]
pub(crate) struct UndocumentedItemJson<'a> {
    record_id: &'a str,
    kind: &'a str,
    /// Crate-relative fully-qualified path (alias-aware for re-exports).
    path: &'a str,
    /// Recorded visibility class; `public` on externally-reachable rows.
    visibility: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    /// Declaration signature persisted by issue #124, joined when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<&'a str>,
    /// Concrete evidence asserted for this row: `doc_comment_absent` always,
    /// plus `externally_reachable` on public-surface rows.
    evidence: &'a [&'static str],
    /// Present (`true`) only on rows contributed by a `pub use` re-export;
    /// such rows are attributed to the re-export site.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    via_reexport: bool,
    /// Crate-relative use-path the re-export points at.
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a str>,
    /// Record ID of the resolved re-export target whose doc fact was checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_record_id: Option<&'a str>,
}

/// Deterministic tallies in the undocumented response.
#[derive(Serialize)]
pub(crate) struct UndocumentedCountsJson {
    considered: usize,
    documented: usize,
    undocumented: usize,
    reexports: usize,
    modules_excluded: usize,
    reexports_unresolved: usize,
    doc_capture_missing: usize,
}

/// Top-level undocumented response envelope.
#[derive(Serialize)]
pub(crate) struct UndocumentedResponse<'a> {
    ok: bool,
    language: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    /// Present (`true`) only when the audit was widened to all symbols.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    include_private: bool,
    /// `doc_facts_recorded` when the store carries issue #124 doc-capture
    /// facts; `doc_facts_unavailable` when it predates them (explicit
    /// capability-absent verdict — never "everything is undocumented").
    capability: &'static str,
    /// Per-response soundness boundary: presence/absence only, never quality.
    disclaimer: &'static str,
    items: Vec<UndocumentedItemJson<'a>>,
    counts: UndocumentedCountsJson,
    diagnostics: Vec<PublicApiDiagnosticJson<'a>>,
    /// Corpus the current-state view read (issue #427): `head_anchored` over a
    /// scan-history store carrying a `source_snapshot`, `single_snapshot` over
    /// a plain snapshot-less scan. Inherited from the public-API surface.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) const UNDOCUMENTED_DISCLAIMER: &str = "Asserts the presence or absence of a recorded doc comment (///, /** */, or #[doc = \
     \"...\"]) on externally-reachable public symbols. Never a claim about doc quality, \
     accuracy, or completeness.";

#[allow(clippy::too_many_lines)]
pub(crate) fn query_undocumented_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    limit: Option<usize>,
    include_private: bool,
    format: OutputFormat,
) -> Result<()> {
    let report = query::undocumented_public_api(records, index, repo_scope, include_private, limit);
    let capability = if report.capability_absent {
        "doc_facts_unavailable"
    } else {
        "doc_facts_recorded"
    };
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        disclose_head_anchored_corpus(records, false);

    match format {
        OutputFormat::Json => {
            let response = UndocumentedResponse {
                ok: true,
                language: "rust",
                repo_scope,
                include_private,
                capability,
                disclaimer: UNDOCUMENTED_DISCLAIMER,
                items: report
                    .items
                    .iter()
                    .map(|item| UndocumentedItemJson {
                        record_id: item.record_id,
                        kind: &item.kind,
                        path: &item.path,
                        visibility: item.visibility,
                        repo_relative_path: item.repo_relative_path,
                        span: item.span,
                        signature: item.signature,
                        evidence: &item.evidence,
                        via_reexport: item.via_reexport,
                        target: item.target.as_deref(),
                        target_record_id: item.target_record_id,
                    })
                    .collect(),
                counts: UndocumentedCountsJson {
                    considered: report.counts.considered,
                    documented: report.counts.documented,
                    undocumented: report.counts.undocumented,
                    reexports: report.counts.reexports,
                    modules_excluded: report.counts.modules_excluded,
                    reexports_unresolved: report.counts.reexports_unresolved,
                    doc_capture_missing: report.counts.doc_capture_missing,
                },
                diagnostics: report
                    .diagnostics
                    .iter()
                    .map(|d| PublicApiDiagnosticJson {
                        code: d.code,
                        record_id: d.record_id.as_deref(),
                        detail: &d.detail,
                    })
                    .collect(),
                corpus_mode,
                corpus_mode_source,
                corpus_disclaimer,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize undocumented report")?;
            println!("{output}");
        }
        OutputFormat::Text => {
            println!(
                "Undocumented public API symbols (doc-comment presence only — never doc quality). \
                 Capability: {capability}."
            );
            for item in &report.items {
                let citation = match (item.repo_relative_path, item.span) {
                    (Some(path), Some(span)) => {
                        format!(" @ {path}:{}-{}", span.start_line, span.end_line)
                    }
                    (Some(path), None) => format!(" @ {path}"),
                    (None, _) => String::new(),
                };
                let reexport = item
                    .target
                    .as_deref()
                    .map_or_else(String::new, |target| format!(" via pub use {target}"));
                println!(
                    "- {} [{}] {}{}{} evidence={} ({})",
                    item.path,
                    item.kind,
                    item.visibility,
                    reexport,
                    citation,
                    item.evidence.join(","),
                    item.record_id
                );
            }
            println!(
                "counts: considered={} documented={} undocumented={} reexports={} \
                 modules_excluded={} reexports_unresolved={} doc_capture_missing={}",
                report.counts.considered,
                report.counts.documented,
                report.counts.undocumented,
                report.counts.reexports,
                report.counts.modules_excluded,
                report.counts.reexports_unresolved,
                report.counts.doc_capture_missing
            );
            for d in &report.diagnostics {
                println!("diagnostic: {}: {}", d.code, d.detail);
            }
            println!("corpus: {corpus_mode}");
        }
    }
    Ok(())
}
