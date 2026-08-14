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

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_symbol_all(
    records: &[GraphRecord],
    name: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    package: Option<&str>,
    freshness_code: Option<&(String, &'static str)>,
    corpus_mode: query::CorpusMode,
    corpus_mode_source: query::CorpusModeSource,
) -> Result<()> {
    let deleted = current_deleted_ids(records);
    // HEAD-anchor keep-last coalescing (issue #456): the ID-level HEAD-anchor
    // pre-filter (`non_head_current_record_ids`, applied by the CLI dispatch)
    // drops symbol IDs whose EVERY version is off-HEAD (`gone`), but retains a
    // multi-version ID whose HEAD version is current (`keeper` at both c1 and
    // HEAD c2). Mirroring the deps pure fn's own keep-last, emit only the latest
    // version per record ID when head-anchored so `keeper` is one HEAD row, not
    // one row per commit; the `union` corpus keeps every version.
    let latest_by_id: std::collections::HashMap<&str, usize> =
        if matches!(corpus_mode, query::CorpusMode::HeadAnchored) {
            let mut best: std::collections::HashMap<
                &str,
                (usize, Option<chrono::DateTime<chrono::FixedOffset>>),
            > = std::collections::HashMap::new();
            for (i, r) in records.iter().enumerate() {
                let GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    id,
                    name: node_name,
                    temporal,
                    valid_time,
                    ..
                } = r
                else {
                    continue;
                };
                if node_name.as_deref() != Some(name) {
                    continue;
                }
                let vt = temporal
                    .as_ref()
                    .map(|t| t.valid_time.as_str())
                    .or(valid_time.as_deref())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                let better =
                    best.get(id.as_str())
                        .is_none_or(|(prev_i, prev_vt)| match (vt, *prev_vt) {
                            (Some(a), Some(b)) => a > b || (a == b && i > *prev_i),
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            (None, None) => i > *prev_i,
                        });
                if better {
                    best.insert(id.as_str(), (i, vt));
                }
            }
            best.into_iter().map(|(k, (i, _))| (k, i)).collect()
        } else {
            std::collections::HashMap::new()
        };
    let head_anchored = matches!(corpus_mode, query::CorpusMode::HeadAnchored);
    let mut results: Vec<SymbolResult<'_>> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            if let GraphRecord::Node {
                id, temporal: None, ..
            } = r
            {
                !deleted.contains(id.as_str())
            } else {
                true
            }
        })
        .filter(|(i, r)| {
            // Keep only the latest version per id under the head-anchored corpus.
            !head_anchored || latest_by_id.get(r.id()).is_none_or(|latest| latest == i)
        })
        .filter_map(|(_, r)| symbol_result(r, name, index, records, &deleted))
        .collect();
    if let Some(repo) = selected_repo {
        results.retain(|r| r.repository_id == Some(repo));
    }
    retain_package_scope(&mut results, package);

    if results.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    stamp_freshness(&mut results, freshness_code);
    // Corpus disclosure (issue #456): the caller resolved the effective corpus
    // (HEAD-anchored by default over a scan-history store, `union` under
    // `--all-history`, `single_snapshot` over a plain scan) and pre-filtered the
    // record slice accordingly; stamp that resolution on each row.
    stamp_symbol_corpus(&mut results, corpus_mode, corpus_mode_source);
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

