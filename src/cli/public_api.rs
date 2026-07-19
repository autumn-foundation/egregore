use super::*;

// ---------------------------------------------------------------------------
// public-api surface query (issue #213)
// ---------------------------------------------------------------------------

/// One externally-reachable item row in the public-api response.
#[derive(Serialize)]
pub(crate) struct PublicApiItemJson<'a> {
    record_id: &'a str,
    kind: &'a str,
    /// Externally visible crate-relative fully-qualified path.
    path: &'a str,
    visibility: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    /// Declaration signature persisted by issue #124, joined when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<&'a str>,
    /// Present (`true`) only on rows contributed by a `pub use` re-export;
    /// such rows are attributed to the re-export site.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    via_reexport: bool,
    /// Crate-relative use-path the re-export points at.
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a str>,
    /// Record ID of the in-graph re-export target, when it resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_record_id: Option<&'a str>,
}

/// Exclusion-tier tallies in the public-api response.
#[derive(Serialize)]
pub(crate) struct PublicApiCountsJson {
    externally_reachable: usize,
    reexports: usize,
    crate_internal: usize,
    private: usize,
    trapped_public: usize,
}

/// Top-level public-api response envelope.
#[derive(Serialize)]
pub(crate) struct PublicApiResponse<'a> {
    ok: bool,
    language: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    /// Per-response disclaimer: parse-derived enumeration, not a build claim.
    disclaimer: &'static str,
    items: Vec<PublicApiItemJson<'a>>,
    counts: PublicApiCountsJson,
    diagnostics: Vec<PublicApiDiagnosticJson<'a>>,
    /// Corpus the current-state view read (issue #427): `head_anchored` over a
    /// scan-history store carrying a `source_snapshot`, `single_snapshot` over
    /// a plain snapshot-less scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) const PUBLIC_API_DISCLAIMER: &str = "Parse-derived enumeration of the externally-reachable public API surface from recorded \
     visibility and module containment. Not a build-verified or semver claim.";

pub(crate) fn query_public_api_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<()> {
    let surface = query::public_api_surface(records, index, repo_scope);
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        disclose_head_anchored_corpus(records, false);

    let response = PublicApiResponse {
        ok: true,
        language: "rust",
        repo_scope,
        disclaimer: PUBLIC_API_DISCLAIMER,
        items: surface
            .items
            .iter()
            .map(|item| PublicApiItemJson {
                record_id: item.record_id,
                kind: &item.kind,
                path: &item.path,
                visibility: "public",
                repo_relative_path: item.repo_relative_path,
                span: item.span,
                signature: item.signature,
                via_reexport: item.via_reexport,
                target: item.target.as_deref(),
                target_record_id: item.target_record_id,
            })
            .collect(),
        counts: PublicApiCountsJson {
            externally_reachable: surface.counts.externally_reachable,
            reexports: surface.counts.reexports,
            crate_internal: surface.counts.crate_internal,
            private: surface.counts.private,
            trapped_public: surface.counts.trapped_public,
        },
        diagnostics: surface
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
        .context("failed to serialize public-api surface")?;
    println!("{output}");
    Ok(())
}
