//! Incremental repository scanning.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

use chrono::Utc;

use crate::{
    PROCESS_STARTED_AT, code_graph_producer,
    error::{CodegraphError, Result},
    identity,
    ir::{Graph, GraphRecord, ProducerKind, SCHEMA_VERSION, stable_id, versioned_stable_id},
    languages::cross_file::{
        FileFacts, apply_out_of_line_test_scope, cross_file_call_records,
        label_same_file_call_resolutions,
    },
    repository_record_from_identity, scan_source_file_records,
    schema_version::validate_record_version,
};

/// Incremental cache schema for extractor output stored on disk.
///
/// v5: cached Rust `Symbol` records carry `visibility` / `signature` / `doc`
/// declaration-surface fields (issue #124); older caches rebuild so reused
/// records are never missing the new fields.
///
/// v6 adds per-file cross-file resolution facts and the previously emitted
/// cross-file record IDs (issue #152), and invalidates caches whose per-file
/// records still contain phantom comment/string-sourced reference edges
/// (issue #134); same-file resolution labels are recomputed per scan and are
/// never cached.
const CACHE_SCHEMA_VERSION: u32 = 7;

/// Result of an incremental repository scan.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IncrementalScan {
    /// Graph records for the current repository state plus tombstones.
    pub graph: Graph,
    /// Repository-relative files rebuilt in this scan.
    pub rebuilt_files: Vec<String>,
    /// Repository-relative files reused from cache.
    pub reused_files: Vec<String>,
    /// Repository-relative files emitted as tombstones.
    pub tombstoned_files: Vec<String>,
}

/// Scans a repository using a JSON file cache for unchanged source files.
///
/// # Errors
///
/// Returns an error when repository discovery, source parsing, cache parsing, or
/// cache persistence fails.
pub fn scan_repository_incremental(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
) -> Result<IncrementalScan> {
    let transaction_time = Utc::now().to_rfc3339();
    scan_repository_incremental_at(repo_path, cache_path, &transaction_time)
}

/// Like [`scan_repository_incremental`] but excludes repo-relative paths from the
/// dirty probe when stamping the snapshot.
///
/// Pass the data-dir path (if inside the repository) so an existing embedded store
/// is not counted as a source change, keeping `eg freshness --data-dir` clean after
/// `eg refresh` on an uncommitted-edits tree (PR #186 A).
///
/// # Errors
///
/// Returns an error when repository discovery, source parsing, cache parsing, or
/// cache persistence fails.
pub fn scan_repository_incremental_excluding(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
    snapshot_exclusions: &[String],
) -> Result<IncrementalScan> {
    let transaction_time = Utc::now().to_rfc3339();
    scan_repository_incremental_at_inner(
        repo_path,
        cache_path,
        &transaction_time,
        snapshot_exclusions,
    )
}

/// Like [`scan_repository_incremental`] but accepts an explicit `transaction_time` (RFC 3339).
///
/// # Errors
///
/// Returns an error when repository discovery, source parsing, cache parsing, or
/// cache persistence fails.
pub fn scan_repository_incremental_at(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
    transaction_time: &str,
) -> Result<IncrementalScan> {
    scan_repository_incremental_at_inner(repo_path, cache_path, transaction_time, &[])
}

