use super::*;
use std::process::Stdio;

/// Routes `eg audit` subcommands.
#[allow(clippy::too_many_lines)] // a flat dispatch table, one arm per subcommand
pub(crate) fn audit_cmd(subcommand: AuditSubcommand) -> Result<()> {
    match subcommand {
        AuditSubcommand::Citations {
            graph,
            data_dir,
            min_code_citation,
            min_log_citation,
            format,
        } => audit_citations_cmd(
            graph.as_deref(),
            data_dir.as_deref(),
            min_code_citation,
            min_log_citation,
            format,
        ),
        AuditSubcommand::MemoryHealth {
            graph,
            data_dir,
            min_provenance_coverage,
            max_dangling_evidence,
            max_unverified,
            max_current_guidance_contamination,
            format,
        } => audit_memory_health_cmd(
            graph.as_deref(),
            data_dir.as_deref(),
            min_provenance_coverage,
            max_dangling_evidence,
            max_unverified,
            max_current_guidance_contamination,
            format,
        ),
        AuditSubcommand::TokenCost {
            corpus,
            min_ratio,
            format,
        } => audit_token_cost_cmd(&corpus, min_ratio, format),
        AuditSubcommand::QueryLatency {
            corpus,
            samples,
            budget_p50_ms,
            source,
            format,
        } => audit_query_latency_cmd(&corpus, samples, budget_p50_ms, source, format),
        AuditSubcommand::QueryBudget {
            corpus,
            samples,
            warm_samples,
            scale_factor,
            max_ratio,
            budget_p95_ms,
            format,
        } => audit_query_budget_cmd(
            &corpus,
            samples,
            warm_samples,
            scale_factor,
            max_ratio,
            budget_p95_ms,
            format,
        ),
        AuditSubcommand::SymbolLatency {
            corpus,
            warm_samples,
            budget_p95_ms,
            history_commits,
            at_commit_index,
            format,
        } => audit_symbol_latency_cmd(
            &corpus,
            warm_samples,
            budget_p95_ms,
            history_commits,
            at_commit_index,
            format,
        ),
        AuditSubcommand::Accuracy {
            corpus_dir,
            labels,
            span_line_tolerance,
            min_precision,
            min_recall,
            format,
        } => crate::accuracy::eval_accuracy_cmd(
            &corpus_dir,
            &labels,
            span_line_tolerance,
            min_precision,
            min_recall,
            format,
        ),
        AuditSubcommand::ControlCatalog { catalog, format } => control_catalog_cmd(catalog, format),
        AuditSubcommand::EvidencePack { action } => evidence_pack_cmd(action),
        AuditSubcommand::ReviewCoverage {
            from,
            to,
            graph,
            data_dir,
            min_coverage,
            require_non_author,
            require_final_head,
            format,
        } => review_coverage_cmd(
            &from,
            &to,
            graph.as_deref(),
            data_dir.as_deref(),
            min_coverage,
            require_non_author,
            require_final_head,
            format,
        ),
        AuditSubcommand::CriteriaCoverage {
            graph,
            data_dir,
            min_proven_ratio,
            max_claimed_done_unproven,
            limit,
            format,
        } => criteria_coverage_cmd(
            graph.as_deref(),
            data_dir.as_deref(),
            min_proven_ratio,
            max_claimed_done_unproven,
            limit,
            format,
        ),
        AuditSubcommand::SemanticRelevance {
            corpus,
            data_dir,
            min_hit_rate_5,
            min_mrr,
            fp_threshold,
            top_k,
            format,
        } => audit_semantic_relevance_cmd(
            &corpus,
            &data_dir,
            min_hit_rate_5,
            min_mrr,
            fp_threshold,
            top_k,
            format,
        ),
        AuditSubcommand::EvidenceLinks {
            graph,
            data_dir,
            format,
        } => audit_evidence_links_cmd(graph.as_deref(), data_dir.as_deref(), format),
        AuditSubcommand::SchemaConstraints {
            data_dir,
            profile,
            declare,
            drop,
            include_foreign,
            format,
        } => audit_schema_constraints_cmd(
            &data_dir,
            &profile,
            declare,
            drop,
            include_foreign,
            format,
        ),
        // Appended (issue #185); kept at the end to minimize cross-lane merge conflicts.
        AuditSubcommand::MemoryEvidenceHealth {
            graph,
            data_dir,
            format,
        } => audit_memory_evidence_health_cmd(graph.as_deref(), data_dir.as_deref(), format),
    }
}

/// Prints a redaction-safe JSON error and exits with the usage/load code (2).
pub(crate) fn review_coverage_exit(value: &serde_json::Value) -> ! {
    eprintln!("{value}");
    std::process::exit(2);
}

/// The `eg audit criteria-coverage` usage/load-error exit (issue #115).
///
/// Every audit subcommand carries its own lane-named exit helper
/// (`review_coverage_exit`, `evidence_pack_exit`, `control_catalog_exit`, …), so
/// a reader tracing an `invalid_min_proven_ratio` envelope lands in a function
/// named for THIS lane and a change to another lane's error shape cannot
/// silently change this one's.
fn criteria_coverage_exit(value: &serde_json::Value) -> ! {
    eprintln!("{value}");
    std::process::exit(2);
}

/// Handles `eg audit review-coverage` (issue #339): gates review coverage over
/// PRs merged in a valid-time window. Exit 0 coverage met (empty window is a
/// vacuous pass), 1 below threshold (report still printed), 2 usage/load error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn review_coverage_cmd(
    from: &str,
    to: &str,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    min_coverage: f64,
    require_non_author: bool,
    require_final_head: bool,
    format: OutputFormat,
) -> Result<()> {
    use crate::evidence_pack::{ReviewCoverageOptions, Window};

    // Validate the gate threshold: a non-finite or out-of-range value would
    // silently disable or invert the gate.
    if !min_coverage.is_finite() || !(0.0..=1.0).contains(&min_coverage) {
        review_coverage_exit(&serde_json::json!({
            "code": "invalid_min_coverage",
            "value": min_coverage.to_string(),
            "message": "--min-coverage must be a finite value in [0.0, 1.0]",
        }));
    }

    // Validate the window bounds before opening any store, so a reversed/invalid
    // window is a precise usage error rather than a downstream failure.
    let from_ts = chrono::DateTime::parse_from_rfc3339(from).unwrap_or_else(|_| {
        review_coverage_exit(&serde_json::json!({
            "code": "invalid_timestamp",
            "which": "from",
            "value": from,
        }))
    });
    let to_ts = chrono::DateTime::parse_from_rfc3339(to).unwrap_or_else(|_| {
        review_coverage_exit(&serde_json::json!({
            "code": "invalid_timestamp",
            "which": "to",
            "value": to,
        }))
    });
    if from_ts >= to_ts {
        review_coverage_exit(&serde_json::json!({
            "code": "reversed_window",
            "from": from,
            "to": to,
        }));
    }

    // Enforce exactly-one-of the input flags before opening any store.
    match (graph, data_dir) {
        (Some(_), Some(_)) => review_coverage_exit(&serde_json::json!({
            "code": "conflicting_input_flags",
            "message": "provide only one of --graph or --data-dir, not both",
        })),
        (None, None) => review_coverage_exit(&serde_json::json!({
            "code": "missing_input_flag",
            "message": "provide --graph <path> or --data-dir <path>",
        })),
        _ => {}
    }

    // Read records read-only. An embedded store is read through a throwaway copy.
    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());
    #[allow(clippy::option_if_let_else)] // `--graph` uses the sanitizing reader (round-19 P2)
    let records = match graph {
        Some(graph_path) => load_graph_records_sanitized(graph_path),
        None => match load_query_records(graph, effective_data_dir) {
            Ok(records) => records,
            Err(error) => {
                eprintln!("{error}");
                drop(store_copy);
                std::process::exit(2);
            }
        },
    };

    // A genuinely empty evidence input is a LOAD error naming the path, distinct
    // from the vacuous `empty_window` SUCCESS (a non-empty store whose PRs simply
    // fall outside the window).
    if records.is_empty() {
        let source_path = graph
            .or(data_dir)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        drop(store_copy);
        review_coverage_exit(&serde_json::json!({
            "code": "empty_evidence_input",
            "path": source_path,
            "message": "evidence input holds zero records; provide a non-empty graph or store",
        }));
    }

    let window = Window {
        from: from.to_owned(),
        to: to.to_owned(),
    };
    let options = ReviewCoverageOptions {
        require_non_author,
        require_final_head,
    };
    let report =
        crate::review_coverage::run_review_coverage(&records, &window, options, min_coverage);

    let output = match format {
        OutputFormat::Json => {
            serde_json::to_string(&report).context("failed to serialize review-coverage report")?
        }
        OutputFormat::Text => render_review_coverage_text(&report),
    };
    println!("{output}");
    let exit_code = i32::from(!report.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Renders a review-coverage report as a deterministic human-readable form.
fn render_review_coverage_text(report: &crate::review_coverage::ReviewCoverageReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "window: {} <= t < {}",
        report.window.from, report.window.to
    ));
    lines.push(format!(
        "options: require_non_author={} require_final_head={}",
        report.options.require_non_author, report.options.require_final_head
    ));
    lines.push(format!("ok: {}", report.ok));
    lines.push(format!(
        "coverage: {:.4} (covered {} / merged {}) vs minimum {:.4}",
        report.coverage, report.covered_count, report.merged_pr_count, report.min_coverage
    ));
    lines.push("verdict_counts:".to_owned());
    for (verdict, count) in &report.verdict_counts {
        lines.push(format!("  {verdict}: {count}"));
    }
    lines.push("rows:".to_owned());
    for row in &report.rows {
        let subs = if row.sub_labels.is_empty() {
            String::new()
        } else {
            format!(" [{}]", row.sub_labels.join(","))
        };
        lines.push(format!("  {} {}{}", row.pr_task_id, row.verdict, subs));
    }
    lines.push(format!("diagnostics: {}", report.diagnostics.len()));
    for d in &report.diagnostics {
        lines.push(format!("  {} {}", d.code, d.detail));
    }
    lines.push(format!("disclaimer: {}", report.disclaimer));
    lines.join("\n")
}

