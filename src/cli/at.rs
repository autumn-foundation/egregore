use super::*;

// ---------------------------------------------------------------------------
// query at (file:line → enclosing symbol, issue #151)
// ---------------------------------------------------------------------------

/// Parses `<path>:<line>` with a 1-based line into its components.
pub(crate) fn parse_file_line_location(
    location: &str,
) -> std::result::Result<(&str, usize), &'static str> {
    let Some((path, line_str)) = location.rsplit_once(':') else {
        return Err("expected <repo-relative-path>:<line>, e.g. src/lib.rs:42");
    };
    if path.is_empty() {
        return Err("path component is empty");
    }
    if line_str.is_empty() {
        return Err("line component is empty");
    }
    let Ok(line) = line_str.parse::<usize>() else {
        return Err("line component is not an integer");
    };
    if line == 0 {
        return Err("line numbers are 1-based; 0 is not a valid line");
    }
    Ok((path, line))
}

/// One resolved node row in the `query at` response: the primary symbol or
/// one enclosing-chain entry.
#[derive(Serialize)]
pub(crate) struct LocationNodeJson<'a> {
    record_id: &'a str,
    kind: &'a str,
    schema_version: u32,
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<&'a str>,
    /// Full commit SHA for history-backed (temporal) records.
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
}

/// Top-level `query at` success envelope.
#[derive(Serialize)]
pub(crate) struct LocationResponse<'a> {
    ok: bool,
    path: &'a str,
    line: usize,
    /// The smallest enclosing `Symbol` node (the innermost).
    symbol: LocationNodeJson<'a>,
    /// Containing `Module`/`Symbol` nodes, outermost → innermost; the last
    /// entry is always the primary `symbol`.
    enclosing_chain: Vec<LocationNodeJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
}

pub(crate) fn location_node_json(record: &GraphRecord) -> Option<LocationNodeJson<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        name,
        symbol_kind,
        repo_relative_path,
        span,
        language,
        visibility,
        signature,
        temporal,
        valid_time,
        ..
    } = record
    else {
        return None;
    };
    Some(LocationNodeJson {
        record_id: id,
        kind: kind.as_str(),
        schema_version: *schema_version,
        name: name.as_deref(),
        symbol_kind: symbol_kind.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        language: language.as_deref(),
        visibility: visibility.as_deref(),
        signature: signature.as_deref(),
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
    })
}

/// Prints a one-line `{"ok":false,"error":{...}}` envelope and exits.
pub(crate) fn location_error_exit(error: &serde_json::Value, exit_code: i32) -> ! {
    let envelope = serde_json::json!({ "ok": false, "error": error });
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("error envelope must serialize")
    );
    std::process::exit(exit_code);
}

pub(crate) fn query_at_cmd(
    records: &[GraphRecord],
    path: &str,
    line: usize,
    at_prefix: Option<&str>,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<()> {
    // Resolve the temporal pin first: a full SHA or unique prefix, with the
    // same repository-scoped ambiguity rules as `query symbol --at`.
    let at_commit: Option<String> = at_prefix.map(|prefix| {
        let matching_commits: std::collections::BTreeSet<&str> = records
            .iter()
            .filter(|r| {
                repo_scope.is_none_or(|repo| record_belongs_to_repo_for_commit_scan(r, index, repo))
            })
            .filter_map(|r| temporal_commit_if_prefix(r, prefix))
            .collect();
        match matching_commits.len() {
            0 => location_error_exit(
                &serde_json::json!({ "code": "missing_commit", "commit": prefix }),
                2,
            ),
            1 => (*matching_commits
                .iter()
                .next()
                .expect("len() == 1 guarantees a first element"))
            .to_owned(),
            _ => {
                let candidates: Vec<&str> = matching_commits.into_iter().collect();
                location_error_exit(
                    &serde_json::json!({
                        "code": "ambiguous_commit_prefix",
                        "commit": prefix,
                        "candidates": candidates,
                    }),
                    1,
                );
            }
        }
    });

    let ctx = query::location_context(records, path, line, at_commit.as_deref(), index, repo_scope);

    // No record carries this path in the selected view: unknown handle.
    if ctx.repo_groups.is_empty() {
        location_error_exit(
            &serde_json::json!({ "code": "no_match", "path": path, "line": line }),
            2,
        );
    }

    // A single-symbol answer cannot represent two repositories: fail closed
    // on an unscoped collision instead of picking one implicitly (issue #67).
    if repo_scope.is_none() && ctx.repo_groups.len() > 1 {
        exit_ambiguous_repository(&ctx.repo_groups);
    }

    let Some(primary) = ctx.primary else {
        // The line sits outside every recorded symbol span. Absence is the
        // answer — never a nearest-neighbor guess.
        let mut error = serde_json::json!({
            "code": "no_enclosing_symbol",
            "path": path,
            "line": line,
        });
        if let Some(file_record) = ctx.file_record {
            error["file_record_id"] = serde_json::json!(file_record.id());
        }
        location_error_exit(&error, 2);
    };

    let symbol = location_node_json(primary).expect("primary is a node by construction");
    let repository_id = index.owner_of(symbol.record_id);
    let response = LocationResponse {
        ok: true,
        path,
        line,
        symbol,
        enclosing_chain: ctx
            .chain
            .iter()
            .filter_map(|record| location_node_json(record))
            .collect(),
        repository_id,
        repository: repository_id.and_then(|repo| index.display_of(repo)),
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize location context")?;
    println!("{output}");
    Ok(())
}
