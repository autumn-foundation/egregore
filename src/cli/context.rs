use super::*;

use super::record_budget::RecordBudget;

/// Renders a resolved [`query::SymbolContext`] into the serializable section
/// views, reusing the existing per-record builders (`context_source_fact`,
/// `context_observation`, `context_linked_item`). `.copied()` collapses the
/// `&&GraphRecord` from `iter()` so each view borrows the record slice directly.
pub(crate) fn build_context_sections<'a>(
    records: &'a [GraphRecord],
    ctx: &'a query::SymbolContext<'a>,
    trust: &query::TrustIndex<'_>,
) -> ContextSections<'a> {
    ContextSections {
        source_facts: ctx
            .source_facts
            .iter()
            .copied()
            .filter_map(|r| context_source_fact(r, trust))
            .collect(),
        topology_edges: ctx
            .topology_edges
            .iter()
            .copied()
            .filter_map(|r| {
                if let GraphRecord::Edge {
                    id,
                    label,
                    source,
                    target,
                    summary,
                    temporal,
                    ..
                } = r
                {
                    Some(ContextTopologyEdge {
                        record_id: id,
                        trust: trust.classify(r),
                        label: label.as_str(),
                        source_id: source,
                        target_id: target,
                        summary,
                        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
                        valid_time: temporal.as_ref().map(|t| t.valid_time.as_str()),
                    })
                } else {
                    None
                }
            })
            .collect(),
        observations: ctx
            .observations
            .iter()
            .copied()
            .filter_map(|r| context_observation(r, trust))
            .collect(),
        // Section contract (issue #191): decisions surface in their own
        // section and never in `observations`.
        decisions: ctx
            .decisions
            .iter()
            .copied()
            .filter_map(|r| context_decision(r, records, trust))
            .collect(),
        project_state: ctx
            .project_state
            .iter()
            .copied()
            .filter_map(|r| context_linked_item(r, trust))
            .collect(),
        artifacts: ctx
            .artifacts
            .iter()
            .copied()
            .filter_map(|r| context_linked_item(r, trust))
            .collect(),
        verification_evidence: ctx
            .verification_evidence
            .iter()
            .copied()
            .filter_map(|r| context_linked_item(r, trust))
            .collect(),
        unresolved: ctx
            .unresolved
            .iter()
            .map(|u| ContextUnresolved {
                source_record_id: &u.source_record_id,
                target_handle: &u.target_handle,
                relation: &u.relation,
                target_domain: &u.target_domain,
                verification_status: "unresolved",
            })
            .collect(),
        // Issue #169: fold active approved policy into the context answer.
        // Always present (possibly empty) — a context with no applicable
        // policy carries an explicit empty `policy` section.
        policy: ctx
            .policy
            .iter()
            .map(|p| ContextPolicyRow {
                record_id: p.record_id(),
                kind: p.kind.as_str(),
                body: p.body,
                approval_decision_id: p.approval_decision_id,
                active_from: p.active_from,
                status: p.status.as_str(),
                trust: trust.classify(p.record),
                scope_repo: p.scope.repo.as_deref(),
                scope_path_glob: p.scope.path_glob.as_deref(),
                scope_language: p.scope.language.as_deref(),
                scope_lifecycle_phase: p.scope.lifecycle_phase.as_deref(),
            })
            .collect(),
    }
}

/// A context row that can carry temporal-supersession flags (issue #191:
/// decisions need the same supersession handling as observations).
pub(crate) trait SupersessionRow<'a> {
    fn record_id(&self) -> &'a str;
    fn trust(&self) -> crate::query::TrustClass;
    fn set_temporal_status(&mut self, status: String);
    fn set_superseded_by(&mut self, ids: Option<Vec<crate::temporal_status::TemporalReference>>);
    fn set_contradicted_by(&mut self, ids: Option<Vec<crate::temporal_status::TemporalReference>>);
}

