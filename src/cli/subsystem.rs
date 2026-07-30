use super::*;

#[allow(clippy::too_many_lines)]
pub(crate) fn query_subsystem_cmd(
    records: &[GraphRecord],
    prefix: &str,
    at_head: bool,
    all_history: bool,
    _format: OutputFormat,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
    // Corpus-mode selection (issue #456): head-anchor by default over a
    // scan-history store; `--all-history` opts into the union. Pre-filter drops
    // off-HEAD records BEFORE the cross-domain traversal.
    let index = query::RepositoryIndex::build(records);
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, &index, false, at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    let ctx = match query::subsystem_context(records, prefix) {
        Ok(ctx) => ctx,
        Err(query::SubsystemPrefixError::Malformed { prefix: p }) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "malformed_prefix",
                    "prefix": p,
                    "message": "prefix must be non-empty after stripping trailing slashes"
                }
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
    };

    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "prefix": prefix,
                "message": "no records found under the given prefix"
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let source_facts: Vec<ContextSourceFact<'_>> = ctx
        .source_facts
        .iter()
        .filter_map(|r| context_source_fact(r))
        .collect();

    let raw_observations: Vec<ContextObservation<'_>> = ctx
        .observations
        .iter()
        .filter_map(|r| context_observation(r))
        .collect();

    let resolver = crate::temporal_status::TemporalResolver::build(records);
    let (observations, excluded) = apply_supersession(raw_observations, &resolver, supersession);

    let project_state: Vec<ContextLinkedItem<'_>> = ctx
        .project_state
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let artifacts: Vec<ContextLinkedItem<'_>> = ctx
        .artifacts
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let verification_evidence: Vec<ContextLinkedItem<'_>> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let unresolved: Vec<ContextUnresolved<'_>> = ctx
        .unresolved
        .iter()
        .map(|u| ContextUnresolved {
            source_record_id: &u.source_record_id,
            target_handle: &u.target_handle,
            relation: &u.relation,
            target_domain: &u.target_domain,
            verification_status: "unresolved",
        })
        .collect();

    let topology_edges: Vec<ContextTopologyEdge<'_>> = ctx
        .topology_edges
        .iter()
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
        .collect();

    let semantic_drift: Vec<SubsystemDrift<'_>> = ctx
        .semantic_drift
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                semantic_drift: Some(drift_meta),
                ..
            } = r
            else {
                return None;
            };
            let (path, _, span) = query::resolve_drift_target(records, id, drift_meta, None, None);
            Some(SubsystemDrift {
                record_id: id,
                score: drift_meta.score,
                target_repo_relative_path: path,
                target_span: span,
                after_git_commit: Some(drift_meta.after_git_commit.as_str()),
            })
        })
        .collect();

    let log_signatures: Vec<SubsystemLogSignature<'_>> = ctx
        .log_signatures
        .iter()
        .map(|s| SubsystemLogSignature {
            record_id: s.record_id,
            kind: "ErrorSignature",
            trust_class: "runtime_observation",
            schema_version: s.schema_version,
            severity: s.severity,
            occurrence_count: s.occurrence_count,
            template_excerpt: s.template_excerpt,
            first_seen_valid_time: (!s.first_seen.is_empty()).then_some(s.first_seen),
            last_seen_valid_time: (!s.last_seen.is_empty()).then_some(s.last_seen),
            resolved_frames: s
                .in_prefix_frames
                .iter()
                .map(|f| SubsystemLogFrame {
                    frame_index: f.frame_index,
                    frame_resolution: f.frame_resolution,
                    target_repo_relative_path: f.target_repo_relative_path,
                    target_span: f.target_span,
                })
                .collect(),
        })
        .collect();

    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    let response = SubsystemResponse {
        ok: true,
        prefix: ctx.prefix.as_str(),
        source_facts,
        topology_edges,
        observations,
        project_state,
        artifacts,
        verification_evidence,
        semantic_drift,
        log_signatures,
        unresolved,
        excluded,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize subsystem context")?;
    println!("{output}");
    Ok(())
}
