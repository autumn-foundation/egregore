use super::*;

#[allow(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]
pub(crate) fn decide_cmd(
    candidate_id: String,
    outcome: String,
    data_dir: Option<PathBuf>,
    graph: Option<PathBuf>,
    out: Option<PathBuf>,
    edited_rule_text: Option<String>,
    rationale: Option<String>,
    decided_by: String,
    prompt_surface: String,
    prompted_to: String,
) -> Result<()> {
    if out.is_none() && data_dir.is_none() {
        anyhow::bail!("Either --out or --data-dir must be specified to write the decision records");
    }

    #[cfg(feature = "embedded-aletheiadb")]
    let mut sink_opt = None;

    #[allow(unused_mut)]
    let mut records = if let Some(path) = &graph {
        load_records_from_jsonl(path)?
    } else if let Some(dir) = &data_dir {
        #[cfg(feature = "embedded-aletheiadb")]
        {
            validate_existing_embedded_store(dir)?;
            let sink = EmbeddedAletheiaSink::open(dir)
                .with_context(|| format!("failed to open embedded store {}", dir.display()))?;
            let db_recs = sink
                .read_all_records()
                .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
            sink_opt = Some(sink);
            db_recs
        }
        #[cfg(not(feature = "embedded-aletheiadb"))]
        {
            let _ = dir;
            anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
        }
    } else {
        anyhow::bail!("provide --graph <path> or --data-dir <path>");
    };

    if graph.is_some()
        && let Some(dir) = &data_dir
    {
        #[cfg(feature = "embedded-aletheiadb")]
        {
            let sink = if let Some(s) = sink_opt.take() {
                s
            } else {
                EmbeddedAletheiaSink::open(dir)
                    .with_context(|| format!("failed to open embedded store {}", dir.display()))?
            };

            let store_exists = fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some());
            let db_records = if store_exists {
                sink.read_all_records()
                    .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?
            } else {
                Vec::new()
            };
            sink_opt = Some(sink);

            if !db_records.is_empty() {
                let mut merged = db_records;
                for r in records {
                    if !merged.contains(&r) {
                        merged.push(r);
                    }
                }
                records = merged;
            }
        }
        #[cfg(not(feature = "embedded-aletheiadb"))]
        {
            let _ = dir;
            anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
        }
    }

    let req = crate::decide::DecideRequest {
        candidate_id,
        outcome,
        edited_rule_text,
        rationale,
        decided_by,
        prompt_surface,
        prompted_to,
        transaction_time: None,
    };

    let generated = crate::decide::decide_candidate(&records, &req)?;

    if let Some(out_path) = out {
        let mut lines = String::new();
        for rec in &generated {
            let json = serde_json::to_string(rec)?;
            lines.push_str(&json);
            lines.push('\n');
        }
        fs::write(&out_path, lines).with_context(|| {
            format!("failed to write decision records to {}", out_path.display())
        })?;
    }

    if let Some(dir) = data_dir {
        #[cfg(feature = "embedded-aletheiadb")]
        {
            let mut sink = sink_opt.take().unwrap();
            let edges = crate::decide::synthesize_user_context_edges(&records, &generated);

            let mut source_records_to_persist = Vec::new();
            let mut seen_ids = std::collections::HashSet::new();
            for g in &generated {
                seen_ids.insert(g.id().to_owned());
            }
            let mut validation_edges = Vec::new();
            if let Some(cand) = records.iter().rfind(|r| r.id() == req.candidate_id) {
                let val_edges =
                    crate::daemon::validate_promote_candidate_for_cli(cand, &records, &sink)
                        .context("Candidate validation failed")?;
                validation_edges = val_edges;

                source_records_to_persist.push(cand.clone());
                seen_ids.insert(cand.id().to_owned());
            }

            let mut idx = 0;
            while idx < source_records_to_persist.len() {
                let rec = source_records_to_persist[idx].clone();
                idx += 1;

                if let GraphRecord::Node {
                    kind,
                    user_context,
                    evidence_links,
                    superseded_by,
                    ..
                } = &rec
                {
                    match kind {
                        NodeKind::PromoteCandidate => {
                            if let Some(evidence) = &user_context.supporting_evidence {
                                for link in evidence {
                                    if let Some(ref_id) = &link.target_record_id
                                        && seen_ids.insert(ref_id.clone())
                                        && let Some(evidence_rec) =
                                            records.iter().rfind(|r| r.id() == *ref_id)
                                    {
                                        if let GraphRecord::Node {
                                            kind: ev_kind,
                                            evidence_links: ev_links,
                                            ..
                                        } = evidence_rec
                                            && *ev_kind == NodeKind::Observation
                                            && ev_links
                                                .as_deref()
                                                .is_none_or(<[EvidenceLink]>::is_empty)
                                        {
                                            anyhow::bail!(
                                                "Observation '{ref_id}' must have at least one evidence link"
                                            );
                                        }
                                        source_records_to_persist.push(evidence_rec.clone());
                                    }
                                }
                            }
                            if let Some(evidence) = &user_context.contradicting_evidence {
                                for link in evidence {
                                    if let Some(ref_id) = &link.target_record_id {
                                        if user_context.proposed_rule_kind.as_deref()
                                            == Some("revocation")
                                            && let Some(target_rec) =
                                                records.iter().rfind(|r| r.id() == *ref_id)
                                            && let Ok(target_chain) =
                                                crate::query::audit_trail(&records, target_rec)
                                        {
                                            for chain_rec in target_chain {
                                                if seen_ids.insert(chain_rec.id().to_owned()) {
                                                    source_records_to_persist
                                                        .push(chain_rec.clone());
                                                }
                                            }
                                        }
                                        if seen_ids.insert(ref_id.clone())
                                            && let Some(evidence_rec) =
                                                records.iter().rfind(|r| r.id() == *ref_id)
                                        {
                                            source_records_to_persist.push(evidence_rec.clone());
                                        }
                                    }
                                }
                            }
                            if let Some(rej_id) = superseded_by {
                                if seen_ids.insert(rej_id.clone())
                                    && let Some(rej_cand) =
                                        records.iter().rfind(|r| r.id() == *rej_id)
                                {
                                    source_records_to_persist.push(rej_cand.clone());
                                }
                                for r in &records {
                                    if let GraphRecord::Node {
                                        kind: NodeKind::PromotionDecision,
                                        user_context: dec_uc,
                                        ..
                                    } = r
                                        && dec_uc.candidate_id.as_deref() == Some(rej_id)
                                        && dec_uc.outcome.as_deref() == Some("rejected")
                                        && seen_ids.insert(r.id().to_owned())
                                    {
                                        source_records_to_persist.push(r.clone());
                                    }
                                }
                            }
                        }
                        NodeKind::PromotionDecision => {
                            if let Some(p_id) = &user_context.prompt_id
                                && seen_ids.insert(p_id.clone())
                                && let Some(prompt_rec) = records.iter().rfind(|r| r.id() == *p_id)
                            {
                                source_records_to_persist.push(prompt_rec.clone());
                            }
                        }
                        _ => {
                            if let Some(links) = evidence_links {
                                for ev_link in links {
                                    if let Some(target_id) = &ev_link.target_record_id
                                        && seen_ids.insert(target_id.clone())
                                        && let Some(target_rec) =
                                            records.iter().rfind(|r| r.id() == *target_id)
                                    {
                                        source_records_to_persist.push(target_rec.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let generated_ids: std::collections::HashSet<String> =
                generated.iter().map(|g| g.id().to_owned()).collect();
            let mut copied_synthesized_edges = Vec::new();
            let mut seen_memory_edges: std::collections::HashMap<String, (Option<String>, String)> =
                std::collections::HashMap::new();
            for rec in &source_records_to_persist {
                if generated_ids.contains(rec.id()) {
                    continue;
                }
                if rec.id().starts_with("agent_memory:v1:") {
                    crate::daemon::validate_agent_memory_record_for_cli(rec, &records, &sink)
                        .context("Copied evidence record validation failed")?;
                    if let GraphRecord::Node {
                        id,
                        evidence_links: Some(links),
                        ..
                    } = rec
                    {
                        for link in links {
                            if let Some(target_id) = &link.target_record_id
                                && let Some(label) =
                                    crate::ir::EdgeLabel::from_relation(&link.relation)
                            {
                                let summary =
                                    format!("{} {} (from evidence link)", id, label.as_str());
                                let mut edge = GraphRecord::agent_memory_edge(
                                    label,
                                    id.clone(),
                                    target_id.clone(),
                                    Some(link.confidence.clone()),
                                    summary,
                                );
                                if let Some(ref commit) = link.as_of_commit {
                                    edge = edge.with_temporal(crate::ir::TemporalMetadata {
                                        git_commit: commit.clone(),
                                        git_parent_commits: Vec::new(),
                                        valid_time: "1970-01-01T00:00:00Z".to_owned(),
                                        author_time: None,
                                        observed_at: "1970-01-01T00:00:00Z".to_owned(),
                                        valid_time_source: None,
                                    });
                                }
                                let edge_id = edge.id().to_owned();
                                match seen_memory_edges.get(&edge_id) {
                                    Some((existing_commit, existing_conf))
                                        if *existing_commit == link.as_of_commit =>
                                    {
                                        if existing_conf != &link.confidence {
                                            anyhow::bail!(
                                                "evidence links for edge '{edge_id}' have conflicting confidence values"
                                            );
                                        }
                                        // exact duplicate, skip silently
                                    }
                                    Some(_) => {
                                        anyhow::bail!(
                                            "evidence links for edge '{edge_id}' have conflicting as_of_commit values"
                                        );
                                    }
                                    None => {
                                        seen_memory_edges.insert(
                                            edge_id,
                                            (link.as_of_commit.clone(), link.confidence.clone()),
                                        );
                                        copied_synthesized_edges.push(edge);
                                    }
                                }
                            }
                        }
                    }
                } else if rec.id().starts_with("user_context:v1:") {
                    let edges =
                        crate::daemon::validate_user_context_record_for_cli(rec, &records, &sink)
                            .context("Copied user-context record validation failed")?;
                    copied_synthesized_edges.extend(edges);
                }

                crate::redaction::validate_record(rec).map_err(|e| {
                    anyhow::anyhow!(
                        "Redaction check failed for source record '{}': {}",
                        rec.id(),
                        e
                    )
                })?;

                if let GraphRecord::Node {
                    id,
                    kind,
                    user_context,
                    ..
                } = rec
                {
                    if *kind == NodeKind::PromotionDecision {
                        if let Some(c_id) = &user_context.candidate_id {
                            let edge_id = crate::ir::user_context_stable_id(&[
                                "edge",
                                "DECIDED_ON",
                                id,
                                c_id,
                            ]);
                            copied_synthesized_edges.push(GraphRecord::Edge {
                                id: edge_id,
                                schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                                label: crate::ir::EdgeLabel::DecidedOn,
                                source: id.clone(),
                                target: c_id.clone(),
                                confidence: None,
                                resolution: None,
                                frame_resolution: None,
                                frame_index: None,
                                basis: None,
                                temporal: None,
                                summary: "PromotionDecision decided on PromoteCandidate".to_owned(),
                                producer: None,
                            });
                        }
                        if let Some(mat_id) = &user_context.materialized_record_id {
                            let outcome_str = user_context.outcome.as_deref().unwrap_or("");
                            if outcome_str == "approved" || outcome_str == "edited_then_approved" {
                                let is_revocation =
                                    user_context.candidate_id.as_ref().is_some_and(|c_id| {
                                        records
                                            .iter()
                                            .rfind(|r| match r {
                                                GraphRecord::Node { id: node_id, .. } => {
                                                    node_id == c_id
                                                }
                                                _ => false,
                                            })
                                            .and_then(|r| match r {
                                                GraphRecord::Node {
                                                    user_context: node_uc,
                                                    ..
                                                } => node_uc.proposed_rule_kind.as_deref(),
                                                _ => None,
                                            })
                                            == Some("revocation")
                                    });

                                if is_revocation {
                                    let edge_id = crate::ir::user_context_stable_id(&[
                                        "edge",
                                        "REVOKED_BY",
                                        mat_id,
                                        id,
                                    ]);
                                    copied_synthesized_edges.push(GraphRecord::Edge {
                                        id: edge_id,
                                        schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                                        label: crate::ir::EdgeLabel::RevokedBy,
                                        source: mat_id.clone(),
                                        target: id.clone(),
                                        confidence: None,
                                        resolution: None,
                                        frame_resolution: None,
                                        frame_index: None,
                                        basis: None,
                                        temporal: None,
                                        summary:
                                            "PromotionDecision revoked durable user-context record"
                                                .to_owned(),
                                        producer: None,
                                    });
                                } else {
                                    let edge_id = crate::ir::user_context_stable_id(&[
                                        "edge",
                                        "MATERIALIZED_AS",
                                        id,
                                        mat_id,
                                    ]);
                                    copied_synthesized_edges.push(GraphRecord::Edge {
                                        id: edge_id,
                                        schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                                        label: crate::ir::EdgeLabel::MaterializedAs,
                                        source: id.clone(),
                                        target: mat_id.clone(),
                                        confidence: None,
                                        resolution: None,
                                        frame_resolution: None,
                                        frame_index: None,
                                        basis: None,
                                        temporal: None,
                                        summary: "PromotionDecision materialized durable user-context record".to_owned(),
                                        producer: None,
                                    });
                                }
                            }
                        }
                    } else if *kind == NodeKind::PromotionPrompt
                        && let Some(c_id) = &user_context.candidate_id
                    {
                        let edge_id =
                            crate::ir::user_context_stable_id(&["edge", "PROMPTED_FOR", id, c_id]);
                        copied_synthesized_edges.push(GraphRecord::Edge {
                            id: edge_id,
                            schema_version: crate::ir::USER_CONTEXT_SCHEMA_VERSION,
                            label: crate::ir::EdgeLabel::PromptedFor,
                            source: id.clone(),
                            target: c_id.clone(),
                            confidence: None,
                            resolution: None,
                            frame_resolution: None,
                            frame_index: None,
                            basis: None,
                            temporal: None,
                            summary: "PromotionPrompt prompted for PromoteCandidate".to_owned(),
                            producer: None,
                        });
                    }
                }
            }

            let mut all_records = generated;
            all_records.extend(edges);
            all_records.extend(validation_edges);

            for edge in copied_synthesized_edges {
                if seen_ids.insert(edge.id().to_owned()) {
                    all_records.push(edge);
                }
            }

            let source_records_filtered: Vec<GraphRecord> = source_records_to_persist
                .into_iter()
                .filter(|r| !generated_ids.contains(r.id()))
                .collect();
            all_records.extend(source_records_filtered);
            let report = ingest_records(&all_records, &mut sink);
            if !report.is_success() {
                for failure in &report.failures {
                    eprintln!("{}: {}", failure.record_id, failure.message);
                }
                anyhow::bail!("failed to write decision records to store");
            }
            sink.persist_indexes()
                .with_context(|| format!("failed to persist embedded store {}", dir.display()))?;
        }
        #[cfg(not(feature = "embedded-aletheiadb"))]
        {
            let _ = dir;
            anyhow::bail!("--data-dir requires the embedded-aletheiadb feature");
        }
    }

    Ok(())
}
