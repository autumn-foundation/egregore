use super::*;

/// Routes `eg audit` subcommands.
pub(crate) fn audit_cmd(subcommand: AuditSubcommand) -> Result<()> {
    match subcommand {
        AuditSubcommand::Citations {
            graph,
            data_dir,
            min_code_citation,
            format,
        } => audit_citations_cmd(
            graph.as_deref(),
            data_dir.as_deref(),
            min_code_citation,
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
    }
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

/// Handles `eg audit evidence-pack assemble` (issue #338): builds a
/// control-scoped, time-windowed evidence pack. Exit 0 all verdicts pass, 1 any
/// verdict fails (report still printed), 2 usage/load error.
#[allow(clippy::too_many_arguments)]
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
    let records = match load_query_records(graph, effective_data_dir) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("{error}");
            drop(store_copy);
            std::process::exit(2);
        }
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
        evidence_pack_exit(&serde_json::json!({
            "code": "pack_parse_error",
            "path": path.display().to_string(),
            "message": error.to_string(),
        }))
    });

    let report = verify_pack(&pack);
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
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "control: {} — {}",
        pack.manifest.control_id, pack.manifest.control_title
    ));
    lines.push(format!(
        "window: {} <= t < {}",
        pack.manifest.window.from, pack.manifest.window.to
    ));
    lines.push(format!(
        "catalog: {} ({})",
        pack.manifest.catalog_pin.catalog_id, pack.manifest.catalog_pin.catalog_hash
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
            println!("catalog: {} ({})", parsed.catalog_id, catalog_hash);
            println!(
                "schema_version: {} {} v{}",
                parsed.schema_version.domain,
                parsed.schema_version.kind,
                parsed.schema_version.version
            );
            println!("controls: {}", parsed.controls.len());
            for control in &parsed.controls {
                println!("  {} — {}", control.control_id, control.title);
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
    format: OutputFormat,
) -> Result<()> {
    // The gate threshold is a fraction; reject values that would silently disable
    // or invert the gate (e.g. a negative threshold makes 0% completeness pass).
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
