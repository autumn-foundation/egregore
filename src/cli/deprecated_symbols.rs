use super::*;

// ---------------------------------------------------------------------------
// deprecated-symbol inventory with the migration worklist (issue #249)
// ---------------------------------------------------------------------------

/// One worklist call-site row in the deprecated-symbols response.
#[derive(Serialize)]
pub(crate) struct DeprecatedCallSiteJson<'a> {
    /// Stable record ID of the `CALLS` edge — the citable call-site handle.
    call_edge_record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_record_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_span: Option<SourceSpan>,
    /// Recorded edge resolution: `resolved`, `ambiguous`, or `unrecorded`.
    resolution: &'a str,
}

/// One deprecated-symbol row in the deprecated-symbols response.
#[derive(Serialize)]
pub(crate) struct DeprecatedSymbolJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    /// Introducing commit for temporal (history-backed) records.
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// Verbatim `deprecated(since = ...)` payload; `null` when the attribute
    /// did not carry one — the documented absent value, never fabricated
    /// text.
    deprecated_since: Option<&'a str>,
    /// Verbatim `deprecated(note = ...)` payload; `null` when absent.
    deprecated_note: Option<&'a str>,
    /// The migration worklist: still-resolvable call sites.
    call_sites: Vec<DeprecatedCallSiteJson<'a>>,
}

/// Deterministic tallies in the deprecated-symbols response.
#[derive(Serialize)]
pub(crate) struct DeprecatedCountsJson {
    symbols_considered: usize,
    deprecated_symbols: usize,
    resolved_call_sites: usize,
    unresolved_call_edges: usize,
}

/// Call edges the graph could not resolve to an in-repo target (issue #152).
#[derive(Serialize)]
pub(crate) struct DeprecatedUnresolvedCallsJson<'a> {
    /// Honest count — never silently zero.
    count: usize,
    /// Stable record ID of every unresolved `CALLS` edge, sorted; every
    /// such edge is cited, never dropped.
    call_edge_record_ids: &'a [String],
}

/// Coverage section of the deprecated-symbols response.
#[derive(Serialize)]
pub(crate) struct DeprecatedCoverageJson<'a> {
    unresolved_call_edges: DeprecatedUnresolvedCallsJson<'a>,
}

/// One stable machine-readable diagnostic in the deprecated-symbols response.
#[derive(Serialize)]
pub(crate) struct DeprecatedDiagnosticJson<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    detail: &'a str,
}

/// Top-level deprecated-symbols response envelope.
#[derive(Serialize)]
pub(crate) struct DeprecatedResponse<'a> {
    ok: bool,
    lane: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    /// The `--file` selector, when one scoped the inventory.
    #[serde(skip_serializing_if = "Option::is_none")]
    file_filter: Option<&'a str>,
    /// Per-response soundness boundary: inventory only, never advice.
    disclaimer: &'static str,
    deprecated_symbols: Vec<DeprecatedSymbolJson<'a>>,
    counts: DeprecatedCountsJson,
    coverage: DeprecatedCoverageJson<'a>,
    diagnostics: Vec<DeprecatedDiagnosticJson<'a>>,
    /// Corpus the current-state view read (issue #427): `head_anchored` over a
    /// scan-history store carrying a `source_snapshot`, `single_snapshot` over
    /// a plain snapshot-less scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
}

pub(crate) const DEPRECATED_SYMBOLS_DISCLAIMER: &str = "Rows are advisory migration-triage leads derived solely from deterministic \
     extractor facts. Each row asserts only that a symbol carries a #[deprecated] \
     attribute mark and that the listed call sites resolve to it over the recorded \
     CALLS edges — never what should replace the symbol. No LLM prose, no \
     replacement suggestions, and no source text beyond the bounded since/note \
     attribute payloads is synthesized.";

/// Exit codes for the deprecated-symbols lane (issue #249 AC7):
/// - `0` — inventory returned (at least one deprecated symbol).
/// - `3` — `no_deprecated_symbols`: live code symbols exist, none marked.
/// - `4` — `no_code_symbols`: the input resolves to zero code-graph nodes.
/// - `1` — usage errors (unreadable graph, bad `--repo`, both/neither
///   `--graph`/`--data-dir`).
pub(crate) const DEPRECATED_SYMBOLS_EXIT_NO_DEPRECATED: i32 = 3;
pub(crate) const DEPRECATED_SYMBOLS_EXIT_NO_CODE_SYMBOLS: i32 = 4;

