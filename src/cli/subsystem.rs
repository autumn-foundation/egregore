use super::*;

#[allow(clippy::too_many_lines)]
pub(crate) fn query_subsystem_cmd(
    records: &[GraphRecord],
    prefix: &str,
    _format: OutputFormat,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
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
        unresolved,
        excluded,
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize subsystem context")?;
    println!("{output}");
    Ok(())
}
