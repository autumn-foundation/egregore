use std::collections::{BTreeMap, BTreeSet};

use super::{CommitOrder, RepositoryIndex};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

/// Always-present advisory label for [`range_deltas`] responses.
///
/// Rows are observed deltas derived from stored graph snapshots; they never
/// assert anything about behavior, tests, or verification.
pub const RANGE_DELTAS_DISCLAIMER: &str = "Rows are observed structural and semantic deltas \
     between the resolved commits; they are not proof of behavior change, breakage, test \
     failure, or verification, and absence of a delta is not proof a behavior was preserved.";

/// Stable label attached to the semantic-drift section of a range-deltas
/// response, distinguishing semantic movement from structural change.
pub const RANGE_DELTAS_DRIFT_LABEL: &str = "semantic_movement_not_structural_change";

/// One classified structural delta between the two endpoints of a commit range.
///
/// Serialization is deliberately bounded to identity/path/span/commit metadata
/// (never node summaries, which embed normalized source bodies for
/// `scan-history` records).
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct RangeDeltaItem<'a> {
    /// Stable record ID of the delta's code fact (head-side snapshot for
    /// added/modified rows; base-side snapshot for removed rows).
    pub record_id: &'a str,
    /// Schema version stamped on the backing record.
    pub schema_version: u32,
    /// Stable change-class label (documented in `docs/cli/deltas.md`):
    /// `added_symbol` / `removed_symbol` / `modified_symbol` /
    /// `added_file` / `removed_file` / `modified_file`.
    pub change_class: &'static str,
    /// Symbol name; absent for file rows (the path is the handle).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    /// Language-specific symbol category, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<&'a str>,
    /// Repository-relative path of the file or symbol definition.
    pub repo_relative_path: &'a str,
    /// Source span, when available (base-side span for removed rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented reason a symbol row carries no span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<&'static str>,
    /// The commit within the range that introduced the head-visible state of
    /// this delta (last such commit in topological order).
    pub commit: &'a str,
    /// Valid time (committer date) of the introducing commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<&'a str>,
}

/// One semantic-drift row folded into a range-deltas response.
///
/// Drift is semantic movement measured over embeddings, never structural
/// change; the parent section's `label` states this explicitly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeDriftRow<'a> {
    /// Stable record ID of the drift marker record.
    pub record_id: &'a str,
    /// Schema version stamped on the drift record.
    pub schema_version: u32,
    /// Stable change-class label: always `semantic_drift`.
    pub change_class: &'static str,
    /// Stable record ID of the drifted code fact.
    pub target_record_id: &'a str,
    /// Drift score under the recorded metric.
    pub score: f64,
    /// Commit SHA of the earlier embedding.
    pub before_git_commit: &'a str,
    /// Commit SHA of the later embedding.
    pub after_git_commit: &'a str,
    /// Valid time of the later embedding.
    pub after_valid_time: &'a str,
}

/// Semantic-drift section of a range-deltas response.
///
/// Structural deltas are always returned; when the store carries no drift
/// records (embeddings absent or drift never computed) the section reports
/// `status: "unavailable"` with a reason instead of an indistinguishable empty
/// list.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeDriftSection<'a> {
    /// `available` when the store carries drift records, else `unavailable`.
    pub status: &'static str,
    /// Stable reason when `status` is `unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// Always [`RANGE_DELTAS_DRIFT_LABEL`]: drift is semantic movement, not
    /// structural change.
    pub label: &'static str,
    /// Drift rows whose later embedding lands inside the queried range.
    pub rows: Vec<RangeDriftRow<'a>>,
}

/// One row of the unresolved/unsupported diagnostic group of a range-deltas
/// response. These are stable machine-readable markers, never partial output.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct RangeDeltaDiagnostic {
    /// Stable diagnostic code (`unresolved_introducing_commit`,
    /// `missing_repo_relative_path`).
    pub code: &'static str,
    /// Stable record ID of the affected fact.
    pub record_id: String,
    /// Bounded human-readable detail (identity fields only, never payloads).
    pub detail: String,
}

