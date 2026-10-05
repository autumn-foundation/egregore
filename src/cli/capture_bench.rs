use super::*;

use crate::bench_capture::{
    BenchCaptureError, BenchmarkRunRequest, CRITERION_ESTIMATES_FORMAT, bench_error_diagnostic,
    build_bench_run_records, parse_benchmark_inputs, scan_criterion_dir,
};

/// Exit code for an empty criterion output dir (`empty_bench_output`).
const EXIT_EMPTY: i32 = 4;
/// Exit code for unparseable estimates (`unparseable_bench_output`).
const EXIT_UNPARSEABLE: i32 = 5;

/// Arguments for [`capture_bench`], bundled to keep the dispatch arm readable.
pub(crate) struct CaptureBenchArgs<'a> {
    pub input: &'a Path,
    pub out: &'a Path,
    pub session_id: &'a str,
    pub commit: &'a str,
    pub suite: &'a str,
    pub command: Option<&'a str>,
    pub executed_at: &'a str,
    pub graph: Option<&'a Path>,
    pub format: &'a str,
    pub repo: Option<&'a str>,
}

/// Emits the machine-readable `{"code":..,"field":..}` usage diagnostic to
/// stderr and exits 1, matching the typed evidence-writer envelope shape.
fn exit_usage_error(code: &str, field: &str) -> ! {
    eprintln!(r#"{{"code":"{code}", "field":"{field}"}}"#);
    process::exit(1);
}

/// Classifies an unanchored benchmark link for the envelope's `unresolved`
/// section: `ambiguous` when the code graph held two-plus same-named symbols,
/// `unresolved` otherwise.
fn unresolved_reason(records: &[GraphRecord], benchmark_id: &str) -> &'static str {
    use crate::bench_capture::BENCH_SYMBOL_AMBIGUOUS_CODE;
    let ambiguous = records.iter().any(|r| {
        matches!(
            r,
            GraphRecord::Node {
                name: Some(n),
                symbol_kind: Some(k),
                ..
            } if n == benchmark_id && k == BENCH_SYMBOL_AMBIGUOUS_CODE
        )
    });
    if ambiguous { "ambiguous" } else { "unresolved" }
}

/// Writes a single-node graph to `out` and prints a machine-readable error
/// envelope to stdout, then exits `exit_code`. Used for the empty /
/// unparseable diagnostic paths — never a `BenchmarkRun`.
fn write_diagnostic_and_exit(
    out: &Path,
    diagnostic: GraphRecord,
    error: BenchCaptureError,
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
    println!(
        "{}",
        serde_json::to_string(&envelope).map_err(|err| anyhow::anyhow!(
            "failed to serialize capture diagnostic envelope: {err}"
        ))?
    );
    process::exit(exit_code);
}

/// Handles `eg capture-bench` — capture one criterion output directory as
/// `BenchmarkRun` records.
///
/// CAPTURE-ONLY: the caller runs the benchmarks; this command only reads the
/// `*/new/estimates.json` files criterion already wrote. Deterministic:
/// identical input bytes plus identical `--executed-at` produce byte-identical
/// output JSONL, so re-capturing converges to a no-op (issue #130).
#[allow(clippy::too_many_lines)]
pub(crate) fn capture_bench(args: &CaptureBenchArgs) -> Result<()> {
    // ── Usage validation (exit 1) ────────────────────────────────────────────
    if args.format != CRITERION_ESTIMATES_FORMAT {
        exit_usage_error("invalid_field", "format");
    }
    if chrono::DateTime::parse_from_rfc3339(args.executed_at).is_err() {
        exit_usage_error("invalid_field", "executed_at");
    }
    if args.input.to_string_lossy().trim().is_empty() {
        exit_usage_error("missing_field", "input");
    }
    let _ = args.repo; // reserved for future repository scoping

    let source_root = args.input.to_string_lossy();

    // ── Scan the criterion output directory ──────────────────────────────────
    let inputs = match scan_criterion_dir(args.input) {
        Ok(inputs) => inputs,
        Err(error) => {
            let diagnostic = bench_error_diagnostic(
                args.session_id,
                args.commit,
                args.suite,
                &source_root,
                error,
            );
            return write_diagnostic_and_exit(args.out, diagnostic, error, EXIT_EMPTY);
        }
    };

    // ── Parse the estimates (executes nothing) ───────────────────────────────
    let parse = match parse_benchmark_inputs(&inputs) {
        Ok(parse) => parse,
        Err(error) => {
            let diagnostic = bench_error_diagnostic(
                args.session_id,
                args.commit,
                args.suite,
                &source_root,
                error,
            );
            return write_diagnostic_and_exit(args.out, diagnostic, error, EXIT_UNPARSEABLE);
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

    // ── Build and write the BenchmarkRun record batch ────────────────────────
    let req = BenchmarkRunRequest {
        session_id: args.session_id,
        commit: args.commit,
        suite: args.suite,
        command: args.command,
        executed_at: args.executed_at,
    };
    let outcome = build_bench_run_records(&req, &parse, code_graph.as_deref())
        .context("failed to build benchmark run records")?;

    let mut graph = Graph::new();
    for record in &outcome.records {
        graph.push(record.clone());
    }
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize BenchmarkRun JSONL")?;
    fs::write(args.out, &jsonl).with_context(|| {
        format!(
            "failed to write BenchmarkRun JSONL to {}",
            args.out.display()
        )
    })?;

    // The `unresolved` section: every benchmark→symbol link that could not be
    // anchored is reported here (and as a Diagnostic in the output) — never
    // dropped silently, mirroring `eg query task`.
    let unresolved: Vec<serde_json::Value> = outcome
        .benchmark_ids
        .iter()
        .zip(outcome.record_ids.iter())
        .filter(|(id, _)| outcome.unresolved.iter().any(|u| u == *id))
        .map(|(benchmark_id, record_id)| {
            serde_json::json!({
                "benchmark_id": benchmark_id,
                "record_id": record_id,
                "reason": unresolved_reason(&outcome.records, benchmark_id),
            })
        })
        .collect();

    let verdicts: serde_json::Map<String, serde_json::Value> = outcome
        .benchmark_ids
        .iter()
        .zip(outcome.verdicts.iter())
        .map(|(id, v)| (id.clone(), serde_json::Value::String(v.as_str().to_owned())))
        .collect();

    let envelope = serde_json::json!({
        "ok": true,
        "records": outcome.records.len(),
        "benchmarks": outcome.benchmark_ids,
        "record_ids": outcome.record_ids,
        "verdicts": verdicts,
        "resolved_symbols": outcome.resolved_count,
        "ambiguous_symbols": outcome.ambiguous_count,
        "unresolved": unresolved,
    });
    println!(
        "{}",
        serde_json::to_string(&envelope)
            .map_err(|err| anyhow::anyhow!("failed to serialize capture envelope: {err}"))?
    );

    Ok(())
}
