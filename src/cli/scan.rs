use super::*;
use crate::error::CodegraphError;

/// Scan a repository and write graph JSONL, resolving flags against the
/// checked-in `egregore.toml` (issue #261).
///
/// A config-pinned `scan.transaction_time` governs the whole scan instant —
/// transaction time, coverage-generation stamp, and `producer_started_at` — so
/// two runs sharing the checked-in config produce byte-for-byte identical
/// graph JSONL. With no pin the historical wall-clock path runs, unchanged.
pub(crate) fn scan(repo_path: &Path, out: &Path, args: &ResolvedScanArgs) -> Result<()> {
    warn_on_unconsumed_scope_pins();
    // Exclude the graph output and any in-tree egregore store from the dirty probe
    // (PR #186 E/FF1): a pre-existing graph.jsonl or .egregore data-dir from a
    // previous workflow must not stamp `dirty = true` on the new scan output.
    let exclusions = store_exclusions_including_egregore(repo_path, &[Some(out)]);
    let graph = args
        .transaction_time
        .as_deref()
        .map_or_else(
            || {
                scan_repository_with_exclusions(
                    repo_path,
                    args.repo_id_override.as_deref(),
                    &exclusions,
                )
            },
            |pinned| {
                crate::scan_repository_at_with_exclusions(
                    repo_path,
                    pinned,
                    args.repo_id_override.as_deref(),
                    &exclusions,
                )
            },
        )
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;

    let repo_identity =
        identity::compute_repository_identity(repo_path, args.repo_id_override.as_deref());
    let (repository_id, _) = crate::repository_record_from_identity(&repo_identity);

    let mut records = graph.into_records();
    crate::redaction::redact_code_graph(&mut records, args.raw_literals, &repository_id);
    let graph = Graph::from_records(records);

    print_scan_coverage(&graph);

    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write graph JSONL to {}", out.display()))?;
    Ok(())
}

