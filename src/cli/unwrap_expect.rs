use super::*;

// ---------------------------------------------------------------------------
// unwrap/expect panic-risk call-site inventory (issue #223)
// ---------------------------------------------------------------------------

/// The enclosing-symbol handle carried by an unwrap/expect site row.
#[derive(Serialize)]
pub(crate) struct UnwrapExpectSymbolJson<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

/// One unwrap/expect panic-risk call-site row.
#[derive(Serialize)]
pub(crate) struct UnwrapExpectSiteJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    schema_version: u32,
    /// Closed machine-readable category: `unwrap` or `expect`.
    category: &'a str,
    /// Closed context class: `production` or `test`.
    context: &'a str,
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
    /// encloses the site (top-level), never silently omitted.
    enclosing_symbol: Option<UnwrapExpectSymbolJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Every row is a deterministic extractor fact, advisory by contract.
    trust: &'static str,
}

/// Machine-readable per-category and per-context totals.
#[derive(Serialize)]
pub(crate) struct UnwrapExpectCounts {
    total: usize,
    unwrap: usize,
    expect: usize,
    production: usize,
    test: usize,
}

/// Top-level unwrap/expect inventory response envelope.
#[derive(Serialize)]
pub(crate) struct UnwrapExpectResponse<'a> {
    ok: bool,
    lane: &'static str,
    /// The closed known-risk method set for this slice.
    method_set: [&'static str; 2],
    path_prefix: Option<&'a str>,
    at_commit: Option<&'a str>,
    disclaimer: &'static str,
    sites: Vec<UnwrapExpectSiteJson<'a>>,
    counts: UnwrapExpectCounts,
    /// Distinguishes "scope contains zero unwrap/expect sites" from
    /// "scope not found" (which is an error envelope, exit 2).
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

pub(crate) const UNWRAP_EXPECT_DISCLAIMER: &str = "Rows are advisory panic-risk triage leads derived solely from deterministic \
     extractor facts. Each row asserts only that an unwrap/expect call exists at \
     this span in this context — never a verdict on whether it is justified.";

#[allow(clippy::too_many_lines)]
pub(crate) fn query_unwrap_expect_cmd(
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

    let inventory = match query::unwrap_expect_sites(records, path_prefix, at, index, repo_scope) {
        Ok(inventory) => inventory,
        Err(err) => {
            let (selector_key, selector_value, message): (&str, &str, String) = match &err {
                query::UnwrapExpectScopeError::MalformedPrefix { prefix } => (
                    "prefix",
                    prefix,
                    "prefix must be non-empty after stripping trailing slashes".to_owned(),
                ),
                query::UnwrapExpectScopeError::ScopeNotFound { prefix } => (
                    "prefix",
                    prefix,
                    format!("no file in the selected store slice lies under `{prefix}`"),
                ),
                query::UnwrapExpectScopeError::UnknownCommit { commit } => (
                    "commit",
                    commit,
                    format!("no record in the selected store slice carries commit `{commit}`"),
                ),
                query::UnwrapExpectScopeError::AmbiguousCommit { commit, count } => (
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
                query::UnwrapExpectScopeError::MalformedPrefix { .. }
                | query::UnwrapExpectScopeError::AmbiguousCommit { .. } => 1,
                query::UnwrapExpectScopeError::ScopeNotFound { .. }
                | query::UnwrapExpectScopeError::UnknownCommit { .. } => 2,
            };
            std::process::exit(exit_code);
        }
    };

    let rows: Vec<UnwrapExpectSiteJson<'_>> = inventory
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
                Some(UnwrapExpectSymbolJson {
                    record_id: symbol_id,
                    name: symbol_name.as_deref(),
                    symbol_kind: symbol_kind.as_deref(),
                    span: *symbol_span,
                })
            });
            let repository_id = index.owner_of(id);
            Some(UnwrapExpectSiteJson {
                record_id: id,
                kind: "PanicRiskSite",
                schema_version: *schema_version,
                category: site.category,
                context: site.context,
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

    let counts = UnwrapExpectCounts {
        total: rows.len(),
        unwrap: rows.iter().filter(|r| r.category == "unwrap").count(),
        expect: rows.iter().filter(|r| r.category == "expect").count(),
        production: rows.iter().filter(|r| r.context == "production").count(),
        test: rows.iter().filter(|r| r.context == "test").count(),
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
            println!(
                "{} ({}) @ {path}:{line} in {owner}",
                row.category, row.context
            );
        }
        if rows.is_empty() {
            println!("# no_sites_in_scope: scope contains zero unwrap/expect sites");
        }
        return Ok(());
    }

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    let response = UnwrapExpectResponse {
        ok: true,
        lane: "unwrap_expect",
        method_set: ["expect", "unwrap"],
        path_prefix,
        at_commit: inventory.at_commit.as_deref(),
        disclaimer: UNWRAP_EXPECT_DISCLAIMER,
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
        .context("failed to serialize unwrap/expect inventory")?;
    println!("{output}");
    Ok(())
}
