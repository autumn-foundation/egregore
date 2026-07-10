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
            };
            let outcome =
                build_observation_records(&req).unwrap_or_else(|e| write_evidence_error(&e));
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
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
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
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
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
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
            write_evidence_outcome(&outcome.records, &out, &outcome.record_id)
        }
    }
}

/// Serializes evidence records to JSONL and prints the evidence handle.
pub(crate) fn write_evidence_outcome(
    records: &[GraphRecord],
    out: &Path,
    evidence_handle: &str,
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
    println!(
        r#"{{"ok":true,"evidence_handle":"{}","records":{}}}"#,
        evidence_handle,
        records.len()
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
