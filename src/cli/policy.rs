use super::*;

pub(crate) fn query_policy_cmd(
    records: &[GraphRecord],
    scope: &crate::UserContextScope,
    format: OutputFormat,
) -> Result<()> {
    let scope_filter = if scope.repo.is_none()
        && scope.path_glob.is_none()
        && scope.language.is_none()
        && scope.lifecycle_phase.is_none()
    {
        None
    } else {
        Some(scope)
    };

    let list = crate::query::active_policy(records, scope_filter);
    let mut cli_policy = Vec::new();
    for rec in list {
        if let GraphRecord::Node {
            id,
            kind,
            user_context,
            ..
        } = rec
        {
            cli_policy.push(serde_json::json!({
                "id": id,
                "kind": kind.as_str(),
                "rule_text": user_context.rule_text,
                "constraint_text": user_context.constraint_text,
                "canonical_name": user_context.canonical_name,
                "entity_kind": user_context.entity_kind,
                "approval_decision_id": user_context.approval_decision_id,
                "scope": user_context.scope,
                "active_from": user_context.active_from,
                "triggers": user_context.triggers,
                "action_summary": user_context.action_summary,
                "alternatives_rejected": user_context.alternatives_rejected,
                "enforcement_level": user_context.enforcement_level,
            }));
        }
    }

    match format {
        OutputFormat::Json => {
            let out = serde_json::json!({
                "ok": true,
                "policy": cli_policy,
            });
            println!("{}", serde_json::to_string(&out)?);
        }
        OutputFormat::Text => {
            for p in cli_policy {
                let id = p["id"].as_str().unwrap_or("");
                let kind = p["kind"].as_str().unwrap_or("");
                let body = p["rule_text"]
                    .as_str()
                    .or_else(|| p["constraint_text"].as_str())
                    .or_else(|| p["canonical_name"].as_str())
                    .unwrap_or("");
                println!("{id}: [{kind}] {body}");
            }
        }
    }
    Ok(())
}
