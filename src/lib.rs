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
/// Criterion benchmark capture as verification-domain `BenchmarkRun` records
/// (issue #237).
pub mod bench_capture;
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
/// Capture `cargo llvm-cov` runs as citable verification-domain
/// `CoverageReport` records (issue #230).
pub mod coverage_capture;
/// Owning-Cargo-package attribution for code-graph facts (issue #117).
pub mod crate_attribution;
/// Acceptance-criterion verification-coverage census (issue #115).
pub mod criteria_coverage;
/// Local daemon for shared multi-agent store access.
#[cfg(feature = "embedded-aletheiadb")]
pub mod daemon;
/// Decision record generation for user-context candidates.
pub mod decide;
/// Design-doc (ADR/PRD/Plan) markdown importer (issue #149).
pub mod doc_ingest;
/// Semantic enrichment and embedding boundaries.
pub mod embeddings;
/// Error and result types.
pub mod error;
/// Typed evidence write workflows for observations, command evidence, artifacts, and verification.
pub mod evidence;
/// Evidence-link freshness verdicts for agent observations (issue #85).
pub mod evidence_freshness;
/// Store-wide cross-domain evidence-link integrity audit (issue #217).
pub mod evidence_link_audit;
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
/// Persistent sidecar index for `eg query … --graph` lanes (issue #447).
pub mod graph_index;
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
/// Frozen MCP tool I/O contract: versioned schemas for the `eg mcp` tool
/// payloads (issue #194).
#[cfg(feature = "embedded-aletheiadb")]
pub mod mcp_contract;
/// Store-wide agent-memory evidence health sweep (issue #185).
///
/// Per-link `resolves_live`/`drifted`/`dangling` classification plus
/// denormalized-array vs stored-edge integrity violations.
pub mod memory_evidence_health;
/// Agent-memory health report to flag reviewability risk (issue #94).
pub mod memory_health;
/// Agent-memory recall evaluation harness (issue #91).
pub mod memory_recall_eval;
/// Agent-memory record retirement from recall (issue #156).
pub mod memory_retire;
/// Parser orchestration.
pub mod parser;
/// Local setup preflight report for the `eg doctor` command (issue #75).
pub mod preflight;
/// Checked-in per-repo project configuration — `egregore.toml` (issue #261).
pub mod project_config;
/// Capture a local Verus proof run as citable `ProofResult` + `CommandRun`
/// records (issue #69).
pub mod proof_capture;
/// Protected raw-artifact capture and retrieval (issue #60).
pub mod protected;
/// Agent-facing graph query helpers.
pub mod query;
/// Structural query latency budget and scaling regression gate (issue #120).
pub mod query_budget;
/// Cold query-latency budget measurement for time-to-first-symbol-answer (issue #255).
pub mod query_latency;
/// Redaction policy engine (`docs/schema/redaction.md` v1).
pub mod redaction;
/// At-import redaction report (issue #266).
pub mod redaction_report;
/// In-place re-embedding of an `--embed` store under a new local model (issue #167).
#[cfg(feature = "embeddings")]
pub mod reembed;
/// Offline repair workflow for Egregore stores (issue #49).
#[cfg(feature = "embedded-aletheiadb")]
pub mod repair;
/// Whole-repository logical eviction (issue #248).
pub mod repo_evict;
/// Standing citable review-coverage gate over merged PRs (issue #339).
pub mod review_coverage;
/// Commit-time schema-constraint evaluation and declaration (issue #486).
pub mod schema_constraints;
/// JSON Schema (draft 2020-12) export for persisted record contracts (issue #226).
pub mod schema_export;
/// Record schema-version compatibility checks.
pub mod schema_version;
/// SCIP code-intelligence export (issue #233).
pub mod scip;
/// Calibrated confidence floor and abstention for semantic search (issue #263).
pub mod semantic_confidence;
/// Semantic search relevance evaluation harness (issue #58).
pub mod semantic_eval;
/// Write-time supersession / contradiction authoring for `eg write observation` (issue #184).
pub mod supersede_write;
/// Time-to-first-citable-answer product gate (issue #57).
pub mod symbol_latency;
/// Transitive memory supersession and contradiction resolution (issue #92).
pub mod temporal_status;
/// Capture `cargo test` / libtest JSON runs as citable `TestRun` records (issue #165).
pub mod test_capture;
/// Query-answer token-cost measurement against the ripgrep baseline (issue #84).
pub mod token_cost;
/// `rust-swe-agent` `.traj` importer (M2 agent-memory source).
pub mod traj;
/// Pre-ingest referential-integrity validation for graph JSONL (issue #103).
pub mod validate;
/// Evidence-freshness verdicts for verification records (issue #111).
pub mod verification_freshness;
/// Byte-for-byte scan reproducibility comparison (issue #239).
pub mod verify_scan;
/// Transcripts watcher.
pub mod watch;
/// Read-only working-tree diff computation for diff-scoped briefings (issue #214).
pub mod worktree_diff;

