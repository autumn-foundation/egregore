use super::*;

// ---------------------------------------------------------------------------
// blind-spot triage lane (issue #265)
// ---------------------------------------------------------------------------

/// `--kind` population filter for `eg query blind-spots`.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum BlindSpotKindArg {
    /// Evaluate both symbols and files.
    #[default]
    Both,
    /// Evaluate only `Symbol` records.
    Symbol,
    /// Evaluate only `File` records.
    File,
}

impl BlindSpotKindArg {
    /// Maps the CLI flag onto the query-layer population selector.
    pub(crate) const fn as_query(self) -> query::BlindSpotKind {
        match self {
            Self::Both => query::BlindSpotKind::Both,
            Self::Symbol => query::BlindSpotKind::Symbol,
            Self::File => query::BlindSpotKind::File,
        }
    }
}

/// One zero-evidence target row in the blind-spots response.
#[derive(Serialize)]
pub(crate) struct BlindSpotRowJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    target_kind: &'static str,
    name: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    /// Introducing commit for temporal (history-backed) records.
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// Inbound structural reference count — the prominence ranking key.
    inbound_reference_count: usize,
}

/// Deterministic tallies in the blind-spots response.
#[derive(Serialize)]
pub(crate) struct BlindSpotCountsJson {
    targets_evaluated: usize,
    targets_with_evidence: usize,
    blind_spots: usize,
}

/// One stable machine-readable diagnostic in the blind-spots response.
#[derive(Serialize)]
pub(crate) struct BlindSpotDiagnosticJson<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    detail: &'a str,
}

/// Top-level blind-spots response envelope.
#[derive(Serialize)]
pub(crate) struct BlindSpotsResponse<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    /// Per-response disclaimer: leads for triage, never proof of anything.
    disclaimer: &'static str,
    /// Edge classes counted as cross-domain evidence, sorted.
    evidence_edge_classes: &'static [&'static str],
    blind_spots: Vec<BlindSpotRowJson<'a>>,
    counts: BlindSpotCountsJson,
    diagnostics: Vec<BlindSpotDiagnosticJson<'a>>,
    /// Corpus the current-state view read (issue #427): `head_anchored` over a
    /// scan-history store carrying a `source_snapshot`, `single_snapshot` over
    /// a plain snapshot-less scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) const BLIND_SPOTS_DISCLAIMER: &str = "Code targets with zero recorded cross-domain evidence edges into the agent-memory, \
     verification, or project domains. Rows are triage leads, not verdicts: no recorded \
     evidence is not evidence the code is unimportant, untested, or unsafe — and absence of \
     evidence is never evidence of correctness.";

const fn blind_spot_target_kind_name(kind: query::BlindSpotKind) -> &'static str {
    match kind {
        query::BlindSpotKind::Symbol => "symbol",
        query::BlindSpotKind::File => "file",
        query::BlindSpotKind::Both => "both",
    }
}

pub(crate) fn query_blind_spots_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    kind: query::BlindSpotKind,
    format: OutputFormat,
) -> Result<()> {
    let result = query::blind_spots(records, index, repo_scope, kind);
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        disclose_head_anchored_corpus(records, false);

    let rows: Vec<BlindSpotRowJson<'_>> = result
        .blind_spots
        .iter()
        .map(|spot| BlindSpotRowJson {
            record_id: spot.record_id,
            schema_version: spot.schema_version,
            target_kind: blind_spot_target_kind_name(spot.target_kind),
            name: spot.name,
            kind: spot.kind,
            repo_relative_path: spot.repo_relative_path,
            span: spot.span,
            git_commit: spot.git_commit,
            inbound_reference_count: spot.inbound_reference_count,
        })
        .collect();

    let output = match format {
        OutputFormat::Json => {
            let response = BlindSpotsResponse {
                ok: true,
                repo_scope,
                disclaimer: BLIND_SPOTS_DISCLAIMER,
                evidence_edge_classes: query::BLIND_SPOT_EVIDENCE_CLASS_NAMES,
                blind_spots: rows,
                counts: BlindSpotCountsJson {
                    targets_evaluated: result.counts.targets_evaluated,
                    targets_with_evidence: result.counts.targets_with_evidence,
                    blind_spots: result.counts.blind_spots,
                },
                diagnostics: result
                    .diagnostics
                    .iter()
                    .map(|d| BlindSpotDiagnosticJson {
                        code: d.code,
                        record_id: d.record_id.as_deref(),
                        detail: &d.detail,
                    })
                    .collect(),
                corpus_mode,
                corpus_mode_source,
                corpus_disclaimer,
            };
            serde_json::to_string_pretty(&response)
                .context("failed to serialize blind-spot rows")?
        }
        OutputFormat::Text => render_blind_spots_text(&result, repo_scope),
    };
    println!("{output}");
    Ok(())
}

/// Renders the blind-spot result as a deterministic human-readable form
/// (issue #265 AC5): one line per blind spot, riskiest first.
fn render_blind_spots_text(result: &query::BlindSpots<'_>, repo_scope: Option<&str>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let scope = repo_scope.map_or_else(String::new, |s| format!(" (repo scope: {s})"));
    lines.push(format!(
        "Blind spots: {} of {} evaluated targets carry zero cross-domain evidence{scope}. \
         Prominence = inbound structural references, riskiest first.",
        result.counts.blind_spots, result.counts.targets_evaluated
    ));
    for spot in &result.blind_spots {
        let at = if matches!(spot.target_kind, query::BlindSpotKind::File) {
            String::new()
        } else {
            let path = spot.repo_relative_path.unwrap_or("<no path>");
            let location = spot
                .span
                .map_or_else(|| path.to_owned(), |s| format!("{path}:{}", s.start_line));
            format!(" @ {location}")
        };
        lines.push(format!(
            "- {} [{}]{} refs={} ({})",
            spot.name, spot.kind, at, spot.inbound_reference_count, spot.record_id
        ));
    }
    for diagnostic in &result.diagnostics {
        lines.push(format!(
            "diagnostic {}: {}",
            diagnostic.code, diagnostic.detail
        ));
    }
    lines.join("\n")
}
