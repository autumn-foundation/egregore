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
    GraphRecord, NodeKind, UserContextFields, UserContextScope,
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

    // Refuse decisions for suppressed candidates
    if let Some(suppressed_reason) =
        crate::query::is_candidate_suppressed(records, &req.candidate_id)
    {
        return Err(anyhow!(
            "Cannot decide candidate '{}': {}",
            req.candidate_id,
            suppressed_reason
        ));
    }

    // Reject new decisions if any prior decision has a terminal outcome
    let terminal_outcomes = ["approved", "edited_then_approved", "rejected", "expired"];
    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } = rec
            && user_context.candidate_id.as_deref() == Some(req.candidate_id.as_str())
            && let Some(outcome) = &user_context.outcome
            && terminal_outcomes.contains(&outcome.as_str())
        {
            return Err(anyhow!(
                "Cannot decide candidate '{}' because it already has a terminal decision outcome: '{}'",
                req.candidate_id,
                outcome
            ));
        }
    }

    let valid_time_str = req
        .transaction_time
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));

    let (cand_confidence, cand_evidence_quality, cand_superseded_by) = match candidate {
        GraphRecord::Node {
            confidence,
            evidence_quality,
            superseded_by,
            ..
        } => (
            confidence.as_deref(),
            evidence_quality.as_deref(),
            superseded_by.as_deref(),
        ),
        _ => unreachable!(),
    };

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

    let allowed_prompt_surfaces = ["cli", "mcp", "web", "other"];
    if !allowed_prompt_surfaces.contains(&req.prompt_surface.as_str()) {
        return Err(anyhow!(
            "Invalid prompt_surface '{}'. Allowed values are: cli, mcp, web, other",
            req.prompt_surface
        ));
    }

    if req.prompted_to.trim().is_empty() {
        return Err(anyhow!("prompted_to cannot be empty or whitespace-only"));
    }
    if req.decided_by.trim().is_empty() {
        return Err(anyhow!("decided_by cannot be empty or whitespace-only"));
    }

    if req.outcome == "approved" || req.outcome == "edited_then_approved" {
        // Validate PromoteCandidate metadata and evidence offline
        let conf_str =
            cand_confidence.ok_or_else(|| anyhow!("PromoteCandidate.confidence (required)"))?;
        let conf_val: f64 = conf_str.parse().map_err(|_| {
            anyhow!(
                "PromoteCandidate.confidence '{}' must be a numeric float string",
                conf_str
            )
        })?;
        if !(0.0..=1.0).contains(&conf_val) {
            return Err(anyhow!(
                "PromoteCandidate.confidence '{}' must be in the range [0.0, 1.0]",
                conf_str
            ));
        }

        let eq_str = cand_evidence_quality
            .ok_or_else(|| anyhow!("PromoteCandidate.evidence_quality (required)"))?;
        if !["verbatim", "summarized", "referenced_only"].contains(&eq_str) {
            return Err(anyhow!(
                "Invalid PromoteCandidate.evidence_quality '{}'. Allowed values are: verbatim, summarized, referenced_only",
                eq_str
            ));
        }

        let scope = cand_fields.scope.as_ref().ok_or_else(|| {
            anyhow!(
                "PromoteCandidate '{}' lacks required scope",
                req.candidate_id
            )
        })?;
        if let Some(lifecycle_phase) = scope
            .lifecycle_phase
            .as_deref()
            .filter(|&p| !["pre_commit", "pre_pr", "pre_merge", "runtime", "any"].contains(&p))
        {
            return Err(anyhow!(
                "Invalid scope.lifecycle_phase '{}'. Allowed values are: pre_commit, pre_pr, pre_merge, runtime, any",
                lifecycle_phase
            ));
        }

        let supporting = cand_fields
            .supporting_evidence
            .as_deref()
            .ok_or_else(|| anyhow!("PromoteCandidate.supporting_evidence (required)"))?;
        let mut unique_supporting_targets = std::collections::BTreeSet::new();
        let mut sessions = std::collections::BTreeSet::new();
        for link in supporting {
            if link.target_domain != "agent_memory" || link.relation != "PROPOSED_BY" {
                return Err(anyhow!(
                    "PromoteCandidate '{}' supporting_evidence must use target_domain 'agent_memory' and relation PROPOSED_BY",
                    req.candidate_id
                ));
            }
            let conf_val: f64 = link.confidence.parse().map_err(|_| {
                anyhow!("PromoteCandidate.supporting_evidence[].confidence '{}' must be a numeric float string", link.confidence)
            })?;
            if !(0.0..=1.0).contains(&conf_val) {
                return Err(anyhow!(
                    "PromoteCandidate.supporting_evidence[].confidence '{}' must be in the range [0.0, 1.0]",
                    link.confidence
                ));
            }
            let target_id = link.target_record_id.as_deref().ok_or_else(|| {
                anyhow!("PromoteCandidate.supporting_evidence[].target_record_id is required")
            })?;
            let target_node = records
                .iter()
                .find(|r| r.id() == target_id)
                .ok_or_else(|| anyhow!("supporting evidence target '{}' not found", target_id))?;
            let (target_kind, target_session_id) = match target_node {
                GraphRecord::Node {
                    kind, session_id, ..
                } => (*kind, session_id.as_deref()),
                _ => {
                    return Err(anyhow!(
                        "supporting evidence target '{}' is not a node record",
                        target_id
                    ));
                }
            };
            if !matches!(
                target_kind,
                NodeKind::Observation | NodeKind::AgentTurn | NodeKind::Decision
            ) {
                return Err(anyhow!(
                    "supporting evidence target '{}' must be an Observation, AgentTurn, or Decision",
                    target_id
                ));
            }
            let session_id_str = target_session_id.ok_or_else(|| {
                anyhow!(
                    "supporting_evidence.session_id is required for evidence target '{}'",
                    target_id
                )
            })?;
            if session_id_str.is_empty() {
                return Err(anyhow!(
                    "supporting_evidence.session_id is required for evidence target '{}'",
                    target_id
                ));
            }
            if unique_supporting_targets.insert(target_id) {
                sessions.insert(session_id_str.to_owned());
            }
        }

        if unique_supporting_targets.len() < 3 {
            return Err(anyhow!(
                "PromoteCandidate '{}' has {} unique supporting observations; at least 3 are required",
                req.candidate_id,
                unique_supporting_targets.len()
            ));
        }
        if sessions.len() < 2 {
            return Err(anyhow!(
                "PromoteCandidate '{}' has evidence from {} distinct sessions; at least 2 are required",
                req.candidate_id,
                sessions.len()
            ));
        }

        if proposed_rule_kind == "revocation" {
            let contradicting = cand_fields.contradicting_evidence.as_deref().ok_or_else(|| {
                anyhow!("PromoteCandidate.contradicting_evidence is required for revocation candidates")
            })?;
            for link in contradicting {
                if link.target_domain != "user_context" || link.relation != "CONTRADICTS" {
                    return Err(anyhow!(
                        "PromoteCandidate.contradicting_evidence for '{}' must use target_domain 'user_context' and relation CONTRADICTS",
                        req.candidate_id
                    ));
                }
                let conf_val: f64 = link.confidence.parse().map_err(|_| {
                    anyhow!("PromoteCandidate.contradicting_evidence[].confidence '{}' must be a numeric float string", link.confidence)
                })?;
                if !(0.0..=1.0).contains(&conf_val) {
                    return Err(anyhow!(
                        "PromoteCandidate.contradicting_evidence[].confidence '{}' must be in the range [0.0, 1.0]",
                        link.confidence
                    ));
                }
                let target_id = link.target_record_id.as_deref().ok_or_else(|| {
                    anyhow!(
                        "PromoteCandidate.contradicting_evidence[].target_record_id is required"
                    )
                })?;
                let target_node = records
                    .iter()
                    .find(|r| r.id() == target_id)
                    .ok_or_else(|| anyhow!("evidence target '{}' not found", target_id))?;
                let target_kind = match target_node {
                    GraphRecord::Node { kind, .. } => *kind,
                    _ => {
                        return Err(anyhow!(
                            "evidence target '{}' is not a node record",
                            target_id
                        ));
                    }
                };
                let allowed_target_kinds = [
                    NodeKind::Preference,
                    NodeKind::WorkflowRule,
                    NodeKind::NamingDecision,
                    NodeKind::Constraint,
                ];
                if !allowed_target_kinds.contains(&target_kind) {
                    return Err(anyhow!(
                        "PromoteCandidate.contradicting_evidence target '{}' must be a Preference, WorkflowRule, NamingDecision, or Constraint, got {}",
                        target_id,
                        target_kind.as_str()
                    ));
                }
            }
        }

        if let Some(rejected_id) = cand_superseded_by {
            let rejected_node = records
                .iter()
                .find(|r| r.id() == rejected_id)
                .ok_or_else(|| anyhow!("superseded candidate '{}' not found", rejected_id))?;
            let target_kind = match rejected_node {
                GraphRecord::Node { kind, .. } => *kind,
                _ => {
                    return Err(anyhow!(
                        "superseded candidate '{}' is not a node record",
                        rejected_id
                    ));
                }
            };
            if target_kind != NodeKind::PromoteCandidate {
                return Err(anyhow!(
                    "superseded candidate '{}' is not a PromoteCandidate node",
                    rejected_id
                ));
            }
            let has_rejection = records.iter().any(|r| match r {
                GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } => {
                    user_context.candidate_id.as_deref() == Some(rejected_id)
                        && user_context.outcome.as_deref() == Some("rejected")
                }
                _ => false,
            });
            if !has_rejection {
                return Err(anyhow!(
                    "PromoteCandidate '{}' superseded_by '{}' must reference a rejected candidate",
                    req.candidate_id,
                    rejected_id
                ));
            }
        }

        if proposed_rule_kind == "workflow_rule" {
            let allowed_triggers = [
                "pre_commit",
                "pre_pr",
                "pre_merge",
                "pre_command",
                "post_command",
            ];
            let triggers_ok = cand_fields.triggers.as_ref().is_some_and(|t| {
                !t.is_empty() && t.iter().all(|s| allowed_triggers.contains(&s.as_str()))
            });
            let action_summary_ok = cand_fields
                .action_summary
                .as_ref()
                .is_some_and(|s| !s.trim().is_empty());
            if !triggers_ok || !action_summary_ok {
                return Err(anyhow!(
                    "Approved workflow_rule candidate must carry valid triggers and an action_summary"
                ));
            }
        }

        if proposed_rule_kind == "naming_decision" {
            let entity_kind_ok = cand_fields.entity_kind.as_ref().is_some_and(|k| {
                matches!(
                    k.as_str(),
                    "crate" | "module" | "type" | "function" | "field" | "feature" | "other"
                )
            });
            if !entity_kind_ok {
                return Err(anyhow!(
                    "Approved naming_decision candidate must carry a valid entity_kind ('crate', 'module', 'type', 'function', 'field', 'feature', 'other')"
                ));
            }

            let canonical_name_ok = if req.outcome == "edited_then_approved" {
                true
            } else {
                cand_fields
                    .canonical_name
                    .as_ref()
                    .is_some_and(|n| !n.trim().is_empty())
            };
            if !canonical_name_ok {
                return Err(anyhow!(
                    "Approved naming_decision candidate must carry a non-empty canonical_name"
                ));
            }
            if req.outcome == "approved" {
                let prop_text = cand_fields
                    .proposed_rule_text
                    .as_deref()
                    .unwrap_or("")
                    .trim();
                let canon_name = cand_fields.canonical_name.as_deref().unwrap_or("").trim();
                if prop_text != canon_name {
                    return Err(anyhow!(
                        "Approved naming_decision candidate proposed_rule_text ('{}') must match canonical_name ('{}')",
                        prop_text,
                        canon_name
                    ));
                }
            }
        }

        if proposed_rule_kind == "constraint" {
            let enforcement_level_ok = cand_fields
                .enforcement_level
                .as_ref()
                .is_some_and(|l| matches!(l.as_str(), "advisory" | "blocking"));
            if !enforcement_level_ok {
                return Err(anyhow!(
                    "Approved constraint candidate must carry a valid enforcement_level ('advisory' or 'blocking')"
                ));
            }
        }
    }

    if req.outcome == "edited_then_approved" {
        let is_empty = req
            .edited_rule_text
            .as_ref()
            .is_none_or(|s| s.trim().is_empty());
        if is_empty {
            return Err(anyhow!(
                "non-empty edited_rule_text is required when outcome is edited_then_approved"
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
        ref mut redaction_policy_version,
        ..
    } = prompt_node
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some(valid_time_str.clone());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        let redacted_prompt_text = redact_value(&prompt_text);
        if crate::redaction::is_redacted(&redacted_prompt_text) {
            *redaction_policy_version = Some(crate::redaction::REDACTION_POLICY_VERSION.to_owned());
        }
        *user_context = UserContextFields {
            candidate_id: Some(req.candidate_id.clone()),
            prompt_surface: Some(req.prompt_surface.clone()),
            prompt_text: Some(redacted_prompt_text),
            prompted_at: Some(prompted_at),
            prompted_to: Some(req.prompted_to.clone()),
            ..UserContextFields::empty()
        };
    }

    // 4. Generate PromotionDecision
    let decided_at = valid_time_str.clone();
    let decision_hash =
        blake3_hash_parts(&[&req.candidate_id, &prompt_id, &decided_at, &req.outcome]);
    let decision_id = user_context_stable_id(&["decision", &decision_hash]);

    let mut generated_records = vec![prompt_node];

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
        let mut updated_candidate = candidate.clone();
        let mut redacted_any = false;
        if let GraphRecord::Node {
            user_context,
            redaction_policy_version,
            ..
        } = &mut updated_candidate
        {
            if let Some(text) = &user_context.proposed_rule_text {
                let redacted_text = redact_value(text);
                if crate::redaction::is_redacted(&redacted_text) {
                    redacted_any = true;
                }
                user_context.proposed_rule_text = Some(redacted_text);
            }
            if let Some(name) = &user_context.canonical_name {
                let redacted_name = redact_value(name);
                if crate::redaction::is_redacted(&redacted_name) {
                    redacted_any = true;
                }
                user_context.canonical_name = Some(redacted_name);
            }
            if let Some(action) = &user_context.action_summary {
                let redacted_action = redact_value(action);
                if crate::redaction::is_redacted(&redacted_action) {
                    redacted_any = true;
                }
                user_context.action_summary = Some(redacted_action);
            }
            if redacted_any {
                *redaction_policy_version =
                    Some(crate::redaction::REDACTION_POLICY_VERSION.to_owned());
            }
        }

        let updated_cand_fields = match &updated_candidate {
            GraphRecord::Node { user_context, .. } => user_context,
            _ => unreachable!(),
        };

        let rule_text = if req.outcome == "edited_then_approved" {
            req.edited_rule_text.clone().unwrap()
        } else {
            let text = updated_cand_fields
                .proposed_rule_text
                .clone()
                .ok_or_else(|| anyhow!("Candidate proposed_rule_text is missing"))?;
            if text.trim().is_empty() {
                return Err(anyhow!(
                    "Candidate proposed_rule_text cannot be empty or whitespace-only"
                ));
            }
            text
        };

        let redacted_rule_text = redact_value(&rule_text);

        let scope = updated_cand_fields.scope.clone().ok_or_else(|| {
            anyhow!(
                "PromoteCandidate '{}' lacks required scope",
                req.candidate_id
            )
        })?;

        if proposed_rule_kind == "revocation" {
            // Find target record to revoke.
            // In revocation candidates, the evidence link or contradicts links back to the durable record.
            let target_durable_id = updated_cand_fields
                .contradicting_evidence
                .as_deref()
                .and_then(|v| v.first().and_then(|l| l.target_record_id.clone()))
                .or_else(|| {
                    // Fall back to matches in supporting evidence
                    updated_cand_fields
                        .supporting_evidence
                        .as_deref()
                        .and_then(|v| {
                            v.iter()
                                .find(|l| l.target_domain == "user_context")
                                .and_then(|l| l.target_record_id.clone())
                        })
                })
                .ok_or_else(|| {
                    anyhow!("Revocation candidate does not specify target record to revoke")
                })?;

            let target_copies: Vec<&GraphRecord> = records
                .iter()
                .filter(|r| r.id() == target_durable_id)
                .collect();
            if target_copies.is_empty() {
                return Err(anyhow!(
                    "Durable record '{}' to revoke not found",
                    target_durable_id
                ));
            }

            let already_revoked = target_copies.iter().any(|r| {
                if let GraphRecord::Node { user_context, .. } = r {
                    user_context.active_to.is_some()
                } else {
                    false
                }
            });
            if already_revoked {
                return Err(anyhow!(
                    "Revocation target '{}' is already inactive/revoked",
                    target_durable_id
                ));
            }

            let mut original_record = target_copies
                .iter()
                .copied()
                .find(|r| {
                    if let GraphRecord::Node { user_context, .. } = r {
                        user_context.active_to.is_none()
                    } else {
                        false
                    }
                })
                .ok_or_else(|| {
                    anyhow!(
                        "No active copy of durable record '{}' found to revoke",
                        target_durable_id
                    )
                })?
                .clone();

            let is_policy_kind = match &original_record {
                GraphRecord::Node { kind, .. } => matches!(
                    kind,
                    NodeKind::Preference
                        | NodeKind::WorkflowRule
                        | NodeKind::NamingDecision
                        | NodeKind::Constraint
                ),
                _ => false,
            };
            if !is_policy_kind {
                return Err(anyhow!(
                    "Revocation target '{}' is not a durable policy record",
                    target_durable_id
                ));
            }

            // Verify that the revocation target has a valid approval chain back to observations:
            crate::query::audit_trail(records, &original_record).map_err(|e| {
                anyhow!(
                    "Revocation target '{}' has no valid approval chain: {}",
                    target_durable_id,
                    e
                )
            })?;

            if let GraphRecord::Node {
                ref mut user_context,
                ..
            } = original_record
            {
                user_context.active_to = Some(decided_at);
            }

            decision_fields.materialized_record_id = Some(target_durable_id);

            generated_records.push(original_record);
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
                NodeKind::Preference => blake3_hash_parts(&[
                    &redacted_rule_text,
                    &canonical_scope(&scope),
                    &decision_id,
                ]),
                NodeKind::WorkflowRule => blake3_hash_parts(&[
                    &redacted_rule_text,
                    &canonical_scope(&scope),
                    &decision_id,
                ]),
                NodeKind::NamingDecision => {
                    let entity_kind = updated_cand_fields
                        .entity_kind
                        .as_deref()
                        .unwrap_or("other");
                    blake3_hash_parts(&[
                        entity_kind,
                        &redacted_rule_text,
                        &canonical_scope(&scope),
                        &decision_id,
                    ])
                }
                NodeKind::Constraint => blake3_hash_parts(&[
                    &redacted_rule_text,
                    &canonical_scope(&scope),
                    &decision_id,
                ]),
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

            let mut rule_has_redaction = crate::redaction::is_redacted(&redacted_rule_text);
            if let GraphRecord::Node {
                ref mut schema_version,
                ref mut domain,
                ref mut valid_time,
                ref mut valid_time_source,
                ref mut user_context,
                ref mut redaction_policy_version,
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
                        durable_fields.rule_text = Some(redacted_rule_text);
                        if materialized_kind == NodeKind::WorkflowRule {
                            durable_fields.triggers = updated_cand_fields.triggers.clone();
                            let redacted_action = updated_cand_fields
                                .action_summary
                                .as_ref()
                                .map(|s| redact_value(s));
                            if redacted_action
                                .as_ref()
                                .is_some_and(|s| crate::redaction::is_redacted(s))
                            {
                                rule_has_redaction = true;
                            }
                            durable_fields.action_summary = redacted_action;
                        }
                    }
                    NodeKind::NamingDecision => {
                        durable_fields.entity_kind = updated_cand_fields.entity_kind.clone();
                        let canonical = redacted_rule_text;
                        if crate::redaction::is_redacted(&canonical) {
                            rule_has_redaction = true;
                        }
                        durable_fields.canonical_name = Some(canonical);
                        durable_fields.alternatives_rejected = Some(
                            updated_cand_fields
                                .alternatives_rejected
                                .clone()
                                .unwrap_or_default(),
                        );
                    }
                    NodeKind::Constraint => {
                        durable_fields.constraint_text = Some(redacted_rule_text);
                        durable_fields.enforcement_level =
                            updated_cand_fields.enforcement_level.clone();
                    }
                    _ => unreachable!(),
                }

                *user_context = durable_fields;
                if rule_has_redaction {
                    *redaction_policy_version =
                        Some(crate::redaction::REDACTION_POLICY_VERSION.to_owned());
                }
            }

            decision_fields.materialized_record_id = Some(materialized_id);
            if req.outcome == "edited_then_approved" {
                decision_fields.edited_rule_text = Some(redact_value(&rule_text));
            }

            generated_records.push(materialized_node);
        }

        if redacted_any {
            generated_records.push(updated_candidate);
        }
    }

    let decision_has_redaction = decision_fields
        .decision_rationale
        .as_deref()
        .is_some_and(crate::redaction::is_redacted)
        || decision_fields
            .edited_rule_text
            .as_deref()
            .is_some_and(crate::redaction::is_redacted);

    let mut decision_node = GraphRecord::node(
        decision_id,
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
        ref mut redaction_policy_version,
        ..
    } = decision_node
    {
        *schema_version = USER_CONTEXT_SCHEMA_VERSION;
        *domain = Some("user_context".to_owned());
        *valid_time = Some(valid_time_str);
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *user_context = decision_fields;
        if decision_has_redaction {
            *redaction_policy_version = Some(crate::redaction::REDACTION_POLICY_VERSION.to_owned());
        }
    }

    generated_records.push(decision_node);

    Ok(generated_records)
}

