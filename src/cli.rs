//! Command-line interface for Egregore.

use std::{
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
    traj::{self, ImportOptions},
};

#[cfg(feature = "embedded-aletheiadb")]
use crate::adapters::EmbeddedAletheiaSink;
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
    },
    /// Import a rust-swe-agent .traj trajectory file into agent-memory JSONL.
    ImportTraj {
        /// Path to the `.traj` trajectory file.
        traj_path: PathBuf,
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
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
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
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
    },
    /// Find semantic drift nodes ranked by score descending.
    Drift {
        /// Graph JSONL path (mutually exclusive with --data-dir).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Maximum number of results (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: OutputFormat,
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
        } => ingest(
            &graph,
            adapter,
            data_dir.as_deref(),
            &agent_id,
            &session_id,
            idempotency_key.as_deref(),
        ),
        Commands::ImportTraj { traj_path, out } => import_traj_cmd(&traj_path, &out),
        Commands::Query { subcommand } => query_cmd(subcommand),
        #[cfg(feature = "embedded-aletheiadb")]
        Commands::Daemon { action } => daemon(action),
    }
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
    for repo in &counts.repositories {
        println!("repository: {} ({})", repo.id, repo.identity_summary);
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
) -> Result<()> {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    let _ = (data_dir, agent_id, session_id, idempotency_key);

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
            if let Some(metadata) = crate::daemon::active_metadata(&data_dir) {
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
    before_commit: &'a str,
    after_commit: &'a str,
    score: &'a str,
    model_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
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
            at,
            as_of,
            tx_as_of,
            format,
        } => {
            // --tx-as-of is reserved: always return a not_implemented envelope.
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
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_file(&records, &path, format)
        }
        QuerySubcommand::Drift {
            graph,
            data_dir,
            limit,
            format,
        } => {
            let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
            query_drift(&records, limit, format)
        }
    }
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

fn load_records_from_db(data_dir: &Path) -> Result<Vec<GraphRecord>> {
    #[cfg(feature = "embedded-aletheiadb")]
    {
        // Reject missing or empty directories before opening — a fresh/nonexistent
        // directory means the caller made a typo or forgot to run `eg ingest` first.
        match std::fs::read_dir(data_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(
                    "error: embedded store not found at {} — \
                     run `eg ingest --adapter embedded --data-dir <path>` first",
                    data_dir.display()
                );
            }
            Ok(mut entries) => {
                if entries.next().is_none() {
                    anyhow::bail!(
                        "error: embedded store at {} is empty — \
                         run `eg ingest --adapter embedded --data-dir <path>` first",
                        data_dir.display()
                    );
                }
            }
            Err(_) => {}
        }
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
            before_commit: &drift.before_git_commit,
            after_commit: &drift.after_git_commit,
            score: &drift.score,
            model_id: &drift.model_id,
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
            "{name} score={} {}..{} @ {path}",
            self.score, self.before_commit, self.after_commit
        )
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
    repositories: Vec<RepositorySummary>,
}

impl InspectCounts {
    fn from_jsonl(jsonl: &str) -> Result<Self> {
        let mut counts = Self::default();
        for (index, line) in jsonl
            .lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
        {
            let record = serde_json::from_str::<GraphRecord>(line)
                .with_context(|| format!("failed to parse graph record on line {}", index + 1))?;
            counts.records += 1;
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
        }
        Ok(counts)
    }
}
