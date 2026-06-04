//! Decision record generation for user-context candidates.
#![allow(
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::uninlined_format_args,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::manual_let_else,
    clippy::match_same_arms,
    clippy::assigning_clones
)]

use anyhow::{Result, anyhow};
use chrono::Utc;

use crate::{
    EdgeLabel, GraphRecord, NodeKind, UserContextFields, UserContextScope,
    ir::{USER_CONTEXT_SCHEMA_VERSION, user_context_stable_id},
    redaction::redact_value,
};

fn blake3_hash_parts(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}

fn canonical_scope(scope: &UserContextScope) -> String {
    let mut parts = Vec::new();
    if let Some(repo) = &scope.repo {
        parts.push(format!("repo:{}", repo));
    }
    if let Some(path_glob) = &scope.path_glob {
        parts.push(format!("path_glob:{}", path_glob));
    }
    if let Some(language) = &scope.language {
        parts.push(format!("language:{}", language));
    }
    if let Some(lifecycle_phase) = &scope.lifecycle_phase {
        parts.push(format!("lifecycle_phase:{}", lifecycle_phase));
    }
    parts.join(",")
}

/// Request parameters for deciding on a candidate.
#[derive(Debug, Clone)]
pub struct DecideRequest {
    /// Candidate ID to decide on.
    pub candidate_id: String,
    /// Outcome of the decision (approved, edited_then_approved, rejected, deferred, expired).
    pub outcome: String,
    /// Edited rule text if outcome is edited_then_approved.
    pub edited_rule_text: Option<String>,
    /// Rationale for the decision.
    pub rationale: Option<String>,
    /// Who decided (operator handle).
    pub decided_by: String,
    /// surface where prompt was shown (e.g. cli, mcp).
    pub prompt_surface: String,
    /// operator prompt was shown to.
    pub prompted_to: String,
    /// Optional fixed transaction/valid time for determinism.
    pub transaction_time: Option<String>,
}

/// Helper to construct a user-context edge record.
pub fn user_context_edge(
    label: EdgeLabel,
    source: &str,
    target: &str,
    confidence: Option<String>,
    summary: &str,
) -> GraphRecord {
    GraphRecord::Edge {
        id: user_context_stable_id(&["edge", label.as_str(), source, target]),
        schema_version: USER_CONTEXT_SCHEMA_VERSION,
        label,
        source: source.to_owned(),
        target: target.to_owned(),
        confidence,
        temporal: None,
        summary: summary.to_owned(),
        producer: None,
    }
}

