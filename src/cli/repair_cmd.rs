use super::*;

use crate::repair::{PreflightReport, RepairOptions, RepairSessionReport};

/// Handles `eg repair preflight` and `eg repair run`.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn repair_cmd(action: RepairCliAction) -> Result<()> {
    match action {
        RepairCliAction::Preflight { data_dir, format } => {
            let report = repair::preflight(&data_dir)?;
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                }
                OutputFormat::Text => {
                    print!("{}", render_preflight_text(&report));
                }
            }
            Ok(())
        }
        RepairCliAction::Run {
            data_dir,
            confirm,
            dry_run,
            quarantine,
            format,
            transaction_time,
        } => {
            let report = repair::run_repair_with(
                &data_dir,
                &RepairOptions {
                    dry_run,
                    confirm,
                    quarantine,
                    transaction_time,
                },
            )?;
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                }
                OutputFormat::Text => {
                    print!("{}", render_run_text(&report));
                }
            }
            Ok(())
        }
    }
}

/// Renders a preflight report as a deterministic human-readable summary.
///
/// Carries the same facts as the JSON form; no secrets, no timestamps.
#[cfg(feature = "embedded-aletheiadb")]
fn render_preflight_text(report: &PreflightReport) -> String {
    let mut out = String::new();
    out.push_str("repair preflight\n");
    out.push_str(&format!("  data_dir: {}\n", report.data_dir.display()));
    out.push_str(&format!(
        "  runtime_dir: {}\n",
        report.runtime_dir.display()
    ));
    out.push_str(&format!(
        "  ownership_verdict: {}\n",
        verdict_str(&report.ownership_verdict)
    ));
    out.push_str(&format!("  allow: {}\n", report.allow));
    out.push_str(&format!("  repair_needed: {}\n", report.repair_needed));
    if let Some(status) = &report.before_daemon_status {
        out.push_str(&format!("  daemon_state: {}\n", status.state));
    }
    if report.refusal_reasons.is_empty() {
        out.push_str("  refusal_reasons: none\n");
    } else {
        out.push_str(&format!(
            "  refusal_reasons: {}\n",
            join_codes(&report.refusal_reasons)
        ));
    }
    if let Some(cmd) = &report.safe_daemon_command {
        out.push_str(&format!("  next: {cmd}\n"));
    }
    out
}

/// Renders a run report as a deterministic human-readable summary.
#[cfg(feature = "embedded-aletheiadb")]
fn render_run_text(report: &RepairSessionReport) -> String {
    let mut out = String::new();
    out.push_str("repair run\n");
    out.push_str(&format!("  data_dir: {}\n", report.data_dir.display()));
    out.push_str(&format!(
        "  runtime_dir: {}\n",
        report.runtime_dir.display()
    ));
    out.push_str(&format!(
        "  ownership_verdict: {}\n",
        verdict_str(&report.ownership_verdict)
    ));
    out.push_str(&format!("  dry_run: {}\n", report.dry_run));
    out.push_str(&format!(
        "  result: {}\n",
        serde_json::to_value(&report.result)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    ));
    if let Some(after) = &report.after_ownership_verdict {
        out.push_str(&format!(
            "  after_ownership_verdict: {}\n",
            verdict_str(after)
        ));
    }
    if report.changed_file_paths.is_empty() {
        out.push_str("  changed_files: none\n");
    } else {
        for path in &report.changed_file_paths {
            out.push_str(&format!("  changed_file: {}\n", path.display()));
        }
    }
    for entry in &report.manifest {
        let action = serde_json::to_value(entry.action)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let result = serde_json::to_value(entry.result)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        out.push_str(&format!("  manifest: {action} [{result}]\n"));
    }
    if !report.refusal_reasons.is_empty() {
        out.push_str(&format!(
            "  refusal_reasons: {}\n",
            join_codes(&report.refusal_reasons)
        ));
    }
    out
}

#[cfg(feature = "embedded-aletheiadb")]
fn verdict_str(verdict: &crate::repair::OwnershipVerdict) -> String {
    serde_json::to_value(verdict)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(feature = "embedded-aletheiadb")]
fn join_codes(codes: &[crate::repair::RepairRefusalCode]) -> String {
    codes
        .iter()
        .filter_map(|c| {
            serde_json::to_value(c)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
        })
        .collect::<Vec<_>>()
        .join(", ")
}
