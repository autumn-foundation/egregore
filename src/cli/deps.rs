use super::*;

// ---------------------------------------------------------------------------
// deps query (issue #123)
// ---------------------------------------------------------------------------

/// One resolved dependency row in the deps output.
#[derive(Serialize)]
pub(crate) struct DepsRowJson<'a> {
    category: &'static str,
    relation: &'a str,
    record_id: &'a str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    edge_record_id: &'a str,
    trust: &'static str,
}

/// One unresolved-target row in the deps output (AC3): the edge is reported,
/// never silently dropped, with whatever citable handle exists.
#[derive(Serialize)]
pub(crate) struct DepsUnresolvedRowJson<'a> {
    category: &'static str,
    relation: &'a str,
    reason: &'static str,
    /// The Diagnostic marker's record ID, when the callee marker is in-graph.
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    /// Callee display name recorded by the unresolved-call marker.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    /// Call-site path/span from the marker, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    /// The raw target record ID carried by the edge.
    target_record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    edge_record_id: &'a str,
    trust: &'static str,
}

/// The queried symbol's own citable handle in the summary envelope.
#[derive(Serialize)]
pub(crate) struct DepsTargetJson<'a> {
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
pub(crate) struct DepsHeaderJson<'a> {
    ok: bool,
    handle: &'a str,
    target: DepsTargetJson<'a>,
    direction: &'static str,
    edge_labels: [&'static str; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    at_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    total_dependencies: usize,
    total_unresolved: usize,
    /// Corpus the current-state view read (issue #427):
    /// `head_anchored`/`union`/`commit_pinned`/`single_snapshot`.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: `default`/`explicit_flag`/`selector`.
    corpus_mode_source: &'static str,
    /// One-line human description of what the corpus includes.
    corpus_disclaimer: &'static str,
    disclaimer: &'static str,
    diagnostics: Vec<AuditDiagnostic<'a>>,
}

pub(crate) const DEPS_DISCLAIMER: &str = "Rows are graph-derived dependency LEADS: an outbound edge exists in the graph. They are \
     not proof that a dependency is exercised at runtime or that the list is complete; dynamic \
     dispatch, macro-generated calls, and cross-crate targets are outside the extraction \
     contract, and absence of an edge is not proof of independence.";

pub(crate) fn deps_row_json<'a>(row: &query::SymbolDependencyRow<'a>) -> Option<DepsRowJson<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        ..
    } = row.record
    else {
        return None;
    };
    Some(DepsRowJson {
        category: "dependency",
        relation: row.relation,
        record_id: id,
        schema_version: *schema_version,
        name: name.as_deref(),
        kind: kind.as_str(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        resolution: row.resolution.map(CallResolution::as_str),
        edge_record_id: row.edge_id,
        trust: "dependency_lead",
    })
}

