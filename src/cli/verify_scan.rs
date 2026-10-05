use super::*;
use crate::verify_scan::{VerifyScanInputs, run_verify_scan, verify_scan_exit_code};

// ---------------------------------------------------------------------------
// verify-scan command (issue #239)
// ---------------------------------------------------------------------------

/// `eg verify-scan <repo>`: scan the repository twice with pinned
/// transaction time and repository identity, and report whether the two
/// outputs are byte-for-byte stable.
///
/// The JSON/text report goes to stdout; the process exits 0 when the scans
/// are stable and 1 when they differ (see
/// [`crate::verify_scan::verify_scan_exit_code`]). A scan failure returns an
/// error (exit 1 via the top-level handler) with no stability verdict.
///
/// The two scans and the comparison run in
/// [`crate::verify_scan::run_verify_scan`], which takes the scan step as an
/// injectable closure so tests can exercise the unstable path; this wrapper
/// only wires up the real pipeline, renders the report, and exits.
pub(crate) fn verify_scan_cmd(
    repo_path: &Path,
    repo_id_override: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    warn_on_unconsumed_scope_pins();
    // verify-scan writes no artifact into the repo, so there is no output
    // path to exclude — only the in-tree egregore store, as with `eg scan`.
    let exclusions = store_exclusions_including_egregore(repo_path, &[]);
    // Same pipeline as `eg scan`, including standard redaction: the check
    // verifies the real scan path, not a reduced one. The driver invokes
    // this closure twice with one shared set of pinned inputs.
    let mut scan_once = |inputs: &VerifyScanInputs| -> Result<String> {
        let graph = crate::scan_repository_at_with_exclusions(
            repo_path,
            &inputs.transaction_time,
            inputs.repo_id_override.as_deref(),
            &exclusions,
        )
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;
        let mut records = graph.into_records();
        crate::redaction::redact_code_graph(&mut records, false, &inputs.repository_id);
        Graph::from_records(records)
            .to_jsonl()
            .context("failed to serialize graph JSONL")
    };
    let report = run_verify_scan(repo_path, repo_id_override, &mut scan_once)?;
    match format {
        OutputFormat::Json => {
            let output = report
                .render_json()
                .context("failed to serialize verify-scan report")?;
            println!("{output}");
        }
        OutputFormat::Text => print!("{}", report.render_text()),
    }
    process::exit(verify_scan_exit_code(report.ok));
}
