use super::*;

// ---------------------------------------------------------------------------
// unreferenced-symbol prune candidates (issue #113)
// ---------------------------------------------------------------------------

/// Extraction-completeness caveat on one unreferenced candidate row.
#[derive(Serialize)]
pub(crate) struct UnreferencedCaveatJson<'a> {
    code: &'a str,
    diagnostic_count: usize,
    diagnostic_record_ids: &'a [String],
    detail: &'a str,
}

/// One zero-inbound-reference candidate row in the unreferenced response.
#[derive(Serialize)]
pub(crate) struct UnreferencedCandidateJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    /// Introducing commit for temporal (history-backed) records.
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// The inbound-reference count that selected the row — always 0.
    inbound_reference_count: usize,
    /// Present only when the candidate's file scope contains extractor
    /// `Diagnostic` markers (issue #87): confidence is lower there.
    #[serde(skip_serializing_if = "Option::is_none")]
    extraction_caveat: Option<UnreferencedCaveatJson<'a>>,
}

/// Deterministic tallies in the unreferenced response.
#[derive(Serialize)]
pub(crate) struct UnreferencedCountsJson {
    symbols_considered: usize,
    referenced: usize,
    candidates: usize,
    files_with_diagnostic_markers: usize,
}

/// One stable machine-readable diagnostic in the unreferenced response.
#[derive(Serialize)]
pub(crate) struct UnreferencedDiagnosticJson<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    detail: &'a str,
}

/// Top-level unreferenced response envelope.
#[derive(Serialize)]
pub(crate) struct UnreferencedResponse<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    /// Per-response disclaimer: leads for pruning triage, never proof.
    disclaimer: &'static str,
    /// Edge classes counted as references, sorted.
    reference_edge_classes: &'static [&'static str],
    candidates: Vec<UnreferencedCandidateJson<'a>>,
    counts: UnreferencedCountsJson,
    diagnostics: Vec<UnreferencedDiagnosticJson<'a>>,
    /// Corpus the current-state view read (issue #427): `head_anchored` over a
    /// scan-history store carrying a `source_snapshot`, `single_snapshot` over
    /// a plain snapshot-less scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) const UNREFERENCED_DISCLAIMER: &str = "Symbols with zero recorded inbound reference edges in this graph. Candidates are leads \
     for pruning triage, not proof of dead code: public API consumed outside this repository, \
     trait-dispatched methods, macro-generated call sites, FFI/#[no_mangle] exports, \
     derive-generated use, and crate entry points (main, #[test]) can all be used without a \
     recorded in-graph reference.";

pub(crate) fn query_unreferenced_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<()> {
    let result = query::unreferenced_symbols(records, index, repo_scope);
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        disclose_head_anchored_corpus(records, false);

    let response = UnreferencedResponse {
        ok: true,
        repo_scope,
        disclaimer: UNREFERENCED_DISCLAIMER,
        reference_edge_classes: query::UNREFERENCED_REFERENCE_CLASS_NAMES,
        candidates: result
            .candidates
            .iter()
            .map(|candidate| UnreferencedCandidateJson {
                record_id: candidate.record_id,
                schema_version: candidate.schema_version,
                name: candidate.name,
                kind: candidate.kind,
                repo_relative_path: candidate.repo_relative_path,
                span: candidate.span,
                git_commit: candidate.git_commit,
                inbound_reference_count: candidate.inbound_reference_count,
                extraction_caveat: candidate.extraction_caveat.as_ref().map(|caveat| {
                    UnreferencedCaveatJson {
                        code: caveat.code,
                        diagnostic_count: caveat.diagnostic_count,
                        diagnostic_record_ids: &caveat.diagnostic_record_ids,
                        detail: &caveat.detail,
                    }
                }),
            })
            .collect(),
        counts: UnreferencedCountsJson {
            symbols_considered: result.counts.symbols_considered,
            referenced: result.counts.referenced,
            candidates: result.counts.candidates,
            files_with_diagnostic_markers: result.counts.files_with_diagnostic_markers,
        },
        diagnostics: result
            .diagnostics
            .iter()
            .map(|d| UnreferencedDiagnosticJson {
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
        .context("failed to serialize unreferenced-symbol candidates")?;
    println!("{output}");
    Ok(())
}
