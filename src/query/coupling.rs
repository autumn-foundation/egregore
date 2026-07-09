use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use super::{
    RangeDeltasError, RepositoryIndex, commit_topology, reachable_commits, resolve_commit_prefix,
    resolve_commit_range,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

// ---------------------------------------------------------------------------
// Historical co-change coupling (issue #153)
// ---------------------------------------------------------------------------

/// Default minimum-support threshold for [`co_change_coupling`]: partner
/// files sharing fewer distinct in-scope commits with the target are
/// suppressed as noise.
pub const CO_CHANGE_DEFAULT_MIN_SUPPORT: usize = 2;

/// Upper bound accepted for the [`co_change_coupling`] min-support threshold.
pub const CO_CHANGE_MAX_MIN_SUPPORT: usize = 100;

/// Default partner-row cap for [`co_change_coupling`] responses.
pub const CO_CHANGE_DEFAULT_LIMIT: usize = 20;

/// Upper bound accepted for the [`co_change_coupling`] partner-row cap.
pub const CO_CHANGE_MAX_LIMIT: usize = 500;

/// Stable identifier of the normalized coupling-strength ranking metric.
///
/// The metric is the Jaccard index `co / (target + partner - co)` over
/// distinct in-scope commit sets. Documented in `docs/cli/coupling.md`.
pub const CO_CHANGE_COUPLING_METRIC: &str = "jaccard_v1";

/// Trust label stamped on every partner row: rows are historical co-change
/// leads, never dependency proof.
pub const CO_CHANGE_COUPLING_TRUST: &str = "historical_co_change_lead";

/// Always-present advisory disclaimer on co-change coupling responses.
pub const CO_CHANGE_COUPLING_DISCLAIMER: &str = "Rows are historical co-change leads - files \
     observed changing in the same commits as the target - and are not proof of dependency, \
     breakage, behavior change, or verification; no causality is inferred from commit \
     messages, file names, or proximity, and absence of coupling is not proof of independence.";

/// Selector and threshold options for [`co_change_coupling`].
///
/// The temporal selectors follow the existing contract: `base`+`head` bound
/// the in-scope commits to the `(base, head]` range exactly like
/// `eg query deltas` (issue #118); `at` bounds them to the ancestor closure
/// of one commit handle; `as_of` bounds them by valid time (committer date).
/// The selectors are mutually exclusive; `base` and `head` come as a pair.
#[derive(Debug, Clone)]
pub struct CoChangeCouplingOptions<'a> {
    /// Range base commit handle (older, exclusive endpoint); requires `head`.
    pub base: Option<&'a str>,
    /// Range head commit handle (newer, inclusive endpoint); requires `base`.
    pub head: Option<&'a str>,
    /// Single-commit bound: in-scope commits are the ancestor closure
    /// (inclusive) of this commit handle.
    pub at: Option<&'a str>,
    /// Valid-time bound: in-scope commits are those recorded at or before
    /// this RFC 3339 instant.
    pub as_of: Option<&'a str>,
    /// Minimum shared-commit count for a partner row
    /// (1..=[`CO_CHANGE_MAX_MIN_SUPPORT`]).
    pub min_support: usize,
    /// Partner-row cap (1..=[`CO_CHANGE_MAX_LIMIT`]).
    pub limit: usize,
}

impl Default for CoChangeCouplingOptions<'_> {
    fn default() -> Self {
        Self {
            base: None,
            head: None,
            at: None,
            as_of: None,
            min_support: CO_CHANGE_DEFAULT_MIN_SUPPORT,
            limit: CO_CHANGE_DEFAULT_LIMIT,
        }
    }
}

/// The resolved target file of a co-change coupling response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CouplingTarget<'a> {
    /// Stable record ID of the target's `File` node.
    pub record_id: &'a str,
    /// Schema version stamped on the `File` node.
    pub schema_version: u32,
    /// Repo-relative path of the target file.
    pub repo_relative_path: &'a str,
    /// Distinct in-scope commits that modified the target.
    pub change_count: usize,
}

