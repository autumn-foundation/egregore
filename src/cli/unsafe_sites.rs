use super::*;

// ---------------------------------------------------------------------------
// unsafe-code surface inventory (issue #222)
// ---------------------------------------------------------------------------

/// The enclosing-symbol handle carried by an unsafe-site row.
#[derive(Serialize)]
pub(crate) struct UnsafeSiteSymbolJson<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

/// One unsafe-surface site row.
#[derive(Serialize)]
pub(crate) struct UnsafeSiteJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    schema_version: u32,
    /// Closed machine-readable site kind: `block`, `fn`, or `impl`.
    site_kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// Always serialized: an explicit `null` states that no `DEFINES` owner
    /// encloses the site (module-top-level), never silently omitted.
    enclosing_symbol: Option<UnsafeSiteSymbolJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Every row is a deterministic extractor fact, advisory by contract.
    trust: &'static str,
}

/// Machine-readable per-kind totals; `total` always equals the number of
/// returned sites.
#[derive(Serialize)]
pub(crate) struct UnsafeSitesCounts {
    total: usize,
    block: usize,
    r#fn: usize,
    r#impl: usize,
}

/// Top-level unsafe-surface inventory response envelope.
#[derive(Serialize)]
pub(crate) struct UnsafeSitesResponse<'a> {
    ok: bool,
    lane: &'static str,
    /// The closed unsafe-site kind set for this slice.
    kind_set: [&'static str; 3],
    path_prefix: Option<&'a str>,
    at_commit: Option<&'a str>,
    disclaimer: &'static str,
    sites: Vec<UnsafeSiteJson<'a>>,
    counts: UnsafeSitesCounts,
    /// Distinguishes "scope contains zero unsafe sites" from "scope not
    /// found" (which is an error envelope, exit 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_reason: Option<&'static str>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
    page: AuditPage,
    /// Corpus the view read (issue #427): `commit_pinned` under `--at`,
    /// `union` over a scan-history store, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `selector` under `--at`, else `default`.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) const UNSAFE_SITES_DISCLAIMER: &str = "Rows are an advisory unsafe-surface inventory derived solely from \
     deterministic extractor facts. Each row asserts only that an unsafe site \
     of this kind exists at this span — never that the code is sound or \
     unsound. A zero count is not a safety guarantee: macro-expanded, \
     build-script, and dependency unsafe are out of this slice.";

#[allow(clippy::too_many_lines)]
pub(crate) fn query_unsafe_sites_cmd(
    records: &[GraphRecord],
    path_prefix: Option<&str>,
    at: Option<&str>,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at_head: bool,
    all_history: bool,
    format: OutputFormat,
) -> Result<()> {
    // Corpus-mode selection (issue #456): head-anchor by default over a
    // scan-history store; `--all-history` opts into the union and `--at` pins a
    // commit (handled by the pure fn). The HEAD-anchor pre-filter runs BEFORE
    // the pure fn — whose own #468 latest-write-per-id coalescing then applies
    // over the narrowed slice, so the surviving coalesced record is the HEAD one.
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, index, at.is_some(), at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    let inventory = match query::unsafe_sites(records, path_prefix, at, index, repo_scope) {
        Ok(inventory) => inventory,
        Err(err) => {
            let (selector_key, selector_value, message): (&str, &str, String) = match &err {
                query::UnsafeSitesScopeError::MalformedPrefix { prefix } => (
                    "prefix",
                    prefix,
                    "prefix must be non-empty after stripping trailing slashes".to_owned(),
                ),
                query::UnsafeSitesScopeError::ScopeNotFound { prefix } => (
                    "prefix",
                    prefix,
                    format!("no file in the selected store slice lies under `{prefix}`"),
                ),
                query::UnsafeSitesScopeError::UnknownCommit { commit } => (
                    "commit",
                    commit,
                    format!("no record in the selected store slice carries commit `{commit}`"),
                ),
                query::UnsafeSitesScopeError::AmbiguousCommit { commit, count } => (
                    "commit",
                    commit,
                    format!("commit prefix `{commit}` matches {count} commits"),
                ),
            };
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": err.code(),
                    selector_key: selector_value,
                    "message": message,
                }
            });
            println!("{}", serde_json::to_string(&envelope)?);
            let exit_code = match &err {
                query::UnsafeSitesScopeError::MalformedPrefix { .. }
                | query::UnsafeSitesScopeError::AmbiguousCommit { .. } => 1,
                query::UnsafeSitesScopeError::ScopeNotFound { .. }
                | query::UnsafeSitesScopeError::UnknownCommit { .. } => 2,
            };
            std::process::exit(exit_code);
        }
    };

    let rows: Vec<UnsafeSiteJson<'_>> = inventory
        .sites
        .iter()
        .filter_map(|site| {
            let GraphRecord::Node {
                id,
                schema_version,
                repo_relative_path,
                span,
                language,
                temporal,
                valid_time,
                ..
            } = site.record
            else {
                return None;
            };
            let enclosing_symbol = site.enclosing_symbol.and_then(|symbol| {
                let GraphRecord::Node {
                    id: symbol_id,
                    name: symbol_name,
                    symbol_kind,
                    span: symbol_span,
                    ..
                } = symbol
                else {
                    return None;
                };
                Some(UnsafeSiteSymbolJson {
                    record_id: symbol_id,
                    name: symbol_name.as_deref(),
                    symbol_kind: symbol_kind.as_deref(),
                    span: *symbol_span,
                })
            });
            let repository_id = index.owner_of(id);
            Some(UnsafeSiteJson {
                record_id: id,
                kind: "UnsafeSite",
                schema_version: *schema_version,
                site_kind: site.site_kind,
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                language: language.as_deref(),
                valid_time: temporal
                    .as_ref()
                    .map(|t| t.valid_time.as_str())
                    .or(valid_time.as_deref()),
                git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
                enclosing_symbol,
                repository_id,
                repository: repository_id.and_then(|repo| index.display_of(repo)),
                trust: "source_fact",
            })
        })
        .collect();

    let counts = UnsafeSitesCounts {
        total: rows.len(),
        block: rows.iter().filter(|r| r.site_kind == "block").count(),
        r#fn: rows.iter().filter(|r| r.site_kind == "fn").count(),
        r#impl: rows.iter().filter(|r| r.site_kind == "impl").count(),
    };

    if format == OutputFormat::Text {
        for row in &rows {
            let path = row.repo_relative_path.unwrap_or("(unknown)");
            let line = row.span.map_or(0, |s| s.start_line);
            let owner = row
                .enclosing_symbol
                .as_ref()
                .and_then(|s| s.name)
                .unwrap_or("(top-level)");
            println!("unsafe {} @ {path}:{line} in {owner}", row.site_kind);
        }
        if rows.is_empty() {
            println!("# no_sites_in_scope: scope contains zero unsafe sites");
        }
        return Ok(());
    }

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    let response = UnsafeSitesResponse {
        ok: true,
        lane: "unsafe_sites",
        kind_set: ["block", "fn", "impl"],
        path_prefix,
        at_commit: inventory.at_commit.as_deref(),
        disclaimer: UNSAFE_SITES_DISCLAIMER,
        counts,
        empty_reason: if rows.is_empty() {
            Some("no_sites_in_scope")
        } else {
            None
        },
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned: rows.len(),
        },
        sites: rows,
        diagnostics: Vec::new(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    };

    let output = serde_json::to_string_pretty(&response)
        .context("failed to serialize unsafe-surface inventory")?;
    println!("{output}");
    Ok(())
}
