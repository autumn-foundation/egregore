use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use super::{CommitOrder, RepositoryIndex};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SnapshotHead};

// ---------------------------------------------------------------------------
// File ownership / bus-factor aggregation (issue #245)
// ---------------------------------------------------------------------------

/// Always-present advisory label for [`ownership_map`] responses.
///
/// Rows aggregate recorded Git authorship; they never assert declared
/// ownership, review authority, or expertise.
pub const OWNERSHIP_DISCLAIMER: &str = "Rows are empirical history-derived leads aggregated \
     from recorded Git commits; they are not declared ownership, authority, review \
     responsibility, or proven expertise. The primary owner is only the author of the largest \
     share of in-scope commits, and a low bus factor is a knowledge-concentration lead to \
     inspect, never proof that other editors are incompetent or that the file is unmaintained.";

/// Default cumulative ownership-share threshold percent for the bus factor.
pub const OWNERSHIP_DEFAULT_THRESHOLD_PERCENT: u32 = 50;

/// Default maximum number of file rows returned by [`ownership_map`].
pub const OWNERSHIP_DEFAULT_LIMIT: usize = 100;

/// Hard cap on `limit` for [`ownership_map`].
pub const OWNERSHIP_MAX_LIMIT: usize = 1000;

/// Query options for [`ownership_map`].
#[derive(Debug, Clone)]
pub struct OwnershipOptions<'q> {
    /// Optional exact repo-relative file path filter.
    pub path: Option<&'q str>,
    /// Report ownership as-of this commit SHA or unique prefix (valid-time
    /// axis). Mutually exclusive with `as_of`.
    pub at_commit: Option<&'q str>,
    /// Report ownership at the most recent commit at or before this RFC 3339
    /// instant (valid-time axis). Mutually exclusive with `at_commit`.
    pub as_of: Option<&'q str>,
    /// Restrict aggregation to one resolved repository record ID.
    pub repo_scope: Option<&'q str>,
    /// Cumulative ownership-share threshold percent for the bus factor
    /// (`1..=100`, default [`OWNERSHIP_DEFAULT_THRESHOLD_PERCENT`]).
    pub threshold_percent: u32,
    /// Maximum file rows: from 1 up to [`OWNERSHIP_MAX_LIMIT`], default
    /// [`OWNERSHIP_DEFAULT_LIMIT`].
    pub limit: usize,
}

impl Default for OwnershipOptions<'_> {
    fn default() -> Self {
        Self {
            path: None,
            at_commit: None,
            as_of: None,
            repo_scope: None,
            threshold_percent: OWNERSHIP_DEFAULT_THRESHOLD_PERCENT,
            limit: OWNERSHIP_DEFAULT_LIMIT,
        }
    }
}

/// One ranked author entry in an ownership row.
///
/// The identity is the normalized Git author identity recorded on `Commit`
/// records (issue #116): the exact `author_name` + `author_email` pair, with
/// no `.mailmap` or cross-email reconciliation. `author_email` is
/// redaction-eligible PII: a redaction-on export carries a
/// `<REDACTED:email:hash_prefix>` marker here instead of a raw address.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnershipAuthor<'a> {
    /// Git author display name, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_name: Option<&'a str>,
    /// Git author email (redaction-eligible PII), when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_email: Option<&'a str>,
    /// Distinct in-scope commits by this author touching the file.
    pub commits: usize,
    /// Ownership share: `commits / total_commits` for the file.
    pub share: f64,
}

/// One per-file ownership row returned by [`ownership_map`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnershipFileRow<'a> {
    /// Stable record ID of the file's `File` node — the citable handle.
    pub record_id: &'a str,
    /// Schema version stamped on the backing `File` record.
    pub schema_version: u32,
    /// Owning repository record ID, when the store topology attributes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Repository-relative file path.
    pub repo_relative_path: &'a str,
    /// Total distinct in-scope commits touching the file.
    pub total_commits: usize,
    /// Minimum number of top-ranked authors whose cumulative ownership share
    /// reaches the threshold. Lower means more concentrated knowledge.
    pub bus_factor: usize,
    /// The max-share author (ties break to the lexicographically smallest
    /// `(author_email, author_name)` identity). An empirical lead, never a
    /// declared-maintainer claim.
    pub primary_owner: OwnershipAuthor<'a>,
    /// All authors ranked by distinct commit count descending, then by
    /// `(author_email, author_name)` ascending.
    pub authors: Vec<OwnershipAuthor<'a>>,
}

