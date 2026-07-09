use std::collections::{BTreeMap, BTreeSet};

use super::{CommitOrder, RepositoryIndex};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

// ─────────────────────────────────────────────────────────────────────────────
// File churn ranking (issue #128)
// ─────────────────────────────────────────────────────────────────────────────

/// Default `--limit` for `eg query churn`.
pub const CHURN_DEFAULT_LIMIT: usize = 50;

/// Maximum accepted `--limit` for `eg query churn`.
pub const CHURN_MAX_LIMIT: usize = 500;

/// One ranked file row in a churn ranking.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct ChurnFile {
    /// 1-based rank position after the documented ordering.
    pub rank: usize,
    /// Repository-relative file handle.
    pub repo_relative_path: String,
    /// Stable record ID of the `File` node the handle resolves to.
    pub file_record_id: String,
    /// Record schema version of the cited `File` node.
    pub schema_version: u32,
    /// Number of distinct commits in scope that modified the file.
    pub commit_count: usize,
    /// First commit (inclusive) of the range the frequency was measured over.
    pub first_commit: String,
    /// Last commit (inclusive) of the range the frequency was measured over.
    pub last_commit: String,
    /// Owning repository record ID, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Human-usable repository identity handle, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// The inclusive commit range covered by the store for one repository scope.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct ChurnCommitRange {
    /// Repository record ID; `None` groups rows the store topology cannot
    /// attribute (legacy records without repository containment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Human-usable repository identity handle, when attributable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// First commit (inclusive) in deterministic topological order.
    pub first_commit: String,
    /// Last commit (inclusive) in deterministic topological order.
    pub last_commit: String,
    /// Number of distinct commits in the scope.
    pub commit_count: usize,
}

/// Deterministic file-churn ranking over a `scan-history` temporal store.
#[derive(serde::Serialize, Clone, PartialEq, Eq, Debug)]
pub struct FileChurnReport {
    /// Documented ranking basis: distinct commits that modified the file.
    pub ranking_basis: &'static str,
    /// Documented stable tie-break key for equal commit counts.
    pub tie_break: &'static str,
    /// The limit the ranking was truncated to.
    pub limit: usize,
    /// Total ranked files before truncation.
    pub total_file_count: usize,
    /// Files returned after truncation.
    pub returned_file_count: usize,
    /// Completeness signal: whether `files` was truncated by `limit`.
    pub truncated: bool,
    /// Inclusive commit range(s) the ranking was measured over, one per
    /// repository scope, sorted by repository record ID.
    pub commit_ranges: Vec<ChurnCommitRange>,
    /// Ranked rows, highest churn first.
    pub files: Vec<ChurnFile>,
}

/// Errors returned by the file churn query.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum FileChurnError {
    /// The store (or the selected repository scope) holds no `Commit` nodes,
    /// so change frequency is undefined — the graph was not produced by
    /// `eg scan-history`.
    #[error("store has no commit history; run `eg scan-history` first")]
    NoHistory,
    /// Commits exist but no `File` node carries a `CHANGED_IN` commit edge in
    /// scope, so there is nothing to rank.
    #[error("no file changes found in the store's commit history")]
    NoMatch,
}

