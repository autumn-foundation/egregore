//! `eg query task-list` — list project tasks filtered by status (issue #119).

use super::*;

use crate::query::{
    TASK_LIST_DISCLAIMER, TASK_LIST_TRUST, TaskListCounts, TaskListGroup, TaskListOutcome,
    parse_task_list_status_filter, task_list_report,
};

/// Lane-level trust for the single-domain trust table in `docs/cli/query.md`:
/// rows name in-flight work, never work proven done.
pub(crate) const TASK_LIST_TRUST_LANE: &str = TASK_LIST_TRUST;

#[derive(serde::Serialize)]
struct TaskListEnvelope<'a> {
    ok: bool,
    lane: &'static str,
    trust: &'static str,
    disclaimer: &'static str,
    status_filter: &'a [String],
    counts: &'a TaskListCounts,
    groups: &'a [TaskListGroup],
    zero_matches: bool,
}

#[derive(serde::Serialize)]
struct TaskListError<'a> {
    code: &'a str,
    message: String,
}

#[derive(serde::Serialize)]
struct TaskListErrorEnvelope<'a> {
    ok: bool,
    error: TaskListError<'a>,
}

pub(crate) fn query_task_list_cmd(
    records: &[GraphRecord],
    status: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Malformed `--status` filters fail before any store is touched: the
    // input is malformed regardless of what the graph contains.
    let filter = match parse_task_list_status_filter(status) {
        Ok(filter) => filter,
        Err(err) => {
            let envelope = TaskListErrorEnvelope {
                ok: false,
                error: TaskListError {
                    code: err.code(),
                    message: err.to_string(),
                },
            };
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(3);
        }
    };
    let status_filter: Vec<String> = filter.iter().cloned().collect();
    let outcome = task_list_report(records, &filter);
    match &outcome {
        TaskListOutcome::NoProjectData => {
            let envelope = TaskListErrorEnvelope {
                ok: false,
                error: TaskListError {
                    code: "no_project_data",
                    message:
                        "no Task records in the project domain: import tasks first (eg import-local-tasks)"
                            .to_owned(),
                },
            };
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(2);
        }
        TaskListOutcome::Report(report) => match format {
            OutputFormat::Json => {
                let envelope = TaskListEnvelope {
                    ok: true,
                    lane: "task-list",
                    trust: TASK_LIST_TRUST_LANE,
                    disclaimer: TASK_LIST_DISCLAIMER,
                    status_filter: &status_filter,
                    counts: &report.counts,
                    groups: &report.groups,
                    zero_matches: report.zero_matches,
                };
                println!("{}", serde_json::to_string(&envelope)?);
            }
            OutputFormat::Text => print_task_list_text(report, &status_filter),
        },
    }
    Ok(())
}

fn print_task_list_text(report: &crate::query::TaskListReport, status_filter: &[String]) {
    if report.zero_matches {
        println!(
            "task-list: zero matching tasks ({} tasks total; statuses: {})",
            report.counts.tasks,
            status_filter.join(",")
        );
    } else {
        println!(
            "task-list: {} matching task(s) in {} status group(s) ({} tasks total; statuses: {})",
            report.counts.matched,
            report.groups.len(),
            report.counts.tasks,
            status_filter.join(",")
        );
    }
    println!("{TASK_LIST_DISCLAIMER}");
    for group in &report.groups {
        println!("[{}]", group.status);
        for row in &group.tasks {
            println!(
                "  {}  {}  {}  {}",
                row.record_id,
                row.source_kind.as_deref().unwrap_or("-"),
                row.source_handle.as_deref().unwrap_or("-"),
                row.title.as_deref().unwrap_or("-"),
            );
        }
    }
}