/// The commit a repository's ownership view is anchored at.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnershipAnchor<'a> {
    /// Owning repository record ID, when attributed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Full SHA of the anchor commit (the resolved `--at` commit, the most
    /// recent commit at or before `--as-of`, or the repository head).
    pub commit_sha: &'a str,
    /// Valid time (committer date) of the anchor commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<&'a str>,
}

/// One stable machine-readable diagnostic attached to an ownership response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnershipDiagnostic {
    /// Stable diagnostic code (`empty_surface`, `no_recorded_changes`).
    pub code: &'static str,
    /// Bounded human-readable detail (paths and counts only, never payloads).
    pub detail: String,
}

/// Per-file authorship aggregates with primary owner and bus factor.
/// Returned by [`ownership_map`]; canonically ordered so repeated queries are
/// byte-equivalent after serialization.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnershipMap<'a> {
    /// Cumulative ownership-share threshold percent used for the bus factor.
    pub threshold_percent: u32,
    /// Always-present advisory disclaimer ([`OWNERSHIP_DISCLAIMER`]).
    pub disclaimer: &'static str,
    /// One anchor per contributing repository view, ordered by
    /// `(repository_id, commit_sha)`.
    pub anchors: Vec<OwnershipAnchor<'a>>,
    /// File rows in scope before `limit` was applied.
    pub total_file_count: usize,
    /// File rows actually returned.
    pub returned_file_count: usize,
    /// Whether `limit` truncated the row set.
    pub truncated: bool,
    /// File rows ordered by `(bus_factor asc, total_commits desc,
    /// repo_relative_path asc, repository_id asc)`.
    pub files: Vec<OwnershipFileRow<'a>>,
    /// Stable machine-readable diagnostics (never silent empty output).
    pub diagnostics: Vec<OwnershipDiagnostic>,
}

/// Errors that can occur while resolving an ownership query.
///
/// Each variant serializes to a stable machine-readable diagnostic
/// (`error_type` + snake_case payload) rather than partial or silent output.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum OwnershipError {
    /// The store carries no in-scope `Commit` records (for example a plain
    /// `eg scan` graph): there is no authorship history to aggregate.
    EmptyHistory,
    /// The `--at` commit prefix could not be resolved to any commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// The `--at` commit prefix was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// The `--as-of` value is not a valid RFC 3339 timestamp.
    MalformedTimestamp {
        /// The rejected input.
        as_of: String,
    },
    /// No in-scope commit exists at or before the `--as-of` instant.
    NoCommitsAtTime {
        /// The queried instant.
        as_of: String,
    },
    /// The path filter does not resolve to an indexed source file present at
    /// the resolved anchor(s).
    UnknownPath {
        /// The rejected path.
        path: String,
    },
    /// The bus-factor threshold is outside `1..=100`.
    InvalidThreshold {
        /// The rejected value.
        threshold_percent: u32,
    },
    /// The row limit is zero or above [`OWNERSHIP_MAX_LIMIT`].
    InvalidLimit {
        /// The rejected value.
        limit: usize,
        /// The documented maximum.
        max: usize,
    },
}

/// Normalized author identity key: the exact `(author_email, author_name)`
/// pair recorded on `Commit` records, with absent fields as empty strings.
type OwnershipAuthorKey<'a> = (&'a str, &'a str);

/// Aggregation key for one file view: `(owning repository, repo-relative path)`.
type OwnershipPathKey<'a> = (Option<String>, &'a str);

/// Distinct commit SHAs per author identity for one file view.
type OwnershipAuthorCommits<'a> = BTreeMap<OwnershipAuthorKey<'a>, BTreeSet<&'a str>>;

