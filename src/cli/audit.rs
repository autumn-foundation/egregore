use super::*;

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
    }
}

/// Prints a redaction-safe JSON error and exits with the usage/load code (2).
pub(crate) fn review_coverage_exit(value: &serde_json::Value) -> ! {
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
        review_coverage_exit(&serde_json::json!({
            "code": "invalid_min_proven_ratio",
            "value": min_proven_ratio.to_string(),
            "message": "--min-proven-ratio must be a finite value in [0.0, 1.0]",
        }));
    }
    let limit = limit.unwrap_or(CRITERIA_COVERAGE_DEFAULT_LIMIT);
    if limit == 0 || limit > CRITERIA_COVERAGE_MAX_LIMIT {
        review_coverage_exit(&serde_json::json!({
            "code": "invalid_limit",
            "value": limit.to_string(),
            "message": format!("--limit must be in 1..={CRITERIA_COVERAGE_MAX_LIMIT}"),
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
        review_coverage_exit(&serde_json::json!({
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
        lines.push(format!(
            "  {} task={} bucket={} closing={}",
            row.record_id,
            row.parent_task_id.as_deref().unwrap_or("-"),
            row.bucket,
            if row.closing_links.is_empty() {
                "-".to_owned()
            } else {
                row.closing_links
                    .iter()
                    .map(|l| format!("{}[{}]", l.handle, l.resolution))
                    .collect::<Vec<_>>()
                    .join(",")
            }
        ));
    }
    lines.push(format!("criteria: {}", report.criteria.len()));
    for row in &report.criteria {
        lines.push(format!(
            "  {} task={} bucket={}",
            row.record_id,
            row.parent_task_id.as_deref().unwrap_or("-"),
            row.bucket
        ));
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
    }
    lines.push(format!("disclaimer: {}", report.disclaimer));
    lines.join("\n")
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