impl<'a> SupersessionRow<'a> for query::ContextObservation<'a> {
    fn record_id(&self) -> &'a str {
        self.record_id
    }
    fn trust(&self) -> crate::query::TrustClass {
        self.trust
    }
    fn set_temporal_status(&mut self, status: String) {
        self.temporal_status = Some(status);
    }
    fn set_superseded_by(&mut self, ids: Option<Vec<crate::temporal_status::TemporalReference>>) {
        self.superseded_by = ids;
    }
    fn set_contradicted_by(&mut self, ids: Option<Vec<crate::temporal_status::TemporalReference>>) {
        self.contradicted_by = ids;
    }
}

impl<'a> SupersessionRow<'a> for query::ContextDecision<'a> {
    fn record_id(&self) -> &'a str {
        self.record_id
    }
    fn trust(&self) -> crate::query::TrustClass {
        self.trust
    }
    fn set_temporal_status(&mut self, status: String) {
        self.temporal_status = Some(status);
    }
    fn set_superseded_by(&mut self, ids: Option<Vec<crate::temporal_status::TemporalReference>>) {
        self.superseded_by = ids;
    }
    fn set_contradicted_by(&mut self, ids: Option<Vec<crate::temporal_status::TemporalReference>>) {
        self.contradicted_by = ids;
    }
}

pub(crate) fn apply_supersession<'a, R: SupersessionRow<'a>>(
    rows: Vec<R>,
    resolver: &crate::temporal_status::TemporalResolver<'a>,
    mode: crate::temporal_status::SupersessionMode,
) -> (Vec<R>, Vec<ExcludedDiagnostic<'a>>) {
    let mut filtered = Vec::new();
    let mut excluded = Vec::new();

    for mut row in rows {
        let (status, superseded_by, contradicted_by) = resolver.resolve_status(row.record_id());

        let is_superseded = status == "superseded" || status == "cycle";
        let is_contradicted = status == "contradicted";

        if is_superseded || is_contradicted {
            let reason = if is_superseded {
                "superseded"
            } else {
                "contradicted"
            };
            match mode {
                crate::temporal_status::SupersessionMode::Exclude => {
                    excluded.push(ExcludedDiagnostic {
                        record_id: row.record_id(),
                        trust: row.trust(),
                        reason,
                        superseded_by: if superseded_by.is_empty() {
                            None
                        } else {
                            Some(superseded_by)
                        },
                        contradicted_by: if contradicted_by.is_empty() {
                            None
                        } else {
                            Some(contradicted_by)
                        },
                    });
                }
                crate::temporal_status::SupersessionMode::IncludeButFlag => {
                    row.set_temporal_status(status.to_string());
                    row.set_superseded_by(if superseded_by.is_empty() {
                        None
                    } else {
                        Some(superseded_by)
                    });
                    row.set_contradicted_by(if contradicted_by.is_empty() {
                        None
                    } else {
                        Some(contradicted_by)
                    });
                    filtered.push(row);
                }
            }
        } else {
            match mode {
                crate::temporal_status::SupersessionMode::IncludeButFlag => {
                    row.set_temporal_status(status.to_string());
                    filtered.push(row);
                }
                crate::temporal_status::SupersessionMode::Exclude => {
                    filtered.push(row);
                }
            }
        }
    }

    (filtered, excluded)
}

