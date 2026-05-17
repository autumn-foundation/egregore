//! Command-line interface for Egregore.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{
    adapters::{DryRunSink, ingest_records, records_from_jsonl},
    ir::{GraphRecord, NodeKind},
    scan_repository, scan_repository_history,
};

#[cfg(feature = "embedded-aletheiadb")]
use crate::adapters::EmbeddedAletheiaSink;

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
    },
    /// Replay Git history and write temporal graph JSONL.
    ScanHistory {
        /// Git repository path to scan.
        repo_path: PathBuf,
        /// Output JSONL path.
        #[arg(long)]
        out: PathBuf,
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
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::ValueEnum)]
enum IngestAdapter {
    /// Validate ingest ordering and read-back without writing external storage.
    DryRun,
    /// Write into an embedded `AletheiaDB` store.
    #[cfg(feature = "embedded-aletheiadb")]
    Embedded,
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
        Commands::Scan { repo_path, out } => scan(&repo_path, &out),
        Commands::ScanHistory { repo_path, out } => scan_history(&repo_path, &out),
        Commands::Inspect { graph } => inspect(&graph),
        Commands::Ingest {
            graph,
            adapter,
            data_dir,
        } => ingest(&graph, adapter, data_dir.as_deref()),
    }
}

fn scan(repo_path: &Path, out: &Path) -> Result<()> {
    let graph = scan_repository(repo_path)
        .with_context(|| format!("failed to scan repository {}", repo_path.display()))?;
    let jsonl = graph
        .to_jsonl()
        .context("failed to serialize graph JSONL")?;
    fs::write(out, jsonl)
        .with_context(|| format!("failed to write graph JSONL to {}", out.display()))?;
    Ok(())
}

fn scan_history(repo_path: &Path, out: &Path) -> Result<()> {
    let graph = scan_repository_history(repo_path)
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
    Ok(())
}

fn ingest(graph: &Path, adapter: IngestAdapter, data_dir: Option<&Path>) -> Result<()> {
    #[cfg(not(feature = "embedded-aletheiadb"))]
    let _ = data_dir;

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

#[derive(Debug, Default)]
struct InspectCounts {
    records: usize,
    nodes: usize,
    edges: usize,
    tombstones: usize,
    diagnostics: usize,
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
            match record {
                GraphRecord::Node { kind, .. } => {
                    counts.nodes += 1;
                    if kind == NodeKind::Diagnostic {
                        counts.diagnostics += 1;
                    }
                }
                GraphRecord::Edge { .. } => counts.edges += 1,
                GraphRecord::Tombstone { .. } => counts.tombstones += 1,
            }
        }
        Ok(counts)
    }
}
