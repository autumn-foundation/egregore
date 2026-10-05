use super::*;

// ---------------------------------------------------------------------------
// dead-code triage candidates (issue #240)
// ---------------------------------------------------------------------------

/// One dead-code candidate row in the JSON response.
#[derive(Serialize)]
pub(crate) struct DeadCodeCandidateJson<'a> {
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
    /// Row verdict — always `candidate`, never a dead-code proof.
    classification: &'static str,
    /// The concrete evidence asserted for this row.
    evidence: DeadCodeEvidenceJson,
}

/// The concrete evidence asserted for one candidate row (issue #240 AC2).
#[derive(Serialize)]
pub(crate) struct DeadCodeEvidenceJson {
    /// Recorded direct incoming `CALLS` edges (`resolved`/`ambiguous`).
    incoming_calls: usize,
    /// `true` when `incoming_calls > 0` and every recorded direct caller is
    /// itself a reported candidate (the one-hop dead-cluster rule).
    callers_all_candidates: bool,
    /// Always `false`: the asserted exclusion from the issue #213 public
    /// surface (top-level `pub`, visibility-widening `pub use` re-exports).
    on_public_surface: bool,
}

/// Deterministic tallies in the dead-code response (totals, unaffected by
/// `--limit`).
#[derive(Serialize)]
pub(crate) struct DeadCodeCountsJson {
    symbols_considered: usize,
    referenced: usize,
    candidates: usize,
    candidates_returned: usize,
    entry_points_excluded: usize,
    public_surface_excluded: usize,
}

/// One stable machine-readable diagnostic in the dead-code response.
#[derive(Serialize)]
pub(crate) struct DeadCodeDiagnosticJson<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    detail: &'a str,
}

/// Top-level dead-code response envelope.
#[derive(Serialize)]
pub(crate) struct DeadCodeResponse<'a> {
    ok: bool,
    lane: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    /// Row verdict for every row — always `candidate`, never proven-dead.
    classification: &'static str,
    /// Per-response soundness boundary: suspected dead code, not proof.
    soundness: &'static str,
    candidates: Vec<DeadCodeCandidateJson<'a>>,
    counts: DeadCodeCountsJson,
    diagnostics: Vec<DeadCodeDiagnosticJson<'a>>,
    /// Corpus the current-state view read (issue #427): `head_anchored` over a
    /// scan-history store carrying a `source_snapshot`, `single_snapshot` over
    /// a plain snapshot-less scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) fn query_dead_code_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    let result = query::dead_code_candidates(records, index, repo_scope, limit);
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        disclose_head_anchored_corpus(records, false);

    match format {
        OutputFormat::Json => print_dead_code_json(
            &result,
            repo_scope,
            corpus_mode,
            corpus_mode_source,
            corpus_disclaimer,
        ),
        OutputFormat::Text => {
            print_dead_code_text(&result, repo_scope, limit);
            Ok(())
        }
    }
}

/// Emits the dead-code triage set as one compact JSON document on a single
/// line, like the sibling query lanes — byte-identical across runs on an
/// unchanged store.
fn print_dead_code_json(
    result: &query::DeadCodeResult<'_>,
    repo_scope: Option<&str>,
    corpus_mode: &'static str,
    corpus_mode_source: &'static str,
    corpus_disclaimer: String,
) -> Result<()> {
    let response = DeadCodeResponse {
        ok: true,
        lane: "dead-code",
        repo_scope,
        classification: "candidate",
        soundness: query::DEAD_CODE_SOUNDNESS,
        candidates: result
            .candidates
            .iter()
            .map(|candidate| DeadCodeCandidateJson {
                record_id: candidate.record_id,
                schema_version: candidate.schema_version,
                name: candidate.name,
                kind: candidate.kind,
                repo_relative_path: candidate.repo_relative_path,
                span: candidate.span,
                git_commit: candidate.git_commit,
                classification: candidate.classification,
                evidence: DeadCodeEvidenceJson {
                    incoming_calls: candidate.incoming_calls,
                    callers_all_candidates: candidate.callers_all_candidates,
                    on_public_surface: candidate.on_public_surface,
                },
            })
            .collect(),
        counts: DeadCodeCountsJson {
            symbols_considered: result.counts.symbols_considered,
            referenced: result.counts.referenced,
            candidates: result.counts.candidates,
            candidates_returned: result.counts.candidates_returned,
            entry_points_excluded: result.counts.entry_points_excluded,
            public_surface_excluded: result.counts.public_surface_excluded,
        },
        diagnostics: result
            .diagnostics
            .iter()
            .map(|d| DeadCodeDiagnosticJson {
                code: d.code,
                record_id: d.record_id.as_deref(),
                detail: &d.detail,
            })
            .collect(),
        corpus_mode,
        corpus_mode_source,
        corpus_disclaimer,
    };
    let output =
        serde_json::to_string(&response).context("failed to serialize dead-code candidates")?;
    println!("{output}");
    Ok(())
}

/// Renders the triage set for `--format text`: one citable line per
/// candidate plus the honesty header.
fn print_dead_code_text(
    result: &query::DeadCodeResult<'_>,
    repo_scope: Option<&str>,
    limit: usize,
) {
    let scope = repo_scope.map_or_else(|| "all repositories".to_owned(), str::to_owned);
    println!(
        "dead-code candidates (classification: candidate — suspected, not proven) \
         [{scope}, limit {limit}]"
    );
    println!(
        "considered={} referenced={} candidates={} returned={} entry_points_excluded={} \
         public_surface_excluded={}",
        result.counts.symbols_considered,
        result.counts.referenced,
        result.counts.candidates,
        result.counts.candidates_returned,
        result.counts.entry_points_excluded,
        result.counts.public_surface_excluded,
    );
    for candidate in &result.candidates {
        let citation = match (candidate.repo_relative_path, candidate.span) {
            (Some(path), Some(span)) => {
                format!(" @ {path}:{}-{}", span.start_line, span.end_line)
            }
            (Some(path), None) => format!(" @ {path}"),
            (None, _) => String::new(),
        };
        let via_cluster = if candidate.callers_all_candidates {
            " callers_all_candidates"
        } else {
            ""
        };
        println!(
            "- {} [{}] incoming_calls={}{via_cluster}{citation} ({})",
            candidate.name, candidate.kind, candidate.incoming_calls, candidate.record_id,
        );
    }
    for diagnostic in &result.diagnostics {
        println!("diagnostic {}: {}", diagnostic.code, diagnostic.detail);
    }
}
