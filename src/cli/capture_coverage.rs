use super::*;

use crate::coverage_capture::{
    CoverageCaptureError, CoverageReportRequest, LLVM_COV_JSON_FORMAT, LLVM_COV_LCOV_FORMAT,
    build_coverage_report_records, coverage_error_diagnostic, parse_lcov, parse_llvm_cov_json,
};
use crate::protected::{ProtectedPayloadClass, ProtectedStore};

/// Exit code for an empty input file (`empty_coverage_report`).
const EXIT_EMPTY: i32 = 4;
/// Exit code for an unparseable input or a missing coverage tool
/// (`unparseable_coverage_report` / `coverage_tool_missing`).
const EXIT_UNPARSEABLE: i32 = 5;
/// Exit code for a protected-store I/O failure (mirrors `scan-logs`).
const EXIT_PROTECTED_IO: i32 = 3;

/// Arguments for [`capture_coverage`], bundled to keep the dispatch arm readable.
pub(crate) struct CaptureCoverageArgs<'a> {
    pub input: &'a Path,
    pub out: &'a Path,
    pub session_id: &'a str,
    pub commit: &'a str,
    pub suite: Option<&'a str>,
    pub command: &'a str,
    pub exit_code: i64,
    pub executed_at: &'a str,
    pub tool: &'a str,
    pub tool_version: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub repo_root: Option<&'a Path>,
    pub graph: Option<&'a Path>,
    pub format: &'a str,
    pub protected_raw_artifacts: bool,
    pub protected_store: Option<&'a Path>,
    pub producer: Option<&'a str>,
}

/// Emits the machine-readable `{"code":..,"field":..}` usage diagnostic to
/// stderr and exits 1, matching the typed evidence-writer envelope shape.
fn exit_usage_error(code: &str, field: &str) -> ! {
    eprintln!(r#"{{"code":"{code}", "field":"{field}"}}"#);
    process::exit(1);
}

/// Emits the machine-readable capture-failure envelope to stderr and exits with
/// `exit_code`, mirroring the `scan-logs` protected-capture failure shape.
fn exit_capture_error(code: &str, detail: &serde_json::Value, exit_code: i32) -> ! {
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": code, "detail": detail }
    });
    eprintln!("{}", serde_json::to_string(&envelope).expect("infallible"));
    process::exit(exit_code);
}

/// Writes a single-node graph to `out` and prints a machine-readable error
/// envelope to stdout, then exits `exit_code`. Used for the empty /
/// unparseable / tool-missing diagnostic paths — never a `CoverageReport`.
fn write_diagnostic_and_exit(
    out: &Path,
    diagnostic: GraphRecord,
    error: CoverageCaptureError,
    exit_code: i32,
) -> Result<()> {
    let mut graph = Graph::new();
    let record_id = diagnostic.id().to_owned();
    graph.push(diagnostic);
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize diagnostic JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write diagnostic JSONL to {}", out.display()))?;
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": error.code(), "diagnostic_id": record_id }
    });
    println!("{}", serde_json::to_string(&envelope).expect("infallible"));
    process::exit(exit_code);
}

