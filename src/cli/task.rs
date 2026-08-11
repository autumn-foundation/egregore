use super::*;

#[allow(clippy::too_many_lines)]
pub(crate) fn query_task_cmd(records: &[GraphRecord], id_or_handle: &str) -> Result<()> {
    // Resolve task ID
    let resolved_ids = match query::resolve_task_ids(records, id_or_handle) {
        Ok(ids) => ids,
        Err(query::TaskResolveError::Ambiguous { handle, candidates }) => {
            let err_json =
                serde_json::to_string(&query::TaskResolveError::Ambiguous { handle, candidates })?;
            eprintln!("{err_json}");
            std::process::exit(1);
        }
        Err(query::TaskResolveError::Unsupported { handle, message }) => {
            let err_json =
                serde_json::to_string(&query::TaskResolveError::Unsupported { handle, message })?;
            eprintln!("{err_json}");
            std::process::exit(1);
        }
    };

    if resolved_ids.is_empty() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "task_id": id_or_handle
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let task_id = resolved_ids.iter().next().unwrap();
    let ctx = query::task_evidence_context(records, task_id);

    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "task_id": id_or_handle
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let trust = query::TrustIndex::build(records);

    let tasks: Vec<ContextLinkedItem<'_>> = ctx
        .tasks
        .iter()
        .filter_map(|r| context_linked_item(r, &trust))
        .collect();

    let acceptance_criteria: Vec<ContextLinkedItem<'_>> = ctx
        .acceptance_criteria
        .iter()
        .filter_map(|r| {
            let mut ac = context_linked_item(r, &trust)?;
            if ac.status == Some("verified") {
                let GraphRecord::Node {
                    verification_link_id,
                    ..
                } = r
                else {
                    return Some(ac);
                };
                let ver_id = verification_link_id.as_deref().or_else(|| {
                    records.iter().find_map(|edge| {
                        if let GraphRecord::Edge {
                            label: EdgeLabel::ClosesAcceptanceCriterion,
                            source,
                            target,
                            ..
                        } = edge
                        {
                            if source == r.id() {
                                Some(target.as_str())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                });

                if let Some(ver_record) =
                    ver_id.and_then(|vid| records.iter().rfind(|cand| cand.id() == vid))
                {
                    ac.verification_record = context_linked_item(ver_record, &trust).map(Box::new);
                }
            }
            Some(ac)
        })
        .collect();

    let source_facts: Vec<ContextSourceFact<'_>> = ctx
        .source_facts
        .iter()
        .filter_map(|r| context_source_fact(r, &trust))
        .collect();

    let observations: Vec<ContextObservation<'_>> = ctx
        .observations
        .iter()
        .filter_map(|r| context_observation(r, &trust))
        .collect();

    let artifacts: Vec<ContextLinkedItem<'_>> = ctx
        .artifacts
        .iter()
        .filter_map(|r| context_linked_item(r, &trust))
        .collect();

    let verification_evidence: Vec<ContextLinkedItem<'_>> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| context_linked_item(r, &trust))
        .collect();

    let reviews: Vec<ContextLinkedItem<'_>> = ctx
        .reviews
        .iter()
        .filter_map(|r| context_linked_item(r, &trust))
        .collect();

    let external_links: Vec<ContextLinkedItem<'_>> = ctx
        .external_links
        .iter()
        .filter_map(|r| context_linked_item(r, &trust))
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

    let response = TaskContextResponse {
        ok: true,
        task_id,
        tasks,
        acceptance_criteria,
        source_facts,
        observations,
        artifacts,
        verification_evidence,
        reviews,
        external_links,
        unresolved,
        excluded: Vec::new(),
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize task context")?;
    println!("{output}");
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn query_task_via_daemon(id_or_handle: &str, data_dir: &Path) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let params = serde_json::json!({ "task_id": id_or_handle });
    match client.query_verb_raw("criteria_for_task", &params, None) {
        Ok(result) => {
            let output = serde_json::to_string_pretty(&result)
                .context("failed to serialize task query result")?;
            println!("{output}");
            Ok(())
        }
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("not_found") || err_msg.contains("no records found") {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "no_match",
                        "task_id": id_or_handle
                    }
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(2);
            } else if err_msg.contains("ambiguous") || err_msg.contains("unsupported") {
                eprintln!("{err_msg}");
                std::process::exit(1);
            }
            Err(e)
        }
    }
}
