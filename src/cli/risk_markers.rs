use super::*;

// ---------------------------------------------------------------------------
// query risk-markers — stub and panic-risk macro triage lane (issue #210)
// ---------------------------------------------------------------------------

/// The enclosing-symbol handle carried by a risk-marker row.
#[derive(Serialize)]
pub(crate) struct RiskMarkerSymbolJson<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

/// One risk-classified macro marker row.
#[derive(Serialize)]
pub(crate) struct RiskMarkerJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    schema_version: u32,
    /// Closed machine-readable category: `stub` / `panic`.
    category: &'a str,
    /// The macro name as recorded by the extractor (e.g. `todo!`).
    macro_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// Always serialized: an explicit `null` states that no
    /// `DEFINES` owner encloses the marker (module top level),
    /// never silently omitted.
    enclosing_symbol: Option<RiskMarkerSymbolJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Every row is a deterministic extractor fact, advisory by contract.
    trust: &'static str,
}

/// Machine-readable per-category totals.
#[derive(Serialize)]
pub(crate) struct RiskMarkerCounts {
    total: usize,
    stub: usize,
    panic: usize,
}

/// Top-level risk-marker inventory response envelope.
#[derive(Serialize)]
pub(crate) struct RiskMarkerResponse<'a> {
    ok: bool,
    lane: &'static str,
    /// The closed known-risk macro set for this slice.
    risk_macro_set: [&'static str; 4],
    path_prefix: Option<&'a str>,
    at_commit: Option<&'a str>,
    disclaimer: &'static str,
    markers: Vec<RiskMarkerJson<'a>>,
    counts: RiskMarkerCounts,
    /// Distinguishes "scope contains zero risk markers" from "scope not
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

pub(crate) const RISK_MARKER_DISCLAIMER: &str = "Rows are advisory triage leads derived solely from deterministic \
     extractor facts. Each row asserts only that a stub or panic macro marker of the given category exists at this \
     span — never that the surrounding code is correct or incorrect. Not a substitute for reading the source.";

/// Machine-readable empty reason: the scope exists and the extractor recorded
/// zero stub/panic macro markers in it — a citable positive fact.
pub(crate) const RISK_MARKER_CLEAN_REASON: &str = "no_markers_in_scope";

/// Machine-readable disclaimer for the clean-scope marker.
pub(crate) const RISK_MARKER_CLEAN_MESSAGE: &str = "scope contains zero risk markers";

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) fn query_risk_markers_cmd(
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
    // the pure fn — whose own latest-write-per-id coalescing then applies over
    // the narrowed slice, so the surviving coalesced record is the HEAD one.
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, index, at.is_some(), at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);
    let inventory = match query::risk_markers(records, path_prefix, at, index, repo_scope) {
        Ok(inventory) => inventory,
        Err(err) => {
            let (selector_key, selector_value, message): (&str, &str, String) = match &err {
                query::RiskMarkerScopeError::MalformedPrefix { prefix } => (
                    "prefix",
                    prefix,
                    "prefix must be non-empty after stripping trailing slashes".to_owned(),
                ),
                query::RiskMarkerScopeError::ScopeNotFound { prefix } => (
                    "prefix",
                    prefix,
                    format!("no file in the selected store slice lies under `{prefix}`"),
                ),
                query::RiskMarkerScopeError::UnknownCommit { commit } => (
                    "commit",
                    commit,
                    format!("no record in the selected store slice carries commit `{commit}`"),
                ),
                query::RiskMarkerScopeError::AmbiguousCommit { commit, count } => (
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
                query::RiskMarkerScopeError::MalformedPrefix { .. }
                | query::RiskMarkerScopeError::AmbiguousCommit { .. } => 1,
                query::RiskMarkerScopeError::ScopeNotFound { .. }
                | query::RiskMarkerScopeError::UnknownCommit { .. } => 2,
            };
            std::process::exit(exit_code);
        }
    };

    let rows: Vec<RiskMarkerJson<'_>> = inventory
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
                Some(RiskMarkerSymbolJson {
                    record_id: symbol_id,
                    name: symbol_name.as_deref(),
                    symbol_kind: symbol_kind.as_deref(),
                    span: *symbol_span,
                })
            });
            let repository_id = marker.repository_id;
            Some(RiskMarkerJson {
                record_id: id,
                kind: "Diagnostic",
                schema_version: *schema_version,
                category: marker.category.as_str(),
                macro_name: marker.macro_name,
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                language: language.as_deref(),
                git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
                enclosing_symbol,
                repository_id,
                repository: repository_id.and_then(|repo| index.display_of(repo)),
                trust: "source_fact",
            })
        })
        .collect();

    let counts = RiskMarkerCounts {
        total: rows.len(),
        stub: rows.iter().filter(|r| r.category == "stub").count(),
        panic: rows.iter().filter(|r| r.category == "panic").count(),
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
                "{} {} @ {path}:{line} in {owner}",
                row.category, row.macro_name
            );
        }
        if rows.is_empty() {
            println!("# {RISK_MARKER_CLEAN_REASON}: {RISK_MARKER_CLEAN_MESSAGE}");
        }
        return Ok(());
    }

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    let response = RiskMarkerResponse {
        ok: true,
        lane: "risk-markers",
        risk_macro_set: query::RISK_MACRO_SET,
        path_prefix,
        at_commit: inventory.at_commit.as_deref(),
        disclaimer: RISK_MARKER_DISCLAIMER,
        counts,
        empty_reason: if rows.is_empty() {
            Some(RISK_MARKER_CLEAN_REASON)
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
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    };

    let output = serde_json::to_string_pretty(&response)
        .context("failed to serialize risk-marker inventory")?;
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_record(id: &str, macro_name: &str, path: &str) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::Diagnostic,
            path.to_owned(),
            SourceSpan {
                start_byte: 20,
                end_byte: 30,
                start_line: 2,
                end_line: 2,
                start_column: None,
                end_column: None,
            },
            macro_name.to_owned(),
            "rust",
            format!("unsupported macro invocation {macro_name}"),
        )
    }

    fn symbol_record(id: &str, name: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 200,
                start_line: 1,
                end_line: 12,
                start_column: None,
                end_column: None,
            }),
            Some(name.to_owned()),
            format!("fn {name}"),
        )
    }

    fn project<'a>(
        index: &'a query::RepositoryIndex,
        marker: &'a query::RiskMarkerRow<'a>,
    ) -> RiskMarkerJson<'a> {
        let GraphRecord::Node {
            id,
            schema_version,
            repo_relative_path,
            span,
            language,
            temporal,
            ..
        } = marker.record
        else {
            panic!("marker must be a node")
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
            Some(RiskMarkerSymbolJson {
                record_id: symbol_id,
                name: symbol_name.as_deref(),
                symbol_kind: symbol_kind.as_deref(),
                span: *symbol_span,
            })
        });
        let repository_id = index.owner_of(id);
        RiskMarkerJson {
            record_id: id,
            kind: "Diagnostic",
            schema_version: *schema_version,
            category: marker.category.as_str(),
            macro_name: marker.macro_name,
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            language: language.as_deref(),
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            enclosing_symbol,
            repository_id,
            repository: repository_id.and_then(|repo| index.display_of(repo)),
            trust: "source_fact",
        }
    }

    #[test]
    fn row_projects_category_and_handles() {
        let record = marker_record("codegraph:v5:todo", "todo!", "src/a.rs");
        let index = query::RepositoryIndex::build(std::slice::from_ref(&record));
        let inventory =
            query::risk_markers(std::slice::from_ref(&record), None, None, &index, None)
                .expect("inventory");
        let row = project(&index, inventory.markers.first().expect("one marker"));
        assert_eq!(row.record_id, "codegraph:v5:todo");
        assert_eq!(row.kind, "Diagnostic");
        assert_eq!(row.category, "stub");
        assert_eq!(row.macro_name, "todo!");
        assert_eq!(row.repo_relative_path, Some("src/a.rs"));
        assert_eq!(row.span.map(|s| s.start_byte), Some(20));
        assert_eq!(row.trust, "source_fact");
        let json = serde_json::to_string(&row).expect("row should serialize");
        for field in [
            "record_id",
            "category",
            "macro_name",
            "repo_relative_path",
            "span",
            "enclosing_symbol",
            "trust",
        ] {
            assert!(json.contains(field), "row JSON should carry {field}");
        }
    }

    #[test]
    fn row_carries_explicit_null_enclosing_symbol_at_top_level() {
        let record = marker_record("codegraph:v5:panic", "panic!", "src/a.rs");
        let index = query::RepositoryIndex::build(std::slice::from_ref(&record));
        let inventory =
            query::risk_markers(std::slice::from_ref(&record), None, None, &index, None)
                .expect("inventory");
        let row = project(&index, inventory.markers.first().expect("one marker"));
        assert!(row.enclosing_symbol.is_none());
        let json = serde_json::to_string(&row).expect("row should serialize");
        assert!(
            json.contains("\"enclosing_symbol\":null"),
            "top-level markers must carry an explicit null: {json}"
        );
    }

    #[test]
    fn row_projects_enclosing_symbol_handle() {
        let records = [
            symbol_record("sym-f", "finish", "src/a.rs"),
            marker_record("m-todo", "todo!", "src/a.rs"),
        ];
        let index = query::RepositoryIndex::build(&records);
        let inventory = query::risk_markers(&records, None, None, &index, None).expect("inventory");
        let row = project(&index, inventory.markers.first().expect("one marker"));
        let owner = row.enclosing_symbol.as_ref().expect("should have an owner");
        assert_eq!(owner.record_id, "sym-f");
        assert_eq!(owner.name, Some("finish"));
    }

    #[test]
    fn clean_reason_constants() {
        assert_eq!(RISK_MARKER_CLEAN_REASON, "no_markers_in_scope");
        assert!(!RISK_MARKER_CLEAN_MESSAGE.is_empty());
        assert!(RISK_MARKER_DISCLAIMER.contains("advisory"));
    }
}
