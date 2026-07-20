use super::*;

// ---------------------------------------------------------------------------
// who-constructs query — `eg query who-constructs <Type>` (issue #471)
//
// The inbound, type-anchored mirror of the outbound `deps` (#123) lane and the
// symmetric partner to `who-imports` (#444). Resolves a TYPE handle (exact name
// or canonical record ID via the shared `resolve_failure_handle`) and lists
// every live inbound `CONSTRUCTS` edge (issue #443 / PR #467) — the struct/enum
// construction sites — as an NDJSON envelope + one row per constructing symbol.
// Each row carries the E0063 blast-radius `e0063_risk` flag. This is the
// first-class form of the `construction_sites` group `eg query change-impact`
// already surfaces; both read the same edges and expose the same signal.
// ---------------------------------------------------------------------------

/// One constructing-symbol row emitted as its own NDJSON line.
#[derive(Serialize)]
pub(crate) struct WhoConstructsRowJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    edge_record_id: &'a str,
    /// E0063 blast-radius flag (issue #443): `true` when adding a required field
    /// to the anchor type would break this site (the exhaustive, non-`..base`
    /// form); `false` when the site's `..base` FRU keeps it valid. Derived as
    /// `is_exhaustive.unwrap_or(true)` — a missing marker is conservatively risky.
    e0063_risk: bool,
    /// The raw exhaustiveness marker recorded on the edge; absent on a legacy
    /// edge that predates the marker (`e0063_risk` still defaults it to risky).
    #[serde(skip_serializing_if = "Option::is_none")]
    is_exhaustive: Option<bool>,
    trust: &'static str,
}

/// The queried type's own citable handle in the summary envelope.
#[derive(Serialize)]
pub(crate) struct WhoConstructsTargetJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
pub(crate) struct WhoConstructsHeaderJson<'a> {
    ok: bool,
    handle: &'a str,
    target: WhoConstructsTargetJson<'a>,
    direction: &'static str,
    edge_label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    at_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    total_constructors: usize,
    /// Corpus the current-state view read (issue #427):
    /// `head_anchored`/`union`/`commit_pinned`/`single_snapshot`.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `default`/`explicit_flag`/`selector`.
    corpus_mode_source: &'static str,
    /// One-line human description of what the corpus includes.
    corpus_disclaimer: &'static str,
    disclaimer: &'static str,
}

pub(crate) const WHO_CONSTRUCTS_DISCLAIMER: &str = "Rows are the symbols whose bodies build a `Type { … }` literal of the queried type, from the \
     extractor-minted CONSTRUCTS edges (a provably-resolved struct/enum construction site). \
     Construction is a caller-granularity relation: per-site spans collapse to the constructing \
     symbol, exactly as CALLS does. `e0063_risk: true` means at least one collapsed site uses the \
     exhaustive, non-`..base` form that fails to compile (rustc E0063) when a required field is \
     added; `false` means every site used struct-update `..base` (FRU), which stays valid. Rows \
     are construction-site LEADS and the `e0063_risk` flag an actionable signal, never proof a \
     specific field addition breaks.";

