use super::*;

// ---------------------------------------------------------------------------
// dep-usage query — `eg query uses <PATH>` (issue #258)
//
// The pre-upgrade question a dependency bump requires: every usage site of an
// external dependency symbol, alias-aware, with no comment/string false
// positives. Reads the AST-derived unresolved `CALLS` edges (a call the
// repo-wide resolver could not bind to an in-repo definition) and resolves
// each written callee against the file's `use` declarations, so
// `use a::b::c as d; d(...)` reports under `a::b::c`.
//
// Usage sites of a symbol the resolver bound INSIDE the repo are out of scope
// (its callers are the `transitive-callers` lane's job); an unresolved
// `crate::…` path is reported with `external: false` and a hint toward that
// lane. Every row is backed by a parsed call site — comments and string
// literals can never produce rows.
// ---------------------------------------------------------------------------

/// Hint attached to internal (`external: false`) rows: the symbol resolved
/// inside the repository, so the callers lane is the right next step.
pub(crate) const DEP_USAGE_CALLERS_HINT: &str = "eg query transitive-callers";

/// Trust note for dep-usage rows (issue #258): a row is a parsed call site the
/// resolver could not bind in-repo — a usage-site LEAD, never proof the call
/// still exists at current source.
pub(crate) const DEP_USAGE_TRUST: &str = "source_fact";

/// One usage-site row emitted as its own NDJSON line.
#[derive(Serialize)]
pub(crate) struct DepUsageRowJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    repo_relative_path: &'a str,
    span: Option<SourceSpan>,
    /// The callee exactly as written at the call site.
    callee: &'a str,
    /// The resolving import path after alias substitution.
    import_path: &'a str,
    /// `false` for paths inside the repository's own crate namespace.
    external: bool,
    /// Present only on internal rows: where to look next.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
    trust: &'static str,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
pub(crate) struct DepUsageHeaderJson<'a> {
    ok: bool,
    /// The query path exactly as passed.
    handle: &'a str,
    direction: &'static str,
    edge_label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    at_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    total_usage_sites: usize,
    /// Corpus the current-state view read (issue #427):
    /// `head_anchored`/`union`/`commit_pinned`/`single_snapshot`.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `default`/`explicit_flag`/`selector`.
    corpus_mode_source: &'static str,
    /// One-line human description of what the corpus includes.
    corpus_disclaimer: &'static str,
    disclaimer: &'static str,
}

pub(crate) const DEP_USAGE_DISCLAIMER: &str = "Rows are usage sites of the queried dependency symbol, resolved through \
     the file's `use` declarations against AST-derived unresolved CALLS edges. Alias-aware \
     (`use a::b::c as d; d(...)` resolves against `a::b::c`); comments and string literals \
     can never produce rows. `external: false` rows name paths inside the repository's own \
     crate namespace — use `eg query transitive-callers` for those. Rows are usage-site \
     LEADS, never proof the call still exists at current source.";

