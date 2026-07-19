use super::*;

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn query_symbol_via_daemon(
    name: &str,
    data_dir: &Path,
    at: Option<&str>,
    as_of: Option<&str>,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let (verb, mut params) = at.map_or_else(
        || ("symbol_by_name", serde_json::json!({ "name": name })),
        |commit| {
            (
                "symbol_at_commit",
                serde_json::json!({ "name": name, "commit": commit }),
            )
        },
    );
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let records = client
        .query_verb(verb, &params, as_of)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;
    if repo.is_none() && (at.is_some() || as_of.is_some()) {
        fail_on_unscoped_daemon_repo_collision(&records);
    }
    if records.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_symbol_record(rec, format)?;
    }
    Ok(())
}

/// Prints a daemon symbol/file record (`serde_json::Value`) in the requested format.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn print_daemon_symbol_record(
    rec: &serde_json::Value,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let name = rec["name"].as_str().unwrap_or("(unknown)");
            let kind = rec["kind"].as_str().unwrap_or("Symbol");
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            let line = rec["span"]["start_line"].as_u64().unwrap_or(0);
            let commit = rec["git_commit"]
                .as_str()
                .map_or(String::new(), |c| format!(" [{c}]"));
            println!("{name} ({kind}) @ {path}:{line}{commit}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query symbol (all matching)
// ---------------------------------------------------------------------------

pub(crate) fn query_symbol_all(
    records: &[GraphRecord],
    name: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
    let deleted = current_deleted_ids(records);
    let mut results: Vec<SymbolResult<'_>> = records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                id, temporal: None, ..
            } = r
            {
                !deleted.contains(id.as_str())
            } else {
                true
            }
        })
        .filter_map(|r| symbol_result(r, name, index, records, &deleted))
        .collect();
    if let Some(repo) = selected_repo {
        results.retain(|r| r.repository_id == Some(repo));
    }

    if results.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    stamp_freshness(&mut results, freshness_code);
    // Disclosure-only (issue #427): `query symbol` with no `--at`/`--as-of`
    // returns every matching Symbol node across the store — the UNION of all
    // commit snapshots over a scan-history store, or the single snapshot over a
    // plain `scan`. Disclose that honestly on each row.
    let (corpus_mode, corpus_mode_source, _) =
        query::disclose_corpus(records, query::CorpusMode::Union);
    stamp_symbol_corpus(&mut results, corpus_mode, corpus_mode_source);
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

pub(crate) fn symbol_result<'a>(
    record: &'a GraphRecord,
    name: &str,
    index: &'a query::RepositoryIndex,
    all_records: &'a [GraphRecord],
    deleted: &std::collections::BTreeSet<&str>,
) -> Option<SymbolResult<'a>> {
    if let GraphRecord::Node {
        kind: NodeKind::Symbol,
        name: node_name,
        ..
    } = record
        && node_name.as_deref() == Some(name)
    {
        symbol_row(record, index, all_records, deleted)
    } else {
        None
    }
}

/// Builds a `SymbolResult` row for any `Symbol` node record, without a name
/// predicate. Shared by the exact-name (`query symbol`) and partial-name
/// (`query symbols`, issue #102) paths so both emit the same row shape.
pub(crate) fn symbol_row<'a>(
    record: &'a GraphRecord,
    index: &'a query::RepositoryIndex,
    all_records: &'a [GraphRecord],
    deleted: &std::collections::BTreeSet<&str>,
) -> Option<SymbolResult<'a>> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Symbol,
        schema_version,
        name: node_name,
        repo_relative_path,
        span,
        visibility,
        signature,
        doc,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    let (completeness, _) = repo_relative_path
        .as_deref()
        .map_or(("complete", None), |path| {
            get_file_diagnostics(all_records, path, deleted)
        });
    let repository_id = index.owner_of(id);
    Some(SymbolResult {
        record_id: id,
        schema_version: *schema_version,
        name: node_name.as_deref().unwrap_or(""),
        kind: "Symbol",
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        visibility: visibility.as_deref(),
        signature: signature.as_deref(),
        doc: doc.as_deref(),
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        repository_id,
        repository: repository_id.and_then(|repo| index.display_of(repo)),
        freshness: None,
        extraction_completeness: completeness,
        diagnostics: None,
        corpus_mode: None,
        corpus_mode_source: None,
        corpus_disclaimer: None,
    })
}

