use super::*;

use crate::log_graph::{self, LogScanError};

/// Handles `eg scan-logs <log_path> --repo-path <repo> --out <log.graph.jsonl>`.
///
/// Extracts deterministic, redaction-safe log-signature graph records from one
/// captured log file (issues #319 / #320) and writes them as JSONL. An
/// unrecognized (binary / non-UTF-8) input prints a machine-readable diagnostic
/// to stdout and exits 1 with no partial output. Exemplar-cap diagnostics, when
/// any, are printed as machine-readable JSON lines to stderr — never a silent
/// drop.
pub(crate) fn scan_logs(
    log_path: &Path,
    repo_path: &Path,
    out: &Path,
    repo_id_override: Option<&str>,
) -> Result<()> {
    let repository_id = identity::compute_repository_identity(repo_path, repo_id_override).id;
    let transaction_time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let scan =
        match log_graph::scan_log_records(log_path, repo_path, &repository_id, &transaction_time) {
            Ok(scan) => scan,
            Err(LogScanError::UnrecognizedFormat { detail }) => {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": { "code": "unrecognized_format", "message": detail }
                });
                println!("{}", serde_json::to_string(&envelope).unwrap_or_default());
                process::exit(1);
            }
            Err(err @ LogScanError::Read { .. }) => {
                return Err(anyhow::anyhow!(err.to_string()))
                    .with_context(|| format!("failed to scan log file {}", log_path.display()));
            }
        };

    let producer = log_graph::log_importer_producer(scan.source_format_version, &transaction_time);
    let mut graph = Graph::new();
    for record in scan.records {
        graph.push(record);
    }
    let graph = graph.stamp_producer(&producer);
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize log graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write log graph JSONL to {}", out.display()))?;

    // Surface exemplar-cap diagnostics to stderr (machine-readable), never silent.
    for diagnostic in &scan.diagnostics {
        eprintln!("{}", serde_json::to_string(diagnostic).unwrap_or_default());
    }
    Ok(())
}