/// Narrows `results` to one owning Cargo package and stamps the containment
/// caveat on the surviving rows (issue #117).
///
/// Runs AFTER row projection and BEFORE the empty check, and never touches the
/// sort keys — so a scoped answer is an order-preserving subsequence of the
/// unscoped one. The selector was already validated against the corpus
/// catalog by `resolve_package_scope`, so reaching zero rows here means the
/// package genuinely owns no matching symbol (the lane's ordinary exit-2
/// no-match), never a typo.
pub(crate) fn retain_package_scope(results: &mut Vec<SymbolResult<'_>>, package: Option<&str>) {
    let Some(selector) = package else {
        return;
    };
    results.retain(|row| {
        // The manifest must enclose THIS row's path, so a forged pairing the
        // nearest-enclosing walk could never produce is not scopable.
        row.repo_relative_path
            .zip(row.crate_attribution)
            .and_then(|(path, attribution)| attribution.owning_package_for(path))
            .is_some_and(|(name, _)| name == selector)
    });
    for row in results {
        row.crate_attribution_disclaimer = Some(crate::cli::CRATE_ATTRIBUTION_DISCLAIMER);
    }
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
        // Only a claim the resolver could have produced is presentable, read
        // through the ONE record-level boundary the text render and the package
        // catalog also use, so no two surfaces can disagree about one record and
        // no future caller can inherit a weaker gate (issue #117). This row is
        // statically a `Symbol`, so the boundary's kind check is a no-op here —
        // it is the shared entry point that matters.
        crate_attribution: record.presentable_crate_attribution().map(|(a, _)| a),
        crate_attribution_disclaimer: None,
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
    package: Option<&str>,
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
    retain_package_scope(&mut results, package);

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

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_symbol_at(
    records: &[GraphRecord],
    name: &str,
    prefix: &str,
    format: OutputFormat,
    index: &query::RepositoryIndex,
    selected_repo: Option<&str>,
    package: Option<&str>,
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
    // Package scope narrows the CANDIDATES, before a single winner is chosen
    // (issue #117). Applying it afterwards would report "no match" for a symbol
    // that demonstrably exists in the requested package, purely because a
    // same-named symbol in another package sorted first.
    if let Some(selector) = package {
        matches.retain(|r| r.owning_package().map(|(name, _)| name) == Some(selector));
    }
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
            if let Some(result) = symbol_result(record, name, index, records, &deleted) {
                // Candidates were already narrowed above; this stamps the
                // containment caveat on the surviving row.
                let mut scoped = vec![result];
                retain_package_scope(&mut scoped, package);
                let Some(mut result) = scoped.pop() else {
                    eprintln!("error: no match found for symbol `{name}` at commit `{prefix}`");
                    std::process::exit(2);
                };
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
        // A repo-relative path is a real filesystem path, and on Unix that can
        // contain a newline or an ESC. Sanitize it for the TEXT render at the
        // same boundary the manifest citation below is sanitized at, so no row
        // can forge an output line or drive the reader's terminal. Never
        // truncated — a truncated path stops being a citation — and `--format
        // json` still carries the exact bytes.
        let path = self.repo_relative_path.map_or_else(
            || "(unknown)".to_owned(),
            crate::embeddings::sanitized_handle,
        );
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
        // Owning Cargo package (issue #117). An ABSENT field prints NOTHING:
        // the record predates issue #117, so its attribution is unknown, and
        // rendering "unattributed" would fabricate a negative fact.
        //
        // BOTH branches read through a checked accessor rather than the raw
        // fields, because a value read back from a store or a graph is
        // operator-controlled and BOTH renders are claims. `owning_package`
        // refuses an ownership claim the resolver could not have produced;
        // `proven_unattributed_reason` refuses the negative one, which is
        // equally a fact — "provably no owner" is exactly what this feature's
        // absent-vs-unattributed contract says an absent field must NOT be read
        // as. A value failing either check prints nothing at all.
        if let Some(attribution) = self.crate_attribution {
            if let Some((name, manifest)) = self
                .repo_relative_path
                .and_then(|path| attribution.owning_package_for(path))
            {
                // The manifest path is a real filesystem path, and on Unix that
                // can contain a newline or an ESC — so the RENDER is what keeps
                // one row to one line, not a producer-side refusal to record
                // the fact. Sanitized, never truncated: a truncated handle
                // stops being a citation. The package NAME needs no such pass,
                // being charset-gated by `package_name_is_valid` at both the
                // production and read-back checks.
                let manifest = crate::embeddings::sanitized_handle(manifest);
                let _ = write!(text, "\n  package: {name} ({manifest})");
            } else if let Some(reason) = self
                .repo_relative_path
                .and_then(|path| attribution.proven_unattributed_reason_for(path))
            {
                let _ = write!(text, "\n  package: (unattributed: {})", reason.as_str());
            }
        }
        text
    }
}