use std::{collections::BTreeMap, path::Path, sync::LazyLock};

pub use antigravity::import_antigravity;
pub use claude_code::import_claude_code;
pub use codex::import_codex;
pub use decide::{DecideRequest, decide_candidate};
pub use error::{CodegraphError, Result};
pub use history::{
    HistoryResumeOutcome, HistoryResumePoint, HistoryWindow, history_replay_tip_id,
    history_replay_tip_repository_ids, history_resume_point, scan_repository_history,
    scan_repository_history_resumed, scan_repository_history_with_override,
    scan_repository_history_with_window,
};
pub use ir::{
    AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, CallResolution,
    DependencyDeclarationPayload, Domain, EdgeLabel, EgregoreGit, EmbeddingModel,
    ErrorSignaturePayload, EvidenceLink, Graph, GraphRecord, IdentitySource, LOG_SCHEMA_VERSION,
    LogEventPayload, LogOccurrenceBucketPayload, LogPayload, LogSourcePayload, MetricKind,
    NodeKind, NodeProvenance, PRODUCER_ENVELOPE_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION,
    PatchHandle, Producer, ProducerKind, RepositoryIdentityPayload, SCHEMA_VERSION,
    SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE, SEMANTIC_SCHEMA_VERSION, ScanCoveragePayload,
    SelectionBasis, SemanticDriftMetadata, SnapshotHead, SourceSnapshotPayload, SourceSpan,
    SymbolRole, TemporalMetadata, USER_CONTEXT_SCHEMA_VERSION, UserContextFields, UserContextScope,
    VERIFICATION_SCHEMA_VERSION, agent_memory_stable_id, artifact_stable_id, log_stable_id,
    project_stable_id, semantic_stable_id, stable_id, user_context_stable_id,
    verification_stable_id,
};
pub use local_project::import_local_tasks;
pub use query::{
    ChangesContext, ChangesError, PolicyEntry, PolicyStatus, PublicApiDeltas,
    PublicApiDeltasOptions, RangeDeltas, RangeDeltasError, RepositoryIndex,
    RepositorySelectorError, SubsystemContext, SubsystemPrefixError, SymbolContext, UnresolvedRef,
    active_policy, audit_trail, changes_context, is_candidate_suppressed, path_glob_matches,
    path_is_under_prefix, pending_candidates, policy_for_anchor, policy_scope_applies,
    policy_scope_for_record, public_api_deltas, range_deltas, subsystem_context, symbol_context,
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
    // Capture ONE scan instant: `transaction_time`/`valid_time` stay seconds
    // precision (unchanged), while `coverage_generation` (issue #406) keeps the
    // full nanosecond precision so two same-UTC-second scans are orderable.
    let instant = chrono::Utc::now();
    let now = instant.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let generation = instant.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    scan_repository_at_with_override_inner(
        repo_path,
        &now,
        repo_id_override,
        &[],
        Some(&generation),
        None,
    )
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
    let instant = chrono::Utc::now();
    let now = instant.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let generation = instant.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    scan_repository_at_with_override_inner(
        repo_path,
        &now,
        repo_id_override,
        snapshot_exclusions,
        Some(&generation),
        None,
    )
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
    // Explicit-time entry (fixed-timestamp callers such as tests and audit):
    // derive `coverage_generation` from `transaction_time` for deterministic,
    // byte-identical output across re-runs with the same override (issue #406),
    // and stamp `producer_started_at` with the pinned time so no wall-clock
    // instant leaks into a time-pinned scan (issue #261).
    scan_repository_at_with_override_inner(
        repo_path,
        transaction_time,
        repo_id_override,
        &[],
        None,
        Some(transaction_time),
    )
}

