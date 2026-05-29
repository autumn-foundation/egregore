//! Command-line interface for Egregore.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::{
    adapters::{DryRunSink, ingest_records, records_from_jsonl},
    ir::{EdgeLabel, GraphRecord, NodeKind, SemanticDriftMetadata, SourceSpan},
    query, scan_repository_history_with_override, scan_repository_with_override,
    schema_version::{RecordVersion, record_version},
    traj::{self, ImportOptions},
};

#[cfg(feature = "embedded-aletheiadb")]
use crate::adapters::EmbeddedAletheiaSink;
#[cfg(feature = "embeddings")]
use crate::adapters::SemanticMatch;
#[cfg(feature = "embedded-aletheiadb")]
use crate::daemon::{DaemonClient, DaemonConfig};

#[derive(Debug, Parser)]
#[command(
    name = "egregore",
    about = "Manage agentic SWE knowledge graphs on AletheiaDB"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Scan a repository and write graph JSONL.
    Scan {
        /// Repository path to scan.
        repo_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Override the auto-detected repository identity.
        ///
        /// Forces `identity_source = operator_override`. Use for fixture-stable
        /// tests or when the auto-detected remote is wrong (e.g. a mirror).
        #[arg(long)]
        repo_id_override: Option<String>,
    },
    /// Replay Git history and write temporal graph JSONL.
    ScanHistory {
        /// Git repository path to scan.
        repo_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
        /// Override the auto-detected repository identity.
        ///
        /// Forces `identity_source = operator_override`. Use for fixture-stable
        /// tests or when the auto-detected remote is wrong (e.g. a mirror).
        #[arg(long)]
        repo_id_override: Option<String>,
    },
    /// Inspect a graph JSONL file.
    Inspect {
        /// Graph JSONL path to inspect.
        graph: PathBuf,
    },
    /// Ingest graph JSONL through a storage adapter.
    Ingest {
        /// Graph JSONL path to ingest.
        graph: PathBuf,
        /// Adapter to use for ingestion.
        #[arg(long, default_value = "dry-run")]
        adapter: IngestAdapter,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Agent ID for daemon-backed writes.
        #[arg(long, default_value = "egregore-cli")]
        agent_id: String,
        /// Session ID for daemon-backed writes.
        #[arg(long, default_value = "egregore-cli")]
        session_id: String,
        /// Idempotency key for daemon-backed writes.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Generate and store semantic embeddings for file and symbol nodes.
        #[cfg(feature = "embeddings")]
        #[arg(long)]
        embed: bool,
    },
    /// Import a rust-swe-agent .traj trajectory file into agent-memory JSONL.
    ImportTraj {
        /// Path to the `.traj` trajectory file.
        traj_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Import a Codex session or rollout JSONL into agent-memory JSONL.
    ImportCodex {
        /// Path to the Codex session or rollout JSONL file.
        codex_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Query an existing graph JSONL for symbols, files, or drift records.
    Query {
        /// Query subcommand.
        #[command(subcommand)]
        subcommand: QuerySubcommand,
    },
    /// Manage the local Egregore daemon.
    #[cfg(feature = "embedded-aletheiadb")]
    Daemon {
        /// Daemon action.
        #[command(subcommand)]
        action: DaemonAction,
    },
}

/// Output format for query results.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum)]
enum OutputFormat {
    /// Newline-delimited JSON objects (default, machine-readable).
    #[default]
    Json,
    /// Human-readable one-line-per-result form.
    Text,
}

/// Subcommands for `query`.
#[derive(Debug, Subcommand)]
enum QuerySubcommand {
    /// Find symbol nodes by name.
    Symbol {
        /// Symbol name to look up.
        name: String,
        /// Graph JSONL path (mutually exclusive with --data-dir / --daemon).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Route the query through the running daemon (requires --data-dir, conflicts with --graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
        /// Restrict to the record at this commit SHA or unique prefix.
        /// Shorthand for --as-of keyed by a Git SHA. Mutually exclusive with --as-of.
        #[arg(long, conflicts_with = "as_of")]
        at: Option<String>,
        /// Return the symbol state at the most recent commit at or before this
        /// RFC 3339 instant (valid-time axis). Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        as_of: Option<String>,
        /// Transaction-time selector (reserved, not yet implemented).
        /// Returns a `not_implemented` error envelope rather than silently ignoring the flag.
        #[arg(long)]
        tx_as_of: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// List symbols defined in a file via DEFINES edges.
    File {
        /// Repository-relative file path.
        path: String,
        /// Graph JSONL path (mutually exclusive with --data-dir / --daemon).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Route the query through the running daemon (requires --data-dir, conflicts with --graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Find semantic drift nodes ranked by score descending.
    Drift {
        /// Graph JSONL path (mutually exclusive with --data-dir / --daemon).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Route the query through the running daemon (requires --data-dir, conflicts with --graph).
        #[cfg(feature = "embedded-aletheiadb")]
        #[arg(long, requires = "data_dir", conflicts_with = "graph")]
        daemon: bool,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Find similar code by semantic embedding.
    #[cfg(feature = "embeddings")]
    Semantic {
        /// Query text (symbol name, description, or code snippet).
        query: String,
        /// Embedded `AletheiaDB` data directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Retrieve evidence-backed context for a named symbol.
    ///
    /// Returns a structured JSON object with five trust-separated sections:
    /// `source_facts` (code-graph), `observations` (agent-authored),
    /// `project_state` (tasks/ACs), `artifacts`, and `verification_evidence`.
    /// Missing evidence links are surfaced as `unresolved` items.
    ///
    /// On no-match: emits `{"ok":false,"error":{"code":"no_match",...}}` to
    /// stdout and exits with code 2. No synthesized prose; no hallucinated
    /// fallback records.
    Context {
        /// Symbol name to look up.
        name: String,
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::ValueEnum)]
enum IngestAdapter {
    /// Validate ingest ordering and read-back without writing external storage.
    DryRun,
    /// Write into an embedded `AletheiaDB` store.
    #[cfg(feature = "embedded-aletheiadb")]
    Embedded,
    /// Write through a running local Egregore daemon.
    #[cfg(feature = "embedded-aletheiadb")]
    Daemon,
}

#[cfg(feature = "embedded-aletheiadb")]
#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start the daemon in the background.
    Start {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Loopback host to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port. Use 0 to ask the OS to choose one.
        #[arg(long, default_value_t = 37_383)]
        port: u16,
        /// Bounded write queue capacity.
        #[arg(long, default_value_t = 64)]
        write_queue_capacity: usize,
    },
    /// Run the daemon in the current process.
    #[command(hide = true)]
    Run {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
        /// Loopback host to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port. Use 0 to ask the OS to choose one.
        #[arg(long, default_value_t = 37_383)]
        port: u16,
        /// Bounded write queue capacity.
        #[arg(long, default_value_t = 64)]
        write_queue_capacity: usize,
    },
    /// Report daemon status.
    Status {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
    },
    /// Stop the daemon.
    Stop {
        /// Embedded `AletheiaDB` data directory.
        #[arg(long, default_value = ".egregore")]
        data_dir: PathBuf,
    },
}

/// Parses process arguments and runs the CLI.
///
/// # Errors
///
/// Returns an error if scanning, serialization, file IO, or inspection fails.
pub fn run() -> Result<()> {
    run_cli(Cli::parse())
}

fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Scan {
            repo_path,
            out,
            repo_id_override,
        } => scan(&repo_path, &out, repo_id_override.as_deref()),
        Commands::ScanHistory {
            repo_path,
            out,
            repo_id_override,
        } => scan_history(&repo_path, &out, repo_id_override.as_deref()),
        Commands::Inspect { graph } => inspect(&graph),
        Commands::Ingest {
            graph,
            adapter,
            data_dir,
            agent_id,
            session_id,
            idempotency_key,
            #[cfg(feature = "embeddings")]
            embed,
        } => ingest(
            &graph,
            adapter,
            data_dir.as_deref(),
            &agent_id,
            &session_id,
            idempotency_key.as_deref(),
            #[cfg(feature = "embeddings")]
            embed,
        ),
        Commands::ImportTraj { traj_path, out } => import_traj_cmd(&traj_path, &out),
        Commands::ImportCodex { codex_path, out } => import_codex_cmd(&codex_path, &out),
        Commands::Query { subcommand } => query_cmd(subcommand),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Daemon { action } => daemon(action),
    }
}

fn import_codex_cmd(codex_path: &Path, out: &Path) -> Result<()> {
    let opts = crate::codex::ImportOptions::default();
    let graph = crate::codex::import_codex(codex_path, &opts)
        .with_context(|| format!("failed to import Codex JSONL from {}", codex_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        codex_path.display()
    );
    Ok(())
}

fn import_traj_cmd(traj_path: &Path, out: &Path) -> Result<()> {
    let opts = ImportOptions::default();
    let graph = traj::import_traj(traj_path, &opts)
        .with_context(|| format!("failed to import .traj from {}", traj_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize agent-memory JSONL")?;
    fs::write(out, jsonl).with_context(|| format!("failed to write JSONL to {}", out.display()))?;
    println!(
        "imported {} records from {}",
        graph.records().len(),
        traj_path.display()
    );
    Ok(())
}

fn scan(repo_path: &Path, out: &Path, repo_id_override: Option<&str>) -> Result<()> {
    let graph = scan_repository_with_override(repo_path, repo_id_override)
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write graph JSONL to {}", out.display()))?;
    Ok(())
}

fn scan_history(repo_path: &Path, out: &Path, repo_id_override: Option<&str>) -> Result<()> {
    let graph = scan_repository_history_with_override(repo_path, repo_id_override)
        .with_context(|| format!("failed to scan Git history for {}", repo_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize history graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write history graph JSONL to {}", out.display()))?;
    Ok(())
}

fn inspect(graph: &Path) -> Result<()> {
    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    let counts = InspectCounts::from_jsonl(&jsonl)?;
    println!("records: {}", counts.records);
    println!("nodes: {}", counts.nodes);
    println!("edges: {}", counts.edges);
    println!("tombstones: {}", counts.tombstones);
    println!("diagnostics: {}", counts.diagnostics);
    for (version, count) in &counts.schema_versions {
        println!("schema_version {version}: {count}");
    }
    for (version, count) in &counts.unknown_schema_versions {
        println!("unknown_schema_version {version}: {count}");
    }
    for repo in &counts.repositories {
        println!("repository: {} ({})", repo.id, repo.identity_summary);
    }
    for (kind, count) in &counts.producer_kinds {
        println!("producer_kind {kind}: {count}");
    }
    for (version, count) in &counts.egregore_versions {
        println!("egregore_version {version}: {count}");
    }
    Ok(())
}

fn ingest(
    graph: &Path,
    adapter: IngestAdapter,
    data_dir: Option<&Path>,
    agent_id: &str,
    session_id: &str,
    idempotency_key: Option<&str>,
    #[cfg(feature = "embeddings")] embed: bool,
) -> Result<()> {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    let _ = (data_dir, agent_id, session_id, idempotency_key);

    #[cfg(feature = "embeddings")]
    if embed && adapter != IngestAdapter::Embedded {
        anyhow::bail!("--embed requires --adapter embedded");
    }

    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    let records = records_from_jsonl(&jsonl).context("failed to parse graph JSONL")?;

    let report = match adapter {
        IngestAdapter::DryRun => {
            let mut sink = DryRunSink::default();
            ingest_records(&records, &mut sink)
        }
        #[cfg(feature = "embedded-aletheiadb")]
        IngestAdapter::Embedded => {
            let data_dir = data_dir.map_or_else(|| PathBuf::from(".egregore"), Path::to_path_buf);
            #[cfg(feature = "embeddings")]
            let mut sink = if embed {
                let (vectors, dimensions) = generate_embeddings(&records)?;
                EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, dimensions)
                    .with_context(|| {
                        format!("failed to open embedded store {}", data_dir.display())
                    })?
            } else {
                EmbeddedAletheiaSink::open(&data_dir).with_context(|| {
                    format!("failed to open embedded store {}", data_dir.display())
                })?
            };
            #[cfg(not(feature = "embeddings"))]
            let mut sink = EmbeddedAletheiaSink::open(&data_dir)
                .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
            let report = ingest_records(&records, &mut sink);
            if report.is_success() {
                sink.persist_indexes().with_context(|| {
                    format!("failed to persist embedded store {}", data_dir.display())
                })?;
            }
            report
        }
        #[cfg(feature = "embedded-aletheiadb")]
        IngestAdapter::Daemon => {
            let data_dir = data_dir.map_or_else(|| PathBuf::from(".egregore"), Path::to_path_buf);
            let idempotency_key =
                idempotency_key.context("--idempotency-key is required for --adapter daemon")?;
            let client = DaemonClient::from_data_dir(&data_dir)
                .with_context(|| format!("failed to load daemon for {}", data_dir.display()))?;
            let response =
                client.ingest_records(&records, agent_id, session_id, idempotency_key)?;
            println!("attempted: {}", response.attempted);
            println!("succeeded: {}", response.succeeded);
            println!("failed: {}", response.failed);
            println!("idempotent: {}", response.idempotent);
            if response.failed == 0 {
                return Ok(());
            }
            for failure in &response.failures {
                eprintln!("{}: {}", failure.record_id, failure.message);
            }
            anyhow::bail!("ingest failed for {} records", response.failed);
        }
    };

    println!("attempted: {}", report.attempted);
    println!("succeeded: {}", report.succeeded);
    println!("failed: {}", report.failed);

    if report.is_success() {
        Ok(())
    } else {
        for failure in &report.failures {
            eprintln!("{}: {}", failure.record_id, failure.message);
        }
        anyhow::bail!("ingest failed for {} records", report.failed);
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn daemon(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start {
            data_dir,
            host,
            port,
            write_queue_capacity,
        } => {
            let mut config = DaemonConfig::new(data_dir);
            config.host = host;
            config.port = port;
            config.write_queue_capacity = write_queue_capacity;
            let metadata = crate::daemon::start_background(&config)?;
            println!("daemon started at {}", metadata.address);
            Ok(())
        }
        DaemonAction::Run {
            data_dir,
            host,
            port,
            write_queue_capacity,
        } => {
            let mut config = DaemonConfig::new(data_dir);
            config.host = host;
            config.port = port;
            config.write_queue_capacity = write_queue_capacity;
            crate::daemon::run_foreground(&config)
        }
        DaemonAction::Status { data_dir } => {
            if let Some(metadata) = crate::daemon::active_metadata(&data_dir)? {
                println!("daemon running at {}", metadata.address);
                Ok(())
            } else {
                anyhow::bail!("daemon not running for {}", data_dir.display());
            }
        }
        DaemonAction::Stop { data_dir } => {
            crate::daemon::stop(&data_dir)?;
            println!("daemon stopped");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Query output types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SymbolResult<'a> {
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    kind: &'static str,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
}

#[derive(Serialize)]
struct DriftResult<'a> {
    record_id: &'a str,
    schema_version: u32,
    before_commit: &'a str,
    after_commit: &'a str,
    before_valid_time: &'a str,
    after_valid_time: &'a str,
    embedding_model_provider: &'a str,
    embedding_model_name: &'a str,
    embedding_model_version: &'a str,
    embedding_model_dim: u32,
    embedding_model_content_hash: &'a str,
    metric_kind: &'static str,
    prior_record_id: &'a str,
    target_record_id: &'a str,
    score: f64,
    selection_threshold: f64,
    selection_basis: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

/// Output row for a semantic similarity result.
#[cfg(feature = "embeddings")]
#[derive(Serialize)]
struct SemanticResult<'a> {
    record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
}

#[cfg(feature = "embeddings")]
impl<'a> From<&'a SemanticMatch> for SemanticResult<'a> {
    fn from(m: &'a SemanticMatch) -> Self {
        Self {
            record_id: &m.record_id,
            name: m.name.as_deref(),
            repo_relative_path: m.repo_relative_path.as_deref(),
            score: m.score,
            span: m.span,
        }
    }
}

// ---------------------------------------------------------------------------
// Context query output types (issue #38)
// ---------------------------------------------------------------------------

/// One item in the `source_facts` section of a context query result.
///
/// Every field that was present on the source record is forwarded directly so
/// the output is fully citable. Per AC2 from issue #38, every item must
/// include `record_id` plus at least one of `repo_relative_path`,
/// `git_commit`, or `valid_time`.
#[derive(Serialize)]
struct ContextSourceFact<'a> {
    record_id: &'a str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
}

/// One item in the `observations` section.
///
/// Per AC3 from issue #38, every observation must include `record_id`,
/// `provenance_handle` (or `agent_id`/`session_id`), `observed_at`,
/// `confidence`, and the evidence links.
#[derive(Serialize)]
struct ContextObservation<'a> {
    record_id: &'a str,
    /// Node kind (`"Observation"`, `"Decision"`, or `"Failure"`).
    /// Lets consumers distinguish subjective observation types without
    /// re-inspecting the raw graph.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    /// Provenance handle composed from `agent_id:session_id` when both are present.
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<&'a str>,
    /// Supporting evidence links from the observation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    evidence_links: Vec<&'a crate::ir::EvidenceLink>,
}

/// One item in the `project_state`, `artifacts`, or `verification_evidence` sections.
#[derive(Serialize)]
struct ContextLinkedItem<'a> {
    record_id: &'a str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    /// Human-readable criterion text for `AcceptanceCriterion` nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_kind: Option<&'a str>,
    /// Captured stdout from a `CommandRun` / `TestRun` (hash + optional inline).
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout_handle: Option<&'a crate::ir::OutputHandle>,
    /// Captured stderr from a `CommandRun` / `TestRun`.
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_handle: Option<&'a crate::ir::OutputHandle>,
    /// Repo-relative path to the script, config, or CI definition that was run.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_path: Option<&'a str>,
    /// BLAKE3 hex of the artefact at `source_artifact_path` at run time.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_artifact_hash: Option<&'a str>,
    /// File path for `FileEdit` nodes (the file that was edited).
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    /// Edit operation for `FileEdit` nodes (e.g. `"modify"`, `"rename"`, `"delete"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    edit_kind: Option<&'a str>,
    /// BLAKE3 hash of the file content before the edit (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    before_hash: Option<&'a str>,
    /// BLAKE3 hash of the file content after the edit (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    after_hash: Option<&'a str>,
    /// New path when the file was renamed (`FileEdit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    rename_to: Option<&'a str>,
    // ── PatchArtifact-specific fields ────────────────────────────────────────
    /// Patch validation status (`"valid"`, `"invalid"`, `"pending"`) for `PatchArtifact`.
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_status: Option<&'a str>,
    /// Storage handle for raw patch bytes (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_handle: Option<&'a crate::ir::PatchHandle>,
    /// BLAKE3 hash of the raw patch bytes (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_bytes_hash: Option<&'a str>,
    /// Repo-relative file paths touched by the patch (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    target_files: Option<&'a [String]>,
    /// Human-readable validation summary, redacted by policy (`PatchArtifact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    validation_summary: Option<&'a str>,
    /// Evidence links that connect this item to the queried symbol.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    evidence_links: Vec<&'a crate::ir::EvidenceLink>,
}

/// One unresolved evidence link target, surfaced per AC5.
#[derive(Serialize)]
struct ContextUnresolved<'a> {
    source_record_id: &'a str,
    target_handle: &'a str,
    relation: &'a str,
    target_domain: &'a str,
    verification_status: &'static str,
}

/// One codegraph topology edge in the context response.
#[derive(Serialize)]
struct ContextTopologyEdge<'a> {
    record_id: &'a str,
    label: &'static str,
    source_id: &'a str,
    target_id: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    summary: &'a str,
}

/// Full context query response envelope.
#[derive(Serialize)]
struct ContextResponse<'a> {
    ok: bool,
    symbol_name: &'a str,
    source_facts: Vec<ContextSourceFact<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<ContextTopologyEdge<'a>>,
    observations: Vec<ContextObservation<'a>>,
    project_state: Vec<ContextLinkedItem<'a>>,
    artifacts: Vec<ContextLinkedItem<'a>>,
    verification_evidence: Vec<ContextLinkedItem<'a>>,
    unresolved: Vec<ContextUnresolved<'a>>,
}

// ---------------------------------------------------------------------------
// query_cmd — dispatch
// ---------------------------------------------------------------------------

fn query_cmd(subcommand: QuerySubcommand) -> Result<()> {
    match subcommand {
        QuerySubcommand::Symbol {
            name,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            at,
            as_of,
            tx_as_of,
            format,
        } => {
            if tx_as_of.is_some() {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "not_implemented",
                        "message": "--tx-as-of: transaction-time queries are reserved and not yet \
                                    implemented for JSONL queries; see docs/schema/temporal-selectors.md"
                    }
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(1);
            }
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_symbol_via_daemon(
                    &name,
                    dir,
                    at.as_deref(),
                    as_of.as_deref(),
                    format,
                );
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            as_of.map_or_else(
                || {
                    at.map_or_else(
                        || query_symbol_all(&records, &name, format),
                        |prefix| query_symbol_at(&records, &name, &prefix, format),
                    )
                },
                |instant| query_symbol_as_of(&records, &name, &instant, format),
            )
        }
        QuerySubcommand::File {
            path,
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            format,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_file_via_daemon(&path, dir, format);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_file(&records, &path, format)
        }
        QuerySubcommand::Drift {
            graph,
            data_dir,
            #[cfg(feature = "embedded-aletheiadb")]
            daemon,
            limit,
            format,
        } => {
            #[cfg(feature = "embedded-aletheiadb")]
            if daemon {
                let dir = data_dir
                    .as_deref()
                    .expect("clap requires --data-dir with --daemon");
                return query_drift_via_daemon(dir, limit, format);
            }
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_drift(&records, limit, format)
        }
        #[cfg(feature = "embeddings")]
        QuerySubcommand::Semantic {
            query,
            data_dir,
            limit,
            format,
        } => query_semantic(&query, &data_dir, limit, format),
        QuerySubcommand::Context {
            name,
            graph,
            data_dir,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_context_cmd(&records, &name)
        }
    }
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_symbol_via_daemon(
    name: &str,
    data_dir: &Path,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let (verb, params) = at.map_or_else(
        || ("symbol_by_name", serde_json::json!({ "name": name })),
        |commit| {
            (
                "symbol_at_commit",
                serde_json::json!({ "name": name, "commit": commit }),
            )
        },
    );
    let records = client.query_verb(verb, &params, as_of)?;
    if records.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_symbol_record(rec, format)?;
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_file_via_daemon(path: &str, data_dir: &Path, format: OutputFormat) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let params = serde_json::json!({ "repo_relative_path": path });
    let records = client.query_verb("file_defines", &params, None)?;
    if records.is_empty() {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_symbol_record(rec, format)?;
    }
    Ok(())
}

#[cfg(feature = "embedded-aletheiadb")]
fn query_drift_via_daemon(data_dir: &Path, limit: usize, format: OutputFormat) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let params = serde_json::json!({ "limit": limit as u64 });
    let records = client.query_verb("drift_top_n", &params, None)?;
    if records.is_empty() {
        eprintln!("error: no match found — no SemanticDrift nodes in graph");
        std::process::exit(2);
    }
    for rec in &records {
        print_daemon_drift_record(rec, format)?;
    }
    Ok(())
}

