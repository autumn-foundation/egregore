use super::*;

// ---------------------------------------------------------------------------
// change-impact query (issue #76)
// ---------------------------------------------------------------------------

/// One impact lead row in the change-impact response.
#[derive(Serialize)]
pub(crate) struct ImpactLeadJson<'a> {
    record_id: &'a str,
    kind: &'static str,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    /// Wire relation label (e.g. "CALLS", "REFERENCES", "IMPLEMENTS").
    relation: &'static str,
    /// "inbound" or "outbound" relative to the queried anchor.
    direction: &'static str,
    /// Stable edge record ID.
    edge_record_id: &'a str,
    /// Call resolution status ("resolved" / "ambiguous" / "unresolved") for
    /// CALLS edges labeled by the resolution passes (issues #152/#134);
    /// absent when the edge carries none. Lets agents filter to
    /// resolved-only call edges.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    edge_git_commit: Option<&'a str>,
    /// Which anchor record ID reached this lead.
    anchor_id: &'a str,
    /// Hop distance from the anchor (1-based).
    hop: usize,
    /// E0063 blast-radius flag on a `CONSTRUCTS` construction-site row (issue
    /// #443): `true` when the collapsed literal(s) include the exhaustive,
    /// non-`..base` form that breaks when a required field is added (default
    /// `true` when the edge marker is absent); `false` when every site used
    /// `..base`. Absent on non-construction rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    e0063_risk: Option<bool>,
    /// Every row is an impact LEAD — not proof of breakage (AC5).
    trust: &'static str,
}

/// One truncation record emitted when the per-group cap is hit.
#[derive(Serialize)]
pub(crate) struct ImpactTruncationJson {
    group: &'static str,
    returned: usize,
    total: usize,
    depth: usize,
}

/// Top-level change-impact response envelope.
#[derive(Serialize)]
pub(crate) struct ChangeImpactResponse<'a> {
    ok: bool,
    handle: &'a str,
    target_type: &'a str,
    target_ids: Vec<&'a str>,
    depth: usize,
    /// Per-response disclaimer: rows are LEADS, not proof (AC5).
    disclaimer: &'static str,
    /// Corpus the current-state view read (issue #427):
    /// `union` over a scan-history store, `single_snapshot` over a plain scan.
    corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    corpus_disclaimer: String,
    direct_callers: Vec<ImpactLeadJson<'a>>,
    direct_callees: Vec<ImpactLeadJson<'a>>,
    referencing_files: Vec<ImpactLeadJson<'a>>,
    implementation_symbols: Vec<ImpactLeadJson<'a>>,
    containing_context: Vec<ImpactLeadJson<'a>>,
    /// Struct-literal construction sites of the anchor type (issue #443); each
    /// row carries an `e0063_risk` flag. Always present (empty when none).
    construction_sites: Vec<ImpactLeadJson<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    truncations: Vec<ImpactTruncationJson>,
    diagnostics: Vec<AuditDiagnostic<'a>>,
    page: AuditPage,
}

pub(crate) const IMPACT_DISCLAIMER: &str = "Rows are impact LEADS to inspect before editing, not proof of breakage. \
     Absence of a lead is not proof a change is safe.";

pub(crate) fn impact_lead_json<'a>(lead: &'a query::ImpactLead<'a>) -> Option<ImpactLeadJson<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        symbol_kind,
        language,
        ..
    } = lead.record
    else {
        return None;
    };
    let edge_git_commit = if let GraphRecord::Edge {
        temporal: Some(t), ..
    } = lead.edge
    {
        Some(t.git_commit.as_str())
    } else {
        None
    };
    Some(ImpactLeadJson {
        record_id: id,
        kind: kind.as_str(),
        schema_version: *schema_version,
        name: name.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        symbol_kind: symbol_kind.as_deref(),
        language: language.as_deref(),
        relation: lead.relation,
        direction: lead.direction.as_str(),
        edge_record_id: lead.edge.id(),
        resolution: lead.edge.resolution().map(CallResolution::as_str),
        edge_git_commit,
        anchor_id: lead.anchor_id,
        hop: lead.hop,
        e0063_risk: (lead.relation == "CONSTRUCTS")
            .then(|| lead.edge.construct_is_exhaustive().unwrap_or(true)),
        trust: "impact_lead",
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) fn query_change_impact_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    depth: usize,
) -> Result<()> {
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

    // change-impact only operates on code handles (symbol or file). A handle that
    // resolves to a task or source/provenance record is out of scope and must be
    // rejected rather than misclassified as an empty symbol result.
    if matches!(
        target.kind,
        query::FailureTargetKind::Task | query::FailureTargetKind::Source
    ) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {} target; change-impact accepts only code symbol or file handles",
                target.kind.as_str()
            ),
        };
        eprintln!("{}", serde_json::to_string(&err)?);
        std::process::exit(1);
    }

    // A canonical codegraph ID can resolve to a non-File/Symbol node kind
    // (Repository, Module, Import, Commit, Change, …) while still mapping to a
    // `Symbol` target kind. Such handles are out of scope for change-impact and
    // must be rejected rather than traversed as an empty symbol result.
    if let Some(kind) = query::change_impact_unsupported_anchor_kind(records, &target) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {kind:?} node; change-impact accepts only code symbol or file handles"
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

    let ctx = query::change_impact_context(records, &target, depth, index, repo_scope);

    let mut diagnostics: Vec<AuditDiagnostic<'_>> = ctx
        .diagnostics
        .iter()
        .map(|d| AuditDiagnostic {
            code: &d.code,
            source_record_id: &d.source_record_id,
            target_handle: &d.target_handle,
            relation: &d.relation,
            target_domain: &d.target_domain,
        })
        .collect();

    // Run redaction gate over every reached code-graph record
    for lead in ctx
        .direct_callers
        .iter()
        .chain(&ctx.direct_callees)
        .chain(&ctx.referencing_files)
        .chain(&ctx.implementation_symbols)
        .chain(&ctx.containing_context)
        .chain(&ctx.construction_sites)
    {
        protected_payload_diagnostics(lead.record, &mut diagnostics);
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

    let total_returned = ctx.direct_callers.len()
        + ctx.direct_callees.len()
        + ctx.referencing_files.len()
        + ctx.implementation_symbols.len()
        + ctx.containing_context.len()
        + ctx.construction_sites.len();

    let truncations: Vec<ImpactTruncationJson> = ctx
        .truncations
        .iter()
        .map(|t| ImpactTruncationJson {
            group: t.group,
            returned: t.returned,
            total: t.total,
            depth: t.depth,
        })
        .collect();

    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        query::disclose_corpus(records, query::CorpusMode::Union);

    let response = ChangeImpactResponse {
        ok: true,
        handle,
        target_type: ctx.target_kind,
        target_ids: ctx.target_ids.iter().map(String::as_str).collect(),
        depth: ctx.depth,
        disclaimer: IMPACT_DISCLAIMER,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
        direct_callers: ctx
            .direct_callers
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        direct_callees: ctx
            .direct_callees
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        referencing_files: ctx
            .referencing_files
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        implementation_symbols: ctx
            .implementation_symbols
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        containing_context: ctx
            .containing_context
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        construction_sites: ctx
            .construction_sites
            .iter()
            .filter_map(impact_lead_json)
            .collect(),
        truncations,
        diagnostics,
        page: AuditPage {
            cursor: None,
            has_more: false,
            returned: total_returned,
        },
    };

    let output = serde_json::to_string_pretty(&response)
        .context("failed to serialize change-impact context")?;
    println!("{output}");
    Ok(())
}
