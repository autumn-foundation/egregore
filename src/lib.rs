//! Core library for Egregore.
//!
//! The current core scans local source repositories into a deterministic graph
//! IR. Agent memory, project state, artifacts, and richer domain graphs are
//! layered on top of the same `AletheiaDB` substrate.

/// Graph ingestion adapters.
pub mod adapters;
/// Command-line interface.
pub mod cli;
/// Local daemon for shared multi-agent store access.
#[cfg(feature = "embedded-aletheiadb")]
pub mod daemon;
/// Semantic enrichment and embedding boundaries.
pub mod embeddings;
/// Error and result types.
pub mod error;
/// Filesystem discovery.
pub mod fs;
/// Git history replay.
pub mod history;
/// Incremental scan cache.
pub mod incremental;
/// Stable graph intermediate representation.
pub mod ir;
/// Language-specific extractors.
pub mod languages;
/// Parser orchestration.
pub mod parser;
/// Agent-facing graph query helpers.
pub mod query;

use std::{ffi::OsStr, path::Path};

pub use error::{CodegraphError, Result};
pub use history::scan_repository_history;
pub use ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, Graph, GraphRecord, NodeKind,
    NodeProvenance, SCHEMA_VERSION, SemanticDriftMetadata, SourceSpan, TemporalMetadata,
    agent_memory_stable_id, stable_id,
};

/// Scans a repository into deterministic graph records.
///
/// This emits repository, Rust source file, syntax-backed symbol, import,
/// diagnostic, and relationship records without requiring an `AletheiaDB` store.
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository(repo_path: impl AsRef<Path>) -> Result<Graph> {
    let repo_root = repo_path.as_ref();
    validate_repository(repo_root)?;

    let repo_name = repo_root
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("repository");
    let mut graph = Graph::new();
    let (repository_id, repository_record) = repository_record(repo_name);
    graph.push(repository_record);

    for source_file in fs::discover_rust_source_files(repo_root)? {
        for record in scan_source_file_records(&source_file, &repository_id)? {
            graph.push(record);
        }
    }

    Ok(graph)
}

pub(crate) fn repository_record(repo_name: &str) -> (String, GraphRecord) {
    let repository_id = stable_id(&["node", "repository", repo_name]);
    let record = GraphRecord::node(
        repository_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some(repo_name.to_owned()),
        format!("Repository {repo_name}"),
    );
    (repository_id, record)
}

pub(crate) fn scan_source_file_records(
    source_file: &fs::SourceFile,
    repository_id: &str,
) -> Result<Vec<GraphRecord>> {
    let source =
        std::fs::read_to_string(&source_file.path).map_err(|source| CodegraphError::ReadFile {
            path: source_file.path.clone(),
            source,
        })?;
    scan_source_text_records(source_file, &source, repository_id)
}

pub(crate) fn scan_source_text_records(
    source_file: &fs::SourceFile,
    source: &str,
    repository_id: &str,
) -> Result<Vec<GraphRecord>> {
    let mut graph = Graph::new();
    let repo_relative_path = source_file.repo_relative_path.clone();
    let file_id = stable_id(&["node", "file", &repo_relative_path]);
    graph.push(GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some(repo_relative_path.clone()),
        None,
        Some(repo_relative_path.clone()),
        format!("Rust source file {repo_relative_path}"),
    ));
    parser::add_repository_file_edge(&mut graph, repository_id, &file_id);
    parser::extract_source_text(source_file, source, &file_id, &mut graph)?;
    Ok(graph.records().to_vec())
}

fn validate_repository(repo_root: &Path) -> Result<()> {
    if !repo_root.exists() {
        return Err(CodegraphError::RepositoryMissing {
            path: repo_root.to_path_buf(),
        });
    }

    if !repo_root.is_dir() {
        return Err(CodegraphError::RepositoryNotDirectory {
            path: repo_root.to_path_buf(),
        });
    }

    Ok(())
}

pub(crate) fn normalize_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