/// Issue #192: resolve the symbol context for a name, reporting ambiguity
/// instead of merging when the name matches distinct symbols. When
/// `candidate` is `Some`, re-query for exactly that identity (a stable
/// record ID or a `file:span` handle); otherwise look the name up directly.
/// Exits the process with a JSON error envelope when the recall is
/// ambiguous or matches nothing.
fn resolve_symbol_or_candidate<'a>(
    records: &'a [GraphRecord],
    symbol_name: &'a str,
    candidate: Option<&'a str>,
) -> Result<(query::SymbolContext<'a>, &'a str)> {
    if let Some(selector) = candidate {
        let anchor = records
            .iter()
            .find(|r| {
                r.id() == selector
                    && matches!(
                        r,
                        GraphRecord::Node {
                            kind: NodeKind::Symbol,
                            ..
                        }
                    )
            })
            .or_else(|| query::resolve_symbol_file_span_handle(records, selector));
        let Some(anchor) = anchor else {
            emit_no_match_candidate(selector)?;
            std::process::exit(2);
        };
        let ctx = query::record_context(records, anchor.id());
        if ctx.is_no_match() {
            emit_no_match_candidate(selector)?;
            std::process::exit(2);
        }
        let name = match anchor {
            GraphRecord::Node { name: Some(n), .. } => n.as_str(),
            _ => selector,
        };
        return Ok((ctx, name));
    }
    let ctx = query::symbol_context(records, symbol_name);
    if ctx.is_ambiguous() {
        let candidates: Vec<serde_json::Value> = ctx
            .candidates
            .iter()
            .map(|c| {
                serde_json::json!({
                    "record_id": c.record_id,
                    "symbol_name": c.symbol_name,
                    "repo_relative_path": c.repo_relative_path,
                    "span": c.span,
                    "file_span_handle": c.file_span_handle(),
                })
            })
            .collect();
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "ambiguous_symbol",
                "symbol_name": symbol_name,
                "message": "the name matches more than one distinct symbol; re-run with --candidate <record_id|file:span handle>",
                "candidates": candidates,
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(1);
    }
    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "symbol_name": symbol_name
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }
    Ok((ctx, symbol_name))
}

