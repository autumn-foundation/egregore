use super::*;

// ---------------------------------------------------------------------------
// transitive-callees query (issue #253) — the outbound mirror of #139.
// ---------------------------------------------------------------------------

/// One hop of a connecting dependency path in the transitive-callees output.
#[derive(Serialize)]
pub(crate) struct TransitiveCalleePathStepJson<'a> {
    source_record_id: &'a str,
    edge_record_id: &'a str,
    edge_label: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    target_record_id: &'a str,
}

/// One reachable row in the transitive-callees output.
#[derive(Serialize)]
pub(crate) struct TransitiveCalleeRowJson<'a> {
    category: &'static str,
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
    hop: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_resolution: Option<&'static str>,
    path: Vec<TransitiveCalleePathStepJson<'a>>,
    trust: &'static str,
}

/// One unresolved-target row in the transitive-callees output (AC4): the edge
/// is reported, never silently dropped, with whatever citable handle exists.
#[derive(Serialize)]
pub(crate) struct TransitiveCalleeUnresolvedRowJson<'a> {
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
    /// The reachable node this unresolved edge departs from.
    source_record_id: &'a str,
    /// Hop distance of the source node from the anchor (0 = the anchor).
    source_hop: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    edge_record_id: &'a str,
    trust: &'static str,
}

/// The queried target's own citable handle in the summary envelope.
#[derive(Serialize)]
pub(crate) struct TransitiveCalleeTargetJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
}

/// Dropped-frontier count at one depth beyond the bound.
#[derive(Serialize)]
pub(crate) struct TransitiveCalleeDroppedDepthJson {
    depth: usize,
    count: usize,
}

/// Depth-bound truncation diagnostic.
#[derive(Serialize)]
pub(crate) struct TransitiveCalleeTruncationJson {
    code: &'static str,
    max_depth: usize,
    dropped_frontier: Vec<TransitiveCalleeDroppedDepthJson>,
    dropped_total: usize,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
pub(crate) struct TransitiveCalleesHeaderJson<'a> {
    ok: bool,
    handle: &'a str,
    target: TransitiveCalleeTargetJson<'a>,
    direction: &'static str,
    edge_labels: [&'static str; 4],
    max_depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    at_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<&'a str>,
    total_reachable: usize,
    total_unresolved: usize,
    disclaimer: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation: Option<TransitiveCalleeTruncationJson>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
}

pub(crate) const TRANSITIVE_CALLEES_DISCLAIMER: &str = "Rows are reachability LEADS: an outbound dependency path exists in the graph. They are not \
     proof that a reachable symbol is exercised at runtime, that a change will break it, or that \
     the list is complete; dynamic dispatch, macro-generated calls, FFI, and cross-crate targets \
     are outside the extraction contract, and absence of a path is not proof of independence.";

pub(crate) fn transitive_callee_row_json<'a>(
    row: &query::TransitiveCalleeRow<'a>,
) -> Option<TransitiveCalleeRowJson<'a>> {
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
    Some(TransitiveCalleeRowJson {
        category: "reachable",
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
        hop: row.hop,
        path_resolution: row.path_resolution.map(CallResolution::as_str),
        path: row
            .path
            .iter()
            .map(|s| TransitiveCalleePathStepJson {
                source_record_id: s.source_record_id,
                edge_record_id: s.edge_record_id,
                edge_label: s.edge_label,
                resolution: s.resolution.map(CallResolution::as_str),
                target_record_id: s.target_record_id,
            })
            .collect(),
        trust: "reachability_lead",
    })
}