/// Structured symbol- and file-level deltas between two commits, grouped by
/// stable change class. Returned by [`range_deltas`].
///
/// Every group is always present (empty vecs, never omitted) and canonically
/// ordered by `(repo_relative_path, name, record_id)` so repeated queries are
/// byte-equivalent after serialization.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeDeltas<'a> {
    /// Resolved full SHA of the base (older) endpoint.
    pub base: &'a str,
    /// Resolved full SHA of the head (newer) endpoint.
    pub head: &'a str,
    /// Number of commits in the range (reachable from head, not from base).
    pub range_commit_count: usize,
    /// Always-present advisory disclaimer ([`RANGE_DELTAS_DISCLAIMER`]).
    pub disclaimer: &'static str,
    /// Symbols present at head but not at base.
    pub added_symbols: Vec<RangeDeltaItem<'a>>,
    /// Symbols present at base but not at head.
    pub removed_symbols: Vec<RangeDeltaItem<'a>>,
    /// Symbols present at both endpoints whose recorded body changed.
    pub modified_symbols: Vec<RangeDeltaItem<'a>>,
    /// Files present at head but not at base.
    pub added_files: Vec<RangeDeltaItem<'a>>,
    /// Files present at base but not at head.
    pub removed_files: Vec<RangeDeltaItem<'a>>,
    /// Files present at both endpoints whose recorded content changed.
    pub modified_files: Vec<RangeDeltaItem<'a>>,
    /// Unresolved/unsupported diagnostic group.
    pub unresolved: Vec<RangeDeltaDiagnostic>,
    /// Semantic drift falling inside the range, or an unavailability marker.
    pub semantic_drift: RangeDriftSection<'a>,
}

/// Errors that can occur while resolving a range-deltas query.
///
/// Each variant serializes to a stable machine-readable diagnostic
/// (`error_type` + snake_case payload) rather than partial or silent output.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum RangeDeltasError {
    /// The specified commit prefix could not be resolved to any commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// The specified commit prefix was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// Both endpoints resolved to the same commit; an empty range is reported
    /// as a diagnostic, never as silent empty output.
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
    /// The store history is empty (no commits present).
    EmptyHistory,
}

/// Internal endpoint delta classes used while resolving introducing commits.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum RangeDeltaClass {
    Added,
    Removed,
    Modified,
}

/// The recorded snapshot body of a node record (empty for non-node records).
/// Used only for in-process comparison; snapshot bodies are never serialized.
pub(super) const fn range_delta_node_summary(record: &GraphRecord) -> &str {
    match record {
        GraphRecord::Node { summary, .. } => summary.as_str(),
        _ => "",
    }
}

/// A resolved `<base>..<head>` commit range: endpoints, commit topology, and
/// the range commit set. Shared by the range queries (issues #118 and #157)
/// so both keep identical endpoint resolution and error taxonomy.
pub(super) struct ResolvedCommitRange<'a> {
    /// Resolved full SHA of the base (older) endpoint.
    pub(super) base_sha: &'a str,
    /// Resolved full SHA of the head (newer) endpoint.
    pub(super) head_sha: &'a str,
    /// Commit SHA → deduplicated parent SHAs (temporal parents + PARENT_OF).
    pub(super) parent_map: BTreeMap<&'a str, Vec<&'a str>>,
    /// Commit SHA → valid time (committer date), when recorded.
    pub(super) commit_valid_time: BTreeMap<&'a str, &'a str>,
    /// Commits reachable from head but not from base.
    pub(super) range_commit_shas: BTreeSet<&'a str>,
    /// Range commits, newest first in deterministic topological order.
    pub(super) range_desc: Vec<&'a str>,
}

