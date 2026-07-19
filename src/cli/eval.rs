use super::*;

/// Exits 1 with a diagnostic if the top-3 recall threshold is missed.
#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
pub(crate) fn eval_semantic_cmd(
    corpus_path: &Path,
    data_dir: &Path,
    top_k: usize,
    threshold: f64,
    fp_threshold: f64,
) -> Result<()> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };
    use crate::semantic_eval::{
        SearchHit, SemanticRelevanceCorpus, build_report, evaluate_query, format_diagnostic,
        print_report,
    };

    validate_existing_embedded_store(data_dir)?;

    let corpus = SemanticRelevanceCorpus::from_json_file(corpus_path)?;

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    // The shared vector index may also hold agent-memory nodes (issue #91). The
    // code-relevance gate must score only deterministic code hits, exactly like
    // `eg query semantic`, so over-fetch the full pool and filter to File/Symbol
    // before scoring; otherwise embedded memory could occupy top-k slots or
    // count as ambiguous-query false positives and corrupt the gate.
    let total_records = sink
        .read_all_records()
        .map(|r| r.len())
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let mut results = Vec::new();
    for query in &corpus.queries {
        let embed_data = rt
            .block_on(aletheia_embeddings::embed_query(
                &[query.text.as_str()],
                &embedder,
                None,
            ))
            .with_context(|| format!("failed to embed query {}", query.id))?;

        let query_vector = aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1))
            .next()
            .with_context(|| format!("no embedding returned for query {}", query.id))?
            .with_context(|| format!("embedding result not dense for query {}", query.id))?
            .embedding;

        let matches = sink
            .semantic_search(&query_vector, total_records.max(top_k.max(3)))
            .with_context(|| {
                format!(
                    "semantic search failed for query {} — was the store ingested with --embed?",
                    query.id
                )
            })?;

        let hits: Vec<SearchHit> = matches
            .iter()
            .filter(|m| {
                m.kind
                    .as_deref()
                    .is_some_and(|k| k == "File" || k == "Symbol")
            })
            .take(top_k.max(3))
            .map(SearchHit::from)
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        results.push(evaluate_query(query, &hits, fp_threshold as f32));
    }

    let report = build_report(results, threshold);
    print_report(&report, std::io::stdout())?;

    if !report.passed {
        eprintln!("{}", format_diagnostic(&report));
        process::exit(1);
    }

    Ok(())
}

/// Prints a redaction-safe JSON error and exits with the usage/capability code (2).
fn semantic_relevance_exit(code: &str, message: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({ "code": code, "message": message })
    );
    process::exit(2);
}

/// Resolves the relevance floors from the optional `--min-*` overrides,
/// defaulting to the issue #106 floors. A non-finite or out-of-[0,1] override
/// would silently disable or invert the gate, so it exits 2.
fn resolve_relevance_floors(
    min_hit_rate_5: Option<f64>,
    min_mrr: Option<f64>,
) -> crate::semantic_eval::RelevanceFloors {
    let mut floors = crate::semantic_eval::RelevanceFloors::default();
    if let Some(value) = min_hit_rate_5 {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            semantic_relevance_exit(
                "invalid_min_hit_rate_5",
                "--min-hit-rate-5 must be a finite value in [0.0, 1.0]",
            );
        }
        floors.min_hit_rate_at_5 = value;
    }
    if let Some(value) = min_mrr {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            semantic_relevance_exit(
                "invalid_min_mrr",
                "--min-mrr must be a finite value in [0.0, 1.0]",
            );
        }
        floors.min_mrr = value;
    }
    floors
}

/// Prints the relevance report as canonical JSON and exits with its code
/// (0 pass, 1 gate failure, 2 capability/usage). The report is always printed
/// first so gate failures remain debuggable.
fn emit_relevance_report(
    report: &crate::semantic_eval::RelevanceReport,
    format: OutputFormat,
) -> ! {
    // JSON is the canonical, deterministic form; text mirrors it.
    let _ = format;
    println!(
        "{}",
        crate::semantic_eval::render_relevance_report_json(report)
    );
    process::exit(crate::semantic_eval::relevance_exit_code(report));
}