/// The resolved temporal scope of a co-change coupling response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CouplingScope<'a> {
    /// Stable selector label: `full_history` / `commit_range` / `at_commit`
    /// / `as_of`.
    pub selector: &'static str,
    /// Resolved full SHA of the range base (commit_range only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<&'a str>,
    /// Resolved full SHA of the range head (commit_range only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<&'a str>,
    /// Resolved full SHA of the `--at` bound (at_commit only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<&'a str>,
    /// The `--as-of` instant as supplied (as_of only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<&'a str>,
    /// Number of distinct in-scope commits considered.
    pub commit_count: usize,
}

/// One ranked co-change partner row. Serialization is bounded to handles,
/// counts, and the documented metrics - never blob contents or patch hunks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CouplingPartner<'a> {
    /// Stable record ID of the partner's `File` node.
    pub record_id: &'a str,
    /// Schema version stamped on the `File` node.
    pub schema_version: u32,
    /// Repo-relative path of the partner file.
    pub repo_relative_path: &'a str,
    /// Distinct in-scope commits that modified both target and partner.
    pub co_change_count: usize,
    /// Distinct in-scope commits that modified the partner.
    pub partner_change_count: usize,
    /// Distinct in-scope commits that modified the target (same for every
    /// row; repeated so each row is independently citable).
    pub target_change_count: usize,
    /// Normalized symmetric coupling strength ([`CO_CHANGE_COUPLING_METRIC`]):
    /// `co / (target + partner - co)`. High-churn partners cannot dominate
    /// purely by volume because their own change count grows the denominator.
    pub coupling: f64,
    /// Directional confidence `co / target`: the fraction of the target's
    /// in-scope changes that also touched this partner.
    pub confidence: f64,
    /// Newest in-scope commit where both changed (by chronological valid
    /// time — parsed, offset-aware — with ties broken by SHA).
    pub last_co_change_commit: &'a str,
    /// Valid time of `last_co_change_commit`, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_co_change_valid_time: Option<&'a str>,
    /// Always [`CO_CHANGE_COUPLING_TRUST`].
    pub trust: &'static str,
}

/// Stable machine-readable diagnostic attached to an otherwise-successful
/// co-change coupling response (explicit empty results, never silence).
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct CouplingDiagnostic {
    /// Stable diagnostic code (`target_never_changed_in_scope`,
    /// `no_partner_at_or_above_min_support`).
    pub code: &'static str,
    /// Bounded human-readable detail (identity fields and counts only).
    pub detail: String,
}

/// Ranked historical co-change partners for one target file. Returned by
/// [`co_change_coupling`]; deterministic and byte-identical across runs on
/// unchanged history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CoChangeCoupling<'a> {
    /// The resolved target file.
    pub target: CouplingTarget<'a>,
    /// The resolved temporal scope.
    pub scope: CouplingScope<'a>,
    /// The minimum-support threshold that was applied (echoed).
    pub min_support: usize,
    /// The partner-row cap that was applied (echoed).
    pub limit: usize,
    /// Always [`CO_CHANGE_COUPLING_METRIC`].
    pub coupling_metric: &'static str,
    /// Always-present advisory disclaimer
    /// ([`CO_CHANGE_COUPLING_DISCLAIMER`]).
    pub disclaimer: &'static str,
    /// Partners at or above `min_support`, before the `limit` cap.
    pub total_partners: usize,
    /// Whether `partners` was truncated by `limit` (completeness signal).
    pub truncated: bool,
    /// Ranked partner rows: coupling strength descending, then co-change
    /// count descending, then repo-relative path ascending, then record ID.
    pub partners: Vec<CouplingPartner<'a>>,
    /// Explicit-empty and advisory diagnostics.
    pub diagnostics: Vec<CouplingDiagnostic>,
}

