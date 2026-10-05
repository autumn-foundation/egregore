//! `eg query belief-timeline <TARGET>` (issue #235).
//!
//! Trust separation: the timeline replays statuses computed solely from
//! author-written `superseded_by` / `SUPERSEDES` / `CONTRADICTS` data already
//! in the store, through the shared temporal resolver. It never judges which
//! belief is true — a contradiction keeps both sides visible — and it emits
//! metadata only (record IDs, handles, timestamps, confidence, statuses):
//! no record bodies ever leave this lane.

use super::*;
use std::fmt::Write as _;

/// Runs the belief-timeline lane and prints the result envelope.
pub(crate) fn query_belief_timeline_cmd(
    records: &[GraphRecord],
    target_handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let target = match query::resolve_belief_target(records, target_handle, index, repo_scope) {
        Ok(target) => target,
        Err(err) => {
            let (code, envelope) = match err {
                query::BeliefTargetError::Unsupported { handle, message } => (
                    1,
                    serde_json::json!({
                        "ok": false,
                        "error": { "code": "unsupported_handle", "handle": handle, "message": message },
                    }),
                ),
                query::BeliefTargetError::Unknown { handle } => (
                    2,
                    serde_json::json!({
                        "ok": false,
                        "error": { "code": "no_match", "handle": handle },
                    }),
                ),
                query::BeliefTargetError::Stale { handle } => (
                    2,
                    serde_json::json!({
                        "ok": false,
                        "error": { "code": "stale_handle", "handle": handle },
                    }),
                ),
                query::BeliefTargetError::NoSymbolAtLine { handle, path, line } => (
                    2,
                    serde_json::json!({
                        "ok": false,
                        "error": { "code": "no_symbol_at_line", "handle": handle, "path": path, "line": line },
                    }),
                ),
                query::BeliefTargetError::Ambiguous { handle, candidates } => (
                    2,
                    serde_json::json!({
                        "ok": false,
                        "error": { "code": "ambiguous", "handle": handle, "candidates": candidates },
                    }),
                ),
            };
            eprintln!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(code);
        }
    };

    let timeline = query::belief_timeline(records, &target);

    match format {
        OutputFormat::Json => {
            let response = serde_json::json!({
                "ok": true,
                "lane": "belief-timeline",
                "timeline": timeline,
            });
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize belief timeline")?;
            println!("{output}");
        }
        OutputFormat::Text => {
            print!("{}", render_belief_timeline_text(&timeline));
        }
    }
    Ok(())
}

/// Renders the timeline as a stable, human-readable column list. Never prints
/// record bodies — the timeline itself carries none.
fn render_belief_timeline_text(timeline: &query::BeliefTimeline) -> String {
    let target = &timeline.target;
    let kind = match target.kind {
        query::BeliefTargetKind::Symbol => "symbol",
        query::BeliefTargetKind::File => "file",
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "belief timeline for {} ({kind}:{})",
        target.target_handle, target.record_id
    );
    let _ = writeln!(
        out,
        "{} entr{} — ordering: {}",
        timeline.entry_count,
        if timeline.entry_count == 1 {
            "y"
        } else {
            "ies"
        },
        timeline.ordering
    );
    for entry in &timeline.entries {
        let when = entry
            .observed_at
            .as_deref()
            .or(entry.ingested_at.as_deref())
            .unwrap_or("no-timestamp");
        let relation = entry.relation.map_or("-", |r| match r {
            query::BeliefRelation::Supersedes => "SUPERSEDES",
            query::BeliefRelation::Contradicts => "CONTRADICTS",
        });
        let status = match entry.status {
            query::BeliefStatus::Current => "current",
            query::BeliefStatus::Superseded => "superseded",
            query::BeliefStatus::Contradicted => "contradicted",
        };
        let forward = entry
            .forward_record_id
            .as_deref()
            .map(|id| {
                let handle = entry.forward_handle.as_deref().unwrap_or("?");
                format!(" -> {id} ({handle})")
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{when}  {}  {}  {status}{forward}  [{relation}]",
            entry.handle, entry.kind
        );
    }
    for diagnostic in &timeline.diagnostics {
        let members = if diagnostic.record_ids.is_empty() {
            diagnostic
                .record_id
                .clone()
                .or_else(|| diagnostic.forward_record_id.clone())
                .unwrap_or_default()
        } else {
            diagnostic.record_ids.join(", ")
        };
        let _ = writeln!(
            out,
            "diagnostic {}: {members} — {}",
            diagnostic.code, diagnostic.message
        );
    }
    out
}