/// Gates semantic-search relevance against a labeled corpus (issue #106).
///
/// Mirrors `eg audit token-cost`'s 0/1/2 exit discipline. The store-backed path
/// requires the `embeddings` feature; with it off, emits an honest
/// capability-unavailable report (exit 2) and never silently passes.
pub(crate) fn audit_semantic_relevance_cmd(
    corpus_path: &Path,
    data_dir: &Path,
    min_hit_rate_5: Option<f64>,
    min_mrr: Option<f64>,
    fp_threshold: f64,
    top_k: usize,
    format: OutputFormat,
) -> Result<()> {
    let floors = resolve_relevance_floors(min_hit_rate_5, min_mrr);

    #[cfg(feature = "embeddings")]
    let report =
        build_embedded_relevance_report(corpus_path, data_dir, floors, fp_threshold, top_k);

    #[cfg(not(feature = "embeddings"))]
    let report = {
        // Honest degradation: the gate cannot run without the embedding model.
        let _ = (corpus_path, data_dir, fp_threshold, top_k);
        crate::semantic_eval::capability_unavailable_report(floors, "requires_embeddings_feature")
    };

    emit_relevance_report(&report, format);
}

/// Runs every corpus query against the embedded store and builds the #106
/// relevance report. Load/environment failures (missing store, embed failure)
/// exit 2, matching the audit usage/capability convention.
#[cfg(feature = "embeddings")]
fn build_embedded_relevance_report(
    corpus_path: &Path,
    data_dir: &Path,
    floors: crate::semantic_eval::RelevanceFloors,
    fp_threshold: f64,
    top_k: usize,
) -> crate::semantic_eval::RelevanceReport {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };
    use crate::semantic_eval::{
        SearchHit, SemanticRelevanceCorpus, build_relevance_report, evaluate_relevance_query,
    };

    if let Err(error) = validate_existing_embedded_store(data_dir) {
        semantic_relevance_exit("store_unavailable", &error.to_string());
    }

    let corpus = SemanticRelevanceCorpus::from_json_file(corpus_path)
        .unwrap_or_else(|error| semantic_relevance_exit("corpus_load_error", &error.to_string()));

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .unwrap_or_else(|error| {
            semantic_relevance_exit("embedding_model_unavailable", &error.to_string())
        });

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .unwrap_or_else(|error| semantic_relevance_exit("store_open_error", &error.to_string()));

    // The shared vector index may also hold agent-memory nodes (issue #91); score
    // only deterministic code hits, exactly like `eg query semantic`, by
    // over-fetching the full pool and filtering to File/Symbol before scoring.
    let total_records = match sink.read_all_records() {
        Ok(records) => records.len(),
        Err(error) => semantic_relevance_exit("store_read_error", &error.to_string()),
    };

    // hit-rate@10 requires keeping at least 10 code hits per query.
    let keep = top_k.max(10);

    let rt = tokio::runtime::Runtime::new()
        .unwrap_or_else(|error| semantic_relevance_exit("runtime_error", &error.to_string()));

    let mut results = Vec::new();
    for query in &corpus.queries {
        let embed_data = rt
            .block_on(aletheia_embeddings::embed_query(
                &[query.text.as_str()],
                &embedder,
                None,
            ))
            .unwrap_or_else(|error| {
                semantic_relevance_exit("embed_query_error", &error.to_string())
            });

        let query_vector =
            match aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1)).next() {
                Some(Ok(dense)) => dense.embedding,
                _ => semantic_relevance_exit(
                    "embed_query_error",
                    &format!("no dense embedding returned for query {}", query.id),
                ),
            };

        let matches = sink
            .semantic_search(&query_vector, total_records.max(keep))
            .unwrap_or_else(|error| {
                semantic_relevance_exit("semantic_search_error", &error.to_string())
            });

        let hits: Vec<SearchHit> = matches
            .iter()
            .filter(|m| {
                m.kind
                    .as_deref()
                    .is_some_and(|k| k == "File" || k == "Symbol")
            })
            .take(keep)
            .map(SearchHit::from)
            .collect();

        #[allow(clippy::cast_possible_truncation)]
        results.push(evaluate_relevance_query(
            &query.id,
            &query.text,
            &query.class,
            &query.expected,
            &hits,
            fp_threshold as f32,
        ));
    }

    build_relevance_report(&results, floors, corpus.source_snapshot)
}

