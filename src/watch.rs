//! Transcripts watcher to auto-ingest new turns from Antigravity, Claude Code, and Codex.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::ir::agent_memory_stable_id;

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
    session_id: String,
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
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_symlink() {
                    continue; // Skip symlinks to prevent infinite loops and escaping watch root
                }
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if file_type.is_dir() {
                    // Skip massive build, VCS, and package manager directories to avoid heavy IO
                    if name_str == ".git"
                        || name_str == ".egregore"
                        || name_str == "target"
                        || name_str == "node_modules"
                    {
                        continue;
                    }
                    find_jsonl_files(&entry.path(), files);
                } else if file_type.is_file() {
                    // Only allocate path when it is a file and matches extension
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "jsonl") {
                        files.push(path);
                    }
                }
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
fn ingest_batch(data_dir: &Path, records: &[crate::ir::GraphRecord], embed: bool) -> Result<()> {
    use crate::adapters::{EmbeddedAletheiaSink, ingest_records};

    #[cfg_attr(not(feature = "embeddings"), allow(unused_mut))]
    let mut records = records.to_vec();

    #[cfg(feature = "embeddings")]
    let mut sink = if embed {
        let (vectors, dimensions, model) = crate::cli::generate_embeddings(&records)?;
        let sink = EmbeddedAletheiaSink::open_with_embeddings(data_dir, vectors, dimensions)
            .context("failed to open embedded store with embeddings")?;
        // Refuse before writing when the index was built by a different model
        // (issue #104) rather than blending two vector spaces.
        crate::cli::refuse_conflicting_index_identity(&sink, &model)?;
        // The watch loop maintains the same vector index the `--embed` ingest
        // creates, so it maintains the same identity record too.
        records.push(crate::embeddings::embedding_index_identity_record(&model));
        sink
    } else {
        EmbeddedAletheiaSink::open(data_dir).context("failed to open embedded store")?
    };

    #[cfg(not(feature = "embeddings"))]
    let mut sink = EmbeddedAletheiaSink::open(data_dir).context("failed to open embedded store")?;

    let _ = embed; // silence unused warning if feature disabled

    let report = ingest_records(&records, &mut sink);
    if report.is_success() {
        sink.persist_indexes()
            .context("failed to persist indexes")?;
    } else {
        anyhow::bail!("ingestion failed: {report:?}");
    }
    Ok(())
}

#[cfg(not(feature = "embedded-aletheiadb"))]
fn ingest_batch(_data_dir: &Path, _records: &[crate::ir::GraphRecord], _embed: bool) -> Result<()> {
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
    let mut found_files: Vec<(PathBuf, AgentType)> = Vec::new();
    let mut files_buf: Vec<PathBuf> = Vec::new();

    loop {
        found_files.clear();

        if let Some(p) = antigravity_dir {
            files_buf.clear();
            find_jsonl_files(p, &mut files_buf);
            for f in &files_buf {
                found_files.push((f.clone(), AgentType::Antigravity));
            }
        }
        if let Some(p) = codex_dir {
            files_buf.clear();
            find_jsonl_files(p, &mut files_buf);
            for f in &files_buf {
                found_files.push((f.clone(), AgentType::Codex));
            }
        }
        if let Some(p) = claude_dir {
            files_buf.clear();
            find_jsonl_files(p, &mut files_buf);
            for f in &files_buf {
                found_files.push((f.clone(), AgentType::Claude));
            }
        }

        let mut current_iteration_paths = std::collections::HashSet::new();
        let mut batch_records = Vec::new();
        let mut pending_updates = Vec::new();

        for (path, agent_type) in &found_files {
            current_iteration_paths.insert(path.clone());
            if let Ok(metadata) = fs::metadata(path) {
                let mtime = metadata.modified().unwrap_or_else(|_| SystemTime::now());
                let len = metadata.len();

                let (should_import, cached_session_id) =
                    file_states.get(path).map_or((true, None), |state| {
                        (
                            state.last_modified != mtime || state.len != len,
                            Some(state.session_id.clone()),
                        )
                    });

                if should_import {
                    println!(
                        "[Watcher] Found new or modified transcript: {}",
                        path.display()
                    );
                    let session_id = cached_session_id
                        .unwrap_or_else(|| derive_stable_session_id(path, *agent_type));

                    let graph_result = match agent_type {
                        AgentType::Antigravity => {
                            let opts = crate::antigravity::ImportOptions {
                                session_id_override: Some(session_id.clone()),
                                ..Default::default()
                            };
                            crate::antigravity::import_antigravity(path, &opts)
                        }
                        AgentType::Claude => {
                            let opts = crate::claude_code::ImportOptions {
                                session_id_override: Some(session_id.clone()),
                                ..Default::default()
                            };
                            crate::claude_code::import_claude_code(path, &opts)
                        }
                        AgentType::Codex => {
                            let opts = crate::codex::ImportOptions {
                                session_id_override: Some(session_id.clone()),
                                ..Default::default()
                            };
                            crate::codex::import_codex(path, &opts)
                        }
                    };

                    match graph_result {
                        Ok(graph) => {
                            // Run defense-in-depth safety gate: validate all records for unredacted secrets
                            let mut validation_ok = true;
                            for record in graph.records() {
                                if let Err(err) = crate::redaction::validate_record(record) {
                                    eprintln!(
                                        "[Watcher Warning] Transcript record in {} failed safety validation: {err:?}",
                                        path.display()
                                    );
                                    validation_ok = false;
                                    break;
                                }
                            }

                            if validation_ok {
                                batch_records.extend(graph.into_records());
                                // We will update the cache since parsing and validation succeeded.
                                // But only after the whole batch is successfully ingested!
                                pending_updates.push((
                                    path.clone(),
                                    FileState {
                                        last_modified: mtime,
                                        len,
                                        session_id,
                                    },
                                ));
                            } else {
                                // If validation fails, we STILL update the cache to prevent loop spamming,
                                // but we do not ingest these records.
                                file_states.insert(
                                    path.clone(),
                                    FileState {
                                        last_modified: mtime,
                                        len,
                                        session_id,
                                    },
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("[Watcher] Error importing {}: {e:?}", path.display());
                            // Mark permanent parsing errors as processed to avoid infinite loops and log spam
                            file_states.insert(
                                path.clone(),
                                FileState {
                                    last_modified: mtime,
                                    len,
                                    session_id,
                                },
                            );
                        }
                    }
                }
            }
        }

        if !batch_records.is_empty() {
            println!(
                "[Watcher] Ingesting batch of {} records into database...",
                batch_records.len()
            );
            if let Err(e) = ingest_batch(data_dir, &batch_records, embed) {
                eprintln!("[Watcher] Error ingesting batch: {e:?}");
                // Transient database ingestion failure: do NOT update the file state cache,
                // so the next iteration will retry importing and ingesting.
            } else {
                println!("[Watcher] Batch ingestion successful.");
                // Apply the cache updates only after successful ingestion
                for (path, state) in pending_updates {
                    file_states.insert(path, state);
                }
            }
        }

        // Clean up deleted files from state registry to prevent memory leak
        file_states.retain(|path, _| current_iteration_paths.contains(path));

        thread::sleep(poll_interval);

        if on_iteration.is_some_and(|cb| !cb()) {
            break;
        }
    }

    Ok(())
}
