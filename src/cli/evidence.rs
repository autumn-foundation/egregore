use super::*;

/// Prints a machine-readable JSON error envelope to stderr and exits with code 1.
///
/// Callers that detect a [`crate::evidence::ProvenanceError`] use this instead of
/// propagating the error so that `main` does not emit a second human-readable line
/// after the JSON envelope has already been written.
pub(crate) fn write_evidence_error(e: &crate::evidence::ProvenanceError) -> ! {
    eprintln!(r#"{{"code":"{}", "field":"{}"}}"#, e.code, e.field);
    process::exit(1);
}

/// Prints a machine-readable JSON supersession/contradiction diagnostic to
/// stderr (issue #184) and exits with the error's stable exit code.
///
/// The envelope never echoes observation text or payload values — only the
/// stable handles the caller supplied (`field` names the CLI flag,
/// `target` the record handle).
pub(crate) fn write_supersession_error(
    e: &crate::supersede_write::SupersessionError,
    field: &str,
) -> ! {
    eprintln!("{}", e.to_json(field));
    process::exit(e.exit_code());
}

/// Resolves `--supersedes` / `--contradicts` into a validated
/// [`SupersessionTarget`] (issue #184).
///
/// Returns `None` when neither flag was given. When a flag is present the
/// target handle is validated against the local store (`--graph` JSONL or
/// embedded `--data-dir`) before any write happens: the handle must name a
/// live observation-class record, never a deterministic code-graph fact.
#[allow(clippy::too_many_arguments)]
fn resolve_supersession_target(
    supersedes: Option<String>,
    contradicts: Option<String>,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
) -> Option<crate::supersede_write::SupersessionTarget> {
    use crate::ir::EdgeLabel;
    use crate::supersede_write::{
        SupersessionError, SupersessionTarget, validate_supersession_target,
    };

    let (target_id, label, field) = match (supersedes, contradicts) {
        (None, None) => return None,
        (Some(id), None) => (id, EdgeLabel::Supersedes, "supersedes"),
        (None, Some(id)) => (id, EdgeLabel::Contradicts, "contradicts"),
        // Clap `conflicts_with` rejects this combination before we get here.
        (Some(_), Some(_)) => {
            eprintln!(r#"{{"code":"invalid_field", "field":"supersedes"}}"#);
            process::exit(1);
        }
    };
    if target_id.is_empty() {
        write_supersession_error(&SupersessionError::UnknownTarget { target_id }, field);
    }
    // A supersession/contradiction write is completely local, but the target
    // must be validated against a store the user points at — never inferred.
    let store_records: Vec<crate::ir::GraphRecord> = match (graph, data_dir) {
        // Clap `conflicts_with` rejects `--graph` + `--data-dir` together
        // before we get here; both arms below are the "no usable store"
        // case.
        (None, None) | (Some(_), Some(_)) => {
            write_supersession_error(&SupersessionError::StoreRequired, field)
        }
        (Some(path), None) => load_records_from_jsonl(path).unwrap_or_else(|_| {
            write_supersession_error(
                &SupersessionError::StoreUnreadable {
                    input: "graph".to_owned(),
                },
                field,
            )
        }),
        (None, Some(dir)) => load_records_from_db(dir).unwrap_or_else(|_| {
            write_supersession_error(
                &SupersessionError::StoreUnreadable {
                    input: "data_dir".to_owned(),
                },
                field,
            )
        }),
    };
    validate_supersession_target(&store_records, &target_id)
        .unwrap_or_else(|e| write_supersession_error(&e, field));
    Some(SupersessionTarget {
        target_record_id: target_id,
        label,
    })
}

/// Handles `eg write <kind>` subcommands.
///
/// On provenance failure the function writes a JSON error to stderr
/// (`{"code":"missing_field","field":"<name>"}`) and returns an error.
#[allow(clippy::too_many_lines)]
pub(crate) fn write_evidence(kind: WriteKind) -> Result<()> {
    match kind {
        WriteKind::Observation {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            text,
            confidence,
            evidence_target,
            evidence_domain,
            supersedes,
            contradicts,
            graph,
            data_dir,
            out,
        } => {
            // Only codegraph (OBSERVES) and verification (VALIDATED_BY) are supported.
            // The daemon rejects OBSERVES on non-codegraph targets and VALIDATED_BY on
            // non-verification targets, so any other domain would produce invalid JSONL.
            if evidence_domain != "codegraph" && evidence_domain != "verification" {
                write_evidence_error(&crate::evidence::ProvenanceError::invalid(
                    "evidence_domain",
                ));
            }
            let relation = if evidence_domain == "verification" {
                EdgeLabel::ValidatedBy.as_str().to_owned()
            } else {
                EdgeLabel::Observes.as_str().to_owned()
            };
            let evidence_links: Vec<EvidenceLink> = evidence_target
                .into_iter()
                .map(|target_id| EvidenceLink {
                    target_record_id: Some(target_id),
                    target_domain: evidence_domain.clone(),
                    relation: relation.clone(),
                    confidence: confidence.to_string(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                })
                .collect();
            let req = ObservationRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle: Some(source_handle),
                },
                text,
                confidence,
                evidence_links,
                // Write-time supersession/contradiction (issue #184): validated
                // against the local store before any record is built.
                supersession: resolve_supersession_target(
                    supersedes,
                    contradicts,
                    graph.as_deref(),
                    data_dir.as_deref(),
                ),
            };
            let outcome = build_observation_records(&req).unwrap_or_else(|e| {
                // In the CLI path the builder only ever receives a valid
                // SUPERSEDES/CONTRADICTS label and a non-empty target, so a
                // builder rejection on the `supersession` field is
                // unambiguously the self-target cycle guard firing.
                if e.field == "supersession" {
                    let sup = req.supersession.as_ref();
                    let target_id = sup.map(|s| s.target_record_id.clone()).unwrap_or_default();
                    let field = match sup.map(|s| s.label) {
                        Some(EdgeLabel::Contradicts) => "contradicts",
                        _ => "supersedes",
                    };
                    write_supersession_error(
                        &crate::supersede_write::SupersessionError::SelfTarget { target_id },
                        field,
                    );
                }
                write_evidence_error(&e)
            });
            // Idempotency-conflict check (issue #184): the same observation
            // identity (content-addressed, i.e. the idempotency key) already
            // carrying a live edge of the same relation to a *different*
            // target conflicts instead of duplicating. An identical re-run
            // builds byte-identical records and is a no-op, not a conflict.
            if let Some(sup) = &req.supersession {
                // The builder only ever receives SUPERSEDES or CONTRADICTS
                // here (validated at flag-parse time and by the builder).
                let field = if sup.label == EdgeLabel::Contradicts {
                    "contradicts"
                } else {
                    "supersedes"
                };
                let store_records: Vec<GraphRecord> = match (graph.as_deref(), data_dir.as_deref())
                {
                    (Some(path), None) => load_records_from_jsonl(path).unwrap_or_else(|_| {
                        write_supersession_error(
                            &crate::supersede_write::SupersessionError::StoreUnreadable {
                                input: "graph".to_owned(),
                            },
                            field,
                        )
                    }),
                    (None, Some(dir)) => load_records_from_db(dir).unwrap_or_else(|_| {
                        write_supersession_error(
                            &crate::supersede_write::SupersessionError::StoreUnreadable {
                                input: "data_dir".to_owned(),
                            },
                            field,
                        )
                    }),
                    _ => write_supersession_error(
                        &crate::supersede_write::SupersessionError::StoreRequired,
                        field,
                    ),
                };
                crate::supersede_write::check_supersession_conflict(
                    &store_records,
                    &outcome.record_id,
                    sup.label,
                    &sup.target_record_id,
                )
                .unwrap_or_else(|e| write_supersession_error(&e, field));
            }
            // Surface the authored edge's handle so the caller can cite the
            // forward handle the read side reports for the prior record.
            let supersession_edge = outcome.records.iter().find_map(|record| match record {
                GraphRecord::Edge {
                    id,
                    label: EdgeLabel::Supersedes | EdgeLabel::Contradicts,
                    ..
                } => Some(id.as_str()),
                _ => None,
            });
            write_evidence_outcome(
                &outcome.records,
                &out,
                &outcome.record_id,
                supersession_edge,
            )
        }
        WriteKind::CommandEvidence {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            executed_at,
            exit_code,
            stdout,
            stderr,
            evidence_quality,
            source_artifact_path,
            source_artifact_hash,
            out,
        } => {
            let exit_code = exit_code.unwrap_or_else(|| {
                write_evidence_error(&crate::evidence::ProvenanceError::missing("exit_code"))
            });
            let req = CommandEvidenceRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle,
                },
                executed_at,
                exit_code,
                stdout,
                stderr,
                evidence_quality,
                source_artifact_path,
                source_artifact_hash,
            };
            let outcome =
                build_command_evidence_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id, None)
        }
        WriteKind::Artifact {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            patch_file,
            target_file,
            patch_status,
            base_commit,
            source_artifact_path,
            source_artifact_hash,
            validation_summary,
            out,
        } => {
            let patch_bytes = fs::read(&patch_file)
                .with_context(|| format!("failed to read patch file {}", patch_file.display()))?;
            let req = ArtifactRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle,
                },
                patch_bytes,
                target_files: target_file,
                patch_status,
                base_commit,
                source_artifact_path,
                source_artifact_hash,
                validation_summary,
            };
            let outcome = build_artifact_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id, None)
        }
        WriteKind::Verification {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            executed_at,
            status,
            verification_kind,
            stdout,
            evidence_quality,
            source_artifact_path,
            source_artifact_hash,
            linked_command_evidence_id,
            out,
        } => {
            let req = VerificationRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle,
                },
                executed_at,
                status,
                verification_kind,
                stdout,
                evidence_quality,
                source_artifact_path,
                source_artifact_hash,
                linked_command_evidence_id,
            };
            let outcome =
                build_verification_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id, None)
        }
        WriteKind::Failure {
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            source_handle,
            failure_kind,
            text,
            exit_code,
            failed_on,
            references_task,
            out,
        } => {
            let req = FailureRequest {
                provenance: EvidenceProvenance {
                    agent_id,
                    agent_kind,
                    session_id,
                    observed_at,
                    source_handle: Some(source_handle),
                },
                failure_kind,
                text,
                exit_code,
                failed_on,
                references_task,
            };
            let outcome = build_failure_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id, None)
        }
    }
}