impl<'a> ResolvedCommitRange<'a> {
    /// The range commit that established the head-visible state of a delta:
    /// the last (newest topological) range commit where the class transition
    /// is observable against the commit's parents. For
    /// [`RangeDeltaClass::Modified`] two snapshots are compared through
    /// `modified_key` (the recorded body for #118, the signature or
    /// visibility surface for #157).
    pub(super) fn introducing(
        &self,
        per_commit: &BTreeMap<&str, &'a GraphRecord>,
        class: RangeDeltaClass,
        modified_key: impl Fn(&GraphRecord) -> &str,
    ) -> Option<&'a str> {
        for &sha in &self.range_desc {
            let parents: &[&str] = self.parent_map.get(sha).map_or(&[], Vec::as_slice);
            match class {
                RangeDeltaClass::Added => {
                    if per_commit.contains_key(sha)
                        && parents.iter().all(|p| !per_commit.contains_key(p))
                    {
                        return Some(sha);
                    }
                }
                RangeDeltaClass::Removed => {
                    if !per_commit.contains_key(sha)
                        && parents.iter().any(|p| per_commit.contains_key(p))
                    {
                        return Some(sha);
                    }
                }
                RangeDeltaClass::Modified => {
                    if let Some(snap) = per_commit.get(sha) {
                        let key = modified_key(snap);
                        if parents.iter().any(|p| {
                            per_commit
                                .get(p)
                                .is_some_and(|parent_snap| modified_key(parent_snap) != key)
                        }) {
                            return Some(sha);
                        }
                    }
                }
            }
        }
        None
    }
}

/// Resolves one commit handle (full SHA or unique prefix) against the
/// store's in-scope `Commit` nodes. Shared by the range queries (issues
/// #118 and #157) and the co-change coupling query (issue #153).
#[allow(clippy::missing_panics_doc)]
pub(super) fn resolve_commit_prefix<'a>(
    records: &'a [GraphRecord],
    prefix: &str,
    in_scope: &dyn Fn(&str) -> bool,
) -> Result<&'a str, RangeDeltasError> {
    let mut matches = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            ..
        } = r
        {
            if sha.to_lowercase().starts_with(&prefix.to_lowercase()) && in_scope(r.id()) {
                matches.push(sha.as_str());
            }
        }
    }
    matches.sort_unstable();
    matches.dedup();

    if matches.is_empty() {
        return Err(RangeDeltasError::MissingCommit {
            commit_prefix: prefix.to_owned(),
        });
    }
    if matches.len() > 1 {
        let string_matches = matches.iter().map(|s| (*s).to_owned()).collect();
        return Err(RangeDeltasError::AmbiguousCommitPrefix {
            commit_prefix: prefix.to_owned(),
            matches: string_matches,
        });
    }
    Ok(matches.into_iter().next().unwrap())
}

/// Commit topology over the in-scope `Commit` nodes: commit SHA →
/// deduplicated parent SHAs (temporal parents + `PARENT_OF` edges), plus
/// commit SHA → valid time (committer date) where recorded. Shared by the
/// range queries (issues #118 and #157) and the co-change coupling query
/// (issue #153).
#[allow(clippy::type_complexity)]
pub(super) fn commit_topology<'a>(
    records: &'a [GraphRecord],
    in_scope: &dyn Fn(&str) -> bool,
) -> (BTreeMap<&'a str, Vec<&'a str>>, BTreeMap<&'a str, &'a str>) {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let mut parent_map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut commit_valid_time: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = r
        {
            if in_scope(r.id()) {
                let entry = parent_map.entry(sha.as_str()).or_default();
                for parent in &t.git_parent_commits {
                    entry.push(parent.as_str());
                }
                commit_valid_time
                    .entry(sha.as_str())
                    .or_insert(t.valid_time.as_str());
            }
        }
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
            if let (Some(parent_node), Some(child_node)) =
                (by_id.get(source.as_str()), by_id.get(target.as_str()))
            {
                if let (
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
                {
                    let entry = parent_map.entry(csha.as_str()).or_default();
                    entry.push(psha.as_str());
                }
            }
        }
    }
    for parents in parent_map.values_mut() {
        parents.sort_unstable();
        parents.dedup();
    }
    (parent_map, commit_valid_time)
}