/// Prints the deterministic human-readable scan-coverage summary to stderr
/// (issue #135), reading it back from the single `ScanCoverage` graph node so
/// the printed summary and the persisted graph fact share one source of truth.
///
/// Only emitted when coverage is complete (the Git-tracked-files walk): the
/// non-Git filesystem-walk fallback has no walked/skipped denominator, so it
/// stays silent rather than print a misleading partial tally — preserving the
/// empty-stderr contract for non-Git fixture scans. The machine-readable form
/// always rides in the JSONL as the `ScanCoverage` node.
fn print_scan_coverage(graph: &Graph) {
    let Some(coverage) = graph
        .records()
        .iter()
        .find_map(crate::ir::GraphRecord::scan_coverage)
    else {
        return;
    };
    if !coverage.coverage_complete {
        return;
    }
    let skipped_total: usize = coverage.skipped_by_extension.values().sum();
    eprintln!(
        "scan coverage: {} files walked, {} indexed, {} skipped",
        coverage.files_walked, coverage.files_indexed, skipped_total
    );
    if !coverage.skipped_by_extension.is_empty() {
        let skipped = coverage
            .skipped_by_extension
            .iter()
            .map(|(ext, count)| {
                let label = if ext.is_empty() { "(no-ext)" } else { ext };
                format!("{label}: {count}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("  skipped by extension: {skipped}");
    }
    eprintln!(
        "indexed languages: {}",
        coverage.indexed_languages.join(", ")
    );
}

/// Prints the single-line machine-readable JSON diagnostic for a CLI-level
/// failure (issues #224 / #256) and exits non-zero without writing partial
/// output.
fn exit_with_diagnostic(code: &'static str, message: &str) -> ! {
    let diag = serde_json::json!({ "code": code, "message": message });
    eprintln!("{}", serde_json::to_string(&diag).unwrap_or_default());
    std::process::exit(2);
}

pub(crate) fn scan_history(
    repo_path: &Path,
    out: &Path,
    args: &ResolvedScanArgs,
    resume_from: Option<&Path>,
) -> Result<()> {
    warn_on_unconsumed_scope_pins();

    // Issue #256: validate the commit window before any repository or output
    // work, so a conflicting or unparseable window fails with the single-line
    // JSON diagnostic and never a partial output.
    let window = match HistoryWindow::from_flags(
        args.max_commits.as_deref(),
        args.since.as_deref(),
        args.from_rev.clone(),
        args.to_rev.clone(),
    ) {
        Ok(window) => window,
        Err(CodegraphError::HistoryWindow { code, message }) => {
            exit_with_diagnostic(code, &message)
        }
        Err(err) => return Err(err.into()),
    };

    // Issue #224: resuming from a frontier and bounding the replay with a
    // window are mutually exclusive — a bounded store is not a valid resume
    // frontier. Fail before any repository or output work.
    if resume_from.is_some() && !matches!(window, HistoryWindow::Full) {
        exit_with_diagnostic(
            "resume_with_window",
            "--resume-from cannot be combined with a commit window (--max-commits, --since, --from, or --to); resume from a full-history frontier",
        );
    }

    // AC5: Verify git is available in PATH.
    let git_available = std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !git_available {
        let diag = serde_json::json!({
            "code": "git_unavailable",
            "message": "git command not found in PATH"
        });
        eprintln!("{}", serde_json::to_string(&diag).unwrap_or_default());
        std::process::exit(2);
    }

    // AC5: Verify the path is a git repository.
    let is_git_repo = std::process::Command::new("git")
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "--git-dir"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !is_git_repo {
        let diag = serde_json::json!({
            "code": "not_a_git_repository",
            "message": format!("path is not a git repository: {}", repo_path.display())
        });
        eprintln!("{}", serde_json::to_string(&diag).unwrap_or_default());
        std::process::exit(2);
    }

    // AC5: Verify the git history is readable (has at least one commit).
    let git_history_readable = std::process::Command::new("git")
        .args(["-c", "core.excludesFile="])
        .arg("-C")
        .arg(repo_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["log", "-1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !git_history_readable {
        let diag = serde_json::json!({
            "code": "git_history_unreadable",
            "message": "git history is not readable (e.g. repository has no commits)"
        });
        eprintln!("{}", serde_json::to_string(&diag).unwrap_or_default());
        std::process::exit(2);
    }

    // History replay reads only committed Git objects, so the stamped snapshot is
    // always `dirty=false` (committed HEAD state); a pre-existing in-tree output or
    // companion store cannot affect it, and no dirty-probe exclusions are needed
    // (TT1 supersedes the earlier CC1/GG1 exclusion machinery).
    //
    // Issue #256: a window that resolves to no commits (or an unresolvable
    // revision) fails with the single-line JSON diagnostic and a non-zero
    // exit; no partial output is written.
    //
    // Issue #224: `--resume-from` replays only the commits that landed after
    // the frontier's tip and merges them with the frontier's records.
    if let Some(frontier_path) = resume_from {
        return scan_history_resumed(repo_path, out, args, frontier_path);
    }

    let graph = match scan_repository_history_with_window(
        repo_path,
        args.repo_id_override.as_deref(),
        &window,
    ) {
        Ok(graph) => graph,
        Err(CodegraphError::HistoryWindow { code, message }) => {
            exit_with_diagnostic(code, &message)
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to scan Git history for {}", repo_path.display())
            });
        }
    };

    let repo_identity =
        identity::compute_repository_identity(repo_path, args.repo_id_override.as_deref());
    let (repository_id, _) = crate::repository_record_from_identity(&repo_identity);

    let mut records = graph.into_records();
    crate::redaction::redact_code_graph(&mut records, args.raw_literals, &repository_id);
    let graph = Graph::from_records(records);

    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize history graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write history graph JSONL to {}", out.display()))?;
    Ok(())
}

/// Incremental `scan-history --resume-from <frontier>` (issue #224).
///
/// Reads the history-replay tip recorded in the frontier, replays only the
/// commits that landed after it, and merges them with the frontier's records
/// so the output is byte-identical to a fresh full replay. Prints a
/// single-line JSON `{"processed":N,"skipped":M}` report to stderr, where
/// `processed` counts the commits read from Git in this run and `skipped`
/// counts the commits the frontier already covered.
///
/// Every failure mode prints the single-line JSON diagnostic and exits 2
/// without writing partial output: `invalid_frontier` when the frontier file
/// cannot be read or parsed as JSONL, `no_resume_point` when the frontier
/// carries no tip for this repository (a windowed replay or a plain `scan`
/// graph), `history_rewrite_detected` when the stored tip is no longer an
/// ancestor of HEAD (recovery: a full replay), and
/// `repository_identity_mismatch` when the frontier belongs to another
/// repository. A zero-new-commit resume writes nothing: `--out` is left
/// untouched while the report still prints.
fn scan_history_resumed(
    repo_path: &Path,
    out: &Path,
    args: &ResolvedScanArgs,
    frontier_path: &Path,
) -> Result<()> {
    let repo_identity =
        identity::compute_repository_identity(repo_path, args.repo_id_override.as_deref());
    let (repository_id, _) = crate::repository_record_from_identity(&repo_identity);

    // Read the whole frontier before any output work: `--out` may name the
    // frontier itself (a no-op resume must leave it byte-identical), and no
    // failure below may leave a partial output behind. An unreadable or
    // unparsable frontier is the machine-readable `invalid_frontier`
    // diagnostic, never a stack trace.
    let frontier = match fs::read_to_string(frontier_path) {
        Ok(frontier) => frontier,
        Err(err) => exit_with_diagnostic(
            "invalid_frontier",
            &format!(
                "cannot read resume frontier {}: {err}",
                frontier_path.display()
            ),
        ),
    };
    let prior_records = match crate::adapters::records_from_jsonl(&frontier) {
        Ok(records) => records,
        Err(err) => exit_with_diagnostic(
            "invalid_frontier",
            &format!(
                "cannot parse resume frontier {} as JSONL: {err}",
                frontier_path.display()
            ),
        ),
    };

    let Some(resume) = crate::history_resume_point(&prior_records, &repository_id) else {
        // A frontier that carries tips for *other* repositories is a foreign
        // frontier, not a tipless one: report the identity mismatch rather
        // than a missing resume point.
        let tip_repos = crate::history_replay_tip_repository_ids(&prior_records);
        if !tip_repos.is_empty() && !tip_repos.contains(&repository_id) {
            exit_with_diagnostic(
                "repository_identity_mismatch",
                &format!(
                    "resume frontier {} belongs to repository {}; current repository is {repository_id}; resume points never cross repository identities",
                    frontier_path.display(),
                    tip_repos.join(", "),
                ),
            );
        }
        exit_with_diagnostic(
            "no_resume_point",
            &format!(
                "resume frontier {} has no history-replay tip for this repository; resume from a full scan-history output",
                frontier_path.display(),
            ),
        );
    };

    let outcome = match crate::scan_repository_history_resumed(
        repo_path,
        args.repo_id_override.as_deref(),
        resume,
    ) {
        Ok(outcome) => outcome,
        Err(CodegraphError::HistoryResume { code, message }) => {
            exit_with_diagnostic(code, &message);
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to resume Git history scan for {}",
                    repo_path.display()
                )
            });
        }
    };

    // Redact the merged record set exactly as the full path does: the prior
    // records were already redacted when the frontier was written (redaction
    // is idempotent over them), and the new commits' records need it now.
    let mut records = outcome.graph.into_records();
    crate::redaction::redact_code_graph(&mut records, args.raw_literals, &repository_id);
    let graph = Graph::from_records(records);

    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize history graph JSONL")?;
    // A zero-new-commit resume is a no-op: the frontier already holds the
    // converged record set, so `--out` is left untouched — never truncated or
    // rewritten, preserving its mtime — while the processed/skipped report
    // below still prints.
    if outcome.processed > 0 {
        fs::write(out, jsonl)
            .with_context(|| format!("failed to write history graph JSONL to {}", out.display()))?;
    }

    let report = serde_json::json!({
        "processed": outcome.processed,
        "skipped": outcome.skipped,
    });
    eprintln!("{}", serde_json::to_string(&report).unwrap_or_default());
    Ok(())
}