/// Synthesize user-context edges for direct write to an embedded store.
pub fn synthesize_user_context_edges(
    records: &[GraphRecord],
    generated: &[GraphRecord],
) -> Vec<GraphRecord> {
    let mut edges = Vec::new();
    let mut prompt_id = None;
    let mut decision_id = None;
    let mut candidate_id = None;
    let mut outcome = None;
    let mut materialized_id = None;

    for rec in generated {
        if let GraphRecord::Node {
            id,
            kind,
            user_context,
            ..
        } = rec
        {
            match kind {
                NodeKind::PromotionPrompt => {
                    prompt_id = Some(id.clone());
                    candidate_id = user_context.candidate_id.clone();
                }
                NodeKind::PromotionDecision => {
                    decision_id = Some(id.clone());
                    outcome = user_context.outcome.clone();
                    materialized_id = user_context.materialized_record_id.clone();
                }
                _ => {}
            }
        }
    }

    if let (Some(p_id), Some(c_id)) = (prompt_id, &candidate_id) {
        let edge_id = user_context_stable_id(&["edge", "PROMPTED_FOR", &p_id, c_id]);
        edges.push(GraphRecord::Edge {
            id: edge_id,
            schema_version: USER_CONTEXT_SCHEMA_VERSION,
            label: crate::ir::EdgeLabel::PromptedFor,
            source: p_id,
            target: c_id.clone(),
            confidence: None,
            temporal: None,
            summary: "PromotionPrompt prompted for PromoteCandidate".to_owned(),
            producer: None,
        });
    }

    if let (Some(d_id), Some(c_id)) = (decision_id, &candidate_id) {
        let edge_id = user_context_stable_id(&["edge", "DECIDED_ON", &d_id, c_id]);
        edges.push(GraphRecord::Edge {
            id: edge_id,
            schema_version: USER_CONTEXT_SCHEMA_VERSION,
            label: crate::ir::EdgeLabel::DecidedOn,
            source: d_id.clone(),
            target: c_id.clone(),
            confidence: None,
            temporal: None,
            summary: "PromotionDecision decided on PromoteCandidate".to_owned(),
            producer: None,
        });

        if let Some(mat_id) = materialized_id {
            let out_ref = outcome.as_deref().unwrap_or("");
            if out_ref == "approved" || out_ref == "edited_then_approved" {
                let is_revocation = records.iter().any(|r| match r {
                    GraphRecord::Node {
                        id, user_context, ..
                    } if id == c_id => {
                        user_context.proposed_rule_kind.as_deref() == Some("revocation")
                    }
                    _ => false,
                });

                if is_revocation {
                    let edge_id = user_context_stable_id(&["edge", "REVOKED_BY", &mat_id, &d_id]);
                    edges.push(GraphRecord::Edge {
                        id: edge_id,
                        schema_version: USER_CONTEXT_SCHEMA_VERSION,
                        label: crate::ir::EdgeLabel::RevokedBy,
                        source: mat_id,
                        target: d_id,
                        confidence: None,
                        temporal: None,
                        summary: "PromotionDecision revoked durable user-context record".to_owned(),
                        producer: None,
                    });
                } else {
                    let edge_id =
                        user_context_stable_id(&["edge", "MATERIALIZED_AS", &d_id, &mat_id]);
                    edges.push(GraphRecord::Edge {
                        id: edge_id,
                        schema_version: USER_CONTEXT_SCHEMA_VERSION,
                        label: crate::ir::EdgeLabel::MaterializedAs,
                        source: d_id,
                        target: mat_id,
                        confidence: None,
                        temporal: None,
                        summary: "PromotionDecision materialized durable user-context record"
                            .to_owned(),
                        producer: None,
                    });
                }
            }
        }
    }

    edges
}
