use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use chrono::DateTime;

use super::RepositoryIndex;
use crate::ir::{GraphRecord, NodeKind, SnapshotHead};

/// Finds a symbol record by name at the most recent commit at or before `as_of`.
///
/// `as_of` must be an RFC 3339 timestamp string. Returns an error string if the
/// timestamp cannot be parsed. Returns `None` when no record exists at or before
/// the given instant.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbol_as_of_valid_time<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
) -> Result<Option<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut best: Option<(&GraphRecord, DateTime<chrono::FixedOffset>)> = None;

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        // Resolve valid_time from history temporal block (history records) or
        // node-level field (current-tree records stamped by with_valid_time_inferred).
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else {
            continue;
        };
        let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        let is_better = best.as_ref().is_none_or(|(prev_r, prev_vt)| {
            vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
        });
        if is_better {
            best = Some((record, vt));
        }
    }

    Ok(best.map(|(r, _)| r))
}

/// Repository-aware variant of [`symbol_as_of_valid_time`] (issue #67).
///
/// Returns the best record (most recent `valid_time` at or before `as_of`,
/// ties broken by ascending record ID) **per owning repository**, sorted by
/// record ID. When `repo` is supplied only records owned by that repository
/// are considered.
///
/// A multi-repository collision therefore yields one row per repository so
/// the caller can either surface all of them or fail with an
/// ambiguous-repository diagnostic — never picking a repository implicitly.
/// Records the index cannot attribute to any repository share one unattributed
/// group, preserving single-repository and legacy-fixture behavior.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbol_as_of_valid_time_by_repo<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Result<Vec<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut best: BTreeMap<Option<&str>, (&GraphRecord, DateTime<chrono::FixedOffset>)> =
        BTreeMap::new();

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        let owner = index.owner_of(record.id());
        if let Some(repo_id) = repo
            && owner != Some(repo_id)
        {
            continue;
        }
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else {
            continue;
        };
        let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        let is_better = best.get(&owner).is_none_or(|(prev_r, prev_vt)| {
            vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
        });
        if is_better {
            best.insert(owner, (record, vt));
        }
    }

    let mut results: Vec<&GraphRecord> = best.into_values().map(|(r, _)| r).collect();
    results.sort_by(|left, right| left.id().cmp(right.id()));
    Ok(results)
}

/// Resolves the symbol nodes representing the current HEAD state of their respective repositories.
/// Fallbacks to maximum-timestamp matching if Repository metadata or Git context is missing.
#[must_use]
pub fn resolve_head_symbols<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Vec<&'records GraphRecord> {
    let mut repo_heads = HashMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot: Some(snapshot),
            ..
        } = record
        {
            if let SnapshotHead::Commit { sha } = &snapshot.head {
                repo_heads.insert(id.as_str(), sha.as_str());
            }
        }
    }

    let mut best: BTreeMap<Option<&str>, &GraphRecord> = BTreeMap::new();
    let mut repos_with_match = HashSet::new();

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name,
            temporal: Some(t),
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        let owner = index.owner_of(record.id());
        if let Some(repo_id) = repo {
            if owner != Some(repo_id) {
                continue;
            }
        }
        if let Some(owner_id) = owner {
            if let Some(&head_sha) = repo_heads.get(owner_id) {
                if t.git_commit == head_sha {
                    let is_better = best
                        .get(&owner)
                        .is_none_or(|prev_r| record.id() < prev_r.id());
                    if is_better {
                        best.insert(owner, record);
                        repos_with_match.insert(owner_id);
                    }
                }
            }
        }
    }

    let mut matched: Vec<&GraphRecord> = best.into_values().collect();

    let mut fallback_repos = Vec::new();
    if let Some(repo_id) = repo {
        if !repos_with_match.contains(repo_id) {
            fallback_repos.push(Some(repo_id));
        }
    } else {
        for record in records {
            if let GraphRecord::Node {
                kind: NodeKind::Symbol,
                name,
                ..
            } = record
            {
                if name.as_deref() == Some(symbol_name) {
                    let owner = index.owner_of(record.id());
                    if let Some(o) = owner {
                        if !repos_with_match.contains(o) {
                            fallback_repos.push(Some(o));
                        }
                    } else if repos_with_match.is_empty() {
                        fallback_repos.push(None);
                    }
                }
            }
        }
    }

    fallback_repos.sort();
    fallback_repos.dedup();

    for r_opt in fallback_repos {
        let mut best: Option<(&GraphRecord, DateTime<chrono::FixedOffset>)> = None;
        for record in records {
            let GraphRecord::Node {
                kind: NodeKind::Symbol,
                name,
                temporal,
                valid_time,
                ..
            } = record
            else {
                continue;
            };
            if name.as_deref() != Some(symbol_name) {
                continue;
            }
            let owner = index.owner_of(record.id());
            if owner != r_opt {
                continue;
            }
            let vt_str = temporal
                .as_ref()
                .map(|t| t.valid_time.as_str())
                .or(valid_time.as_deref());
            let Some(vt_str) = vt_str else {
                continue;
            };
            let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
                continue;
            };
            let is_better = best.as_ref().is_none_or(|(prev_r, prev_vt)| {
                vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
            });
            if is_better {
                best = Some((record, vt));
            }
        }
        if let Some((r, _)) = best {
            matched.push(r);
        }
    }

    matched.sort_by(|left, right| left.id().cmp(right.id()));
    matched
}