pub(crate) fn query_deprecated_symbols_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    file_filter: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let result = query::deprecated_symbols(records, index, repo_scope, file_filter);
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        disclose_head_anchored_corpus(records, false);

    // Distinct exit codes keep "no deprecated symbols" and "no code symbols
    // at all" from being conflated with an empty success.
    let exit_code = if result.symbols.is_empty() {
        if result.counts.symbols_considered == 0 {
            DEPRECATED_SYMBOLS_EXIT_NO_CODE_SYMBOLS
        } else {
            DEPRECATED_SYMBOLS_EXIT_NO_DEPRECATED
        }
    } else {
        0
    };

    match format {
        OutputFormat::Json => print_deprecated_symbols_json(
            &result,
            repo_scope,
            file_filter,
            corpus_mode,
            corpus_mode_source,
            corpus_disclaimer,
        )?,
        OutputFormat::Text => {
            print_deprecated_symbols_text(&result, repo_scope, file_filter, corpus_mode);
        }
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Emits the deprecated-symbol inventory as newline-delimited JSON (issue
/// #249 AC): one compact JSON document on a single line, like the sibling
/// query lanes — byte-identical across runs on an unchanged store.
fn print_deprecated_symbols_json(
    result: &query::DeprecatedSymbols<'_>,
    repo_scope: Option<&str>,
    file_filter: Option<&str>,
    corpus_mode: &'static str,
    corpus_mode_source: &'static str,
    corpus_disclaimer: String,
) -> Result<()> {
    let response = DeprecatedResponse {
        ok: true,
        lane: "deprecated_symbols",
        repo_scope,
        file_filter,
        disclaimer: DEPRECATED_SYMBOLS_DISCLAIMER,
        deprecated_symbols: result
            .symbols
            .iter()
            .map(|symbol| DeprecatedSymbolJson {
                record_id: symbol.record_id,
                schema_version: symbol.schema_version,
                name: symbol.name,
                kind: symbol.kind,
                repo_relative_path: symbol.repo_relative_path,
                span: symbol.span,
                git_commit: symbol.git_commit,
                deprecated_since: symbol.since,
                deprecated_note: symbol.note,
                call_sites: symbol
                    .call_sites
                    .iter()
                    .map(|site| DeprecatedCallSiteJson {
                        call_edge_record_id: site.call_edge_record_id,
                        caller_record_id: site.caller_record_id,
                        caller_name: site.caller_name,
                        caller_kind: site.caller_kind,
                        caller_repo_relative_path: site.caller_repo_relative_path,
                        caller_span: site.caller_span,
                        resolution: site.resolution,
                    })
                    .collect(),
            })
            .collect(),
        counts: DeprecatedCountsJson {
            symbols_considered: result.counts.symbols_considered,
            deprecated_symbols: result.counts.deprecated_symbols,
            resolved_call_sites: result.counts.resolved_call_sites,
            unresolved_call_edges: result.counts.unresolved_call_edges,
        },
        coverage: DeprecatedCoverageJson {
            unresolved_call_edges: DeprecatedUnresolvedCallsJson {
                count: result.counts.unresolved_call_edges,
                call_edge_record_ids: &result.unresolved_call_edge_ids,
            },
        },
        diagnostics: result
            .diagnostics
            .iter()
            .map(|d| DeprecatedDiagnosticJson {
                code: d.code,
                record_id: d.record_id.as_deref(),
                detail: &d.detail,
            })
            .collect(),
        corpus_mode,
        corpus_mode_source,
        corpus_disclaimer,
    };
    let output = serde_json::to_string(&response)
        .context("failed to serialize deprecated-symbol inventory")?;
    println!("{output}");
    Ok(())
}

/// Renders one inventory row plus its worklist for `--format text`.
fn print_deprecated_symbol_text(symbol: &query::DeprecatedSymbol<'_>) {
    let citation = match (symbol.repo_relative_path, symbol.span) {
        (Some(path), Some(span)) => {
            format!(" @ {path}:{}-{}", span.start_line, span.end_line)
        }
        (Some(path), None) => format!(" @ {path}"),
        (None, _) => String::new(),
    };
    println!(
        "- {} [{}] deprecated_since={} deprecated_note={}{citation} ({})",
        symbol.name,
        symbol.kind,
        symbol.since.unwrap_or("-"),
        symbol.note.unwrap_or("-"),
        symbol.record_id
    );
    for site in &symbol.call_sites {
        let caller = match (
            site.caller_name,
            site.caller_repo_relative_path,
            site.caller_span,
        ) {
            (Some(name), Some(path), Some(span)) => {
                format!("{name} @ {path}:{}-{}", span.start_line, span.end_line)
            }
            (Some(name), Some(path), None) => format!("{name} @ {path}"),
            (Some(name), None, _) => name.to_owned(),
            (None, _, _) => "<unknown caller>".to_owned(),
        };
        println!(
            "  - {} [{}] ({}) via {}",
            caller,
            site.resolution,
            site.call_edge_record_id,
            site.caller_record_id.unwrap_or("-"),
        );
    }
}

/// Emits the deprecated-symbol inventory as human-readable lines (issue #249
/// `--format text`).
fn print_deprecated_symbols_text(
    result: &query::DeprecatedSymbols<'_>,
    repo_scope: Option<&str>,
    file_filter: Option<&str>,
    corpus_mode: &str,
) {
    match (repo_scope, file_filter) {
        (Some(repo), Some(file)) => println!("scope: repo={repo} file={file}"),
        (Some(repo), None) => println!("scope: repo={repo}"),
        (None, Some(file)) => println!("scope: file={file}"),
        (None, None) => {}
    }
    if result.symbols.is_empty() {
        for d in &result.diagnostics {
            println!("diagnostic: {}: {}", d.code, d.detail);
        }
    } else {
        println!(
            "Deprecated symbols ({}) — migration worklist. Code facts only: no replacement \
             APIs are suggested.",
            result.symbols.len()
        );
        for symbol in &result.symbols {
            print_deprecated_symbol_text(symbol);
        }
    }
    println!(
        "counts: symbols_considered={} deprecated_symbols={} resolved_call_sites={} \
         unresolved_call_edges={}",
        result.counts.symbols_considered,
        result.counts.deprecated_symbols,
        result.counts.resolved_call_sites,
        result.counts.unresolved_call_edges
    );
    for d in &result.diagnostics {
        println!("diagnostic: {}: {}", d.code, d.detail);
    }
    println!("corpus: {corpus_mode}");
}