/// Ancestor closure of `start_sha` (inclusive) over a commit parent map.
pub(super) fn reachable_commits<'a>(
    parent_map: &BTreeMap<&'a str, Vec<&'a str>>,
    start_sha: &'a str,
) -> BTreeSet<&'a str> {
    let mut reachable = BTreeSet::new();
    let mut queue = vec![start_sha];
    while let Some(current) = queue.pop() {
        if !reachable.insert(current) {
            continue;
        }
        if let Some(parents) = parent_map.get(current) {
            for parent in parents {
                if !reachable.contains(*parent) {
                    queue.push(*parent);
                }
            }
        }
    }
    reachable
}

/// Resolves two commit handles (full SHA or unique prefix) against the
/// store's `Commit` nodes into a [`ResolvedCommitRange`], reporting every
/// failure as a stable [`RangeDeltasError`] rather than partial output.
pub(super) fn resolve_commit_range<'a>(
    records: &'a [GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    in_scope: &dyn Fn(&str) -> bool,
) -> Result<ResolvedCommitRange<'a>, RangeDeltasError> {
    let has_any_commits = records
        .iter()
        .any(|r| matches!(r.node_kind_name(), Some("Commit")));
    if !has_any_commits {
        return Err(RangeDeltasError::EmptyHistory);
    }

    // ── endpoint resolution (full SHA or unique prefix) ─────────────────────
    let base_sha = resolve_commit_prefix(records, base_prefix, in_scope)?;
    let head_sha = resolve_commit_prefix(records, head_prefix, in_scope)?;
    if base_sha == head_sha {
        return Err(RangeDeltasError::IdenticalEndpoints {
            commit: base_sha.to_owned(),
        });
    }

    // ── commit topology (temporal parents + PARENT_OF edges) ────────────────
    let (parent_map, commit_valid_time) = commit_topology(records, in_scope);

    let reachable_head = reachable_commits(&parent_map, head_sha);
    let reachable_base = reachable_commits(&parent_map, base_sha);
    if !reachable_head.contains(base_sha) {
        if reachable_base.contains(head_sha) {
            return Err(RangeDeltasError::ReversedRange {
                base: base_prefix.to_owned(),
                head: head_prefix.to_owned(),
            });
        }
        return Err(RangeDeltasError::NoPath {
            base: base_prefix.to_owned(),
            head: head_prefix.to_owned(),
        });
    }
    let range_commit_shas: BTreeSet<&str> = reachable_head
        .difference(&reachable_base)
        .copied()
        .collect();

    // Range commits, newest first in deterministic topological order, so the
    // introducing-commit search finds the last commit that established the
    // head-visible state.
    let order = CommitOrder::build(records);
    let mut range_desc: Vec<&str> = range_commit_shas.iter().copied().collect();
    range_desc.sort_by(|a, b| order.rank(b).cmp(&order.rank(a)).then_with(|| b.cmp(a)));

    Ok(ResolvedCommitRange {
        base_sha,
        head_sha,
        parent_map,
        commit_valid_time,
        range_commit_shas,
        range_desc,
    })
}

/// Snapshot index for one node kind: stable record ID → (commit SHA →
/// snapshot record). Shared by the range queries (issues #118 and #157).
pub(super) fn temporal_snapshot_index<'a>(
    records: &'a [GraphRecord],
    kind: NodeKind,
    in_scope: &dyn Fn(&str) -> bool,
) -> BTreeMap<&'a str, BTreeMap<&'a str, &'a GraphRecord>> {
    let mut snaps: BTreeMap<&str, BTreeMap<&str, &'a GraphRecord>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: record_kind,
            temporal: Some(t),
            ..
        } = r
        {
            if *record_kind != kind || !in_scope(id.as_str()) {
                continue;
            }
            snaps
                .entry(id.as_str())
                .or_default()
                .insert(t.git_commit.as_str(), r);
        }
    }
    snaps
}