/// Resolves the symbol nodes representing the state of their respective repositories as of a specific valid time.
///
/// Strictly filters candidate symbol commits by repository HEAD lineage before picking the newest.
/// Fallbacks to maximum-timestamp matching if Repository metadata or Git context is missing.
///
/// # Errors
///
/// Returns an error if the `--as-of` timestamp is not a valid RFC3339 string.
#[allow(clippy::implicit_hasher)]
pub fn resolve_as_of_symbols<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
    index: &RepositoryIndex,
    repo: Option<&str>,
    commit_parents: &HashMap<&str, &'records [String]>,
    commit_nodes: &HashMap<&str, Vec<&'records GraphRecord>>,
) -> Result<Vec<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    // Find HEAD commit of each repository
    let mut repo_heads = HashMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot: Some(snapshot),
            ..
        } = record
        {
            if let SnapshotHead::Commit { sha } = &snapshot.head {
                repo_heads.insert(id.as_str(), sha.as_str());
            }
        }
    }

    // For each repository owner, compute its filtered lineage (ancestors of HEAD as of as_of_dt)
    let mut repo_lineages = HashMap::new();
    for (&repo_id, &head_sha) in &repo_heads {
        if let Some(repo_filter) = repo {
            if repo_id != repo_filter {
                continue;
            }
        }

        // Traverse ancestry from head_sha
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(head_sha);

        while let Some(sha) = queue.pop_front() {
            if visited.insert(sha) {
                if let Some(&parents) = commit_parents.get(sha) {
                    for parent in parents {
                        let p_str = parent.as_str();
                        if !visited.contains(p_str) {
                            queue.push_back(p_str);
                        }
                    }
                }
            }
        }

        // Filter visited commits by valid_time <= as_of_dt
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
        repo_lineages.insert(repo_id, filtered);
    }

    // Now, find all candidate symbols that are on the computed lineages and <= as_of_dt
    let mut best: BTreeMap<Option<&str>, (&GraphRecord, DateTime<chrono::FixedOffset>)> =
        BTreeMap::new();

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        let owner = index.owner_of(record.id());
        if let Some(repo_id) = repo {
            if owner != Some(repo_id) {
                continue;
            }
        }

        // Must be on the lineage of its owner repository
        if let Some(owner_id) = owner {
            if let Some(lineage) = repo_lineages.get(owner_id) {
                let Some(t) = temporal else {
                    continue;
                };
                if !lineage.contains(t.git_commit.as_str()) {
                    continue;
                }
            }
        }

        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else {
            continue;
        };
        let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }

        let is_better = best.get(&owner).is_none_or(|(prev_r, prev_vt)| {
            vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
        });
        if is_better {
            best.insert(owner, (record, vt));
        }
    }

    let mut results: Vec<&GraphRecord> = best.into_values().map(|(r, _)| r).collect();
    results.sort_by(|left, right| left.id().cmp(right.id()));
    Ok(results)
}