/// Processes a decision request against a slice of graph records.
/// Returns the list of new/updated records to write.
pub fn decide_candidate(records: &[GraphRecord], req: &DecideRequest) -> Result<Vec<GraphRecord>> {
    // 1. Locate PromoteCandidate
    let candidate = records
        .iter()
        .find(|r| r.id() == req.candidate_id)
        .ok_or_else(|| anyhow!("PromoteCandidate '{}' not found", req.candidate_id))?;

    let cand_fields = match candidate {
        GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            ..
        } => user_context,
        _ => {
            return Err(anyhow!(
                "Record '{}' is not a PromoteCandidate",
                req.candidate_id
            ));
        }
    };

    let valid_time_str = req
        .transaction_time
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    let proposed_rule_kind = cand_fields
        .proposed_rule_kind
        .as_deref()
        .ok_or_else(|| anyhow!("Candidate lacks proposed_rule_kind"))?;

    // 2. Validate request parameters
    let allowed_outcomes = [
        "approved",
        "rejected",
        "deferred",
        "expired",
        "edited_then_approved",
    ];
    if !allowed_outcomes.contains(&req.outcome.as_str()) {
        return Err(anyhow!(
            "Invalid outcome '{}'. Allowed values are: approved, rejected, deferred, expired, edited_then_approved",
            req.outcome
        ));
    }

    if (req.outcome == "approved" || req.outcome == "edited_then_approved")
        && proposed_rule_kind == "workflow_rule"
    {
        let triggers_ok = cand_fields
            .triggers
            .as_ref()
            .is_some_and(|t| !t.is_empty() && t.iter().all(|s| !s.trim().is_empty()));
        let action_summary_ok = cand_fields
            .action_summary
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());
        if !triggers_ok || !action_summary_ok {
            return Err(anyhow!(
                "Approved workflow_rule candidate must carry triggers and an action_summary"
            ));
        }
    }

    if req.outcome == "edited_then_approved" {
        if req.edited_rule_text.is_none() {
            return Err(anyhow!(
                "edited_rule_text is required when outcome is edited_then_approved"
            ));
        }
        if proposed_rule_kind == "revocation" {
            return Err(anyhow!(
                "edited_then_approved is not supported for revocation candidates"
            ));
        }
    }

    // 3. Generate PromotionPrompt
    let prompted_at = valid_time_str.clone();
    let prompt_text = format!(
        "Do you approve the promotion candidate '{}'?",
        req.candidate_id
    );
    let prompt_hash = blake3_hash_parts(&[&req.candidate_id, &prompted_at, &req.prompt_surface]);
    let prompt_id = user_context_stable_id(&["prompt", &prompt_hash]);

    let mut prompt_node = GraphRecord::node(
        prompt_id.clone(),
        NodeKind::PromotionPrompt,
        None,
        None,
        None,
        format!("Prompt for candidate {}", req.candidate_id),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut valid_time,
        ref mut valid_time_source,
        ref mut user_context,
        ..
    } = prompt_node
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some(valid_time_str.clone());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = UserContextFields {
            candidate_id: Some(req.candidate_id.clone()),
            prompt_surface: Some(req.prompt_surface.clone()),
            prompt_text: Some(redact_value(&prompt_text)),
            prompted_at: Some(prompted_at),
            prompted_to: Some(req.prompted_to.clone()),
            ..UserContextFields::empty()
        };
    }

    let prompt_edge = user_context_edge(
        EdgeLabel::PromptedFor,
        &prompt_id,
        &req.candidate_id,
        None,
        "Prompt prompted for PromoteCandidate",
    );

    // 4. Generate PromotionDecision
    let decided_at = valid_time_str.clone();
    let decision_hash =
        blake3_hash_parts(&[&req.candidate_id, &prompt_id, &decided_at, &req.outcome]);
    let decision_id = user_context_stable_id(&["decision", &decision_hash]);

    let mut generated_records = vec![prompt_node, prompt_edge];

    let mut decision_fields = UserContextFields {
        candidate_id: Some(req.candidate_id.clone()),
        prompt_id: Some(prompt_id),
        outcome: Some(req.outcome.clone()),
        decided_at: Some(decided_at.clone()),
        decided_by: Some(req.decided_by.clone()),
        decision_rationale: req.rationale.as_ref().map(|r| redact_value(r)),
        ..UserContextFields::empty()
    };

    // 5. Materialize or Revoke if approved
    if req.outcome == "approved" || req.outcome == "edited_then_approved" {
        let rule_text = if req.outcome == "edited_then_approved" {
            req.edited_rule_text.clone().unwrap()
        } else {
            cand_fields
                .proposed_rule_text
                .clone()
                .ok_or_else(|| anyhow!("Candidate proposed_rule_text is missing"))?
        };

        let scope = cand_fields
            .scope
            .clone()
            .unwrap_or_else(UserContextScope::default);

        if proposed_rule_kind == "revocation" {
            // Find target record to revoke.
            // In revocation candidates, the evidence link or contradicts links back to the durable record.
            let target_durable_id = cand_fields
                .contradicting_evidence
                .as_deref()
                .and_then(|v| v.first().and_then(|l| l.target_record_id.clone()))
                .or_else(|| {
                    // Fall back to matches in supporting evidence
                    cand_fields.supporting_evidence.as_deref().and_then(|v| {
                        v.iter()
                            .find(|l| l.target_domain == "user_context")
                            .and_then(|l| l.target_record_id.clone())
                    })
                })
                .ok_or_else(|| {
                    anyhow!("Revocation candidate does not specify target record to revoke")
                })?;

            let mut original_record = records
                .iter()
                .find(|r| r.id() == target_durable_id)
                .ok_or_else(|| {
                    anyhow!("Durable record '{}' to revoke not found", target_durable_id)
                })?
                .clone();

            if let GraphRecord::Node {
                ref mut user_context,
                ..
            } = original_record
            {
                user_context.active_to = Some(decided_at);
            }

            decision_fields.materialized_record_id = Some(target_durable_id.clone());

            generated_records.push(original_record);
            generated_records.push(user_context_edge(
                EdgeLabel::RevokedBy,
                &target_durable_id,
                &decision_id,
                None,
                "Durable record revoked by Decision",
            ));
        } else {
            // Materialize a new durable record
            let materialized_kind = match proposed_rule_kind {
                "preference" => NodeKind::Preference,
                "workflow_rule" => NodeKind::WorkflowRule,
                "naming_decision" => NodeKind::NamingDecision,
                "constraint" => NodeKind::Constraint,
                _ => {
                    return Err(anyhow!(
                        "Unsupported proposed rule kind '{}'",
                        proposed_rule_kind
                    ));
                }
            };

            let materialized_hash = match materialized_kind {
                NodeKind::Preference => {
                    blake3_hash_parts(&[&rule_text, &canonical_scope(&scope), &decision_id])
                }
                NodeKind::WorkflowRule => {
                    blake3_hash_parts(&[&rule_text, &canonical_scope(&scope), &decision_id])
                }
                NodeKind::NamingDecision => {
                    let entity_kind = cand_fields.entity_kind.as_deref().unwrap_or("other");
                    let canonical_name = cand_fields.canonical_name.as_deref().unwrap_or("");
                    blake3_hash_parts(&[
                        entity_kind,
                        canonical_name,
                        &canonical_scope(&scope),
                        &decision_id,
                    ])
                }
                NodeKind::Constraint => {
                    blake3_hash_parts(&[&rule_text, &canonical_scope(&scope), &decision_id])
                }
                _ => unreachable!(),
            };

            let materialized_id = match materialized_kind {
                NodeKind::Preference => user_context_stable_id(&["preference", &materialized_hash]),
                NodeKind::WorkflowRule => {
                    user_context_stable_id(&["workflow_rule", &materialized_hash])
                }
                NodeKind::NamingDecision => {
                    user_context_stable_id(&["naming_decision", &materialized_hash])
                }
                NodeKind::Constraint => user_context_stable_id(&["constraint", &materialized_hash]),
                _ => unreachable!(),
            };

            let mut materialized_node = GraphRecord::node(
                materialized_id.clone(),
                materialized_kind,
                None,
                None,
                None,
                format!("Materialized durable {:?}", materialized_kind),
            );

            if let GraphRecord::Node {
                ref mut schema_version,
                ref mut domain,
                ref mut valid_time,
                ref mut valid_time_source,
                ref mut user_context,
                ..
            } = materialized_node
            {
                *schema_version = USER_CONTEXT_SCHEMA_VERSION;
                *domain = Some("user_context".to_owned());
                *valid_time = Some(valid_time_str.clone());
                *valid_time_source = Some("inferred_from_transaction_time".to_owned());

                let mut durable_fields = UserContextFields {
                    scope: Some(scope),
                    approval_decision_id: Some(decision_id.clone()),
                    active_from: Some(valid_time_str.clone()),
                    proposed_rule_kind: Some(proposed_rule_kind.to_owned()),
                    ..UserContextFields::empty()
                };

                match materialized_kind {
                    NodeKind::Preference | NodeKind::WorkflowRule => {
                        durable_fields.rule_text = Some(redact_value(&rule_text));
                        if materialized_kind == NodeKind::WorkflowRule {
                            durable_fields.triggers = cand_fields.triggers.clone();
                            durable_fields.action_summary =
                                cand_fields.action_summary.as_ref().map(|s| redact_value(s));
                        }
                    }
                    NodeKind::NamingDecision => {
                        durable_fields.entity_kind = cand_fields.entity_kind.clone();
                        durable_fields.canonical_name = if req.outcome == "edited_then_approved" {
                            Some(rule_text.clone())
                        } else {
                            cand_fields.canonical_name.clone()
                        };
                        durable_fields.alternatives_rejected =
                            cand_fields.alternatives_rejected.clone();
                    }
                    NodeKind::Constraint => {
                        durable_fields.constraint_text = Some(redact_value(&rule_text));
                        durable_fields.enforcement_level = cand_fields.enforcement_level.clone();
                    }
                    _ => unreachable!(),
                }

                *user_context = durable_fields;
            }

            decision_fields.materialized_record_id = Some(materialized_id.clone());
            if req.outcome == "edited_then_approved" {
                decision_fields.edited_rule_text = Some(redact_value(&rule_text));
            }

            generated_records.push(materialized_node);
            generated_records.push(user_context_edge(
                EdgeLabel::MaterializedAs,
                &decision_id,
                &materialized_id,
                None,
                "Decision materialized durable user-context record",
            ));
        }
    }

    let mut decision_node = GraphRecord::node(
        decision_id.clone(),
        NodeKind::PromotionDecision,
        None,
        None,
        None,
        format!("Decision for candidate {}", req.candidate_id),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut valid_time,
        ref mut valid_time_source,
        ref mut user_context,
        ..
    } = decision_node
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some(valid_time_str);
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = decision_fields;
    }

    let decision_edge = user_context_edge(
        EdgeLabel::DecidedOn,
        &decision_id,
        &req.candidate_id,
        None,
        "Decision decided on PromoteCandidate",
    );

    generated_records.push(decision_node);
    generated_records.push(decision_edge);

    Ok(generated_records)
}
