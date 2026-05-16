//! Incremental repository scanning.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{CodegraphError, Result},
    ir::{Graph, GraphRecord, SCHEMA_VERSION, stable_id},
    repository_record, scan_source_file_records,
};

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

    let repo_name = repo_root
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("repository");
    let (repository_id, repository) = repository_record(repo_name);
    let previous_cache = CacheFile::load(cache_path.as_ref())?;
    let mut next_cache = CacheFile::default();
    let mut graph = Graph::new();
    let mut rebuilt_files = Vec::new();
    let mut reused_files = Vec::new();
    let mut seen_files = BTreeSet::new();

    graph.push(repository);

    for source_file in crate::fs::discover_rust_source_files(repo_root)? {
        let hash = file_hash(&source_file.path)?;
        seen_files.insert(source_file.repo_relative_path.clone());
        let cached = previous_cache.files.get(&source_file.repo_relative_path);

        let records = if let Some(cached) = cached.filter(|entry| entry.hash == hash) {
            reused_files.push(source_file.repo_relative_path.clone());
            cached.records.clone()
        } else {
            rebuilt_files.push(source_file.repo_relative_path.clone());
            scan_source_file_records(&source_file, &repository_id)?
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
    for removed in previous_cache.files.keys() {
        if !seen_files.contains(removed) {
            tombstoned_files.push(removed.clone());
            graph.push(file_tombstone(removed));
        }
    }

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
    files: BTreeMap<String, CachedFile>,
}

impl Default for CacheFile {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
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

fn file_tombstone(repo_relative_path: &str) -> GraphRecord {
    let deleted_id = stable_id(&["node", "file", repo_relative_path]);
    GraphRecord::Tombstone {
        id: stable_id(&["tombstone", "file", repo_relative_path, &deleted_id]),
        schema_version: SCHEMA_VERSION,
        deleted_id,
        summary: format!("Removed source file {repo_relative_path}"),
    }
}