/// Runs the agent-memory recall corpus evaluation against an embedded store
/// seeded with imported memory records (issue #91).
///
/// Reads each natural-language question, embeds it with the local model, runs
/// semantic search, keeps only recallable agent-memory hits (trust-separated
/// from code, provenance-bearing), then evaluates top-1/top-3/MRR against the
/// reviewed expected memory record IDs. Exits 1 with a diagnostic if the top-3
/// recall threshold is missed.
#[cfg(feature = "embeddings")]
pub(crate) fn eval_memory_recall_cmd(
    corpus_path: &Path,
    data_dir: &Path,
    top_k: usize,
    threshold: f64,
    verified_only: bool,
) -> Result<()> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };
    use crate::memory_recall_eval::{
        MemoryHit, MemoryRecallCorpus, build_report, evaluate_query, format_diagnostic,
        print_report,
    };

    validate_existing_embedded_store(data_dir)?;

    let corpus = MemoryRecallCorpus::from_json_file(corpus_path)?;

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let (edges_from, tombstoned) = query::verification_support_indexes(&records);

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let mut results = Vec::new();
    for question in &corpus.questions {
        let embed_data = rt
            .block_on(aletheia_embeddings::embed_query(
                &[question.text.as_str()],
                &embedder,
                None,
            ))
            .with_context(|| format!("failed to embed question {}", question.id))?;

        let query_vector = aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1))
            .next()
            .with_context(|| format!("no embedding returned for question {}", question.id))?
            .with_context(|| format!("embedding result not dense for question {}", question.id))?
            .embedding;

        // Fetch a generous pool, then narrow to recallable memory so `top_k`
        // bounds memory hits rather than the code+memory blend.
        let matches = sink
            .semantic_search(&query_vector, records.len().max(top_k))
            .with_context(|| {
                format!(
                    "semantic search failed for question {} — was the store ingested with --embed?",
                    question.id
                )
            })?;

        // Collect every recallable hit, then apply the canonical score/record-id
        // ordering before truncating to top-k: truncating the raw ANN order first
        // could drop a record that belongs in the canonical top 3 when scores tie
        // (and vary between runs). `evaluate_query` re-applies canonical ordering.
        let mut hits: Vec<MemoryHit> = matches
            .iter()
            .filter(|m| is_recallable_memory(m, &by_id, &edges_from, &tombstoned, verified_only))
            .map(|m| MemoryHit {
                record_id: m.record_id.clone(),
                score: m.score,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
        hits.truncate(top_k.max(3));

        results.push(evaluate_query(question, &hits));
    }

    let report = build_report(results, threshold);
    print_report(&report, std::io::stdout())?;

    if !report.passed {
        eprintln!("{}", format_diagnostic(&report));
        process::exit(1);
    }

    Ok(())
}

