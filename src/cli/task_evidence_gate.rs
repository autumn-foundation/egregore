//! `eg query task-evidence-gate` — gate task completion on live verification
//! evidence for every acceptance criterion (issue #147).

use super::*;

use crate::query::{
    CriterionGateRow, NotReadyReason, TaskEvidenceGateOutcome, task_evidence_gate_exit_code,
    task_evidence_gate_report,
};

/// Lane-level trust for the single-domain trust table in `docs/cli/query.md`:
/// the verdict is a completion-gating lead, not a correctness claim.
pub(crate) const TASK_EVIDENCE_GATE_TRUST: &str = "evidence_gate";

const TASK_EVIDENCE_GATE_DISCLAIMER: &str = "The verdict gates on recorded verification evidence only: every acceptance criterion needs a passing verification record whose cited code has not drifted since the run. It is not a claim the task is correct, safe, or complete.";

#[derive(serde::Serialize)]
struct TaskEvidenceGateEnvelope<'a> {
    ok: bool,
    lane: &'static str,
    trust: &'static str,
    disclaimer: &'static str,
    task: &'a crate::query::GateTaskRow,
    ready: bool,
    criteria: &'a [CriterionGateRow],
    counts: &'a crate::query::TaskEvidenceGateCounts,
}

#[derive(serde::Serialize)]
struct NoSuchTaskError<'a> {
    code: &'a str,
    task_id: &'a str,
}

#[derive(serde::Serialize)]
struct NoSuchTaskEnvelope<'a> {
    ok: bool,
    error: NoSuchTaskError<'a>,
}

pub(crate) fn query_task_evidence_gate_cmd(
    records: &[GraphRecord],
    id_or_handle: &str,
    format: OutputFormat,
) -> Result<()> {
    // Handle dereferencing reuses `eg query task`'s resolver (issue #160).
    let resolved_ids = match query::resolve_task_ids(records, id_or_handle) {
        Ok(ids) => ids,
        Err(query::TaskResolveError::Ambiguous { handle, candidates }) => {
            let err_json =
                serde_json::to_string(&query::TaskResolveError::Ambiguous { handle, candidates })?;
            eprintln!("{err_json}");
            std::process::exit(1);
        }
        Err(query::TaskResolveError::Unsupported { handle, message }) => {
            let err_json =
                serde_json::to_string(&query::TaskResolveError::Unsupported { handle, message })?;
            eprintln!("{err_json}");
            std::process::exit(1);
        }
    };

    // `resolve_task_ids` errors on ambiguity, so at most one live task remains.
    let Some(task_id) = resolved_ids.iter().next() else {
        print_no_such_task(id_or_handle)?;
        std::process::exit(2);
    };

    let outcome = task_evidence_gate_report(records, task_id);
    let exit_code = task_evidence_gate_exit_code(&outcome);
    match &outcome {
        TaskEvidenceGateOutcome::NoSuchTask => {
            print_no_such_task(id_or_handle)?;
            std::process::exit(2);
        }
        TaskEvidenceGateOutcome::Ready(report) => match format {
            OutputFormat::Json => {
                let envelope = TaskEvidenceGateEnvelope {
                    ok: true,
                    lane: "task-evidence-gate",
                    trust: TASK_EVIDENCE_GATE_TRUST,
                    disclaimer: TASK_EVIDENCE_GATE_DISCLAIMER,
                    task: &report.task,
                    ready: report.ready,
                    criteria: &report.criteria,
                    counts: &report.counts,
                };
                println!("{}", serde_json::to_string(&envelope)?);
            }
            OutputFormat::Text => print_task_evidence_gate_text(report),
        },
    }
    debug_assert_eq!(exit_code, 0, "a produced verdict always exits 0");
    Ok(())
}

fn print_no_such_task(id_or_handle: &str) -> Result<()> {
    let envelope = NoSuchTaskEnvelope {
        ok: false,
        error: NoSuchTaskError {
            code: "no_match",
            task_id: id_or_handle,
        },
    };
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

const fn reason_description(reason: NotReadyReason) -> &'static str {
    match reason {
        NotReadyReason::MissingEvidence => {
            "no live verification record is linked to this criterion"
        }
        NotReadyReason::FailingEvidence => {
            "a linked verification record's recorded outcome is not passing"
        }
        NotReadyReason::StaleEvidence => {
            "no passing record has a confirmed-current code basis (drifted, removed, or unanchored)"
        }
    }
}

fn print_task_evidence_gate_text(report: &crate::query::TaskEvidenceGateReport) {
    let verdict = if report.ready { "READY" } else { "NOT READY" };
    println!(
        "task-evidence-gate: {} [{}] {} — {verdict} ({}/{} criteria satisfied)",
        report.task.record_id,
        report.task.status.as_deref().unwrap_or("unknown"),
        report.task.title.as_deref().unwrap_or("(untitled)"),
        report.counts.satisfied,
        report.counts.criteria,
    );
    println!("{TASK_EVIDENCE_GATE_DISCLAIMER}");
    if report.criteria.is_empty() {
        println!("\nno acceptance criteria: nothing to gate on");
        return;
    }
    println!("\nCRITERIA");
    for row in &report.criteria {
        let mark = if row.satisfied { "ok  " } else { "FAIL" };
        let ordinal = row
            .ordinal
            .map_or_else(|| "?".to_owned(), |n| n.to_string());
        println!(
            "  [{mark}] {} (criterion {ordinal}): {}",
            row.record_id,
            row.text.as_deref().unwrap_or("(no text)")
        );
        if let Some(reason) = row.not_ready_reason {
            println!(
                "         reason: {} — {}",
                reason.as_str(),
                reason_description(reason)
            );
        }
        for item in &row.evidence {
            let pass = if item.pass { "pass" } else { "FAIL" };
            println!(
                "         [{pass}] {} [{}] status={} freshness={}",
                item.record_id,
                item.verification_kind,
                item.status.as_deref().unwrap_or("unknown"),
                item.freshness.as_str(),
            );
            for citation in &item.citations {
                let span = citation
                    .span
                    .map_or_else(String::new, |s| format!(":{}-{}", s.start_line, s.end_line));
                println!(
                    "           cites {}{} ({}) — {}",
                    citation.repo_relative_path.as_deref().unwrap_or("?"),
                    span,
                    citation.relation,
                    citation.verdict.as_str(),
                );
            }
        }
    }
}
