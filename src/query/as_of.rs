use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use chrono::DateTime;

use super::RepositoryIndex;
use crate::ir::{GraphRecord, NodeKind, SnapshotHead, TemporalMetadata};

// ---------------------------------------------------------------------------
// Shared corpus-mode selection for current-state code lanes (issue #427)
// ---------------------------------------------------------------------------

/// The corpus a current-state code lane reads over a store.
///
/// Over a `scan-history` store with no `--at`/`--as-of`, lanes historically
/// disagreed on which records to read: some walked the UNION of every commit
/// snapshot (an edge deleted at HEAD still contributed), others HEAD-anchored
/// to the `Repository` `source_snapshot`. Issue #427 unifies this: current-state
/// code lanes DEFAULT to [`CorpusMode::HeadAnchored`] when a snapshot exists and
/// expose the union behind an opt-in flag.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CorpusMode {
    /// Records filtered to those current at each repository's stamped HEAD
    /// commit (`source_snapshot`, issue #82). A dependency removed at HEAD does
    /// not appear. This is the new default when a snapshot exists.
    HeadAnchored,
    /// The union of all commit snapshots — no head anchoring. An edge or target
    /// removed at a later commit can still appear. Opt-in via `--all-history`.
    Union,
    /// A single commit's snapshot selected by `--at`/`--as-of`.
    CommitPinned,
    /// A snapshot-less store (plain `scan`): keep-last-per-id current state.
    /// Equivalent to the pre-#427 default for such stores.
    SingleSnapshot,
}

impl CorpusMode {
    /// Stable wire string for the envelope `corpus_mode` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeadAnchored => "head_anchored",
            Self::Union => "union",
            Self::CommitPinned => "commit_pinned",
            Self::SingleSnapshot => "single_snapshot",
        }
    }

    /// One-line human description of what the corpus includes, for the
    /// envelope `corpus_disclaimer` field.
    #[must_use]
    pub const fn disclaimer(self) -> &'static str {
        match self {
            Self::HeadAnchored => {
                "Corpus: records current at each repository's stamped HEAD commit; \
                 an edge or target removed at HEAD is excluded. Pass --all-history \
                 for the union of all commit snapshots."
            }
            Self::Union => {
                "Corpus: the union of all commit snapshots; an edge or target removed \
                 at a later commit can still appear. Pass --at-head for HEAD-only state."
            }
            Self::CommitPinned => "Corpus: the single-commit snapshot selected by --at/--as-of.",
            Self::SingleSnapshot => {
                "Corpus: the single scanned snapshot (no commit history); \
                 keep-last-per-id current state."
            }
        }
    }
}

/// How the effective [`CorpusMode`] was chosen, for the envelope
/// `corpus_mode_source` field.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CorpusModeSource {
    /// No corpus flag or temporal selector supplied — the lane's default.
    Default,
    /// An explicit `--at-head`/`--all-history` flag chose the mode.
    ExplicitFlag,
    /// An `--at`/`--as-of` temporal selector chose the mode.
    Selector,
}

impl CorpusModeSource {
    /// Stable wire string for the envelope `corpus_mode_source` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ExplicitFlag => "explicit_flag",
            Self::Selector => "selector",
        }
    }
}

/// Resolves the effective corpus mode for a current-state code lane from the
/// four selector inputs (issue #427).
///
/// Precedence: an `--at`/`--as-of` temporal pin → [`CorpusMode::CommitPinned`];
/// otherwise `--all-history` → [`CorpusMode::Union`]; otherwise `--at-head` →
/// [`CorpusMode::HeadAnchored`]; otherwise the DEFAULT — `HeadAnchored` when a
/// `source_snapshot` exists (`has_snapshot`), else [`CorpusMode::SingleSnapshot`]
/// (plain `scan`/snapshot-less store, equivalently keep-last).
///
/// `--at-head` and `--all-history` are mutually exclusive with each other and
/// with any temporal pin.
///
/// # Errors
///
/// Returns a human-readable message when the flags conflict; callers surface it
/// as a machine-readable `unsupported_combination` diagnostic and exit 1.
#[allow(clippy::fn_params_excessive_bools)]
pub fn resolve_corpus_mode(
    has_temporal_pin: bool,
    at_head: bool,
    all_history: bool,
    has_snapshot: bool,
) -> Result<(CorpusMode, CorpusModeSource), String> {
    if at_head && all_history {
        return Err(
            "--at-head cannot be combined with --all-history; pass at most one corpus selector"
                .to_owned(),
        );
    }
    if has_temporal_pin && (at_head || all_history) {
        return Err(
            "--at-head/--all-history cannot be combined with --at/--as-of; a temporal pin \
             already selects a single-commit corpus"
                .to_owned(),
        );
    }
    if has_temporal_pin {
        return Ok((CorpusMode::CommitPinned, CorpusModeSource::Selector));
    }
    if all_history {
        return Ok((CorpusMode::Union, CorpusModeSource::ExplicitFlag));
    }
    if at_head {
        return Ok((CorpusMode::HeadAnchored, CorpusModeSource::ExplicitFlag));
    }
    if has_snapshot {
        Ok((CorpusMode::HeadAnchored, CorpusModeSource::Default))
    } else {
        Ok((CorpusMode::SingleSnapshot, CorpusModeSource::Default))
    }
}

