use super::*;

/// Handles `eg doctor`: gather environment observations, build the preflight
/// report, print it, and exit with the appropriate code.
///
/// Exits 0 when structural checks all pass; exits 1 otherwise.
/// Optional/semantic failures never change the exit code.
pub(crate) fn doctor_cmd(
    path: PathBuf,
    out: PathBuf,
    data_dir: PathBuf,
    require_history: bool,
    network: bool,
    format: OutputFormat,
) -> Result<()> {
    use crate::preflight::{DoctorConfig, build_report, gather_observations, render_doctor_text};

    let config = DoctorConfig {
        repo_path: path,
        out,
        data_dir,
        require_history,
        network,
    };
    let obs = gather_observations(&config);
    let report = build_report(&config, &obs);

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        OutputFormat::Text => {
            print!("{}", render_doctor_text(&report));
        }
    }

    if !report.structural_ready {
        process::exit(1);
    }
    Ok(())
}
