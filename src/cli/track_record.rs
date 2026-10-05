//! `eg query track-record` (alias `eg query agents`) command implementation.

use anyhow::Result;
use serde::Serialize;

use crate::cli::OutputFormat;
use crate::ir::GraphRecord;
use crate::query::{
    AgentTrackRecord, CitableCount, PromotionVerdictBucket, TRACK_RECORD_DISCLAIMER,
    TRACK_RECORD_LANE, TrackRecordDiagnostic, TrackRecordReport, agent_track_record,
};

/// Query the per-agent track record over the downstream fate of observations.
///
/// Read-only and local: computes the lane over the loaded records and
/// prints JSON (default) or text.
pub(crate) fn query_track_record_cmd(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let report = agent_track_record(records, repo_id);

    match format {
        OutputFormat::Json => {
            let envelope = TrackRecordEnvelope {
                ok: true,
                lane: TRACK_RECORD_LANE,
                repository_id: repo_id.map(str::to_owned),
                disclaimer: TRACK_RECORD_DISCLAIMER,
                agents: report.agents,
                diagnostics: report.diagnostics,
            };
            println!("{}", serde_json::to_string(&envelope)?);
        }
        OutputFormat::Text => {
            print_track_record_text(&report, repo_id);
        }
    }
    Ok(())
}

/// JSON envelope for the track-record lane.
#[derive(Debug, Serialize)]
struct TrackRecordEnvelope {
    ok: bool,
    lane: &'static str,
    repository_id: Option<String>,
    disclaimer: &'static str,
    agents: Vec<AgentTrackRecord>,
    diagnostics: Vec<TrackRecordDiagnostic>,
}

/// Sanitize one agent id for text output: collapse all whitespace
/// (including hostile newlines) to middle dots so a forged newline can
/// never start a new line.
fn sanitize_agent_id(agent_id: &str) -> String {
    agent_id.split_whitespace().collect::<Vec<_>>().join("·")
}

/// Deterministic, byte-stable text rendering.
///
/// Agents are already canonically sorted by `agent_id` from the core lane;
/// this renderer preserves that order and sorts nothing else.
fn print_track_record_text(report: &TrackRecordReport, repo_id: Option<&str>) {
    match repo_id {
        Some(repo) => println!("agent track record for repository {repo}"),
        None => println!("agent track record"),
    }
    println!("disclaimer: {TRACK_RECORD_DISCLAIMER}");
    if report.agents.is_empty() {
        println!("no agents with qualifying records");
    }
    for agent in &report.agents {
        println!("agent: {}", sanitize_agent_id(&agent.agent_id));
        println!("  kind: {}", agent.agent_kind);
        print_bucket("  observations_written", &agent.observations_written);
        println!("  promotion_outcomes:");
        print_promotion_bucket("    approved", &agent.promotion_outcomes.approved);
        print_promotion_bucket(
            "    edited_then_approved",
            &agent.promotion_outcomes.edited_then_approved,
        );
        print_promotion_bucket("    rejected", &agent.promotion_outcomes.rejected);
        print_promotion_bucket("    deferred", &agent.promotion_outcomes.deferred);
        print_promotion_bucket("    expired", &agent.promotion_outcomes.expired);
        print_bucket("  superseded_observations", &agent.superseded_observations);
        println!("  verification_outcomes:");
        print_bucket("    passed", &agent.verification_outcomes.passed);
        print_bucket("    failed", &agent.verification_outcomes.failed);
        print_bucket(
            "    inconclusive",
            &agent.verification_outcomes.inconclusive,
        );
    }
    if report.diagnostics.is_empty() {
        println!("diagnostics: none");
    } else {
        println!("diagnostics:");
        for diagnostic in &report.diagnostics {
            println!("  {}: {}", diagnostic.code, diagnostic.message);
            if diagnostic.record_ids.is_empty() {
                println!("    records: (none)");
            } else {
                println!("    records: {}", diagnostic.record_ids.join(", "));
            }
        }
    }
}

fn print_bucket(label: &str, bucket: &CitableCount) {
    if bucket.record_ids.is_empty() {
        println!("{label}: {}", bucket.count);
    } else {
        println!(
            "{label}: {} [{}]",
            bucket.count,
            bucket.record_ids.join(", ")
        );
    }
}

fn print_promotion_bucket(label: &str, bucket: &PromotionVerdictBucket) {
    if bucket.candidate_ids.is_empty() {
        println!("{label}: {}", bucket.count);
    } else {
        println!(
            "{label}: {} candidates [{}] decisions [{}]",
            bucket.count,
            bucket.candidate_ids.join(", "),
            bucket.decision_ids.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_collapses_hostile_whitespace() {
        assert_eq!(
            sanitize_agent_id("zzz-agent\nINJECTED"),
            "zzz-agent·INJECTED"
        );
        assert_eq!(sanitize_agent_id("  spaced\tout  "), "spaced·out");
    }
}