/// Prints a daemon symbol/file record (`serde_json::Value`) in the requested format.
#[cfg(feature = "embedded-aletheiadb")]
fn print_daemon_symbol_record(rec: &serde_json::Value, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let name = rec["name"].as_str().unwrap_or("(unknown)");
            let kind = rec["kind"].as_str().unwrap_or("Symbol");
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            let line = rec["span"]["start_line"].as_u64().unwrap_or(0);
            let commit = rec["git_commit"]
                .as_str()
                .map_or(String::new(), |c| format!(" [{c}]"));
            println!("{name} ({kind}) @ {path}:{line}{commit}");
        }
    }
    Ok(())
}

/// Prints a daemon drift record (`serde_json::Value`) in the requested format.
#[cfg(feature = "embedded-aletheiadb")]
fn print_daemon_drift_record(rec: &serde_json::Value, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let name = rec["name"].as_str().unwrap_or("(unknown)");
            // score may be a JSON string ("0.92") or a JSON number (0.92);
            // accept both so text output is correct regardless of serialisation path.
            let score_owned = rec["score"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| rec["score"].as_f64().map(|f| f.to_string()))
                .unwrap_or_else(|| "?".to_owned());
            let score = score_owned.as_str();
            let before = rec["before_commit"].as_str().unwrap_or("?");
            let after = rec["after_commit"].as_str().unwrap_or("?");
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            println!("{name} score={score} {before}..{after} @ {path}");
        }
    }
    Ok(())
}