pub(crate) fn deps_unresolved_row_json<'a>(
    row: &query::UnresolvedDependencyRow<'a>,
) -> DepsUnresolvedRowJson<'a> {
    let marker = row.diagnostic.and_then(|d| match d {
        GraphRecord::Node {
            id,
            kind,
            name,
            repo_relative_path,
            span,
            ..
        } => Some((
            id.as_str(),
            kind.as_str(),
            name.as_deref(),
            repo_relative_path.as_deref(),
            *span,
        )),
        _ => None,
    });
    DepsUnresolvedRowJson {
        category: "unresolved",
        relation: row.relation,
        reason: row.reason.as_str(),
        record_id: marker.map(|m| m.0),
        name: marker.and_then(|m| m.2),
        kind: marker.map(|m| m.1),
        repo_relative_path: marker.and_then(|m| m.3),
        span: marker.and_then(|m| m.4),
        target_record_id: row.target_id,
        resolution: row.resolution.map(CallResolution::as_str),
        edge_record_id: row.edge_id,
        trust: "dependency_lead",
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools
)]
pub(crate) fn query_deps_cmd(
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
    // ── corpus-mode selection (issue #427) ────────────────────────────────────
    // Current-state code lanes default to HEAD-anchoring (records current at
    // each repository's stamped `source_snapshot` HEAD) when a snapshot exists;
    // `--all-history` opts into the union of all commit snapshots and `--at-head`
    // makes the default explicit. `--at`/`--as-of` still pin a single commit.
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

    // ── temporal narrowing: one commit's snapshot view (issue #123 AC4) ───────
    // Reuses the transitive-callers commit-view resolver: both verbs answer
    // from the same single-commit history slice.
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
            // Head-anchor: drop every record not current at its owning repository's
            // stamped HEAD BEFORE traversal, so a dependency (edge or target) removed
            // at HEAD does not appear. Snapshot-less stores never reach here (the
            // default resolves to SingleSnapshot). See `query::non_head_current_record_ids`.
            let non_current = query::non_head_current_record_ids(records, index);
            let view: Vec<GraphRecord> = records
                .iter()
                .filter(|r| !non_current.contains(r.id()))
                .cloned()
                .collect();
            Some(view)
        } else {
            // Union / SingleSnapshot: keep the full record set; `symbol_dependencies`
            // applies its keep-last-per-id / latest-edge-version liveness itself.
            None
        };
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    // ── handle resolution (symbol record ID or exact symbol name only) ────────
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

    if matches!(
        target.kind,
        query::FailureTargetKind::Task | query::FailureTargetKind::Source
    ) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {} target; deps accepts only symbol handles",
                target.kind.as_str()
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    if matches!(target.kind, query::FailureTargetKind::File) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: "handle resolved to a file; deps accepts only symbol handles \
                      (use `eg query file` for a file's defined symbols)"
                .to_owned(),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    if let Some(kind) = query::symbol_dependencies_non_symbol_anchor_kind(records, &target) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {kind:?} node; deps accepts only symbol handles"
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

    // A name matching more than one live symbol is ambiguous for this verb:
    // merging the outbound edges of unrelated same-name symbols would blend
    // their dependency sets (issue #123 AC5). All candidate record IDs are
    // reported so the caller can re-run with one of them.
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

    let Some(ctx) = query::symbol_dependencies(records, anchor_id) else {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    };

    // ── diagnostics: redaction gate over every returned dependency record ─────
    let mut diagnostics: Vec<AuditDiagnostic<'_>> = Vec::new();
    for row in &ctx.dependencies {
        protected_payload_diagnostics(row.record, &mut diagnostics);
    }
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.source_record_id.cmp(b.source_record_id))
            .then_with(|| a.target_handle.cmp(b.target_handle))
            .then_with(|| a.relation.cmp(b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    let rows_json: Vec<DepsRowJson<'_>> =
        ctx.dependencies.iter().filter_map(deps_row_json).collect();
    let unresolved_json: Vec<DepsUnresolvedRowJson<'_>> = ctx
        .unresolved
        .iter()
        .map(deps_unresolved_row_json)
        .collect();

    let GraphRecord::Node {
        id: target_id,
        kind: target_kind,
        schema_version: target_schema_version,
        name: target_name,
        repo_relative_path: target_path,
        span: target_span,
        ..
    } = ctx.anchor
    else {
        anyhow::bail!("resolved anchor is not a node record");
    };

    let header = DepsHeaderJson {
        ok: true,
        handle,
        target: DepsTargetJson {
            record_id: target_id,
            schema_version: *target_schema_version,
            name: target_name.as_deref(),
            kind: target_kind.as_str(),
            repo_relative_path: target_path.as_deref(),
            span: *target_span,
        },
        direction: "outbound",
        edge_labels: ["CALLS", "IMPLEMENTS", "IMPORTS", "REFERENCES"],
        at_commit: at_commit.as_deref(),
        as_of,
        total_dependencies: rows_json.len(),
        total_unresolved: unresolved_json.len(),
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer: corpus_mode.disclaimer(),
        disclaimer: DEPS_DISCLAIMER,
        diagnostics,
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header).context("failed to serialize deps header")?
            );
            for row in &rows_json {
                println!(
                    "{}",
                    serde_json::to_string(row).context("failed to serialize deps row")?
                );
            }
            for row in &unresolved_json {
                println!(
                    "{}",
                    serde_json::to_string(row)
                        .context("failed to serialize deps unresolved row")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            println!(
                "dependencies of {} ({target_id}) total={} unresolved={} — dependency leads, not proof of runtime behavior",
                target_name.as_deref().unwrap_or("?"),
                rows_json.len(),
                unresolved_json.len(),
            );
            for row in &rows_json {
                let location = row.repo_relative_path.map_or_else(String::new, |p| {
                    row.span
                        .map_or_else(|| format!(" {p}"), |s| format!(" {p}:{}", s.start_line))
                });
                let res = row
                    .resolution
                    .map_or_else(String::new, |r| format!(" [resolution={r}]"));
                println!(
                    "{} {}{location}{res}",
                    row.relation,
                    row.name.unwrap_or("?")
                );
            }
            for row in &unresolved_json {
                let location = row.repo_relative_path.map_or_else(String::new, |p| {
                    row.span
                        .map_or_else(|| format!(" {p}"), |s| format!(" {p}:{}", s.start_line))
                });
                println!(
                    "unresolved {} {}{location} ({})",
                    row.relation,
                    row.name.unwrap_or(row.target_record_id),
                    row.reason,
                );
            }
        }
    }
    Ok(())
}