#[allow(clippy::too_many_lines)]
fn scan_repository_incremental_at_inner(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
    transaction_time: &str,
    snapshot_exclusions: &[String],
) -> Result<IncrementalScan> {
    std::sync::LazyLock::force(&PROCESS_STARTED_AT);
    let repo_root = repo_path.as_ref();
    crate::validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, None);
    let (repository_id, repository) = repository_record_from_identity(&repo_identity);
    let previous_cache = CacheFile::load(cache_path.as_ref())?;
    let mut can_reuse_cache_records = previous_cache.schema_version == CACHE_SCHEMA_VERSION
        && previous_cache.repository_id == repository_id;
    if can_reuse_cache_records && previous_cache.validate_record_versions().is_err() {
        can_reuse_cache_records = false;
    }
    let mut next_cache = CacheFile::default();
    let mut graph = Graph::new();
    let mut rebuilt_files = Vec::new();
    let mut reused_files = Vec::new();
    let mut seen_files = BTreeSet::new();

    // Stamp the store-level source-snapshot identity (issue #82) on the Repository
    // node, mirroring the full-scan path. Without this, refreshing a stale store
    // would replace the stamped Repository node with one whose snapshot is absent,
    // so a follow-up `eg freshness --data-dir` would report `unknown` instead of
    // confirming the refreshed store is fresh.
    let (head, dirty) = identity::working_tree_snapshot_excluding(repo_root, snapshot_exclusions);
    let snapshot = crate::ir::SourceSnapshotPayload {
        head,
        dirty,
        repository_id: repository_id.clone(),
        scanned_at: transaction_time.to_owned(),
    };
    graph.push(
        repository
            .with_valid_time_inferred(transaction_time)
            .with_source_snapshot(snapshot),
    );

    // If the repository identity changed from a previous scan, tombstone the old Repository node
    // so it does not remain live in persisted stores alongside the new identity.  Without this,
    // a store first scanned with local_path identity keeps the stale Repository node even after
    // the identity changes to Remote, which causes the daemon's shared-store guard to keep
    // rejecting otherwise valid writes.
    if !previous_cache.repository_id.is_empty() && previous_cache.repository_id != repository_id {
        let old_repo_id = &previous_cache.repository_id;
        graph.push(GraphRecord::Tombstone {
            id: stable_id(&["tombstone", "repository-identity-changed", old_repo_id]),
            schema_version: SCHEMA_VERSION,
            deleted_id: old_repo_id.clone(),
            summary: format!("Repository identity changed; stale Repository {old_repo_id} removed"),
            producer: None,
        });
    } else if previous_cache.repository_id.is_empty() && !previous_cache.files.is_empty() {
        // Legacy cache written before the repository_id field existed: infer the old
        // basename-derived ID and tombstone it so persisted stores can retire stale records.
        let basename = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("repository");
        // Use the old cache's schema version so the deleted_id matches what the old
        // extractor actually wrote (e.g. `codegraph:v1:…` for a v1 cache).
        let legacy_repo_id = versioned_stable_id(
            previous_cache.schema_version,
            &["node", "repository", basename],
        );
        if legacy_repo_id != repository_id {
            graph.push(GraphRecord::Tombstone {
                id: stable_id(&["tombstone", "repository-identity-changed", &legacy_repo_id]),
                schema_version: SCHEMA_VERSION,
                deleted_id: legacy_repo_id.clone(),
                summary: format!(
                    "Repository identity changed; stale Repository {legacy_repo_id} removed"
                ),
                producer: None,
            });
        }
    }

    for source_file in crate::fs::discover_source_files(repo_root)? {
        let hash = file_hash(&source_file.path)?;
        seen_files.insert(source_file.repo_relative_path.clone());
        let previous_entry = previous_cache.files.get(&source_file.repo_relative_path);
        let cached = previous_entry.filter(|_| can_reuse_cache_records);

        let (records, facts) = if let Some(cached) = cached.filter(|entry| entry.hash == hash) {
            reused_files.push(source_file.repo_relative_path.clone());
            // Restamp reused records so valid_time reflects this scan's transaction time,
            // not the prior scan's time when they were first extracted.
            let records = cached
                .records
                .iter()
                .cloned()
                .map(|r| r.with_valid_time_inferred(transaction_time))
                .collect::<Vec<_>>();
            (records, cached.facts.clone())
        } else {
            rebuilt_files.push(source_file.repo_relative_path.clone());
            let (records, facts) = scan_source_file_records(&source_file, &repository_id)?;
            let records = records
                .into_iter()
                .map(|r| r.with_valid_time_inferred(transaction_time))
                .collect::<Vec<_>>();
            if let Some(invalidated) = previous_entry {
                for tombstone in invalidated_record_tombstones(
                    &source_file.repo_relative_path,
                    &invalidated.records,
                    &records,
                    transaction_time,
                ) {
                    graph.push(tombstone);
                }
            }
            (records, facts)
        };

        for record in &records {
            graph.push(record.clone());
        }
        next_cache.files.insert(
            source_file.repo_relative_path.clone(),
            CachedFile {
                hash,
                records,
                facts,
            },
        );
    }

    // Repo-wide cross-file call resolution (issue #152): recompute the pass
    // from every file's cached or freshly extracted facts, and tombstone any
    // cross-file record from the previous scan that no longer exists so
    // persisted stores can retire it.
    let facts_by_file: BTreeMap<String, FileFacts> = next_cache
        .files
        .iter()
        .filter(|(_, cached_file)| !cached_file.facts.is_empty())
        .map(|(path, cached_file)| (path.clone(), cached_file.facts.clone()))
        .collect();
    let cross_file_records = cross_file_call_records(&repository_id, &facts_by_file);
    let cross_file_ids: BTreeSet<String> = cross_file_records
        .iter()
        .map(|record| record.id().to_owned())
        .collect();
    // Previous cross-file record IDs are tombstoned even when cache reuse is
    // disabled (repository identity or cache schema mismatch): those IDs can
    // embed the old repository identity, so the recomputed pass never re-emits
    // them and a persisted store would otherwise keep them live forever. This
    // mirrors the per-file path, which tombstones invalidated cached records
    // regardless of reuse eligibility.
    for stale_id in previous_cache
        .cross_file_record_ids
        .iter()
        .filter(|previous_id| !cross_file_ids.contains(*previous_id))
    {
        graph.push(invalidated_record_tombstone(
            "cross-file",
            stale_id,
            transaction_time,
        ));
    }
    for record in cross_file_records {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }
    next_cache.cross_file_record_ids = cross_file_ids.into_iter().collect();
    // Same-file resolution labeling (issue #134): recomputed over the whole
    // assembled graph every scan — never cached — so a definition added or
    // removed in another file re-labels an unchanged file's edges correctly.
    label_same_file_call_resolutions(graph.records_mut(), &facts_by_file);
    // Out-of-line `#[cfg(test)] mod x;` test-scope marking (issue #223):
    // recomputed over the whole assembled graph every scan — never cached —
    // so a gating change in a parent file re-contexts an unchanged module
    // file's cached panic-risk sites correctly.
    apply_out_of_line_test_scope(graph.records_mut(), &facts_by_file);

    let mut tombstoned_files = Vec::new();
    for (removed, cached_file) in &previous_cache.files {
        if !seen_files.contains(removed) {
            tombstoned_files.push(removed.clone());
            // Tombstone every cached record (File node, Symbol nodes, DEFINES edges) so nothing
            // from the deleted file remains live in persisted stores.
            for tombstone in
                invalidated_record_tombstones(removed, &cached_file.records, &[], transaction_time)
            {
                graph.push(tombstone);
            }
        }
    }

    next_cache.repository_id.clone_from(&repository_id);
    next_cache.save(cache_path.as_ref())?;
    let languages = crate::languages_in_graph(&graph);
    let mut producer = code_graph_producer(&languages);
    producer.producer_kind = ProducerKind::IncrementalCache;
    producer.producer_components.insert(
        "cache_format_version".to_owned(),
        CACHE_SCHEMA_VERSION.to_string(),
    );
    Ok(IncrementalScan {
        graph: graph.stamp_producer(&producer),
        rebuilt_files,
        reused_files,
        tombstoned_files,
    })
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct CacheFile {
    schema_version: u32,
    #[serde(default)]
    repository_id: String,
    files: BTreeMap<String, CachedFile>,
    /// Cross-file resolution records emitted by the previous scan (issue #152),
    /// kept so a later scan can tombstone the ones that disappear.
    #[serde(default)]
    cross_file_record_ids: Vec<String>,
}

