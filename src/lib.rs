//! Core library for Egregore.
//!
//! The current core scans local source repositories into a deterministic graph
//! IR. Agent memory, project state, artifacts, and richer domain graphs are
//! layered on top of the same `AletheiaDB` substrate.

/// Graph ingestion adapters.
pub mod adapters;
/// Claude Code transcript JSONL importer (M4 agent-memory source, issue #52).
pub mod claude_code;
/// Command-line interface.
pub mod cli;
/// Codex session/rollout JSONL importer (M3 agent-memory source).
pub mod codex;
/// Local daemon for shared multi-agent store access.
#[cfg(feature = "embedded-aletheiadb")]
pub mod daemon;
/// Decision record generation for user-context candidates.
pub mod decide;
/// Semantic enrichment and embedding boundaries.
pub mod embeddings;
/// Error and result types.
pub mod error;
/// Typed evidence write workflows for observations, command evidence, artifacts, and verification.
pub mod evidence;
/// Evidence-link freshness verdicts for agent observations (issue #85).
pub mod evidence_freshness;
/// Read-only store freshness classification (issue #82).
pub mod freshness;
/// Filesystem discovery.
pub mod fs;
/// GitHub Issues/PRs importer (issue #46).
pub mod github;
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
/// Evidence-to-code-graph resolver (issue #43).
pub mod link_evidence;
/// Local project/task JSONL importer (issue #42).
pub mod local_project;
/// MCP server exposing read-only evidence-query tools (issue #53).
#[cfg(feature = "embedded-aletheiadb")]
pub mod mcp;
/// Agent-memory recall evaluation harness (issue #91).
pub mod memory_recall_eval;
/// Parser orchestration.
pub mod parser;
/// Local setup preflight report for the `eg doctor` command (issue #75).
pub mod preflight;
/// Protected raw-artifact capture and retrieval (issue #60).
pub mod protected;
/// Agent-facing graph query helpers.
pub mod query;
/// Redaction policy engine (`docs/schema/redaction.md` v1).
pub mod redaction;
/// Offline repair workflow for Egregore stores (issue #49).
#[cfg(feature = "embedded-aletheiadb")]
pub mod repair;
/// Record schema-version compatibility checks.
pub mod schema_version;
/// Semantic search relevance evaluation harness (issue #58).
pub mod semantic_eval;
/// `rust-swe-agent` `.traj` importer (M2 agent-memory source).
pub mod traj;

use std::{collections::BTreeMap, path::Path, sync::LazyLock};

pub use claude_code::import_claude_code;
pub use codex::import_codex;
pub use decide::{DecideRequest, decide_candidate};
pub use error::{CodegraphError, Result};
pub use history::{scan_repository_history, scan_repository_history_with_override};
pub use ir::{
    AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, Domain, EdgeLabel, EgregoreGit,
    EmbeddingModel, EvidenceLink, Graph, GraphRecord, IdentitySource, MetricKind, NodeKind,
    NodeProvenance, PRODUCER_ENVELOPE_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION, PatchHandle,
    Producer, ProducerKind, RepositoryIdentityPayload, SCHEMA_VERSION,
    SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE, SEMANTIC_SCHEMA_VERSION, SelectionBasis,
    SemanticDriftMetadata, SnapshotHead, SourceSnapshotPayload, SourceSpan, TemporalMetadata,
    USER_CONTEXT_SCHEMA_VERSION, UserContextFields, UserContextScope, VERIFICATION_SCHEMA_VERSION,
    agent_memory_stable_id, artifact_stable_id, project_stable_id, semantic_stable_id, stable_id,
    user_context_stable_id, verification_stable_id,
};
pub use local_project::import_local_tasks;
pub use query::{
    ChangesContext, ChangesError, RepositoryIndex, RepositorySelectorError, SubsystemContext,
    SubsystemPrefixError, SymbolContext, UnresolvedRef, active_policy, audit_trail,
    changes_context, is_candidate_suppressed, path_is_under_prefix, pending_candidates,
    subsystem_context, symbol_context,
};
pub use schema_version::{
    RecordLineRead, RecordReadError, RecordVersion, UNKNOWN_SCHEMA_VERSION_CODE,
    UnknownSchemaVersion, record_version, validate_record_version,
};
pub use traj::import_traj;

/// Scans a repository into deterministic graph records.
///
/// Repository identity is derived from VCS remote URL, root commit SHA, or
/// canonical path — see `docs/schema/repository-identity.md`.
///
/// Each node record carries `valid_time` (set to the current wall-clock instant)
/// and `valid_time_source: "inferred_from_transaction_time"` per the rule in
/// `docs/schema/temporal-selectors.md`.
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository(repo_path: impl AsRef<Path>) -> Result<Graph> {
    scan_repository_with_override(repo_path, None)
}

