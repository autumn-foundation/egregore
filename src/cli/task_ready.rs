//! `eg query task-ready` — ready-to-dispatch task resolution (issue #161).

use super::*;

use crate::query::{
    BlockedTaskRow, TaskIdentityRow, TaskReadyCounts, TaskReadyDiagnostic, TaskReadyOutcome,
    task_ready_exit_code, task_ready_report,
};

/// Lane-level trust for the single-domain trust table in `docs/cli/query.md`:
/// the rows are a dispatch-eligibility lead, not a fact.
pub(crate) const TASK_READY_TRUST: &str = "eligibility_lead";

const TASK_READY_DISCLAIMER: &str = "Readiness is prerequisite eligibility only: every declared dependency is closed_completed. It is not a judgment that a task is correct, safe, or non-colliding. Pair with footprint-overlap analysis (issue #150) before dispatching parallel work: dispatch now requires eligible AND non-colliding.";

#[derive(serde::Serialize)]
struct TaskReadyEnvelope<'a> {
    ok: bool,
    lane: &'static str,
    trust: &'static str,
    disclaimer: &'static str,
    counts: &'a TaskReadyCounts,
    ready: &'a [TaskIdentityRow],
    blocked: &'a [BlockedTaskRow],
    author_blocked: &'a [TaskIdentityRow],
    diagnostics: &'a [TaskReadyDiagnostic],
}

#[derive(serde::Serialize)]
struct NoProjectDataError<'a> {
    code: &'a str,
    message: &'a str,
}

#[derive(serde::Serialize)]
struct NoProjectDataEnvelope<'a> {
    ok: bool,
    error: NoProjectDataError<'a>,
}

pub(crate) fn query_task_ready_cmd(records: &[GraphRecord], format: OutputFormat) -> Result<()> {
    let outcome = task_ready_report(records);
    let exit_code = task_ready_exit_code(&outcome);
    match &outcome {
        TaskReadyOutcome::NoProjectData => {
            let envelope = NoProjectDataEnvelope {
                ok: false,
                error: NoProjectDataError {
                    code: "no_project_data",
                    message: "no Task records in the project domain: import tasks first (eg import-local-tasks)",
                },
            };
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(2);
        }
        TaskReadyOutcome::Ready(report) => {
            match format {
                OutputFormat::Json => {
                    let envelope = TaskReadyEnvelope {
                        ok: true,
                        lane: "task-ready",
                        trust: TASK_READY_TRUST,
                        disclaimer: TASK_READY_DISCLAIMER,
                        counts: &report.counts,
                        ready: &report.ready,
                        blocked: &report.blocked,
                        author_blocked: &report.author_blocked,
                        diagnostics: &report.diagnostics,
                    };
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => print_task_ready_text(report),
            }
            if exit_code == 3 {
                eprintln!(
                    "task-ready: dependency cycle detected ({} diagnostic(s)); no task in a cycle is dispatch-eligible",
                    report.diagnostics.len()
                );
                std::process::exit(3);
            }
        }
    }
    Ok(())
}

fn print_task_ready_text(report: &crate::query::TaskReadyReport) {
    println!(
        "task-ready: {} ready, {} blocked, {} author-blocked ({} tasks, {} dependency edges)",
        report.counts.ready,
        report.counts.blocked,
        report.counts.author_blocked,
        report.counts.tasks,
        report.counts.dependency_edges
    );
    println!("{TASK_READY_DISCLAIMER}");
    if !report.ready.is_empty() {
        println!("\nREADY");
        for row in &report.ready {
            println!(
                "  {}  [{}] {}",
                row.record_id,
                row.status,
                row.title.as_deref().unwrap_or("(untitled)")
            );
            if let Some(handle) = row.source_handle.as_deref() {
                println!("    {handle}");
            }
        }
    }
    if !report.blocked.is_empty() {
        println!("\nBLOCKED");
        for row in &report.blocked {
            let cycle_mark = if row.in_cycle {
                " [in dependency cycle]"
            } else {
                ""
            };
            println!(
                "  {}  [{}]{cycle_mark} {}",
                row.task.record_id,
                row.task.status,
                row.task.title.as_deref().unwrap_or("(untitled)")
            );
            for unmet in &row.unmet_dependencies {
                match unmet.resolution {
                    crate::query::UnmetResolution::Resolved => {
                        println!(
                            "    blocked by {} [{}] {}",
                            unmet.record_id.as_deref().unwrap_or("?"),
                            unmet.status.as_deref().unwrap_or("unknown"),
                            unmet.title.as_deref().unwrap_or("(untitled)")
                        );
                    }
                    crate::query::UnmetResolution::Unresolved => {
                        println!(
                            "    blocked by unknown task '{}' (declared in {})",
                            unmet.declared_local_id.as_deref().unwrap_or("?"),
                            unmet.source_handle.as_deref().unwrap_or("?")
                        );
                    }
                    crate::query::UnmetResolution::TargetAbsent => {
                        println!(
                            "    blocked by absent record {}",
                            unmet.record_id.as_deref().unwrap_or("?")
                        );
                    }
                }
            }
        }
    }
    if !report.diagnostics.is_empty() {
        println!("\nDIAGNOSTICS");
        for diag in &report.diagnostics {
            println!("  [{}] {}", diag.code.code_as_str(), diag.message);
        }
    }
    if report.ready.is_empty() && report.blocked.is_empty() && report.author_blocked.is_empty() {
        println!("\nno dispatch candidates");
    }
}
