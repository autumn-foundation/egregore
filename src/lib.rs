//! Core library for Egregore.
//!
//! The current core scans local source repositories into a deterministic graph
//! IR. Agent memory, project state, artifacts, and richer domain graphs are
//! layered on top of the same `AletheiaDB` substrate.

/// Extraction accuracy measurement against a ground-truth labeled corpus (issue #93).
pub mod accuracy;
/// Graph ingestion adapters.
pub mod adapters;
/// Antigravity transcript JSONL importer.
pub mod antigravity;
/// Evidence bundle export, verification, and inspection (issue #68).
pub mod bundle;
/// Citation-completeness audit over public query workflows (issue #65).
pub mod citation_audit;
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
/// Versioned SOC2 control->evidence-class catalog loader, validator, and hash-pin (issue #337).
pub mod evidence_pack;
/// Operator-facing logical retraction of persisted records (issue #231).
pub mod forget;
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
/// Runtime log-signature extraction (`scan-logs`, issues #319 / #320).
pub mod log_graph;
/// Error-signature → agent-run / command correlation linker (`link-logs`,
/// issue #323).
pub mod log_link;
/// Backtrace stack-frame resolution to code-graph symbols (`resolve-frames`,
/// issue #322).
pub mod log_resolve;
/// Cargo manifest dependency-declaration extraction (issue #180).
pub mod manifest_deps;
/// MCP server exposing read-only evidence-query tools (issue #53).
#[cfg(feature = "embedded-aletheiadb")]
pub mod mcp;
/// Agent-memory health report to flag reviewability risk (issue #94).
pub mod memory_health;
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
/// At-import redaction report (issue #266).
pub mod redaction_report;
/// Offline repair workflow for Egregore stores (issue #49).
#[cfg(feature = "embedded-aletheiadb")]
pub mod repair;
/// Standing citable review-coverage gate over merged PRs (issue #339).
pub mod review_coverage;
/// Record schema-version compatibility checks.
pub mod schema_version;
/// Semantic search relevance evaluation harness (issue #58).
pub mod semantic_eval;
/// Transitive memory supersession and contradiction resolution (issue #92).
pub mod temporal_status;
/// Query-answer token-cost measurement against the ripgrep baseline (issue #84).
pub mod token_cost;
/// `rust-swe-agent` `.traj` importer (M2 agent-memory source).
pub mod traj;
/// Pre-ingest referential-integrity validation for graph JSONL (issue #103).
pub mod validate;
/// Transcripts watcher.
pub mod watch;

use std::{collections::BTreeMap, path::Path, sync::LazyLock};