fn load_query_records(graph: Option<&Path>, data_dir: Option<&Path>) -> Result<Vec<GraphRecord>> {
    match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path),
        (None, Some(dir)) => load_records_from_db(dir),
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    }
}

fn load_records_from_jsonl(graph: &Path) -> Result<Vec<GraphRecord>> {
    let jsonl = fs::read_to_string(graph)
        .with_context(|| format!("failed to read graph JSONL from {}", graph.display()))?;
    crate::adapters::records_from_jsonl(&jsonl)
        .map_err(|e| anyhow::anyhow!("failed to parse graph JSONL: {e}"))
}

#[cfg(feature = "embedded-aletheiadb")]
fn validate_existing_embedded_store(data_dir: &Path) -> Result<()> {
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

fn load_records_from_db(data_dir: &Path) -> Result<Vec<GraphRecord>> {
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

// ---------------------------------------------------------------------------
// Embedding helpers
// ---------------------------------------------------------------------------

/// Generates dense embeddings for all file and symbol candidates in `records`.
///
/// Returns a `(record_id → vector, dimension)` map ready for
/// `EmbeddedAletheiaSink::open_with_embeddings`.
#[cfg(feature = "embeddings")]
fn generate_embeddings(
    records: &[GraphRecord],
) -> Result<(crate::embeddings::EmbeddingVectorMap, usize)> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        DEFAULT_EMBEDDING_MODEL_NAME, EmbeddingVectorKey, EmbeddingVectorMap, aletheia_embeddings,
        embedding_candidates,
    };

    let candidates = embedding_candidates(records);
    if candidates.is_empty() {
        return Ok((
            EmbeddingVectorMap::new(),
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS,
        ));
    }

    eprintln!(
        "Generating embeddings for {} file/symbol nodes…",
        candidates.len()
    );

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let embed_data = rt
        .block_on(aletheia_embeddings::embed_query(&texts, &embedder, None))
        .context("embedding generation failed")?;

    let dense: Vec<Vec<f32>> = aletheia_embeddings::embed_data_to_dense_iter(embed_data, None)
        .collect::<Result<Vec<_>, _>>()
        .context("embedding result was not dense")?
        .into_iter()
        .map(|d| d.embedding)
        .collect();

    let dimensions = dense.first().map_or(0, Vec::len);
    if dimensions == 0 {
        anyhow::bail!("embedding model returned zero-dimension vectors");
    }
    let map = candidates
        .into_iter()
        .zip(dense)
        .map(|(candidate, vector)| (EmbeddingVectorKey::from_candidate(&candidate), vector))
        .collect();

    Ok((map, dimensions))
}

