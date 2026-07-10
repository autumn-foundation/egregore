use super::*;

/// Handles `eg repair preflight` and `eg repair run`.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn repair_cmd(action: RepairCliAction) -> Result<()> {
    match action {
        RepairCliAction::Preflight { data_dir } => {
            let report = repair::preflight(&data_dir)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        RepairCliAction::Run {
            data_dir,
            confirm,
            dry_run,
        } => {
            let report = repair::run_repair(&data_dir, dry_run, confirm)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}