impl Default for CacheFile {
    fn default() -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            repository_id: String::new(),
            files: BTreeMap::new(),
            cross_file_record_ids: Vec::new(),
        }
    }
}

impl CacheFile {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path).map_err(|source| CodegraphError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&raw).map_err(CodegraphError::from)
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CodegraphError::WriteFile {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw).map_err(|source| CodegraphError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
    }

    fn validate_record_versions(&self) -> Result<()> {
        for cached_file in self.files.values() {
            for record in &cached_file.records {
                validate_record_version(record).map_err(|unknown| {
                    CodegraphError::UnsupportedSchemaVersion {
                        message: format!(
                            "incremental cache contains unsupported record schema version: {}",
                            unknown.version
                        ),
                    }
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct CachedFile {
    hash: String,
    records: Vec<GraphRecord>,
    /// Cross-file resolution facts for the file (issue #152).
    #[serde(default, skip_serializing_if = "FileFacts::is_empty")]
    facts: FileFacts,
}

fn file_hash(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|source| CodegraphError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn invalidated_record_tombstones(
    repo_relative_path: &str,
    old_records: &[GraphRecord],
    rebuilt_records: &[GraphRecord],
    transaction_time: &str,
) -> Vec<GraphRecord> {
    let rebuilt_ids = rebuilt_records
        .iter()
        .map(GraphRecord::id)
        .collect::<BTreeSet<_>>();
    old_records
        .iter()
        .filter(|record| !rebuilt_ids.contains(record.id()))
        .map(|record| {
            invalidated_record_tombstone(repo_relative_path, record.id(), transaction_time)
        })
        .collect()
}

fn invalidated_record_tombstone(
    repo_relative_path: &str,
    deleted_id: &str,
    transaction_time: &str,
) -> GraphRecord {
    GraphRecord::Tombstone {
        // Include transaction_time so re-removing the same record after a re-add produces
        // a fresh tombstone ID that gets a higher egregore_seq and supersedes the re-added node.
        id: stable_id(&[
            "tombstone",
            "cache-schema",
            repo_relative_path,
            deleted_id,
            transaction_time,
        ]),
        schema_version: SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: format!("Invalidated stale cached record {deleted_id} from {repo_relative_path}"),
        producer: None,
    }
}
