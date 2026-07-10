use super::*;

// ---------------------------------------------------------------------------
// Debt-comment marker inventory (issue #218)
// ---------------------------------------------------------------------------

/// The enclosing-symbol handle carried by a debt-marker row.
#[derive(Serialize)]
pub(crate) struct DebtMarkerSymbolJson<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

/// One debt-comment marker row.
#[derive(Serialize)]
pub(crate) struct DebtMarkerJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    schema_version: u32,
    /// Closed machine-readable category: `todo` / `fixme` / `hack` / `xxx`.
    category: &'a str,
    /// Trimmed single-line note text following the marker token.
    note: &'a str,
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
    /// Always serialized: an explicit `null` states that no
    /// `DEFINES`/`CONTAINS` owner encloses the marker (module top level),
    /// never silently omitted.
    enclosing_symbol: Option<DebtMarkerSymbolJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Every row is a deterministic extractor fact, advisory by contract.
    trust: &'static str,
}

/// Machine-readable per-category totals.
#[derive(Serialize)]
pub(crate) struct DebtMarkerCounts {
    total: usize,
    todo: usize,
    fixme: usize,
    hack: usize,
    xxx: usize,
}

/// Top-level debt-marker inventory response envelope.
#[derive(Serialize)]
pub(crate) struct DebtMarkerResponse<'a> {
    ok: bool,
    lane: &'static str,
    /// The closed recognized marker set for this slice.
    marker_set: [&'static str; 4],
    path_prefix: Option<&'a str>,
    at_commit: Option<&'a str>,
    disclaimer: &'static str,
    markers: Vec<DebtMarkerJson<'a>>,
    counts: DebtMarkerCounts,
    /// Distinguishes "scope contains zero debt markers" from "scope not
    /// found" (which is an error envelope, exit 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_reason: Option<&'static str>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
    page: AuditPage,
}

pub(crate) const DEBT_MARKER_DISCLAIMER: &str = "Rows are advisory debt-triage leads derived solely from deterministic \
     extractor facts. Each row asserts only that a comment of this category \
     with this note text exists at this span — never that the surrounding \
     code is correct or incorrect.";

#[allow(clippy::too_many_lines)]
pub(crate) fn query_debt_markers_cmd(
    records: &[GraphRecord],
    path_prefix: Option<&str>,
    at: Option<&str>,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let inventory = match query::debt_markers(records, path_prefix, at, index, repo_scope) {
        Ok(inventory) => inventory,
        Err(err) => {
            let (selector_key, selector_value, message): (&str, &str, String) = match &err {
                query::DebtMarkerScopeError::MalformedPrefix { prefix } => (
                    "prefix",
                    prefix,
                    "prefix must be non-empty after stripping trailing slashes".to_owned(),
                ),
                query::DebtMarkerScopeError::ScopeNotFound { prefix } => (
                    "prefix",
                    prefix,
                    format!("no file in the selected store slice lies under `{prefix}`"),
                ),
                query::DebtMarkerScopeError::UnknownCommit { commit } => (
                    "commit",
                    commit,
                    format!("no record in the selected store slice carries commit `{commit}`"),
                ),
                query::DebtMarkerScopeError::AmbiguousCommit { commit, count } => (
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
                query::DebtMarkerScopeError::MalformedPrefix { .. }
                | query::DebtMarkerScopeError::AmbiguousCommit { .. } => 1,
                query::DebtMarkerScopeError::ScopeNotFound { .. }
                | query::DebtMarkerScopeError::UnknownCommit { .. } => 2,
            };
            std::process::exit(exit_code);
        }
    };

    let rows: Vec<DebtMarkerJson<'_>> = inventory
        .markers
        .iter()
        .filter_map(|marker| {
            let GraphRecord::Node {
                id,
                schema_version,
                repo_relative_path,
                span,
                language,
                temporal,
                valid_time,
                ..
            } = marker.record
            else {
                return None;
            };
            let enclosing_symbol = marker.enclosing_symbol.and_then(|symbol| {
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
                Some(DebtMarkerSymbolJson {
                    record_id: symbol_id,
                    name: symbol_name.as_deref(),
                    symbol_kind: symbol_kind.as_deref(),
                    span: *symbol_span,
                })
            });
            let repository_id = index.owner_of(id);
            Some(DebtMarkerJson {
                record_id: id,
                kind: "DebtMarker",
                schema_version: *schema_version,
                category: marker.category,
                note: marker.note,
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

    let counts = DebtMarkerCounts {
        total: rows.len(),
        todo: rows.iter().filter(|r| r.category == "todo").count(),
        fixme: rows.iter().filter(|r| r.category == "fixme").count(),
        hack: rows.iter().filter(|r| r.category == "hack").count(),
        xxx: rows.iter().filter(|r| r.category == "xxx").count(),
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
            println!("{} @ {path}:{line} in {owner}: {}", row.category, row.note);
        }
        if rows.is_empty() {
            println!("# no_markers_in_scope: scope contains zero debt markers");
        }
        return Ok(());
    }

    let response = DebtMarkerResponse {
        ok: true,
        lane: "debt_markers",
        marker_set: ["fixme", "hack", "todo", "xxx"],
        path_prefix,
        at_commit: inventory.at_commit.as_deref(),
        disclaimer: DEBT_MARKER_DISCLAIMER,
        counts,
        empty_reason: if rows.is_empty() {
            Some("no_markers_in_scope")
        } else {
            None
        },
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned: rows.len(),
        },
        markers: rows,
        diagnostics: Vec::new(),
    };

    let output = serde_json::to_string_pretty(&response)
        .context("failed to serialize debt-marker inventory")?;
    println!("{output}");
    Ok(())
}