/// Like `scan_repository` but accepts an explicit `transaction_time` (RFC 3339).
///
/// Primarily useful for deterministic tests that need to fix the scan timestamp.
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository_at(repo_path: impl AsRef<Path>, transaction_time: &str) -> Result<Graph> {
    scan_repository_at_with_override(repo_path, transaction_time, None)
}

/// Scans a repository with an optional identity override.
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
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    scan_repository_at_with_override(repo_path, &now, repo_id_override)
}

/// Like [`scan_repository_with_override`] but excludes repo-relative paths from
/// the dirty probe when stamping the snapshot.
///
/// Pass the output graph path (if inside the repository) so a pre-existing
/// `graph.jsonl` from a previous run is not counted as a source change (PR #186 E/F).
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository_with_exclusions(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
    snapshot_exclusions: &[String],
) -> Result<Graph> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    scan_repository_at_with_override_inner(repo_path, &now, repo_id_override, snapshot_exclusions)
}

/// Scans a repository with an explicit `transaction_time` and optional identity override.
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository_at_with_override(
    repo_path: impl AsRef<Path>,
    transaction_time: &str,
    repo_id_override: Option<&str>,
) -> Result<Graph> {
    scan_repository_at_with_override_inner(repo_path, transaction_time, repo_id_override, &[])
}

fn scan_repository_at_with_override_inner(
    repo_path: impl AsRef<Path>,
    transaction_time: &str,
    repo_id_override: Option<&str>,
    snapshot_exclusions: &[String],
) -> Result<Graph> {
    LazyLock::force(&PROCESS_STARTED_AT);
    let repo_root = repo_path.as_ref();
    validate_repository(repo_root)?;

    let repo_identity = identity::compute_repository_identity(repo_root, repo_id_override);
    let mut graph = Graph::new();
    let (repository_id, repo_record) = repository_record_from_identity(&repo_identity);
    // Stamp the store-level source-snapshot identity (issue #82) on the Repository
    // node. `head` + `dirty` are deterministic for an unchanged clean tree at a
    // fixed commit; `scanned_at` reuses the transaction-time override so the JSONL
    // stays byte-for-byte stable.
    let (head, dirty) = identity::working_tree_snapshot_excluding(repo_root, snapshot_exclusions);
    let snapshot = ir::SourceSnapshotPayload {
        head,
        dirty,
        repository_id: repository_id.clone(),
        scanned_at: transaction_time.to_owned(),
    };
    graph.push(
        repo_record
            .with_valid_time_inferred(transaction_time)
            .with_source_snapshot(snapshot),
    );

    for source_file in fs::discover_rust_source_files(repo_root)? {
        for record in scan_source_file_records(&source_file, &repository_id)? {
            graph.push(record.with_valid_time_inferred(transaction_time));
        }
    }

    Ok(graph.stamp_producer(&code_graph_producer()))
}

/// Wall-clock time captured at the start of the first scan in this process.
///
/// Forced before any graph construction in every public scan entry point so that
/// `producer_started_at` reflects scan-start wall-clock rather than the instant
/// `code_graph_producer()` is called at the end of a potentially long run.
pub(crate) static PROCESS_STARTED_AT: LazyLock<String> =
    LazyLock::new(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

pub(crate) fn code_graph_producer() -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::CodeGraphExtractor,
        producer_components: BTreeMap::from([
            (
                "tree_sitter".to_owned(),
                env!("TREE_SITTER_VERSION").to_owned(),
            ),
            (
                "tree_sitter_rust".to_owned(),
                env!("TREE_SITTER_RUST_VERSION").to_owned(),
            ),
        ]),
        producer_started_at: PROCESS_STARTED_AT.clone(),
    }
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
                url.strip_prefix("https://")
                    .or_else(|| url.strip_prefix("http://"))
            })
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, path)| path)
            .filter(|path| !path.is_empty())
            .unwrap_or(&payload.basename)
            .to_owned(),
        IdentitySource::LocalRootCommit => payload.root_commit_sha.as_deref().map_or_else(
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
    let normalized = crate::languages::rust::normalize_file_code(source);
    graph.push(GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some(repo_relative_path.clone()),
        None,
        Some(repo_relative_path.clone()),
        format!("Rust source file {repo_relative_path}\nSource:\n{normalized}"),
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