/// Handles `eg audit criteria-coverage` (issue #115): the store-wide
/// acceptance-criterion verification-coverage census and proof-gap gate.
///
/// Strictly read-only — an embedded store is read through the throwaway copy
/// `readonly_audit_store` makes, so the original is left byte-for-byte
/// untouched. Exit 0 thresholds met (a store with zero criteria is a vacuous
/// pass), 1 threshold breached (report still printed), 2 usage/load error.
pub(crate) fn criteria_coverage_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    min_proven_ratio: f64,
    max_claimed_done_unproven: usize,
    limit: Option<usize>,
    format: OutputFormat,
) -> Result<()> {
    use crate::criteria_coverage::{
        CRITERIA_COVERAGE_DEFAULT_LIMIT, CRITERIA_COVERAGE_MAX_LIMIT, CriteriaCoverageConfig,
        run_criteria_coverage,
    };

    // Validate the gate bounds before touching any store: a non-finite or
    // out-of-range ratio would silently disable or invert the gate.
    if !min_proven_ratio.is_finite() || !(0.0..=1.0).contains(&min_proven_ratio) {
        criteria_coverage_exit(&serde_json::json!({
            "code": "invalid_min_proven_ratio",
            "value": min_proven_ratio.to_string(),
            "message": "--min-proven-ratio must be a finite value in [0.0, 1.0]",
        }));
    }
    let limit = limit.unwrap_or(CRITERIA_COVERAGE_DEFAULT_LIMIT);
    if limit == 0 || limit > CRITERIA_COVERAGE_MAX_LIMIT {
        criteria_coverage_exit(&serde_json::json!({
            "code": "invalid_limit",
            "value": limit.to_string(),
            "message": format!("--limit must be in 1..={CRITERIA_COVERAGE_MAX_LIMIT}"),
        }));
    }

    // Enforce exactly-one-of the input flags before opening any store.
    match (graph, data_dir) {
        (Some(_), Some(_)) => criteria_coverage_exit(&serde_json::json!({
            "code": "conflicting_input_flags",
            "message": "provide only one of --graph or --data-dir, not both",
        })),
        (None, None) => criteria_coverage_exit(&serde_json::json!({
            "code": "missing_input_flag",
            "message": "provide --graph <path> or --data-dir <path>",
        })),
        _ => {}
    }

    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());
    #[allow(clippy::option_if_let_else)] // `--graph` uses the sanitizing reader
    let records = match graph {
        Some(graph_path) => load_graph_records_sanitized(graph_path),
        None => match load_query_records(graph, effective_data_dir) {
            Ok(records) => records,
            Err(error) => {
                eprintln!("{error}");
                drop(store_copy);
                std::process::exit(2);
            }
        },
    };

    // A genuinely empty input is a LOAD error naming the path, distinct from the
    // vacuous `no_acceptance_criteria` SUCCESS (a populated store that simply
    // records no acceptance criteria).
    if records.is_empty() {
        let source_path = graph
            .or(data_dir)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        drop(store_copy);
        criteria_coverage_exit(&serde_json::json!({
            "code": "empty_evidence_input",
            "path": source_path,
            "message": "input holds zero records; provide a non-empty graph or store",
        }));
    }

    let config = CriteriaCoverageConfig {
        min_proven_ratio,
        max_claimed_done_unproven,
        limit,
    };
    let report = run_criteria_coverage(&records, &config);

    let output = match format {
        OutputFormat::Json => serde_json::to_string(&report)
            .context("failed to serialize criteria-coverage report")?,
        OutputFormat::Text => render_criteria_coverage_text(&report),
    };
    println!("{output}");
    let exit_code = i32::from(!report.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Renders a criteria-coverage report as a deterministic human-readable form
/// that mirrors the JSON contract field for field.
fn render_criteria_coverage_text(
    report: &crate::criteria_coverage::CriteriaCoverageReport,
) -> String {
    /// Renders one ratio as `numerator/denominator (ratio)`, never a bare
    /// percentage, and never a fabricated `0.0` on a zero denominator.
    fn ratio_line(label: &str, r: &crate::criteria_coverage::Ratio) -> String {
        let rendered = r
            .ratio
            .map_or_else(|| "n/a".to_owned(), |value| format!("{value:.4}"));
        format!("  {label}: {}/{} ({rendered})", r.numerator, r.denominator)
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("ok: {}", report.ok));
    lines.push(format!("total_criteria: {}", report.total_criteria));
    lines.push("coverage:".to_owned());
    lines.push(ratio_line("proven", &report.proven));
    lines.push(ratio_line("unverified", &report.unverified));
    lines.push(ratio_line("failed_evidence", &report.failed_evidence));
    lines.push(ratio_line("dangling_evidence", &report.dangling_evidence));
    lines.push(ratio_line(
        "non_verification_evidence",
        &report.non_verification_evidence,
    ));
    lines.push(ratio_line(
        "inconclusive_evidence",
        &report.inconclusive_evidence,
    ));
    lines.push(ratio_line("proof_gap", &report.proof_gap));
    lines.push("bucket_counts:".to_owned());
    for (bucket, count) in &report.bucket_counts {
        lines.push(format!("  {bucket}: {count}"));
    }
    lines.push(format!(
        "thresholds: min_proven_ratio={:.4} max_claimed_done_unproven={}",
        report.thresholds.min_proven_ratio, report.thresholds.max_claimed_done_unproven
    ));
    lines.push(format!(
        "done_task_statuses: {}",
        report.done_task_statuses.join(",")
    ));
    lines.push(format!(
        "claimed_done_unproven: {} (showing {})",
        report.claimed_done_unproven_count,
        report.claimed_done_unproven.len()
    ));
    for row in &report.claimed_done_unproven {
        lines.extend(criterion_text_lines(row));
    }
    lines.push(format!("criteria: {}", report.criteria.len()));
    for row in &report.criteria {
        lines.extend(criterion_text_lines(row));
    }
    lines.push(format!("breaches: {}", report.breaches.len()));
    for breach in &report.breaches {
        lines.push(format!(
            "  {} observed={} bound={} — {}",
            breach.metric, breach.observed, breach.bound, breach.message
        ));
    }
    lines.push(format!("diagnostics: {}", report.diagnostics.len()));
    for d in &report.diagnostics {
        lines.push(format!("  {} {}", d.code, d.detail));
        // The record IDs a diagnostic names ARE its citation: dropping them
        // would leave the text form asserting a gap it never points at.
        for id in &d.record_ids {
            lines.push(format!("    {id}"));
        }
    }
    lines.push(format!("disclaimer: {}", report.disclaimer));
    lines.join("\n")
}

/// Renders one criterion row as human-readable lines carrying the SAME citable
/// evidence the JSON row carries.
///
/// The text form is documented as mirroring the JSON contract, and issue #115's
/// citation AC requires every listed criterion to carry a citable record ID and,
/// where present, a repo-relative file/span or source-system handle — in BOTH
/// formats. Collapsing a row to `id/task/bucket` would leave a reader unable to
/// audit WHY a criterion landed in its bucket, which is the whole point of the
/// per-link resolution list.
///
/// Values are already control-sanitized and length-bounded by the pure core, so
/// no value here can forge an extra line or drive the reader's terminal.
fn criterion_text_lines(row: &crate::criteria_coverage::CriterionRowJson) -> Vec<String> {
    let mut lines = vec![format!(
        "  {} task={} task_status={} bucket={} status={} ordinal={}",
        row.record_id,
        row.parent_task_id.as_deref().unwrap_or("-"),
        row.parent_task_status.as_deref().unwrap_or("-"),
        row.bucket,
        row.criterion_status.as_deref().unwrap_or("-"),
        row.ordinal
            .map_or_else(|| "-".to_owned(), |o| o.to_string()),
    )];
    for link in &row.closing_links {
        lines.push(format!(
            "    closing={} origins={} resolution={} node_kind={} verification_kind={} status={} exit_code={}",
            link.handle,
            link.origins.join("+"),
            link.resolution,
            link.node_kind.as_deref().unwrap_or("-"),
            link.verification_kind.as_deref().unwrap_or("-"),
            link.status.as_deref().unwrap_or("-"),
            link.exit_code
                .map_or_else(|| "-".to_owned(), |c| c.to_string()),
        ));
        if let Some(producer) = &link.producer_kind {
            lines.push(format!("      producer={producer}"));
        }
    }
    if let Some(proving) = &row.proving_verification_id {
        lines.push(format!("    proven_by={proving}"));
    }
    // The source handles: only emitted when actually recorded, so the text form
    // never implies a citation the record does not carry.
    let span = row.span.map(|s| format!("{}-{}", s.start_line, s.end_line));
    let handles: Vec<String> = [
        row.repo_relative_path
            .as_deref()
            .map(|p| format!("path={p}")),
        span.map(|s| format!("span={s}")),
        row.source_handle
            .as_deref()
            .map(|h| format!("source_handle={h}")),
        row.external_link_id
            .as_deref()
            .map(|l| format!("external_link={l}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !handles.is_empty() {
        lines.push(format!("    {}", handles.join(" ")));
    }
    lines
}

/// Routes `eg audit evidence-pack` actions (issue #338).
pub(crate) fn evidence_pack_cmd(action: EvidencePackAction) -> Result<()> {
    match action {
        EvidencePackAction::Assemble {
            control,
            from,
            to,
            graph,
            data_dir,
            catalog,
            min_review_coverage,
            captured_at,
            format,
        } => evidence_pack_assemble_cmd(
            &control,
            &from,
            &to,
            graph.as_deref(),
            data_dir.as_deref(),
            catalog,
            min_review_coverage,
            captured_at.as_deref(),
            format,
        ),
        EvidencePackAction::Verify { path, format } => evidence_pack_verify_cmd(&path, format),
    }
}

/// Prints a redaction-safe JSON error and exits with the usage/load code (2).
pub(crate) fn evidence_pack_exit(value: &serde_json::Value) -> ! {
    eprintln!("{value}");
    std::process::exit(2);
}

/// Reads and parses a `--graph` JSONL for the evidence-pack assemble path with a
/// SANITIZED load error (Codex round-19 P2).
///
/// `load_query_records`/`load_records_from_jsonl` stringify the adapter error
/// into an anyhow message; for a wrong-typed `GraphRecord` field serde's
/// `Error::to_string()` embeds the offending VALUE (e.g.
/// `invalid type: string "AKIA...", expected u64`), which rides
/// `AdapterError::Parse.message` and, when printed verbatim, leaked a secret
/// placed in a mistyped field. This reads + parses the graph directly so the
/// serde-message-bearing `Parse` variant can be rewritten to a stable
/// redaction-safe envelope — a value-free serde category plus the 1-based JSONL
/// line, never the raw message — mirroring the pack/catalog parse-error
/// sanitizer. Every other adapter error variant (unknown schema version, etc.)
/// carries no value leak and keeps its existing safe stringified handling. Exits
/// the process (2) on any load error.
fn load_graph_records_sanitized(graph_path: &Path) -> Vec<GraphRecord> {
    let jsonl = fs::read_to_string(graph_path).unwrap_or_else(|error| {
        evidence_pack_exit(&serde_json::json!({
            "code": "graph_read_error",
            "path": graph_path.display().to_string(),
            "message": error.to_string(),
        }))
    });
    match crate::adapters::records_from_jsonl(&jsonl) {
        Ok(records) => records,
        Err(crate::adapters::AdapterError::Parse { line, .. }) => {
            // Re-derive a value-free serde category by re-parsing the offending
            // line as a `GraphRecord` (the same deterministic failure, minus the
            // leaking message). Fall back to a stable generic category when the
            // line text is unavailable.
            use serde_json::error::Category;
            let category = jsonl
                .lines()
                .nth(line.saturating_sub(1))
                .and_then(|l| serde_json::from_str::<GraphRecord>(l).err())
                .map_or("data", |e| match e.classify() {
                    Category::Io => "io",
                    Category::Syntax => "syntax",
                    Category::Data => "data",
                    Category::Eof => "eof",
                });
            evidence_pack_exit(&serde_json::json!({
                "code": "graph_parse_error",
                "path": graph_path.display().to_string(),
                "jsonl_line": line,
                "category": category,
            }));
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

/// Handles `eg audit evidence-pack assemble` (issue #338): builds a
/// control-scoped, time-windowed evidence pack. Exit 0 all verdicts pass, 1 any
/// verdict fails (report still printed), 2 usage/load error.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn evidence_pack_assemble_cmd(
    control: &str,
    from: &str,
    to: &str,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    catalog: Option<PathBuf>,
    min_review_coverage: f64,
    captured_at: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    use crate::evidence_pack::{self, DEFAULT_SOC2_CATALOG_JSON, Window};

    if !min_review_coverage.is_finite() || !(0.0..=1.0).contains(&min_review_coverage) {
        evidence_pack_exit(&serde_json::json!({
            "code": "invalid_min_review_coverage",
            "value": min_review_coverage.to_string(),
            "message": "--min-review-coverage must be a finite value in [0.0, 1.0]",
        }));
    }

    // Enforce exactly-one-of the input flags before opening any store, so the
    // both/neither error is precise rather than a downstream store-copy failure.
    match (graph, data_dir) {
        (Some(_), Some(_)) => evidence_pack_exit(&serde_json::json!({
            "code": "conflicting_input_flags",
            "message": "provide only one of --graph or --data-dir, not both",
        })),
        (None, None) => evidence_pack_exit(&serde_json::json!({
            "code": "missing_input_flag",
            "message": "provide --graph <path> or --data-dir <path>",
        })),
        _ => {}
    }

    // Load and validate the catalog first (exit 2 on any read/parse error).
    let catalog_text = catalog.map_or_else(
        || DEFAULT_SOC2_CATALOG_JSON.to_owned(),
        |path| {
            fs::read_to_string(&path).unwrap_or_else(|error| {
                evidence_pack_exit(&serde_json::json!({
                    "code": "catalog_read_error",
                    "path": path.display().to_string(),
                    "message": error.to_string(),
                }))
            })
        },
    );
    let parsed_catalog = evidence_pack::parse_catalog(&catalog_text)
        .unwrap_or_else(|error| evidence_pack_exit(&error.to_json()));

    // Load records read-only. An embedded store is read through a throwaway copy.
    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());
    #[allow(clippy::option_if_let_else)] // `--graph` uses a sanitizing reader (round-19 P2)
    let records = match graph {
        Some(graph_path) => load_graph_records_sanitized(graph_path),
        None => match load_query_records(graph, effective_data_dir) {
            Ok(records) => records,
            Err(error) => {
                eprintln!("{error}");
                drop(store_copy);
                std::process::exit(2);
            }
        },
    };

    // A genuinely empty evidence input (zero records loaded — an empty or
    // whitespace-only graph, or an initialized store holding zero records) is a
    // LOAD error naming the path (AC6), distinct from the vacuous `empty_window`
    // SUCCESS, which is a non-empty store whose records simply fall outside the
    // window.
    if records.is_empty() {
        let source_path = graph
            .or(data_dir)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        drop(store_copy);
        evidence_pack_exit(&serde_json::json!({
            "code": "empty_evidence_input",
            "path": source_path,
            "message": "evidence input holds zero records; provide a non-empty graph or store",
        }));
    }

    let window = Window {
        from: from.to_owned(),
        to: to.to_owned(),
    };
    let pack = match evidence_pack::assemble_pack(
        &records,
        &parsed_catalog,
        control,
        &window,
        min_review_coverage,
        env!("CARGO_PKG_VERSION"),
        captured_at,
    ) {
        Ok(pack) => pack,
        Err(error) => {
            drop(store_copy);
            evidence_pack_exit(&error.to_json());
        }
    };

    // A whole-artifact SAFETY failure means the pack still carries a raw secret
    // in some field (e.g. a malicious `--catalog` title copied into
    // `manifest.control_title`). Emitting the pack would leak it, so this case is
    // handled BEFORE any serialization: suppress the artifact entirely and emit a
    // redaction-safe `pack_safety_failed` envelope to stderr, exit 2 ("cannot
    // emit a redaction-safe artifact", consistent with every other stderr-only
    // error path here). The safety `detail` names the field label + secret class
    // only, never the value (Codex round-12 P1 Finding 2). This special-casing
    // applies ONLY to Safety: a non-safety verdict failure (citation shortfall,
    // required-class unavailable, review-coverage) produces a redaction-safe pack
    // that MUST still print the full report to stdout with exit 1 below.
    if !pack.verdicts.safety.passed {
        drop(store_copy);
        evidence_pack_exit(&serde_json::json!({
            "code": "pack_safety_failed",
            "detail": pack.verdicts.safety.detail,
            "message": "assembled pack failed whole-artifact safety; artifact suppressed to avoid leaking a raw secret",
        }));
    }

    let output = match format {
        OutputFormat::Json => {
            serde_json::to_string(&pack).context("failed to serialize evidence pack")?
        }
        OutputFormat::Text => render_pack_text(&pack),
    };
    println!("{output}");
    let exit_code = i32::from(!pack.verdicts.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Handles `eg audit evidence-pack verify` (issue #338): re-verifies an
/// assembled pack offline. Exit 0 all checks pass, 1 any fails (report still
/// printed), 2 unreadable/unparseable pack.
pub(crate) fn evidence_pack_verify_cmd(path: &Path, format: OutputFormat) -> Result<()> {
    use crate::evidence_pack::{EvidencePack, verify_pack};

    let text = fs::read_to_string(path).unwrap_or_else(|error| {
        evidence_pack_exit(&serde_json::json!({
            "code": "pack_read_error",
            "path": path.display().to_string(),
            "message": error.to_string(),
        }))
    });
    let pack: EvidencePack = serde_json::from_str(&text).unwrap_or_else(|error| {
        // Sanitize the serde error: `Error::to_string()` embeds the offending
        // VALUE for a wrong-typed field (e.g. `invalid type: string "SECRET",
        // expected usize`), so a secret in a mistyped pack field would leak
        // despite the redaction-safe contract (Codex round-11 Finding B). Emit
        // only a stable category plus 1-based line/column, mirroring the catalog
        // parser (`sanitize_json_error` / `CatalogError::Json`) — never the raw
        // message or value.
        use serde_json::error::Category;
        let category = match error.classify() {
            Category::Io => "io",
            Category::Syntax => "syntax",
            Category::Data => "data",
            Category::Eof => "eof",
        };
        evidence_pack_exit(&serde_json::json!({
            "code": "pack_parse_error",
            "path": path.display().to_string(),
            "line": error.line(),
            "column": error.column(),
            "category": category,
        }))
    });

    let mut report = verify_pack(&pack);
    // serde silently DROPS unknown object keys when deserializing into
    // `EvidencePack`, so a secret planted in an unknown field (top-level or
    // nested) is gone before `verify_pack`'s whole-artifact Safety scan runs — a
    // pack that visibly contains a secret could otherwise verify clean (exit 0),
    // defeating the "verify scans the entire supplied artifact" guarantee. Scan
    // the RAW file text independently of deserialization to close that gap. Only
    // when `verify_pack` itself found the artifact safe do we consult the raw
    // text; a known-field secret already failed Safety with a precise per-field
    // detail we keep. The forced detail names the secret class plus a cheap byte
    // offset location hint — never the secret value (Codex round-12 P1 Finding 1).
    if report.safety.passed
        && let Some((class, offset)) = crate::redaction::detect_secret(&text)
    {
        report.safety = crate::bundle::VerificationVerdict {
            passed: false,
            detail: format!(
                "raw pack artifact contains unredacted secret class: {} at byte offset {offset}",
                class.as_str()
            ),
        };
        report.ok = false;
    }

    let output = match format {
        OutputFormat::Json => {
            serde_json::to_string(&report).context("failed to serialize verify report")?
        }
        OutputFormat::Text => {
            let mut lines = vec![format!("ok: {}", report.ok)];
            for (name, v) in [
                ("integrity", &report.integrity),
                ("coverage", &report.coverage),
                ("safety", &report.safety),
                ("window_consistency", &report.window_consistency),
            ] {
                lines.push(format!("{name}: {} — {}", v.passed, v.detail));
            }
            lines.join("\n")
        }
    };
    println!("{output}");
    std::process::exit(i32::from(!report.ok));
}

/// Renders an assembled evidence pack as a deterministic human-readable report.
fn render_pack_text(pack: &crate::evidence_pack::EvidencePack) -> String {
    // `control_id`, `control_title`, and `catalog_id` are copied verbatim from
    // the (possibly vendor-supplied) `--catalog` document, so text mode must
    // neutralize control characters exactly as `eg audit control-catalog
    // --format text` does — a crafted title must not drive the terminal.
    use crate::evidence_pack::sanitize_catalog_text;
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "control: {} — {}",
        sanitize_catalog_text(&pack.manifest.control_id),
        sanitize_catalog_text(&pack.manifest.control_title)
    ));
    lines.push(format!(
        "window: {} <= t < {}",
        pack.manifest.window.from, pack.manifest.window.to
    ));
    lines.push(format!(
        "catalog: {} ({})",
        sanitize_catalog_text(&pack.manifest.catalog_pin.catalog_id),
        pack.manifest.catalog_pin.catalog_hash
    ));
    lines.push(format!("ok: {}", pack.verdicts.ok));
    for (name, v) in [
        ("required_classes", &pack.verdicts.required_classes),
        ("citation", &pack.verdicts.citation),
        ("integrity", &pack.verdicts.integrity),
        ("safety", &pack.verdicts.safety),
    ] {
        lines.push(format!("  {name}: {} — {}", v.passed, v.detail));
    }
    // Review coverage carries its own applicability status: `gating` for a
    // review-requiring control, `not_applicable` (neutral, never failing the
    // gate) otherwise.
    let rc = &pack.verdicts.review_coverage;
    lines.push(format!(
        "  review_coverage [{}]: {} — {}",
        rc.status, rc.passed, rc.detail
    ));
    lines.push("sections:".to_owned());
    for s in &pack.sections {
        lines.push(format!(
            "  {} [{}] {} ({} records)",
            s.class, s.requirement, s.status, s.record_count
        ));
    }
    lines.push(format!("gaps: {}", pack.gaps.len()));
    for g in &pack.gaps {
        lines.push(format!("  {} {}", g.gap_class, g.record_ids.join(",")));
    }
    lines.push(format!("disclaimer: {}", pack.manifest.disclaimer));
    lines.join("\n")
}

/// Prints a redaction-safe JSON error and exits with the load/parse code (2).
pub(crate) fn control_catalog_exit(value: &serde_json::Value) -> ! {
    eprintln!("{value}");
    std::process::exit(2);
}

/// Handles `eg audit control-catalog` (issue #337): loads, validates, and
/// hash-pins a SOC2 control->evidence-class catalog. Exit 0 valid, 2 on any
/// read/parse/unknown-class/unknown-schema-version error.
pub(crate) fn control_catalog_cmd(catalog: Option<PathBuf>, format: OutputFormat) -> Result<()> {
    use crate::evidence_pack::{self, DEFAULT_SOC2_CATALOG_JSON};

    let text = catalog.map_or_else(
        || DEFAULT_SOC2_CATALOG_JSON.to_owned(),
        |path| {
            fs::read_to_string(&path).unwrap_or_else(|error| {
                control_catalog_exit(&serde_json::json!({
                    "code": "catalog_read_error",
                    "path": path.display().to_string(),
                    "message": error.to_string(),
                }))
            })
        },
    );

    let parsed = evidence_pack::parse_catalog(&text)
        .unwrap_or_else(|error| control_catalog_exit(&error.to_json()));

    let controls: Vec<serde_json::Value> = parsed
        .controls
        .iter()
        .map(|control| {
            let classes: Vec<serde_json::Value> = control
                .evidence_classes
                .iter()
                .map(|cr| {
                    serde_json::json!({
                        "class": cr.class.as_wire(),
                        "requirement": cr.requirement.as_wire(),
                    })
                })
                .collect();
            serde_json::json!({
                "control_id": control.control_id,
                "title": control.title,
                "evidence_classes": classes,
            })
        })
        .collect();

    let catalog_hash = evidence_pack::catalog_hash(&parsed);
    let report = serde_json::json!({
        "ok": true,
        "catalog_id": parsed.catalog_id,
        "catalog_schema_version": {
            "domain": parsed.schema_version.domain,
            "kind": parsed.schema_version.kind,
            "version": parsed.schema_version.version,
        },
        "catalog_hash": catalog_hash,
        "control_count": parsed.controls.len(),
        "controls": controls,
    });

    match format {
        OutputFormat::Json => {
            // Single deterministic compact line, byte-identical across runs.
            println!(
                "{}",
                serde_json::to_string(&report)
                    .context("failed to serialize control-catalog report")?
            );
        }
        OutputFormat::Text => {
            // `catalog_id`, `control_id`, and `title` are free text from the
            // document under validation — a vendor-supplied `--catalog` could
            // carry an ANSI escape that drives the operator's terminal. JSON
            // mode escapes control characters by construction; text mode must
            // neutralize them explicitly (#104 doctrine). Schema domain/kind
            // and the class/requirement wire names are exact validated
            // constants, safe as-is.
            use crate::evidence_pack::sanitize_catalog_text;
            println!(
                "catalog: {} ({})",
                sanitize_catalog_text(&parsed.catalog_id),
                catalog_hash
            );
            println!(
                "schema_version: {} {} v{}",
                parsed.schema_version.domain,
                parsed.schema_version.kind,
                parsed.schema_version.version
            );
            println!("controls: {}", parsed.controls.len());
            for control in &parsed.controls {
                println!(
                    "  {} — {}",
                    sanitize_catalog_text(&control.control_id),
                    sanitize_catalog_text(&control.title)
                );
                for cr in &control.evidence_classes {
                    println!("    {} [{}]", cr.class.as_wire(), cr.requirement.as_wire());
                }
            }
        }
    }
    Ok(())
}

/// Prints a redaction-safe JSON error and exits with the usage/load code (2).
pub(crate) fn token_cost_exit(code: &str, path: &str, message: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({ "code": code, "path": path, "message": message })
    );
    std::process::exit(2);
}

/// Loads the corpus manifest, applies the `--min-ratio` override, and validates
/// the pinned token-count method. Exits 2 on any load/usage error.
pub(crate) fn load_token_cost_corpus(
    corpus_path: &Path,
    min_ratio_override: Option<f64>,
) -> crate::token_cost::TokenCostCorpus {
    use crate::token_cost::{TOKEN_COUNT_METHOD, TokenCostCorpus};

    if let Some(min_ratio) = min_ratio_override
        && (!min_ratio.is_finite() || min_ratio <= 0.0)
    {
        // A non-positive or non-finite override would silently disable the gate
        // (ratio >= 0.0 is always true; zero is as useless as a negative value).
        token_cost_exit(
            "invalid_min_ratio",
            &corpus_path.display().to_string(),
            "--min-ratio must be a finite, positive value",
        );
    }
    let path = corpus_path.display().to_string();
    let text = std::fs::read_to_string(corpus_path)
        .unwrap_or_else(|error| token_cost_exit("corpus_read_error", &path, &error.to_string()));
    let mut corpus: TokenCostCorpus = serde_json::from_str(&text)
        .unwrap_or_else(|error| token_cost_exit("corpus_parse_error", &path, &error.to_string()));

    // The token-count method is pinned; reject a manifest that asks for another
    // so the reported ratio is always produced by the documented method (AC3).
    if corpus.token_count_method != TOKEN_COUNT_METHOD {
        token_cost_exit(
            "unsupported_token_count_method",
            &path,
            &format!("only '{TOKEN_COUNT_METHOD}' is supported"),
        );
    }
    if let Some(min_ratio) = min_ratio_override {
        corpus.min_ratio = min_ratio;
    }
    corpus
}

/// Scans the corpus into a deterministic graph and reads its source files for
/// the grep baseline. Exits 2 on any scan/read error.
pub(crate) fn load_token_cost_inputs(
    corpus: &crate::token_cost::TokenCostCorpus,
    source_dir: &Path,
) -> (Vec<GraphRecord>, BTreeMap<String, String>) {
    let dir = source_dir.display().to_string();
    let graph = crate::scan_repository_at_with_override(
        source_dir,
        &corpus.scan_time,
        Some(&corpus.repository_id_override),
    )
    .unwrap_or_else(|error| token_cost_exit("corpus_scan_error", &dir, &error.to_string()));
    let records = graph.records().to_vec();

    // Read the same source files for the grep-shaped baseline, keyed by their
    // repo-relative path so the baseline reads exactly what the scan indexed.
    let mut source_files: BTreeMap<String, String> = BTreeMap::new();
    let discovered = crate::fs::discover_source_files(source_dir)
        .unwrap_or_else(|error| token_cost_exit("corpus_discover_error", &dir, &error.to_string()));
    for source_file in discovered {
        let content = std::fs::read_to_string(&source_file.path).unwrap_or_else(|error| {
            token_cost_exit(
                "corpus_read_error",
                &source_file.path.display().to_string(),
                &error.to_string(),
            )
        });
        source_files.insert(source_file.repo_relative_path.clone(), content);
    }
    (records, source_files)
}

pub(crate) fn audit_token_cost_cmd(
    corpus_path: &Path,
    min_ratio_override: Option<f64>,
    format: OutputFormat,
) -> Result<()> {
    let corpus = load_token_cost_corpus(corpus_path, min_ratio_override);

    // Resolve the corpus source directory relative to the manifest's parent so
    // the gate is runnable regardless of the working directory.
    let manifest_dir = corpus_path.parent().unwrap_or_else(|| Path::new("."));
    let source_dir = manifest_dir.join(&corpus.source_dir);
    let corpus_display = source_dir.to_string_lossy().replace('\\', "/");

    let (records, source_files) = load_token_cost_inputs(&corpus, &source_dir);
    let report =
        crate::token_cost::run_token_cost_report(&corpus, &source_files, &records, &corpus_display);

    let output = match format {
        OutputFormat::Json | OutputFormat::Text => serde_json::to_string_pretty(&report)
            .unwrap_or_else(|error| token_cost_exit("serialize_error", "", &error.to_string())),
    };
    println!("{output}");
    std::process::exit(i32::from(!report.ok));
}

/// Prints a JSON error envelope to stderr and exits 2 (issue #255).
pub(crate) fn query_latency_exit(code: &str, path: &str, message: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({ "code": code, "path": path, "message": message })
    );
    std::process::exit(2);
}

/// Loads the query-latency corpus manifest, applies the `--samples` /
/// `--budget-p50-ms` overrides, and validates them. Exits 2 on any
/// load/usage error.
pub(crate) fn load_query_latency_corpus(
    corpus_path: &Path,
    samples_override: Option<usize>,
    budget_override: Option<f64>,
) -> crate::query_latency::QueryLatencyCorpus {
    use crate::query_latency::QueryLatencyCorpus;

    let display = corpus_path.display().to_string();
    if let Some(samples) = samples_override
        && samples == 0
    {
        // Zero samples would gate on nothing; refuse instead of passing vacuously.
        query_latency_exit("invalid_samples", &display, "--samples must be at least 1");
    }
    if let Some(budget) = budget_override
        && (!budget.is_finite() || budget <= 0.0)
    {
        // A non-positive or non-finite budget would silently disable the gate
        // (p50 >= 0.0 is always true against a negative budget).
        query_latency_exit(
            "invalid_budget_p50_ms",
            &display,
            "--budget-p50-ms must be a finite, positive value",
        );
    }
    let text = std::fs::read_to_string(corpus_path).unwrap_or_else(|error| {
        query_latency_exit("corpus_read_error", &display, &error.to_string())
    });
    let mut corpus: QueryLatencyCorpus = serde_json::from_str(&text).unwrap_or_else(|error| {
        query_latency_exit("corpus_parse_error", &display, &error.to_string())
    });
    if let Some(samples) = samples_override {
        corpus.samples = samples;
    }
    if let Some(budget) = budget_override {
        corpus.budget_p50_ms = budget;
    }
    if corpus.samples == 0 {
        query_latency_exit(
            "invalid_samples",
            &display,
            "manifest `samples` must be at least 1",
        );
    }
    if !corpus.budget_p50_ms.is_finite() || corpus.budget_p50_ms <= 0.0 {
        query_latency_exit(
            "invalid_budget_p50_ms",
            &display,
            "manifest `budget_p50_ms` must be a finite, positive value",
        );
    }
    corpus
}

/// Builds the reference corpus in a temp dir: deterministic scan of the
/// fixture into `graph.jsonl`. Returns the work dir, the JSONL path, and the
/// record count. Setup only — not timed.
fn build_query_latency_corpus(
    corpus: &crate::query_latency::QueryLatencyCorpus,
    source_dir: &Path,
) -> (tempfile::TempDir, std::path::PathBuf, u64) {
    use crate::query_latency::MIN_REFERENCE_RECORDS;

    let dir = source_dir.display().to_string();
    let work = tempfile::tempdir()
        .unwrap_or_else(|error| query_latency_exit("tempdir_error", &dir, &error.to_string()));
    let graph = crate::scan_repository_at_with_override(
        source_dir,
        &corpus.scan_time,
        Some(&corpus.repository_id_override),
    )
    .unwrap_or_else(|error| query_latency_exit("corpus_scan_error", &dir, &error.to_string()));
    let record_count = graph.records().len() as u64;
    if record_count < MIN_REFERENCE_RECORDS {
        // A collapsed corpus would make the gate pass vacuously; refuse to
        // measure instead of rubber-stamping a meaningless number.
        query_latency_exit(
            "corpus_too_small",
            &dir,
            &format!(
                "reference corpus has {record_count} records, below the {MIN_REFERENCE_RECORDS} minimum"
            ),
        );
    }
    let jsonl = graph.to_jsonl().unwrap_or_else(|error| {
        query_latency_exit("corpus_serialize_error", &dir, &error.to_string())
    });
    let graph_path = work.path().join("graph.jsonl");
    std::fs::write(&graph_path, jsonl).unwrap_or_else(|error| {
        query_latency_exit(
            "corpus_write_error",
            &graph_path.display().to_string(),
            &error.to_string(),
        )
    });
    (work, graph_path, record_count)
}

/// Times `samples` cold `eg query symbol` invocations against one input
/// source. Exits 2 when a sample fails (fail-closed: an unanswered query has
/// no time-to-first-answer).
fn sample_query_latency(
    exe: &Path,
    base_args: &[String],
    samples: usize,
    source_name: &str,
) -> Vec<f64> {
    use crate::query_latency::measure_cold_query;

    let mut samples_ms = Vec::with_capacity(samples);
    for _ in 0..samples {
        let args: Vec<&str> = base_args.iter().map(String::as_str).collect();
        match measure_cold_query(exe, &args) {
            Ok(ms) => samples_ms.push(ms),
            Err(error) => query_latency_exit("query_sample_error", source_name, &error),
        }
    }
    samples_ms
}

/// Measures the `--data-dir` (embedded store) source for `audit query-latency`
/// (issue #255) and inserts the result into `sources`. When the
/// `embedded-aletheiadb` feature is disabled, inserts an explicit skip record
/// instead of silently dropping the source.
fn measure_data_dir_latency(
    exe: &Path,
    graph_path: &Path,
    work: &tempfile::TempDir,
    corpus: &crate::query_latency::QueryLatencyCorpus,
    sources: &mut std::collections::BTreeMap<String, crate::query_latency::SourceLatency>,
) {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    use crate::query_latency::skipped_source;
    use crate::query_latency::summarize;

    #[cfg(feature = "embedded-aletheiadb")]
    {
        let data_dir = work.path().join("data-dir");
        let data_dir_str = data_dir.display().to_string();
        let ingest_status = std::process::Command::new(exe)
            .args(["ingest"])
            .arg(graph_path)
            .args(["--adapter", "embedded", "--data-dir"])
            .arg(&data_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .unwrap_or_else(|error| {
                query_latency_exit("ingest_spawn_error", &data_dir_str, &error.to_string())
            });
        if !ingest_status.success() {
            query_latency_exit(
                "ingest_error",
                &data_dir_str,
                &format!("`eg ingest` exited with {ingest_status}"),
            );
        }
        let data_dir_args = vec![
            "query".to_owned(),
            "symbol".to_owned(),
            corpus.query_symbol.clone(),
            "--data-dir".to_owned(),
            data_dir_str,
            "--format".to_owned(),
            "text".to_owned(),
        ];
        let data_dir_samples =
            sample_query_latency(exe, &data_dir_args, corpus.samples, "data_dir");
        let data_dir_latency = summarize("data_dir", data_dir_samples, corpus.budget_p50_ms)
            .unwrap_or_else(|| query_latency_exit("no_samples", "data_dir", "no samples measured"));
        sources.insert("data_dir".to_owned(), data_dir_latency);
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        sources.insert(
            "data_dir".to_owned(),
            skipped_source(
                "data_dir",
                corpus.budget_p50_ms,
                "embedded-aletheiadb feature not enabled",
            ),
        );
    }
}

/// Handles `eg audit query-latency` (issue #255).
pub(crate) fn audit_query_latency_cmd(
    corpus_path: &Path,
    samples_override: Option<usize>,
    budget_override: Option<f64>,
    source: LatencySource,
    format: OutputFormat,
) -> Result<()> {
    use crate::query_latency::{LatencyReport, SourceLatency, machine_info, summarize};

    let corpus = load_query_latency_corpus(corpus_path, samples_override, budget_override);
    let measure_graph = matches!(source, LatencySource::Graph | LatencySource::Both);
    let measure_data_dir = matches!(source, LatencySource::DataDir | LatencySource::Both);

    // Resolve the corpus source directory relative to the manifest's parent so
    // the benchmark is runnable regardless of the working directory.
    let manifest_dir = corpus_path.parent().unwrap_or_else(|| Path::new("."));
    let source_dir = manifest_dir.join(&corpus.source_dir);

    let (work, graph_path, record_count) = build_query_latency_corpus(&corpus, &source_dir);
    let graph_path_str = graph_path.display().to_string();

    let exe = std::env::current_exe()
        .unwrap_or_else(|error| query_latency_exit("current_exe_error", "", &error.to_string()));

    let mut sources: std::collections::BTreeMap<String, SourceLatency> =
        std::collections::BTreeMap::new();

    // --graph <JSONL> source: measured unless `--source data-dir` selects the
    // embedded store alone.
    if measure_graph {
        let graph_args = vec![
            "query".to_owned(),
            "symbol".to_owned(),
            corpus.query_symbol.clone(),
            "--graph".to_owned(),
            graph_path_str,
            "--format".to_owned(),
            "text".to_owned(),
        ];
        let graph_samples = sample_query_latency(&exe, &graph_args, corpus.samples, "graph");
        let graph_latency = summarize("graph", graph_samples, corpus.budget_p50_ms)
            .unwrap_or_else(|| query_latency_exit("no_samples", "graph", "no samples measured"));
        sources.insert("graph".to_owned(), graph_latency);
    }

    // --data-dir <embedded store> source: measured when selected and the
    // embedded feature is enabled; explicitly skipped (never silently
    // dropped) when selected without the feature.
    if measure_data_dir {
        measure_data_dir_latency(&exe, &graph_path, &work, &corpus, &mut sources);
    }

    let ok = sources.values().all(|source| source.pass);
    let report = LatencyReport {
        corpus_name: corpus.corpus_name.clone(),
        corpus_version: corpus.corpus_version.clone(),
        query: format!("query symbol {}", corpus.query_symbol),
        record_count,
        reference_record_count: corpus.reference_record_count,
        reference_machine_class: corpus.reference_machine_class.clone(),
        machine: machine_info(),
        samples: corpus.samples,
        budget_p50_ms: corpus.budget_p50_ms,
        sources,
        ok,
    };

    let output = match format {
        OutputFormat::Json | OutputFormat::Text => serde_json::to_string_pretty(&report)
            .unwrap_or_else(|error| query_latency_exit("serialize_error", "", &error.to_string())),
    };
    println!("{output}");
    for (name, source) in &report.sources {
        if source.skipped {
            eprintln!(
                "query-latency[{name}]: skipped ({})",
                source.skip_reason.as_deref().unwrap_or("")
            );
        } else {
            eprintln!(
                "query-latency[{name}]: p50 {:.0}ms, p95 {:.0}ms over {} samples (budget {:.0}ms) — {}",
                source.p50_ms,
                source.p95_ms,
                source.samples_ms.len(),
                source.budget_p50_ms,
                if source.pass { "PASS" } else { "FAIL" },
            );
        }
    }
    std::process::exit(i32::from(!report.ok));
}

/// Loads the query-budget corpus manifest, applies the CLI overrides, and
/// validates them. Exits 2 on any load/usage error (issue #120).
pub(crate) fn load_query_budget_corpus(
    corpus_path: &Path,
    samples_override: Option<usize>,
    warm_samples_override: Option<usize>,
    scale_factor_override: Option<usize>,
    max_ratio_override: Option<f64>,
    budget_p95_override: Option<f64>,
) -> crate::query_budget::QueryBudgetCorpus {
    use crate::query_budget::QueryBudgetCorpus;

    let display = corpus_path.display().to_string();
    // Overrides that would silently disable the gate are rejected before any
    // measurement runs.
    if samples_override == Some(0) {
        query_latency_exit("invalid_samples", &display, "--samples must be at least 1");
    }
    if warm_samples_override == Some(0) {
        query_latency_exit(
            "invalid_warm_samples",
            &display,
            "--warm-samples must be at least 1",
        );
    }
    if scale_factor_override.is_some_and(|scale| scale < 2) {
        query_latency_exit(
            "invalid_scale_factor",
            &display,
            "--scale-factor must be at least 2",
        );
    }
    if max_ratio_override.is_some_and(|ratio| !ratio.is_finite() || ratio <= 0.0) {
        query_latency_exit(
            "invalid_max_ratio",
            &display,
            "--max-ratio must be a finite, positive value",
        );
    }
    if budget_p95_override.is_some_and(|budget| !budget.is_finite() || budget <= 0.0) {
        query_latency_exit(
            "invalid_budget_p95_ms",
            &display,
            "--budget-p95-ms must be a finite, positive value",
        );
    }
    let text = std::fs::read_to_string(corpus_path).unwrap_or_else(|error| {
        query_latency_exit("corpus_read_error", &display, &error.to_string())
    });
    let mut corpus: QueryBudgetCorpus = serde_json::from_str(&text).unwrap_or_else(|error| {
        query_latency_exit("corpus_parse_error", &display, &error.to_string())
    });
    if let Some(samples) = samples_override {
        corpus.samples = samples;
    }
    if let Some(warm_samples) = warm_samples_override {
        corpus.warm_samples = warm_samples;
    }
    if let Some(scale_factor) = scale_factor_override {
        corpus.scale_factor = scale_factor;
    }
    if let Some(max_ratio) = max_ratio_override {
        corpus.max_scaling_ratio = max_ratio;
    }
    if let Some(budget) = budget_p95_override {
        corpus.budget_p95_ms = budget;
    }
    // Manifest values get the same validation as CLI overrides.
    if corpus.samples == 0 {
        query_latency_exit(
            "invalid_samples",
            &display,
            "manifest `samples` must be at least 1",
        );
    }
    if corpus.warm_samples == 0 {
        query_latency_exit(
            "invalid_warm_samples",
            &display,
            "manifest `warm_samples` must be at least 1",
        );
    }
    if corpus.scale_factor < 2 {
        query_latency_exit(
            "invalid_scale_factor",
            &display,
            "manifest `scale_factor` must be at least 2",
        );
    }
    if !corpus.max_scaling_ratio.is_finite() || corpus.max_scaling_ratio <= 0.0 {
        query_latency_exit(
            "invalid_max_ratio",
            &display,
            "manifest `max_scaling_ratio` must be a finite, positive value",
        );
    }
    if !corpus.budget_p50_ms.is_finite() || corpus.budget_p50_ms <= 0.0 {
        query_latency_exit(
            "invalid_budget_p50_ms",
            &display,
            "manifest `budget_p50_ms` must be a finite, positive value",
        );
    }
    if !corpus.budget_p95_ms.is_finite() || corpus.budget_p95_ms <= 0.0 {
        query_latency_exit(
            "invalid_budget_p95_ms",
            &display,
            "manifest `budget_p95_ms` must be a finite, positive value",
        );
    }
    corpus
}

/// The two fixture stores for the scaling assertion, with their record
/// counts. The 1x store is exactly the first slice of the scaled store.
struct QueryBudgetStores {
    /// 1x store path (`store-1x.jsonl`).
    single: std::path::PathBuf,
    /// Scaled store path (`store-10x.jsonl`).
    scaled: std::path::PathBuf,
    /// Record count of the 1x store.
    single_count: u64,
    /// Record count of the scaled store.
    scaled_count: u64,
}

/// Builds an `eg query` argv vector for one benchmarked query against a
/// store path.
type QueryArgv = Box<dyn Fn(&str) -> Vec<String>>;

/// Builds the 1x and scaled fixture stores in a temp dir (issue #120).
///
/// Scans the fixture corpus `scale_factor` times with disjoint repository
/// identities (`<override>-sNN`), appending deterministic synthetic drift
/// nodes per copy so `eg query drift` has a stable answer. Writes
/// `store-1x.jsonl` (copy 0 alone) and `store-10x.jsonl` (all copies
/// concatenated), so the 1x store is exactly the first slice of the scaled
/// store. Setup only — not timed. Returns the temp-dir guard (keeping it
/// alive keeps both store files on disk) and the stores.
fn build_query_budget_stores(
    corpus: &crate::query_budget::QueryBudgetCorpus,
    source_dir: &Path,
) -> (tempfile::TempDir, QueryBudgetStores) {
    use crate::query_budget::{MIN_REFERENCE_RECORDS, synthetic_drift_records};

    let dir = source_dir.display().to_string();
    let work = tempfile::tempdir()
        .unwrap_or_else(|error| query_latency_exit("tempdir_error", &dir, &error.to_string()));

    let mut copies: Vec<Vec<crate::ir::GraphRecord>> = Vec::with_capacity(corpus.scale_factor);
    for copy in 0..corpus.scale_factor {
        let repo_tag = format!("{}-s{copy:02}", corpus.repository_id_override);
        let graph =
            crate::scan_repository_at_with_override(source_dir, &corpus.scan_time, Some(&repo_tag))
                .unwrap_or_else(|error| {
                    query_latency_exit("corpus_scan_error", &dir, &error.to_string())
                });
        let mut records: Vec<crate::ir::GraphRecord> = graph.records().to_vec();
        records.extend(synthetic_drift_records(
            &records,
            &repo_tag,
            &corpus.scan_time,
            corpus.synthetic_drift_nodes_per_repo,
        ));
        copies.push(records);
    }

    let single_count = copies[0].len() as u64;
    if single_count < MIN_REFERENCE_RECORDS {
        // A collapsed corpus would make the gate pass vacuously; refuse to
        // measure instead of rubber-stamping a meaningless number.
        query_latency_exit(
            "corpus_too_small",
            &dir,
            &format!(
                "reference corpus has {single_count} records, below the {MIN_REFERENCE_RECORDS} minimum"
            ),
        );
    }

    let write_store = |name: &str, records: &[crate::ir::GraphRecord]| -> std::path::PathBuf {
        let mut lines: Vec<String> = records
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .unwrap_or_else(|error| {
                query_latency_exit("corpus_serialize_error", name, &error.to_string())
            });
        // Canonically ordered lines, mirroring `Graph::to_jsonl`, so the
        // store is byte-stable across runs.
        lines.sort_unstable();
        let path = work.path().join(name);
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap_or_else(|error| {
            query_latency_exit(
                "corpus_write_error",
                &path.display().to_string(),
                &error.to_string(),
            )
        });
        path
    };

    let single = write_store("store-1x.jsonl", &copies[0]);
    let all_copies: Vec<crate::ir::GraphRecord> = copies.into_iter().flatten().collect();
    let scaled_count = all_copies.len() as u64;
    let scaled = write_store("store-10x.jsonl", &all_copies);
    (
        work,
        QueryBudgetStores {
            single,
            scaled,
            single_count,
            scaled_count,
        },
    )
}

/// Times one query x temperature x store-size cell (issue #120).
///
/// Cold samples are fresh processes back-to-back. Warm samples are fresh
/// processes too, but the cell is preceded by one unmeasured priming
/// invocation of the identical query so the OS page cache is hot — the
/// process itself still cold-starts, so warm isolates page-cache effects.
/// Exits 2 when a sample fails (fail-closed: an unanswered query has no
/// time-to-first-answer).
fn sample_query_budget_cell(
    exe: &Path,
    base_args: &[String],
    samples: usize,
    warm: bool,
    cell_name: &str,
) -> Vec<f64> {
    use crate::query_budget::measure_query_sample;

    let args: Vec<&str> = base_args.iter().map(String::as_str).collect();
    if warm {
        // Priming run: unmeasured, but its failure is still a real query
        // failure, so fail closed exactly like a sample.
        if let Err(error) = measure_query_sample(exe, &args) {
            query_latency_exit("query_prime_error", cell_name, &error);
        }
    }
    let mut samples_ms = Vec::with_capacity(samples);
    for _ in 0..samples {
        match measure_query_sample(exe, &args) {
            Ok(ms) => samples_ms.push(ms),
            Err(error) => query_latency_exit("query_sample_error", cell_name, &error),
        }
    }
    samples_ms
}

/// Measures the ripgrep baseline for the equivalent symbol lookup
/// (`rg <symbol> <corpus source>`) with the same spawn-to-first-line harness,
/// or returns an explicit skip record when ripgrep is not on `PATH` — never
/// silently absent.
fn measure_ripgrep_baseline(
    corpus: &crate::query_budget::QueryBudgetCorpus,
    source_dir: &Path,
    samples: usize,
) -> crate::query_budget::RipgrepBaseline {
    use crate::query_budget::{measure_query_sample, skipped_ripgrep, summarize_ripgrep};

    let source = source_dir.display().to_string();
    let command = format!(
        "rg -n --no-heading --no-messages {} {source}",
        corpus.query_symbol
    );
    // Probe once: a missing ripgrep is an explicit skip, not a silent gap.
    let available = std::process::Command::new("rg")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !available {
        return skipped_ripgrep(
            &command,
            corpus.budget_p50_ms,
            corpus.budget_p95_ms,
            "ripgrep not found on PATH",
        );
    }
    let mut samples_ms = Vec::with_capacity(samples);
    for _ in 0..samples {
        let args = [
            "-n",
            "--no-heading",
            "--no-messages",
            corpus.query_symbol.as_str(),
            source.as_str(),
        ];
        match measure_query_sample(Path::new("rg"), &args) {
            Ok(ms) => samples_ms.push(ms),
            Err(error) => query_latency_exit("ripgrep_sample_error", &command, &error),
        }
    }
    summarize_ripgrep(
        &command,
        samples_ms,
        corpus.budget_p50_ms,
        corpus.budget_p95_ms,
    )
}

/// Renders the budget report as a human-readable table (`--format text`).
fn render_query_budget_text(report: &crate::query_budget::BudgetReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "query-budget {} — 1x: {} records, {}x: {} records ({} cold + {} warm samples per cell)",
        report.corpus_name,
        report.record_count_1x,
        report.scale_factor,
        report.record_count_10x,
        report.samples,
        report.warm_samples,
    );
    let _ = writeln!(
        out,
        "  {:<6} {:<4} {:<9} {:>9} {:>9}",
        "query", "temp", "size", "p50", "p95"
    );
    for (query, temps) in &report.queries {
        for (temp, sizes) in temps {
            for (size, cell) in sizes {
                let _ = writeln!(
                    out,
                    "  {query:<6} {temp:<4} {size:<9} {:>7.0}ms {:>7.0}ms",
                    cell.p50_ms, cell.p95_ms,
                );
            }
        }
    }
    if report.ripgrep.skipped {
        let _ = writeln!(
            out,
            "  ripgrep baseline: skipped ({})",
            report.ripgrep.skip_reason.as_deref().unwrap_or(""),
        );
    } else {
        let _ = writeln!(
            out,
            "  ripgrep ({}): p50 {:.0}ms, p95 {:.0}ms over {} samples",
            report.ripgrep.command,
            report.ripgrep.p50_ms,
            report.ripgrep.p95_ms,
            report.ripgrep.samples_ms.len(),
        );
    }
    let _ = writeln!(
        out,
        "  scaling ({} {}): 10x/1x = {:.2}x p50, {:.2}x p95 (ceiling {:.1}x) — {}",
        report.scaling.query,
        report.scaling.temp,
        report.scaling.ratio_p50,
        report.scaling.ratio_p95,
        report.scaling.max_ratio,
        if report.scaling.pass { "PASS" } else { "FAIL" },
    );
    let over: Vec<&crate::query_budget::AdvisoryP95> = report
        .advisory_p95
        .iter()
        .filter(|entry| !entry.within_budget)
        .collect();
    if over.is_empty() {
        let _ = writeln!(
            out,
            "  advisory budgets (p50 {:.0}ms, p95 {:.0}ms): all cells within budget",
            report.budgets.p50_ms, report.budgets.p95_ms,
        );
    } else {
        let _ = writeln!(out, "  advisory budget exceedances (not gating):");
        for entry in over {
            let _ = writeln!(
                out,
                "    {}/{} on {}: p95 {:.0}ms > {:.0}ms",
                entry.query, entry.temp, entry.size, entry.p95_ms, entry.budget_p95_ms,
            );
        }
    }
    out
}

/// Handles `eg audit query-budget` (issue #120).
#[allow(clippy::too_many_lines)] // flat orchestration: build stores, sample 12 cells, baseline, gate, report
pub(crate) fn audit_query_budget_cmd(
    corpus_path: &Path,
    samples_override: Option<usize>,
    warm_samples_override: Option<usize>,
    scale_factor_override: Option<usize>,
    max_ratio_override: Option<f64>,
    budget_p95_override: Option<f64>,
    format: OutputFormat,
) -> Result<()> {
    use crate::query_budget::{
        AdvisoryBudgets, BudgetReport, QueryCell, ScalingAssertion, advisory_p95_entries,
        current_machine_info, scaling_passes, scaling_ratio, summarize_cell,
    };
    use std::collections::BTreeMap;

    let corpus = load_query_budget_corpus(
        corpus_path,
        samples_override,
        warm_samples_override,
        scale_factor_override,
        max_ratio_override,
        budget_p95_override,
    );

    // Resolve the corpus source directory relative to the manifest's parent so
    // the benchmark is runnable regardless of the working directory.
    let manifest_dir = corpus_path.parent().unwrap_or_else(|| Path::new("."));
    let source_dir = manifest_dir.join(&corpus.source_dir);

    // The temp-dir guard is held for its `Drop`: keeping `_guard` alive keeps
    // both store files on disk for the whole benchmark.
    let (_guard, fixture) = build_query_budget_stores(&corpus, &source_dir);

    let exe = std::env::current_exe()
        .unwrap_or_else(|error| query_latency_exit("current_exe_error", "", &error.to_string()));

    let drift_limit = corpus.drift_limit.to_string();
    let stores: [(&str, String, u64); 2] = [
        (
            "store_1x",
            fixture.single.display().to_string(),
            fixture.single_count,
        ),
        (
            "store_10x",
            fixture.scaled.display().to_string(),
            fixture.scaled_count,
        ),
    ];
    // Each benchmarked query as (name, argv builder). The argv shape is the
    // exact agent-facing invocation being budgeted. The builders own their
    // captured strings (`move`) so the boxed closures are `'static`.
    let query_symbol = corpus.query_symbol.clone();
    let query_file = corpus.query_file.clone();
    let query_argv: Vec<(&str, QueryArgv)> = vec![
        (
            "symbol",
            Box::new(move |store: &str| {
                vec![
                    "query".to_owned(),
                    "symbol".to_owned(),
                    query_symbol.clone(),
                    "--graph".to_owned(),
                    store.to_owned(),
                    "--format".to_owned(),
                    "text".to_owned(),
                ]
            }),
        ),
        (
            "file",
            Box::new(move |store: &str| {
                vec![
                    "query".to_owned(),
                    "file".to_owned(),
                    query_file.clone(),
                    "--graph".to_owned(),
                    store.to_owned(),
                    "--format".to_owned(),
                    "text".to_owned(),
                ]
            }),
        ),
        (
            "drift",
            Box::new(move |store: &str| {
                vec![
                    "query".to_owned(),
                    "drift".to_owned(),
                    "--graph".to_owned(),
                    store.to_owned(),
                    "--limit".to_owned(),
                    drift_limit.clone(),
                    "--format".to_owned(),
                    "text".to_owned(),
                ]
            }),
        ),
    ];

    let mut queries: BTreeMap<String, BTreeMap<String, BTreeMap<String, QueryCell>>> =
        BTreeMap::new();
    let mut all_cells: Vec<QueryCell> = Vec::new();
    for (query_name, make_argv) in &query_argv {
        let mut temps: BTreeMap<String, BTreeMap<String, QueryCell>> = BTreeMap::new();
        for (temp_name, warm, sample_count) in [
            ("cold", false, corpus.samples),
            ("warm", true, corpus.warm_samples),
        ] {
            let mut sizes: BTreeMap<String, QueryCell> = BTreeMap::new();
            for (size_name, store_path, record_count) in &stores {
                let argv = make_argv(store_path);
                let cell_name = format!("{query_name}/{temp_name}/{size_name}");
                let samples_ms =
                    sample_query_budget_cell(&exe, &argv, sample_count, warm, &cell_name);
                let cell =
                    summarize_cell(query_name, temp_name, size_name, samples_ms, *record_count)
                        .unwrap_or_else(|| {
                            query_latency_exit("no_samples", &cell_name, "no samples measured")
                        });
                all_cells.push(cell.clone());
                sizes.insert((*size_name).to_owned(), cell);
            }
            temps.insert(temp_name.to_owned(), sizes);
        }
        queries.insert((*query_name).to_owned(), temps);
    }

    let ripgrep = measure_ripgrep_baseline(&corpus, &source_dir, corpus.samples);

    // The gate: cold targeted single-symbol lookup, scaled store vs 1x store.
    // Relative and machine-independent — only this assertion fails the gate.
    // Copy the gated percentiles out before `queries` moves into the report.
    let symbol_cold_single = &queries["symbol"]["cold"]["store_1x"];
    let symbol_cold_scaled = &queries["symbol"]["cold"]["store_10x"];
    let (p50_single, p95_single) = (symbol_cold_single.p50_ms, symbol_cold_single.p95_ms);
    let (p50_scaled, p95_scaled) = (symbol_cold_scaled.p50_ms, symbol_cold_scaled.p95_ms);
    let ratio_p50 = scaling_ratio(p50_single, p50_scaled);
    let scaling = ScalingAssertion {
        query: "symbol".to_owned(),
        temp: "cold".to_owned(),
        ratio_p50,
        ratio_p95: scaling_ratio(p95_single, p95_scaled),
        max_ratio: corpus.max_scaling_ratio,
        pass: scaling_passes(ratio_p50, corpus.max_scaling_ratio),
    };

    let cell_refs: Vec<&QueryCell> = all_cells.iter().collect();
    let advisory_p95 = advisory_p95_entries(&cell_refs, corpus.budget_p95_ms);

    let ok = scaling.pass;
    let report = BudgetReport {
        corpus_name: corpus.corpus_name.clone(),
        corpus_version: corpus.corpus_version.clone(),
        query_symbol: corpus.query_symbol.clone(),
        query_file: corpus.query_file.clone(),
        drift_limit: corpus.drift_limit,
        record_count_1x: fixture.single_count,
        record_count_10x: fixture.scaled_count,
        scale_factor: corpus.scale_factor,
        reference_record_count: corpus.reference_record_count,
        reference_machine_class: corpus.reference_machine_class.clone(),
        machine: current_machine_info(),
        samples: corpus.samples,
        warm_samples: corpus.warm_samples,
        budgets: AdvisoryBudgets {
            p50_ms: corpus.budget_p50_ms,
            p95_ms: corpus.budget_p95_ms,
            advisory: true,
        },
        queries,
        ripgrep,
        scaling,
        advisory_p95,
        ok,
    };

    match format {
        OutputFormat::Json => {
            let output = serde_json::to_string_pretty(&report).unwrap_or_else(|error| {
                query_latency_exit("serialize_error", "", &error.to_string())
            });
            println!("{output}");
        }
        OutputFormat::Text => print!("{}", render_query_budget_text(&report)),
    }
    eprintln!(
        "query-budget[symbol/cold]: 1x p50 {:.0}ms -> {}x p50 {:.0}ms (ratio {:.2}x, ceiling {:.1}x) — {}",
        p50_single,
        corpus.scale_factor,
        p50_scaled,
        report.scaling.ratio_p50,
        report.scaling.max_ratio,
        if report.scaling.pass { "PASS" } else { "FAIL" },
    );
    let over_budget = report
        .advisory_p95
        .iter()
        .filter(|entry| !entry.within_budget)
        .count();
    if over_budget > 0 {
        eprintln!(
            "query-budget: {over_budget} cell(s) exceed the advisory p95 budget of {:.0}ms (reported, not gating)",
            report.budgets.p95_ms,
        );
    }
    std::process::exit(i32::from(!report.ok));
}

/// Loads the symbol-latency corpus manifest, applies the CLI overrides, and
/// validates them. Exits 2 on any load/usage error (issue #57).
pub(crate) fn load_symbol_latency_corpus(
    corpus_path: &Path,
    warm_samples_override: Option<usize>,
    budget_p95_override: Option<f64>,
    history_commits_override: Option<usize>,
    at_commit_index_override: Option<usize>,
) -> crate::symbol_latency::SymbolLatencyCorpus {
    use crate::symbol_latency::SymbolLatencyCorpus;

    let display = corpus_path.display().to_string();
    // Overrides that would silently disable the gate are rejected before any
    // measurement runs.
    if warm_samples_override == Some(0) {
        query_latency_exit(
            "invalid_warm_samples",
            &display,
            "--warm-samples must be at least 1",
        );
    }
    if history_commits_override.is_some_and(|commits| commits < 2) {
        query_latency_exit(
            "invalid_history_commits",
            &display,
            "--history-commits must be at least 2",
        );
    }
    if budget_p95_override.is_some_and(|budget| !budget.is_finite() || budget <= 0.0) {
        query_latency_exit(
            "invalid_budget_p95_ms",
            &display,
            "--budget-p95-ms must be a finite, positive value",
        );
    }
    let text = std::fs::read_to_string(corpus_path).unwrap_or_else(|error| {
        query_latency_exit("corpus_read_error", &display, &error.to_string())
    });
    let mut corpus: SymbolLatencyCorpus = serde_json::from_str(&text).unwrap_or_else(|error| {
        query_latency_exit("corpus_parse_error", &display, &error.to_string())
    });
    if let Some(warm_samples) = warm_samples_override {
        corpus.warm_samples = warm_samples;
    }
    if let Some(budget) = budget_p95_override {
        corpus.budget_p95_ms = budget;
    }
    if let Some(commits) = history_commits_override {
        corpus.history_commits = commits;
    }
    if let Some(at_index) = at_commit_index_override {
        corpus.at_commit_index = at_index;
    }
    // Manifest values get the same validation as CLI overrides.
    if corpus.warm_samples == 0 {
        query_latency_exit(
            "invalid_warm_samples",
            &display,
            "manifest `warm_samples` must be at least 1",
        );
    }
    if corpus.history_commits < 2 {
        query_latency_exit(
            "invalid_history_commits",
            &display,
            "manifest `history_commits` must be at least 2",
        );
    }
    if corpus.at_commit_index >= corpus.history_commits {
        query_latency_exit(
            "invalid_at_commit_index",
            &display,
            "manifest `at_commit_index` must be below `history_commits`",
        );
    }
    if !corpus.budget_p95_ms.is_finite() || corpus.budget_p95_ms <= 0.0 {
        query_latency_exit(
            "invalid_budget_p95_ms",
            &display,
            "manifest `budget_p95_ms` must be a finite, positive value",
        );
    }
    corpus
}

/// Recursively copies a directory tree (bytes + standard file modes).
fn copy_dir_tree(source: &Path, dest: &Path, context: &str) {
    let entries = std::fs::read_dir(source).unwrap_or_else(|error| {
        query_latency_exit("corpus_read_error", context, &error.to_string())
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            query_latency_exit("corpus_read_error", context, &error.to_string())
        });
        let target = dest.join(entry.file_name());
        let file_type = entry.file_type().unwrap_or_else(|error| {
            query_latency_exit("corpus_read_error", context, &error.to_string())
        });
        if file_type.is_dir() {
            std::fs::create_dir_all(&target).unwrap_or_else(|error| {
                query_latency_exit("tempdir_error", context, &error.to_string())
            });
            copy_dir_tree(&entry.path(), &target, context);
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target).unwrap_or_else(|error| {
                query_latency_exit("corpus_read_error", context, &error.to_string())
            });
        }
    }
}

/// Builds the deterministic synthetic git history for the history-replay
/// phase and the symbol-at-commit lookup (issue #57 AC1/AC6).
///
/// Copies the fixture source into `work/history-repo`, then creates
/// `commits` commits with fully pinned author/committer identity, dates,
/// and messages, so the resulting SHAs are deterministic across machines.
/// Commit 0 is the pristine fixture; each later commit appends one
/// deterministic comment line to `churn.rs`. Returns the repo path and the
/// commit SHAs oldest-first. Exits 2 when git is unavailable.
fn init_fixture_history(work: &Path, source_dir: &Path, commits: usize) -> (PathBuf, Vec<String>) {
    let context = source_dir.display().to_string();
    let available = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !available {
        query_latency_exit(
            "git_unavailable",
            &context,
            "git is required to build the fixture history",
        );
    }
    let repo = work.join("history-repo");
    std::fs::create_dir_all(&repo)
        .unwrap_or_else(|error| query_latency_exit("tempdir_error", &context, &error.to_string()));
    copy_dir_tree(source_dir, &repo, &context);

    let git = |args: &[&str], env: &[(&str, &str)]| {
        let mut command = std::process::Command::new("git");
        command.args(args).current_dir(&repo);
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command
            .output()
            .unwrap_or_else(|error| query_latency_exit("git_error", &context, &error.to_string()));
        if !output.status.success() {
            query_latency_exit(
                "git_error",
                &context,
                &format!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            );
        }
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    git(
        &[
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgsign=false",
            "init",
            "-q",
        ],
        &[],
    );

    let churn = repo.join("churn.rs");
    if !churn.is_file() {
        query_latency_exit(
            "corpus_churn_missing",
            &context,
            "fixture source has no churn.rs for the synthetic history",
        );
    }
    let mut shas = Vec::with_capacity(commits);
    for index in 0..commits {
        if index > 0 {
            // Deterministic content change: one comment line per commit.
            use std::fmt::Write as _;
            let mut line = String::new();
            let _ = writeln!(line, "// symbol-latency fixture churn line {index}");
            std::fs::OpenOptions::new()
                .append(true)
                .open(&churn)
                .and_then(|mut file| {
                    use std::io::Write as _;
                    file.write_all(line.as_bytes())
                })
                .unwrap_or_else(|error| {
                    query_latency_exit("corpus_write_error", &context, &error.to_string())
                });
        }
        // Pinned identity + timestamps make every SHA deterministic.
        let date = format!("2026-01-01T00:{index:02}:00+00:00");
        let env = [
            ("GIT_AUTHOR_NAME", "Egregore Fixture"),
            ("GIT_AUTHOR_EMAIL", "fixture@egregore.invalid"),
            ("GIT_COMMITTER_NAME", "Egregore Fixture"),
            ("GIT_COMMITTER_EMAIL", "fixture@egregore.invalid"),
            ("GIT_AUTHOR_DATE", date.as_str()),
            ("GIT_COMMITTER_DATE", date.as_str()),
        ];
        git(&["add", "-A"], &[]);
        git(
            &[
                "commit",
                "-q",
                "-m",
                &format!("symbol-latency fixture commit {index}"),
            ],
            &env,
        );
        shas.push(git(&["rev-parse", "HEAD"], &[]));
    }
    (repo, shas)
}

/// The fixture stores for the product gate, with their setup timings.
struct SymbolLatencyFixture {
    /// Temp-dir guard: keeping it alive keeps every store on disk.
    _guard: tempfile::TempDir,
    /// Plain deterministic scan store (+ synthetic drift), for
    /// symbol/file/drift.
    scan_store: PathBuf,
    /// `scan-history` store, for the symbol-at-commit lookup.
    history_store: PathBuf,
    /// The synthetic git history (boring-substitute working dir).
    history_repo: PathBuf,
    /// SHA of the `at_commit_index`-th commit (oldest-first).
    at_commit_sha: String,
    /// Record count of the plain scan store.
    scan_record_count: u64,
    /// Record count of the history store.
    history_record_count: u64,
    /// Setup phases, timed separately from warm query latency (AC6).
    setup_phases: Vec<crate::symbol_latency::SetupPhase>,
}

/// Minimum symbol records that keep the fixture representative (issue #57
/// AC1: >= 500 symbols on the pinned Rust corpus).
const MIN_FIXTURE_SYMBOLS: u64 = 500;

/// Builds the fixture: cold scan, synthetic drift, history replay, ingest,
/// and the embedding-setup record (issue #57 AC1/AC6).
///
/// Setup phases are timed individually and reported separately from warm
/// query latency; only the timed phases below run here, and none of them
/// gates.
#[allow(clippy::too_many_lines)] // flat fixture orchestration: history, scan, drift, replay, ingest, embeddings
fn build_symbol_latency_fixture(
    exe: &Path,
    corpus: &crate::symbol_latency::SymbolLatencyCorpus,
    source_dir: &Path,
) -> SymbolLatencyFixture {
    use crate::query_budget::{MIN_REFERENCE_RECORDS, synthetic_drift_records_namespaced};
    use crate::symbol_latency::{skipped_setup_phase, summarize_setup_phase};
    use std::time::Instant;

    let context = source_dir.display().to_string();
    let work = tempfile::tempdir()
        .unwrap_or_else(|error| query_latency_exit("tempdir_error", &context, &error.to_string()));

    // Deterministic synthetic history first: the history replay reads it.
    let (history_repo, shas) = init_fixture_history(
        &work.path().join("work"),
        source_dir,
        corpus.history_commits,
    );
    let at_commit_sha = shas[corpus.at_commit_index].clone();

    // Cold scan of the pinned fixture source (timed, reported separately).
    let repo_tag = corpus.repository_id_override.clone();
    let scan_start = Instant::now();
    let graph =
        crate::scan_repository_at_with_override(source_dir, &corpus.scan_time, Some(&repo_tag))
            .unwrap_or_else(|error| {
                query_latency_exit("corpus_scan_error", &context, &error.to_string())
            });
    let mut records: Vec<crate::ir::GraphRecord> = graph.records().to_vec();
    let symbol_count = records
        .iter()
        .filter(|record| record.node_kind_ref() == Some(crate::ir::NodeKind::Symbol))
        .count() as u64;
    if symbol_count < MIN_FIXTURE_SYMBOLS {
        query_latency_exit(
            "corpus_too_small",
            &context,
            &format!(
                "reference corpus has {symbol_count} symbol records, below the {MIN_FIXTURE_SYMBOLS} minimum"
            ),
        );
    }
    records.extend(synthetic_drift_records_namespaced(
        &records,
        "symbol-latency",
        &repo_tag,
        &corpus.scan_time,
        corpus.synthetic_drift_nodes_per_repo,
    ));
    let scan_record_count = records.len() as u64;
    if scan_record_count < MIN_REFERENCE_RECORDS {
        query_latency_exit(
            "corpus_too_small",
            &context,
            &format!(
                "reference corpus has {scan_record_count} records, below the {MIN_REFERENCE_RECORDS} minimum"
            ),
        );
    }
    let scan_ms = scan_start.elapsed().as_secs_f64() * 1000.0;

    let write_store = |name: &str, records: &[crate::ir::GraphRecord]| -> PathBuf {
        let mut lines: Vec<String> = records
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .unwrap_or_else(|error| {
                query_latency_exit("corpus_serialize_error", name, &error.to_string())
            });
        // Canonically ordered lines, mirroring `Graph::to_jsonl`, so the
        // store is byte-stable across runs.
        lines.sort_unstable();
        let path = work.path().join(name);
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap_or_else(|error| {
            query_latency_exit(
                "corpus_write_error",
                &path.display().to_string(),
                &error.to_string(),
            )
        });
        path
    };
    let scan_store = write_store("store-scan.jsonl", &records);

    // History replay over the synthetic git history (timed separately).
    let history_repo_tag = format!("{}-history", corpus.repository_id_override);
    let history_out = work.path().join("store-history.jsonl");
    let replay_start = Instant::now();
    let scan_args = resolve_scan_args(Some(history_repo_tag), false);
    scan_history(&history_repo, &history_out, &scan_args, None).unwrap_or_else(|error| {
        query_latency_exit("history_replay_error", &context, &error.to_string())
    });
    let replay_ms = replay_start.elapsed().as_secs_f64() * 1000.0;
    let history_record_count = std::fs::read_to_string(&history_out)
        .unwrap_or_else(|error| {
            query_latency_exit(
                "corpus_read_error",
                &history_out.display().to_string(),
                &error.to_string(),
            )
        })
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count() as u64;

    // Ingest into an embedded store (timed separately). The queries
    // themselves run against the JSONL stores; this phase exists so the
    // report carries the ingest cost honestly instead of hiding it.
    let ingest_phase = if cfg!(feature = "embedded-aletheiadb") {
        let data_dir = work.path().join("embedded-store");
        let ingest_start = Instant::now();
        let output = std::process::Command::new(exe)
            .args([
                "ingest",
                &scan_store.display().to_string(),
                "--adapter",
                "embedded",
                "--data-dir",
                &data_dir.display().to_string(),
            ])
            .output();
        let ingest_ms = ingest_start.elapsed().as_secs_f64() * 1000.0;
        match output {
            Ok(completed) if completed.status.success() => {
                summarize_setup_phase("ingest", ingest_ms)
            }
            Ok(completed) => skipped_setup_phase(
                "ingest",
                &format!(
                    "ingest exited {}: {}",
                    completed.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&completed.stderr).trim()
                ),
            ),
            Err(error) => skipped_setup_phase("ingest", &error.to_string()),
        }
    } else {
        skipped_setup_phase(
            "ingest",
            "embedded-aletheiadb feature not enabled in this build",
        )
    };

    // Embedding setup: no measured query needs embeddings (all four classes
    // are structural), so setup is recorded, not performed. Resolving the
    // model is local and timed; materializing it would download the model on
    // first `eg ingest --embed`, which is network-dependent and explicitly
    // out of this benchmark's local-first contract.
    let embedding_phase = if cfg!(feature = "embeddings") {
        let resolve_start = Instant::now();
        let (model, _) = resolve_embed_model(None);
        let resolve_ms = resolve_start.elapsed().as_secs_f64() * 1000.0;
        let mut phase = summarize_setup_phase("embedding-setup", resolve_ms);
        phase.skipped = true;
        phase.skip_reason = Some(format!(
            "no measured query requires embeddings; first `eg ingest --embed` would download the resolved model `{model}` (network)"
        ));
        phase
    } else {
        skipped_setup_phase(
            "embedding-setup",
            "embeddings feature not enabled in this build",
        )
    };

    SymbolLatencyFixture {
        _guard: work,
        scan_store,
        history_store: history_out,
        history_repo,
        at_commit_sha,
        scan_record_count,
        history_record_count,
        setup_phases: vec![
            summarize_setup_phase("cold-scan", scan_ms),
            summarize_setup_phase("history-replay", replay_ms),
            ingest_phase,
            embedding_phase,
        ],
    }
}

/// Times one warm query cell: each of `warm_samples` fresh processes is
/// preceded by one unmeasured priming invocation of the identical query, so
/// warm isolates hot page-cache / filesystem effects (the process itself
/// still cold-starts).
///
/// Returns `(latencies_ms, full_outputs)`: the full stdout of every sample
/// is captured so the answer can be correctness-checked and compared across
/// runs for determinism. Exits 2 when a sample fails (fail-closed: an
/// unanswered query has no time-to-first-citable-answer).
fn sample_symbol_latency_cell(
    exe: &Path,
    base_args: &[String],
    warm_samples: usize,
    cell_name: &str,
) -> (Vec<f64>, Vec<String>) {
    use crate::symbol_latency::measure_query_full;

    let args: Vec<&str> = base_args.iter().map(String::as_str).collect();
    let mut samples_ms = Vec::with_capacity(warm_samples);
    let mut outputs = Vec::with_capacity(warm_samples);
    for _ in 0..warm_samples {
        // Priming run: unmeasured, but its failure is still a real query
        // failure, so fail closed exactly like a sample.
        if let Err(error) = measure_query_full(exe, &args) {
            query_latency_exit("query_prime_error", cell_name, &error);
        }
        match measure_query_full(exe, &args) {
            Ok(sample) => {
                samples_ms.push(sample.first_line_ms);
                outputs.push(sample.stdout);
            }
            Err(error) => query_latency_exit("query_sample_error", cell_name, &error),
        }
    }
    (samples_ms, outputs)
}

/// Samples for each boring substitute (small fixed count: substitutes are
/// fast text tools, not the gated path).
const SUBSTITUTE_SAMPLES: usize = 3;

/// Times the boring substitutes for the four query classes (issue #57 AC4),
/// or records an explicit skip when the tool is unavailable — never
/// silently absent.
#[allow(clippy::too_many_lines)] // flat per-substitute blocks: rg, git grep, rg-on-file, git show, drift n/a
fn measure_symbol_latency_substitutes(
    corpus: &crate::symbol_latency::SymbolLatencyCorpus,
    source_dir: &Path,
    history_repo: &Path,
    at_sha: &str,
) -> Vec<crate::symbol_latency::SubstituteComparison> {
    use crate::query_latency::measure_cold_query;
    use crate::symbol_latency::{Comparability, skipped_substitute, summarize_substitute};

    let mut substitutes = Vec::new();

    let tool_available = |tool: &str| {
        std::process::Command::new(tool)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    };

    // symbol -> ripgrep: the equivalent text search for the exact symbol.
    // Comparable as a timing, but the note is explicit: text hits are not
    // citable graph handles.
    let rg_symbol_command = format!(
        "rg -n --no-heading --no-messages {} {}",
        corpus.query_symbol,
        source_dir.display()
    );
    if tool_available("rg") {
        let source = source_dir.display().to_string();
        let mut samples_ms = Vec::with_capacity(SUBSTITUTE_SAMPLES);
        for _ in 0..SUBSTITUTE_SAMPLES {
            let args = [
                "-n",
                "--no-heading",
                "--no-messages",
                corpus.query_symbol.as_str(),
                source.as_str(),
            ];
            match measure_cold_query(Path::new("rg"), &args) {
                Ok(ms) => samples_ms.push(ms),
                Err(error) => {
                    query_latency_exit("substitute_sample_error", &rg_symbol_command, &error)
                }
            }
        }
        substitutes.push(summarize_substitute(
            "symbol",
            "ripgrep",
            &rg_symbol_command,
            samples_ms,
            Comparability::Comparable,
            "timing is comparable, but ripgrep returns file:line text hits — not citable graph record IDs, so it cannot answer the agent's question",
        ));
    } else {
        substitutes.push(skipped_substitute(
            "symbol",
            "ripgrep",
            &rg_symbol_command,
            "ripgrep not found on PATH",
            "ripgrep returns file:line text hits, not citable graph record IDs",
        ));
    }

    // symbol -> git grep: text search scoped to the git history working tree.
    let repo_display = history_repo.display().to_string();
    let git_grep_command = format!("git -C {repo_display} grep -n {} -- .", corpus.query_symbol);
    if tool_available("git") {
        let mut samples_ms = Vec::with_capacity(SUBSTITUTE_SAMPLES);
        for _ in 0..SUBSTITUTE_SAMPLES {
            let args = [
                "-C",
                repo_display.as_str(),
                "grep",
                "-n",
                "--no-color",
                corpus.query_symbol.as_str(),
                "--",
                ".",
            ];
            match measure_cold_query(Path::new("git"), &args) {
                Ok(ms) => samples_ms.push(ms),
                Err(error) => {
                    query_latency_exit("substitute_sample_error", &git_grep_command, &error)
                }
            }
        }
        substitutes.push(summarize_substitute(
            "symbol",
            "git-grep",
            &git_grep_command,
            samples_ms,
            Comparability::Comparable,
            "timing is comparable, but git grep returns file:line text hits — not citable graph record IDs and no DEFINES edges",
        ));
    } else {
        substitutes.push(skipped_substitute(
            "symbol",
            "git-grep",
            &git_grep_command,
            "git not found on PATH",
            "git grep returns file:line text hits, not citable graph record IDs",
        ));
    }

    // file -> ripgrep over the single file for top-level item definitions.
    // Not comparable: a regex can list candidate definition lines, but it
    // cannot return DEFINES edges or record IDs.
    let file_path = source_dir.join(&corpus.query_file);
    let rg_file_command = format!(
        "rg -n --no-heading --no-messages '^\\s*(pub\\s+)?(fn|struct|enum|trait|mod|type|const|static)\\b' {}",
        file_path.display()
    );
    if tool_available("rg") {
        let file_display = file_path.display().to_string();
        let mut samples_ms = Vec::with_capacity(SUBSTITUTE_SAMPLES);
        for _ in 0..SUBSTITUTE_SAMPLES {
            let args = [
                "-n",
                "--no-heading",
                "--no-messages",
                r"^\s*(pub\s+)?(fn|struct|enum|trait|mod|type|const|static)\b",
                file_display.as_str(),
            ];
            match measure_cold_query(Path::new("rg"), &args) {
                Ok(ms) => samples_ms.push(ms),
                Err(error) => {
                    query_latency_exit("substitute_sample_error", &rg_file_command, &error)
                }
            }
        }
        substitutes.push(summarize_substitute(
            "file",
            "ripgrep",
            &rg_file_command,
            samples_ms,
            Comparability::NotComparable,
            "not comparable: a definition-line regex cannot return DEFINES edges or citable graph record IDs — it approximates the question but cannot answer it",
        ));
    } else {
        substitutes.push(skipped_substitute(
            "file",
            "ripgrep",
            &rg_file_command,
            "ripgrep not found on PATH",
            "not comparable: no text search returns DEFINES edges or citable graph record IDs",
        ));
    }

    // symbol-at-commit -> git show <sha>:<file>: the boring retrieval step
    // for "the code at this commit". Not comparable: it returns the file's
    // bytes (no symbol record, no span, no record ID) — finding the symbol
    // within the file is left to the reader, so this times retrieval only,
    // a lower bound on the boring path.
    let git_show_command = format!("git -C {repo_display} show {at_sha}:{}", corpus.query_file);
    if tool_available("git") {
        let mut samples_ms = Vec::with_capacity(SUBSTITUTE_SAMPLES);
        for _ in 0..SUBSTITUTE_SAMPLES {
            let args = [
                "-C",
                repo_display.as_str(),
                "show",
                &format!("{}:{}", at_sha, corpus.query_file),
            ];
            match measure_cold_query(Path::new("git"), &args) {
                Ok(ms) => samples_ms.push(ms),
                Err(error) => {
                    query_latency_exit("substitute_sample_error", &git_show_command, &error)
                }
            }
        }
        substitutes.push(summarize_substitute(
            "symbol-at-commit",
            "git-show",
            &git_show_command,
            samples_ms,
            Comparability::NotComparable,
            "not comparable: git show returns the file's bytes at the commit — no symbol record, no span, no record ID; finding the symbol within the file is left to the reader, so this times retrieval only (a lower bound on the boring path)",
        ));
    } else {
        substitutes.push(skipped_substitute(
            "symbol-at-commit",
            "git-show",
            &git_show_command,
            "git not found on PATH",
            "not comparable: history archaeology cannot return citable graph handles",
        ));
    }

    // drift: semantic drift has no text-search equivalent at all.
    substitutes.push(skipped_substitute(
        "drift",
        "n/a",
        "n/a — semantic drift has no text-search equivalent",
        "no boring substitute exists for the semantic-drift listing",
        "not comparable: semantic drift is an embeddings-over-history product; no text tool returns ranked drift records with citable handles",
    ));

    substitutes
}

/// Renders the symbol-latency report as a human-readable table
/// (`--format text`).
fn render_symbol_latency_text(report: &crate::symbol_latency::SymbolLatencyReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "symbol-latency {} — {} records, {} store ({} warm samples per class, budget p95 {:.0}ms)",
        report.corpus_name,
        report.environment.record_count,
        report.environment.store_kind,
        report.warm_samples,
        report.budget_p95_ms,
    );
    let _ = writeln!(
        out,
        "environment: {} / {} / {} profile / egregore {}",
        report.environment.os,
        report.environment.cpu_class,
        report.environment.rust_profile,
        report.environment.egregore_version,
    );
    let _ = writeln!(out, "setup (reported separately from warm query latency):");
    for phase in &report.setup_phases {
        if phase.skipped {
            let _ = writeln!(
                out,
                "  {:<15} skipped: {}",
                phase.phase,
                phase.skip_reason.as_deref().unwrap_or("no reason given"),
            );
        } else {
            let _ = writeln!(out, "  {:<15} {:>8.0}ms", phase.phase, phase.duration_ms);
        }
    }
    let _ = writeln!(out, "queries:");
    for class in crate::symbol_latency::QueryClass::all() {
        let cell = &report.queries[class.as_str()];
        let _ = writeln!(
            out,
            "  {:<15} p50 {:>7.0}ms  p95 {:>7.0}ms  answer: {} row(s), {} citable handle(s), deterministic: {} — {}",
            cell.query,
            cell.p50_ms,
            cell.p95_ms,
            cell.answer.rows,
            cell.answer.handles.len(),
            cell.deterministic,
            if cell.within_budget { "PASS" } else { "FAIL" },
        );
    }
    let _ = writeln!(out, "boring substitutes:");
    for substitute in &report.substitutes {
        if substitute.skipped {
            let _ = writeln!(
                out,
                "  {:<15} {:<8} skipped: {}",
                substitute.query,
                substitute.substitute,
                substitute
                    .skip_reason
                    .as_deref()
                    .unwrap_or("no reason given"),
            );
        } else {
            let _ = writeln!(
                out,
                "  {:<15} {:<8} p50 {:>7.0}ms  p95 {:>7.0}ms  ({})",
                substitute.query,
                substitute.substitute,
                substitute.p50_ms,
                substitute.p95_ms,
                if substitute.comparability == crate::symbol_latency::Comparability::Comparable {
                    "comparable timing"
                } else {
                    "NOT comparable"
                },
            );
        }
        let _ = writeln!(out, "      note: {}", substitute.comparability_note);
    }
    let _ = writeln!(out, "gate: {}", if report.ok { "PASS" } else { "FAIL" },);
    out
}