/// Machine-readable report emitted by `eg refresh`.
///
/// All counts are integers; file lists are sorted repository-relative paths.
/// `freshness_after_refresh` reports the verdict a follow-up `eg freshness`
/// would give (the write-side counterpart to the read-only signal in issue #82).
#[cfg(feature = "embedded-aletheiadb")]
#[derive(Debug, Serialize)]
pub(crate) struct RefreshReport {
    /// Repository-relative paths of files that were re-extracted in this refresh.
    rebuilt_files: Vec<String>,
    /// Number of rebuilt files.
    rebuilt_count: usize,
    /// Repository-relative paths of files whose cached records were reused unchanged.
    reused_files: Vec<String>,
    /// Number of reused files.
    reused_count: usize,
    /// Repository-relative paths of files that were tombstoned because they no longer exist.
    tombstoned_files: Vec<String>,
    /// Number of tombstoned files.
    tombstoned_count: usize,
    /// Total records submitted to the ingest adapter.
    ingest_attempted: usize,
    /// Records successfully written.
    ingest_succeeded: usize,
    /// Records that failed to write.
    ingest_failed: usize,
    /// Semantic embedding state after this refresh.
    ///
    /// `"not_requested"` — `--embed` was not passed; structural records are current
    /// but any prior semantic embeddings for rebuilt/tombstoned nodes may be stale.
    /// Re-run `eg ingest --adapter embedded --embed` to rebuild the full semantic index.
    ///
    /// `"refreshed"` — `--embed` was passed; embeddings for all changed nodes were
    /// regenerated as part of this refresh.
    embed_status: String,
    /// Freshness of the store with respect to the working tree after this refresh.
    ///
    /// The verdict `eg freshness --data-dir` would report for the rebuilt store:
    /// `"fresh"` for a clean tree at the stamped HEAD, or `"stale_dirty"` when the
    /// refresh captured uncommitted `.rs` edits (the store reflects an uncommitted
    /// state). This is the write counterpart to the read-only staleness signal
    /// (issue #82) and stays consistent with a follow-up freshness check (OO1).
    freshness_after_refresh: String,
}

