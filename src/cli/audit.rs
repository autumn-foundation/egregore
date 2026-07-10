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
    }
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