/// Semantic similarity search against an embedded store.
#[cfg(feature = "embeddings")]
fn query_semantic(query: &str, data_dir: &Path, limit: usize, format: OutputFormat) -> Result<()> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };

    validate_existing_embedded_store(data_dir)?;

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let embed_data = rt
        .block_on(aletheia_embeddings::embed_query(&[query], &embedder, None))
        .context("failed to embed query")?;

    let query_vector = aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1))
        .next()
        .context("no embedding returned for query")?
        .context("embedding result was not dense")?
        .embedding;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let matches = sink
        .semantic_search(&query_vector, limit)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;

    if matches.is_empty() {
        eprintln!("no results — store may not have embeddings (re-run ingest with --embed)");
        std::process::exit(2);
    }

    for m in matches {
        print_result(&SemanticResult::from(&m), format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tombstone helpers
// ---------------------------------------------------------------------------

/// Returns the set of record IDs that have been tombstoned and not superseded.
/// Used to exclude deleted records from current-state queries (but not --at queries).
fn current_deleted_ids(records: &[GraphRecord]) -> std::collections::BTreeSet<&str> {
    let mut deleted: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for record in records {
        if let GraphRecord::Tombstone { deleted_id, .. } = record {
            deleted.insert(deleted_id.as_str());
        }
    }
    deleted
}

// ---------------------------------------------------------------------------
// query symbol (all matching)
// ---------------------------------------------------------------------------

fn query_symbol_all(records: &[GraphRecord], name: &str, format: OutputFormat) -> Result<()> {
    let deleted = current_deleted_ids(records);
    let mut results: Vec<SymbolResult<'_>> = records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                id, temporal: None, ..
            } = r
            {
                !deleted.contains(id.as_str())
            } else {
                true
            }
        })
        .filter_map(|r| symbol_result(r, name))
        .collect();

    if results.is_empty() {
        eprintln!("error: no match found for symbol `{name}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

fn symbol_result<'a>(record: &'a GraphRecord, name: &str) -> Option<SymbolResult<'a>> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Symbol,
        schema_version,
        name: node_name,
        repo_relative_path,
        span,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    if node_name.as_deref() != Some(name) {
        return None;
    }
    Some(SymbolResult {
        record_id: id,
        schema_version: *schema_version,
        name: node_name.as_deref().unwrap_or(""),
        kind: "Symbol",
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
    })
}

