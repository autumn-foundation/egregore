#[cfg(feature = "embedded-aletheiadb")]
use std::fmt::Write as _;

use super::*;

#[cfg(feature = "embedded-aletheiadb")]
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
    let _ = writeln!(out, "repair preflight");
    let _ = writeln!(out, "  data_dir: {}", report.data_dir.display());
    let _ = writeln!(out, "  runtime_dir: {}", report.runtime_dir.display());
    let _ = writeln!(
        out,
        "  ownership_verdict: {}",
        verdict_str(&report.ownership_verdict)
    );
    let _ = writeln!(out, "  allow: {}", report.allow);
    let _ = writeln!(out, "  repair_needed: {}", report.repair_needed);
    if let Some(status) = &report.before_daemon_status {
        let _ = writeln!(out, "  daemon_state: {}", status.state);
    }
    if report.refusal_reasons.is_empty() {
        out.push_str("  refusal_reasons: none\n");
    } else {
        let _ = writeln!(
            out,
            "  refusal_reasons: {}",
            join_codes(&report.refusal_reasons)
        );
    }
    if let Some(cmd) = &report.safe_daemon_command {
        let _ = writeln!(out, "  next: {cmd}");
    }
    out
}

/// Renders a run report as a deterministic human-readable summary.
#[cfg(feature = "embedded-aletheiadb")]
fn render_run_text(report: &RepairSessionReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "repair run");
    let _ = writeln!(out, "  data_dir: {}", report.data_dir.display());
    let _ = writeln!(out, "  runtime_dir: {}", report.runtime_dir.display());
    let _ = writeln!(
        out,
        "  ownership_verdict: {}",
        verdict_str(&report.ownership_verdict)
    );
    let _ = writeln!(out, "  dry_run: {}", report.dry_run);
    let _ = writeln!(out, "  result: {}", enum_str(&report.result));
    if let Some(after) = &report.after_ownership_verdict {
        let _ = writeln!(out, "  after_ownership_verdict: {}", verdict_str(after));
    }
    if report.changed_file_paths.is_empty() {
        out.push_str("  changed_files: none\n");
    } else {
        for path in &report.changed_file_paths {
            let _ = writeln!(out, "  changed_file: {}", path.display());
        }
    }
    for entry in &report.manifest {
        let _ = writeln!(
            out,
            "  manifest: {} [{}]",
            enum_str(&entry.action),
            enum_str(&entry.result)
        );
    }
    if !report.refusal_reasons.is_empty() {
        let _ = writeln!(
            out,
            "  refusal_reasons: {}",
            join_codes(&report.refusal_reasons)
        );
    }
    out
}

/// Renders any `#[serde(rename_all = "snake_case")]` unit enum as its stable
/// wire string.
#[cfg(feature = "embedded-aletheiadb")]
fn enum_str<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
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
