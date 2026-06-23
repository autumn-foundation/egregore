//! Transcripts watcher to auto-ingest new turns from Antigravity, Claude Code, and Codex.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::ir::{Graph, agent_memory_stable_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentType {
    Antigravity,
    Codex,
    Claude,
}

impl AgentType {
    const fn importer_tag(self) -> &'static str {
        match self {
            Self::Antigravity => crate::antigravity::IMPORTER_ID,
            Self::Claude => crate::claude_code::IMPORTER_ID,
            Self::Codex => crate::codex::IMPORTER_ID,
        }
    }
}

struct FileState {
    last_modified: SystemTime,
    len: u64,
}

/// Resolves the home directory or user profile path.
#[must_use]
pub fn get_home_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .ok()
}

fn find_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                find_jsonl_files(&path, files);
            } else if path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
}

fn derive_stable_session_id(path: &Path, agent_type: AgentType) -> String {
    let abs_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path_str = abs_path.to_string_lossy();
    let path_hash = blake3::hash(path_str.as_bytes()).to_hex().to_string();
    agent_memory_stable_id(&[
        "node",
        "agent_session",
        agent_type.importer_tag(),
        &path_hash,
    ])
}

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_graph(data_dir: &Path, graph: &Graph, embed: bool) -> Result<()> {
    use crate::adapters::{EmbeddedAletheiaSink, ingest_records};

    #[cfg(feature = "embeddings")]
    let mut sink = if embed {
        let (vectors, dimensions) = crate::cli::generate_embeddings(graph.records())?;
        EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dimensions)
            .context("failed to open embedded store with embeddings")?
    } else {
        EmbeddedAletheiaSink::open(data_dir).context("failed to open embedded store")?
    };

    #[cfg(not(feature = "embeddings"))]
    let mut sink = EmbeddedAletheiaSink::open(data_dir).context("failed to open embedded store")?;

    let _ = embed; // silence unused warning if feature disabled

    let report = ingest_records(graph.records(), &mut sink);
    if report.is_success() {
        sink.persist_indexes()
            .context("failed to persist indexes")?;
    } else {
        anyhow::bail!("ingestion failed: {report:?}");
    }
    Ok(())
}

#[cfg(not(feature = "embedded-aletheiadb"))]
fn ingest_graph(_data_dir: &Path, _graph: &Graph, _embed: bool) -> Result<()> {
    anyhow::bail!("embedded-aletheiadb feature is required for ingestion");
}

/// Watches the specified directories for transcript modifications and ingests updates.
///
/// # Errors
///
/// Returns an error if directory watching or ingestion fails.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn watch(
    data_dir: &Path,
    antigravity_dir: Option<&Path>,
    codex_dir: Option<&Path>,
    claude_dir: Option<&Path>,
    poll_interval: Duration,
    embed: bool,
    on_iteration: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> Result<()> {
    println!(
        "Watcher started. Polling every {}s...",
        poll_interval.as_secs_f32()
    );

    if let Some(p) = antigravity_dir {
        println!("Watching Antigravity: {}", p.display());
    }
    if let Some(p) = codex_dir {
        println!("Watching Codex: {}", p.display());
    }
    if let Some(p) = claude_dir {
        println!("Watching Claude Code: {}", p.display());
    }

    let mut file_states: HashMap<PathBuf, FileState> = HashMap::new();

    loop {
        let mut found_files = Vec::new();

        if let Some(p) = antigravity_dir {
            let mut files = Vec::new();
            find_jsonl_files(p, &mut files);
            for f in files {
                found_files.push((f, AgentType::Antigravity));
            }
        }
        if let Some(p) = codex_dir {
            let mut files = Vec::new();
            find_jsonl_files(p, &mut files);
            for f in files {
                found_files.push((f, AgentType::Codex));
            }
        }
        if let Some(p) = claude_dir {
            let mut files = Vec::new();
            find_jsonl_files(p, &mut files);
            for f in files {
                found_files.push((f, AgentType::Claude));
            }
        }

        for (path, agent_type) in found_files {
            if let Ok(metadata) = fs::metadata(&path) {
                let mtime = metadata.modified().unwrap_or_else(|_| SystemTime::now());
                let len = metadata.len();

                let should_import = file_states
                    .get(&path)
                    .is_none_or(|state| state.last_modified != mtime || state.len != len);

                if should_import {
                    println!(
                        "[Watcher] Found new or modified transcript: {}",
                        path.display()
                    );
                    let session_id = derive_stable_session_id(&path, agent_type);

                    let graph_result = match agent_type {
                        AgentType::Antigravity => {
                            let opts = crate::antigravity::ImportOptions {
                                session_id_override: Some(session_id),
                                ..Default::default()
                            };
                            crate::antigravity::import_antigravity(&path, &opts)
                        }
                        AgentType::Claude => {
                            let opts = crate::claude_code::ImportOptions {
                                session_id_override: Some(session_id),
                                ..Default::default()
                            };
                            crate::claude_code::import_claude_code(&path, &opts)
                        }
                        AgentType::Codex => {
                            let opts = crate::codex::ImportOptions {
                                session_id_override: Some(session_id),
                                ..Default::default()
                            };
                            crate::codex::import_codex(&path, &opts)
                        }
                    };

                    match graph_result {
                        Ok(graph) => {
                            println!(
                                "[Watcher] Ingesting {} records from {}",
                                graph.records().len(),
                                path.display()
                            );
                            if let Err(e) = ingest_graph(data_dir, &graph, embed) {
                                eprintln!("[Watcher] Error ingesting {}: {e:?}", path.display());
                            } else {
                                println!("[Watcher] Ingestion successful.");
                            }
                        }
                        Err(e) => {
                            eprintln!("[Watcher] Error importing {}: {e:?}", path.display());
                        }
                    }

                    file_states.insert(
                        path,
                        FileState {
                            last_modified: mtime,
                            len,
                        },
                    );
                }
            }
        }

        thread::sleep(poll_interval);

        if on_iteration.is_some_and(|cb| !cb()) {
            break;
        }
    }

    Ok(())
}