pub(crate) const fn who_constructs_row_json<'a>(
    row: &query::WhoConstructsRow<'a>,
) -> WhoConstructsRowJson<'a> {
    WhoConstructsRowJson {
        record_id: row.record_id,
        schema_version: row.schema_version,
        name: row.name,
        kind: row.kind,
        repo_relative_path: row.repo_relative_path,
        span: row.span,
        edge_record_id: row.edge_id,
        e0063_risk: row.e0063_risk,
        is_exhaustive: row.is_exhaustive,
        trust: "source_fact",
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools
)]
pub(crate) fn query_who_constructs_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at: Option<&str>,
    as_of: Option<&str>,
    at_head: bool,
    all_history: bool,
    format: OutputFormat,
) -> Result<()> {
    // A blank handle is a malformed query (exit 1), distinct from a well-formed
    // type that simply has no constructors (no_match, exit 2).
    if handle.trim().is_empty() {
        let diag = serde_json::json!({
            "code": "malformed_type_handle",
            "handle": handle,
            "message": "type handle must be a non-empty type name or canonical record ID",
        });
        eprintln!("{diag}");
        std::process::exit(1);
    }

    // ── corpus-mode selection (issue #427) ────────────────────────────────────
    // Current-state code lane: defaults to HEAD-anchoring (records current at
    // each repository's stamped `source_snapshot` HEAD) when a snapshot exists;
    // `--all-history` opts into the union of all commit snapshots and `--at-head`
    // makes the default explicit. `--at`/`--as-of` pin a single commit. Mirrors
    // `deps`. All four inputs run through the shared `resolve_corpus_mode`.
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

    // ── temporal / head-anchor narrowing (mirrors deps) ───────────────────────
    let mut at_commit: Option<String> = None;
    let filtered: Option<Vec<GraphRecord>> =
        if matches!(corpus_mode, query::CorpusMode::CommitPinned) {
            let sha = resolve_transitive_commit_view(records, index, repo_scope, at, as_of)?;
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
            // HEAD BEFORE resolution/traversal, so a constructor (or the edge, or
            // the type) removed at HEAD does not appear.
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

    // ── handle resolution (type name or canonical record ID) ──────────────────
    // Reuses the shared symbol handle resolver so a record ID / exact name
    // resolves identically to `deps`/`change-impact`.
    let target = match query::resolve_failure_handle(records, handle, index, repo_scope) {
        Ok(t) => t,
        Err(
            err @ (query::FailureHandleError::Ambiguous { .. }
            | query::FailureHandleError::Unsupported { .. }),
        ) => {
            eprintln!("{}", serde_json::to_string(&err)?);
            std::process::exit(1);
        }
    };

    // A type is a code Symbol. Task/Source/File handles are unsupported here:
    // CONSTRUCTS edges target the type's definition Symbol, so a file path or
    // task handle can never be the anchor.
    if matches!(
        target.kind,
        query::FailureTargetKind::Task
            | query::FailureTargetKind::Source
            | query::FailureTargetKind::File
    ) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {} target; who-constructs accepts only a type name or \
                 canonical record ID (the struct/enum definition symbol)",
                target.kind.as_str()
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }

    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    // A name matching more than one live type is ambiguous for this verb:
    // merging the constructors of unrelated same-named types would blend their
    // construction sets. All candidate record IDs are reported (mirrors deps).
    if target.anchor_ids.len() > 1 {
        let err = query::FailureHandleError::Ambiguous {
            handle: handle.to_owned(),
            candidates: target.anchor_ids.iter().cloned().collect(),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    let anchor_id = target
        .anchor_ids
        .iter()
        .next()
        .expect("non-empty target has an anchor");

    let Some(result) = query::who_constructs(records, anchor_id) else {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    };

    // A resolved type with zero live constructors is an explicit `no_match`
    // success-shaped diagnostic (exit 2), mirroring `who-imports`.
    if result.is_empty() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "handle": handle,
                "message": "the type resolved but no live symbol constructs it",
            },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let rows_json: Vec<WhoConstructsRowJson<'_>> =
        result.rows.iter().map(who_constructs_row_json).collect();

    let GraphRecord::Node {
        id: target_id,
        kind: target_kind,
        schema_version: target_schema_version,
        name: target_name,
        repo_relative_path: target_path,
        span: target_span,
        ..
    } = result.anchor
    else {
        anyhow::bail!("resolved anchor is not a node record");
    };

    let header = WhoConstructsHeaderJson {
        ok: true,
        handle,
        target: WhoConstructsTargetJson {
            record_id: target_id,
            schema_version: *target_schema_version,
            name: target_name.as_deref(),
            kind: target_kind.as_str(),
            repo_relative_path: target_path.as_deref(),
            span: *target_span,
        },
        direction: "inbound",
        edge_label: "CONSTRUCTS",
        at_commit: at_commit.as_deref(),
        as_of,
        total_constructors: rows_json.len(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer: corpus_mode.disclaimer(),
        disclaimer: WHO_CONSTRUCTS_DISCLAIMER,
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header)
                    .context("failed to serialize who-constructs header")?
            );
            for row in &rows_json {
                println!(
                    "{}",
                    serde_json::to_string(row).context("failed to serialize who-constructs row")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            println!(
                "constructors of {} ({target_id}): {} — construction-site leads, not proof a field addition breaks",
                target_name.as_deref().unwrap_or("?"),
                rows_json.len(),
            );
            for row in &rows_json {
                let location = row.repo_relative_path.map_or_else(String::new, |p| {
                    row.span
                        .map_or_else(|| format!(" {p}"), |s| format!(" {p}:{}", s.start_line))
                });
                let risk = if row.e0063_risk {
                    "e0063_risk"
                } else {
                    "fru_safe"
                };
                println!("  {}{location}  [{risk}]", row.name.unwrap_or("?"));
            }
        }
    }
    Ok(())
}