/// Serializes evidence records to JSONL and prints the evidence handle.
pub(crate) fn write_evidence_outcome(
    records: &[GraphRecord],
    out: &Path,
    evidence_handle: &str,
    supersession_edge: Option<&str>,
) -> Result<()> {
    let mut graph = Graph::new();
    for record in records {
        graph.push(record.clone());
    }
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize evidence JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write evidence JSONL to {}", out.display()))?;
    // The supersession/contradiction edge handle (issue #184) is the forward
    // handle the read side reports when recall/audit flags the prior record.
    let edge_field = supersession_edge
        .map(|id| format!(r#","supersession_edge":"{id}""#))
        .unwrap_or_default();
    println!(
        r#"{{"ok":true,"evidence_handle":"{}","records":{}{}}}"#,
        evidence_handle,
        records.len(),
        edge_field
    );
    Ok(())
}

pub(crate) fn link_evidence_cmd(
    code_graph_path: &Path,
    evidence_path: &Path,
    out: &Path,
) -> Result<()> {
    let cg_jsonl = fs::read_to_string(code_graph_path).with_context(|| {
        format!(
            "failed to read code-graph JSONL from {}",
            code_graph_path.display()
        )
    })?;
    let ev_jsonl = fs::read_to_string(evidence_path).with_context(|| {
        format!(
            "failed to read evidence JSONL from {}",
            evidence_path.display()
        )
    })?;

    let code_graph = records_from_jsonl(&cg_jsonl).with_context(|| {
        format!(
            "failed to parse code-graph JSONL from {}",
            code_graph_path.display()
        )
    })?;
    let evidence = records_from_jsonl(&ev_jsonl).with_context(|| {
        format!(
            "failed to parse evidence JSONL from {}",
            evidence_path.display()
        )
    })?;

    let link_output = link_evidence::link_evidence(&code_graph, &evidence, &LinkOptions::default());

    // Write diagnostics to stderr as JSON lines (machine-readable, per AC5).
    for diag in &link_output.diagnostics {
        let line = serde_json::to_string(diag).context("failed to serialize diagnostic")?;
        eprintln!("{line}");
    }

    // Serialize resolved edges as JSONL.
    let mut lines: Vec<String> = Vec::with_capacity(link_output.edges.len());
    for edge in &link_output.edges {
        let line = serde_json::to_string(edge).context("failed to serialize linked edge")?;
        lines.push(line);
    }
    let content = if lines.is_empty() {
        String::new()
    } else {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    };

    fs::write(out, content)
        .with_context(|| format!("failed to write output to {}", out.display()))?;

    println!(
        "linked: {} edges resolved, {} diagnostics",
        link_output.edges.len(),
        link_output.diagnostics.len()
    );
    Ok(())
}