/// Errors that can occur while resolving a co-change coupling query.
///
/// Each variant serializes to a stable machine-readable diagnostic
/// (`error_type` + snake_case payload) rather than partial or silent output.
/// Commit-handle variants mirror [`RangeDeltasError`] so range failures keep
/// one taxonomy across the history-backed queries.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum CoChangeCouplingError {
    /// The target path was empty (or empty after normalization).
    MalformedPath {
        /// The path as supplied.
        path: String,
    },
    /// No in-scope `File` node carries the normalized repo-relative path.
    /// Untracked, ignored, and non-source paths never have `File` nodes.
    UnknownFile {
        /// The normalized repo-relative path that failed to resolve.
        path: String,
    },
    /// The path resolved to more than one `File` node (multi-repository
    /// store without a repository scope).
    AmbiguousFile {
        /// The normalized repo-relative path.
        path: String,
        /// Stable record IDs of every matching `File` node.
        candidates: Vec<String>,
    },
    /// The min-support threshold was outside the documented bounds.
    InvalidMinSupport {
        /// The rejected value.
        min_support: usize,
        /// The inclusive minimum (always 1).
        min: usize,
        /// The inclusive maximum ([`CO_CHANGE_MAX_MIN_SUPPORT`]).
        max: usize,
    },
    /// The partner-row cap was outside the documented bounds.
    InvalidLimit {
        /// The rejected value.
        limit: usize,
        /// The inclusive minimum (always 1).
        min: usize,
        /// The inclusive maximum ([`CO_CHANGE_MAX_LIMIT`]).
        max: usize,
    },
    /// The `as_of` bound was not a valid RFC 3339 instant.
    InvalidAsOfTimestamp {
        /// The rejected value.
        as_of: String,
        /// Parse failure detail.
        message: String,
    },
    /// No in-scope commit is recorded at or before the `as_of` instant.
    NoCommitAtOrBefore {
        /// The instant as supplied.
        as_of: String,
    },
    /// The selector combination was invalid (`base` without `head`, or
    /// mixing range / `at` / `as_of` selectors).
    MalformedSelector {
        /// Stable description of the rejected combination.
        message: String,
    },
    /// A commit prefix could not be resolved to any commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// A commit prefix was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// Both range endpoints resolved to the same commit.
    IdenticalEndpoints {
        /// The full SHA both endpoints resolved to.
        commit: String,
    },
    /// The range is reversed (base is a descendant of head).
    ReversedRange {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// There is no ancestor path between base and head.
    NoPath {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// The store history is empty (no commits present). Coupling requires a
    /// temporal store produced by `scan-history`.
    EmptyHistory,
}

impl From<RangeDeltasError> for CoChangeCouplingError {
    fn from(err: RangeDeltasError) -> Self {
        match err {
            RangeDeltasError::MissingCommit { commit_prefix } => {
                Self::MissingCommit { commit_prefix }
            }
            RangeDeltasError::AmbiguousCommitPrefix {
                commit_prefix,
                matches,
            } => Self::AmbiguousCommitPrefix {
                commit_prefix,
                matches,
            },
            RangeDeltasError::IdenticalEndpoints { commit } => Self::IdenticalEndpoints { commit },
            RangeDeltasError::ReversedRange { base, head } => Self::ReversedRange { base, head },
            RangeDeltasError::NoPath { base, head } => Self::NoPath { base, head },
            RangeDeltasError::EmptyHistory => Self::EmptyHistory,
        }
    }
}

/// Normalizes a user-supplied target path to the repo-relative form recorded
/// on `File` nodes: forward slashes, no leading `./`, no trailing slash.
fn normalize_coupling_path(raw: &str) -> Result<String, CoChangeCouplingError> {
    let forward = raw.trim().replace('\\', "/");
    let normalized = forward
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        return Err(CoChangeCouplingError::MalformedPath {
            path: raw.to_owned(),
        });
    }
    Ok(normalized)
}