/// Emit the `no_match` envelope for an unresolvable `--candidate` selector.
fn emit_no_match_candidate(selector: &str) -> Result<()> {
    let envelope = serde_json::json!({
        "ok": false,
        "error": {
            "code": "no_match",
            "candidate": selector,
        }
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_context_cmd(
    records: &[GraphRecord],
    symbol_name: &str,
    freshness: Option<(String, &'static str)>,
    supersession: crate::temporal_status::SupersessionMode,
    at_head: bool,
    all_history: bool,
    max_records: Option<usize>,
    store_coverage: query::StoreCoverage,
    candidate: Option<&str>,
) -> Result<()> {
    // Corpus-mode selection (issue #456): head-anchor by default over a
    // scan-history store; `--all-history` opts into the union. Pre-filter drops
    // off-HEAD records BEFORE building the cross-domain context bundle.
    let index = query::RepositoryIndex::build(records);
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, &index, false, at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    // Issue #192: a name shared by distinct symbols is reported, never merged;
    // `--candidate` re-queries for exactly one of them.
    let (ctx, display_name) = resolve_symbol_or_candidate(records, symbol_name, candidate)?;

    let trust = query::TrustIndex::build(records);
    let sections = build_context_sections(records, &ctx, &trust);

    // Attach the freshness verdict only when every source fact belongs to the
    // repository the verdict was computed for (PR #186): `query context` has no
    // repository selector, so in a multi-repo store the same symbol can collect
    // facts from several repositories — presenting one checkout's verdict across
    // all of them would be misleading. Omit it when the response spans repos.
    let freshness_code = freshness.and_then(|(owner_id, code)| {
        let index = query::RepositoryIndex::build(records);
        let owners: std::collections::BTreeSet<Option<&str>> = ctx
            .source_facts
            .iter()
            .map(|record| index.owner_of(record.id()))
            .collect();
        (owners.len() == 1 && owners.contains(&Some(owner_id.as_str()))).then_some(code)
    });

    // Reuse the resolver the trust index already built over this exact slice
    // (one O(n) build per answer) so `trust` and `temporal_status` can never be
    // computed from different corpora.
    let (observations, mut excluded) =
        apply_supersession(sections.observations, trust.resolver(), supersession);

    // Decisions get their own supersession pass (issue #191): same
    // temporal-status semantics, surfaced in the decisions section.
    let (decisions, decision_excluded) =
        apply_supersession(sections.decisions, trust.resolver(), supersession);
    excluded.extend(decision_excluded);

    let resolved_drift_targets = query::resolve_drift_targets(records, &ctx.drift_history);
    let drift_history: Vec<ContextDrift<'_>> = ctx
        .drift_history
        .iter()
        .copied()
        .zip(resolved_drift_targets)
        .filter_map(|(record, resolved)| context_drift(record, resolved, &trust))
        .collect();

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    // Record budget (issue #211): sections fill sequentially in envelope
    // order; each keeps its top-ranked prefix and the remainder flows on.
    let mut budget = RecordBudget::new(max_records);
    let response = ContextResponse {
        ok: true,
        symbol_name: display_name,
        freshness: freshness_code,
        source_facts: budget.section(sections.source_facts),
        topology_edges: budget.section(sections.topology_edges),
        observations: budget.section(observations),
        decisions: budget.section(decisions),
        project_state: budget.section(sections.project_state),
        artifacts: budget.section(sections.artifacts),
        verification_evidence: budget.section(sections.verification_evidence),
        drift_history: budget.section(drift_history),
        unresolved: budget.section(sections.unresolved),
        // Issue #169: policy folds into the single-symbol context answer,
        // budgeted like every other section; always present (possibly empty).
        policy: budget.section(sections.policy),
        excluded,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
        store_coverage,
    };

    let output = serde_json::to_string_pretty(&response).context("failed to serialize context")?;
    println!("{output}");
    Ok(())
}

/// Builds one `drift_history` row from a `SemanticDrift` record (issue #108),
/// given its already-resolved target `(repo_relative_path, name, span)` — the
/// same shape `query::resolve_drift_target`/`resolve_drift_targets` return —
/// so the citation audit's classification of this row matches what actually
/// gets rendered. Callers resolve targets for the whole `drift_history` slice
/// in one batched pass via `query::resolve_drift_targets` (issue #497 Codex
/// review: resolving one row at a time made rendering O(D×N)) rather than
/// calling `resolve_drift_target` here per row.
///
/// Returns `None` for a non-drift record — defensive against a future caller
/// passing the wrong slice; `ctx.drift_history` only ever contains
/// `SemanticDrift` nodes by construction.
pub(crate) fn context_drift<'a>(
    record: &'a GraphRecord,
    resolved: (
        Option<&'a str>,
        Option<&'a str>,
        Option<crate::ir::SourceSpan>,
    ),
    trust: &query::TrustIndex<'_>,
) -> Option<ContextDrift<'a>> {
    let GraphRecord::Node {
        id,
        semantic_drift: Some(drift),
        ..
    } = record
    else {
        return None;
    };
    let (resolved_path, _resolved_name, resolved_span) = resolved;
    Some(ContextDrift {
        record_id: id,
        trust: trust.classify(record),
        score: drift.score,
        before_commit: &drift.before_git_commit,
        after_commit: &drift.after_git_commit,
        before_valid_time: &drift.before_valid_time,
        after_valid_time: &drift.after_valid_time,
        embedding_model: &drift.embedding_model,
        repo_relative_path: resolved_path,
        span: resolved_span,
    })
}

pub(crate) fn context_source_fact<'a>(
    record: &'a GraphRecord,
    trust: &query::TrustIndex<'_>,
) -> Option<ContextSourceFact<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        language,
        symbol_kind,
        ..
    } = record
    else {
        return None;
    };
    Some(ContextSourceFact {
        record_id: id,
        kind: kind.as_str(),
        trust: trust.classify(record),
        name: name.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        // For current-tree records valid_time is at the node level; for
        // scan-history records it is in temporal.valid_time. Prefer the
        // node-level field and fall back to the temporal block.
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        language: language.as_deref(),
        symbol_kind: symbol_kind.as_deref(),
    })
}
