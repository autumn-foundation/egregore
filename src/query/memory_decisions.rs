use crate::ir::{EvidenceLink, GraphRecord, NodeKind, UserContextScope};
use crate::redaction::redact_value;

fn scope_subset(a: &UserContextScope, b: &UserContextScope) -> bool {
    if let Some(r) = &a.repo {
        if Some(r) != b.repo.as_ref() {
            return false;
        }
    }
    if let Some(p) = &a.path_glob {
        if Some(p) != b.path_glob.as_ref() {
            return false;
        }
    }
    if let Some(l) = &a.language {
        if Some(l) != b.language.as_ref() {
            return false;
        }
    }
    if let Some(ph) = &a.lifecycle_phase {
        if Some(ph) != b.lifecycle_phase.as_ref() {
            return false;
        }
    }
    true
}

/// Checks if two UserContextScopes are compatible (one is subset of other or equal)
#[must_use]
pub fn scopes_compatible(a: &UserContextScope, b: &UserContextScope) -> bool {
    scope_subset(a, b) || scope_subset(b, a)
}

/// Checks if a record scope matches a query scope
#[must_use]
pub fn scope_matches(record_scope: &UserContextScope, query_scope: &UserContextScope) -> bool {
    if let (Some(r_repo), Some(q_repo)) = (&record_scope.repo, &query_scope.repo) {
        if r_repo != q_repo {
            return false;
        }
    }
    if let (Some(r_path), Some(q_path)) = (&record_scope.path_glob, &query_scope.path_glob) {
        if r_path != q_path {
            return false;
        }
    }
    if let (Some(r_lang), Some(q_lang)) = (&record_scope.language, &query_scope.language) {
        if r_lang != q_lang {
            return false;
        }
    }
    if let (Some(r_phase), Some(q_phase)) =
        (&record_scope.lifecycle_phase, &query_scope.lifecycle_phase)
    {
        if r_phase != q_phase {
            return false;
        }
    }
    true
}

/// Calculates Jaccard token similarity between two proposed rule texts
#[must_use]
pub fn jaccard_similarity(s1: &str, s2: &str) -> f64 {
    let normalize = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect()
    };
    let w1 = normalize(s1);
    let w2 = normalize(s2);
    if w1.is_empty() && w2.is_empty() {
        return 1.0;
    }
    let set1: std::collections::BTreeSet<String> = w1.into_iter().collect();
    let set2: std::collections::BTreeSet<String> = w2.into_iter().collect();

    let intersection = set1.intersection(&set2).count() as f64;
    let union = set1.union(&set2).count() as f64;
    intersection / union
}

/// Returns the latest decision node for the given candidate ID.
#[must_use]
pub fn latest_decision_for_candidate<'a>(
    records: &'a [GraphRecord],
    candidate_id: &str,
) -> Option<&'a GraphRecord> {
    records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                kind: NodeKind::PromotionDecision,
                user_context,
                ..
            } = r
            {
                user_context.candidate_id.as_deref() == Some(candidate_id)
            } else {
                false
            }
        })
        .max_by(|a, b| {
            let a_time = match a {
                GraphRecord::Node { user_context, .. } => {
                    user_context.decided_at.as_deref().unwrap_or("")
                }
                _ => "",
            };
            let b_time = match b {
                GraphRecord::Node { user_context, .. } => {
                    user_context.decided_at.as_deref().unwrap_or("")
                }
                _ => "",
            };
            if let (Ok(a_t), Ok(b_t)) = (
                chrono::DateTime::parse_from_rfc3339(a_time),
                chrono::DateTime::parse_from_rfc3339(b_time),
            ) {
                a_t.cmp(&b_t)
            } else {
                a_time.cmp(b_time)
            }
        })
}