pub(crate) fn transitive_callee_unresolved_row_json<'a>(
    row: &query::UnresolvedCalleeRow<'a>,
) -> TransitiveCalleeUnresolvedRowJson<'a> {
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
    TransitiveCalleeUnresolvedRowJson {
        category: "unresolved",
        relation: row.relation,
        reason: row.reason.as_str(),
        record_id: marker.map(|m| m.0),
        name: marker.and_then(|m| m.2),
        kind: marker.map(|m| m.1),
        repo_relative_path: marker.and_then(|m| m.3),
        span: marker.and_then(|m| m.4),
        target_record_id: row.target_id,
        source_record_id: row.source_id,
        source_hop: row.source_hop,
        resolution: row.resolution.map(CallResolution::as_str),
        edge_record_id: row.edge_id,
        trust: "reachability_lead",
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn query_transitive_callees_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    max_depth: usize,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // ── temporal narrowing: one commit's snapshot view (issue #253 AC6) ───────
    // Reuses the transitive-callers commit-view resolver: both verbs answer
    // from the same single-commit history slice.
    let mut at_commit: Option<String> = None;
    let filtered: Option<Vec<GraphRecord>> = if at.is_some() || as_of.is_some() {
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
    } else {
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
                "handle resolved to a {} target; transitive-callees accepts only symbol handles",
                target.kind.as_str()
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    if matches!(target.kind, query::FailureTargetKind::File) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: "handle resolved to a file; transitive-callees accepts only symbol handles \
                      (use `eg query change-impact` for file-level blast radius)"
                .to_owned(),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }
    if let Some(kind) = query::transitive_callees_non_symbol_anchor_kind(records, &target) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {kind:?} node; transitive-callees accepts only symbol handles"
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
    // walking the union would bleed an unrelated same-name symbol's edges
    // into the target's reachability (AC7). All candidate record IDs are
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

    let Some(ctx) = query::transitive_callees(records, anchor_id, max_depth) else {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "handle": handle },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    };

    // ── diagnostics: redaction gate over reached rows ────────────────────────
    let mut diagnostics: Vec<AuditDiagnostic<'_>> = Vec::new();
    for row in &ctx.rows {
        protected_payload_diagnostics(row.record, &mut diagnostics);
    }
    for row in &ctx.unresolved {
        if let Some(rec) = row.diagnostic {
            protected_payload_diagnostics(rec, &mut diagnostics);
        }
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

    let rows_json: Vec<TransitiveCalleeRowJson<'_>> = ctx
        .rows
        .iter()
        .filter_map(transitive_callee_row_json)
        .collect();
    let unresolved_json: Vec<TransitiveCalleeUnresolvedRowJson<'_>> = ctx
        .unresolved
        .iter()
        .map(transitive_callee_unresolved_row_json)
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

    let header = TransitiveCalleesHeaderJson {
        ok: true,
        handle,
        target: TransitiveCalleeTargetJson {
            record_id: target_id,
            schema_version: *target_schema_version,
            name: target_name.as_deref(),
            kind: target_kind.as_str(),
            repo_relative_path: target_path.as_deref(),
            span: *target_span,
        },
        direction: "outbound",
        edge_labels: ["CALLS", "IMPLEMENTS", "IMPORTS", "REFERENCES"],
        max_depth: ctx.max_depth,
        at_commit: at_commit.as_deref(),
        as_of,
        total_reachable: rows_json.len(),
        total_unresolved: unresolved_json.len(),
        disclaimer: TRANSITIVE_CALLEES_DISCLAIMER,
        truncation: ctx
            .truncation
            .as_ref()
            .map(|t| TransitiveCalleeTruncationJson {
                code: "max_depth_truncated",
                max_depth: t.max_depth,
                dropped_frontier: t
                    .dropped_frontier
                    .iter()
                    .map(|d| TransitiveCalleeDroppedDepthJson {
                        depth: d.depth,
                        count: d.count,
                    })
                    .collect(),
                dropped_total: t.dropped_total,
            }),
        diagnostics,
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header)
                    .context("failed to serialize transitive-callees header")?
            );
            for row in &rows_json {
                println!(
                    "{}",
                    serde_json::to_string(row)
                        .context("failed to serialize transitive-callees row")?
                );
            }
            for row in &unresolved_json {
                println!(
                    "{}",
                    serde_json::to_string(row)
                        .context("failed to serialize transitive-callees unresolved row")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            let mut names: BTreeMap<&str, &str> = BTreeMap::new();
            names.insert(target_id.as_str(), target_name.as_deref().unwrap_or("?"));
            for row in &ctx.rows {
                if let GraphRecord::Node {
                    id, name: Some(n), ..
                } = row.record
                {
                    names.insert(id.as_str(), n.as_str());
                }
            }
            println!(
                "transitive callees of {} ({target_id}) max_depth={} total={} unresolved={} — reachability leads, not proof of breakage",
                target_name.as_deref().unwrap_or("?"),
                ctx.max_depth,
                rows_json.len(),
                unresolved_json.len(),
            );
            for row in &rows_json {
                let mut chain = String::new();
                chain.push_str(names.get(target_id.as_str()).copied().unwrap_or("?"));
                for step in &row.path {
                    let to = names.get(step.target_record_id).copied().unwrap_or("?");
                    chain.push_str(" -");
                    chain.push_str(step.edge_label);
                    chain.push_str("-> ");
                    chain.push_str(to);
                }
                let location = row.repo_relative_path.map_or_else(String::new, |p| {
                    row.span
                        .map_or_else(|| format!(" {p}"), |s| format!(" {p}:{}", s.start_line))
                });
                let res = row
                    .path_resolution
                    .map_or_else(String::new, |r| format!(" [resolution={r}]"));
                println!(
                    "{}{location} hop={} via {chain}{res}",
                    row.name.unwrap_or("?"),
                    row.hop,
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
            if let Some(t) = &ctx.truncation {
                println!(
                    "truncated at max_depth={}: {} reachable node(s) beyond the bound",
                    t.max_depth, t.dropped_total
                );
            }
        }
    }
    Ok(())
}
