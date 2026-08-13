use super::*;

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn query_file_via_daemon(
    path: &str,
    data_dir: &Path,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let mut params = serde_json::json!({ "repo_relative_path": path });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let result = client
        .query_verb_raw("file_defines", &params, None)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;
    // Forward the daemon's repository-scope diagnostics (e.g.
    // `excluded_other_repositories`) to stderr so the daemon-routed CLI keeps
    // the same machine-readable contract as the local path (issue #67).
    if let Some(diagnostics) = result.get("diagnostics").and_then(|v| v.as_array()) {
        for diagnostic in diagnostics {
            eprintln!("{}", serde_json::to_string(diagnostic)?);
        }
    }
    let records = result
        .get("records")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if records.is_empty() {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_symbol_record(rec, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query file
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
pub(crate) fn query_file(
    records: &[GraphRecord],
    path: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
) -> Result<()> {
    let deleted = current_deleted_ids(records);

    let file_exists = records.iter().any(|r| {
        let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path,
            ..
        } = r
        else {
            return false;
        };
        repo_relative_path.as_deref() == Some(path)
            && !deleted.contains(id.as_str())
            && selected_repo.is_none_or(|repo| index.owner_of(id) == Some(repo))
    });

    let (completeness, diags) = get_file_diagnostics(records, path, &deleted);
    let mut results: Vec<SymbolResult<'_>> = Vec::new();
    // Same-path rows excluded by the repository scope: counted and surfaced
    // through a diagnostic only — never mixed into the result set (issue #67).
    let mut excluded_rows: usize = 0;
    let mut excluded_repos: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    let mut is_first = true;
    for r in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            schema_version,
            name,
            repo_relative_path,
            span,
            temporal,
            ..
        } = r
        else {
            continue;
        };
        if repo_relative_path.as_deref() != Some(path) {
            continue;
        }
        if temporal.is_none() && deleted.contains(id.as_str()) {
            continue;
        }
        let repository_id = index.owner_of(id);
        if let Some(repo) = selected_repo
            && repository_id != Some(repo)
        {
            excluded_rows += 1;
            if let Some(other) = repository_id {
                excluded_repos.insert(other);
            }
            continue;
        }
        results.push(SymbolResult {
            record_id: id,
            schema_version: *schema_version,
            name: name.as_deref().unwrap_or(""),
            kind: "Symbol",
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            // Declaration-surface fields are a symbol-contract lane: they are
            // returned by `eg query symbol`, not repeated on every row of the
            // per-file listing (which would re-serialize much of the file and
            // regress the `eg audit token-cost` savings gate).
            visibility: None,
            signature: None,
            doc: None,
            // Crate attribution IS carried here, unlike the declaration-surface
            // fields above (issue #117). Those differ per row, so omitting them
            // only trims duplication a caller can recover with `eg query
            // symbol`; omitting attribution would drop the owning-package fact
            // from the ENTIRE answer, since this lane has no envelope — and
            // `docs/cli/query.md` promises these rows carry every `eg query
            // symbol` field but those three.
            //
            // Every row takes its OWN record's attribution; duplicates are
            // blanked after the sort below, so whatever survives is always a
            // fact about the row it rides on.
            crate_attribution: r.crate_attribution(),
            crate_attribution_disclaimer: None,
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            repository_id,
            repository: repository_id.and_then(|repo| index.display_of(repo)),
            freshness: None,
            extraction_completeness: completeness,
            diagnostics: if is_first {
                is_first = false;
                diags.clone()
            } else {
                None
            },
            corpus_mode: None,
            corpus_mode_source: None,
            corpus_disclaimer: None,
        });
    }

    if excluded_rows > 0 {
        let diag = serde_json::json!({
            "code": "excluded_other_repositories",
            "repo_relative_path": path,
            "excluded_repository_count": excluded_repos.len(),
            "excluded_row_count": excluded_rows,
        });
        eprintln!("{diag}");
    }

    if !file_exists || results.is_empty() {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    // `crate_attribution` is a file-level fact (issue #117), but "the file" is
    // not globally unique: a repo-relative path can exist in several
    // repositories, and over a `scan-history` graph its owning package can
    // change between commits. So rather than pick one attribution for the whole
    // answer, keep the FIRST occurrence of each DISTINCT one and blank the
    // repeats. Every surviving value is then a fact about the row it rides on,
    // every distinct package in the answer appears at least once, and the common
    // single-package case still emits exactly one — repeating it on every row
    // measurably regresses the `eg audit token-cost` savings gate.
    let mut seen: std::collections::BTreeSet<(Option<&str>, &crate::ir::CrateAttribution)> =
        std::collections::BTreeSet::new();
    for result in &mut results {
        let Some(attribution) = result.crate_attribution else {
            continue;
        };
        if !seen.insert((result.repository_id, attribution)) {
            result.crate_attribution = None;
        }
    }
    stamp_freshness(&mut results, freshness_code);
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query file --at / --as-of (issue #158)
// ---------------------------------------------------------------------------

/// Prints a file's defined-symbol set reconstructed at a past commit or
/// instant, as a single deterministic JSON envelope (issue #158).
///
/// Exit codes follow the documented temporal contract: `0` for a resolved
/// point (including an explicit empty-but-found result), `2` for
/// no-match/invalid input (unknown path, path absent at the point, missing
/// commit, empty history, malformed or out-of-range instant), and `1` for an
/// ambiguous commit prefix or an ambiguous unscoped repository collision.
pub(crate) fn query_file_at_point(
    records: &[GraphRecord],
    path: &str,
    at: Option<&str>,
    as_of: Option<&str>,
    selected_repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let selector = match (at, as_of) {
        (Some(prefix), None) => query::FileAtPointSelector::At(prefix),
        (None, Some(instant)) => query::FileAtPointSelector::AsOf(instant),
        // clap's `conflicts_with` forbids both; the caller guards against neither.
        _ => unreachable!("exactly one of --at / --as-of must be set"),
    };
    match query::file_symbols_at_point(records, path, selector, selected_repo) {
        Ok(result) => {
            // A temporal selector is always in effect on this path, so the
            // corpus is the single-commit snapshot it pins (issue #427).
            let corpus_mode = query::CorpusMode::CommitPinned;
            match format {
                OutputFormat::Json => {
                    #[derive(serde::Serialize)]
                    struct FileAtPointResponse<'a> {
                        ok: bool,
                        #[serde(flatten)]
                        result: query::FileSymbolsAtPoint<'a>,
                        /// Corpus this answer was read from (issue #427):
                        /// always `commit_pinned` on the `--at`/`--as-of` path.
                        corpus_mode: &'static str,
                        /// How the corpus mode was chosen: always `selector`.
                        corpus_mode_source: &'static str,
                        /// One-line human description of the corpus.
                        corpus_disclaimer: String,
                    }
                    let response = FileAtPointResponse {
                        ok: true,
                        result,
                        corpus_mode: corpus_mode.as_str(),
                        corpus_mode_source: query::CorpusModeSource::Selector.as_str(),
                        corpus_disclaimer: corpus_mode.disclaimer().to_owned(),
                    };
                    let output = serde_json::to_string_pretty(&response)
                        .context("failed to serialize file-at-point result")?;
                    println!("{output}");
                }
                OutputFormat::Text => {
                    for row in &result.symbols {
                        let line = row.span.map_or(0, |s| s.start_line);
                        println!(
                            "{} (Symbol) @ {}:{line} [{}]",
                            row.name, row.repo_relative_path, row.commit
                        );
                    }
                    for diag in &result.diagnostics {
                        println!("# {}: {}", diag.code, diag.detail);
                    }
                    println!(
                        "# resolved_commit: {}{}",
                        result.resolved_commit,
                        result
                            .resolved_valid_time
                            .map_or(String::new(), |vt| format!(" ({vt})"))
                    );
                    println!("# corpus: {}", corpus_mode.as_str());
                }
            }
            Ok(())
        }
        Err(err) => {
            #[derive(serde::Serialize)]
            struct FileAtPointErrorResponse {
                ok: bool,
                error: query::FileAtPointError,
            }
            let envelope = FileAtPointErrorResponse {
                ok: false,
                error: err.clone(),
            };
            println!(
                "{}",
                serde_json::to_string(&envelope)
                    .context("failed to serialize file-at-point error")?
            );
            let exit_code = match err {
                query::FileAtPointError::AmbiguousCommitPrefix { .. }
                | query::FileAtPointError::AmbiguousRepository { .. } => 1,
                _ => 2,
            };
            std::process::exit(exit_code);
        }
    }
}