// ---------------------------------------------------------------------------
// query symbols (partial-name pattern, issue #102)
// ---------------------------------------------------------------------------

/// Lists `Symbol` nodes whose name matches a substring or anchored `*`-glob
/// pattern against the structural store — no embedding model required.
///
/// Only `Symbol` node names are searched, so comments, string literals, and
/// doc text can never produce a match. Tombstoned current-state symbols are
/// excluded (parity with `file_defines`). Output is deterministic and
/// byte-stable: rows are sorted by `(repo_relative_path, span.start_line,
/// record_id)`.
pub(crate) fn query_symbols_matching(
    records: &[GraphRecord],
    pattern: &str,
    case_insensitive: bool,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
) -> Result<()> {
    let deleted = current_deleted_ids(records);
    let mut results: Vec<SymbolResult<'_>> = records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                id, temporal: None, ..
            } = r
            {
                !deleted.contains(id.as_str())
            } else {
                true
            }
        })
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name: Some(node_name),
                    ..
                } if query::symbol_name_matches(pattern, node_name, case_insensitive)
            )
        })
        .filter_map(|r| symbol_row(r, index, records, &deleted))
        .collect();
    if let Some(repo) = selected_repo {
        results.retain(|r| r.repository_id == Some(repo));
    }

    if results.is_empty() {
        eprintln!("error: no match found for pattern `{pattern}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| {
        (
            r.repo_relative_path,
            r.span.map(|s| s.start_line),
            r.record_id,
        )
    });
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query symbol --at <commit>
// ---------------------------------------------------------------------------

pub(crate) fn query_symbol_at(
    records: &[GraphRecord],
    name: &str,
    prefix: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
    // The ambiguity check is repository-scoped: a prefix that collides only
    // across the repository boundary is unambiguous within the selected repo.
    let matching_commits: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|r| {
            selected_repo.is_none_or(|repo| record_belongs_to_repo_for_commit_scan(r, index, repo))
        })
        .filter_map(|r| temporal_commit_if_prefix(r, prefix))
        .collect();

    if matching_commits.len() > 1 {
        eprintln!(
            "error: ambiguous commit prefix `{prefix}` matches {} commits",
            matching_commits.len()
        );
        std::process::exit(1);
    }

    let mut matches = query::symbols_at_commit(records, name, prefix);
    if let Some(repo) = selected_repo {
        matches.retain(|r| index.owner_of(r.id()) == Some(repo));
    } else {
        // Two clones of one history can share a commit SHA under distinct
        // repository identities: never pick one implicitly (issue #67).
        // Unattributed legacy rows form their own candidate group.
        let groups: std::collections::BTreeSet<Option<&str>> =
            matches.iter().map(|r| index.owner_of(r.id())).collect();
        if groups.len() > 1 {
            exit_ambiguous_repository(&groups);
        }
    }

    match matches.into_iter().next() {
        None => {
            eprintln!("error: no match found for symbol `{name}` at commit `{prefix}`");
            std::process::exit(2);
        }
        Some(record) => {
            let deleted = current_deleted_ids(records);
            if let Some(mut result) = symbol_result(record, name, index, records, &deleted) {
                stamp_freshness(std::slice::from_mut(&mut result), freshness_code);
                // `--at` pins a single commit: the corpus is commit-pinned,
                // chosen by the selector (issue #427).
                stamp_symbol_corpus(
                    std::slice::from_mut(&mut result),
                    query::CorpusMode::CommitPinned,
                    query::CorpusModeSource::Selector,
                );
                print_result(&result, format)?;
            }
        }
    }
    Ok(())
}

impl PrintText for SymbolResult<'_> {
    fn as_text(&self) -> String {
        use std::fmt::Write as _;
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        let commit = self.git_commit.map_or(String::new(), |c| format!(" [{c}]"));
        let freshness = self
            .freshness
            .map_or(String::new(), |code| format!(" (freshness: {code})"));
        let completeness = format!(" (extraction: {})", self.extraction_completeness);
        let mut text = format!(
            "{} ({}) @ {path}:{line}{commit}{freshness}{completeness}",
            self.name, self.kind
        );
        if let Some(visibility) = self.visibility {
            let _ = write!(text, "\n  visibility: {visibility}");
        }
        if let Some(signature) = self.signature {
            let _ = write!(text, "\n  signature: {signature}");
        }
        if let Some(doc) = self.doc {
            let _ = write!(text, "\n  doc: {doc}");
        }
        text
    }
}
