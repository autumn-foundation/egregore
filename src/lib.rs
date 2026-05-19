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
/// Repository identity computation.
pub mod identity;
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
/// `rust-swe-agent` `.traj` importer (M2 agent-memory source).
pub mod traj;

use std::path::Path;

pub use error::{CodegraphError, Result};
pub use history::{scan_repository_history, scan_repository_history_with_override};
pub use ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, Graph, GraphRecord, IdentitySource,
    NodeKind, NodeProvenance, RepositoryIdentityPayload, SCHEMA_VERSION, SemanticDriftMetadata,
    SourceSpan, TemporalMetadata, agent_memory_stable_id, stable_id,
};
pub use traj::import_traj;

/// Scans a repository into deterministic graph records.
///
/// Repository identity is derived from VCS remote URL, root commit SHA, or
/// canonical path — see `docs/schema/repository-identity.md`.
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository(repo_path: impl AsRef<Path>) -> Result<Graph> {
    scan_repository_with_override(repo_path, None)
}

/// Scans a repository into deterministic graph records with an optional identity override.
///
/// When `repo_id_override` is `Some`, its value is used directly as the
/// canonical input for the repository's stable ID (forcing
/// `identity_source = operator_override`). Pass `None` for normal auto-detection.
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository_with_override(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
) -> Result<Graph> {
    let repo_root = repo_path.as_ref();
    validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, repo_id_override);
    let mut graph = Graph::new();
    let (repository_id, repo_record) = repository_record_from_identity(&repo_identity);
    graph.push(repo_record);

    for source_file in fs::discover_rust_source_files(repo_root)? {
        for record in scan_source_file_records(&source_file, &repository_id)? {
            graph.push(record);
        }
    }

    Ok(graph)
}

pub(crate) fn repository_record_from_identity(
    identity: &identity::RepositoryIdentity,
) -> (String, GraphRecord) {
    let id = identity.id.clone();
    let display_name = stable_display_name(&identity.payload);
    let record = GraphRecord::node(
        id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some(display_name.clone()),
        format!("Repository {display_name}"),
    )
    .with_repository_identity(identity.payload.clone());
    (id, record)
}

/// Returns a stable display name for the repository that does not depend on the
/// local checkout directory basename when a more canonical source is available.
fn stable_display_name(payload: &RepositoryIdentityPayload) -> String {
    match &payload.identity_source {
        IdentitySource::Remote => payload
            .remote_url
            .as_deref()
            .and_then(|url| {
                url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))
            })
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, path)| path)
            .filter(|path| !path.is_empty())
            .unwrap_or(&payload.basename)
            .to_owned(),
        IdentitySource::LocalRootCommit => payload
            .root_commit_sha
            .as_deref()
            .map_or_else(
                || payload.basename.clone(),
                |sha| {
                    let short: String = sha.chars().take(12).collect();
                    format!("commit-{short}")
                },
            ),
        IdentitySource::OperatorOverride | IdentitySource::LocalPath => payload.basename.clone(),
    }
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
    let file_id = stable_id(&["node", "file", repository_id, &repo_relative_path]);
    graph.push(GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some(repo_relative_path.clone()),
        None,
        Some(repo_relative_path.clone()),
        format!("Rust source file {repo_relative_path}"),
    ));
    parser::add_repository_file_edge(&mut graph, repository_id, &file_id);
    parser::extract_source_text(source_file, source, &file_id, repository_id, &mut graph)?;
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
