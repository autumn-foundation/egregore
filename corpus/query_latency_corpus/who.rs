use std::collections::{HashMap, HashSet, VecDeque};

use chrono::DateTime;

use super::{
    CommitOrder, RepositoryIndex, resolve_as_of_symbols, resolve_head_symbols, symbols_at_commit,
};
use crate::ir::{GraphRecord, NodeKind, SnapshotHead};

/// Finds the Commit node that last changed the symbol's file at or before the queried commit/time.
///
/// Returns `Ok(Some((symbol_node, commit_node)))` if found.
///
/// # Errors
///
/// Returns an error string when a temporal selector is malformed or the requested commit is not found.
#[allow(clippy::option_if_let_else)]
pub fn who_last_changed<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    at_commit: Option<&str>,
    as_of_time: Option<&str>,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Result<Option<(&'records GraphRecord, &'records GraphRecord)>, String> {
    // 1. Build indices in a single O(N) pre-pass to prevent multiple scans
    let mut commit_nodes: HashMap<&str, Vec<&GraphRecord>> = HashMap::new();
    let mut commit_parents = HashMap::new();
    let mut tombstoned_ids = HashSet::new();
    let mut matching_commits = Vec::new();
    let mut symbol_by_commit_and_name: HashMap<(&str, &str), Vec<&GraphRecord>> = HashMap::new();

    for record in records {
        match record {
            GraphRecord::Tombstone { deleted_id, .. } => {
                tombstoned_ids.insert(deleted_id.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::Commit,
                temporal: Some(t),
                ..
            } => {
                let sha = t.git_commit.as_str();
                commit_nodes.entry(sha).or_default().push(record);
                commit_parents.insert(sha, t.git_parent_commits.as_slice());
                if let Some(prefix) = at_commit {
                    if sha.starts_with(prefix) {
                        matching_commits.push(sha);
                    }
                }
            }
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                name: Some(name),
                temporal: Some(t),
                ..
            } => {
                symbol_by_commit_and_name
                    .entry((t.git_commit.as_str(), name.as_str()))
                    .or_default()
                    .push(record);
            }
            _ => {}
        }
    }

    // Build a CommitOrder helper for topological rankings
    let commit_order = CommitOrder::build(records);

    // 2. Resolve the symbol node
    let symbol_nodes = if let Some(commit) = at_commit {
        if let Some(repo_id) = repo {
            matching_commits.retain(|sha| {
                commit_nodes.get(sha).is_some_and(|c_nodes| {
                    c_nodes.iter().any(|c_node| {
                        let owner = index.owner_of(c_node.id());
                        owner == Some(repo_id)
                    })
                })
            });
        }

        // Deduplicate matching_commits to ensure unique SHAs
        matching_commits.sort_unstable();
        matching_commits.dedup();

        // Resolve prefix and enforce uniqueness
        if matching_commits.is_empty() {
            return Err(format!("commit prefix '{commit}' not found"));
        } else if matching_commits.len() > 1 {
            return Err(format!(
                "commit prefix '{commit}' is ambiguous, matched: {:?}",
                matching_commits
            ));
        }

        let mut matches = symbols_at_commit(records, symbol_name, commit);
        if let Some(repo_id) = repo {
            matches.retain(|r| index.owner_of(r.id()) == Some(repo_id));
        }
        matches
    } else {
        let mut matches = if let Some(as_of) = as_of_time {
            resolve_as_of_symbols(
                records,
                symbol_name,
                as_of,
                index,
                repo,
                &commit_parents,
                &commit_nodes,
            )?
        } else {
            resolve_head_symbols(records, symbol_name, index, repo)
        };

        // Filter matches to only include live symbols in their repository lineage at as_of
        matches.retain(|r| {
            let Some(repo_id) = index.owner_of(r.id()) else {
                return true; // legacy/unattributed repository, keep it
            };
            let r_commit = if let GraphRecord::Node {
                temporal: Some(t), ..
            } = r
            {
                t.git_commit.as_str()
            } else {
                return true;
            };

            // Find HEAD commit of repo_id
            let mut head_sha = None;
            for record in records {
                if let GraphRecord::Node {
                    kind: NodeKind::Repository,
                    id,
                    source_snapshot: Some(snapshot),
                    ..
                } = record
                {
                    if id == repo_id {
                        if let SnapshotHead::Commit { sha } = &snapshot.head {
                            head_sha = Some(sha.as_str());
                        }
                        break;
                    }
                }
            }

            let Some(start_sha) = head_sha else {
                return true; // no git context/head commit, keep it
            };

            // Traverse ancestry from start_sha
            let mut visited = HashSet::new();
            let mut queue = VecDeque::new();
            queue.push_back(start_sha);

            while let Some(sha) = queue.pop_front() {
                if visited.insert(sha) {
                    if let Some(parents) = commit_parents.get(sha) {
                        for parent in *parents {
                            let p_str = parent.as_str();
                            if !visited.contains(p_str) {
                                queue.push_back(p_str);
                            }
                        }
                    }
                }
            }

            // Filter by as_of_time
            let filtered_lineage = if let Some(as_of_t) = as_of_time {
                let Ok(as_of_dt) = DateTime::parse_from_rfc3339(as_of_t) else {
                    return false; // invalid timestamp format
                };
                let mut filtered = HashSet::new();
                for sha in visited {
                    if let Some(c_nodes) = commit_nodes.get(sha) {
                        let has_valid_node = c_nodes.iter().any(|c_node| {
                            let owner = index.owner_of(c_node.id());
                            if owner.is_some_and(|o| o != repo_id) {
                                return false;
                            }
                            if let GraphRecord::Node {
                                temporal: Some(t), ..
                            } = c_node
                            {
                                if let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) {
                                    return vt <= as_of_dt;
                                }
                            }
                            false
                        });
                        if has_valid_node {
                            filtered.insert(sha);
                        }
                    }
                }
                filtered
            } else {
                visited
            };

            if !filtered_lineage.contains(r_commit) {
                return false;
            }

            // Find lineage_head (latest commit in the lineage)
            let lineage_head = filtered_lineage
                .iter()
                .max_by_key(|&&sha| commit_order.rank(sha))
                .copied();

            let Some(head_commit_sha) = lineage_head else {
                return false; // no commits in lineage at this time
            };

            let r_kind = match r {
                GraphRecord::Node {
                    symbol_kind: Some(k),
                    ..
                } => k.as_str(),
                _ => "fn",
            };
            let r_disambiguator = match r {
                GraphRecord::Node {
                    disambiguator: Some(d),
                    ..
                } => *d,
                _ => 0,
            };
            let r_path = match r {
                GraphRecord::Node {
                    repo_relative_path, ..
                } => repo_relative_path.as_deref(),
                _ => None,
            };

            // Check if there is a Symbol node for symbol_name at head_commit_sha owned by repo_id
            // that matches candidate r's path, kind, and disambiguator.
            records.iter().any(|rec| {
                if let GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name,
                    temporal: Some(t),
                    symbol_kind: Some(rec_kind),
                    disambiguator: Some(rec_disambiguator),
                    repo_relative_path: rec_path,
                    ..
                } = rec
                {
                    if name.as_deref() == Some(symbol_name) && t.git_commit == head_commit_sha {
                        if index.owner_of(rec.id()) == Some(repo_id) {
                            return rec_path.as_deref() == r_path
                                && rec_kind == r_kind
                                && *rec_disambiguator == r_disambiguator;
                        }
                    }
                }
                false
            })
        });

        // Suppress tombstoned symbol nodes for active HEAD / current-state queries
        if as_of_time.is_none() {
            matches.retain(|r| !tombstoned_ids.contains(r.id()));
        }
        matches
    };

    if symbol_nodes.len() > 1 {
        let repos: Vec<&str> = symbol_nodes
            .iter()
            .filter_map(|r| index.owner_of(r.id()))
            .collect();
        return Err(format!(
            "symbol '{symbol_name}' is defined in multiple repositories ({repos:?}). Please specify --repo to resolve ambiguity."
        ));
    }
    let symbol_node = symbol_nodes.into_iter().next();

    let Some(symbol_node) = symbol_node else {
        return Ok(None);
    };

    let (target_symbol_kind, target_disambiguator, target_file_path, start_sha) = {
        if let GraphRecord::Node {
            symbol_kind,
            disambiguator,
            repo_relative_path: Some(path),
            temporal: Some(t),
            ..
        } = symbol_node
        {
            (
                symbol_kind.as_deref(),
                *disambiguator,
                path.as_str(),
                t.git_commit.as_str(),
            )
        } else {
            return Ok(None);
        }
    };

    let target_repo_id = index.owner_of(symbol_node.id());

    // 3. Define candidate commits, scoping them to the active lineage (prevent branch bleeding)
    let candidate_commit_shas = if at_commit.is_some() {
        let start_sha = matching_commits[0];

        // Traverse ancestry from target commit
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(start_sha);

        while let Some(sha) = queue.pop_front() {
            if visited.insert(sha) {
                if let Some(parents) = commit_parents.get(sha) {
                    for parent in *parents {
                        let p_str = parent.as_str();
                        if !visited.contains(p_str) {
                            queue.push_back(p_str);
                        }
                    }
                }
            }
        }

        // Apply valid-time limit to target ancestry if --as-of is co-specified
        if let Some(as_of) = as_of_time {
            let as_of_dt = DateTime::parse_from_rfc3339(as_of)
                .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;
            let mut filtered = HashSet::new();
            for sha in visited {
                if let Some(c_nodes) = commit_nodes.get(sha) {
                    let has_valid_node = c_nodes.iter().any(|c_node| {
                        if let Some(repo_id) = target_repo_id {
                            let owner = index.owner_of(c_node.id());
                            if owner != Some(repo_id) && owner.is_some() {
                                return false;
                            }
                        }
                        if let GraphRecord::Node {
                            temporal: Some(t), ..
                        } = c_node
                        {
                            if let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) {
                                return vt <= as_of_dt;
                            }
                        }
                        false
                    });
                    if has_valid_node {
                        filtered.insert(sha);
                    }
                }
            }
            filtered
        } else {
            visited
        }
    } else {
        // Lineage traversal for HEAD/as-of queries starting from resolved symbol commit
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(start_sha);

        while let Some(sha) = queue.pop_front() {
            if visited.insert(sha) {
                if let Some(parents) = commit_parents.get(sha) {
                    for parent in *parents {
                        let p_str = parent.as_str();
                        if !visited.contains(p_str) {
                            queue.push_back(p_str);
                        }
                    }
                }
            }
        }

        if let Some(as_of) = as_of_time {
            let as_of_dt = DateTime::parse_from_rfc3339(as_of)
                .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;
            let mut filtered = HashSet::new();
            for sha in visited {
                if let Some(c_nodes) = commit_nodes.get(sha) {
                    let has_valid_node = c_nodes.iter().any(|c_node| {
                        if let Some(repo_id) = target_repo_id {
                            let owner = index.owner_of(c_node.id());
                            if owner != Some(repo_id) && owner.is_some() {
                                return false;
                            }
                        }
                        if let GraphRecord::Node {
                            temporal: Some(t), ..
                        } = c_node
                        {
                            if let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) {
                                return vt <= as_of_dt;
                            }
                        }
                        false
                    });
                    if has_valid_node {
                        filtered.insert(sha);
                    }
                }
            }
            filtered
        } else {
            visited
        }
    };

    let get_matching_symbols = |commit_sha: &str| -> Vec<&GraphRecord> {
        symbol_by_commit_and_name
            .get(&(commit_sha, symbol_name))
            .map(|syms| {
                syms.iter()
                    .filter(|sym| {
                        let owner = index.owner_of(sym.id());
                        if owner != target_repo_id {
                            return false;
                        }
                        if let GraphRecord::Node {
                            symbol_kind,
                            disambiguator,
                            ..
                        } = sym
                        {
                            if symbol_kind.as_deref() != target_symbol_kind {
                                return false;
                            }
                            if *disambiguator != target_disambiguator {
                                return false;
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut symbol_path_by_commit: HashMap<&str, &str> = HashMap::new();
    symbol_path_by_commit.insert(start_sha, target_file_path);

    let mut path_queue = VecDeque::new();
    path_queue.push_back(start_sha);

    let mut path_visited = HashSet::new();

    while let Some(sha) = path_queue.pop_front() {
        if !path_visited.insert(sha) {
            continue;
        }

        let Some(&current_path) = symbol_path_by_commit.get(sha) else {
            continue;
        };

        if let Some(parents) = commit_parents.get(sha) {
            for parent in *parents {
                let parent_sha = parent.as_str();
                if !candidate_commit_shas.contains(parent_sha) {
                    continue;
                }
                if symbol_path_by_commit.contains_key(parent_sha) {
                    path_queue.push_back(parent_sha);
                    continue;
                }

                let parent_syms = get_matching_symbols(parent_sha);

                let parent_path = if let Some(p_sym) = parent_syms.iter().find(|s| {
                    if let GraphRecord::Node {
                        repo_relative_path: Some(path),
                        ..
                    } = s
                    {
                        path.as_str() == current_path
                    } else {
                        false
                    }
                }) {
                    if let GraphRecord::Node {
                        repo_relative_path: Some(path),
                        ..
                    } = p_sym
                    {
                        Some(path.as_str())
                    } else {
                        None
                    }
                } else if parent_syms.len() == 1 {
                    if let GraphRecord::Node {
                        repo_relative_path: Some(path),
                        ..
                    } = parent_syms[0]
                    {
                        Some(path.as_str())
                    } else {
                        None
                    }
                } else if !parent_syms.is_empty() {
                    let mut found_path = None;
                    for p_sym in &parent_syms {
                        if let GraphRecord::Node {
                            repo_relative_path: Some(p_path),
                            ..
                        } = p_sym
                        {
                            let has_change = records.iter().any(|rec| {
                                if let GraphRecord::Node {
                                    kind: NodeKind::Change,
                                    repo_relative_path: Some(change_path),
                                    temporal: Some(t_change),
                                    ..
                                } = rec
                                {
                                    t_change.git_commit == sha && change_path == p_path
                                } else {
                                    false
                                }
                            });
                            if has_change {
                                found_path = Some(p_path.as_str());
                                break;
                            }
                        }
                    }
                    found_path.or_else(|| {
                        if let GraphRecord::Node {
                            repo_relative_path: Some(p_path),
                            ..
                        } = parent_syms[0]
                        {
                            Some(p_path.as_str())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };

                if let Some(path) = parent_path {
                    symbol_path_by_commit.insert(parent_sha, path);
                    path_queue.push_back(parent_sha);
                }
            }
        }
    }

    // Helper to find the symbol node at a specific commit SHA that matches target repo, kind, disambiguator, and tracked path
    let get_symbol_node = |commit_sha: &str| -> Option<&GraphRecord> {
        let tracked_path = symbol_path_by_commit.get(commit_sha).copied();
        symbol_by_commit_and_name
            .get(&(commit_sha, symbol_name))
            .and_then(|syms| {
                syms.iter()
                    .find(|sym| {
                        let owner = index.owner_of(sym.id());
                        if owner != target_repo_id {
                            return false;
                        }
                        if let GraphRecord::Node {
                            symbol_kind,
                            disambiguator,
                            repo_relative_path,
                            ..
                        } = sym
                        {
                            if symbol_kind.as_deref() != target_symbol_kind {
                                return false;
                            }
                            if *disambiguator != target_disambiguator {
                                return false;
                            }
                            if tracked_path.is_some()
                                && repo_relative_path.as_deref() != tracked_path
                            {
                                return false;
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .copied()
            })
    };

    // 4. Find the latest commit that changed the file, ordering topologically
    let mut latest_commit: Option<(&GraphRecord, usize, DateTime<chrono::FixedOffset>)> = None;

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Change,
            repo_relative_path: Some(change_path),
            temporal: Some(t),
            ..
        } = record
        else {
            continue;
        };

        let sha = t.git_commit.as_str();
        if !candidate_commit_shas.contains(sha) {
            continue;
        }

        // O(1) commit lookup
        let commit_node = commit_nodes.get(sha).and_then(|c_nodes| {
            target_repo_id.map_or_else(
                || c_nodes.first().copied(),
                |repo_id| {
                    c_nodes
                        .iter()
                        .find(|c_node| {
                            let owner = index.owner_of(c_node.id());
                            owner == Some(repo_id)
                        })
                        .copied()
                },
            )
        });
        let Some(commit_node) = commit_node else {
            continue;
        };

        let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) else {
            continue;
        };

        // Check if the symbol actually changed in this commit
        let Some(sym_node_at_sha) = get_symbol_node(sha) else {
            continue;
        };

        let GraphRecord::Node {
            repo_relative_path: Some(sym_path_at_sha),
            summary: sym_summary,
            ..
        } = sym_node_at_sha
        else {
            continue;
        };

        if change_path != sym_path_at_sha {
            continue;
        }

        let parents = commit_parents.get(sha).copied().unwrap_or(&[]);
        let has_unchanged_parent = parents.iter().any(|parent_sha| {
            let parent_sha_str = parent_sha.as_str();
            if !candidate_commit_shas.contains(parent_sha_str) {
                return false;
            }
            if let Some(GraphRecord::Node {
                summary: parent_summary,
                ..
            }) = get_symbol_node(parent_sha_str)
            {
                return parent_summary == sym_summary;
            }
            false
        });

        if has_unchanged_parent {
            continue;
        }

        let current_rank = commit_order.rank(sha);

        let is_better = if let Some((prev_commit, prev_rank, prev_vt)) = latest_commit {
            // Rank topological descendant first, fall back to timestamp for parallel branches,
            // and use lexicographical ID comparison as the final tie-breaker.
            current_rank > prev_rank
                || (current_rank == prev_rank && vt > prev_vt)
                || (current_rank == prev_rank
                    && vt == prev_vt
                    && commit_node.id() < prev_commit.id())
        } else {
            true
        };

        if is_better {
            latest_commit = Some((commit_node, current_rank, vt));
        }
    }

    Ok(latest_commit.map(|(commit, _, _)| (symbol_node, commit)))
}

// ── Transaction-time queries (Issue #66) ───────────────────────────────────────