/// Handles `eg audit symbol-latency` (issue #57).
#[allow(clippy::too_many_lines)] // flat orchestration: build fixture, sample 4 warm cells, substitutes, gate, report
pub(crate) fn audit_symbol_latency_cmd(
    corpus_path: &Path,
    warm_samples_override: Option<usize>,
    budget_p95_override: Option<f64>,
    history_commits_override: Option<usize>,
    at_commit_index_override: Option<usize>,
    format: OutputFormat,
) -> Result<()> {
    use crate::symbol_latency::{
        AnswerExpectation, QueryClass, SymbolLatencyReport, answers_deterministic,
        budget_diagnostic, check_answer_handles, collect_environment, determinism_diagnostic,
        summarize_warm_cell,
    };
    use std::collections::BTreeMap;

    let corpus = load_symbol_latency_corpus(
        corpus_path,
        warm_samples_override,
        budget_p95_override,
        history_commits_override,
        at_commit_index_override,
    );

    // Resolve the corpus source directory relative to the manifest's parent so
    // the benchmark is runnable regardless of the working directory.
    let manifest_dir = corpus_path.parent().unwrap_or_else(|| Path::new("."));
    let source_dir = manifest_dir.join(&corpus.source_dir);

    let exe = std::env::current_exe()
        .unwrap_or_else(|error| query_latency_exit("current_exe_error", "", &error.to_string()));

    // The fixture (cold scan, synthetic drift, history replay, ingest,
    // embedding-setup record). Setup only — timed separately, never gated.
    let fixture = build_symbol_latency_fixture(&exe, &corpus, &source_dir);

    let drift_limit = corpus.drift_limit.to_string();
    let scan_store = fixture.scan_store.display().to_string();
    let history_store = fixture.history_store.display().to_string();
    let at_sha = fixture.at_commit_sha.clone();

    // Each measured query as (class, argv, fixture expectation). The argv
    // shape is the exact agent-facing invocation being gated; JSON output so
    // every answer row can be checked for citable handles.
    let query_symbol = corpus.query_symbol.clone();
    let query_file = corpus.query_file.clone();
    let specs: Vec<(QueryClass, Vec<String>, AnswerExpectation)> = vec![
        (
            QueryClass::Symbol,
            vec![
                "query".to_owned(),
                "symbol".to_owned(),
                query_symbol.clone(),
                "--graph".to_owned(),
                scan_store.clone(),
                "--format".to_owned(),
                "json".to_owned(),
            ],
            AnswerExpectation::symbol(&query_symbol),
        ),
        (
            QueryClass::File,
            vec![
                "query".to_owned(),
                "file".to_owned(),
                query_file.clone(),
                "--graph".to_owned(),
                scan_store.clone(),
                "--format".to_owned(),
                "json".to_owned(),
            ],
            AnswerExpectation::file(&query_file),
        ),
        (
            QueryClass::SymbolAtCommit,
            vec![
                "query".to_owned(),
                "symbol".to_owned(),
                query_symbol.clone(),
                "--at".to_owned(),
                at_sha.clone(),
                "--graph".to_owned(),
                history_store,
                "--format".to_owned(),
                "json".to_owned(),
            ],
            AnswerExpectation::symbol_at_commit(&query_symbol, &at_sha),
        ),
        (
            QueryClass::Drift,
            vec![
                "query".to_owned(),
                "drift".to_owned(),
                "--graph".to_owned(),
                scan_store,
                "--limit".to_owned(),
                drift_limit,
                "--format".to_owned(),
                "json".to_owned(),
            ],
            AnswerExpectation::drift(),
        ),
    ];

    let mut queries: BTreeMap<String, crate::symbol_latency::WarmCell> = BTreeMap::new();
    let mut ok = true;
    let mut first_failure: Option<serde_json::Value> = None;
    for (class, argv, expectation) in &specs {
        let cell_name = class.as_str();
        let (samples_ms, outputs) =
            sample_symbol_latency_cell(&exe, argv, corpus.warm_samples, cell_name);

        // Correctness first: latency without correctness does not count.
        let answer = match check_answer_handles(&outputs[0], expectation) {
            Ok(summary) => {
                // Every sampled answer must check out, not just the first.
                let mut failed: Option<String> = None;
                for output in outputs.iter().skip(1) {
                    if let Err(error) = check_answer_handles(output, expectation) {
                        failed = Some(error.to_string());
                        break;
                    }
                }
                if let Some(error) = failed {
                    ok = false;
                    let diagnostic = serde_json::json!({
                        "code": "symbol_latency_answer_check_failed",
                        "query": cell_name,
                        "error": error,
                    });
                    first_failure.get_or_insert(diagnostic);
                    // The cell still needs an answer summary for the report;
                    // the checked first answer is the representative one.
                }
                summary
            }
            Err(error) => {
                ok = false;
                let diagnostic = serde_json::json!({
                    "code": "symbol_latency_answer_check_failed",
                    "query": cell_name,
                    "error": error.to_string(),
                });
                first_failure.get_or_insert(diagnostic);
                // Fail-closed needs a report row even for a bad answer.
                crate::symbol_latency::AnswerSummary {
                    rows: 0,
                    record_ids: Vec::new(),
                    handles: Vec::new(),
                }
            }
        };

        let deterministic = answers_deterministic(&outputs);
        if !deterministic {
            ok = false;
            first_failure.get_or_insert_with(|| determinism_diagnostic(*class, outputs.len()));
        }

        let cell = summarize_warm_cell(
            *class,
            samples_ms,
            answer,
            deterministic,
            corpus.budget_p95_ms,
        )
        .unwrap_or_else(|| query_latency_exit("no_samples", cell_name, "no samples measured"));
        if !cell.within_budget {
            ok = false;
            first_failure.get_or_insert_with(|| {
                budget_diagnostic(*class, cell.p95_ms, corpus.budget_p95_ms)
            });
        }
        queries.insert(cell_name.to_owned(), cell);
    }

    let substitutes = measure_symbol_latency_substitutes(
        &corpus,
        &source_dir,
        &fixture.history_repo,
        &fixture.at_commit_sha,
    );

    let report = SymbolLatencyReport {
        corpus_name: corpus.corpus_name.clone(),
        corpus_version: corpus.corpus_version.clone(),
        query_symbol: corpus.query_symbol.clone(),
        query_file: corpus.query_file.clone(),
        drift_limit: corpus.drift_limit,
        at_commit_index: corpus.at_commit_index,
        at_commit_sha: fixture.at_commit_sha.clone(),
        environment: collect_environment(&corpus.corpus_name, fixture.scan_record_count, "jsonl"),
        setup_phases: fixture.setup_phases,
        queries,
        substitutes,
        warm_samples: corpus.warm_samples,
        budget_p95_ms: corpus.budget_p95_ms,
        history_record_count: fixture.history_record_count,
        ok,
    };

    match format {
        OutputFormat::Json => {
            let output = serde_json::to_string_pretty(&report).unwrap_or_else(|error| {
                query_latency_exit("serialize_error", "", &error.to_string())
            });
            println!("{output}");
        }
        OutputFormat::Text => print!("{}", render_symbol_latency_text(&report)),
    }
    if let Some(diagnostic) = first_failure {
        eprintln!("{diagnostic}");
    } else {
        eprintln!(
            "symbol-latency: all {} query classes within the warm-p95 budget of {:.0}ms",
            QueryClass::all().len(),
            corpus.budget_p95_ms,
        );
    }
    std::process::exit(i32::from(!report.ok));
}