/// Identity fields of one indexed `File` node used by [`co_change_coupling`].
struct CouplingFileInfo<'a> {
    /// Stable record ID of the `File` node.
    record_id: &'a str,
    /// Schema version stamped on the `File` node.
    schema_version: u32,
    /// Repo-relative path recorded on the `File` node.
    path: &'a str,
}

/// Rank the files that historically changed in the same commits as
/// `target_path` (issue #153).
///
/// Co-change is counted over distinct in-scope commits carrying
/// `CHANGED_IN` edges from `File` nodes to `Commit` nodes, as recorded by
/// `eg scan-history`. Because partners must resolve to `File` nodes, scope
/// automatically honors the Git-tracked / `.gitignore` / supported-language
/// boundaries of the scanner: untracked, ignored, and non-source paths never
/// appear as target or partner.
///
/// Partners below `min_support` shared commits are suppressed. Rows are
/// ranked by the symmetric Jaccard strength ([`CO_CHANGE_COUPLING_METRIC`])
/// descending, compared exactly (integer cross-multiplication, no float
/// rounding), with ties broken by co-change count descending, then
/// repo-relative path ascending, then record ID - so output is byte-identical
/// across runs on unchanged history.
///
/// `repo_scope`, when set, must be a resolved repository record ID (the CLI
/// resolves `--repo` selectors first); commit resolution, file resolution,
/// and counting are then gated to that repository.
///
/// Purely read-time: reads only the provided records, never Git state or the
/// working tree.
///
/// # Errors
///
/// Returns a [`CoChangeCouplingError`] when the path is malformed, unknown,
/// or ambiguous; when a threshold is out of bounds; when a commit handle is
/// missing or ambiguous; when the range is identical, reversed, or
/// unconnected; when the `as_of` bound is invalid or precedes all recorded
/// commits; or when the store has no commit history at all.
#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::missing_panics_doc
)]
pub fn co_change_coupling<'a>(
    records: &'a [GraphRecord],
    target_path: &str,
    repo_scope: Option<&str>,
    options: &CoChangeCouplingOptions<'a>,
) -> Result<CoChangeCoupling<'a>, CoChangeCouplingError> {
    // ── threshold and selector validation (before any store traversal) ──────
    if options.min_support < 1 || options.min_support > CO_CHANGE_MAX_MIN_SUPPORT {
        return Err(CoChangeCouplingError::InvalidMinSupport {
            min_support: options.min_support,
            min: 1,
            max: CO_CHANGE_MAX_MIN_SUPPORT,
        });
    }
    if options.limit < 1 || options.limit > CO_CHANGE_MAX_LIMIT {
        return Err(CoChangeCouplingError::InvalidLimit {
            limit: options.limit,
            min: 1,
            max: CO_CHANGE_MAX_LIMIT,
        });
    }
    if options.base.is_some() != options.head.is_some() {
        return Err(CoChangeCouplingError::MalformedSelector {
            message: "--base and --head must be provided together".to_owned(),
        });
    }
    let selector_count = usize::from(options.base.is_some())
        + usize::from(options.at.is_some())
        + usize::from(options.as_of.is_some());
    if selector_count > 1 {
        return Err(CoChangeCouplingError::MalformedSelector {
            message: "provide at most one of --base/--head, --at, or --as-of".to_owned(),
        });
    }

    let normalized_path = normalize_coupling_path(target_path)?;

    // ── repository scoping (record IDs gated by owning repository) ──────────
    let repo_index = repo_scope.map(|_| RepositoryIndex::build(records));
    let in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };

    let has_any_commits = records
        .iter()
        .any(|r| matches!(r.node_kind_name(), Some("Commit")));
    if !has_any_commits {
        return Err(CoChangeCouplingError::EmptyHistory);
    }

    // ── file index: File nodes only (tracked, non-ignored source files) ─────
    let mut files: BTreeMap<&str, CouplingFileInfo<'a>> = BTreeMap::new();
    let mut ids_by_path: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            schema_version,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if !in_scope(id.as_str()) {
                continue;
            }
            files.entry(id.as_str()).or_insert(CouplingFileInfo {
                record_id: id.as_str(),
                schema_version: *schema_version,
                path: path.as_str(),
            });
            ids_by_path
                .entry(path.as_str())
                .or_default()
                .insert(id.as_str());
        }
    }

    // ── target resolution (must be an existing File node) ───────────────────
    let target_ids = ids_by_path.get(normalized_path.as_str());
    let target_id = match target_ids {
        None => {
            return Err(CoChangeCouplingError::UnknownFile {
                path: normalized_path,
            });
        }
        Some(ids) if ids.len() > 1 => {
            return Err(CoChangeCouplingError::AmbiguousFile {
                path: normalized_path,
                candidates: ids.iter().map(|s| (*s).to_owned()).collect(),
            });
        }
        Some(ids) => *ids.iter().next().expect("non-empty id set"),
    };

    // ── repository gating for the unscoped commit and partner universe ──────
    // In a shared store two repositories can carry the same Git commit SHA
    // (forks, mirrored history), and the per-file sets count bare SHAs, so
    // without gating a file from another repository could surface as a
    // partner of a target it never co-changed with — and, symmetrically,
    // `--base`/`--head`/`--at`/`--as-of` could resolve against another
    // repository's commits and answer with a misleading zero-change empty
    // result. The target is resolved first, and everything downstream —
    // commit topology, endpoint resolution, temporal bounds, partner files,
    // and Change-record folding — is gated to the target file's owning
    // repository. `--repo` scoping already guarantees this through
    // `in_scope`; unscoped multi-repository stores are gated here through
    // record ownership, so a foreign-repository endpoint fails with the
    // same `missing_commit` / `no_commit_at_or_before` diagnostics the
    // scoped path emits.
    let owner_index: OnceCell<RepositoryIndex> = OnceCell::new();
    let repository_count = records
        .iter()
        .filter(|r| matches!(r.node_kind_name(), Some("Repository")))
        .count();
    let target_owner: Option<Option<String>> = if repo_scope.is_none() && repository_count > 1 {
        let index = owner_index.get_or_init(|| RepositoryIndex::build(records));
        Some(index.owner_of(target_id).map(str::to_owned))
    } else {
        None
    };
    let effective_in_scope = |id: &str| -> bool {
        match (&target_owner, owner_index.get()) {
            (Some(owner), Some(index)) => index.owner_of(id) == owner.as_deref(),
            _ => in_scope(id),
        }
    };
    files.retain(|id, _| effective_in_scope(id));

    // ── commit universe and temporal scope ──────────────────────────────────
    let (parent_map, commit_valid_time) = commit_topology(records, &effective_in_scope);
    let mut all_shas: BTreeSet<&str> = BTreeSet::new();
    let mut sha_by_commit_id: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Commit,
            name: Some(sha),
            ..
        } = r
        {
            if effective_in_scope(id.as_str()) {
                all_shas.insert(sha.as_str());
                sha_by_commit_id.insert(id.as_str(), sha.as_str());
            }
        }
    }
    if all_shas.is_empty() {
        // The target's repository carries no commits at all: coupling
        // requires a temporal store, and answering with an empty success
        // would be indistinguishable from "no coupling".
        return Err(CoChangeCouplingError::EmptyHistory);
    }

    let (scope, in_scope_shas): (CouplingScope<'a>, BTreeSet<&str>) =
        if let (Some(base), Some(head)) = (options.base, options.head) {
            let range = resolve_commit_range(records, base, head, &effective_in_scope)?;
            let shas = range.range_commit_shas.clone();
            (
                CouplingScope {
                    selector: "commit_range",
                    base: Some(range.base_sha),
                    head: Some(range.head_sha),
                    at: None,
                    as_of: None,
                    commit_count: shas.len(),
                },
                shas,
            )
        } else if let Some(at) = options.at {
            let sha = resolve_commit_prefix(records, at, &effective_in_scope)?;
            let shas: BTreeSet<&str> = reachable_commits(&parent_map, sha)
                .intersection(&all_shas)
                .copied()
                .collect();
            (
                CouplingScope {
                    selector: "at_commit",
                    base: None,
                    head: None,
                    at: Some(sha),
                    as_of: None,
                    commit_count: shas.len(),
                },
                shas,
            )
        } else if let Some(as_of) = options.as_of {
            let as_of_dt = DateTime::parse_from_rfc3339(as_of).map_err(|e| {
                CoChangeCouplingError::InvalidAsOfTimestamp {
                    as_of: as_of.to_owned(),
                    message: e.to_string(),
                }
            })?;
            let mut shas: BTreeSet<&str> = BTreeSet::new();
            for sha in &all_shas {
                if let Some(vt) = commit_valid_time.get(sha) {
                    if let Ok(vt) = DateTime::parse_from_rfc3339(vt) {
                        if vt <= as_of_dt {
                            shas.insert(sha);
                        }
                    }
                }
            }
            if shas.is_empty() {
                return Err(CoChangeCouplingError::NoCommitAtOrBefore {
                    as_of: as_of.to_owned(),
                });
            }
            (
                CouplingScope {
                    selector: "as_of",
                    base: None,
                    head: None,
                    at: None,
                    as_of: Some(as_of),
                    commit_count: shas.len(),
                },
                shas,
            )
        } else {
            (
                CouplingScope {
                    selector: "full_history",
                    base: None,
                    head: None,
                    at: None,
                    as_of: None,
                    commit_count: all_shas.len(),
                },
                all_shas.clone(),
            )
        };

    // ── per-file distinct in-scope commit sets from CHANGED_IN edges ────────
    let mut commits_by_file: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::ChangedIn,
            source,
            target,
            ..
        } = r
        {
            let Some(sha) = sha_by_commit_id.get(target.as_str()) else {
                continue; // File -> Change edges and out-of-scope commits.
            };
            if !files.contains_key(source.as_str()) || !in_scope_shas.contains(sha) {
                continue;
            }
            commits_by_file
                .entry(files.get(source.as_str()).expect("checked above").record_id)
                .or_default()
                .insert(sha);
        }
    }

    // ── fold Change records into the commit sets (deletion coverage) ────────
    // `scan-history` replays only paths present in a commit's tree, so a
    // deleted path has a `Change` record for the deletion commit but no
    // `File` snapshot and no `CHANGED_IN` edge there. Co-deletion is real
    // co-change, so every `Change` record whose path resolves to a known
    // `File` node contributes its commit to that file's set (a union with
    // the edge-derived sets: add/modify entries are already covered and
    // deduplicate). In a multi-repository store a Change record is
    // attributed through its owning repository; an unattributable
    // collision is skipped deterministically rather than guessed.
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Change,
            repo_relative_path: Some(path),
            temporal: Some(t),
            ..
        } = r
        {
            let sha = t.git_commit.as_str();
            if !in_scope_shas.contains(sha) || !effective_in_scope(id.as_str()) {
                continue;
            }
            let Some(candidates) = ids_by_path.get(path.as_str()) else {
                continue; // Non-source / never-indexed paths have no File node.
            };
            // The candidate must survive the repository gate above, and in a
            // multi-repository store its owner must match the Change's owner.
            let mut viable: Vec<&str> = candidates
                .iter()
                .copied()
                .filter(|candidate| files.contains_key(*candidate))
                .filter(|candidate| {
                    owner_index.get().is_none_or(|index| {
                        index.owner_of(candidate) == index.owner_of(id.as_str())
                    })
                })
                .collect();
            if viable.len() != 1 {
                continue;
            }
            let file_id = viable.pop().expect("len checked");
            commits_by_file.entry(file_id).or_default().insert(sha);
        }
    }

    let empty = BTreeSet::new();
    let target_commits = commits_by_file.get(target_id).unwrap_or(&empty);
    let target_change_count = target_commits.len();
    let target_info = files.get(target_id).expect("target resolved above");

    // ── partner counting, threshold, metric ─────────────────────────────────
    let mut partners: Vec<CouplingPartner<'a>> = Vec::new();
    for (file_id, commits) in &commits_by_file {
        if *file_id == target_id {
            continue;
        }
        let co: BTreeSet<&str> = commits.intersection(target_commits).copied().collect();
        let co_change_count = co.len();
        if co_change_count < options.min_support {
            continue;
        }
        let partner_change_count = commits.len();
        let union = target_change_count + partner_change_count - co_change_count;
        let info = files.get(file_id).expect("counted files are indexed");
        // Newest shared commit by chronological valid time, ties (and
        // unparseable/missing times) broken by SHA: the citable handle.
        // Valid times are parsed, never string-compared — `scan-history`
        // preserves non-UTC committer offsets, and a lexicographic compare
        // would mis-order them across offsets.
        let last = co
            .iter()
            .max_by_key(|sha| {
                (
                    commit_valid_time
                        .get(*sha)
                        .and_then(|vt| DateTime::parse_from_rfc3339(vt).ok()),
                    *sha,
                )
            })
            .copied()
            .expect("co_change_count >= min_support >= 1");
        partners.push(CouplingPartner {
            record_id: info.record_id,
            schema_version: info.schema_version,
            repo_relative_path: info.path,
            co_change_count,
            partner_change_count,
            target_change_count,
            coupling: co_change_count as f64 / union as f64,
            confidence: co_change_count as f64 / target_change_count as f64,
            last_co_change_commit: last,
            last_co_change_valid_time: commit_valid_time.get(last).copied(),
            trust: CO_CHANGE_COUPLING_TRUST,
        });
    }

    // Deterministic ranking: Jaccard descending compared exactly through
    // integer cross-multiplication (never float rounding), then co-change
    // count descending, then path ascending, then record ID ascending.
    partners.sort_by(|a, b| {
        let a_union = (a.target_change_count + a.partner_change_count - a.co_change_count) as u128;
        let b_union = (b.target_change_count + b.partner_change_count - b.co_change_count) as u128;
        let lhs = a.co_change_count as u128 * b_union;
        let rhs = b.co_change_count as u128 * a_union;
        rhs.cmp(&lhs)
            .then_with(|| b.co_change_count.cmp(&a.co_change_count))
            .then_with(|| a.repo_relative_path.cmp(b.repo_relative_path))
            .then_with(|| a.record_id.cmp(b.record_id))
    });

    let total_partners = partners.len();
    let truncated = total_partners > options.limit;
    partners.truncate(options.limit);

    let mut diagnostics = Vec::new();
    if target_change_count == 0 {
        diagnostics.push(CouplingDiagnostic {
            code: "target_never_changed_in_scope",
            detail: format!(
                "{} has a File node but no recorded change in the {} in-scope commit(s); \
                 co-change requires at least one shared commit",
                target_info.path, scope.commit_count
            ),
        });
    } else if total_partners == 0 {
        diagnostics.push(CouplingDiagnostic {
            code: "no_partner_at_or_above_min_support",
            detail: format!(
                "no file shares at least {} in-scope commit(s) with {}; absence of coupling \
                 is not proof of independence",
                options.min_support, target_info.path
            ),
        });
    }

    Ok(CoChangeCoupling {
        target: CouplingTarget {
            record_id: target_info.record_id,
            schema_version: target_info.schema_version,
            repo_relative_path: target_info.path,
            change_count: target_change_count,
        },
        scope,
        min_support: options.min_support,
        limit: options.limit,
        coupling_metric: CO_CHANGE_COUPLING_METRIC,
        disclaimer: CO_CHANGE_COUPLING_DISCLAIMER,
        total_partners,
        truncated,
        partners,
        diagnostics,
    })
}
