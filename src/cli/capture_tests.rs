use super::*;

use crate::protected::{ProtectedPayloadClass, ProtectedStore};
use crate::test_capture::{
    LIBTEST_JSON_FORMAT, TestCaptureError, TestRunRequest, build_test_run_records,
    error_diagnostic, parse_libtest_json,
};

/// Exit code for an empty input file (`empty_test_output`).
const EXIT_EMPTY: i32 = 4;
/// Exit code for unparseable input (`unparseable_test_output`).
const EXIT_UNPARSEABLE: i32 = 5;
/// Exit code for a protected-store I/O failure (mirrors `scan-logs`).
const EXIT_PROTECTED_IO: i32 = 3;

/// Arguments for [`capture_tests`], bundled to keep the dispatch arm readable.
pub(crate) struct CaptureTestsArgs<'a> {
    pub input: &'a Path,
    pub out: &'a Path,
    pub session_id: &'a str,
    pub commit: &'a str,
    pub suite: &'a str,
    pub command: &'a str,
    pub exit_code: i64,
    pub executed_at: &'a str,
    pub runner: Option<&'a str>,
    pub runner_version: Option<&'a str>,
    pub repo: Option<&'a str>,
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
/// envelope to stdout, then exits `exit_code`. Used for the empty / unparseable
/// diagnostic paths — never a `TestRun`.
fn write_diagnostic_and_exit(
    out: &Path,
    diagnostic: GraphRecord,
    error: TestCaptureError,
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

/// Handles `eg capture-tests` — capture one libtest JSON run as a `TestRun`.
#[allow(clippy::too_many_lines)]
pub(crate) fn capture_tests(args: &CaptureTestsArgs) -> Result<()> {
    // ── Usage validation (exit 1) ────────────────────────────────────────────
    if args.format != LIBTEST_JSON_FORMAT {
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
        .with_context(|| format!("failed to read test output {}", args.input.display()))?;
    let source_artifact_hash = blake3::hash(&raw_bytes).to_hex().to_string();
    let source_artifact_path = args.input.to_string_lossy();

    // Non-UTF-8 input is unparseable (binary is never a libtest JSON stream).
    let Ok(input_text) = std::str::from_utf8(&raw_bytes) else {
        let diagnostic = error_diagnostic(
            args.session_id,
            args.commit,
            args.suite,
            &source_artifact_path,
            TestCaptureError::Unparseable,
        );
        return write_diagnostic_and_exit(
            args.out,
            diagnostic,
            TestCaptureError::Unparseable,
            EXIT_UNPARSEABLE,
        );
    };

    // ── Parse the event stream ───────────────────────────────────────────────
    let parse = match parse_libtest_json(input_text) {
        Ok(parse) => parse,
        Err(error) => {
            let exit_code = match error {
                TestCaptureError::Empty => EXIT_EMPTY,
                TestCaptureError::Unparseable => EXIT_UNPARSEABLE,
            };
            let diagnostic = error_diagnostic(
                args.session_id,
                args.commit,
                args.suite,
                &source_artifact_path,
                error,
            );
            return write_diagnostic_and_exit(args.out, diagnostic, error, exit_code);
        }
    };

    // ── Optional code graph for symbol anchoring ─────────────────────────────
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

    // ── Build and write the TestRun record batch ─────────────────────────────
    let req = TestRunRequest {
        session_id: args.session_id,
        commit: args.commit,
        suite: args.suite,
        command: args.command,
        exit_code: args.exit_code,
        executed_at: args.executed_at,
        runner: args.runner,
        runner_version: args.runner_version,
        source_artifact_path: &source_artifact_path,
        source_artifact_hash: &source_artifact_hash,
    };
    let outcome = build_test_run_records(&req, &parse, code_graph.as_deref());

    let mut graph = Graph::new();
    for record in &outcome.records {
        graph.push(record.clone());
    }
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize TestRun JSONL")?;
    fs::write(args.out, &jsonl)
        .with_context(|| format!("failed to write TestRun JSONL to {}", args.out.display()))?;

    let counts = parse.counts();
    let envelope = serde_json::json!({
        "ok": true,
        "record_id": outcome.record_id,
        "records": outcome.records.len(),
        "suite_status": parse.suite_status().as_str(),
        "counts": {
            "passed": counts.passed,
            "failed": counts.failed,
            "ignored": counts.ignored,
            "timeout": counts.timeout,
            "bench": counts.bench,
            "total": counts.total,
        },
        "resolved_symbols": outcome.resolved_count,
        "unresolved_symbols": outcome.unresolved_count,
        "ambiguous_symbols": outcome.ambiguous_count,
        "partial": outcome.partial,
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
