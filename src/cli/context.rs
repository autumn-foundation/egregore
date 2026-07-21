use super::*;

/// Renders a resolved [`query::SymbolContext`] into the serializable section
/// views, reusing the existing per-record builders (`context_source_fact`,
/// `context_observation`, `context_linked_item`). `.copied()` collapses the
/// `&&GraphRecord` from `iter()` so each view borrows the record slice directly.
pub(crate) fn build_context_sections<'a>(ctx: &'a query::SymbolContext<'a>) -> ContextSections<'a> {
    ContextSections {
        source_facts: ctx
            .source_facts
            .iter()
            .copied()
            .filter_map(context_source_fact)
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
            .filter_map(context_observation)
            .collect(),
        project_state: ctx
            .project_state
            .iter()
            .copied()
            .filter_map(context_linked_item)
            .collect(),
        artifacts: ctx
            .artifacts
            .iter()
            .copied()
            .filter_map(context_linked_item)
            .collect(),
        verification_evidence: ctx
            .verification_evidence
            .iter()
            .copied()
            .filter_map(context_linked_item)
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
    }
}

pub(crate) fn apply_supersession<'a>(
    observations: Vec<query::ContextObservation<'a>>,
    resolver: &crate::temporal_status::TemporalResolver<'a>,
    mode: crate::temporal_status::SupersessionMode,
) -> (
    Vec<query::ContextObservation<'a>>,
    Vec<ExcludedDiagnostic<'a>>,
) {
    let mut filtered = Vec::new();
    let mut excluded = Vec::new();

    for mut obs in observations {
        let (status, superseded_by, contradicted_by) = resolver.resolve_status(obs.record_id);

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
                        record_id: obs.record_id,
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
                    obs.temporal_status = Some(status.to_string());
                    obs.superseded_by = if superseded_by.is_empty() {
                        None
                    } else {
                        Some(superseded_by)
                    };
                    obs.contradicted_by = if contradicted_by.is_empty() {
                        None
                    } else {
                        Some(contradicted_by)
                    };
                    filtered.push(obs);
                }
            }
        } else {
            match mode {
                crate::temporal_status::SupersessionMode::IncludeButFlag => {
                    obs.temporal_status = Some(status.to_string());
                    filtered.push(obs);
                }
                crate::temporal_status::SupersessionMode::Exclude => {
                    filtered.push(obs);
                }
            }
        }
    }

    (filtered, excluded)
}

pub(crate) fn query_context_cmd(
    records: &[GraphRecord],
    symbol_name: &str,
    freshness: Option<(String, &'static str)>,
    supersession: crate::temporal_status::SupersessionMode,
    at_head: bool,
    all_history: bool,
) -> Result<()> {
    // Corpus-mode selection (issue #456): head-anchor by default over a
    // scan-history store; `--all-history` opts into the union. Pre-filter drops
    // off-HEAD records BEFORE building the cross-domain context bundle.
    let index = query::RepositoryIndex::build(records);
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, &index, false, at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    let ctx = query::symbol_context(records, symbol_name);

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

    let sections = build_context_sections(&ctx);

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

    let resolver = crate::temporal_status::TemporalResolver::build(records);
    let (observations, excluded) =
        apply_supersession(sections.observations, &resolver, supersession);

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    let response = ContextResponse {
        ok: true,
        symbol_name,
        freshness: freshness_code,
        source_facts: sections.source_facts,
        topology_edges: sections.topology_edges,
        observations,
        project_state: sections.project_state,
        artifacts: sections.artifacts,
        verification_evidence: sections.verification_evidence,
        unresolved: sections.unresolved,
        excluded,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    };

    let output = serde_json::to_string_pretty(&response).context("failed to serialize context")?;
    println!("{output}");
    Ok(())
}

pub(crate) fn context_source_fact(record: &GraphRecord) -> Option<ContextSourceFact<'_>> {
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