/// Run semantic drift calibration evaluation.
#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
pub(crate) fn eval_drift_cmd(corpus_path: &Path, threshold: f64) -> Result<()> {
    use crate::embeddings::{
        CandidateVector, DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME,
        aletheia_embeddings, embedding_candidates, semantic_drift_records,
    };
    use std::collections::HashSet;
    use std::io::Write as _;
    use std::process::Command;

    #[derive(Debug, Clone, serde::Deserialize)]
    struct DriftCalibrationCorpus {
        #[allow(dead_code)]
        pub corpus_version: String,
        #[allow(dead_code)]
        pub description: String,
        pub scenarios: Vec<DriftScenario>,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct DriftScenario {
        pub id: String,
        pub class: String,
        pub file_path: String,
        pub before: String,
        pub after: String,
    }

    #[allow(clippy::struct_excessive_bools)]
    struct ScenarioEvalResult {
        pub scenario_id: String,
        pub class: String,
        pub drift_detected: bool,
        pub max_score: f64,
        pub drift_details: Vec<DriftDetails>,
        pub git_diff_detected: bool,
        pub git_log_s_detected: bool,
        pub rg_detected: bool,
    }

    struct DriftDetails {
        #[allow(dead_code)]
        pub before_commit: String,
        #[allow(dead_code)]
        pub after_commit: String,
        pub file_path: String,
        pub span: Option<SourceSpan>,
        pub score: f64,
        pub selection_threshold: f64,
        pub model_name: String,
    }

    let corpus_text = std::fs::read_to_string(corpus_path)
        .with_context(|| format!("failed to read corpus file {}", corpus_path.display()))?;
    let corpus: DriftCalibrationCorpus = serde_json::from_str(&corpus_text)
        .with_context(|| format!("failed to parse corpus JSON from {}", corpus_path.display()))?;

    // Validate scenario classes immediately
    for scenario in &corpus.scenarios {
        match scenario.class.as_str() {
            "meaning_changed" | "structure_changed_only" | "text_changed_only" | "unchanged" => {}
            other => {
                anyhow::bail!(
                    "Unrecognized or invalid scenario class '{}' in scenario '{}'. Allowed classes are: meaning_changed, structure_changed_only, text_changed_only, unchanged",
                    other,
                    scenario.id
                );
            }
        }
    }

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let mut scanned_scenarios = Vec::new();

    for scenario in &corpus.scenarios {
        let temp_dir = tempfile::tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| -> Result<()> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&temp_path)
                .output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "git command failed: git {:?} in {}. stderr: {}",
                    args,
                    temp_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        };

        run_git(&["init"])?;
        run_git(&["config", "user.name", "Test User"])?;
        run_git(&["config", "user.email", "test@example.com"])?;
        run_git(&["config", "commit.gpgsign", "false"])?;

        let path = std::path::Path::new(&scenario.file_path);
        if path.is_absolute() {
            anyhow::bail!(
                "Corpus scenario file_path must be relative: {}",
                scenario.file_path
            );
        }
        for component in path.components() {
            match component {
                std::path::Component::Prefix(_) => {
                    anyhow::bail!(
                        "Corpus scenario file_path cannot contain a drive/prefix component: {}",
                        scenario.file_path
                    );
                }
                std::path::Component::ParentDir => {
                    anyhow::bail!(
                        "Corpus scenario file_path cannot escape directory via '..': {}",
                        scenario.file_path
                    );
                }
                std::path::Component::RootDir => {
                    anyhow::bail!(
                        "Corpus scenario file_path must be relative: {}",
                        scenario.file_path
                    );
                }
                _ => {}
            }
        }

        let file_path = temp_path.join(path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file_path, &scenario.before)?;

        run_git(&["add", "."])?;

        let run_git_with_env = |args: &[&str], envs: &[(&str, &str)]| -> Result<()> {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(&temp_path);
            for (k, v) in envs {
                cmd.env(k, v);
            }
            let output = cmd.output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "git command failed with envs: git {:?} in {}. stderr: {}",
                    args,
                    temp_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        };

        let before_date = "2026-01-01T00:00:00Z";
        let after_date = "2026-01-02T00:00:00Z";

        run_git_with_env(
            &["commit", "--allow-empty", "-m", "before"],
            &[
                ("GIT_AUTHOR_DATE", before_date),
                ("GIT_COMMITTER_DATE", before_date),
            ],
        )?;

        std::fs::write(&file_path, &scenario.after)?;

        run_git(&["add", "."])?;
        run_git_with_env(
            &["commit", "--allow-empty", "-m", "after"],
            &[
                ("GIT_AUTHOR_DATE", after_date),
                ("GIT_COMMITTER_DATE", after_date),
            ],
        )?;

        let graph = scan_repository_history_with_override(&temp_path, None)?;
        let scenario_records = graph.into_records();

        let before_list: Vec<&str> = scenario.before.split_whitespace().collect();
        let after_list: Vec<&str> = scenario.after.split_whitespace().collect();

        let before_lookup: HashSet<&str> = before_list.iter().copied().collect();
        let after_lookup: HashSet<&str> = after_list.iter().copied().collect();

        let added_token = after_list
            .iter()
            .find(|w| !before_lookup.contains(*w))
            .copied();
        let removed_token = before_list
            .iter()
            .find(|w| !after_lookup.contains(*w))
            .copied();
        let diff_token = added_token.or(removed_token).map(|s| {
            s.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_owned()
        });

        let has_git_diff = !Command::new("git")
            .args(["diff", "--quiet", "HEAD~1", "HEAD"])
            .current_dir(&temp_path)
            .status()?
            .success();

        let has_git_log_s = if let Some(ref token) = diff_token {
            if token.is_empty() {
                false
            } else {
                let out = Command::new("git")
                    .args(["log", &format!("-S{token}")])
                    .current_dir(&temp_path)
                    .output()?;
                out.status.success() && !out.stdout.is_empty()
            }
        } else {
            false
        };

        let has_rg = if let Some(ref token) = diff_token {
            if token.is_empty() {
                false
            } else {
                Command::new("git")
                    .args(["grep", "-q", token])
                    .current_dir(&temp_path)
                    .status()?
                    .success()
            }
        } else {
            false
        };

        scanned_scenarios.push((
            scenario.id.clone(),
            scenario.class.clone(),
            scenario_records,
            has_git_diff,
            has_git_log_s,
            has_rg,
            temp_dir,
        ));
    }

    let mut all_records = Vec::new();
    for (_, _, records, _, _, _, _) in &scanned_scenarios {
        all_records.extend(records.clone());
    }

    let candidates = embedding_candidates(&all_records);
    if candidates.is_empty() {
        anyhow::bail!("no embedding candidates found in scanned graphs");
    }

    let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
    let embed_data = rt
        .block_on(aletheia_embeddings::embed_query(&texts, &embedder, None))
        .context("embedding generation failed")?;

    let dense: Vec<Vec<f32>> = aletheia_embeddings::embed_data_to_dense_iter(embed_data, None)
        .collect::<Result<Vec<_>, _>>()
        .context("embedding result was not dense")?
        .into_iter()
        .map(|d| d.embedding)
        .collect();

    let candidate_vectors: Vec<CandidateVector> = candidates
        .into_iter()
        .zip(dense)
        .map(|(candidate, vector)| CandidateVector { candidate, vector })
        .collect();

    let mut scenario_results = Vec::new();

    for (scenario_id, class, scenario_records, has_git_diff, has_git_log_s, has_rg, _) in
        scanned_scenarios
    {
        let scenario_record_ids: HashSet<&str> =
            scenario_records.iter().map(GraphRecord::id).collect();
        let scenario_commits: HashSet<String> = scenario_records
            .iter()
            .filter_map(|r| {
                if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = r
                {
                    Some(t.git_commit.clone())
                } else if let GraphRecord::Edge {
                    temporal: Some(t), ..
                } = r
                {
                    Some(t.git_commit.clone())
                } else {
                    None
                }
            })
            .collect();

        let scenario_candidate_vectors: Vec<CandidateVector> = candidate_vectors
            .iter()
            .filter(|cv| {
                scenario_record_ids.contains(cv.candidate.record_id.as_str())
                    && cv
                        .candidate
                        .temporal
                        .as_ref()
                        .is_none_or(|t| scenario_commits.contains(&t.git_commit))
            })
            .cloned()
            .collect();

        let scenario_drifts = semantic_drift_records(
            &scenario_candidate_vectors,
            DEFAULT_EMBEDDING_MODEL_NAME,
            threshold,
        );

        let drift_detected = !scenario_drifts.is_empty();
        let mut max_score = 0.0;
        let mut drift_details = Vec::new();

        for record in &scenario_drifts {
            if let GraphRecord::Node {
                id,
                semantic_drift: Some(drift),
                repo_relative_path: drift_path,
                name: drift_name,
                ..
            } = record
            {
                if drift.score > max_score {
                    max_score = drift.score;
                }

                let (resolved_path, _resolved_name, resolved_span) = query::resolve_drift_target(
                    &all_records,
                    id,
                    drift,
                    drift_path.as_deref(),
                    drift_name.as_deref(),
                );

                drift_details.push(DriftDetails {
                    before_commit: drift.before_git_commit.clone(),
                    after_commit: drift.after_git_commit.clone(),
                    file_path: resolved_path.unwrap_or("").to_owned(),
                    span: resolved_span,
                    score: drift.score,
                    selection_threshold: drift.selection_threshold,
                    model_name: drift.embedding_model.name.clone(),
                });
            }
        }

        scenario_results.push(ScenarioEvalResult {
            scenario_id,
            class,
            drift_detected,
            max_score,
            drift_details,
            git_diff_detected: has_git_diff,
            git_log_s_detected: has_git_log_s,
            rg_detected: has_rg,
        });
    }

    let mut tp = 0;
    let mut fp = 0;
    let mut fn_count = 0;
    let mut unchanged_fp = 0;

    for r in &scenario_results {
        if r.class == "meaning_changed" {
            if r.drift_detected {
                tp += 1;
            } else {
                fn_count += 1;
            }
        } else if r.drift_detected {
            fp += 1;
            if r.class == "unchanged" {
                unchanged_fp += 1;
            }
        }
    }

    let precision = if tp + fp > 0 {
        f64::from(tp) / f64::from(tp + fp)
    } else {
        0.0
    };

    let recall = if tp + fn_count > 0 {
        f64::from(tp) / f64::from(tp + fn_count)
    } else {
        0.0
    };

    let precision_pass = precision >= 0.75;
    let recall_pass = recall >= 0.70;
    let unchanged_pass = unchanged_fp == 0;
    let passed = precision_pass && recall_pass && unchanged_pass;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    writeln!(handle, "Semantic Drift Calibration Report")?;
    writeln!(handle, "=================================")?;
    writeln!(handle, "Evaluation threshold: {threshold:.2}")?;
    writeln!(handle, "Total scenarios: {}", scenario_results.len())?;
    writeln!(handle, "Metrics:")?;
    writeln!(
        handle,
        "  Precision: {precision:.4} (pass: {precision_pass})"
    )?;
    writeln!(handle, "  Recall:    {recall:.4} (pass: {recall_pass})")?;
    writeln!(
        handle,
        "  Unchanged false positives: {unchanged_fp} (pass: {unchanged_pass})"
    )?;
    writeln!(
        handle,
        "  Status:    {}",
        if passed { "PASS" } else { "FAIL" }
    )?;
    writeln!(handle)?;

    writeln!(handle, "Scenario Details:")?;
    writeln!(handle, "-----------------")?;
    for r in &scenario_results {
        writeln!(
            handle,
            "  [{}] class={:<22} detected={:<5} max_score={:.4} | Baselines: diff={:<5} log_s={:<5} rg={:<5}",
            r.scenario_id,
            r.class,
            r.drift_detected,
            r.max_score,
            r.git_diff_detected,
            r.git_log_s_detected,
            r.rg_detected
        )?;

        if r.drift_detected {
            for d in &r.drift_details {
                let span_str = d
                    .span
                    .map_or_else(String::new, |s| format!(" {}:{}", s.start_line, s.end_line));
                writeln!(
                    handle,
                    "         - model={} score={:.4} thresh={:.2} path={}{} status=\"drift is a lead, not proof\"",
                    d.model_name, d.score, d.selection_threshold, d.file_path, span_str
                )?;
            }
        }
    }

    if !passed {
        anyhow::bail!("Calibration metrics did not meet the required gates.");
    }

    Ok(())
}
