//! `eg query task-overlap` — in-flight task footprint-overlap analysis (issue #150).

use super::*;

use crate::query::{
    OverlapPairRow, TASK_OVERLAP_DISCLAIMER, TASK_OVERLAP_TRUST, TaskOverlapCounts,
    TaskOverlapDiagnostic, TaskOverlapOutcome, parse_overlap_status_filter, task_overlap_exit_code,
    task_overlap_report,
};

/// Lane-level trust for the single-domain trust table in `docs/cli/query.md`:
/// the rows are inspection leads, not facts.
pub(crate) const TASK_OVERLAP_TRUST_LANE: &str = TASK_OVERLAP_TRUST;

#[derive(serde::Serialize)]
struct TaskOverlapEnvelope<'a> {
    ok: bool,
    lane: &'static str,
    trust: &'static str,
    disclaimer: &'static str,
    status_filter: &'a [String],
    counts: &'a TaskOverlapCounts,
    pairs: &'a [OverlapPairRow],
    diagnostics: &'a [TaskOverlapDiagnostic],
    zero_overlaps: bool,
}

#[derive(serde::Serialize)]
struct TaskOverlapError<'a> {
    code: &'a str,
    message: String,
}

#[derive(serde::Serialize)]
struct TaskOverlapErrorEnvelope<'a> {
    ok: bool,
    error: TaskOverlapError<'a>,
}

pub(crate) fn query_task_overlap_cmd(
    records: &[GraphRecord],
    status: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Malformed `--status` filters fail before any store is touched: the
    // input is malformed regardless of what the graph contains.
    let filter = match parse_overlap_status_filter(status) {
        Ok(filter) => filter,
        Err(err) => {
            let envelope = TaskOverlapErrorEnvelope {
                ok: false,
                error: TaskOverlapError {
                    code: err.code(),
                    message: err.to_string(),
                },
            };
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(3);
        }
    };
    let status_filter: Vec<String> = filter.iter().cloned().collect();
    let outcome = task_overlap_report(records, &filter);
    let exit_code = task_overlap_exit_code(&outcome);
    match &outcome {
        TaskOverlapOutcome::NoProjectData => {
            let envelope = TaskOverlapErrorEnvelope {
                ok: false,
                error: TaskOverlapError {
                    code: "no_project_data",
                    message: "no Task records in the project domain: import tasks first (eg import-local-tasks)".to_owned(),
                },
            };
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(2);
        }
        TaskOverlapOutcome::Report(report) => {
            match format {
                OutputFormat::Json => {
                    let envelope = TaskOverlapEnvelope {
                        ok: true,
                        lane: "task-overlap",
                        trust: TASK_OVERLAP_TRUST_LANE,
                        disclaimer: TASK_OVERLAP_DISCLAIMER,
                        status_filter: &status_filter,
                        counts: &report.counts,
                        pairs: &report.pairs,
                        diagnostics: &report.diagnostics,
                        zero_overlaps: report.zero_overlaps,
                    };
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => print_task_overlap_text(report, &status_filter),
            }
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
    }
    Ok(())
}

fn print_task_overlap_text(report: &crate::query::TaskOverlapReport, status_filter: &[String]) {
    if report.zero_overlaps {
        println!(
            "task-overlap: zero overlapping in-flight task pairs ({} in-flight of {} tasks; statuses: {})",
            report.counts.in_flight_tasks,
            report.counts.tasks,
            status_filter.join(",")
        );
    } else {
        println!(
            "task-overlap: {} overlapping pair(s) across {} in-flight tasks ({} tasks total; statuses: {})",
            report.counts.pairs,
            report.counts.in_flight_tasks,
            report.counts.tasks,
            status_filter.join(",")
        );
    }
    println!("{TASK_OVERLAP_DISCLAIMER}");
    if !report.pairs.is_empty() {
        println!("\nOVERLAPPING PAIRS (inspection leads)");
        for pair in &report.pairs {
            let a = &pair.task_a;
            let b = &pair.task_b;
            println!(
                "  {} [{}] {}",
                a.record_id,
                a.status,
                a.source_handle.as_deref().unwrap_or("-")
            );
            println!(
                "  {} [{}] {}",
                b.record_id,
                b.status,
                b.source_handle.as_deref().unwrap_or("-")
            );
            for handle in &pair.shared_handles {
                let span = handle
                    .span
                    .as_deref()
                    .map(|s| format!(" {s}"))
                    .unwrap_or_default();
                let path = handle
                    .repo_relative_path
                    .as_deref()
                    .map(|p| format!(" {p}"))
                    .unwrap_or_default();
                println!(
                    "    shared: {} ({}{}){} [{}]",
                    handle.record_id,
                    handle.kind,
                    path,
                    span,
                    handle.via.join("+")
                );
            }
        }
    }
    if !report.diagnostics.is_empty() {
        println!("\nDIAGNOSTICS");
        for diag in &report.diagnostics {
            println!(
                "  [{}] {}: {}",
                diag.code.code_as_str(),
                diag.task_record_id.as_deref().unwrap_or("-"),
                diag.message
            );
        }
    }
}
