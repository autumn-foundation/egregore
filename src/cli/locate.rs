use super::*;

// ---------------------------------------------------------------------------
// query locate (file:line → innermost symbol + context bundle, issue #212)
// ---------------------------------------------------------------------------

/// Top-level `query locate` success envelope: the located symbol handle plus
/// the same trust-separated cross-domain bundle as `eg query context`.
#[derive(Serialize)]
pub(crate) struct LocateResponse<'a> {
    ok: bool,
    path: &'a str,
    line: usize,
    /// Full commit SHA the position was resolved against (temporal pins only).
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_commit: Option<&'a str>,
    /// Valid time of the resolved commit (temporal pins only).
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    /// The smallest enclosing `Symbol` node (the innermost).
    symbol: LocationNodeJson<'a>,
    /// Containing `Module`/`Symbol` nodes, outermost → innermost; the last
    /// entry is always the primary `symbol`.
    enclosing_chain: Vec<LocationNodeJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    source_facts: Vec<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
}

/// Resolves a `--at` prefix or `--as-of` instant to a fully-qualified commit
/// SHA (plus its valid time) for the queried path, reusing the issue #158
/// point-resolution machinery so the temporal + repository-collision contract
/// is identical to `eg query file --at/--as-of`.
///
/// Returns `(resolved_commit, valid_time)` owned strings. On any temporal or
/// path-resolution failure it prints the stable `{"ok":false,...}` envelope and
/// exits, so the returned value is only produced on success.
fn resolve_locate_point(
    records: &[GraphRecord],
    path: &str,
    at: Option<&str>,
    as_of: Option<&str>,
    repo_scope: Option<&str>,
) -> (String, Option<String>) {
    let selector = match (at, as_of) {
        (Some(prefix), None) => query::FileAtPointSelector::At(prefix),
        (None, Some(instant)) => query::FileAtPointSelector::AsOf(instant),
        // clap `conflicts_with` forbids both; the caller only calls this when one is set.
        _ => unreachable!("exactly one of --at / --as-of must be set"),
    };
    match query::file_symbols_at_point(records, path, selector, repo_scope) {
        Ok(result) => (
            result.resolved_commit.to_owned(),
            result.resolved_valid_time.map(str::to_owned),
        ),
        Err(err) => {
            let (error, code) = match &err {
                query::FileAtPointError::EmptyHistory => (
                    serde_json::json!({
                        "code": "empty_history",
                        "message": "temporal selectors require a scan-history store",
                    }),
                    2,
                ),
                query::FileAtPointError::MissingCommit { commit_prefix } => (
                    serde_json::json!({ "code": "missing_commit", "commit": commit_prefix }),
                    2,
                ),
                query::FileAtPointError::AmbiguousCommitPrefix {
                    commit_prefix,
                    matches,
                } => (
                    serde_json::json!({
                        "code": "ambiguous_commit_prefix",
                        "commit": commit_prefix,
                        "candidates": matches,
                    }),
                    1,
                ),
                query::FileAtPointError::InvalidInstant { as_of, detail } => (
                    serde_json::json!({
                        "code": "malformed_timestamp",
                        "as_of": as_of,
                        "message": detail,
                    }),
                    1,
                ),
                query::FileAtPointError::NoCommitAtOrBeforeInstant { as_of } => (
                    serde_json::json!({ "code": "no_commit_at_or_before", "as_of": as_of }),
                    2,
                ),
                query::FileAtPointError::UnknownPath { path } => {
                    (serde_json::json!({ "code": "no_match", "path": path }), 2)
                }
                query::FileAtPointError::FileAbsentAtPoint {
                    path,
                    resolved_commit,
                } => (
                    serde_json::json!({
                        "code": "no_match",
                        "path": path,
                        "resolved_commit": resolved_commit,
                    }),
                    2,
                ),
                query::FileAtPointError::AmbiguousRepository { path, repositories } => (
                    serde_json::json!({
                        "code": "ambiguous_repository",
                        "message": "multiple repositories match; rerun with --repo <SELECTOR>",
                        "path": path,
                        "repositories": repositories,
                    }),
                    1,
                ),
            };
            location_error_exit(&error, code);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_locate_cmd(
    records: &[GraphRecord],
    path: &str,
    line: usize,
    at: Option<&str>,
    as_of: Option<&str>,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    supersession: crate::temporal_status::SupersessionMode,
    format: OutputFormat,
) -> Result<()> {
    // Resolve the temporal pin first (identical contract to `query file`), so a
    // bad commit/instant fails with a machine-readable envelope before any
    // span lookup. The current-state view is used when neither pin is set.
    let pinned_point: Option<(String, Option<String>)> = (at.is_some() || as_of.is_some())
        .then(|| resolve_locate_point(records, path, at, as_of, repo_scope));
    let at_commit = pinned_point.as_ref().map(|(sha, _)| sha.as_str());

    match query::locate(records, path, line, at_commit, index, repo_scope) {
        query::LocateOutcome::NoMatch => location_error_exit(
            &serde_json::json!({ "code": "no_match", "path": path, "line": line }),
            2,
        ),
        query::LocateOutcome::AmbiguousRepository { repositories } => {
            let repos: Vec<&str> = repositories.iter().filter_map(|g| *g).collect();
            let mut error = serde_json::json!({
                "code": "ambiguous_repository",
                "message": "multiple repositories match; rerun with --repo <SELECTOR>",
                "path": path,
                "line": line,
                "repositories": repos,
            });
            if repositories.contains(&None) {
                error["includes_unattributed_rows"] = serde_json::Value::Bool(true);
            }
            location_error_exit(&error, 1);
        }
        query::LocateOutcome::LineOutOfRange {
            file_record,
            max_known_line,
        } => {
            let mut error = serde_json::json!({
                "code": "line_out_of_range",
                "path": path,
                "line": line,
                "max_known_line": max_known_line,
            });
            if let Some(file_record) = file_record {
                error["file_record_id"] = serde_json::json!(file_record.id());
            }
            location_error_exit(&error, 2);
        }
        query::LocateOutcome::NoEnclosingSymbol { file_record } => {
            let mut error = serde_json::json!({
                "code": "no_enclosing_symbol",
                "path": path,
                "line": line,
            });
            if let Some(file_record) = file_record {
                error["file_record_id"] = serde_json::json!(file_record.id());
            }
            location_error_exit(&error, 2);
        }
        query::LocateOutcome::Located {
            primary,
            chain,
            file_record: _,
            repository_id,
            context,
        } => {
            let symbol = location_node_json(primary).expect("primary is a node by construction");
            let sections = build_context_sections(&context);
            let resolver = crate::temporal_status::TemporalResolver::build(records);
            let (observations, excluded) =
                apply_supersession(sections.observations, &resolver, supersession);

            let response = LocateResponse {
                ok: true,
                path,
                line,
                resolved_commit: pinned_point.as_ref().map(|(sha, _)| sha.as_str()),
                valid_time: pinned_point.as_ref().and_then(|(_, vt)| vt.as_deref()),
                symbol,
                enclosing_chain: chain
                    .iter()
                    .filter_map(|record| location_node_json(record))
                    .collect(),
                repository_id,
                repository: repository_id.and_then(|repo| index.display_of(repo)),
                source_facts: sections.source_facts,
                topology_edges: sections.topology_edges,
                observations,
                project_state: sections.project_state,
                artifacts: sections.artifacts,
                verification_evidence: sections.verification_evidence,
                unresolved: sections.unresolved,
                excluded,
            };

            match format {
                OutputFormat::Json => {
                    let output = serde_json::to_string_pretty(&response)
                        .context("failed to serialize locate response")?;
                    println!("{output}");
                }
                OutputFormat::Text => print_locate_text(&response),
            }
            Ok(())
        }
    }
}

/// Human-readable rendering of a located answer: the symbol handle, its
/// enclosing chain, and a one-line count of each trust-separated section.
fn print_locate_text(response: &LocateResponse<'_>) {
    let line_of = |node: &LocationNodeJson<'_>| node.span.map_or(0, |s| s.start_line);
    println!(
        "{} ({}) @ {}:{}",
        response.symbol.name.unwrap_or("(unknown)"),
        response.symbol.symbol_kind.unwrap_or(response.symbol.kind),
        response.path,
        line_of(&response.symbol),
    );
    if let Some(commit) = response.resolved_commit {
        println!("# resolved_commit: {commit}");
    }
    print!("# enclosing:");
    for node in &response.enclosing_chain {
        print!(" {}", node.name.unwrap_or(node.kind));
    }
    println!();
    println!(
        "# source_facts={} observations={} project_state={} artifacts={} verification_evidence={} unresolved={}",
        response.source_facts.len(),
        response.observations.len(),
        response.project_state.len(),
        response.artifacts.len(),
        response.verification_evidence.len(),
        response.unresolved.len(),
    );
}