/// Like [`scan_repository_at_with_override`] but excludes repo-relative paths
/// from the dirty probe when stamping the snapshot.
///
/// This is the entry point the `eg scan` CLI uses when `egregore.toml` pins
/// `scan.transaction_time` (issue #261): the pinned instant governs the
/// transaction time, the coverage-generation stamp, and `producer_started_at`,
/// so two runs sharing the checked-in config produce byte-for-byte identical
/// graph JSONL. With no pin the CLI keeps using
/// [`scan_repository_with_exclusions`] (wall-clock), which is unchanged.
///
/// # Errors
///
/// Returns an error when the repository path is missing, is not a directory, or
/// source discovery cannot read the filesystem.
pub fn scan_repository_at_with_exclusions(
    repo_path: impl AsRef<Path>,
    transaction_time: &str,
    repo_id_override: Option<&str>,
    snapshot_exclusions: &[String],
) -> Result<Graph> {
    scan_repository_at_with_override_inner(
        repo_path,
        transaction_time,
        repo_id_override,
        snapshot_exclusions,
        None,
        Some(transaction_time),
    )
}

fn scan_repository_at_with_override_inner(
    repo_path: impl AsRef<Path>,
    transaction_time: &str,
    repo_id_override: Option<&str>,
    snapshot_exclusions: &[String],
    coverage_generation: Option<&str>,
    producer_started_at: Option<&str>,
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
        match scan_source_file_records(&source_file, &repository_id)? {
            SourceFileScanOutcome::Extracted { records, facts } => {
                for record in records {
                    graph.push(record.with_valid_time_inferred(transaction_time));
                }
                if !facts.is_empty() {
                    facts_by_file.insert(source_file.repo_relative_path.clone(), *facts);
                }
            }
            // A non-UTF-8 or unreadable file is skipped (issue #438): record its
            // deterministic diagnostic and thread the skip into the coverage
            // tally so `reconcile_scan_coverage` counts it UNINDEXED.
            SourceFileScanOutcome::Skipped {
                diagnostic,
                repo_relative_path,
                extension,
            } => {
                graph.push((*diagnostic).with_valid_time_inferred(transaction_time));
                coverage_tally.record_unindexed_skip(repo_relative_path, extension);
            }
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
    // Repo-wide macro-invocation resolution (issue #148): each
    // `macro_invocation` site resolves against the repo-wide pool of
    // `macro_rules!` definitions. A unique match mints a resolved `CALLS`
    // edge from the invoking scope to the macro symbol; anything else keeps
    // the pre-existing `unsupported macro invocation` Diagnostic.
    for record in languages::cross_file::cross_file_macro_records(&repository_id, &facts_by_file) {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }
    // Declared Cargo manifests, harvested BEFORE the import-target pass: the
    // pass resolves absolute `<crate_name>::…` imports against owning-package
    // names. Harvesting is a pure function of the repo root, so moving it
    // ahead of the dependency scan changes nothing downstream.
    let manifest_facts = manifest_deps::scan_manifest_package_facts(repo_root)?;
    let attribution = crate_attribution::CrateAttributionIndex::from_facts(manifest_facts);
    // Inbound IMPORTS edges to imported Module/File targets (issue #444):
    // each resolvable Rust `use` mints `File —IMPORTS→ Module|File` from the
    // importing file to the imported module, so "who imports module X" is
    // traversable without `jq` over the JSONL. Fail-closed: unresolvable
    // imports mint no edge.
    for record in languages::cross_file::cross_file_import_target_edges(
        &repository_id,
        graph.records(),
        &facts_by_file,
        &attribution,
    ) {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }
    // Same-file resolution labeling (issue #134): stamp per-file CALLS edges
    // backed by Tree-sitter call sites with the shared resolution status.
    languages::cross_file::label_same_file_call_resolutions(graph.records_mut(), &facts_by_file);
    // Out-of-line `#[cfg(test)] mod x;` test-scope marking (issue #223): the
    // module file is extracted with no view of the gating attribute, so the
    // repo-wide pass rewrites its panic-risk sites to test context.
    languages::cross_file::apply_out_of_line_test_scope(graph.records_mut(), &facts_by_file);
    // Out-of-line `#[cfg(test)] mod x;` File-role stamping (issue #238):
    // the module file's path-derived `Production` role is upgraded to `Test`
    // by the same test-only-module resolution. Roles are re-stamped on every
    // scan, never cached.
    languages::cross_file::apply_out_of_line_test_roles(graph.records_mut(), &facts_by_file);
    // Out-of-line `#[cfg(...)] mod x;` gate propagation (issue #190): the
    // module file is extracted with no view of the gating attribute, so the
    // repo-wide pass prepends each declaration's gate chain to every
    // File/Symbol/Module record of the target file. Recomputed every scan,
    // never cached; the stamping is idempotent.
    languages::cross_file::apply_out_of_line_cfg_gates(graph.records_mut(), &facts_by_file);

    // Declared Cargo dependencies (issue #180): every manifest's directly-
    // declared dependencies become deterministic, citable graph facts joined
    // with the nearest lockfile's resolved versions. This mints a `File` node
    // for each manifest that declares dependencies — a walked file the source
    // filter skipped yet that ends up genuinely indexed.
    for record in manifest_deps::scan_dependency_records(repo_root, &repository_id)? {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }

    // Owning-Cargo-package attribution (issue #117): a post-extraction rewrite
    // that stamps every path-bearing code-graph node with the package owning it,
    // resolved from the NEAREST ENCLOSING `Cargo.toml`. It runs here because it
    // must see EVERY `File`-producing extractor's output — per-file source
    // extraction above and manifest extraction just now — the same ordering
    // constraint `reconcile_scan_coverage` was placed for. Attribution is never
    // an identity input, so no record ID moves. The index was built above for
    // the import-target pass; only the stamping runs here.
    crate_attribution::apply_crate_attribution(graph.records_mut(), &attribution);

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
    // CONTAINS edge so it is citable and never an orphan. `coverage_generation`
    // (issue #406) carries a full-precision recency signal so two scans within
    // one UTC second — which share a seconds-precision `valid_time` — are still
    // deterministically orderable by `eg inspect --graph`. With no explicit
    // override it defaults to `transaction_time`, keeping fixed-time scans (the
    // byte-stability tests) byte-identical.
    let coverage_generation = coverage_generation.unwrap_or(transaction_time);
    for record in scan_coverage_records(&repository_id, &coverage_tally, coverage_generation) {
        graph.push(record.with_valid_time_inferred(transaction_time));
    }

    let languages = languages_in_graph(&graph);
    // A pinned transaction time also pins `producer_started_at` (issue #261):
    // an explicit-time scan is a deterministic operation, so stamping
    // wall-clock here would leak a per-run instant into the JSONL. Wall-clock
    // scans (no pin) keep the historical `PROCESS_STARTED_AT` behavior.
    let mut producer = code_graph_producer(&languages);
    if let Some(pinned) = producer_started_at {
        pinned.clone_into(&mut producer.producer_started_at);
    }
    Ok(graph.stamp_producer(&producer))
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
        // extension-skip denominator. Count the distinct File-node paths as
        // indexed (this now includes any manifest File nodes) and mirror the
        // best-effort `files_walked == files_indexed`.
        tally.files_indexed = indexed_paths.len();
        tally.files_walked = indexed_paths.len();
        // Even without a full denominator, a decode/unreadable skip (issue #438)
        // is a file the walk DID visit but could not index — recorded in
        // `skipped_paths` via `record_unindexed_skip` (the fallback never
        // records extension skips there). Fold each such path that received no
        // `File` node into `skipped_by_extension` (keyed by its recorded
        // lowercased extension, `""` for none) and add it to `files_walked`, so
        // the skip is honestly counted walked + skipped instead of silently
        // vanishing outside the Git-tracked path. Mirror the complete branch's
        // "skipped_paths minus indexed File nodes" subtraction so a path that
        // did receive a `File` node is never double-counted. `coverage_complete`
        // stays `false`: the fallback still lacks an extension-skip denominator.
        let mut skipped_by_extension = BTreeMap::new();
        for (path, ext) in &tally.skipped_paths {
            if !indexed_paths.contains(path.as_str()) {
                *skipped_by_extension.entry(ext.clone()).or_default() += 1;
            }
        }
        let skipped_total: usize = skipped_by_extension.values().sum();
        tally.skipped_by_extension = skipped_by_extension;
        tally.files_walked += skipped_total;
    }
}