/// The stamped HEAD commit SHA of each live repository (`source_snapshot`,
/// issue #82), keyed by `Repository` record ID. Tombstoned repositories and
/// repositories with a non-`Commit` head (`no_git`/`unborn_head`) are absent.
fn collect_repo_heads(records: &[GraphRecord]) -> HashMap<&str, &str> {
    let tombstoned: HashSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let mut repo_heads = HashMap::new();
    for record in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Repository,
            source_snapshot: Some(snapshot),
            ..
        } = record
            && !tombstoned.contains(id.as_str())
            && let SnapshotHead::Commit { sha } = &snapshot.head
        {
            repo_heads.insert(id.as_str(), sha.as_str());
        }
    }
    repo_heads
}

/// Extracts the temporal provenance of a node or edge record, if any.
const fn record_temporal(record: &GraphRecord) -> Option<&TemporalMetadata> {
    match record {
        GraphRecord::Node { temporal, .. } | GraphRecord::Edge { temporal, .. } => {
            temporal.as_ref()
        }
        GraphRecord::Tombstone { .. } => None,
    }
}

/// Whether ANY live `Repository` node carries a `source_snapshot` HEAD commit,
/// i.e. whether HEAD-anchoring is even possible (issue #427).
///
/// Used to pick the default corpus mode: [`CorpusMode::HeadAnchored`] when true,
/// [`CorpusMode::SingleSnapshot`] (keep-last) when false.
#[must_use]
pub fn store_has_source_snapshot(records: &[GraphRecord]) -> bool {
    !collect_repo_heads(records).is_empty()
}

/// Computes the set of record IDs that are NOT part of a store's current HEAD
/// state (issue #427), for current-state code lanes that head-anchor by default.
///
/// A record ID is returned (NON-current, to be dropped) only when it has at
/// least one temporal version AND **none** of its versions is current at HEAD.
/// This ID-level grouping matters because history replay re-emits a record at
/// every commit under the SAME stable ID: a symbol that still exists at HEAD is
/// retained (its HEAD version is current) even though earlier off-HEAD versions
/// exist, while a symbol removed before HEAD — whose every version is off-HEAD —
/// is dropped. Callers keep-last dedupe the retained versions, so the surviving
/// version is the HEAD one; a per-version stale record (e.g. a `CALLS` edge that
/// existed only at an earlier commit, whose unique edge ID has no HEAD version)
/// is dropped whole.
///
/// A single version is current under the shared generalization of the inlined
/// `owned_record_is_current` / `unowned_record_is_current` gates in
/// `public_api`/`unreferenced` (so those lanes can be retrofitted to call this
/// in a later phase):
///
/// * No temporal provenance → the ID is always current (fallback a).
/// * No repository carries a snapshot → every ID is current (fallback b: a
///   plain `scan`/snapshot-less store keeps its keep-last-per-id view and is
///   never head-filtered — an empty set is returned).
/// * Owner resolvable and stamped → current iff `git_commit` equals that
///   repository's stamped HEAD.
/// * Owner resolvable but its repository carries no snapshot → current
///   (fallback b, applied per repository).
/// * Owner unresolvable (edges, diagnostics) but some snapshot exists → current
///   iff `git_commit` equals ANY repository's stamped HEAD. Commit SHAs do not
///   collide across repositories in practice; this mirrors the existing
///   `unowned_record_is_current` gate.
///
/// Callers subtract the returned set from their working record set BEFORE
/// traversal.
#[must_use]
pub fn non_head_current_record_ids(
    records: &[GraphRecord],
    index: &RepositoryIndex,
) -> HashSet<String> {
    let repo_heads = collect_repo_heads(records);
    // Fallback (b): snapshot-less store keeps everything (keep-last-per-id).
    if repo_heads.is_empty() {
        return HashSet::new();
    }
    // Group by stable ID: an ID is current if ANY of its versions is current
    // (a HEAD-commit temporal version, or a non-temporal version — fallback a).
    let mut has_current: HashSet<&str> = HashSet::new();
    let mut has_off_head_only: HashSet<&str> = HashSet::new();
    for record in records {
        let id = record.id();
        let Some(t) = record_temporal(record) else {
            has_current.insert(id); // fallback a
            continue;
        };
        let version_current = index.owner_of(id).map_or_else(
            || repo_heads.values().any(|head| *head == t.git_commit),
            |owner| {
                repo_heads
                    .get(owner)
                    .is_none_or(|head| t.git_commit == **head)
            },
        );
        if version_current {
            has_current.insert(id);
        } else {
            has_off_head_only.insert(id);
        }
    }
    has_off_head_only
        .into_iter()
        .filter(|id| !has_current.contains(id))
        .map(str::to_owned)
        .collect()
}

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

