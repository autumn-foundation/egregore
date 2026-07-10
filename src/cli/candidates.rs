use super::*;

pub(crate) fn query_candidates_cmd(records: &[GraphRecord], format: OutputFormat) -> Result<()> {
    let list = crate::query::pending_candidates(records, None);
    let mut cli_candidates = Vec::new();
    for rec in list {
        if let GraphRecord::Node {
            id,
            user_context,
            confidence,
            ..
        } = rec
        {
            let suppressed = crate::query::is_candidate_suppressed(records, id);
            cli_candidates.push(serde_json::json!({
                "id": id,
                "proposed_rule_text": user_context.proposed_rule_text,
                "proposed_rule_kind": user_context.proposed_rule_kind,
                "confidence": confidence.as_deref().and_then(|c| c.parse::<f64>().ok()),
                "scope": user_context.scope,
                "supporting_evidence": user_context.supporting_evidence,
                "suppressed": suppressed,
            }));
        }
    }

    match format {
        OutputFormat::Json => {
            let out = serde_json::json!({
                "ok": true,
                "candidates": cli_candidates,
            });
            println!("{}", serde_json::to_string(&out)?);
        }
        OutputFormat::Text => {
            for c in cli_candidates {
                let id = c["id"].as_str().unwrap_or("");
                let rule_text = c["proposed_rule_text"].as_str().unwrap_or("");
                let kind = c["proposed_rule_kind"].as_str().unwrap_or("");
                let conf = c["confidence"].as_f64().unwrap_or(0.0);
                let supp_str = c["suppressed"]
                    .as_str()
                    .map(|s| format!(" [suppressed: {s}]"))
                    .unwrap_or_default();
                println!("{id}: [{kind}] {rule_text} (confidence: {conf}){supp_str}");
            }
        }
    }
    Ok(())
}