/// Handles `eg capture-coverage` — capture one coverage-tool artefact as a
/// `CoverageReport`.
#[allow(clippy::too_many_lines)]
pub(crate) fn capture_coverage(args: &CaptureCoverageArgs) -> Result<()> {
    // ── Usage validation (exit 1) ────────────────────────────────────────────
    if args.format != LLVM_COV_JSON_FORMAT && args.format != LLVM_COV_LCOV_FORMAT {
        exit_usage_error("invalid_field", "format");
    }
    if chrono::DateTime::parse_from_rfc3339(args.executed_at).is_err() {
        exit_usage_error("invalid_field", "executed_at");
    }
    if args.protected_raw_artifacts {
        if args.protected_store.is_none() {
            exit_usage_error("missing_field", "protected_store");
        }
        if args.producer.is_none() {
            exit_usage_error("missing_field", "producer");
        }
        if args.producer.is_some_and(|p| p.trim().is_empty()) {
            exit_usage_error("invalid_field", "producer");
        }
    }
    let _ = args.repo; // reserved for future repository scoping

    // ── Read raw input bytes (source artifact) ───────────────────────────────
    let raw_bytes = fs::read(args.input)
        .with_context(|| format!("failed to read coverage artefact {}", args.input.display()))?;
    let source_artifact_hash = blake3::hash(&raw_bytes).to_hex().to_string();
    let source_artifact_path = args.input.to_string_lossy();
    let repo_root = args.repo_root.map(|p| p.to_string_lossy().into_owned());

    // Non-UTF-8 input is unparseable (binary is never a coverage artefact).
    let diagnostic_for = |error: CoverageCaptureError| {
        coverage_error_diagnostic(
            args.session_id,
            args.commit,
            args.tool,
            &source_artifact_path,
            error,
        )
    };
    let Ok(input_text) = std::str::from_utf8(&raw_bytes) else {
        return write_diagnostic_and_exit(
            args.out,
            diagnostic_for(CoverageCaptureError::Unparseable),
            CoverageCaptureError::Unparseable,
            EXIT_UNPARSEABLE,
        );
    };

    // ── Parse the artefact ───────────────────────────────────────────────────
    let parse = match args.format {
        LLVM_COV_JSON_FORMAT => parse_llvm_cov_json(input_text, repo_root.as_deref()),
        _ => parse_lcov(input_text, repo_root.as_deref()),
    };
    let parse = match parse {
        Ok(parse) => parse,
        Err(error) => {
            let exit_code = match error {
                CoverageCaptureError::Empty => EXIT_EMPTY,
                CoverageCaptureError::Unparseable | CoverageCaptureError::ToolMissing => {
                    EXIT_UNPARSEABLE
                }
            };
            return write_diagnostic_and_exit(args.out, diagnostic_for(error), error, exit_code);
        }
    };

    // ── Optional code graph for file/symbol anchoring ─────────────────────────
    let code_graph = match args.graph {
        Some(path) => {
            let jsonl = fs::read_to_string(path)
                .with_context(|| format!("failed to read code graph {}", path.display()))?;
            let records = records_from_jsonl(&jsonl)
                .with_context(|| format!("failed to parse code graph {}", path.display()))?;
            Some(records)
        }
        None => None,
    };

    // ── Build and write the CoverageReport record batch ──────────────────────
    let req = CoverageReportRequest {
        session_id: args.session_id,
        commit: args.commit,
        suite: args.suite,
        command: args.command,
        exit_code: args.exit_code,
        executed_at: args.executed_at,
        tool: args.tool,
        tool_version: args.tool_version,
        artifact_format: args.format,
        source_artifact_path: &source_artifact_path,
        source_artifact_hash: &source_artifact_hash,
    };
    let outcome = build_coverage_report_records(&req, &parse, code_graph.as_deref());

    let mut graph = Graph::new();
    for record in &outcome.records {
        graph.push(record.clone());
    }
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize CoverageReport JSONL")?;
    fs::write(args.out, &jsonl).with_context(|| {
        format!(
            "failed to write CoverageReport JSONL to {}",
            args.out.display()
        )
    })?;

    let totals = parse.totals_lines();
    let envelope = serde_json::json!({
        "ok": true,
        "record_id": outcome.record_id,
        "records": outcome.records.len(),
        "tool": args.tool,
        "format": args.format,
        "line_coverage_percent": totals.percent,
        "lines_covered": totals.covered,
        "lines_total": totals.total,
        "branch_coverage_percent": parse.totals_branches().map(|b| b.percent)
            .or_else(|| parse.totals_regions().map(|r| r.percent)),
        "files_measured": parse.file_count(),
        "resolved_files": outcome.resolved_file_count,
        "unresolved_files": outcome.unresolved_file_count,
        "unresolved": outcome.unresolved,
        "resolved_functions": outcome.resolved_function_count,
        "unresolved_functions": outcome.unresolved_function_count,
        "ambiguous_functions": outcome.ambiguous_function_count,
    });
    println!("{}", serde_json::to_string(&envelope).expect("infallible"));

    // ── Protected raw-artifact capture (issue #60) ───────────────────────────
    if args.protected_raw_artifacts {
        let store_dir = args.protected_store.expect("validated present above");
        let producer_id = args.producer.expect("validated present above");
        let store = ProtectedStore::new(store_dir);
        match store.capture_bytes(
            ProtectedPayloadClass::CommandOutput,
            &source_artifact_path,
            &raw_bytes,
            producer_id,
            env!("CARGO_PKG_VERSION"),
            args.executed_at,
            true,
        ) {
            Ok(report) => {
                let entry = &report.entries[0];
                let envelope = serde_json::json!({
                    "ok": true,
                    "protected_capture": {
                        "handle": entry.handle,
                        "content_hash": entry.content_hash,
                        "byte_len": entry.byte_len,
                        "source_class": ProtectedPayloadClass::CommandOutput.as_str(),
                        "stored": entry.stored,
                    }
                });
                println!("{}", serde_json::to_string(&envelope).expect("infallible"));
            }
            Err(e) => {
                exit_capture_error(
                    "store_io_error",
                    &serde_json::json!({
                        "message": format!(
                            "protected store I/O failed at {}: {e}",
                            store_dir.display()
                        )
                    }),
                    EXIT_PROTECTED_IO,
                );
            }
        }
    }

    Ok(())
}