/// Handles `eg refresh <repo_path> --data-dir <dir> [--cache <path>] [--format json|text]`.
///
/// Performs an incremental scan (BLAKE3 file-hash cache) and ingests only the
/// changed/added/removed records into the embedded store.  Non-codegraph records
/// (agent-memory, project, artifact, verification) are never touched.
#[cfg(feature = "embedded-aletheiadb")]
#[allow(clippy::too_many_lines)]
pub(crate) fn scan_refresh_cmd(
    repo_path: &Path,
    data_dir: &Path,
    cache: Option<&Path>,
    format: OutputFormat,
    #[cfg(feature = "embeddings")] embed: bool,
    #[cfg(feature = "embeddings")] embed_model: Option<String>,
    raw_literals: bool,
) -> Result<()> {
    // Resolve the checked-in config (issue #261): fail fast on a malformed
    // file, warn on scope pins the refresh does not consume, and let the
    // config pin the redaction behavior and repository identity. The data dir
    // itself was already resolved (flag > config > default) by the dispatcher.
    warn_on_unconsumed_scope_pins();
    let loaded = cli_project_config();
    let config = loaded.as_ref().map(|loaded| &loaded.config);
    let (raw_literals, _) = crate::project_config::resolve_flag(
        raw_literals,
        config.and_then(|c| c.redaction.raw_literals),
    );
    let repo_id_override = config.and_then(|c| c.repo_id_override.as_deref());

    // AC9: The embedded store must already exist before we can refresh it.
    if !data_dir.exists() {
        eprintln!(
            r#"{{"code":"no_prior_scan","message":"embedded store not found at {}; run `eg scan <repo> --out g.jsonl && eg ingest g.jsonl --adapter embedded --data-dir {}` first"}}"#,
            data_dir.display(),
            data_dir.display()
        );
        process::exit(2);
    }

    // Derive effective cache path: defaults to <data_dir>/codegraph-cache.json.
    let default_cache = data_dir.join("codegraph-cache.json");
    let cache_path = cache.unwrap_or(&default_cache);

    // AC9: If the cache already has a repository_id, it must match the current
    // repository — otherwise the cache was built for a different repo and a full
    // rebuild is required.
    if cache_path.exists() {
        let cache_raw = fs::read_to_string(cache_path)
            .with_context(|| format!("failed to read cache {}", cache_path.display()))?;
        if let Ok(cache_json) = serde_json::from_str::<serde_json::Value>(&cache_raw)
            && let Some(cached_repo_id) = cache_json
                .get("repository_id")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
        {
            let current_identity =
                crate::identity::compute_repository_identity(repo_path, repo_id_override);
            if current_identity.id != cached_repo_id {
                eprintln!(
                    r#"{{"code":"repository_identity_mismatch","cached_id":"{}","current_id":"{}","message":"cache at {} was built for a different repository; delete it and re-run from `eg scan`"}}"#,
                    cached_repo_id,
                    current_identity.id,
                    cache_path.display()
                );
                process::exit(2);
            }
        }
    }

    // Perform the incremental scan (reads cache, hashes files, rebuilds changed ones).
    // Exclude the data-dir, the cache file, and any in-tree `.egregore*` companion
    // store from the dirty probe (PR #186 A/MM1): all are refresh/store artifacts;
    // counting any as dirty would stamp `dirty = true` on the snapshot and make a
    // follow-up `eg freshness --data-dir` report `stale_dirty` with no source change.
    let snapshot_exclusions =
        store_exclusions_including_egregore(repo_path, &[Some(data_dir), Some(cache_path)]);
    let scan = scan_repository_incremental_excluding(
        repo_path,
        cache_path,
        &snapshot_exclusions,
        raw_literals,
    )
    .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;

    #[cfg_attr(not(feature = "embeddings"), allow(unused_mut))]
    let mut records = scan.graph.records().to_vec();

    // Open the embedded store and ingest the incremental graph.
    #[cfg(feature = "embeddings")]
    let mut sink = if embed {
        // Resolve the embedding model (issue #261): `--embed-model` >
        // `[embeddings].model` > built-in default. The resolved name is what
        // the embedder loads AND what the refreshed identity records; the
        // write-time conflict refusal below still prevents blending two
        // vector spaces when the resolved model differs from the index's.
        let (embed_model, _) = resolve_embed_model(embed_model);
        let (vectors, dimensions, model) = generate_embeddings(&records, &embed_model)?;
        let sink = EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dimensions)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        // Refuse before writing anything when the index was built by a different
        // model (issue #104). This is the path an operator reaches for after an
        // `eg` upgrade, and it must not quietly blend two vector spaces.
        refuse_conflicting_index_identity(&sink, &model)?;
        // A refresh maintains the same vector index a full `--embed` ingest
        // creates, so it maintains the same identity record — otherwise a
        // refreshed store would look "unverifiable" at query time.
        records.push(crate::embeddings::embedding_index_identity_record(&model));
        sink
    } else {
        EmbeddedAletheiaSink::open(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?
    };
    #[cfg(not(feature = "embeddings"))]
    let mut sink = EmbeddedAletheiaSink::open(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let ingest_report = ingest_records(&records, &mut sink);
    if !ingest_report.is_success() {
        // Delete the cache so the next run does a clean rebuild; avoids a cache-ahead-of-store skew
        // where the cache reflects file hashes the store never ingested.
        let _ = fs::remove_file(cache_path);
        for failure in &ingest_report.failures {
            eprintln!("{}: {}", failure.record_id, failure.message);
        }
        anyhow::bail!("refresh failed for {} records", ingest_report.failed);
    }
    sink.persist_indexes()
        .with_context(|| format!("failed to persist embedded store {}", data_dir.display()))?;

    // Determine semantic embedding state for the report (AC8).
    #[cfg(feature = "embeddings")]
    let embed_status = if embed {
        "refreshed".to_owned()
    } else {
        "not_requested".to_owned()
    };
    #[cfg(not(feature = "embeddings"))]
    let embed_status = "not_requested".to_owned();

    let rebuilt_files = scan.rebuilt_files;
    let reused_files = scan.reused_files;
    let tombstoned_files = scan.tombstoned_files;
    let rebuilt_count = rebuilt_files.len();
    let reused_count = reused_files.len();
    let tombstoned_count = tombstoned_files.len();

    // Report the verdict `eg freshness --data-dir` would compute, not an
    // unconditional "fresh" (OO1 / PR #186 follow-up). When the working tree had
    // uncommitted `.rs` edits the refreshed snapshot is stamped `dirty`, so the
    // store is `stale_dirty` even immediately after rebuild — exactly as a full
    // scan of a dirty tree behaves. The snapshot was just computed from the
    // current tree, so classifying it against itself yields the same verdict a
    // follow-up freshness check would, without re-probing Git.
    let refresh_identity = identity::compute_repository_identity(repo_path, repo_id_override);
    let freshness_after_refresh = freshness::stored_snapshot(&records, &refresh_identity.id)
        .map_or(Freshness::Unknown, |snapshot| {
            freshness::classify(Some(snapshot), &snapshot.head, snapshot.dirty)
        })
        .code()
        .to_owned();

    let refresh_report = RefreshReport {
        rebuilt_files,
        rebuilt_count,
        reused_files,
        reused_count,
        tombstoned_files,
        tombstoned_count,
        ingest_attempted: ingest_report.attempted,
        ingest_succeeded: ingest_report.succeeded,
        ingest_failed: ingest_report.failed,
        embed_status,
        freshness_after_refresh,
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&refresh_report)
                    .context("failed to serialize refresh report")?
            );
        }
        OutputFormat::Text => {
            println!("rebuilt: {}", refresh_report.rebuilt_count);
            println!("reused: {}", refresh_report.reused_count);
            println!("tombstoned: {}", refresh_report.tombstoned_count);
            println!("attempted: {}", refresh_report.ingest_attempted);
            println!("succeeded: {}", refresh_report.ingest_succeeded);
            println!("failed: {}", refresh_report.ingest_failed);
            println!("embed_status: {}", refresh_report.embed_status);
            println!(
                "freshness_after_refresh: {}",
                refresh_report.freshness_after_refresh
            );
            for f in &refresh_report.rebuilt_files {
                println!("rebuilt_file: {f}");
            }
            for f in &refresh_report.reused_files {
                println!("reused_file: {f}");
            }
            for f in &refresh_report.tombstoned_files {
                println!("tombstoned_file: {f}");
            }
        }
    }

    Ok(())
}
