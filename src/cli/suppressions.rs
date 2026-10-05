use super::*;

// ---------------------------------------------------------------------------
// lint-suppression inventory (issue #227)
// ---------------------------------------------------------------------------

/// The enclosing-symbol handle carried by a lint-suppression row.
#[derive(Serialize)]
pub(crate) struct LintSuppressionSymbolJson<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

/// One `#[allow(...)]` / `#![allow(...)]` suppression row.
#[derive(Serialize)]
pub(crate) struct LintSuppressionSiteJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    schema_version: u32,
    /// Sorted, deduplicated lint names silenced by the attribute.
    lints: Vec<&'a str>,
    /// Closed scope class: `item` / `module` / `crate`.
    scope: &'a str,
    /// Adjacent justification-comment signal detected at extraction time.
    has_justification: bool,
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
    /// encloses the suppression (crate scope / top-level), never silently
    /// omitted.
    enclosing_symbol: Option<LintSuppressionSymbolJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Every row is a deterministic extractor fact, advisory by contract.
    trust: &'static str,
}

/// Machine-readable per-scope and per-justification totals.
#[derive(Serialize)]
pub(crate) struct LintSuppressionCounts {
    total: usize,
    item: usize,
    module: usize,
    #[serde(rename = "crate")]
    crate_scope: usize,
    justified: usize,
    unjustified: usize,
}

/// Top-level lint-suppression inventory response envelope.
#[derive(Serialize)]
pub(crate) struct LintSuppressionResponse<'a> {
    ok: bool,
    lane: &'static str,
    path_prefix: Option<&'a str>,
    at_commit: Option<&'a str>,
    disclaimer: &'static str,
    suppressions: Vec<LintSuppressionSiteJson<'a>>,
    counts: LintSuppressionCounts,
    /// Distinguishes "scope contains zero lint suppressions" from
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

pub(crate) const LINT_SUPPRESSION_DISCLAIMER: &str = "Rows are advisory lint-suppression triage leads derived solely from deterministic \
     extractor facts. Each row asserts only that an allow attribute exists at \
     this span silencing these lints — never a verdict on whether the \
     suppression is justified.";

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) fn query_lint_suppressions_cmd(
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

    let inventory = match query::lint_suppressions(records, path_prefix, at, index, repo_scope) {
        Ok(inventory) => inventory,
        Err(err) => {
            let (selector_key, selector_value, message): (&str, &str, String) = match &err {
                query::LintSuppressionScopeError::MalformedPrefix { prefix } => (
                    "prefix",
                    prefix,
                    "prefix must be non-empty after stripping trailing slashes".to_owned(),
                ),
                query::LintSuppressionScopeError::ScopeNotFound { prefix } => (
                    "prefix",
                    prefix,
                    format!("no file in the selected store slice lies under `{prefix}`"),
                ),
                query::LintSuppressionScopeError::UnknownCommit { commit } => (
                    "commit",
                    commit,
                    format!("no record in the selected store slice carries commit `{commit}`"),
                ),
                query::LintSuppressionScopeError::AmbiguousCommit { commit, count } => (
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
                query::LintSuppressionScopeError::MalformedPrefix { .. }
                | query::LintSuppressionScopeError::AmbiguousCommit { .. } => 1,
                query::LintSuppressionScopeError::ScopeNotFound { .. }
                | query::LintSuppressionScopeError::UnknownCommit { .. } => 2,
            };
            std::process::exit(exit_code);
        }
    };

    let rows: Vec<LintSuppressionSiteJson<'_>> = inventory
        .suppressions
        .iter()
        .filter_map(|row| {
            let GraphRecord::Node {
                id,
                schema_version,
                repo_relative_path,
                span,
                language,
                temporal,
                valid_time,
                ..
            } = row.record
            else {
                return None;
            };
            let enclosing_symbol = row.enclosing_symbol.and_then(|symbol| {
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
                Some(LintSuppressionSymbolJson {
                    record_id: symbol_id,
                    name: symbol_name.as_deref(),
                    symbol_kind: symbol_kind.as_deref(),
                    span: *symbol_span,
                })
            });
            let repository_id = index.owner_of(id);
            Some(LintSuppressionSiteJson {
                record_id: id,
                kind: "LintSuppression",
                schema_version: *schema_version,
                lints: row.lints.clone(),
                scope: row.scope,
                has_justification: row.has_justification,
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

    let counts = LintSuppressionCounts {
        total: rows.len(),
        item: rows.iter().filter(|r| r.scope == "item").count(),
        module: rows.iter().filter(|r| r.scope == "module").count(),
        crate_scope: rows.iter().filter(|r| r.scope == "crate").count(),
        justified: rows.iter().filter(|r| r.has_justification).count(),
        unjustified: rows.iter().filter(|r| !r.has_justification).count(),
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
            let lints = row.lints.join(", ");
            let justified = if row.has_justification {
                "justified"
            } else {
                "unjustified"
            };
            println!(
                "{} [{}] ({}) @ {path}:{line} in {owner}",
                row.scope, lints, justified
            );
        }
        if rows.is_empty() {
            println!("# no_suppressions_in_scope: scope contains zero lint suppressions");
        }
        return Ok(());
    }

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    let response = LintSuppressionResponse {
        ok: true,
        lane: "suppressions",
        path_prefix,
        at_commit: inventory.at_commit.as_deref(),
        disclaimer: LINT_SUPPRESSION_DISCLAIMER,
        counts,
        empty_reason: if rows.is_empty() {
            Some("no_suppressions_in_scope")
        } else {
            None
        },
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned: rows.len(),
        },
        suppressions: rows,
        diagnostics: Vec::new(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    };

    let output = serde_json::to_string_pretty(&response)
        .context("failed to serialize lint-suppression inventory")?;
    println!("{output}");
    Ok(())
}