// ---------------------------------------------------------------------------
// query symbol --as-of <instant>
// ---------------------------------------------------------------------------

fn query_symbol_as_of(
    records: &[GraphRecord],
    name: &str,
    as_of: &str,
    format: OutputFormat,
) -> Result<()> {
    match query::symbol_as_of_valid_time(records, name, as_of) {
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
        Ok(None) => {
            eprintln!("error: no match found for symbol `{name}` at or before `{as_of}`");
            std::process::exit(2);
        }
        Ok(Some(record)) => {
            if let Some(result) = symbol_result(record, name) {
                print_result(&result, format)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query symbol --at <commit>
// ---------------------------------------------------------------------------

fn query_symbol_at(
    records: &[GraphRecord],
    name: &str,
    prefix: &str,
    format: OutputFormat,
) -> Result<()> {
    let matching_commits: std::collections::BTreeSet<&str> = records
        .iter()
        .filter_map(|r| temporal_commit_if_prefix(r, prefix))
        .collect();

    if matching_commits.len() > 1 {
        eprintln!(
            "error: ambiguous commit prefix `{prefix}` matches {} commits",
            matching_commits.len()
        );
        std::process::exit(1);
    }

    match query::symbol_at_commit(records, name, prefix) {
        None => {
            eprintln!("error: no match found for symbol `{name}` at commit `{prefix}`");
            std::process::exit(2);
        }
        Some(record) => {
            if let Some(result) = symbol_result(record, name) {
                print_result(&result, format)?;
            }
        }
    }
    Ok(())
}

fn temporal_commit_if_prefix<'a>(record: &'a GraphRecord, prefix: &str) -> Option<&'a str> {
    let commit = match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        }
        | GraphRecord::Edge {
            temporal: Some(t), ..
        } => t.git_commit.as_str(),
        _ => return None,
    };
    if commit.starts_with(prefix) {
        Some(commit)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// query file
// ---------------------------------------------------------------------------

fn query_file(records: &[GraphRecord], path: &str, format: OutputFormat) -> Result<()> {
    let deleted = current_deleted_ids(records);

    let file_exists = records.iter().any(|r| {
        let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path,
            ..
        } = r
        else {
            return false;
        };
        repo_relative_path.as_deref() == Some(path) && !deleted.contains(id.as_str())
    });

    if !file_exists {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }

    let mut results: Vec<SymbolResult<'_>> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                schema_version,
                name,
                repo_relative_path,
                span,
                temporal,
                ..
            } = r
            else {
                return None;
            };
            if repo_relative_path.as_deref() != Some(path) {
                return None;
            }
            if temporal.is_none() && deleted.contains(id.as_str()) {
                return None;
            }
            Some(SymbolResult {
                record_id: id,
                schema_version: *schema_version,
                name: name.as_deref().unwrap_or(""),
                kind: "Symbol",
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            })
        })
        .collect();

    if results.is_empty() {
        eprintln!("error: no match found for file `{path}`");
        std::process::exit(2);
    }

    results.sort_by_key(|r| (r.span.map(|s| s.start_line), r.record_id));
    for result in &results {
        print_result(result, format)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query drift
// ---------------------------------------------------------------------------

fn query_drift(records: &[GraphRecord], limit: usize, format: OutputFormat) -> Result<()> {
    let drifts = query::largest_semantic_drifts(records, limit);

    if drifts.is_empty() {
        eprintln!("error: no match found — no SemanticDrift nodes in graph");
        std::process::exit(2);
    }

    for record in drifts {
        let GraphRecord::Node {
            id,
            schema_version,
            semantic_drift: Some(drift),
            repo_relative_path: drift_path,
            name: drift_name,
            ..
        } = record
        else {
            continue;
        };

        let (resolved_path, resolved_name) = resolve_drift_target(
            records,
            id,
            drift,
            drift_path.as_deref(),
            drift_name.as_deref(),
        );

        let result = DriftResult {
            record_id: id,
            schema_version: *schema_version,
            before_commit: &drift.before_git_commit,
            after_commit: &drift.after_git_commit,
            before_valid_time: &drift.before_valid_time,
            after_valid_time: &drift.after_valid_time,
            embedding_model_provider: &drift.embedding_model.provider,
            embedding_model_name: &drift.embedding_model.name,
            embedding_model_version: &drift.embedding_model.version,
            embedding_model_dim: drift.embedding_model.dim,
            embedding_model_content_hash: &drift.embedding_model.content_hash,
            metric_kind: drift.metric_kind.as_str(),
            prior_record_id: &drift.prior_record_id,
            target_record_id: &drift.target_record_id,
            score: drift.score,
            selection_threshold: drift.selection_threshold,
            selection_basis: drift.selection_basis.as_str(),
            repo_relative_path: resolved_path,
            name: resolved_name,
        };
        print_result(&result, format)?;
    }
    Ok(())
}

fn resolve_drift_target<'a>(
    records: &'a [GraphRecord],
    drift_id: &str,
    drift: &'a SemanticDriftMetadata,
    drift_path: Option<&'a str>,
    drift_name: Option<&'a str>,
) -> (Option<&'a str>, Option<&'a str>) {
    // Follow DriftsFrom edge first (stable contract per CLI docs); fall back to
    // target_record_id when no edge is present in this slice.
    let target_id = records
        .iter()
        .find_map(|r| {
            let GraphRecord::Edge {
                label: EdgeLabel::DriftsFrom,
                source,
                target,
                ..
            } = r
            else {
                return None;
            };
            if source == drift_id {
                Some(target.as_str())
            } else {
                None
            }
        })
        .unwrap_or(drift.target_record_id.as_str());

    if let Some(GraphRecord::Node {
        repo_relative_path,
        name,
        ..
    }) = records.iter().find(|r| r.id() == target_id)
    {
        return (repo_relative_path.as_deref(), name.as_deref());
    }
    (drift_path, drift_name)
}