pub(crate) const fn dep_usage_row_json<'a>(row: &'a query::DepUsageRow<'a>) -> DepUsageRowJson<'a> {
    DepUsageRowJson {
        record_id: row.record_id,
        schema_version: row.schema_version,
        repo_relative_path: row.repo_relative_path,
        span: row.span,
        callee: row.callee,
        import_path: row.import_path.as_str(),
        external: row.external,
        hint: if row.external {
            None
        } else {
            Some(DEP_USAGE_CALLERS_HINT)
        },
        trust: DEP_USAGE_TRUST,
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools
)]
pub(crate) fn query_dep_usage_cmd(
    records: &[GraphRecord],
    path: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at: Option<&str>,
    as_of: Option<&str>,
    at_head: bool,
    all_history: bool,
    format: OutputFormat,
) -> Result<()> {
    // ── corpus-mode selection (issue #427) ────────────────────────────────────
    // Current-state code lane: defaults to HEAD-anchoring (records current at
    // each repository's stamped `source_snapshot` HEAD) when a snapshot exists;
    // `--all-history` opts into the union of all commit snapshots and
    // `--at-head` makes the default explicit. `--at`/`--as-of` pin a single
    // commit. Mirrors `who-constructs`. All four inputs run through the shared
    // `resolve_corpus_mode`.
    let has_snapshot = query::store_has_source_snapshot(records);
    let (corpus_mode, corpus_mode_source) = match query::resolve_corpus_mode(
        at.is_some() || as_of.is_some(),
        at_head,
        all_history,
        has_snapshot,
    ) {
        Ok(pair) => pair,
        Err(message) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": { "code": "unsupported_combination", "message": message },
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
    };

    // ── temporal / head-anchor narrowing (mirrors who-constructs) ─────────────
    let mut at_commit: Option<String> = None;
    let filtered: Option<Vec<GraphRecord>> =
        if matches!(corpus_mode, query::CorpusMode::CommitPinned) {
            let sha = resolve_transitive_commit_view(records, index, repo_scope, at, as_of);
            let view: Vec<GraphRecord> = records
                .iter()
                .filter(|r| match r {
                    GraphRecord::Node {
                        temporal: Some(t), ..
                    }
                    | GraphRecord::Edge {
                        temporal: Some(t), ..
                    } => t.git_commit == sha,
                    _ => false,
                })
                .cloned()
                .collect();
            at_commit = Some(sha);
            Some(view)
        } else if matches!(corpus_mode, query::CorpusMode::HeadAnchored) {
            // Drop every record not current at its owning repository's stamped
            // HEAD BEFORE resolution, so a usage site (or the edge, or the
            // import) removed at HEAD does not appear.
            let non_current = query::non_head_current_record_ids(records, index);
            let view: Vec<GraphRecord> = records
                .iter()
                .filter(|r| !non_current.contains(r.id()))
                .cloned()
                .collect();
            Some(view)
        } else {
            None
        };
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    // ── usage-site resolution ─────────────────────────────────────────────────
    // A malformed path is exit 1 (machine-readable JSON on stderr), distinct
    // from a well-formed path with zero usage sites (no_match, exit 2).
    let rows = match query::dep_usage(records, path, index, repo_scope) {
        Ok(rows) => rows,
        Err(err @ query::DepUsageError::MalformedPath { .. }) => {
            let diag = serde_json::json!({
                "code": "malformed_path",
                "handle": path,
                "message": err.to_string(),
            });
            eprintln!("{diag}");
            std::process::exit(1);
        }
    };

    if rows.is_empty() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "handle": path },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let rows_json: Vec<DepUsageRowJson<'_>> = rows.iter().map(dep_usage_row_json).collect();

    let header = DepUsageHeaderJson {
        ok: true,
        handle: path,
        direction: "outbound",
        edge_label: "CALLS",
        at_commit: at_commit.as_deref(),
        as_of,
        total_usage_sites: rows_json.len(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer: corpus_mode.disclaimer(),
        disclaimer: DEP_USAGE_DISCLAIMER,
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header).context("failed to serialize dep-usage header")?
            );
            for row in &rows_json {
                println!(
                    "{}",
                    serde_json::to_string(row).context("failed to serialize dep-usage row")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            println!(
                "usage sites of {} ({}): {}",
                path,
                rows_json.len(),
                if rows_json.iter().all(|r| r.external) {
                    "all external"
                } else {
                    "includes internal paths — see hint"
                },
            );
            for row in &rows_json {
                let location = row
                    .span
                    .map_or_else(String::new, |s| format!(":{}", s.start_line));
                let scope = if row.external { "external" } else { "internal" };
                println!(
                    "  {}{location}  {} -> {}  [{scope}]",
                    row.repo_relative_path, row.callee, row.import_path
                );
            }
        }
    }
    Ok(())
}
