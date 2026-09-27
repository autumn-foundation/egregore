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
    /// Agent-authored decisions with rationale (issue #191) — never mixed
    /// into `observations`.
    decisions: Vec<ContextDecision<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedDiagnostic<'a>>,
    /// Corpus the position was resolved against (issue #427): `commit_pinned`
    /// under `--at`/`--as-of`, otherwise `head_anchored` over a scan-history
    /// store carrying a `source_snapshot`, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `selector` under a temporal pin, else
    /// `default`.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

/// Maps a [`query::FileAtPointError`] to the stable machine-readable error body
/// and the process exit code the cold CLI path uses for it. Shared by the
/// local `query locate` surface and the daemon `locate` verb so both report
/// identical typed errors.
pub(crate) fn file_at_point_error_parts(err: &query::FileAtPointError) -> (serde_json::Value, i32) {
    match err {
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
    }
}

/// Non-exiting variant of the temporal-pin resolution: returns the error body
/// and exit code instead of printing and exiting, so the daemon `locate` verb
/// can translate them into its wire contract. The CLI keeps the exiting
/// behavior through [`location_error_exit`].
pub(crate) fn resolve_locate_point_value(
    records: &[GraphRecord],
    path: &str,
    at: Option<&str>,
    as_of: Option<&str>,
    repo_scope: Option<&str>,
) -> Result<(String, Option<String>), (serde_json::Value, i32)> {
    let selector = match (at, as_of) {
        (Some(prefix), None) => query::FileAtPointSelector::At(prefix),
        (None, Some(instant)) => query::FileAtPointSelector::AsOf(instant),
        // clap `conflicts_with` forbids both; the caller only calls this when one is set.
        _ => unreachable!("exactly one of --at / --as-of must be set"),
    };
    match query::file_symbols_at_point(records, path, selector, repo_scope) {
        Ok(result) => Ok((
            result.resolved_commit.to_owned(),
            result.resolved_valid_time.map(str::to_owned),
        )),
        Err(err) => Err(file_at_point_error_parts(&err)),
    }
}