/// Checks if a candidate is suppressed under the rejection debounce window
#[must_use]
pub fn is_candidate_suppressed(records: &[GraphRecord], cand_id: &str) -> Option<String> {
    let cand = records.iter().rfind(|r| r.id() == cand_id)?;
    let (cand_text, cand_scope, superseded_by_id) = match cand {
        GraphRecord::Node {
            user_context,
            superseded_by,
            ..
        } => {
            let text = user_context.proposed_rule_text.as_deref()?;
            let scope = user_context.scope.as_ref()?;
            (text, scope, superseded_by.as_deref())
        }
        _ => return None,
    };

    if let Some(old_id) = superseded_by_id {
        if let Some(old_rec) = records.iter().rfind(|r| r.id() == old_id) {
            let decision = records.iter().rfind(|r| {
                if let GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } = r
                {
                    user_context.candidate_id.as_deref() == Some(old_id)
                        && user_context.outcome.as_deref() == Some("rejected")
                } else {
                    false
                }
            });
            if let Some(GraphRecord::Node {
                user_context: decision_fields,
                ..
            }) = decision
            {
                if decision_fields.outcome.as_deref() == Some("rejected") {
                    let decided_at_str = decision_fields.decided_at.as_deref()?;
                    let current_time_str = match cand {
                        GraphRecord::Node { valid_time, .. } => valid_time.as_deref(),
                        _ => None,
                    }
                    .unwrap_or("");

                    let elapsed_ok = if let Ok(decided_at) =
                        chrono::DateTime::parse_from_rfc3339(decided_at_str)
                        && let Ok(current_time) =
                            chrono::DateTime::parse_from_rfc3339(current_time_str)
                    {
                        current_time.signed_duration_since(decided_at) >= chrono::Duration::days(30)
                    } else {
                        false
                    };

                    let old_obs: std::collections::BTreeSet<&str> = match old_rec {
                        GraphRecord::Node { user_context, .. } => user_context
                            .supporting_evidence
                            .as_deref()
                            .map(|v| {
                                v.iter()
                                    .filter_map(|l| l.target_record_id.as_deref())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => std::collections::BTreeSet::new(),
                    };

                    let new_obs: std::collections::BTreeSet<&str> = match cand {
                        GraphRecord::Node { user_context, .. } => user_context
                            .supporting_evidence
                            .as_deref()
                            .map(|v| {
                                v.iter()
                                    .filter_map(|l| l.target_record_id.as_deref())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => std::collections::BTreeSet::new(),
                    };

                    let additional_count = new_obs.difference(&old_obs).count();

                    if additional_count < 5 || !elapsed_ok {
                        return Some("rejection_debounce".to_string());
                    }
                }
            }
        }
    }

    for old_rec in records {
        let (old_id, old_text, old_scope) = match old_rec {
            GraphRecord::Node {
                id,
                kind: NodeKind::PromoteCandidate,
                user_context,
                ..
            } => {
                if let Some(text) = &user_context.proposed_rule_text
                    && let Some(scope) = &user_context.scope
                {
                    (id.as_str(), text.as_str(), scope)
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        if old_id == cand_id {
            continue;
        }
        if jaccard_similarity(cand_text, old_text) >= 0.6
            && scopes_compatible(cand_scope, old_scope)
        {
            let decision = records.iter().rfind(|r| {
                if let GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } = r
                {
                    user_context.candidate_id.as_deref() == Some(old_id)
                        && user_context.outcome.as_deref() == Some("rejected")
                } else {
                    false
                }
            });
            if let Some(GraphRecord::Node {
                user_context: decision_fields,
                ..
            }) = decision
            {
                if decision_fields.outcome.as_deref() == Some("rejected") {
                    let decided_at_str = decision_fields.decided_at.as_deref()?;
                    let current_time_str = match cand {
                        GraphRecord::Node { valid_time, .. } => valid_time.as_deref(),
                        _ => None,
                    }
                    .unwrap_or("");

                    let elapsed_ok = if let Ok(decided_at) =
                        chrono::DateTime::parse_from_rfc3339(decided_at_str)
                        && let Ok(current_time) =
                            chrono::DateTime::parse_from_rfc3339(current_time_str)
                    {
                        current_time.signed_duration_since(decided_at) >= chrono::Duration::days(30)
                    } else {
                        false
                    };

                    let old_obs: std::collections::BTreeSet<&str> = {
                        match old_rec {
                            GraphRecord::Node { user_context, .. } => user_context
                                .supporting_evidence
                                .as_deref()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|l| l.target_record_id.as_deref())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            _ => std::collections::BTreeSet::new(),
                        }
                    };

                    let new_obs: std::collections::BTreeSet<&str> = {
                        match cand {
                            GraphRecord::Node { user_context, .. } => user_context
                                .supporting_evidence
                                .as_deref()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|l| l.target_record_id.as_deref())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            _ => std::collections::BTreeSet::new(),
                        }
                    };

                    let additional_count = new_obs.difference(&old_obs).count();

                    if additional_count < 5 || !elapsed_ok {
                        return Some("rejection_debounce".to_string());
                    }
                }
            }
        }
    }

    None
}

/// Returns a list of pending candidates (those without a decision, or whose decision is deferred)
#[must_use]
pub fn pending_candidates<'a>(
    records: &'a [GraphRecord],
    query_scope: Option<&UserContextScope>,
) -> Vec<&'a GraphRecord> {
    let mut candidates = Vec::new();

    let mut terminal_candidates = std::collections::BTreeSet::new();
    let terminal_outcomes = ["approved", "edited_then_approved", "rejected", "expired"];

    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } = rec
            && let (Some(cand_id), Some(outcome)) =
                (&user_context.candidate_id, &user_context.outcome)
            && terminal_outcomes.contains(&outcome.as_str())
        {
            terminal_candidates.insert(cand_id.clone());
        }
    }

    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            ..
        } = rec
        {
            if terminal_candidates.contains(rec.id()) {
                continue;
            }
            if is_candidate_suppressed(records, rec.id()).is_some() {
                continue;
            }
            if let Some(q_scope) = query_scope {
                if let Some(r_scope) = &user_context.scope {
                    if !scope_matches(r_scope, q_scope) {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            candidates.push(rec);
        }
    }

    candidates.sort_by_key(|c| c.id());
    candidates
}

/// Returns active policy records matching the query scope
#[must_use]
pub fn active_policy<'a>(
    records: &'a [GraphRecord],
    query_scope: Option<&UserContextScope>,
) -> Vec<&'a GraphRecord> {
    let revoked_ids: std::collections::BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node {
                kind, user_context, ..
            } = r
            {
                if matches!(
                    kind,
                    NodeKind::Preference
                        | NodeKind::WorkflowRule
                        | NodeKind::NamingDecision
                        | NodeKind::Constraint
                ) && user_context.active_to.is_some()
                {
                    return Some(r.id());
                }
            }
            None
        })
        .collect();

    let mut policy_map = std::collections::BTreeMap::new();
    for rec in records {
        if let GraphRecord::Node {
            kind, user_context, ..
        } = rec
        {
            if matches!(
                kind,
                NodeKind::Preference
                    | NodeKind::WorkflowRule
                    | NodeKind::NamingDecision
                    | NodeKind::Constraint
            ) {
                if revoked_ids.contains(rec.id()) {
                    continue;
                }
                if user_context.active_to.is_some() {
                    continue;
                }
                if audit_trail(records, rec).is_err() {
                    continue;
                }
                if let Some(q_scope) = query_scope {
                    if let Some(r_scope) = &user_context.scope {
                        if !scope_matches(r_scope, q_scope) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                policy_map.insert(rec.id(), rec);
            }
        }
    }
    let mut policy: Vec<&'a GraphRecord> = policy_map.into_values().collect();
    policy.sort_by_key(|p| p.id());
    policy
}

fn validate_contradicting_evidence_links(
    candidate_id: &str,
    links: &[EvidenceLink],
    records: &[GraphRecord],
) -> std::result::Result<(), String> {
    for link in links {
        if link.target_domain != "user_context" || link.relation != "CONTRADICTS" {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence for '{}' must use target_domain 'user_context' and relation CONTRADICTS",
                candidate_id
            ));
        }
        let conf_val: f64 = link.confidence.parse().map_err(|_| {
            format!(
                "PromoteCandidate.contradicting_evidence[].confidence '{}' must be a numeric float string",
                link.confidence
            )
        })?;
        if !(0.0..=1.0).contains(&conf_val) {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence[].confidence '{}' must be in the range [0.0, 1.0]",
                link.confidence
            ));
        }
        let target_id = link.target_record_id.as_deref().ok_or_else(|| {
            "PromoteCandidate.contradicting_evidence[].target_record_id is missing".to_owned()
        })?;
        let target_node = records
            .iter()
            .rfind(|r| r.id() == target_id)
            .ok_or_else(|| format!("evidence target '{}' not found", target_id))?;
        let kind = match target_node {
            GraphRecord::Node { kind, .. } => *kind,
            _ => return Err(format!("evidence target '{}' is not a node", target_id)),
        };
        if !matches!(
            kind,
            NodeKind::Preference
                | NodeKind::WorkflowRule
                | NodeKind::NamingDecision
                | NodeKind::Constraint
        ) {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence target '{}' must be a Preference, WorkflowRule, NamingDecision, or Constraint, got {}",
                target_id,
                kind.as_str()
            ));
        }
    }
    Ok(())
}