/// Handles `eg audit citations` (issue #65).
pub(crate) fn audit_citations_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    min_code_citation: f64,
    min_log_citation: f64,
    format: OutputFormat,
) -> Result<()> {
    // The gate thresholds are fractions; reject values that would silently disable
    // or invert a gate (e.g. a negative threshold makes 0% completeness pass).
    if !min_code_citation.is_finite() || !(0.0..=1.0).contains(&min_code_citation) {
        eprintln!(
            "{}",
            serde_json::json!({
                "code": "invalid_min_code_citation",
                "value": min_code_citation.to_string(),
                "message": "--min-code-citation must be a finite value in [0.0, 1.0]"
            })
        );
        std::process::exit(2);
    }
    if !min_log_citation.is_finite() || !(0.0..=1.0).contains(&min_log_citation) {
        eprintln!(
            "{}",
            serde_json::json!({
                "code": "invalid_min_log_citation",
                "value": min_log_citation.to_string(),
                "message": "--min-log-citation must be a finite value in [0.0, 1.0]"
            })
        );
        std::process::exit(2);
    }

    // For an embedded store, read from a throwaway read-only copy: opening the
    // embedded engine re-persists index files, and a citation audit must never
    // mutate the store it is only measuring. The guard keeps the copy alive for
    // the duration of every read below.
    // `store_copy` owns the throwaway copy path plus its tempdir guard; keeping
    // it bound here holds the copy alive for every read below.
    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

    let records = match load_query_records(graph, effective_data_dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    let semantic = collect_semantic_input(effective_data_dir, &records);
    // The evidence-freshness lane mirrors `eg query evidence-freshness`, which
    // reads the history-inclusive store view so superseded versions can produce
    // drift/unresolved verdicts. A JSONL graph already carries that history; an
    // embedded store needs the explicit history-inclusive load.
    // Surface a history-load failure rather than silently auditing current-only
    // rows (the public `eg query evidence-freshness --data-dir` uses `?`).
    let freshness_records = effective_data_dir.map(|dir| match load_records_from_db_history(dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let config = crate::citation_audit::AuditConfig {
        min_code_citation,
        min_log_citation,
        semantic,
        freshness_records,
    };
    let report = crate::citation_audit::run_citation_audit(&records, &config);

    let output = match format {
        OutputFormat::Json | OutputFormat::Text => serde_json::to_string_pretty(&report)
            .context("failed to serialize citation audit report")?,
    };
    println!("{output}");
    // `process::exit` bypasses destructors, so the throwaway store copy's `TempDir`
    // guard would leak a full copied store under the temp dir on every `--data-dir`
    // run. Drop it explicitly before exiting (the borrow in `effective_data_dir` is
    // dead after the reads above).
    let exit_code = i32::from(!report.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Handles `eg audit memory-health` (issue #94).
pub(crate) fn audit_memory_health_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    min_provenance_coverage: f64,
    max_dangling_evidence: f64,
    max_unverified: Option<f64>,
    max_current_guidance_contamination: Option<f64>,
    format: OutputFormat,
) -> Result<()> {
    // Validate inputs
    for (name, val) in [
        ("min-provenance-coverage", min_provenance_coverage),
        ("max-dangling-evidence", max_dangling_evidence),
    ] {
        if !val.is_finite() || !(0.0..=1.0).contains(&val) {
            eprintln!(
                "{}",
                serde_json::json!({
                    "code": format!("invalid_{}", name.replace('-', "_")),
                    "value": val.to_string(),
                    "message": format!("--{} must be a finite value in [0.0, 1.0]", name)
                })
            );
            std::process::exit(2);
        }
    }
    for (name, val_opt) in [
        ("max-unverified", max_unverified),
        (
            "max-current-guidance-contamination",
            max_current_guidance_contamination,
        ),
    ] {
        if val_opt.is_some_and(|val| !val.is_finite() || !(0.0..=1.0).contains(&val)) {
            let val = val_opt.unwrap();
            eprintln!(
                "{}",
                serde_json::json!({
                    "code": format!("invalid_{}", name.replace('-', "_")),
                    "value": val.to_string(),
                    "message": format!("--{} must be a finite value in [0.0, 1.0]", name)
                })
            );
            std::process::exit(2);
        }
    }

    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

    let records = match load_query_records_history(graph, effective_data_dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    let config = crate::memory_health::MemoryHealthConfig {
        min_provenance_coverage,
        max_dangling_evidence,
        max_unverified,
        max_current_guidance_contamination,
    };
    let report = crate::memory_health::run_memory_health_audit(&records, &config);

    let output = match format {
        OutputFormat::Json | OutputFormat::Text => serde_json::to_string_pretty(&report)
            .context("failed to serialize memory health report")?,
    };
    println!("{output}");

    let exit_code = i32::from(!report.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Handles `eg audit evidence-links` (issue #217): store-wide cross-domain
/// evidence-link integrity sweep. Exit 0 clean (`ok: true`), 1 broken links
/// found (the full report is still printed to stdout so #72 repair / CI can
/// consume it), 2 usage/load error (both/neither input flag, unreadable/empty
/// store or graph).
pub(crate) fn audit_evidence_links_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    // Read-only: an embedded store is read through a throwaway copy so opening
    // the engine never re-persists index files into the original (AC6). The guard
    // keeps the copy alive for the duration of the read below.
    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

    // History-inclusive load so tombstones and superseded target versions stay
    // VISIBLE — required to tell "absent" from "tombstoned" and to recover a
    // tombstoned code target's path/span. `--graph` JSONL already carries that
    // history; `--data-dir` reads the superseded-inclusive view of the copy. The
    // loader enforces exactly-one-of `--graph`/`--data-dir` (both/neither exit 2).
    let records = match load_query_records_history(graph, effective_data_dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            drop(store_copy);
            std::process::exit(2);
        }
    };

    // A genuinely empty input (an empty/whitespace-only graph or an initialized
    // store holding zero records) is a LOAD error naming the path: a false "clean"
    // verdict on an empty store would be dangerous for a trust gate. Mirrors the
    // `review-coverage` / `evidence-pack` empty-input contract.
    if records.is_empty() {
        let source_path = graph
            .or(data_dir)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        drop(store_copy);
        eprintln!(
            "{}",
            serde_json::json!({
                "code": "empty_evidence_input",
                "path": source_path,
                "message": "evidence input holds zero records; provide a non-empty graph or store",
            })
        );
        std::process::exit(2);
    }

    let report = crate::evidence_link_audit::run_evidence_link_audit(&records);

    let output = match format {
        OutputFormat::Json => serde_json::to_string_pretty(&report)
            .context("failed to serialize evidence-link audit report")?,
        OutputFormat::Text => render_evidence_links_text(&report),
    };
    println!("{output}");
    let exit_code = i32::from(!report.ok);
    drop(store_copy);
    std::process::exit(exit_code);
}

/// Renders an evidence-link audit report as a deterministic human-readable form
/// (issue #217 AC7: counts per domain and per edge label).
fn render_evidence_links_text(
    report: &crate::evidence_link_audit::EvidenceLinkAuditReport,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("ok: {}", report.ok));
    lines.push(format!(
        "checked evidence edges: {}",
        report.checked_edge_count
    ));
    lines.push(format!(
        "broken evidence edges: {}",
        report.broken_edge_count
    ));
    lines.push("by source domain:".to_owned());
    for (domain, count) in &report.by_source_domain {
        lines.push(format!("  {domain}: {count}"));
    }
    lines.push("by edge label:".to_owned());
    for (label, count) in &report.by_edge_label {
        lines.push(format!("  {label}: {count}"));
    }
    lines.push("by case:".to_owned());
    for (case, count) in &report.by_case {
        lines.push(format!("  {case}: {count}"));
    }
    lines.push("broken edges:".to_owned());
    for edge in &report.broken_edges {
        let target = edge
            .target_repo_relative_path
            .as_deref()
            .map_or_else(String::new, |p| format!(" @ {p}"));
        lines.push(format!(
            "  {} [{}] {} -> {} ({}){}",
            edge.source_record_id,
            edge.edge_label,
            edge.representation.as_wire(),
            edge.target_record_id,
            edge.case.as_wire(),
            target
        ));
    }
    lines.push("diagnostics:".to_owned());
    for d in &report.diagnostics {
        let count = d.count.map_or_else(String::new, |c| format!(" count={c}"));
        lines.push(format!("  {}{count}", d.code));
    }
    lines.join("\n")
}

/// Collects embedded-store semantic retrieval leads for the audit, when the
/// `embeddings` feature is built and a `--data-dir` store is supplied.
#[cfg(feature = "embeddings")]
pub(crate) fn collect_semantic_input(
    data_dir: Option<&Path>,
    records: &[GraphRecord],
) -> crate::citation_audit::SemanticInput {
    use crate::citation_audit::{SemanticInput, SemanticRow};

    let Some(dir) = data_dir else {
        return SemanticInput::default();
    };
    let Ok(sink) = EmbeddedAletheiaSink::open_unleased(dir) else {
        return SemanticInput::Disabled {
            reason: "embedded_store_unavailable",
        };
    };
    let Ok(query_vector) = embed_query_text("foo") else {
        return SemanticInput::Disabled {
            reason: "embedding_unavailable",
        };
    };
    let fetch = records.len().max(10);
    let Ok(mut matches) = sink.semantic_search(&query_vector, fetch) else {
        return SemanticInput::Disabled {
            reason: "semantic_index_unavailable",
        };
    };
    matches.retain(|m| {
        m.kind
            .as_deref()
            .is_some_and(|k| k == "File" || k == "Symbol")
    });
    // Measure the DEFAULT `eg query semantic` output, which truncates the
    // code-filtered matches to the default `--limit` (mirrors `query_semantic`).
    matches.truncate(crate::citation_audit::DEFAULT_QUERY_LIMIT);
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let rows = matches
        .iter()
        .map(|m| {
            let (path, span) = by_id.get(m.record_id.as_str()).map_or((None, None), |r| {
                if let GraphRecord::Node {
                    repo_relative_path,
                    span,
                    ..
                } = r
                {
                    (repo_relative_path.clone(), *span)
                } else {
                    (None, None)
                }
            });
            SemanticRow {
                record_id: m.record_id.clone(),
                kind: m.kind.clone().unwrap_or_else(|| "Symbol".to_owned()),
                repo_relative_path: path,
                span,
            }
        })
        .collect();
    SemanticInput::Enabled { rows }
}

/// Without the `embeddings` feature there is no vector index; `semantic` is
/// reported disabled with a stable reason rather than silently dropped.
#[cfg(not(feature = "embeddings"))]
pub(crate) fn collect_semantic_input(
    data_dir: Option<&Path>,
    _records: &[GraphRecord],
) -> crate::citation_audit::SemanticInput {
    use crate::citation_audit::SemanticInput;
    if data_dir.is_some() {
        SemanticInput::Disabled {
            reason: "requires_embeddings_feature",
        }
    } else {
        SemanticInput::default()
    }
}

/// Handles `eg audit memory-evidence-health` (issue #185).
///
/// Store-wide agent-memory evidence health sweep. Exit 0 clean (`ok: true`),
/// 1 findings (dangling links or integrity violations; the full NDJSON report
/// is still printed to stdout so triage tooling can consume it), 2
/// usage/load error (both/neither input flag, unreadable/empty store or
/// graph).
pub(crate) fn audit_memory_evidence_health_cmd(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    format: OutputFormat,
) -> Result<()> {
    // Read-only: an embedded store is read through a throwaway copy so opening
    // the engine never re-persists index files into the original. The guard
    // keeps the copy alive for the duration of the read below.
    let store_copy = data_dir.map(|dir| match readonly_audit_store(dir) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    });
    let effective_data_dir = store_copy.as_ref().map(|(path, _guard)| path.as_path());

    // History-inclusive load so tombstones and superseded versions stay
    // VISIBLE — required to tell "absent" from "tombstoned" and to apply the
    // latest-write-wins liveness rule. `--graph` JSONL already carries that
    // history; `--data-dir` reads the history-inclusive view of the copy. The
    // loader enforces exactly-one-of `--graph`/`--data-dir` (both/neither exit 2).
    let records = match load_query_records_history(graph, effective_data_dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            drop(store_copy);
            std::process::exit(2);
        }
    };

    // A genuinely empty input (an empty/whitespace-only graph or an initialized
    // store holding zero records) is a LOAD error naming the path: a false
    // "clean" verdict on an empty store would be dangerous for a trust gate.
    // Mirrors the `evidence-links` / `review-coverage` empty-input contract.
    if records.is_empty() {
        let source_path = graph
            .or(data_dir)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        drop(store_copy);
        eprintln!(
            "{}",
            serde_json::json!({
                "code": "empty_memory_evidence_input",
                "path": source_path,
                "message": "evidence input holds zero records; provide a non-empty graph or store",
            })
        );
        std::process::exit(2);
    }

    let lines = crate::memory_evidence_health::run_memory_evidence_health_audit(&records);

    match format {
        OutputFormat::Json => {
            for line in &lines {
                println!(
                    "{}",
                    serde_json::to_string(line)
                        .context("failed to serialize memory evidence health line")?
                );
            }
        }
        OutputFormat::Text => {
            print!("{}", render_memory_evidence_health_text(&lines));
        }
    }

    let ok = lines.iter().any(|line| {
        matches!(
            line,
            crate::memory_evidence_health::MemoryEvidenceHealthLine::Summary(summary) if summary.ok
        )
    });
    drop(store_copy);
    std::process::exit(i32::from(!ok));
}

/// Renders a memory-evidence-health report as a deterministic human-readable
/// form (issue #185 AC5): per-source link rows grouped by source record, then
/// integrity violations, then the bucket-count summary.
fn render_memory_evidence_health_text(
    lines: &[crate::memory_evidence_health::MemoryEvidenceHealthLine],
) -> String {
    use crate::memory_evidence_health::MemoryEvidenceHealthLine;
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut current_source = String::new();
    for line in lines {
        match line {
            MemoryEvidenceHealthLine::Link(row) => {
                if row.source_record_id != current_source {
                    current_source.clone_from(&row.source_record_id);
                    let _ = writeln!(out, "{} ({})", row.source_record_id, row.source_kind);
                }
                let detail = match row.bucket {
                    crate::memory_evidence_health::LinkBucket::Drifted => format!(
                        " drift_record={}",
                        row.drift_record_id.as_deref().unwrap_or("?")
                    ),
                    crate::memory_evidence_health::LinkBucket::Dangling => {
                        format!(" tombstoned={}", row.tombstoned.unwrap_or(false))
                    }
                    crate::memory_evidence_health::LinkBucket::ResolvesLive => String::new(),
                };
                let bucket = match row.bucket {
                    crate::memory_evidence_health::LinkBucket::ResolvesLive => "resolves_live",
                    crate::memory_evidence_health::LinkBucket::Drifted => "drifted",
                    crate::memory_evidence_health::LinkBucket::Dangling => "dangling",
                };
                let _ = writeln!(
                    out,
                    "  [{bucket}] {} -> {}{detail}",
                    row.relation, row.target_record_id
                );
            }
            MemoryEvidenceHealthLine::IntegrityViolation(row) => {
                let violation = match row.violation {
                    crate::memory_evidence_health::IntegrityViolationKind::ArrayWithoutEdge => {
                        "array_without_edge"
                    }
                    crate::memory_evidence_health::IntegrityViolationKind::EdgeWithoutArray => {
                        "edge_without_array"
                    }
                };
                let _ = writeln!(
                    out,
                    "integrity violation [{violation}] {} {} -> {} (array: {}, edges: {})",
                    row.source_record_id,
                    row.relation,
                    row.target_record_id,
                    row.array_count,
                    row.edge_count
                );
            }
            MemoryEvidenceHealthLine::Summary(summary) => {
                out.push_str("summary:\n");
                let _ = writeln!(out, "  ok: {}", summary.ok);
                let _ = writeln!(out, "  sources_checked: {}", summary.sources_checked);
                let _ = writeln!(out, "  links_checked: {}", summary.links_checked);
                let _ = writeln!(out, "  resolves_live: {}", summary.resolves_live);
                let _ = writeln!(out, "  drifted: {}", summary.drifted);
                let _ = writeln!(out, "  dangling: {}", summary.dangling);
                let _ = writeln!(
                    out,
                    "  integrity_violations: {}",
                    summary.integrity_violations
                );
            }
        }
    }
    out
}
