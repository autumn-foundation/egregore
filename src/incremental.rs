//! Incremental repository scanning.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{CodegraphError, Result},
    identity,
    ir::{Graph, GraphRecord, SCHEMA_VERSION, stable_id},
    repository_record_from_identity, scan_source_file_records,
};

/// Incremental cache schema for extractor output stored on disk.
const CACHE_SCHEMA_VERSION: u32 = 3;

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

/// Scans a repository using a JSON file cache for unchanged Rust files.
///
/// # Errors
///
/// Returns an error when repository discovery, source parsing, cache parsing, or
/// cache persistence fails.
pub fn scan_repository_incremental(
    repo_path: impl AsRef<Path>,
    cache_path: impl AsRef<Path>,
) -> Result<IncrementalScan> {
    let repo_root = repo_path.as_ref();
    crate::validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, None);
    let (repository_id, repository) = repository_record_from_identity(&repo_identity);
    let previous_cache = CacheFile::load(cache_path.as_ref())?;
    let can_reuse_cache_records = previous_cache.schema_version == CACHE_SCHEMA_VERSION
        && previous_cache.repository_id == repository_id;
    let mut next_cache = CacheFile::default();
    let mut graph = Graph::new();
    let mut rebuilt_files = Vec::new();
    let mut reused_files = Vec::new();
    let mut seen_files = BTreeSet::new();

    graph.push(repository);

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
        });
    }

    for source_file in crate::fs::discover_rust_source_files(repo_root)? {
        let hash = file_hash(&source_file.path)?;
        seen_files.insert(source_file.repo_relative_path.clone());
        let previous_entry = previous_cache.files.get(&source_file.repo_relative_path);
        let cached = previous_entry.filter(|_| can_reuse_cache_records);

        let records = if let Some(cached) = cached.filter(|entry| entry.hash == hash) {
            reused_files.push(source_file.repo_relative_path.clone());
            cached.records.clone()
        } else {
            rebuilt_files.push(source_file.repo_relative_path.clone());
            let records = scan_source_file_records(&source_file, &repository_id)?;
            if !can_reuse_cache_records && let Some(invalidated) = previous_entry {
                for tombstone in invalidated_record_tombstones(
                    &source_file.repo_relative_path,
                    &invalidated.records,
                    &records,
                ) {
                    graph.push(tombstone);
                }
            }
            records
        };

        for record in &records {
            graph.push(record.clone());
        }
        next_cache.files.insert(
            source_file.repo_relative_path.clone(),
            CachedFile { hash, records },
        );
    }

    let mut tombstoned_files = Vec::new();
    for (removed, cached_file) in &previous_cache.files {
        if !seen_files.contains(removed) {
            tombstoned_files.push(removed.clone());
            if can_reuse_cache_records {
                // repository_id is stable — tombstone the expected current file ID.
                graph.push(file_tombstone(removed, &repository_id));
            } else {
                // repository_id changed; emit tombstones from the actual cached record IDs
                // so stale records from the old identity are correctly deleted.
                for record in &cached_file.records {
                    graph.push(invalidated_record_tombstone(removed, record.id()));
                }
            }
        }
    }

    next_cache.repository_id.clone_from(&repository_id);
    next_cache.save(cache_path.as_ref())?;
    Ok(IncrementalScan {
        graph,
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
}

impl Default for CacheFile {
    fn default() -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            repository_id: String::new(),
            files: BTreeMap::new(),
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
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct CachedFile {
    hash: String,
    records: Vec<GraphRecord>,
}

fn file_hash(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|source| CodegraphError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn file_tombstone(repo_relative_path: &str, repository_id: &str) -> GraphRecord {
    let deleted_id = stable_id(&["node", "file", repository_id, repo_relative_path]);
    GraphRecord::Tombstone {
        id: stable_id(&[
            "tombstone",
            "file",
            repository_id,
            repo_relative_path,
            &deleted_id,
        ]),
        schema_version: SCHEMA_VERSION,
        deleted_id,
        summary: format!("Removed source file {repo_relative_path}"),
    }
}

fn invalidated_record_tombstones(
    repo_relative_path: &str,
    old_records: &[GraphRecord],
    rebuilt_records: &[GraphRecord],
) -> Vec<GraphRecord> {
    let rebuilt_ids = rebuilt_records
        .iter()
        .map(GraphRecord::id)
        .collect::<BTreeSet<_>>();
    old_records
        .iter()
        .filter(|record| !rebuilt_ids.contains(record.id()))
        .map(|record| invalidated_record_tombstone(repo_relative_path, record.id()))
        .collect()
}

fn invalidated_record_tombstone(repo_relative_path: &str, deleted_id: &str) -> GraphRecord {
    GraphRecord::Tombstone {
        id: stable_id(&["tombstone", "cache-schema", repo_relative_path, deleted_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id: deleted_id.to_owned(),
        summary: format!("Invalidated stale cached record {deleted_id} from {repo_relative_path}"),
    }
}