// ---------------------------------------------------------------------------
// query context (issue #38)
// ---------------------------------------------------------------------------

fn query_context_cmd(records: &[GraphRecord], symbol_name: &str) -> Result<()> {
    let ctx = query::symbol_context(records, symbol_name);

    if ctx.is_no_match() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "symbol_name": symbol_name
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let source_facts: Vec<ContextSourceFact<'_>> = ctx
        .source_facts
        .iter()
        .filter_map(|r| context_source_fact(r))
        .collect();

    let observations: Vec<ContextObservation<'_>> = ctx
        .observations
        .iter()
        .filter_map(|r| context_observation(r))
        .collect();

    let project_state: Vec<ContextLinkedItem<'_>> = ctx
        .project_state
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let artifacts: Vec<ContextLinkedItem<'_>> = ctx
        .artifacts
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let verification_evidence: Vec<ContextLinkedItem<'_>> = ctx
        .verification_evidence
        .iter()
        .filter_map(|r| context_linked_item(r))
        .collect();

    let unresolved: Vec<ContextUnresolved<'_>> = ctx
        .unresolved
        .iter()
        .map(|u| ContextUnresolved {
            source_record_id: &u.source_record_id,
            target_handle: &u.target_handle,
            relation: &u.relation,
            target_domain: &u.target_domain,
            verification_status: "unresolved",
        })
        .collect();

    let topology_edges: Vec<ContextTopologyEdge<'_>> = ctx
        .topology_edges
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Edge {
                id,
                label,
                source,
                target,
                summary,
                ..
            } = r
            {
                Some(ContextTopologyEdge {
                    record_id: id,
                    label: label.as_str(),
                    source_id: source,
                    target_id: target,
                    summary,
                })
            } else {
                None
            }
        })
        .collect();

    let response = ContextResponse {
        ok: true,
        symbol_name,
        source_facts,
        topology_edges,
        observations,
        project_state,
        artifacts,
        verification_evidence,
        unresolved,
    };

    let output = serde_json::to_string_pretty(&response).context("failed to serialize context")?;
    println!("{output}");
    Ok(())
}