/// Ranks Git-tracked files by how many distinct commits modified them across
/// the commit history captured by `eg scan-history` (issue #128).
///
/// Counting is graph-native and read-only: a file's churn is the number of
/// distinct commit SHAs among `CHANGED_IN` edges from its `File` node to
/// `Commit` nodes. Only paths with a live (non-tombstoned) `File` node can
/// rank, so untracked, ignored, and non-indexed paths never appear — history
/// replay only records committed, Git-tracked source files (#67/#99).
///
/// Ordering is deterministic and byte-stable: `commit_count` descending, then
/// `repo_relative_path` ascending (documented tie-break), then
/// `file_record_id` ascending (cross-repository path collisions in an
/// unscoped multi-repository store). `limit` truncates the ranking after
/// ordering; the report states the truncation explicitly.
///
/// # Errors
///
/// Returns [`FileChurnError::NoHistory`] when no `Commit` node exists in
/// scope, and [`FileChurnError::NoMatch`] when commits exist but no file
/// carries a commit-backed `CHANGED_IN` edge in scope.
pub fn file_churn(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
) -> Result<FileChurnReport, FileChurnError> {
    let index = RepositoryIndex::build(records);
    let is_owned =
        |id: &str| -> bool { repo_id.is_none_or(|r_id| index.owner_of(id) == Some(r_id)) };

    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    // Commit node record ID → SHA, and per-repository-scope SHA sets.
    let mut commit_sha_by_node: BTreeMap<&str, &str> = BTreeMap::new();
    let mut commits_by_owner: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
    // Live File node record ID → (path, schema_version).
    let mut files: BTreeMap<&str, (&str, u32)> = BTreeMap::new();

    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            name,
            repo_relative_path,
            schema_version,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if tombstoned.contains(id.as_str()) || !is_owned(id) {
            continue;
        }
        match kind {
            NodeKind::Commit => {
                let sha = temporal
                    .as_ref()
                    .map(|t| t.git_commit.as_str())
                    .or(name.as_deref());
                if let Some(sha) = sha {
                    commit_sha_by_node.insert(id.as_str(), sha);
                    commits_by_owner
                        .entry(index.owner_of(id))
                        .or_default()
                        .insert(sha);
                }
            }
            NodeKind::File => {
                if let Some(path) = repo_relative_path.as_deref() {
                    files.entry(id.as_str()).or_insert((path, *schema_version));
                }
            }
            _ => {}
        }
    }

    if commit_sha_by_node.is_empty() {
        return Err(FileChurnError::NoHistory);
    }

    // File node ID → distinct commit SHAs that modified it.
    let mut changed_commits: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for record in records {
        let GraphRecord::Edge {
            id,
            label: EdgeLabel::ChangedIn,
            source,
            target,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        // Honor retraction tombstones on the change edge itself: a retracted
        // relationship must not count toward churn. Temporal guard shared with
        // `changes_context` / subsystem seeding — an edge carrying commit
        // provenance is a historical fact exempt from current-state tombstone
        // suppression (`eg forget` refuses such targets for the same reason).
        if temporal.is_none() && tombstoned.contains(id.as_str()) {
            continue;
        }
        if !files.contains_key(source.as_str()) {
            continue;
        }
        if let Some(&sha) = commit_sha_by_node.get(target.as_str()) {
            changed_commits
                .entry(source.as_str())
                .or_default()
                .insert(sha);
        }
    }

    if changed_commits.is_empty() {
        return Err(FileChurnError::NoMatch);
    }

    // Deterministic inclusive bounds per repository scope: topological rank
    // first (longest ancestor chain), SHA ascending as the stable tie-break.
    let commit_order = CommitOrder::build(records);
    let range_bounds = |shas: &BTreeSet<&str>| -> Option<(String, String)> {
        let mut ordered: Vec<&str> = shas.iter().copied().collect();
        ordered.sort_by(|a, b| {
            commit_order
                .rank(a)
                .cmp(&commit_order.rank(b))
                .then(a.cmp(b))
        });
        match (ordered.first(), ordered.last()) {
            (Some(first), Some(last)) => Some(((*first).to_owned(), (*last).to_owned())),
            _ => None,
        }
    };

    let mut commit_ranges = Vec::new();
    let mut bounds_by_owner: BTreeMap<Option<&str>, (String, String)> = BTreeMap::new();
    for (owner, shas) in &commits_by_owner {
        if let Some((first, last)) = range_bounds(shas) {
            bounds_by_owner.insert(*owner, (first.clone(), last.clone()));
            commit_ranges.push(ChurnCommitRange {
                repository_id: owner.map(ToOwned::to_owned),
                repository: owner
                    .and_then(|o| index.display_of(o))
                    .map(ToOwned::to_owned),
                first_commit: first,
                last_commit: last,
                commit_count: shas.len(),
            });
        }
    }

    let mut rows: Vec<ChurnFile> = Vec::new();
    for (file_id, shas) in &changed_commits {
        let Some((path, schema_version)) = files.get(file_id) else {
            continue;
        };
        let owner = index.owner_of(file_id);
        // The inclusive range the frequency was measured over is the owning
        // repository's full commit scope; fall back to the file's own modified
        // set for stores whose topology cannot attribute the file.
        let (first_commit, last_commit) = match bounds_by_owner.get(&owner) {
            Some((first, last)) => (first.clone(), last.clone()),
            None => match range_bounds(shas) {
                Some(bounds) => bounds,
                None => continue,
            },
        };
        rows.push(ChurnFile {
            rank: 0,
            repo_relative_path: (*path).to_owned(),
            file_record_id: (*file_id).to_owned(),
            schema_version: *schema_version,
            commit_count: shas.len(),
            first_commit,
            last_commit,
            repository_id: owner.map(ToOwned::to_owned),
            repository: owner
                .and_then(|o| index.display_of(o))
                .map(ToOwned::to_owned),
        });
    }

    if rows.is_empty() {
        return Err(FileChurnError::NoMatch);
    }

    rows.sort_by(|a, b| {
        b.commit_count
            .cmp(&a.commit_count)
            .then_with(|| a.repo_relative_path.cmp(&b.repo_relative_path))
            .then_with(|| a.file_record_id.cmp(&b.file_record_id))
    });
    for (position, row) in rows.iter_mut().enumerate() {
        row.rank = position + 1;
    }

    let total_file_count = rows.len();
    rows.truncate(limit);

    Ok(FileChurnReport {
        ranking_basis: "distinct_commit_count",
        tie_break: "repo_relative_path",
        limit,
        total_file_count,
        returned_file_count: rows.len(),
        truncated: total_file_count > rows.len(),
        commit_ranges,
        files: rows,
    })
}