pub use antigravity::import_antigravity;
pub use claude_code::import_claude_code;
pub use codex::import_codex;
pub use decide::{DecideRequest, decide_candidate};
pub use error::{CodegraphError, Result};
pub use history::{scan_repository_history, scan_repository_history_with_override};
pub use ir::{
    AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, CallResolution,
    DependencyDeclarationPayload, Domain, EdgeLabel, EgregoreGit, EmbeddingModel,
    ErrorSignaturePayload, EvidenceLink, Graph, GraphRecord, IdentitySource, LOG_SCHEMA_VERSION,
    LogEventPayload, LogOccurrenceBucketPayload, LogPayload, LogSourcePayload, MetricKind,
    NodeKind, NodeProvenance, PRODUCER_ENVELOPE_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION,
    PatchHandle, Producer, ProducerKind, RepositoryIdentityPayload, SCHEMA_VERSION,
    SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE, SEMANTIC_SCHEMA_VERSION, ScanCoveragePayload,
    SelectionBasis, SemanticDriftMetadata, SnapshotHead, SourceSnapshotPayload, SourceSpan,
    TemporalMetadata, USER_CONTEXT_SCHEMA_VERSION, UserContextFields, UserContextScope,
    VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, artifact_stable_id, log_stable_id,
    project_stable_id, semantic_stable_id, stable_id, user_context_stable_id,
    verification_stable_id,
};
pub use local_project::import_local_tasks;
pub use query::{
    ChangesContext, ChangesError, PublicApiDeltas, PublicApiDeltasOptions, RangeDeltas,
    RangeDeltasError, RepositoryIndex, RepositorySelectorError, SubsystemContext,
    SubsystemPrefixError, SymbolContext, UnresolvedRef, active_policy, audit_trail,
    changes_context, is_candidate_suppressed, path_is_under_prefix, pending_candidates,
    public_api_deltas, range_deltas, subsystem_context, symbol_context,
};
pub use schema_version::{
    RecordLineRead, RecordReadError, RecordVersion, UNKNOWN_SCHEMA_VERSION_CODE,
    UnknownSchemaVersion, record_version, validate_record_version,
};
pub use temporal_status::{SupersessionMode, TemporalReference, TemporalResolver};
pub use traj::import_traj;
pub use watch::watch;

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

    let (source_files, mut coverage_tally) = fs::discover_source_files_with_coverage(repo_root)?;

    let mut facts_by_file = BTreeMap::new();
    for source_file in source_files {
        let (records, facts) = scan_source_file_records(&source_file, &repository_id)?;
        for record in records {
            graph.push(record.with_valid_time_inferred(transaction_time));
        }
        if !facts.is_empty() {
            facts_by_file.insert(source_file.repo_relative_path.clone(), facts);
        }
    }

    // Repo-wide cross-file call resolution (issue #152): per-file extraction
    // only links calls to same-file definitions, so resolve the collected
    // call sites against every file's definitions before sealing the graph.
    for record in languages::cross_file::cross_file_call_records(&repository_id, &facts_by_file) {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }
    // Repo-wide cross-file trait resolution (issue #344): an out-of-line impl
    // (`impl crate::T for Foo` in a `mod m;` file) whose trait is defined in
    // another file resolves against every file's exported trait/type
    // definitions here, so it edge-backs instead of dropping.
    for record in
        languages::cross_file::cross_file_implements_records(&repository_id, &facts_by_file)
    {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }
    // Same-file resolution labeling (issue #134): stamp per-file CALLS edges
    // backed by Tree-sitter call sites with the shared resolution status.
    languages::cross_file::label_same_file_call_resolutions(graph.records_mut(), &facts_by_file);
    // Out-of-line `#[cfg(test)] mod x;` test-scope marking (issue #223): the
    // module file is extracted with no view of the gating attribute, so the
    // repo-wide pass rewrites its panic-risk sites to test context.
    languages::cross_file::apply_out_of_line_test_scope(graph.records_mut(), &facts_by_file);

    // Declared Cargo dependencies (issue #180): every manifest's directly-
    // declared dependencies become deterministic, citable graph facts joined
    // with the nearest lockfile's resolved versions. This mints a `File` node
    // for each manifest that declares dependencies — a walked file the source
    // filter skipped yet that ends up genuinely indexed.
    for record in manifest_deps::scan_dependency_records(repo_root, &repository_id)? {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }

    // Scan-coverage reconciliation (issue #135): finalize the tally against the
    // COMPLETE set of `File` nodes the graph now carries, not source discovery
    // alone. A manifest the source filter skipped (`Cargo.toml` -> `toml`) but
    // that manifest extraction indexed with a `File` node is honestly counted
    // under `files_indexed`, never left mislabeled under `skipped_by_extension`.
    // This inherently covers any current or future non-source `File` producer
    // and preserves the invariant `files_indexed + Σ skipped == files_walked`.
    reconcile_scan_coverage(&graph, &mut coverage_tally);
    // A single deterministic `ScanCoverage` node makes file-level indexing
    // coverage a stated, queryable graph fact, attached to its Repository by a
    // CONTAINS edge so it is citable and never an orphan.
    for record in scan_coverage_records(&repository_id, &coverage_tally) {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }

    let languages = languages_in_graph(&graph);
    Ok(graph.stamp_producer(&code_graph_producer(&languages)))
}