fn context_source_fact(record: &GraphRecord) -> Option<ContextSourceFact<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        repo_relative_path,
        span,
        temporal,
        valid_time,
        language,
        symbol_kind,
        ..
    } = record
    else {
        return None;
    };
    Some(ContextSourceFact {
        record_id: id,
        kind: kind.as_str(),
        name: name.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        // For current-tree records valid_time is at the node level; for
        // scan-history records it is in temporal.valid_time. Prefer the
        // node-level field and fall back to the temporal block.
        valid_time: valid_time
            .as_deref()
            .or_else(|| temporal.as_ref().map(|t| t.valid_time.as_str())),
        language: language.as_deref(),
        symbol_kind: symbol_kind.as_deref(),
    })
}

fn context_observation(record: &GraphRecord) -> Option<ContextObservation<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        text,
        agent_id,
        session_id,
        observed_at,
        confidence,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    Some(ContextObservation {
        record_id: id,
        kind: kind.as_str(),
        text: text.as_deref(),
        provenance_handle,
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
    })
}

fn context_linked_item(record: &GraphRecord) -> Option<ContextLinkedItem<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        text,
        status,
        verification_kind,
        stdout_handle,
        stderr_handle,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
        edit_kind,
        before_hash,
        after_hash,
        rename_to,
        patch_status,
        patch_handle,
        patch_bytes_hash,
        target_files,
        validation_summary,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    Some(ContextLinkedItem {
        record_id: id,
        kind: kind.as_str(),
        title: title.as_deref(),
        name: name.as_deref(),
        text: text.as_deref(),
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        stdout_handle: stdout_handle.as_deref(),
        stderr_handle: stderr_handle.as_deref(),
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        edit_kind: edit_kind.as_deref(),
        before_hash: before_hash.as_deref(),
        after_hash: after_hash.as_deref(),
        rename_to: rename_to.as_deref(),
        patch_status: patch_status.as_deref(),
        patch_handle: patch_handle.as_deref(),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        target_files: target_files.as_deref(),
        validation_summary: validation_summary.as_deref(),
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
    })
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn print_result<T: Serialize + PrintText>(result: &T, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let line = serde_json::to_string(result).context("failed to serialize query result")?;
            println!("{line}");
        }
        OutputFormat::Text => {
            println!("{}", result.as_text());
        }
    }
    Ok(())
}

