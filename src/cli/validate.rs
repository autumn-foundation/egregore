use super::*;

/// Prints a redaction-safe JSON load error to stderr and exits 2, keeping the
/// load-error exit code distinct from the defects-found gate failure (1).
pub(crate) fn validate_load_exit(code: &str, path: &Path, message: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({
            "code": code,
            "path": path.display().to_string(),
            "message": message
        })
    );
    std::process::exit(2);
}

/// Handles `eg validate` (issue #103): pre-ingest referential-integrity
/// validation of a graph JSONL. Exit 0 clean, 1 with one diagnostic per
/// defect, 2 on load errors.
pub(crate) fn validate_cmd(graph: &Path, format: OutputFormat) -> Result<()> {
    let jsonl = fs::read_to_string(graph)
        .unwrap_or_else(|error| validate_load_exit("graph_read_error", graph, &error.to_string()));
    let records = records_from_jsonl(&jsonl)
        .unwrap_or_else(|error| validate_load_exit("graph_parse_error", graph, &error.to_string()));

    let report = crate::validate::validate_records(&records);

    match format {
        OutputFormat::Json => {
            for diagnostic in &report.diagnostics {
                println!(
                    "{}",
                    serde_json::to_string(diagnostic)
                        .context("failed to serialize validation diagnostic")?
                );
            }
            let summary = serde_json::json!({
                "ok": report.is_clean(),
                "records": report.records,
                "nodes": report.nodes,
                "edges": report.edges,
                "tombstones": report.tombstones,
                "defects": report.diagnostics.len(),
            });
            println!("{}", serde_json::to_string(&summary)?);
        }
        OutputFormat::Text => {
            for diagnostic in &report.diagnostics {
                println!("{}", diagnostic.to_text());
            }
            let defects = report.diagnostics.len();
            let plural = if defects == 1 { "" } else { "s" };
            println!(
                "validated {} records ({} nodes, {} edges, {} tombstones): {defects} defect{plural}",
                report.records, report.nodes, report.edges, report.tombstones
            );
        }
    }

    if !report.is_clean() {
        std::process::exit(1);
    }
    Ok(())
}
