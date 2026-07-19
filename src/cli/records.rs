use super::*;

pub(crate) fn load_query_records(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path),
        (None, Some(dir)) => load_records_from_db(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    }
}

pub(crate) fn load_records_from_jsonl(graph: &Path) -> Result<Vec<GraphRecord>> {
    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    crate::adapters::records_from_jsonl(&jsonl)
        .map_err(|e| anyhow::anyhow!("failed to parse graph JSONL: {e}"))
}

/// Loads query records for a targeted lane, using the sidecar index (issue #447)
/// to hydrate only the [`Selector`]'s closure when a valid `<graph>.idx` exists.
///
/// This is the ONE loader hook for the graph sidecar index. It is a pure
/// access-path optimization: the records it returns for a non-`Whole` selector
/// are a byte-identical superset of the records the migrated lane needs, so the
/// lane's output is unchanged from the cold path.
///
/// * `--data-dir`: unchanged embedded load (the embedded store is out of scope
///   for #447); the selector is ignored.
/// * `--graph` + [`Selector::Whole`]: the existing cold scan, byte-identical to
///   today (no index consulted).
/// * `--graph` + a non-`Whole` selector: load and validate `<graph>.idx`; on a
///   valid index hydrate the closure by seek, otherwise transparently fall back
///   to the cold scan. This path NEVER fails because of the index and NEVER
///   writes it.
pub(crate) fn load_records_selected(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    selector: &crate::graph_index::Selector,
) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl_selected(path, selector),
        (None, Some(dir)) => load_records_from_db(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    }
}

/// `--graph` half of [`load_records_selected`]: hydrate the selector's closure
/// through a valid sidecar index, else cold-scan the whole file.
pub(crate) fn load_records_from_jsonl_selected(
    graph: &Path,
    selector: &crate::graph_index::Selector,
) -> Result<Vec<GraphRecord>> {
    use crate::graph_index::{GraphIndex, Selector};

    if matches!(selector, Selector::Whole) {
        return load_records_from_jsonl(graph);
    }
    // Try the index; ANY failure (absent, stale, corrupt, version mismatch, or a
    // hydration I/O error) transparently degrades to the cold scan.
    let Ok(index) = GraphIndex::load_for(graph) else {
        return load_records_from_jsonl(graph);
    };
    // Compose with #457: over a history / corpus store the default HEAD-anchor
    // gate (`query::non_head_current_record_ids`) and the other history-view
    // lanes need GLOBAL commit topology and every version of every record to
    // decide what is current at HEAD — a set a targeted closure cannot soundly
    // supply. A closure would silently omit off-HEAD versions (or the
    // `Repository` snapshot / commit topology the gate reads), diverging from the
    // cold answer. So a history store falls back to the cold whole-file scan; a
    // plain current-tree `scan` graph (no temporal records) keeps the #447 fast
    // path, where head-anchoring drops nothing and the closure is byte-identical.
    if index.body.has_temporal_history {
        return load_records_from_jsonl(graph);
    }
    match index.hydrate(graph, selector) {
        Ok(Some(records)) => Ok(records),
        Ok(None) | Err(_) => load_records_from_jsonl(graph),
    }
}

#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn validate_existing_embedded_store(data_dir: &Path) -> Result<()> {
    match fs::read_dir(data_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!(
                "error: embedded store not found at {} - \
                 run `eg ingest --adapter embedded --data-dir <path>` first",
                data_dir.display()
            );
        }
        Ok(mut entries) => {
            if entries.next().is_none() {
                anyhow::bail!(
                    "error: embedded store at {} is empty - \
                     run `eg ingest --adapter embedded --data-dir <path>` first",
                    data_dir.display()
                );
            }
        }
        Err(_) => {}
    }
    Ok(())
}

pub(crate) fn load_records_from_db(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        sink.read_all_records()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Reads the tombstoned-record → attribution-parent map from an embedded
/// store (issue #234 `--repo` scoping).
///
/// The current-state read suppresses tombstoned edge and node records, so a
/// scoped producer-drift run cannot resolve a deletion tombstone's
/// `deleted_id` from the record slice alone; this recovers the edge sources
/// and containment parents the append-only store still holds. Callers
/// honouring the read-only guarantee must pass the same throwaway store copy
/// they load records from.
pub(crate) fn load_tombstoned_record_parents_from_db(
    data_dir: &Path,
) -> Result<BTreeMap<String, String>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        sink.tombstoned_record_parents()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Loads records from an embedded `--data-dir` store without mutating it (issue #82).
///
/// The embedded engine re-persists its index files on open, so a freshness check
/// that opened the live store directly would modify it — violating the read-only
/// guarantee. This copies the store to a throwaway temporary directory and reads
/// the copy, leaving the original byte-for-byte untouched.
/// Returns a read-only working location for embedded-store audit reads plus the
/// tempdir guard that must outlive those reads.
///
/// With the embedded feature this is a throwaway copy of the store, so the audit
/// never re-persists or otherwise mutates the original. Without the feature the
/// path is returned unchanged (the subsequent read bails on the missing feature).
pub(crate) fn readonly_audit_store(
    data_dir: &Path,
) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only audit",
                data_dir.display()
            )
        })?;
        Ok((copy_root, Some(temp)))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        Ok((data_dir.to_path_buf(), None))
    }
}