trait PrintText {
    fn as_text(&self) -> String;
}

impl PrintText for SymbolResult<'_> {
    fn as_text(&self) -> String {
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        let commit = self.git_commit.map_or(String::new(), |c| format!(" [{c}]"));
        format!("{} ({}) @ {path}:{line}{commit}", self.name, self.kind)
    }
}

impl PrintText for DriftResult<'_> {
    fn as_text(&self) -> String {
        let name = self.name.unwrap_or("(unknown)");
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        format!(
            "{name} score={:.6} {}..{} @ {path}",
            self.score, self.before_commit, self.after_commit
        )
    }
}

#[cfg(feature = "embeddings")]
impl PrintText for SemanticResult<'_> {
    fn as_text(&self) -> String {
        let name = self.name.unwrap_or("(unknown)");
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        format!("{name} score={:.4} @ {path}:{line}", self.score)
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RepositorySummary {
    id: String,
    identity_summary: String,
}

#[derive(Debug, Default)]
struct InspectCounts {
    records: usize,
    nodes: usize,
    edges: usize,
    tombstones: usize,
    diagnostics: usize,
    schema_versions: BTreeMap<RecordVersion, usize>,
    unknown_schema_versions: BTreeMap<RecordVersion, usize>,
    repositories: Vec<RepositorySummary>,
    /// Per-`producer_kind` breakdown; legacy records use key `"legacy_pre_v1"`.
    producer_kinds: BTreeMap<String, usize>,
    /// Per-`egregore_version` breakdown; legacy records use key `"legacy_pre_v1"`.
    egregore_versions: BTreeMap<String, usize>,
}

impl InspectCounts {
    fn from_jsonl(jsonl: &str) -> Result<Self> {
        let mut counts = Self::default();
        let report = crate::adapters::records_from_jsonl_report(jsonl)?;
        for unknown in report.unknown_schema_versions {
            counts.records += 1;
            *counts
                .unknown_schema_versions
                .entry(unknown.version)
                .or_default() += 1;
        }
        for record in report.records {
            counts.records += 1;
            *counts
                .schema_versions
                .entry(record_version(&record))
                .or_default() += 1;
            match &record {
                GraphRecord::Node {
                    id,
                    kind,
                    repository_identity,
                    ..
                } => {
                    counts.nodes += 1;
                    if *kind == NodeKind::Diagnostic {
                        counts.diagnostics += 1;
                    }
                    if *kind == NodeKind::Repository {
                        let identity_summary = repository_identity.as_deref().map_or_else(
                            || "unknown".to_owned(),
                            |p| {
                                use crate::ir::IdentitySource;
                                let source_str = match p.identity_source {
                                    IdentitySource::Remote => "remote",
                                    IdentitySource::LocalRootCommit => "local_root_commit",
                                    IdentitySource::LocalPath => "local_path",
                                    IdentitySource::OperatorOverride => "operator_override",
                                };
                                let canonical = p
                                    .remote_url
                                    .as_deref()
                                    .or(p.root_commit_sha.as_deref())
                                    .or(p.canonical_path.as_deref())
                                    .unwrap_or(p.basename.as_str());
                                format!("{source_str}: {canonical}")
                            },
                        );
                        counts.repositories.push(RepositorySummary {
                            id: id.clone(),
                            identity_summary,
                        });
                    }
                }
                GraphRecord::Edge { .. } => counts.edges += 1,
                GraphRecord::Tombstone { .. } => counts.tombstones += 1,
            }
            // Producer breakdown — legacy records (no `producer` field) go under "legacy_pre_v1".
            let (kind_key, version_key) = record.producer().map_or_else(
                || ("legacy_pre_v1".to_owned(), "legacy_pre_v1".to_owned()),
                |p| {
                    (
                        p.producer_kind.as_str().to_owned(),
                        p.egregore_version.clone(),
                    )
                },
            );
            *counts.producer_kinds.entry(kind_key).or_default() += 1;
            *counts.egregore_versions.entry(version_key).or_default() += 1;
        }
        Ok(counts)
    }
}