/// Wall-clock time captured at the start of the first scan in this process.
///
/// Forced before any graph construction in every public scan entry point so that
/// `producer_started_at` reflects scan-start wall-clock rather than the instant
/// `code_graph_producer()` is called at the end of a potentially long run.
pub(crate) static PROCESS_STARTED_AT: LazyLock<String> =
    LazyLock::new(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

/// The set of source languages whose records appear in `graph`, deduplicated and
/// in a deterministic order.
///
/// Drives the producer envelope so a graph records exactly the Tree-sitter
/// grammars that produced it — a Rust-only graph stays byte-identical to its
/// historical form, and Python adds its grammar component only when present.
pub(crate) fn languages_in_graph(graph: &Graph) -> Vec<languages::Language> {
    let mut seen = std::collections::BTreeSet::new();
    for record in graph.records() {
        if let GraphRecord::Node {
            language: Some(tag),
            ..
        } = record
            && let Some(language) = languages::Language::from_tag(tag)
        {
            seen.insert(language.tag());
        }
    }
    seen.into_iter()
        .filter_map(languages::Language::from_tag)
        .collect()
}

pub(crate) fn code_graph_producer(languages: &[languages::Language]) -> Producer {
    let mut producer_components = BTreeMap::from([(
        "tree_sitter".to_owned(),
        env!("TREE_SITTER_VERSION").to_owned(),
    )]);
    // Default to Rust so a graph with no syntax-backed nodes (e.g. an empty repo)
    // keeps the historical Rust-only producer envelope.
    let languages = if languages.is_empty() {
        &[languages::Language::Rust][..]
    } else {
        languages
    };
    for language in languages {
        let (key, version) = language.tree_sitter_component();
        producer_components.insert(key.to_owned(), version.to_owned());
    }
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::CodeGraphExtractor,
        producer_components,
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

/// Builds the `ScanCoverage` node and its `Repository —CONTAINS→ ScanCoverage`
/// attribution edge from a discovery tally (issue #135).
///
/// The node ID is keyed solely on the repository so a repository has exactly one
/// coverage node, re-minted deterministically on every full scan. The named
/// language scope is derived from [`languages::Language::ALL`], never a
/// hard-coded list, so it stays in lockstep with the extractor's real
/// capability (AC6).
/// Finalizes the scan-coverage tally against the complete set of `File` nodes
/// the graph carries after every File-producing extractor has run (issue #135).
///
/// Source discovery classifies a walked file as skipped purely on its extension,
/// but a later extractor (manifest dependency extraction, issue #180) may mint a
/// `File` node for one of those skipped files, genuinely indexing it. This
/// re-derives `files_indexed` and `skipped_by_extension` from the paths that
/// actually received a `File` node, so the reported counts never claim a file
/// was "never indexed" when the graph holds a node for it. The complete
/// (Git-tracked) walk keeps its `files_walked` denominator and the invariant
/// `files_indexed + Σ skipped == files_walked`; the fallback walk (no
/// denominator) reports `files_indexed == files_walked` over the distinct
/// File-node paths without fabricating a skip tally.
pub(crate) fn reconcile_scan_coverage(graph: &Graph, tally: &mut fs::ScanCoverageTally) {
    let indexed_paths: std::collections::BTreeSet<&str> = graph
        .records()
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Node {
                kind: NodeKind::File,
                repo_relative_path: Some(path),
                ..
            } => Some(path.as_str()),
            _ => None,
        })
        .collect();

    if tally.coverage_complete {
        // Re-derive the skip tally from the retained walked-but-skipped paths,
        // excluding any that a File-producing extractor indexed after the walk.
        let mut skipped_by_extension = BTreeMap::new();
        for (path, ext) in &tally.skipped_paths {
            if !indexed_paths.contains(path.as_str()) {
                *skipped_by_extension.entry(ext.clone()).or_default() += 1;
            }
        }
        let skipped_total: usize = skipped_by_extension.values().sum();
        tally.skipped_by_extension = skipped_by_extension;
        // `files_walked` is the fixed denominator; everything not still skipped
        // is indexed, preserving `files_indexed + Σ skipped == files_walked`.
        tally.files_indexed = tally.files_walked - skipped_total;
    } else {
        // The fallback walk enumerated only matching files, so it has no
        // walked/skipped denominator. Count the distinct File-node paths as
        // indexed (this now includes any manifest File nodes) and mirror the
        // best-effort `files_walked == files_indexed` without a skip tally.
        tally.files_indexed = indexed_paths.len();
        tally.files_walked = indexed_paths.len();
    }
}

pub(crate) fn scan_coverage_records(
    repository_id: &str,
    tally: &fs::ScanCoverageTally,
) -> Vec<GraphRecord> {
    let coverage_id = stable_id(&["node", "scan_coverage", repository_id]);
    let payload = ir::ScanCoveragePayload {
        files_walked: tally.files_walked,
        files_indexed: tally.files_indexed,
        skipped_by_extension: tally.skipped_by_extension.clone(),
        indexed_languages: languages::Language::ALL
            .iter()
            .map(|language| language.display_name().to_owned())
            .collect(),
        coverage_complete: tally.coverage_complete,
    };
    let node = GraphRecord::node(
        coverage_id.clone(),
        NodeKind::ScanCoverage,
        None,
        None,
        None,
        format!(
            "Scan coverage: {} files walked, {} indexed",
            tally.files_walked, tally.files_indexed
        ),
    )
    .with_scan_coverage(payload);
    let edge = GraphRecord::edge(
        ir::EdgeLabel::Contains,
        repository_id.to_owned(),
        coverage_id,
        Some("1.0".to_owned()),
        "Repository contains scan coverage summary".to_owned(),
    );
    vec![node, edge]
}

pub(crate) fn scan_source_file_records(
    source_file: &fs::SourceFile,
    repository_id: &str,
) -> Result<(Vec<GraphRecord>, languages::cross_file::FileFacts)> {
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
) -> Result<(Vec<GraphRecord>, languages::cross_file::FileFacts)> {
    let source_lf = source.replace("\r\n", "\n");
    let source = &source_lf;
    let mut graph = Graph::new();
    let repo_relative_path = source_file.repo_relative_path.clone();
    let file_id = stable_id(&["node", "file", repository_id, &repo_relative_path]);
    let language = languages::detect(&repo_relative_path).unwrap_or(languages::Language::Rust);
    let normalized = match language {
        languages::Language::Rust => crate::languages::rust::normalize_file_code(source),
        languages::Language::Python => crate::languages::python::normalize_file_code(source),
        languages::Language::TypeScript => {
            crate::languages::typescript::normalize_file_code(source)
        }
        languages::Language::Go => crate::languages::go::normalize_file_code(source),
    };
    graph.push(GraphRecord::node(
        file_id.clone(),
        NodeKind::File,
        Some(repo_relative_path.clone()),
        None,
        Some(repo_relative_path.clone()),
        format!(
            "{} source file {repo_relative_path}\nSource:\n{normalized}",
            language.display_name()
        ),
    ));
    parser::add_repository_file_edge(&mut graph, repository_id, &file_id);
    let facts =
        parser::extract_source_text(source_file, source, &file_id, repository_id, &mut graph)?;
    Ok((graph.records().to_vec(), facts))
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
            std::path::Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