fn validate_scope(
    scope: Option<&UserContextScope>,
    field_name: &str,
) -> std::result::Result<(), String> {
    let scope = scope.ok_or_else(|| format!("{} lacks scope", field_name))?;
    if let Some(lifecycle_phase) = scope.lifecycle_phase.as_deref() {
        if !["pre_commit", "pre_pr", "pre_merge", "runtime", "any"].contains(&lifecycle_phase) {
            return Err(format!(
                "{} has invalid lifecycle_phase '{}'",
                field_name, lifecycle_phase
            ));
        }
    }
    Ok(())
}

fn validate_durable_fields(
    kind: NodeKind,
    user_context: &crate::ir::UserContextFields,
    durable_id: &str,
) -> std::result::Result<(), String> {
    match kind {
        NodeKind::Preference => {
            if user_context.proposed_rule_kind.as_deref() != Some("preference") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'preference'",
                    durable_id
                ));
            }
            let text = user_context.rule_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!("Durable record '{}' lacks rule_text", durable_id));
            }
        }
        NodeKind::WorkflowRule => {
            if user_context.proposed_rule_kind.as_deref() != Some("workflow_rule") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'workflow_rule'",
                    durable_id
                ));
            }
            let text = user_context.rule_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!("Durable record '{}' lacks rule_text", durable_id));
            }
            let triggers = user_context
                .triggers
                .as_ref()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("Durable record '{}' lacks triggers", durable_id))?;
            for trigger in triggers {
                if trigger.is_empty() {
                    return Err(format!(
                        "Durable record '{}' has empty trigger entry",
                        durable_id
                    ));
                }
                if ![
                    "pre_commit",
                    "pre_pr",
                    "pre_merge",
                    "pre_command",
                    "post_command",
                ]
                .contains(&trigger.as_str())
                {
                    return Err(format!(
                        "Durable record '{}' trigger '{}' is invalid",
                        durable_id, trigger
                    ));
                }
            }
            let action_summary = user_context.action_summary.as_deref().unwrap_or("");
            if action_summary.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks action_summary",
                    durable_id
                ));
            }
        }
        NodeKind::NamingDecision => {
            if user_context.proposed_rule_kind.as_deref() != Some("naming_decision") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'naming_decision'",
                    durable_id
                ));
            }
            let entity_kind = user_context.entity_kind.as_deref().unwrap_or("");
            if ![
                "crate", "module", "type", "function", "field", "feature", "other",
            ]
            .contains(&entity_kind)
            {
                return Err(format!(
                    "Durable record '{}' entity_kind '{}' is invalid",
                    durable_id, entity_kind
                ));
            }
            let name = user_context.canonical_name.as_deref().unwrap_or("");
            if name.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks canonical_name",
                    durable_id
                ));
            }
            if user_context.alternatives_rejected.is_none() {
                return Err(format!(
                    "Durable record '{}' lacks alternatives_rejected",
                    durable_id
                ));
            }
        }
        NodeKind::Constraint => {
            if user_context.proposed_rule_kind.as_deref() != Some("constraint") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'constraint'",
                    durable_id
                ));
            }
            let text = user_context.constraint_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks constraint_text",
                    durable_id
                ));
            }
            let level = user_context.enforcement_level.as_deref().unwrap_or("");
            if !["advisory", "blocking"].contains(&level) {
                return Err(format!(
                    "Durable record '{}' enforcement_level '{}' is invalid",
                    durable_id, level
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Traces the approval chain back to supporting observations
///
/// # Errors
///
/// Returns an error if any hop in the chain is missing, stale, or ambiguous.
pub fn audit_trail<'a>(
    records: &'a [GraphRecord],
    durable: &'a GraphRecord,
) -> std::result::Result<Vec<&'a GraphRecord>, String> {
    let mut chain = Vec::new();

    let durable_id = durable.id();
    let kind = match durable {
        GraphRecord::Node { kind, .. } => *kind,
        _ => return Err(format!("Durable record '{}' is not a node", durable_id)),
    };
    if !matches!(
        kind,
        NodeKind::Preference
            | NodeKind::WorkflowRule
            | NodeKind::NamingDecision
            | NodeKind::Constraint
    ) {
        return Err(format!(
            "Record '{}' is not a durable user-context policy node",
            durable_id
        ));
    }
    chain.push(durable);

    let user_context = match durable {
        GraphRecord::Node { user_context, .. } => user_context,
        _ => unreachable!(),
    };

    validate_scope(
        user_context.scope.as_ref(),
        &format!("Durable record '{}'", durable_id),
    )?;
    validate_durable_fields(kind, user_context, durable_id)?;

    let decision_id = user_context
        .approval_decision_id
        .as_deref()
        .ok_or_else(|| format!("Durable record '{}' lacks approval_decision_id", durable_id))?;

    let decision = records
        .iter()
        .rfind(|r| r.id() == decision_id)
        .ok_or_else(|| {
            format!(
                "Approval decision '{}' not found for durable record '{}'",
                decision_id, durable_id
            )
        })?;
    chain.push(decision);

    let decision_fields = match decision {
        GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } => user_context,
        _ => {
            return Err(format!(
                "Decision '{}' is not a PromotionDecision node",
                decision_id
            ));
        }
    };
    let decided_by = decision_fields
        .decided_by
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks decided_by", decision_id))?;
    if decided_by.is_empty() {
        return Err(format!("Decision '{}' has empty decided_by", decision_id));
    }
    let outcome = decision_fields
        .outcome
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks outcome", decision_id))?;
    if outcome != "approved" && outcome != "edited_then_approved" {
        return Err(format!(
            "Decision '{}' has non-approving outcome '{}'",
            decision_id, outcome
        ));
    }
    let mat_id = decision_fields
        .materialized_record_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks materialized_record_id", decision_id))?;
    if mat_id != durable_id {
        return Err(format!(
            "Decision '{}' targets materialized record '{}', expected '{}'",
            decision_id, mat_id, durable_id
        ));
    }
    let decided_at = decision_fields.decided_at.as_deref();
    let active_from = user_context.active_from.as_deref();

    let decided_at_str =
        decided_at.ok_or_else(|| format!("Decision '{}' lacks decided_at", decision_id))?;
    let active_from_str =
        active_from.ok_or_else(|| format!("Durable record '{}' lacks active_from", durable_id))?;

    let decided_at_parsed = chrono::DateTime::parse_from_rfc3339(decided_at_str)
        .map_err(|e| format!("Decision '{}' has invalid decided_at: {}", decision_id, e))?;
    let active_from_parsed =
        chrono::DateTime::parse_from_rfc3339(active_from_str).map_err(|e| {
            format!(
                "Durable record '{}' has invalid active_from: {}",
                durable_id, e
            )
        })?;

    if decided_at_parsed != active_from_parsed {
        return Err(format!(
            "Durable record '{}' active_from '{}' does not match decision '{}' decided_at '{}'",
            durable_id, active_from_str, decision_id, decided_at_str
        ));
    }
    let prompt_id = decision_fields
        .prompt_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks prompt_id", decision_id))?;
    let prompt = records
        .iter()
        .rfind(|r| r.id() == prompt_id)
        .ok_or_else(|| {
            format!(
                "PromotionPrompt '{}' not found for decision '{}'",
                prompt_id, decision_id
            )
        })?;
    chain.push(prompt);

    let prompt_fields = match prompt {
        GraphRecord::Node {
            kind: NodeKind::PromotionPrompt,
            user_context,
            ..
        } => user_context,
        _ => {
            return Err(format!(
                "Prompt '{}' is not a PromotionPrompt node",
                prompt_id
            ));
        }
    };
    let candidate_id = prompt_fields
        .candidate_id
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks candidate_id", prompt_id))?;
    let surface = prompt_fields
        .prompt_surface
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompt_surface", prompt_id))?;
    if !["cli", "mcp", "web", "other"].contains(&surface) {
        return Err(format!(
            "Prompt '{}' prompt_surface '{}' is invalid",
            prompt_id, surface
        ));
    }
    let prompt_text = prompt_fields
        .prompt_text
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompt_text", prompt_id))?;
    if prompt_text.is_empty() {
        return Err(format!("Prompt '{}' has empty prompt_text", prompt_id));
    }
    let prompted_at = prompt_fields
        .prompted_at
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompted_at", prompt_id))?;
    chrono::DateTime::parse_from_rfc3339(prompted_at)
        .map_err(|e| format!("Prompt '{}' has invalid prompted_at: {}", prompt_id, e))?;
    let prompted_to = prompt_fields
        .prompted_to
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompted_to", prompt_id))?;
    if prompted_to.is_empty() {
        return Err(format!("Prompt '{}' has empty prompted_to", prompt_id));
    }
    let decision_candidate_id = decision_fields
        .candidate_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks candidate_id", decision_id))?;
    if decision_candidate_id != candidate_id {
        return Err(format!(
            "Decision '{}' targets candidate '{}', but prompt '{}' targets candidate '{}'",
            decision_id, decision_candidate_id, prompt_id, candidate_id
        ));
    }
    let candidate = records
        .iter()
        .rfind(|r| r.id() == candidate_id)
        .ok_or_else(|| {
            format!(
                "PromoteCandidate '{}' not found for prompt '{}'",
                candidate_id, prompt_id
            )
        })?;
    chain.push(candidate);

    let (candidate_fields, cand_confidence, cand_evidence_quality) = match candidate {
        GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            confidence,
            evidence_quality,
            ..
        } => (
            user_context,
            confidence.as_deref(),
            evidence_quality.as_deref(),
        ),
        _ => {
            return Err(format!(
                "Candidate '{}' is not a PromoteCandidate node",
                candidate_id
            ));
        }
    };

    let conf_str = cand_confidence
        .ok_or_else(|| format!("PromoteCandidate '{}' lacks confidence", candidate_id))?;
    let conf_val: f64 = conf_str.parse().map_err(|_| {
        format!(
            "PromoteCandidate '{}' confidence '{}' must be a numeric float string",
            candidate_id, conf_str
        )
    })?;
    if !(0.0..=1.0).contains(&conf_val) {
        return Err(format!(
            "PromoteCandidate '{}' confidence '{}' must be in the range [0.0, 1.0]",
            candidate_id, conf_str
        ));
    }

    let eq_str = cand_evidence_quality
        .ok_or_else(|| format!("PromoteCandidate '{}' lacks evidence_quality", candidate_id))?;
    if !["verbatim", "summarized", "referenced_only"].contains(&eq_str) {
        return Err(format!(
            "PromoteCandidate '{}' evidence_quality '{}' is invalid (must be verbatim, summarized, or referenced_only)",
            candidate_id, eq_str
        ));
    }

    validate_scope(
        candidate_fields.scope.as_ref(),
        &format!("PromoteCandidate '{}'", candidate_id),
    )?;

    let contradicting = candidate_fields
        .contradicting_evidence
        .as_ref()
        .ok_or_else(|| {
            format!(
                "PromoteCandidate '{}' lacks contradicting_evidence",
                candidate_id
            )
        })?;
    validate_contradicting_evidence_links(candidate_id, contradicting, records)?;

    // Perform candidate-kind/body consistency checks
    let expected_kind = match kind {
        NodeKind::Preference => "preference",
        NodeKind::WorkflowRule => "workflow_rule",
        NodeKind::NamingDecision => "naming_decision",
        NodeKind::Constraint => "constraint",
        _ => unreachable!(),
    };
    if candidate_fields.proposed_rule_kind.as_deref() != Some(expected_kind) {
        return Err(format!(
            "Candidate '{}' proposed rule kind '{:?}' does not match durable policy kind '{:?}'",
            candidate_id, candidate_fields.proposed_rule_kind, kind
        ));
    }

    let is_edited = outcome == "edited_then_approved";
    match kind {
        NodeKind::Preference | NodeKind::WorkflowRule => {
            let durable_text = user_context.rule_text.as_deref().unwrap_or("");
            if is_edited {
                let edited_text = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_text != edited_text {
                    return Err(format!(
                        "Durable record rule_text '{}' does not match decision edited_rule_text '{}'",
                        durable_text, edited_text
                    ));
                }
            } else {
                let cand_text = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_text = redact_value(cand_text);
                if durable_text != redacted_cand_text {
                    return Err(format!(
                        "Durable record rule_text '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_text, cand_text, redacted_cand_text
                    ));
                }
            }
        }
        NodeKind::Constraint => {
            let durable_text = user_context.constraint_text.as_deref().unwrap_or("");
            if is_edited {
                let edited_text = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_text != edited_text {
                    return Err(format!(
                        "Durable record constraint_text '{}' does not match decision edited_rule_text '{}'",
                        durable_text, edited_text
                    ));
                }
            } else {
                let cand_text = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_text = redact_value(cand_text);
                if durable_text != redacted_cand_text {
                    return Err(format!(
                        "Durable record constraint_text '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_text, cand_text, redacted_cand_text
                    ));
                }
            }
        }
        NodeKind::NamingDecision => {
            let durable_name = user_context.canonical_name.as_deref().unwrap_or("");
            if is_edited {
                let edited_name = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_name != edited_name {
                    return Err(format!(
                        "Durable record canonical_name '{}' does not match decision edited_rule_text '{}'",
                        durable_name, edited_name
                    ));
                }
            } else {
                let cand_name = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_name = redact_value(cand_name);
                if durable_name != redacted_cand_name {
                    return Err(format!(
                        "Durable record canonical_name '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_name, cand_name, redacted_cand_name
                    ));
                }
            }
        }
        _ => unreachable!(),
    }
    let supporting = candidate_fields
        .supporting_evidence
        .as_deref()
        .ok_or_else(|| format!("Candidate '{}' lacks supporting_evidence", candidate_id))?;

    let mut unique_supporting_targets = std::collections::BTreeSet::new();
    let mut sessions = std::collections::BTreeSet::new();
    let mut obs_nodes = Vec::new();
    for link in supporting {
        if link.target_domain != "agent_memory" {
            return Err(format!(
                "Candidate '{}' supporting evidence target domain '{}' is invalid (must be 'agent_memory')",
                candidate_id, link.target_domain
            ));
        }
        if link.relation != "PROPOSED_BY" {
            return Err(format!(
                "Candidate '{}' supporting evidence relation '{}' is invalid (must be 'PROPOSED_BY')",
                candidate_id, link.relation
            ));
        }
        let conf_val: f64 = link.confidence.parse().map_err(|_| {
            format!(
                "Candidate '{}' supporting evidence link confidence '{}' must be a numeric float string",
                candidate_id, link.confidence
            )
        })?;
        if !(0.0..=1.0).contains(&conf_val) {
            return Err(format!(
                "Candidate '{}' supporting evidence link confidence '{}' must be in the range [0.0, 1.0]",
                candidate_id, link.confidence
            ));
        }
        let obs_id = link.target_record_id.as_deref().ok_or_else(|| {
            format!(
                "Candidate '{}' supporting evidence link lacks target_record_id",
                candidate_id
            )
        })?;
        let obs = records.iter().rfind(|r| r.id() == obs_id).ok_or_else(|| {
            format!(
                "Supporting observation '{}' not found for candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        let session_id = match obs {
            GraphRecord::Node {
                kind, session_id, ..
            } => {
                if !matches!(
                    kind,
                    NodeKind::Observation | NodeKind::AgentTurn | NodeKind::Decision
                ) {
                    return Err(format!(
                        "Supporting evidence '{}' has invalid node kind '{:?}' (must be Observation, AgentTurn, or Decision)",
                        obs_id, kind
                    ));
                }
                session_id.as_deref()
            }
            _ => {
                return Err(format!("Supporting evidence '{}' is not a node", obs_id));
            }
        };
        let sess = session_id.ok_or_else(|| {
            format!(
                "supporting_evidence.session_id is required for evidence target '{}' in candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        if sess.is_empty() {
            return Err(format!(
                "supporting_evidence.session_id is required for evidence target '{}' in candidate '{}'",
                obs_id, candidate_id
            ));
        }
        if unique_supporting_targets.insert(obs_id.to_owned()) {
            sessions.insert(sess.to_owned());
            obs_nodes.push(obs);
        }
    }

    if unique_supporting_targets.len() < 3 {
        return Err(format!(
            "Candidate '{}' has {} unique supporting observations; at least 3 are required",
            candidate_id,
            unique_supporting_targets.len()
        ));
    }
    if sessions.len() < 2 {
        return Err(format!(
            "Candidate '{}' has evidence from {} distinct sessions; at least 2 are required",
            candidate_id,
            sessions.len()
        ));
    }

    obs_nodes.sort_by_key(|o| o.id());
    chain.extend(obs_nodes);

    Ok(chain)
}

// ── Memory Evidence Audit Queries (Issue #64) ──────────────────────────────────