/// Builds the full `query locate` success envelope as a JSON value without
/// printing or exiting: the located symbol handle plus the same
/// trust-separated cross-domain bundle as `eg query context`.
///
/// Returns `Ok` with the success envelope, or `Err` with the stable typed
/// error body plus the process exit code the CLI surface uses for it. The
/// daemon `locate` verb shares this builder so the daemon answer is the same
/// JSON the `--graph` path emits.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn locate_response_value(
    records: &[GraphRecord],
    path: &str,
    line: usize,
    at: Option<&str>,
    as_of: Option<&str>,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<serde_json::Value, (serde_json::Value, i32)> {
    // Resolve the temporal pin first (identical contract to `query file`), so a
    // bad commit/instant fails with a machine-readable envelope before any
    // span lookup. The current-state view is used when neither pin is set.
    let pinned_point: Option<(String, Option<String>)> = (at.is_some() || as_of.is_some())
        .then(|| resolve_locate_point_value(records, path, at, as_of, repo_scope))
        .transpose()?;
    let at_commit = pinned_point.as_ref().map(|(sha, _)| sha.as_str());

    match query::locate(records, path, line, at_commit, index, repo_scope) {
        query::LocateOutcome::NoMatch => Err((
            serde_json::json!({ "code": "no_match", "path": path, "line": line }),
            2,
        )),
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
            Err((error, 1))
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
            Err((error, 2))
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
            Err((error, 2))
        }
        query::LocateOutcome::Located {
            primary,
            chain,
            file_record: _,
            repository_id,
            context,
        } => {
            let trust = query::TrustIndex::build(records);
            let symbol = location_node_json(primary, &trust).ok_or_else(|| {
                (
                    serde_json::json!({
                        "code": "internal_error",
                        "message": "located primary record is not a node",
                    }),
                    1,
                )
            })?;
            let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
                disclose_head_anchored_corpus(records, at.is_some() || as_of.is_some());
            let sections = build_context_sections(records, &context, &trust);
            let (observations, mut excluded) =
                apply_supersession(sections.observations, trust.resolver(), supersession);
            // Section contract (issue #191): decisions surface in their own
            // section with the same supersession handling as observations.
            let (decisions, decision_excluded) =
                apply_supersession(sections.decisions, trust.resolver(), supersession);
            excluded.extend(decision_excluded);

            let response = LocateResponse {
                ok: true,
                path,
                line,
                resolved_commit: pinned_point.as_ref().map(|(sha, _)| sha.as_str()),
                valid_time: pinned_point.as_ref().and_then(|(_, vt)| vt.as_deref()),
                symbol,
                enclosing_chain: chain
                    .iter()
                    .filter_map(|record| location_node_json(record, &trust))
                    .collect(),
                repository_id,
                repository: repository_id.and_then(|repo| index.display_of(repo)),
                source_facts: sections.source_facts,
                topology_edges: sections.topology_edges,
                observations,
                decisions,
                project_state: sections.project_state,
                artifacts: sections.artifacts,
                verification_evidence: sections.verification_evidence,
                unresolved: sections.unresolved,
                // Issue #169: policy is deferred in the locate lane (locate
                // answers "where is X", not "what governs X"). Documented in
                // docs/cli/policy-context.md.
                excluded,
                corpus_mode,
                corpus_mode_source,
                corpus_disclaimer,
            };

            serde_json::to_value(&response).map_err(|err| {
                (
                    serde_json::json!({
                        "code": "internal_error",
                        "message": format!("failed to serialize locate response: {err}"),
                    }),
                    1,
                )
            })
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
    match locate_response_value(
        records,
        path,
        line,
        at,
        as_of,
        index,
        repo_scope,
        supersession,
    ) {
        Err((error, code)) => location_error_exit(&error, code),
        Ok(value) => {
            match format {
                OutputFormat::Json => {
                    let output = serde_json::to_string_pretty(&value)
                        .context("failed to serialize locate response")?;
                    println!("{output}");
                }
                OutputFormat::Text => print_locate_text_value(&value),
            }
            Ok(())
        }
    }
}

/// Routes `query locate` through the running daemon (issue #212 `--daemon`).
///
/// The daemon `locate` verb returns the same JSON envelope the `--graph` path
/// emits; this client prints it verbatim for `--format json`, renders the
/// human-readable form for `--format text`, and re-emits the daemon's typed
/// positional errors (`no_match`, `no_enclosing_symbol`, `line_out_of_range`,
/// …) as the same machine-readable envelope plus the cold path's exit code.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn query_locate_via_daemon(
    path: &str,
    line: usize,
    data_dir: &Path,
    at: Option<&str>,
    as_of: Option<&str>,
    repo: Option<&str>,
    supersession: crate::temporal_status::SupersessionMode,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut params = serde_json::json!({
        "repo_relative_path": path,
        "line": line,
        "supersession": supersession,
    });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    if let Some(at) = at {
        params["at"] = serde_json::json!(at);
    }
    if let Some(as_of) = as_of {
        params["as_of"] = serde_json::json!(as_of);
    }
    match client.query_verb_raw("locate", &params, None) {
        Ok(result) => {
            let envelope = &result["locate"];
            match format {
                OutputFormat::Json => {
                    let output = serde_json::to_string_pretty(envelope)
                        .context("failed to serialize locate response")?;
                    println!("{output}");
                }
                OutputFormat::Text => print_locate_text_value(envelope),
            }
            Ok(())
        }
        Err(e) => {
            // Re-emit the daemon's typed positional error as the
            // machine-readable envelope the `--graph` path prints, keeping the
            // cold path's exit-code contract (2 = not found, 1 = ambiguous /
            // malformed).
            if let Some(rejection) = e.downcast_ref::<crate::daemon::DaemonQueryRejection>() {
                let mut error = serde_json::json!({
                    "code": rejection.code,
                    "path": path,
                    "line": line,
                    "message": rejection.message,
                });
                if let Some(candidates) = &rejection.candidates {
                    error["candidates"] = serde_json::json!(candidates);
                }
                let exit_code = match rejection.code.as_str() {
                    "no_match"
                    | "no_enclosing_symbol"
                    | "line_out_of_range"
                    | "empty_history"
                    | "missing_commit"
                    | "no_commit_at_or_before" => 2,
                    "ambiguous_repository"
                    | "ambiguous_commit_prefix"
                    | "malformed_timestamp"
                    | "malformed_location" => 1,
                    _ => return Err(e),
                };
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({ "ok": false, "error": error }))?
                );
                std::process::exit(exit_code);
            }
            Err(e)
        }
    }
}

/// Human-readable rendering of a located answer from the JSON envelope: the
/// symbol handle, its enclosing chain, and a one-line count of each
/// trust-separated section. Reads the same fields the struct-based renderer
/// used so `--format text` renders identically over the daemon transport.
fn print_locate_text_value(value: &serde_json::Value) {
    let symbol = &value["symbol"];
    let name = symbol["name"].as_str().unwrap_or("(unknown)");
    let kind = symbol["symbol_kind"]
        .as_str()
        .or_else(|| symbol["kind"].as_str())
        .unwrap_or("?");
    let path = value["path"].as_str().unwrap_or("?");
    let start_line = symbol["span"]["start_line"].as_u64().unwrap_or(0);
    println!("{name} ({kind}) @ {path}:{start_line}");
    if let Some(commit) = value["resolved_commit"].as_str() {
        println!("# resolved_commit: {commit}");
    }
    print!("# enclosing:");
    if let Some(chain) = value["enclosing_chain"].as_array() {
        for node in chain {
            let node_name = node["name"]
                .as_str()
                .unwrap_or_else(|| node["kind"].as_str().unwrap_or("?"));
            print!(" {node_name}");
        }
    }
    println!();
    let section_len = |key: &str| value[key].as_array().map_or(0, Vec::len);
    println!(
        "# source_facts={} observations={} project_state={} artifacts={} verification_evidence={} unresolved={}",
        section_len("source_facts"),
        section_len("observations"),
        section_len("project_state"),
        section_len("artifacts"),
        section_len("verification_evidence"),
        section_len("unresolved"),
    );
    println!("# corpus: {}", value["corpus_mode"].as_str().unwrap_or("?"));
}