pub(crate) fn load_records_from_data_dir_readonly(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only inspection",
                data_dir.display()
            )
        })?;
        load_records_from_db(&copy_root)
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Loads records from an embedded store, additionally retaining every
/// superseded non-temporal log observation (issue #363).
///
/// Mirrors [`load_records_from_db`] but calls
/// [`EmbeddedAletheiaSink::read_all_records_log_retained`], so the `--data-dir`
/// log-signature coalescers (#326 `log-deltas`, #324 `error-context`) see the
/// duplicate `ErrorSignature` / `LogOccurrenceBucket` versions that differing
/// `scan-logs` ingests append, exactly as the `--graph` path does over
/// concatenated JSONL. Non-log kinds keep their single current-state record.
pub(crate) fn load_records_from_db_log_retained(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        sink.read_all_records_log_retained()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Log-retained record load that leaves the store byte-for-byte untouched (issue
/// #363), for the strictly read-only `log-deltas` / `error-context` lanes.
///
/// Opening the embedded engine in place re-persists its on-disk index files, so
/// this copies the store to a throwaway temporary directory and reads the
/// log-retained view from the copy (mirrors [`load_records_from_data_dir_readonly`]).
pub(crate) fn load_records_from_data_dir_log_retained_readonly(
    data_dir: &Path,
) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only inspection",
                data_dir.display()
            )
        })?;
        load_records_from_db_log_retained(&copy_root)
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Recursively copies the regular files and directories under `src` into `dst`.
///
/// Symlinks and other non-regular entries are skipped; this is used only to make
/// a read-only working copy of an embedded store directory.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Loads query records for a transaction-time query (issue #66).
///
/// The `--graph` JSONL path already preserves every written line, so it is used
/// unchanged. The embedded `--data-dir` path additionally surfaces superseded
/// non-temporal versions so a prior store view can be reconstructed.
pub(crate) fn load_query_records_history(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path),
        (None, Some(dir)) => load_records_from_db_history(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    }
}

pub(crate) fn load_records_from_db_history(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        sink.read_all_records_including_superseded()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Loads the history-inclusive view from a store without mutating it (issue #85).
///
/// `eg evidence_freshness` is strictly read-only, but opening the embedded engine
/// re-persists its on-disk index files. This copies the store to a throwaway
/// temporary directory and reads the history-inclusive view from the copy, leaving
/// the original byte-for-byte untouched (mirrors `load_records_from_data_dir_readonly`).
pub(crate) fn load_records_from_db_history_readonly(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only inspection",
                data_dir.display()
            )
        })?;
        let sink = EmbeddedAletheiaSink::open_unleased(&copy_root)
            .with_context(|| format!("failed to open embedded store {}", copy_root.display()))?;
        sink.read_all_records_including_superseded()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// Like [`load_records_from_db_history`], but additionally collapses
/// enrichment-only rewrites of a non-temporal log observation (issue #363).
///
/// Mirrors [`load_records_from_db_history`] but calls
/// [`EmbeddedAletheiaSink::read_all_records_including_superseded_log_retained`],
/// so the `--at`/`--as-of` `error-context` (#324) lane sees enrichment-only
/// `ErrorSignature` rewrites collapsed to a single observation (never
/// double-counted) while every non-log superseded/temporal version stays intact
/// for valid-time reconstruction.
pub(crate) fn load_records_from_db_history_log_retained(
    data_dir: &Path,
) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
            .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
        sink.read_all_records_including_superseded_log_retained()
            .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// History-inclusive, log-retained record load that leaves the store
/// byte-for-byte untouched (issue #363), for the strictly read-only
/// `error-context --at`/`--as-of` lane.
///
/// Opening the embedded engine in place re-persists its on-disk index files, so
/// this copies the store to a throwaway temporary directory and reads the
/// log-retained history-inclusive view from the copy (mirrors
/// [`load_records_from_db_history_readonly`]). The only difference from that
/// function is the underlying read: enrichment-only log rewrites are collapsed
/// (see [`load_records_from_db_history_log_retained`]).
pub(crate) fn load_records_from_db_history_log_retained_readonly(
    data_dir: &Path,
) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        validate_existing_embedded_store(data_dir)?;
        let temp =
            tempfile::tempdir().context("failed to create temporary read-only store copy")?;
        let copy_root = temp.path().join("store");
        copy_dir_recursive(data_dir, &copy_root).with_context(|| {
            format!(
                "failed to copy store {} for read-only inspection",
                data_dir.display()
            )
        })?;
        load_records_from_db_history_log_retained(&copy_root)
    }
    #[cfg(not(feature = "embedded-aletheiadb"))]
    {
        let _ = data_dir;
        anyhow::bail!("--data-dir requires the embedded-aletheiadb feature")
    }
}

/// History-inclusive record load for the strictly read-only evidence-freshness
/// command. `--graph` is already read-only; `--data-dir` reads a throwaway copy.
pub(crate) fn load_evidence_freshness_records(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path),
        (None, Some(dir)) => load_records_from_db_history_readonly(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    }
}
