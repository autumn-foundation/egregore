use super::*;

pub(crate) fn scan(
    repo_path: &Path,
    out: &Path,
    repo_id_override: Option<&str>,
    raw_literals: bool,
) -> Result<()> {
    // Exclude the graph output and any in-tree egregore store from the dirty probe
    // (PR #186 E/FF1): a pre-existing graph.jsonl or .egregore data-dir from a
    // previous workflow must not stamp `dirty = true` on the new scan output.
    let exclusions = store_exclusions_including_egregore(repo_path, &[Some(out)]);
    let graph = scan_repository_with_exclusions(repo_path, repo_id_override, &exclusions)
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;

    let repo_identity = identity::compute_repository_identity(repo_path, repo_id_override);
    let (repository_id, _) = crate::repository_record_from_identity(&repo_identity);

    let mut records = graph.into_records();
    crate::redaction::redact_code_graph(&mut records, raw_literals, &repository_id);
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

pub(crate) fn scan_history(
    repo_path: &Path,
    out: &Path,
    repo_id_override: Option<&str>,
    raw_literals: bool,
) -> Result<()> {
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
    let graph = scan_repository_history_with_override(repo_path, repo_id_override)
        .with_context(|| format!("failed to scan Git history for {}", repo_path.display()))?;

    let repo_identity = identity::compute_repository_identity(repo_path, repo_id_override);
    let (repository_id, _) = crate::repository_record_from_identity(&repo_identity);

    let mut records = graph.into_records();
    crate::redaction::redact_code_graph(&mut records, raw_literals, &repository_id);
    let graph = Graph::from_records(records);

    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize history graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write history graph JSONL to {}", out.display()))?;
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
    raw_literals: bool,
) -> Result<()> {
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
            let current_identity = crate::identity::compute_repository_identity(repo_path, None);
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

    let records = scan.graph.records().to_vec();

    // Open the embedded store and ingest the incremental graph.
    #[cfg(feature = "embeddings")]
    let mut sink = if embed {
        let (vectors, dimensions) = generate_embeddings(&records)?;
        EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dimensions)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?
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
    let refresh_identity = identity::compute_repository_identity(repo_path, None);
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