/// Compute symbol- and file-level deltas between two commit handles, grouped
/// by stable change class (issue #118).
///
/// The two endpoints are full SHAs or unique prefixes resolved against the
/// store's `Commit` nodes. Deltas compare the recorded `File`/`Symbol`
/// snapshots at the base endpoint against the head endpoint; a fact that
/// appears and disappears strictly inside the range is not an endpoint delta.
/// Each row carries the range commit that introduced its head-visible state
/// (the last such commit in deterministic topological order) plus that
/// commit's valid time. A rename surfaces as a `removed_*` row for the old
/// name and an `added_*` row for the new name, because symbol identity is
/// path- and name-based.
///
/// Purely read-time: reads only the provided records, never Git state or the
/// working tree.
///
/// # Errors
///
/// Returns a [`RangeDeltasError`] when the history is empty, a commit handle
/// is missing or ambiguous, the endpoints are identical, the range is
/// reversed, or no ancestor path connects the endpoints.
#[allow(clippy::missing_panics_doc)]
pub fn range_deltas<'a>(
    records: &'a [GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    repo_scope: Option<&str>,
) -> Result<RangeDeltas<'a>, RangeDeltasError> {
    // Repository scoping mirrors `changes_context`: in a shared store two
    // repositories can carry the same commit SHA, so commit resolution and
    // snapshot selection are gated by owning repository when a scope is set.
    let repo_index = repo_scope.map(|_| RepositoryIndex::build(records));
    let in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };

    let range = resolve_commit_range(records, base_prefix, head_prefix, &in_scope)?;
    let base_sha = range.base_sha;
    let head_sha = range.head_sha;

    // ── snapshot index: record id → (commit sha → snapshot record) ──────────
    let file_snaps = temporal_snapshot_index(records, NodeKind::File, &in_scope);
    let symbol_snaps = temporal_snapshot_index(records, NodeKind::Symbol, &in_scope);

    let introducing =
        |per_commit: &BTreeMap<&str, &'a GraphRecord>, class: RangeDeltaClass| -> Option<&'a str> {
            range.introducing(per_commit, class, range_delta_node_summary)
        };

    let mut unresolved: Vec<RangeDeltaDiagnostic> = Vec::new();

    let build_item = |record: &'a GraphRecord,
                      class_label: &'static str,
                      class: RangeDeltaClass,
                      per_commit: &BTreeMap<&str, &'a GraphRecord>,
                      is_symbol: bool,
                      unresolved: &mut Vec<RangeDeltaDiagnostic>|
     -> Option<RangeDeltaItem<'a>> {
        let GraphRecord::Node {
            id,
            schema_version,
            name,
            symbol_kind,
            repo_relative_path,
            span,
            ..
        } = record
        else {
            return None;
        };
        let Some(path) = repo_relative_path.as_deref() else {
            unresolved.push(RangeDeltaDiagnostic {
                code: "missing_repo_relative_path",
                record_id: id.clone(),
                detail: format!("{class_label} row dropped: snapshot carries no repo path"),
            });
            return None;
        };
        let commit = introducing(per_commit, class).unwrap_or_else(|| {
            unresolved.push(RangeDeltaDiagnostic {
                code: "unresolved_introducing_commit",
                record_id: id.clone(),
                detail: format!(
                    "{class_label} delta confirmed between endpoints but no range commit \
                     shows the transition; falling back to the head commit"
                ),
            });
            head_sha
        });
        let absent_span_reason = if is_symbol && span.is_none() {
            Some("no_span_module_level")
        } else {
            None
        };
        Some(RangeDeltaItem {
            record_id: id,
            schema_version: *schema_version,
            change_class: class_label,
            name: if is_symbol { name.as_deref() } else { None },
            symbol_kind: if is_symbol {
                symbol_kind.as_deref()
            } else {
                None
            },
            repo_relative_path: path,
            span: *span,
            absent_span_reason,
            commit,
            valid_time: range.commit_valid_time.get(commit).copied(),
        })
    };

    let classify = |snaps: &BTreeMap<&str, BTreeMap<&str, &'a GraphRecord>>,
                    is_symbol: bool,
                    labels: [&'static str; 3],
                    unresolved: &mut Vec<RangeDeltaDiagnostic>|
     -> (
        Vec<RangeDeltaItem<'a>>,
        Vec<RangeDeltaItem<'a>>,
        Vec<RangeDeltaItem<'a>>,
    ) {
        let [added_label, removed_label, modified_label] = labels;
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut modified = Vec::new();
        for per_commit in snaps.values() {
            match (per_commit.get(base_sha), per_commit.get(head_sha)) {
                (None, Some(head_snap)) => {
                    added.extend(build_item(
                        head_snap,
                        added_label,
                        RangeDeltaClass::Added,
                        per_commit,
                        is_symbol,
                        unresolved,
                    ));
                }
                (Some(base_snap), None) => {
                    removed.extend(build_item(
                        base_snap,
                        removed_label,
                        RangeDeltaClass::Removed,
                        per_commit,
                        is_symbol,
                        unresolved,
                    ));
                }
                (Some(base_snap), Some(head_snap)) => {
                    if range_delta_node_summary(base_snap) != range_delta_node_summary(head_snap) {
                        modified.extend(build_item(
                            head_snap,
                            modified_label,
                            RangeDeltaClass::Modified,
                            per_commit,
                            is_symbol,
                            unresolved,
                        ));
                    }
                }
                // Present at neither endpoint: the fact appeared and
                // disappeared strictly inside the range, so it is not an
                // endpoint delta.
                (None, None) => {}
            }
        }
        let sort_items = |items: &mut Vec<RangeDeltaItem<'a>>| {
            items.sort_by(|a, b| {
                a.repo_relative_path
                    .cmp(b.repo_relative_path)
                    .then_with(|| a.name.unwrap_or("").cmp(b.name.unwrap_or("")))
                    .then_with(|| a.record_id.cmp(b.record_id))
            });
        };
        sort_items(&mut added);
        sort_items(&mut removed);
        sort_items(&mut modified);
        (added, removed, modified)
    };

    let (added_symbols, removed_symbols, modified_symbols) = classify(
        &symbol_snaps,
        true,
        ["added_symbol", "removed_symbol", "modified_symbol"],
        &mut unresolved,
    );
    let (added_files, removed_files, modified_files) = classify(
        &file_snaps,
        false,
        ["added_file", "removed_file", "modified_file"],
        &mut unresolved,
    );

    unresolved.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    unresolved.dedup();

    // ── semantic drift folding ───────────────────────────────────────────────
    let mut store_has_drift = false;
    let mut drift_rows: Vec<RangeDriftRow<'a>> = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::SemanticDrift,
            schema_version,
            temporal,
            semantic_drift: Some(drift),
            ..
        } = r
        {
            store_has_drift = true;
            let in_range = temporal
                .as_ref()
                .is_some_and(|t| range.range_commit_shas.contains(t.git_commit.as_str()))
                || range
                    .range_commit_shas
                    .contains(drift.after_git_commit.as_str());
            if in_range && in_scope(r.id()) {
                drift_rows.push(RangeDriftRow {
                    record_id: r.id(),
                    schema_version: *schema_version,
                    change_class: "semantic_drift",
                    target_record_id: &drift.target_record_id,
                    score: drift.score,
                    before_git_commit: &drift.before_git_commit,
                    after_git_commit: &drift.after_git_commit,
                    after_valid_time: &drift.after_valid_time,
                });
            }
        }
    }
    drift_rows.sort_by(|a, b| {
        a.target_record_id
            .cmp(b.target_record_id)
            .then_with(|| a.record_id.cmp(b.record_id))
    });
    let semantic_drift = if store_has_drift {
        RangeDriftSection {
            status: "available",
            reason: None,
            label: RANGE_DELTAS_DRIFT_LABEL,
            rows: drift_rows,
        }
    } else {
        RangeDriftSection {
            status: "unavailable",
            reason: Some("no_drift_records_in_store"),
            label: RANGE_DELTAS_DRIFT_LABEL,
            rows: Vec::new(),
        }
    };

    Ok(RangeDeltas {
        base: base_sha,
        head: head_sha,
        range_commit_count: range.range_commit_shas.len(),
        disclaimer: RANGE_DELTAS_DISCLAIMER,
        added_symbols,
        removed_symbols,
        modified_symbols,
        added_files,
        removed_files,
        modified_files,
        unresolved,
        semantic_drift,
    })
}

// ---------------------------------------------------------------------------
// As-of file symbol listing (issue #158)
// ---------------------------------------------------------------------------