#[cfg(test)]
mod corpus_tests {
    //! Unit coverage for the shared corpus-mode selection (issue #427):
    //! flag-precedence resolution, the HEAD-current record-id gate and its
    //! documented fallbacks, and snapshot detection.
    use super::*;
    use crate::ir::{EdgeLabel, SnapshotHead, SourceSnapshotPayload, SourceSpan};

    const T1: &str = "2026-01-01T00:00:00Z";
    const T2: &str = "2026-02-01T00:00:00Z";
    const C1: &str = "aaaa1111";
    const C2: &str = "bbbb2222";

    fn temporal(commit: &str, vt: &str) -> TemporalMetadata {
        TemporalMetadata {
            git_commit: commit.to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: vt.to_owned(),
            author_time: Some(vt.to_owned()),
            observed_at: vt.to_owned(),
            valid_time_source: Some("git_commit_committer_date".to_owned()),
        }
    }

    fn repo_node(id: &str, head: Option<&str>) -> GraphRecord {
        let node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some("repo".to_owned()),
            "Repository".to_owned(),
        );
        match head {
            Some(sha) => node.with_source_snapshot(SourceSnapshotPayload {
                head: SnapshotHead::Commit {
                    sha: sha.to_owned(),
                },
                dirty: false,
                repository_id: id.to_owned(),
                scanned_at: T2.to_owned(),
            }),
            None => node,
        }
    }

    fn sym(id: &str, name: &str, commit: &str, vt: &str) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::Symbol,
            "src/h.rs".to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            },
            name.to_owned(),
            "rust",
            format!("fn {name}"),
        )
        .with_temporal(temporal(commit, vt))
    }

    // ── resolve_corpus_mode precedence ──────────────────────────────────────
    #[test]
    fn temporal_pin_is_commit_pinned_via_selector() {
        assert_eq!(
            resolve_corpus_mode(true, false, false, true).unwrap(),
            (CorpusMode::CommitPinned, CorpusModeSource::Selector)
        );
    }

    #[test]
    fn all_history_flag_is_union() {
        assert_eq!(
            resolve_corpus_mode(false, false, true, true).unwrap(),
            (CorpusMode::Union, CorpusModeSource::ExplicitFlag)
        );
    }

    #[test]
    fn at_head_flag_is_head_anchored() {
        assert_eq!(
            resolve_corpus_mode(false, true, false, false).unwrap(),
            (CorpusMode::HeadAnchored, CorpusModeSource::ExplicitFlag)
        );
    }

    #[test]
    fn default_head_anchors_when_snapshot_present() {
        assert_eq!(
            resolve_corpus_mode(false, false, false, true).unwrap(),
            (CorpusMode::HeadAnchored, CorpusModeSource::Default)
        );
    }

    #[test]
    fn default_single_snapshot_without_snapshot() {
        assert_eq!(
            resolve_corpus_mode(false, false, false, false).unwrap(),
            (CorpusMode::SingleSnapshot, CorpusModeSource::Default)
        );
    }

    #[test]
    fn at_head_and_all_history_conflict() {
        assert!(resolve_corpus_mode(false, true, true, true).is_err());
    }

    #[test]
    fn temporal_pin_and_corpus_flag_conflict() {
        assert!(resolve_corpus_mode(true, true, false, true).is_err());
        assert!(resolve_corpus_mode(true, false, true, true).is_err());
    }

    #[test]
    fn mode_and_source_wire_strings_are_stable() {
        assert_eq!(CorpusMode::HeadAnchored.as_str(), "head_anchored");
        assert_eq!(CorpusMode::Union.as_str(), "union");
        assert_eq!(CorpusMode::CommitPinned.as_str(), "commit_pinned");
        assert_eq!(CorpusMode::SingleSnapshot.as_str(), "single_snapshot");
        assert_eq!(CorpusModeSource::Default.as_str(), "default");
        assert_eq!(CorpusModeSource::ExplicitFlag.as_str(), "explicit_flag");
        assert_eq!(CorpusModeSource::Selector.as_str(), "selector");
    }

    // ── store_has_source_snapshot ───────────────────────────────────────────
    #[test]
    fn snapshot_detection() {
        assert!(store_has_source_snapshot(&[repo_node("repo", Some(C2))]));
        assert!(!store_has_source_snapshot(&[repo_node("repo", None)]));
        assert!(!store_has_source_snapshot(&[]));
    }

    // ── non_head_current_record_ids ─────────────────────────────────────────
    #[test]
    fn snapshotless_store_filters_nothing_fallback_b() {
        // No Repository snapshot: keep-last, never head-filter.
        let records = vec![
            sym("codegraph:v5:a", "a", C1, T1),
            sym("codegraph:v5:b", "b", C2, T2),
        ];
        let index = RepositoryIndex::build(&records);
        assert!(non_head_current_record_ids(&records, &index).is_empty());
    }

    #[test]
    fn owned_symbol_off_head_is_non_current() {
        // Repo HEAD is C2. An owned symbol stamped at C1 is non-current; one at
        // C2 is current. An unowned CALLS edge is head-anchored by any-head.
        let repo_id = "codegraph:v5:repo";
        let file_id = "codegraph:v5:file";
        let a_id = "codegraph:v5:a";
        let b_id = "codegraph:v5:b";
        let records = vec![
            repo_node(repo_id, Some(C2)),
            GraphRecord::syntax_node(
                file_id.to_owned(),
                NodeKind::File,
                "src/h.rs".to_owned(),
                SourceSpan {
                    start_byte: 0,
                    end_byte: 10,
                    start_line: 1,
                    end_line: 2,
                },
                "h.rs".to_owned(),
                "rust",
                "file".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::Contains,
                repo_id.to_owned(),
                file_id.to_owned(),
                None,
                "contains".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::Defines,
                file_id.to_owned(),
                a_id.to_owned(),
                None,
                "defines a".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::Defines,
                file_id.to_owned(),
                b_id.to_owned(),
                None,
                "defines b".to_owned(),
            ),
            sym(a_id, "a", C1, T1),
            sym(b_id, "b", C2, T2),
            GraphRecord::edge(
                EdgeLabel::Calls,
                a_id.to_owned(),
                b_id.to_owned(),
                Some("1.0".to_owned()),
                "stale call".to_owned(),
            )
            .with_temporal(temporal(C1, T1)),
        ];
        let index = RepositoryIndex::build(&records);
        let non_current = non_head_current_record_ids(&records, &index);
        assert!(non_current.contains(a_id), "C1 symbol is off HEAD");
        assert!(!non_current.contains(b_id), "C2 symbol is at HEAD");
        // The stale CALLS edge (git_commit C1, unowned) is head-anchored out.
        let stale_edge_id = records
            .iter()
            .find_map(|r| match r {
                GraphRecord::Edge {
                    id,
                    label: EdgeLabel::Calls,
                    ..
                } => Some(id.as_str()),
                _ => None,
            })
            .expect("calls edge");
        assert!(
            non_current.contains(stale_edge_id),
            "an unowned stale-commit edge is filtered by the any-head rule"
        );
    }

    #[test]
    fn record_without_temporal_is_always_current() {
        // A snapshot exists, but a record with no temporal provenance is kept.
        let repo_id = "codegraph:v5:repo";
        let mut records = vec![repo_node(repo_id, Some(C2))];
        records.push(GraphRecord::syntax_node(
            "codegraph:v5:notemporal".to_owned(),
            NodeKind::Symbol,
            "src/h.rs".to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            },
            "n".to_owned(),
            "rust",
            "fn n".to_owned(),
        ));
        let index = RepositoryIndex::build(&records);
        assert!(!non_head_current_record_ids(&records, &index).contains("codegraph:v5:notemporal"));
    }
}