pub(crate) fn scan_coverage_records(
    repository_id: &str,
    tally: &fs::ScanCoverageTally,
    coverage_generation: &str,
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
        // Full-precision recency tie-break for same-UTC-second re-scans
        // (issue #406). Non-identity: never part of `coverage_id`.
        coverage_generation: Some(coverage_generation.to_owned()),
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

/// Outcome of attempting to scan one discovered source file (issue #438).
///
/// A file that cannot be decoded as UTF-8, or cannot be read at all, no longer
/// aborts the whole scan: it becomes a [`SourceFileScanOutcome::Skipped`]
/// carrying a deterministic `Diagnostic` node and the accounting the caller
/// threads into scan coverage so the file is honestly counted UNINDEXED.
pub(crate) enum SourceFileScanOutcome {
    /// The file decoded and extracted normally. `facts` is boxed so the
    /// large [`FileFacts`] struct does not bloat the enum variant (the same
    /// boxing the `Skipped` variant already applies to its diagnostic node).
    Extracted {
        records: Vec<GraphRecord>,
        facts: Box<languages::cross_file::FileFacts>,
    },
    /// The file was skipped (non-UTF-8 or unreadable). `diagnostic` names the
    /// repo-relative path and the fixed decode/read-failure reason;
    /// `repo_relative_path`/`extension` reconcile the skip into `ScanCoverage`.
    /// The diagnostic node is boxed so the (common) `Extracted` variant is not
    /// bloated by the large `GraphRecord` node.
    Skipped {
        diagnostic: Box<GraphRecord>,
        repo_relative_path: String,
        extension: String,
    },
}

/// Builds the `Diagnostic` node for a source file skipped during scanning
/// (issue #438).
///
/// The summary is a FIXED, platform-independent string (no raw bytes, no
/// OS-error text) so the graph is byte-stable across runs and machines. The ID
/// keys on the failure class + repository + repo-relative path. `Diagnostic`
/// markers legitimately stand alone (they are exempt from the `eg validate`
/// orphan check, like the extractor's macro diagnostic), so no anchoring edge
/// is minted.
fn skipped_source_diagnostic(
    repository_id: &str,
    repo_relative_path: &str,
    class: &str,
    summary: &str,
) -> GraphRecord {
    let diag_id = stable_id(&[
        "node",
        "diagnostic",
        class,
        repository_id,
        repo_relative_path,
    ]);
    GraphRecord::node(
        diag_id,
        NodeKind::Diagnostic,
        Some(repo_relative_path.to_owned()),
        None,
        Some(class.to_owned()),
        summary.to_owned(),
    )
}

/// Builds the `Skipped` outcome for a source file that cannot be READ at all —
/// an io error such as a permission failure (issue #438).
///
/// Shared by the full-scan path ([`scan_source_file_records`], where the first
/// `std::fs::read` fails) and the incremental refresh path (`incremental.rs`,
/// where the earlier byte-hash read fails before extraction is reached), so both
/// emit the IDENTICAL deterministic `unreadable_source` diagnostic — id shape
/// `["node","diagnostic","unreadable_source",repository_id,repo_relative_path]`,
/// fixed summary — and reconcile scan coverage the same way. Factoring the
/// construction here keeps the two call sites from duplicating it.
pub(crate) fn unreadable_skip_outcome(
    source_file: &fs::SourceFile,
    repository_id: &str,
) -> SourceFileScanOutcome {
    let repo_relative_path = source_file.repo_relative_path.clone();
    let extension = fs::lowercased_extension(&source_file.path);
    SourceFileScanOutcome::Skipped {
        diagnostic: Box::new(skipped_source_diagnostic(
            repository_id,
            &repo_relative_path,
            "unreadable_source",
            "skipped source file: unreadable",
        )),
        repo_relative_path,
        extension,
    }
}

pub(crate) fn scan_source_file_records(
    source_file: &fs::SourceFile,
    repository_id: &str,
) -> Result<SourceFileScanOutcome> {
    let repo_relative_path = source_file.repo_relative_path.clone();
    let extension = fs::lowercased_extension(&source_file.path);
    // Read bytes, then decode: this single path covers BOTH the non-UTF-8 class
    // (a genuine text file in another encoding) and the adjacent unreadable
    // class (permission/io error) — skip and record rather than abort (issue
    // #438).
    let Ok(bytes) = std::fs::read(&source_file.path) else {
        return Ok(unreadable_skip_outcome(source_file, repository_id));
    };
    let Ok(source) = std::str::from_utf8(&bytes) else {
        return Ok(SourceFileScanOutcome::Skipped {
            diagnostic: Box::new(skipped_source_diagnostic(
                repository_id,
                &repo_relative_path,
                "non_utf8_source",
                "skipped source file: not valid UTF-8",
            )),
            repo_relative_path,
            extension,
        });
    };
    let (records, facts) = scan_source_text_records(source_file, source, repository_id)?;
    Ok(SourceFileScanOutcome::Extracted {
        records,
        facts: Box::new(facts),
    })
}

/// Derives the path-signal test-vs-production role for a `File` record
/// (issue #238 signal c): `Test` when the repo-relative path's first segment
/// is `tests` or `benches` (Cargo's integration-test and bench roots),
/// `Production` otherwise. Segment comparison is separator-agnostic, so `\`
/// checkouts classify identically to `/` ones.
fn file_role_for_path(repo_relative_path: &str) -> SymbolRole {
    let is_test_root = languages::common::path_segments(repo_relative_path)
        .first()
        .is_some_and(|segment| segment == "tests" || segment == "benches");
    if is_test_root {
        SymbolRole::Test
    } else {
        SymbolRole::Production
    }
}

/// Whole-file [`SourceSpan`] minted on the `File` node (issue #212): the file's
/// true line count, so positional queries report `line_out_of_range` against
/// the file's actual last line instead of the last recorded symbol span.
///
/// Line counting follows `str::lines` semantics — a trailing newline does not
/// create an extra line, and lone trailing blank lines do count. An empty file
/// yields a degenerate zero-length span with `end_line == 0`, so every
/// positive line is out of range for it. The span covers the LF-normalized
/// source the extractor parses, keeping the count identical across CRLF/LF
/// checkouts (issue #242).
fn whole_file_span(source: &str) -> SourceSpan {
    let mut line_count = 0usize;
    let mut last_line_len = 0usize;
    for line in source.lines() {
        line_count += 1;
        last_line_len = line.len();
    }
    SourceSpan {
        start_byte: 0,
        end_byte: source.len(),
        start_line: 1,
        end_line: line_count,
        start_column: Some(0),
        end_column: Some(last_line_len),
    }
}

pub(crate) fn scan_source_text_records(
    source_file: &fs::SourceFile,
    source: &str,
    repository_id: &str,
) -> Result<(Vec<GraphRecord>, languages::cross_file::FileFacts)> {
    // Normalize line endings at the scan funnel (issue #242): CRLF and lone
    // CR both become LF up front so the File-node summary text below is
    // canonical, and the parse boundary (`extract_file_source`) normalizes
    // idempotently again before Tree-sitter. A CRLF checkout and an LF
    // checkout of the same commit therefore yield byte-identical spans,
    // symbol text, signatures, summaries, and content hashes.
    let source_lf = languages::normalize_line_endings(source);
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
    graph.push(
        GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some(repo_relative_path.clone()),
            Some(whole_file_span(source)),
            Some(repo_relative_path.clone()),
            format!(
                "{} source file {repo_relative_path}\nSource:\n{normalized}",
                language.display_name()
            ),
        )
        // Test-vs-production role, path signal (issue #238 signal c): a file
        // under a top-level `tests/` or `benches/` root is test, everything else
        // production. Out-of-line `#[cfg(test)] mod x;` targets are upgraded to
        // test by the repo-wide `apply_out_of_line_test_roles` pass, which runs
        // after all files are extracted.
        .with_role(file_role_for_path(&repo_relative_path)),
    );
    parser::add_repository_file_edge(&mut graph, repository_id, &file_id);
    let facts =
        parser::extract_source_text(source_file, source, &file_id, repository_id, &mut graph)?;
    // Conditional-compilation gates on the `File` node (issue #190): the
    // extractor exported the file's `#![cfg(...)]` / `#![cfg_attr(...)]`
    // inner-attribute gates on `FileFacts`; stamp them on the File node
    // emitted above so the file's own record carries the same gate every
    // symbol in it inherits. The File node is the first record this funnel
    // pushes, found by its stable id. Additive, never an identity input.
    if !facts.file_cfg_gates.is_empty()
        && let Some(file_record) = graph
            .records_mut()
            .iter_mut()
            .find(|record| record.id() == file_id)
    {
        let stamped = file_record.clone().with_cfg(facts.file_cfg_gates.clone());
        *file_record = stamped;
    }
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

#[cfg(test)]
mod tests {
    use super::{
        GraphRecord, NodeKind, SourceFileScanOutcome, SymbolRole, file_role_for_path,
        fs::SourceFile, scan_source_file_records,
    };

    /// Issue #238: `File` path-signal roles classify `tests/` and `benches/`
    /// roots as test, everything else as production, on either separator.
    #[test]
    fn file_role_for_path_classifies_test_roots() {
        assert_eq!(file_role_for_path("tests/integration.rs"), SymbolRole::Test);
        assert_eq!(file_role_for_path("benches/bench.rs"), SymbolRole::Test);
        assert_eq!(
            file_role_for_path("tests\\integration.rs"),
            SymbolRole::Test
        );
        assert_eq!(file_role_for_path("benches\\bench.rs"), SymbolRole::Test);
        assert_eq!(file_role_for_path("src/lib.rs"), SymbolRole::Production);
        assert_eq!(file_role_for_path("src\\lib.rs"), SymbolRole::Production);
        // A mere prefix is not a root: `testing/` is production.
        assert_eq!(file_role_for_path("testing/foo.rs"), SymbolRole::Production);
    }

    /// Issue #212: the `File` node's whole-file span reports the file's true
    /// line count — `str::lines` semantics, so a trailing newline adds no
    /// extra line, while trailing blank lines do count. An empty file yields
    /// the degenerate zero-length span (`end_line == 0`).
    #[test]
    fn whole_file_span_counts_lines_like_str_lines() {
        let span = super::whole_file_span("a\nb\nc");
        assert_eq!((span.start_line, span.end_line), (1, 3));
        assert_eq!((span.start_byte, span.end_byte), (0, 5));
        assert_eq!(span.end_column, Some(1));

        // Trailing newline: still 3 lines, not 4.
        let span = super::whole_file_span("a\nb\nc\n");
        assert_eq!((span.start_line, span.end_line), (1, 3));
        assert_eq!((span.start_byte, span.end_byte), (0, 6));

        // Trailing blank lines count.
        let span = super::whole_file_span("a\n\n");
        assert_eq!((span.start_line, span.end_line), (1, 2));
        assert_eq!(span.end_column, Some(0));

        // Empty file: degenerate span, every positive line out of range.
        let span = super::whole_file_span("");
        assert_eq!((span.start_line, span.end_line), (1, 0));
        assert_eq!((span.start_byte, span.end_byte), (0, 0));
    }
    /// Issue #438: an unreadable source file (here the reader is pointed at a
    /// directory, so `std::fs::read` returns an io error deterministically and
    /// without permission games) yields a `Skipped` outcome carrying an
    /// `unreadable` diagnostic rather than aborting the scan.
    #[test]
    fn unreadable_source_file_is_skipped_not_aborted() {
        let temp = tempfile::tempdir().expect("temp dir");
        // A directory at the source path: reading it as a file is an io error on
        // every platform (EISDIR on Unix), root-safe and deterministic.
        let dir_as_source = temp.path().join("src");
        std::fs::create_dir_all(&dir_as_source).expect("dir");
        let source_file = SourceFile {
            path: dir_as_source,
            repo_relative_path: "src".to_owned(),
        };

        let outcome = scan_source_file_records(&source_file, "repo:test").expect("must not abort");
        match outcome {
            SourceFileScanOutcome::Skipped {
                diagnostic,
                repo_relative_path,
                ..
            } => {
                assert_eq!(repo_relative_path, "src");
                let GraphRecord::Node { kind, summary, .. } = diagnostic.as_ref() else {
                    panic!("skip diagnostic must be a node");
                };
                assert_eq!(*kind, NodeKind::Diagnostic);
                assert!(
                    summary.contains("unreadable"),
                    "expected an unreadable diagnostic, got: {summary}"
                );
            }
            SourceFileScanOutcome::Extracted { .. } => {
                panic!("an unreadable path must be Skipped, not Extracted")
            }
        }
    }
}