/// Recorded metadata for one in-scope commit during ownership aggregation.
struct OwnershipCommitMeta<'a> {
    valid_time: Option<&'a str>,
    author_name: Option<&'a str>,
    author_email: Option<&'a str>,
    parents: Vec<&'a str>,
}

/// Aggregate per-file Git authorship into ownership shares, a primary owner,
/// and a bus-factor signal (issue #245).
///
/// Consumes the author-attributed `Commit` facts recorded by `eg scan-history`
/// (issue #116) together with the per-commit `Change` records, scoped to the
/// commits reachable from the resolved anchor (repository head, `--at`
/// commit, or the most recent commit at or before `--as-of`). Only files
/// present at the anchor commit with a resolvable `File` node are reported,
/// so every row's handle resolves and untracked/ignored paths never appear.
///
/// Purely read-time: reads only the provided records, never Git state or the
/// working tree.
///
/// # Errors
///
/// Returns an [`OwnershipError`] when the history is empty, a selector is
/// malformed/missing/ambiguous, the path filter is unknown, or the threshold
/// or limit is out of range.
#[allow(clippy::missing_panics_doc)]
pub fn ownership_map<'a>(
    records: &'a [GraphRecord],
    options: &OwnershipOptions<'_>,
) -> Result<OwnershipMap<'a>, OwnershipError> {
    if !(1..=100).contains(&options.threshold_percent) {
        return Err(OwnershipError::InvalidThreshold {
            threshold_percent: options.threshold_percent,
        });
    }
    if options.limit == 0 || options.limit > OWNERSHIP_MAX_LIMIT {
        return Err(OwnershipError::InvalidLimit {
            limit: options.limit,
            max: OWNERSHIP_MAX_LIMIT,
        });
    }

    let repo_index = RepositoryIndex::build(records);
    let in_scope = |id: &str| -> bool {
        options
            .repo_scope
            .is_none_or(|scope| repo_index.owner_of(id) == Some(scope))
    };

    // ── in-scope commits grouped by owning repository ────────────────────────
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let mut groups: BTreeMap<Option<String>, BTreeMap<&'a str, OwnershipCommitMeta<'a>>> =
        BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal,
            author_name,
            author_email,
            ..
        } = r
        {
            if !in_scope(r.id()) {
                continue;
            }
            let owner = repo_index.owner_of(r.id()).map(ToOwned::to_owned);
            let entry = groups
                .entry(owner)
                .or_default()
                .entry(sha.as_str())
                .or_insert_with(|| OwnershipCommitMeta {
                    valid_time: None,
                    author_name: None,
                    author_email: None,
                    parents: Vec::new(),
                });
            if let Some(t) = temporal {
                entry.valid_time.get_or_insert(t.valid_time.as_str());
                for parent in &t.git_parent_commits {
                    entry.parents.push(parent.as_str());
                }
            }
            if entry.author_name.is_none() {
                entry.author_name = author_name.as_deref();
            }
            if entry.author_email.is_none() {
                entry.author_email = author_email.as_deref();
            }
        }
    }
    // PARENT_OF edges complete the topology for stores whose temporal parents
    // are absent (mirrors `resolve_commit_range`).
    for r in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::ParentOf,
            source,
            target,
            ..
        } = r
        {
            if !in_scope(source.as_str()) || !in_scope(target.as_str()) {
                continue;
            }
            let (Some(parent_node), Some(child_node)) =
                (by_id.get(source.as_str()), by_id.get(target.as_str()))
            else {
                continue;
            };
            let (
                GraphRecord::Node {
                    kind: NodeKind::Commit,
                    name: Some(psha),
                    ..
                },
                GraphRecord::Node {
                    kind: NodeKind::Commit,
                    name: Some(csha),
                    ..
                },
            ) = (parent_node, child_node)
            else {
                continue;
            };
            let owner = repo_index.owner_of(child_node.id()).map(ToOwned::to_owned);
            if let Some(group) = groups.get_mut(&owner) {
                if let Some(meta) = group.get_mut(csha.as_str()) {
                    meta.parents.push(psha.as_str());
                }
            }
        }
    }
    for group in groups.values_mut() {
        for meta in group.values_mut() {
            meta.parents.sort_unstable();
            meta.parents.dedup();
        }
    }
    if groups.is_empty() {
        return Err(OwnershipError::EmptyHistory);
    }

    // ── temporal selectors ───────────────────────────────────────────────────
    let as_of_dt = match options.as_of {
        Some(ts) => Some(DateTime::parse_from_rfc3339(ts).map_err(|_| {
            OwnershipError::MalformedTimestamp {
                as_of: ts.to_owned(),
            }
        })?),
        None => None,
    };
    let within_as_of = |meta: &OwnershipCommitMeta<'_>| -> bool {
        as_of_dt.is_none_or(|cutoff| {
            meta.valid_time.is_some_and(|vt| {
                DateTime::parse_from_rfc3339(vt).is_ok_and(|parsed| parsed <= cutoff)
            })
        })
    };

    let at_anchor: Option<&str> = match options.at_commit {
        None => None,
        Some(prefix) => {
            let needle = prefix.to_lowercase();
            let mut matches: Vec<&str> = groups
                .values()
                .flat_map(BTreeMap::keys)
                .filter(|sha| sha.to_lowercase().starts_with(&needle))
                .copied()
                .collect();
            matches.sort_unstable();
            matches.dedup();
            if matches.is_empty() {
                return Err(OwnershipError::MissingCommit {
                    commit_prefix: prefix.to_owned(),
                });
            }
            if matches.len() > 1 {
                return Err(OwnershipError::AmbiguousCommitPrefix {
                    commit_prefix: prefix.to_owned(),
                    matches: matches.iter().map(|s| (*s).to_owned()).collect(),
                });
            }
            Some(matches[0])
        }
    };

    // ── per-repository anchor + lineage (commits reachable from the anchor) ──
    let order = CommitOrder::build(records);
    let mut snapshot_heads: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot: Some(snapshot),
            ..
        } = r
        {
            if let SnapshotHead::Commit { sha } = &snapshot.head {
                snapshot_heads.insert(id.as_str(), sha.as_str());
            }
        }
    }

    let mut anchors: Vec<OwnershipAnchor<'a>> = Vec::new();
    // Owner → the set of lineage commit SHAs contributing to aggregation.
    let mut lineages: BTreeMap<Option<String>, BTreeSet<&'a str>> = BTreeMap::new();
    // (owner, anchor sha) pairs whose file trees define the reportable rows.
    let mut anchor_shas: BTreeMap<Option<String>, &'a str> = BTreeMap::new();
    for (owner, commits) in &groups {
        let anchor: Option<&str> = if at_anchor.is_some() {
            at_anchor.filter(|sha| commits.contains_key(*sha))
        } else {
            let candidates: Vec<&str> = commits
                .iter()
                .filter(|(_, meta)| within_as_of(meta))
                .map(|(sha, _)| *sha)
                .collect();
            if candidates.is_empty() {
                None
            } else {
                // The snapshot-head shortcut only applies without a cutoff:
                // under `--as-of` the documented anchor is the most recent
                // commit at or before the instant on the valid-time axis,
                // and clock skew can place a reachable commit's committer
                // date after HEAD's while both sit inside the cutoff.
                let head = owner
                    .as_deref()
                    .filter(|_| as_of_dt.is_none())
                    .and_then(|repo_id| snapshot_heads.get(repo_id).copied())
                    .filter(|sha| candidates.contains(sha));
                head.or_else(|| {
                    candidates.iter().copied().max_by(|a, b| {
                        // Under `--as-of`, the documented anchor is the most
                        // recent commit at or before the cutoff (valid-time
                        // axis); topological rank and SHA only break ties.
                        // Merged histories can hold a side-branch commit
                        // whose committer date is later than a deeper
                        // mainline commit's, so rank alone picks the wrong
                        // anchor. Without a cutoff (head absent from the
                        // recorded commits), rank picks the head-most commit.
                        let time_cmp = if as_of_dt.is_some() {
                            let parsed = |sha: &str| {
                                commits
                                    .get(sha)
                                    .and_then(|meta| meta.valid_time)
                                    .and_then(|vt| DateTime::parse_from_rfc3339(vt).ok())
                            };
                            parsed(a).cmp(&parsed(b))
                        } else {
                            Ordering::Equal
                        };
                        time_cmp
                            .then_with(|| order.rank(a).cmp(&order.rank(b)))
                            .then_with(|| a.cmp(b))
                    })
                })
            }
        };
        let Some(anchor) = anchor else {
            continue;
        };

        // Ancestors of the anchor (inclusive), constrained to the recorded
        // group and, under `--as-of`, to commits at or before the cutoff.
        // Reachability and the cutoff are independent constraints: clock skew
        // can date a reachable parent after the cutoff while its own
        // ancestors sit at or before it, so traversal always continues past
        // an out-of-cutoff commit — only counting excludes it.
        let mut lineage: BTreeSet<&str> = BTreeSet::new();
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut queue = vec![anchor];
        while let Some(sha) = queue.pop() {
            if !visited.insert(sha) {
                continue;
            }
            let Some(meta) = commits.get(sha) else {
                continue;
            };
            if within_as_of(meta) {
                lineage.insert(sha);
            }
            for parent in &meta.parents {
                if !visited.contains(parent) {
                    queue.push(parent);
                }
            }
        }
        if lineage.is_empty() {
            continue;
        }
        anchors.push(OwnershipAnchor {
            repository_id: owner.clone(),
            commit_sha: anchor,
            valid_time: commits.get(anchor).and_then(|meta| meta.valid_time),
        });
        lineages.insert(owner.clone(), lineage);
        anchor_shas.insert(owner.clone(), anchor);
    }
    if anchor_shas.is_empty() {
        if let Some(ts) = options.as_of {
            return Err(OwnershipError::NoCommitsAtTime {
                as_of: ts.to_owned(),
            });
        }
        return Err(OwnershipError::EmptyHistory);
    }
    anchors.sort_by(|a, b| {
        a.repository_id
            .cmp(&b.repository_id)
            .then_with(|| a.commit_sha.cmp(b.commit_sha))
    });

    // ── files present at each anchor commit ──────────────────────────────────
    // (owner, path) → (File record id, schema version).
    let mut files_at_anchor: BTreeMap<(Option<String>, &'a str), (&'a str, u32)> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            schema_version,
            repo_relative_path: Some(path),
            temporal: Some(t),
            ..
        } = r
        {
            if !in_scope(id.as_str()) {
                continue;
            }
            let owner = repo_index.owner_of(id.as_str()).map(ToOwned::to_owned);
            if anchor_shas.get(&owner) != Some(&t.git_commit.as_str()) {
                continue;
            }
            if options.path.is_some_and(|filter| filter != path.as_str()) {
                continue;
            }
            let entry = files_at_anchor
                .entry((owner, path.as_str()))
                .or_insert((id.as_str(), *schema_version));
            if id.as_str() < entry.0 {
                *entry = (id.as_str(), *schema_version);
            }
        }
    }
    if files_at_anchor.is_empty() {
        if let Some(filter) = options.path {
            return Err(OwnershipError::UnknownPath {
                path: filter.to_owned(),
            });
        }
    }

    // ── distinct commits per (owner, path, author identity) ─────────────────
    let mut commits_by_path: BTreeMap<OwnershipPathKey<'a>, BTreeSet<&'a str>> = BTreeMap::new();
    let mut commits_by_author: BTreeMap<OwnershipPathKey<'a>, OwnershipAuthorCommits<'a>> =
        BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Change,
            repo_relative_path: Some(path),
            temporal: Some(t),
            ..
        } = r
        {
            if !in_scope(id.as_str()) {
                continue;
            }
            let owner = repo_index.owner_of(id.as_str()).map(ToOwned::to_owned);
            let sha = t.git_commit.as_str();
            let Some(lineage) = lineages.get(&owner) else {
                continue;
            };
            if !lineage.contains(sha) {
                continue;
            }
            let key = (owner.clone(), path.as_str());
            if !files_at_anchor.contains_key(&key) {
                continue;
            }
            let Some(meta) = groups.get(&owner).and_then(|group| group.get(sha)) else {
                continue;
            };
            let author_key: OwnershipAuthorKey<'a> = (
                meta.author_email.unwrap_or(""),
                meta.author_name.unwrap_or(""),
            );
            commits_by_path.entry(key.clone()).or_default().insert(sha);
            commits_by_author
                .entry(key)
                .or_default()
                .entry(author_key)
                .or_default()
                .insert(sha);
        }
    }

    // ── rows: ranked authors, shares, primary owner, bus factor ─────────────
    let mut diagnostics: Vec<OwnershipDiagnostic> = Vec::new();
    let mut files: Vec<OwnershipFileRow<'a>> = Vec::new();
    for ((owner, path), (record_id, schema_version)) in &files_at_anchor {
        let key = (owner.clone(), *path);
        let Some(total_shas) = commits_by_path.get(&key) else {
            diagnostics.push(OwnershipDiagnostic {
                code: "no_recorded_changes",
                detail: format!("{path}: present at the anchor but no in-scope Change records"),
            });
            continue;
        };
        let total_commits = total_shas.len();
        let by_author = commits_by_author
            .get(&key)
            .expect("author map exists whenever the path map does");
        let mut authors: Vec<OwnershipAuthor<'a>> = by_author
            .iter()
            .map(|((email, name), shas)| OwnershipAuthor {
                author_name: (!name.is_empty()).then_some(*name),
                author_email: (!email.is_empty()).then_some(*email),
                commits: shas.len(),
                share: shas.len() as f64 / total_commits as f64,
            })
            .collect();
        authors.sort_by(|a, b| {
            b.commits
                .cmp(&a.commits)
                .then_with(|| {
                    a.author_email
                        .unwrap_or("")
                        .cmp(b.author_email.unwrap_or(""))
                })
                .then_with(|| a.author_name.unwrap_or("").cmp(b.author_name.unwrap_or("")))
        });
        let mut cumulative = 0_usize;
        let mut bus_factor = authors.len();
        for (rank, author) in authors.iter().enumerate() {
            cumulative += author.commits;
            if cumulative * 100 >= total_commits * options.threshold_percent as usize {
                bus_factor = rank + 1;
                break;
            }
        }
        let primary_owner = authors
            .first()
            .expect("a counted file always has at least one author")
            .clone();
        files.push(OwnershipFileRow {
            record_id,
            schema_version: *schema_version,
            repository_id: owner.clone(),
            repo_relative_path: path,
            total_commits,
            bus_factor,
            primary_owner,
            authors,
        });
    }

    // Most-concentrated first, then most-churned, then the stable path key.
    files.sort_by(|a, b| {
        a.bus_factor
            .cmp(&b.bus_factor)
            .then_with(|| b.total_commits.cmp(&a.total_commits))
            .then_with(|| a.repo_relative_path.cmp(b.repo_relative_path))
            .then_with(|| a.repository_id.cmp(&b.repository_id))
    });
    let total_file_count = files.len();
    let truncated = total_file_count > options.limit;
    files.truncate(options.limit);
    let returned_file_count = files.len();
    if files.is_empty() {
        diagnostics.push(OwnershipDiagnostic {
            code: "empty_surface",
            detail: "no indexed source files with recorded changes at the resolved anchor(s)"
                .to_owned(),
        });
    }
    diagnostics.sort_by(|a, b| a.code.cmp(b.code).then_with(|| a.detail.cmp(&b.detail)));

    Ok(OwnershipMap {
        threshold_percent: options.threshold_percent,
        disclaimer: OWNERSHIP_DISCLAIMER,
        anchors,
        total_file_count,
        returned_file_count,
        truncated,
        files,
        diagnostics,
    })
}
