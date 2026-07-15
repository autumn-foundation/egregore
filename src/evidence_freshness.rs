//! Evidence-link freshness verdicts for agent observations (issue #85).
//!
//! When an agent observation cites a code handle (file/symbol at a recorded
//! valid-time or commit) and that code later **drifts**, nothing in the temporal
//! graph flags the note as standing on moved ground. This module closes that loop
//! with a strictly read-only, deterministic join over records the graph already
//! stores: agent-memory evidence links, semantic-drift records, code-graph
//! versions (content), tombstones, and temporal anchors.
//!
//! # What this is — and is not
//!
//! A verdict is a **freshness lead, never a truth claim**. `drifted` /
//! `unresolved` state only that the *evidence basis moved* — never that the
//! observation is now false, superseded, or correct (AC3). The verdict attaches
//! to the observation's evidence link; a code fact is never rewritten, hidden, or
//! marked stale, and an observation is never promoted to source truth (AC4).
//!
//! # Verdicts
//!
//! - `current` — cited code unchanged since the observation's anchor.
//! - `drifted` — the cited symbol/file changed after the observation's anchor.
//! - `unresolved` — the cited handle no longer resolves (symbol removed/renamed
//!   or file deleted).
//! - `untemporal` — the observation carries no commit, valid-time, or recording
//!   time to anchor a comparison.
//!
//! # Triggers (reused, never re-derived — AC11)
//!
//! Drift is read from signals the graph already carries, anchored to the cited
//! handle so a sibling symbol changing under the same file never flags a
//! neighbor (AC5):
//!
//! - **drift record** — a [`SemanticDriftMetadata`] whose `prior_record_id`
//!   equals the cited record ID and whose later measurement post-dates the
//!   anchor. The triggering handle is the drift record ID.
//! - **content change** — a later code-graph version of the *same* cited record
//!   ID (same handle, distinct `temporal.git_commit`) whose content hash differs
//!   from the anchor version. The triggering handle is the later commit plus the
//!   content hash.
//!
//! Documented in `docs/cli/evidence-freshness.md`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ir::{EvidenceLink, GraphRecord, NodeKind, SemanticDriftMetadata, SourceSpan};

// ── Verdict ────────────────────────────────────────────────────────────────────

/// Per-evidence-link freshness verdict.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessVerdict {
    /// Cited code unchanged since the observation's anchor.
    Current,
    /// The cited symbol/file changed after the observation's anchor.
    Drifted,
    /// The cited handle no longer resolves: removed/renamed/deleted, or absent
    /// from the latest snapshot in a history graph.
    Unresolved,
    /// The observation carries no commit, valid-time, or recording-time anchor to
    /// compare against.
    Untemporal,
}

impl FreshnessVerdict {
    /// Wire string value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Drifted => "drifted",
            Self::Unresolved => "unresolved",
            Self::Untemporal => "untemporal",
        }
    }

    /// Returns true for the stale verdicts (`drifted` / `unresolved`).
    #[must_use]
    pub const fn is_stale(self) -> bool {
        matches!(self, Self::Drifted | Self::Unresolved)
    }
}

/// Stable text stamped on every stale verdict so the output reads as a freshness
/// lead, never a truth claim (AC3).
pub const FRESHNESS_LEAD: &str = "evidence basis moved since the observation's anchor; a reason to re-verify, \
     not proof the note is wrong";

/// Stable diagnostic emitted when stale-only mode finds nothing (AC7).
pub const NO_STALE_DIAGNOSTIC: &str = "no_stale_observations";
/// Stable diagnostic emitted when stale-only mode finds stale observations.
pub const STALE_PRESENT_DIAGNOSTIC: &str = "stale_observations_present";

// ── Output types ───────────────────────────────────────────────────────────────

/// Provenance carried on every verdict (AC6). Never includes raw observation
/// text — only handles, confidence, and a redaction marker.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct VerdictProvenance {
    /// Stable agent identity, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Active session identifier, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// RFC 3339 wall-clock observation time, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Extraction confidence, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Redaction policy version, when the claim carried redacted text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_policy_version: Option<String>,
}

/// The cited code handle plus its anchor (AC6).
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct CitedHandle {
    /// Stable record ID of the cited code node, when the link supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_record_id: Option<String>,
    /// Repo-relative path of the cited handle, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span of the cited handle, when symbol-level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Anchor commit the citation was recorded against, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_commit: Option<String>,
    /// Anchor valid-time the citation was recorded against, when resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_valid_time: Option<String>,
    /// Cross-domain edge label of the evidence link.
    pub relation: String,
    /// Domain of the cited target (always `codegraph` for these verdicts).
    pub target_domain: String,
}

/// The handle that proves the cited code moved (AC6). Present only on
/// `drifted` / `unresolved` verdicts.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggeringHandle {
    /// A semantic-drift record measured change from the cited handle.
    DriftRecord {
        /// Stable drift record ID.
        drift_record_id: String,
        /// Commit SHA of the later (post-anchor) measurement.
        after_git_commit: String,
        /// Valid-time of the later (post-anchor) measurement.
        after_valid_time: String,
    },
    /// A later code-graph version of the same handle changed its content.
    ContentChange {
        /// Commit SHA of the later version that differs from the anchor version.
        after_git_commit: String,
        /// Valid-time of the later version.
        after_valid_time: String,
        /// Content hash of the later version (`blake3:<hex>`).
        content_hash: String,
    },
    /// The cited handle was removed/renamed and survives only as a tombstone.
    HandleRemoved {
        /// Stable tombstone record ID for the deleted handle.
        tombstone_id: String,
    },
    /// The cited handle resolves to nothing in the store (no live node, no
    /// tombstone). The cited handle itself is the triggering evidence.
    HandleAbsent,
}

/// One per-link freshness verdict.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct FreshnessVerdictEntry {
    /// Record ID of the observation under verdict.
    pub observation_id: String,
    /// Observation kind (`Observation` / `Decision`).
    pub kind: String,
    /// The freshness verdict.
    pub verdict: FreshnessVerdict,
    /// Freshness-lead text on stale verdicts; absent on `current` / `untemporal`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_lead: Option<&'static str>,
    /// Observation provenance.
    pub provenance: VerdictProvenance,
    /// The cited code handle and its anchor.
    pub cited_handle: CitedHandle,
    /// The handle that proves the cited code moved (stale verdicts only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggering_handle: Option<TriggeringHandle>,
}

// ── Internal index ──────────────────────────────────────────────────────────────

struct FreshnessIndex<'a> {
    /// Live (non-tombstoned) code-graph node versions grouped by record ID.
    live_code_by_id: BTreeMap<&'a str, Vec<&'a GraphRecord>>,
    /// Tombstone record ID, keyed by the deleted record ID.
    tombstone_by_deleted: BTreeMap<&'a str, &'a str>,
    /// Drift metadata keyed by the prior (anchor-side) record ID.
    drifts_by_prior: BTreeMap<&'a str, Vec<(&'a str, &'a SemanticDriftMetadata)>>,
    /// Node kind keyed by record ID, for any domain. Lets the scan tell a code
    /// handle (`File`/`Symbol`/…) apart from a non-handle codegraph record such
    /// as a `Commit`/`Change` cited by `EXPLAINS_CHANGE`.
    kind_by_id: BTreeMap<&'a str, NodeKind>,
    /// Every code-handle node version, including historical and tombstoned ones,
    /// used to resolve a triple citation at the commit it was anchored to.
    all_code_handles: Vec<&'a GraphRecord>,
    /// Commit DAG child adjacency (`parent → children`), used to decide whether a
    /// version is a descendant of an anchor commit regardless of timestamps.
    commit_children: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// Record IDs that have been superseded (by a node's `superseded_by`, a live
    /// `SUPERSEDES` edge, or a `SUPERSEDES` evidence link). These are excluded from
    /// the current view — both superseded observations and superseded code handles.
    superseded_ids: BTreeSet<&'a str>,
    /// Tip commits (no other commit's parent). Empty for edge-less graphs. Used to
    /// resolve unanchored triple citations against the frontier only.
    tips: BTreeSet<&'a str>,
    /// Commits that participate in any parent/child edge. A commit-anchored
    /// comparison trusts descendant reachability only when its anchor is here.
    dag_commits: BTreeSet<&'a str>,
    /// Per owning-repository tip commit sets (issue #203, Codex #454). Keyed by
    /// owning repository record ID (owned so the map outlives the local
    /// `RepositoryIndex`); the `None` bucket holds unattributable handles and
    /// equals the store-wide union for single-repo/legacy stores. Lets the triple
    /// resolvers scope tip membership to a candidate handle's OWN repository
    /// instead of the store-wide union.
    per_repo_tips: BTreeMap<Option<String>, BTreeSet<&'a str>>,
    /// Per owning-repository current-tree scan frontier `valid_time` (issue #204,
    /// Codex #559). Keyed by owning repository record ID. A non-temporal triple
    /// candidate counts as frontier only when its `valid_time` equals its repo's
    /// entry here, so an older-scan span no longer resolves.
    per_repo_frontier: BTreeMap<Option<String>, &'a str>,
    /// Owning repository record ID for each live code-handle record ID, so a
    /// triple candidate's frontier/tip check is scoped to its own repository.
    owner_by_handle: BTreeMap<&'a str, Option<String>>,
}

impl<'a> FreshnessIndex<'a> {
    #[allow(clippy::too_many_lines)]
    fn build(records: &'a [GraphRecord]) -> Self {
        let mut live_code_by_id: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
        let mut tombstone_by_deleted: BTreeMap<&str, &str> = BTreeMap::new();
        let mut drifts_by_prior: BTreeMap<&str, Vec<(&str, &SemanticDriftMetadata)>> =
            BTreeMap::new();
        let mut kind_by_id: BTreeMap<&str, NodeKind> = BTreeMap::new();
        let mut all_code_handles: Vec<&GraphRecord> = Vec::new();
        // Drift metadata keyed by the drift record's own ID, so a `DRIFTS_PRIOR`
        // edge can recover the metadata when the metadata's `prior_record_id` is
        // stale or filtered out of the slice.
        let mut drift_meta_by_id: BTreeMap<&str, &SemanticDriftMetadata> = BTreeMap::new();

        // First pass: active tombstones. In an append-only graph a delete can be
        // followed by re-ingesting the same stable ID (restoration), so a
        // tombstone is "active" only when it is the *latest* event for its deleted
        // ID — a later node version supersedes it and the handle is live again.
        // Treating every tombstone line as current would report a restored handle
        // as `unresolved`.
        let mut last_record_idx: BTreeMap<&str, usize> = BTreeMap::new();
        let mut last_tombstone: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
        for (idx, record) in records.iter().enumerate() {
            match record {
                // A node *or* edge re-emitted after its tombstone restores that ID;
                // both share the stable-ID/tombstone mechanism in append-only input.
                GraphRecord::Node { id, .. } | GraphRecord::Edge { id, .. } => {
                    last_record_idx.insert(id.as_str(), idx);
                }
                GraphRecord::Tombstone { id, deleted_id, .. } => {
                    last_tombstone
                        .entry(deleted_id.as_str())
                        .and_modify(|e| {
                            if idx > e.0 {
                                *e = (idx, id.as_str());
                            }
                        })
                        .or_insert((idx, id.as_str()));
                }
            }
        }
        for (deleted_id, (t_idx, t_id)) in &last_tombstone {
            let restored = last_record_idx
                .get(deleted_id)
                .is_some_and(|n_idx| n_idx > t_idx);
            if !restored {
                tombstone_by_deleted.insert(deleted_id, t_id);
            }
        }

        // Superseded IDs: a record replaced by a newer one is non-current, the same
        // way the store's current-state read and the memory query paths treat it.
        // This covers both superseded observations and superseded code handles
        // (e.g. a renamed/replaced symbol), via the node's `superseded_by` field, a
        // live `SUPERSEDES` edge, or a `SUPERSEDES` evidence link.
        let mut superseded_ids: BTreeSet<&str> = BTreeSet::new();
        // The node's own `superseded_by` marker is decided from the LATEST physical
        // row per stable ID, so a restored row re-emitted without the marker clears
        // an earlier supersession — mirroring tombstone restoration.
        let mut last_node_superseded: BTreeMap<&str, bool> = BTreeMap::new();
        for record in records {
            match record {
                GraphRecord::Node {
                    id,
                    superseded_by,
                    evidence_links,
                    ..
                } => {
                    last_node_superseded.insert(
                        id.as_str(),
                        superseded_by.as_deref().is_some_and(|s| !s.is_empty()),
                    );
                    // A `SUPERSEDES` evidence link is honored only when its source
                    // (this node) is not itself actively tombstoned — a retracted
                    // superseding note must not hide the older record.
                    if let Some(links) = evidence_links
                        && !tombstone_by_deleted.contains_key(id.as_str())
                    {
                        for link in links {
                            if link.relation == crate::ir::EdgeLabel::Supersedes.as_str()
                                && let Some(target) = link.target_record_id.as_deref()
                                && !target.is_empty()
                            {
                                superseded_ids.insert(target);
                            }
                        }
                    }
                }
                GraphRecord::Edge {
                    id,
                    label: crate::ir::EdgeLabel::Supersedes,
                    source,
                    target,
                    ..
                } if !tombstone_by_deleted.contains_key(id.as_str())
                    && !tombstone_by_deleted.contains_key(source.as_str()) =>
                {
                    // Honor a standalone SUPERSEDES edge only when neither the edge
                    // nor its superseding source node has been retracted.
                    superseded_ids.insert(target.as_str());
                }
                _ => {}
            }
        }
        for (id, superseded) in &last_node_superseded {
            if *superseded {
                superseded_ids.insert(id);
            }
        }

        for record in records {
            if let GraphRecord::Node { id, kind, .. } = record {
                kind_by_id.insert(id.as_str(), *kind);
                if is_code_handle_kind(*kind) {
                    all_code_handles.push(record);
                    // A superseded handle is kept for anchor lookup (above) but is
                    // not a current code fact, so it is excluded from the live index
                    // — a citation to a renamed/replaced symbol resolves `unresolved`.
                    if !tombstone_by_deleted.contains_key(id.as_str())
                        && !superseded_ids.contains(id.as_str())
                    {
                        live_code_by_id.entry(id).or_default().push(record);
                    }
                } else if let (NodeKind::SemanticDrift, Some(drift)) =
                    (*kind, record_semantic_drift(record))
                {
                    // A retracted (tombstoned) or superseded drift record is no
                    // longer part of current memory and must not trigger a
                    // `drifted` verdict — the same current-state filter applied to
                    // code handles above. A replaced/recomputed measurement that
                    // was superseded rather than tombstoned is excluded here too,
                    // which also keeps it out of the `DRIFTS_PRIOR` edge recovery
                    // path (it reads `drift_meta_by_id`).
                    if !tombstone_by_deleted.contains_key(id.as_str())
                        && !superseded_ids.contains(id.as_str())
                    {
                        drift_meta_by_id.insert(id.as_str(), drift);
                        drifts_by_prior
                            .entry(drift.prior_record_id.as_str())
                            .or_default()
                            .push((id.as_str(), drift));
                    }
                }
            }
        }

        // Frontier liveness: `scan-history` emits a full snapshot at every commit
        // but no `Tombstone` when a symbol is removed or renamed, so a handle that
        // exists only at older commits would otherwise look live and a citation to
        // it could be reported `current`/`drifted` instead of `unresolved`.
        //
        // The frontier is the set of **tip commits** — commits that are no other
        // commit's parent (HEAD and any branch tips). A temporal handle is live
        // only when it has a version at a tip; a handle present solely at interior
        // (superseded) commits has been removed. Using tips rather than the latest
        // committer timestamp is robust to clock skew, rebases, and merged side
        // branches: HEAD is always a tip, so code at HEAD is never falsely
        // `unresolved`. A non-temporal (current-tree `scan`) version has no commit
        // and keeps its handle live.
        //
        // The DAG is built from *every* temporal record — `Commit`/`Change` nodes
        // included — not just code handles. A HEAD commit that deletes the last
        // code file emits no code-handle version but still emits a `Commit` node,
        // so without it the deletion commit would be missing from the graph, the
        // prior commit would look like the tip, and a citation to the now-deleted
        // code would be reported `current` instead of `unresolved`.
        //
        // Tips are partitioned **per owning repository** (issue #203). In a shared
        // multi-repo store two histories can share a commit SHA, so a commit that is
        // one repository's HEAD (a tip) can be another repository's interior commit.
        // A store-wide `all − parents` set would drop that HEAD from the tip set and
        // falsely report its live HEAD code `unresolved`. Each commit is attributed
        // to its repository through the deterministic containment topology
        // (`Repository → CONTAINS → Commit`, and code handles via
        // `Repository → CONTAINS → File → DEFINES → …`) that `RepositoryIndex`
        // already indexes; tips are then `all − parents` **within** each repository
        // and unioned. Records with no attributable repository — legacy stores with
        // no `Repository` node — share one `None` bucket that is byte-identical to
        // the previous global computation, and a single-repository store is a single
        // bucket, so the primary `scan-history` workflow is unchanged.
        let repo_index = crate::query::RepositoryIndex::build(records);
        let mut per_repo_all: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
        let mut per_repo_parents: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
        let mut commit_children: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        // Commits that participate in any parent/child edge. Ancestry is trusted
        // (descendant-only comparison) for an anchor only when the anchor itself is
        // in this set; an anchor with no ancestry metadata falls back to timestamps
        // even if unrelated histories in the same store carry edges.
        let mut dag_commits: BTreeSet<&str> = BTreeSet::new();
        for record in records {
            if let Some(commit) = version_commit(record) {
                let owner = match record {
                    GraphRecord::Node { id, .. } => repo_index.owner_of(id.as_str()),
                    _ => None,
                };
                per_repo_all.entry(owner).or_default().insert(commit);
                for parent in version_parents(record) {
                    per_repo_parents
                        .entry(owner)
                        .or_default()
                        .insert(parent.as_str());
                    dag_commits.insert(parent.as_str());
                    dag_commits.insert(commit);
                    commit_children
                        .entry(parent.as_str())
                        .or_default()
                        .insert(commit);
                }
            }
        }
        // Per-repository tip sets. A commit is a tip when it is no other commit's
        // parent **within its own repository** (issue #203). The store-wide union
        // (`tips`) is retained only as the fallback bucket for handles with no
        // attributable repository (legacy no-`Repository` stores) and for
        // `has_ancestry` checks; every liveness/frontier check — the retains below
        // AND the runtime triple resolvers (`resolve_triple`, the anchored-lineage
        // check) via the stored `per_repo_tips`/`per_repo_frontier` (Codex round-2
        // findings #454/#559) — is scoped **within each handle's owning
        // repository**. Codex round-1 finding #1: in a shared history two
        // repositories can share a commit SHA that is one repo's HEAD (a tip) yet
        // the other's interior commit, so a store-wide `tips.contains(c)` would keep
        // the interior repo's deleted/moved handle live and report a citation to it
        // `current`/`drifted` instead of `unresolved`.
        let empty_parents: BTreeSet<&str> = BTreeSet::new();
        let mut per_repo_tips: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
        let mut tips: BTreeSet<&str> = BTreeSet::new();
        for (owner, all) in &per_repo_all {
            let parents = per_repo_parents.get(owner).unwrap_or(&empty_parents);
            let repo_tips: BTreeSet<&str> = all.difference(parents).copied().collect();
            tips.extend(repo_tips.iter().copied());
            per_repo_tips.insert(*owner, repo_tips);
        }
        if !tips.is_empty() {
            live_code_by_id.retain(|id, versions| {
                // Check tip membership within the handle's OWN owning repository.
                // A handle whose repository cannot be attributed (a legacy
                // single-repo store with no `Repository` node) shares the `None`
                // bucket, whose tip set equals the old global computation, so those
                // stores stay byte-identical; a missing/empty owner bucket falls
                // back to the store-wide union.
                let repo_tips = per_repo_tips
                    .get(&repo_index.owner_of(id))
                    .filter(|set| !set.is_empty())
                    .unwrap_or(&tips);
                versions.iter().any(|r| {
                    // A non-temporal (current-tree) version keeps the handle live.
                    version_commit(r).is_none_or(|c| repo_tips.contains(c))
                })
            });
        }

        // Transaction-time frontier for repeated current-tree scans (issue #204),
        // computed PER owning repository (Codex finding #3) and sourced from the
        // repository/source-snapshot node's valid_time as well as code-handle
        // valid_times (Codex finding #2).
        //
        // A full `scan`/`refresh` emits every current handle with a node-level
        // `valid_time` but no `temporal` commit and no `Tombstone` when a handle is
        // deleted between two scans. The commit-tip frontier above cannot see such a
        // deletion (these versions carry no commit), so a purely non-temporal handle
        // is live only when it has a version at its repository's newest scan
        // `valid_time`; one present solely at older scans was removed. This mirrors
        // the history path's tip frontier on the transaction-time axis and leaves
        // the commit-anchored history workflow untouched (a handle with any temporal
        // version is governed by the commit tips above and skipped here).
        //
        // Finding #3: a shared store can hold current-tree scans for more than one
        // repository taken at different times. A single global frontier would prune
        // every handle of the older-scanned repository (it has no version at the
        // newer repo's scan time), falsely reporting valid citations `unresolved`.
        // Each handle is pruned against ITS OWN repository's newest scan instead.
        //
        // Finding #2: a repeated scan that deletes the LAST source file/symbol in a
        // repository re-emits that repo's `Repository` (source-snapshot) node with
        // the new scan's node-level valid_time but no code-handle version. Deriving
        // the frontier only from code handles would never see the newer scan, so the
        // deleted handle would stay live. Folding the snapshot node's valid_time in
        // lets an empty latest scan still advance the frontier and prune the prior
        // handles.
        //
        // A handle whose repository cannot be attributed shares the `None` bucket,
        // whose frontier equals the old global newest-scan computation, so legacy
        // single-repo stores stay byte-identical. Timestamps are compared by parsed
        // instant; two scans that collapse to the same instant are the documented
        // tie — both count as the frontier, so a deletion is only observable across
        // scans with distinct `valid_time`s.
        let mut per_repo_frontier: BTreeMap<Option<&str>, &str> = BTreeMap::new();
        // Code-handle snapshot times, attributed to each handle's repository.
        for (id, versions) in &live_code_by_id {
            let owner = repo_index.owner_of(id);
            for record in versions {
                if version_commit(record).is_none()
                    && let Some(vt) = version_valid(record)
                {
                    let cur = per_repo_frontier.entry(owner).or_insert(vt);
                    if time_cmp(vt, cur) == std::cmp::Ordering::Greater {
                        *cur = vt;
                    }
                }
            }
        }
        // Repository/source-snapshot node times (finding #2): a scan that deletes a
        // repository's last handle still re-emits its `Repository` node with the new
        // valid_time, so an empty latest scan advances the frontier.
        for record in records {
            if let GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                ..
            } = record
                && let Some(vt) = version_valid(record)
            {
                let owner = repo_index.owner_of(id);
                let cur = per_repo_frontier.entry(owner).or_insert(vt);
                if time_cmp(vt, cur) == std::cmp::Ordering::Greater {
                    *cur = vt;
                }
            }
        }
        if !per_repo_frontier.is_empty() {
            live_code_by_id.retain(|id, versions| {
                let has_temporal = versions.iter().any(|r| version_commit(r).is_some());
                let has_non_temporal = versions.iter().any(|r| version_commit(r).is_none());
                // A PURE-history handle (every version commit-anchored) is governed
                // by the commit-tip frontier above — never pruned here.
                if has_temporal && !has_non_temporal {
                    return true;
                }
                // Prune against the handle's OWN repository's newest scan. With no
                // frontier for this owner there is nothing to prune against.
                let Some(&frontier) = per_repo_frontier.get(&repo_index.owner_of(id)) else {
                    return true;
                };
                // Liveness on the current-tree scan axis: a NON-temporal version at
                // the repository's newest scan `valid_time`.
                let at_scan_frontier = versions.iter().any(|r| {
                    version_commit(r).is_none()
                        && version_valid(r)
                            .is_some_and(|vt| time_cmp(vt, frontier) == std::cmp::Ordering::Equal)
                });
                if !has_temporal {
                    // Pure current-tree handle (issue #204): live only at its
                    // repository's newest scan.
                    return at_scan_frontier;
                }
                // MIXED handle (issue #405): one identity-derived ID carrying BOTH a
                // `scan-history` (commit-anchored) version and current-tree `scan`
                // versions — no single command emits both; only a hand-combined store
                // (e.g. `cat graph.jsonl history.graph.jsonl`, or ingesting both into
                // one `--data-dir`) does. It is exempt from neither axis. The two
                // "latest" axes — a commit's committer date (temporal `valid_time`)
                // and a current-tree scan's wall-clock `valid_time` — have no reliable
                // cross-ordering (rebases, clock skew: #203 keys on tips, not
                // timestamps), so we never order them across axes. Instead the handle
                // is live iff present in the latest state of EITHER axis: a temporal
                // version at one of its repository's tip commits, OR a non-temporal
                // version at the newest scan. A symbol present at a tip commit stays
                // `current` (never a false `unresolved` — the regression a naive "must
                // also be at the scan frontier" predicate would cause); a symbol
                // present at the newest scan stays `current`; a symbol absent from
                // BOTH latest states is pruned to `unresolved`, closing the gap where
                // a mixed handle was pruned by neither frontier.
                if at_scan_frontier {
                    return true;
                }
                let repo_tips = per_repo_tips
                    .get(&repo_index.owner_of(id))
                    .filter(|set| !set.is_empty())
                    .unwrap_or(&tips);
                versions
                    .iter()
                    .any(|r| version_commit(r).is_some_and(|c| repo_tips.contains(c)))
            });
        }

        // Stored per-repository views for the runtime frontier resolvers (Codex
        // round-2 findings #454/#559). Round 1 scoped the build-time liveness
        // retains above but left `resolve_triple` and the anchored-lineage check on
        // the store-wide `tips` union; these owned-key copies let those resolvers
        // scope the tip/frontier check to each candidate handle's OWN repository.
        // Keys are owned so the maps can outlive the local `repo_index`; the `None`
        // bucket is byte-identical to the old global computation for single-repo /
        // legacy stores. `owner_by_handle` records each surviving live handle's
        // owner so a resolver looks its repository up in O(log n).
        let per_repo_tips: BTreeMap<Option<String>, BTreeSet<&str>> = per_repo_tips
            .iter()
            .map(|(owner, set)| ((*owner).map(str::to_owned), set.clone()))
            .collect();
        let per_repo_frontier: BTreeMap<Option<String>, &str> = per_repo_frontier
            .iter()
            .map(|(owner, vt)| ((*owner).map(str::to_owned), *vt))
            .collect();
        let owner_by_handle: BTreeMap<&str, Option<String>> = live_code_by_id
            .keys()
            .map(|id| (*id, repo_index.owner_of(id).map(str::to_owned)))
            .collect();

        // Second pass: index drift triggers from `DRIFTS_PRIOR` edges too. The
        // edge target is the prior (cited-side) symbol, the same neighbor-safe key
        // as `prior_record_id`, so this only adds drifts a stale metadata ID would
        // have missed — never a sibling's drift.
        for record in records {
            if let GraphRecord::Edge {
                id,
                label: crate::ir::EdgeLabel::DriftsPrior,
                source,
                target,
                ..
            } = record
                && !tombstone_by_deleted.contains_key(id.as_str())
                && let Some(drift) = drift_meta_by_id.get(source.as_str())
            {
                let entry = drifts_by_prior.entry(target.as_str()).or_default();
                if !entry.iter().any(|(id, _)| *id == source.as_str()) {
                    entry.push((source.as_str(), drift));
                }
            }
        }

        Self {
            live_code_by_id,
            tombstone_by_deleted,
            drifts_by_prior,
            kind_by_id,
            all_code_handles,
            commit_children,
            superseded_ids,
            tips,
            dag_commits,
            per_repo_tips,
            per_repo_frontier,
            owner_by_handle,
        }
    }

    /// Tip-commit set scoped to the handle's owning repository (Codex #454),
    /// falling back to the store-wide union for an unattributable or empty owner
    /// bucket — byte-identical for single-repo / legacy stores.
    fn repo_tips_for(&self, handle_id: &str) -> &BTreeSet<&'a str> {
        self.owner_by_handle
            .get(handle_id)
            .and_then(|owner| self.per_repo_tips.get(owner))
            .filter(|set| !set.is_empty())
            .unwrap_or(&self.tips)
    }

    /// Current-tree scan frontier `valid_time` scoped to the handle's owning
    /// repository (Codex #559), when one was recorded.
    fn repo_frontier_for(&self, handle_id: &str) -> Option<&'a str> {
        self.owner_by_handle
            .get(handle_id)
            .and_then(|owner| self.per_repo_frontier.get(owner))
            .copied()
    }

    /// True when `commit` participates in the commit DAG (has a parent or child
    /// edge). Descendant reachability is authoritative for a commit-anchored
    /// comparison only when its anchor is in the DAG; an anchor with no ancestry
    /// metadata falls back to committer timestamps even if unrelated histories in
    /// the same store carry edges (so a mixed/pruned graph is not over-filtered).
    fn has_ancestry_for(&self, commit: &str) -> bool {
        self.dag_commits.contains(commit)
    }

    /// Commits reachable forward from `commit` through child edges (its
    /// descendants, excluding `commit` itself). A version at any of these is a
    /// later code state than an anchor at `commit`, regardless of committer
    /// timestamps (rebases/clock skew can backdate a descendant).
    fn descendants_of(&self, commit: &str) -> BTreeSet<&'a str> {
        let mut seen: BTreeSet<&'a str> = BTreeSet::new();
        let mut stack: Vec<&str> = vec![commit];
        while let Some(current) = stack.pop() {
            if let Some(children) = self.commit_children.get(current) {
                for &child in children {
                    if seen.insert(child) {
                        stack.push(child);
                    }
                }
            }
        }
        seen
    }

    /// True when `record_id` resolves to a codegraph record that is *not* a code
    /// handle (e.g. a `Commit`/`Change`), so a valid `EXPLAINS_CHANGE`-style link
    /// is skipped instead of being mis-reported as an `unresolved` handle.
    fn is_non_handle_target(&self, record_id: &str) -> bool {
        self.kind_by_id
            .get(record_id)
            .is_some_and(|kind| !is_code_handle_kind(*kind))
    }

    /// Synthesizes evidence-link citations from graph **edges** whose source is an
    /// agent observation and whose target is a code handle (`OBSERVES`,
    /// `MENTIONS_SYMBOL`, `TOUCHED_FILE`, …), keyed by the source record ID.
    ///
    /// An `Observation`/`Decision` can cite code through the graph-edge form
    /// instead of an inline `evidence_links` array; existing query paths honor
    /// both, so freshness must too or a store with edge-only memory links silently
    /// drops those notes. Only evidence-link labels are considered, and only when
    /// the target resolves to a code-handle node kind — so a `Commit`/`Change`
    /// target (`EXPLAINS_CHANGE`/`CHANGED_IN`) or a cross-domain target is never
    /// mis-synthesized as a stale code handle. A tombstoned or superseded citation
    /// edge is skipped, matching the current-state view used elsewhere.
    fn edge_code_citations(
        &self,
        records: &'a [GraphRecord],
    ) -> BTreeMap<&'a str, Vec<EvidenceLink>> {
        let mut by_source: BTreeMap<&str, Vec<EvidenceLink>> = BTreeMap::new();
        for record in records {
            let GraphRecord::Edge {
                id,
                label,
                source,
                target,
                confidence,
                temporal,
                ..
            } = record
            else {
                continue;
            };
            if !label.is_evidence_link_label() {
                continue;
            }
            // A retracted or replaced citation edge is not part of current memory.
            if self.tombstone_by_deleted.contains_key(id.as_str())
                || self.superseded_ids.contains(id.as_str())
            {
                continue;
            }
            // Only code-handle targets are freshness-relevant. When the target
            // node is present, trust its kind (this also excludes `Commit`/`Change`
            // and cross-domain targets). When the target is absent from the slice
            // (tombstoned-and-pruned, or omitted), fall back to the citation
            // relation: `MENTIONS_SYMBOL` / `TOUCHED_FILE` are unambiguous
            // code-handle citations from the link-evidence workflow, so the note
            // still resolves to `unresolved` instead of being silently dropped —
            // matching how an inline link to the same absent handle is reported.
            let target_is_handle = self.kind_by_id.get(target.as_str()).map_or_else(
                || is_code_handle_relation(*label),
                |kind| is_code_handle_kind(*kind),
            );
            if !target_is_handle {
                continue;
            }
            let anchor_commit = temporal
                .as_ref()
                .map(|t| t.git_commit.clone())
                .filter(|s| !s.is_empty());
            by_source
                .entry(source.as_str())
                .or_default()
                .push(EvidenceLink {
                    target_record_id: Some(target.clone()),
                    target_domain: "codegraph".to_owned(),
                    relation: label.as_str().to_owned(),
                    confidence: confidence.clone().unwrap_or_default(),
                    as_of_commit: anchor_commit,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                });
        }
        by_source
    }

    /// Resolves an unanchored triple `(path, span)` to a live code node ID against
    /// the **frontier** only, scoped **per owning repository** (Codex round-2
    /// findings #454/#559): a commit-anchored version is matched only when it sits
    /// at a tip commit of its OWN repository, and a non-temporal (current-tree)
    /// version only when its `valid_time` equals its OWN repository's newest scan.
    /// Round 1 checked tip membership against the store-wide union and treated every
    /// non-temporal version as frontier, so a SHA that is another repo's HEAD kept an
    /// interior version resolvable (#454), and an older-scan span still resolved
    /// across repeated current-tree scans (#559). A span that matched only a
    /// historical / older-scan (non-frontier) version now no longer resolves, so a
    /// note recorded against a since-moved span is `unresolved` rather than a stale
    /// `current`/`drifted`. An unattributable handle falls back to the store-wide
    /// union / global frontier, so single-repo / legacy stores are byte-identical.
    ///
    /// A spanned triple targets a symbol/module/import, never the file: if no live
    /// frontier handle matches the span it returns `None` (→ `unresolved`), and only
    /// a path-only triple (no span) falls back to the file.
    ///
    /// A triple with no `target_record_id` carries no repository identity, so in a
    /// shared multi-repo store the same `(path, span)` can match handles in more
    /// than one repository. Such ambiguous matches are **not** silently resolved
    /// to an arbitrary one — they return `None` so the verdict is `unresolved`.
    fn resolve_triple(&self, path: &str, span: Option<&SourceSpan>) -> Option<&'a str> {
        let mut span_matches: BTreeSet<&str> = BTreeSet::new();
        let mut file_matches: BTreeSet<&str> = BTreeSet::new();
        for versions in self.live_code_by_id.values() {
            for record in versions {
                let GraphRecord::Node {
                    id,
                    kind,
                    repo_relative_path: Some(rp),
                    span: node_span,
                    ..
                } = record
                else {
                    continue;
                };
                if rp != path {
                    continue;
                }
                // Match against the frontier only, scoped to the candidate handle's
                // OWN repository (Codex round-2 findings #454/#559). A commit-anchored
                // version must sit at a tip of its own repository — a SHA that is
                // another repo's HEAD but this repo's interior commit is not a tip
                // here, so its since-moved span no longer resolves (#454). A
                // non-temporal (current-tree) version counts as frontier only when its
                // valid_time equals its repository's newest scan, so a span the symbol
                // occupied only at an older scan no longer resolves (#559).
                let at_frontier = version_commit(record).map_or_else(
                    || {
                        // A non-temporal (current-tree) version is frontier only at
                        // its repo's newest scan (#559); no frontier ⇒ nothing to
                        // prune against, keep it.
                        self.repo_frontier_for(id.as_str()).is_none_or(|frontier| {
                            version_valid(record).is_some_and(|vt| {
                                time_cmp(vt, frontier) == std::cmp::Ordering::Equal
                            })
                        })
                    },
                    |commit| {
                        // A commit-anchored version must sit at a tip of its OWN
                        // repository (#454); an empty scoped set keeps it (no
                        // ancestry to prune against).
                        let repo_tips = self.repo_tips_for(id.as_str());
                        repo_tips.is_empty() || repo_tips.contains(commit)
                    },
                );
                if !at_frontier {
                    continue;
                }
                match kind {
                    // An exact span match is a direct hit for any code handle —
                    // including a `File` cited by its own recorded span — so a
                    // file-level triple resolves instead of being reported absent.
                    k if is_code_handle_kind(*k)
                        && span.is_some()
                        && node_span.as_ref() == span =>
                    {
                        span_matches.insert(id.as_str());
                    }
                    // Otherwise a `File` is the path-only fallback (no recorded span).
                    NodeKind::File => {
                        file_matches.insert(id.as_str());
                    }
                    _ => {}
                }
            }
        }
        // A spanned citation never falls back to the whole file; only a path-only
        // triple does.
        if span.is_some() {
            unique_match(&span_matches)
        } else {
            unique_match(&file_matches)
        }
    }

    /// Resolves a triple `(path, span)` to the code node that occupied it **at the
    /// anchor commit**, scanning historical and tombstoned versions too. Honoring
    /// the commit keeps a citation bound to the identity it named: if that symbol
    /// was later removed or renamed and a different live symbol reused the same
    /// path/span, this returns the original (now-tombstoned) ID, so the verdict is
    /// `unresolved` rather than silently re-pointing at the new occupant.
    fn resolve_triple_at_commit(
        &self,
        path: &str,
        span: Option<&SourceSpan>,
        commit: &str,
    ) -> TripleResolution<'a> {
        let mut span_matches: BTreeSet<&str> = BTreeSet::new();
        let mut file_matches: BTreeSet<&str> = BTreeSet::new();
        for record in &self.all_code_handles {
            let GraphRecord::Node {
                id,
                kind,
                repo_relative_path: Some(rp),
                span: node_span,
                temporal,
                ..
            } = record
            else {
                continue;
            };
            if rp != path {
                continue;
            }
            if temporal.as_ref().map(|t| t.git_commit.as_str()) != Some(commit) {
                continue;
            }
            match kind {
                // An exact span match is a direct hit for any code handle —
                // including a `File` cited by its own recorded span — so a
                // file-level triple resolves instead of being reported absent.
                k if is_code_handle_kind(*k) && span.is_some() && node_span.as_ref() == span => {
                    span_matches.insert(id.as_str());
                }
                // Otherwise a `File` is the path-only fallback (no recorded span).
                NodeKind::File => {
                    file_matches.insert(id.as_str());
                }
                _ => {}
            }
        }
        // A spanned citation never falls back to the whole file (a moved/removed
        // symbol must be `unresolved`, not the file). Ambiguity (same
        // path/span/commit across repositories) is distinguished from absence so
        // the caller does not silently fall back to the live frontier on ambiguity.
        let matches = if span.is_some() {
            &span_matches
        } else {
            &file_matches
        };
        match matches.len() {
            0 => TripleResolution::Absent,
            1 => TripleResolution::Resolved(matches.iter().next().copied().expect("one match")),
            _ => TripleResolution::Ambiguous,
        }
    }

    /// Resolves a link's cited handle to a code node ID: the explicit
    /// `target_record_id` first, then a `(path, span)` triple resolved at the
    /// link's identity commit (falling back to the live frontier). Shared by
    /// classification and the edge-dedupe key so a triple-only inline link and a
    /// daemon-materialized record-ID edge for the *same* citation resolve to the
    /// same ID and are not double-counted.
    fn resolve_cited_id<'l>(&'l self, link: &'l EvidenceLink) -> Option<&'l str> {
        let anchor_commit = link
            .as_of_commit
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| link.target_git_commit.as_deref().filter(|s| !s.is_empty()));
        let identity_commit = link
            .target_git_commit
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(anchor_commit);
        link.target_record_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                link.target_repo_relative_path.as_deref().and_then(|p| {
                    let span = link.target_span.as_ref();
                    match identity_commit
                        .map(|commit| self.resolve_triple_at_commit(p, span, commit))
                    {
                        Some(TripleResolution::Resolved(id)) => Some(id),
                        Some(TripleResolution::Ambiguous) => None,
                        Some(TripleResolution::Absent) | None => self.resolve_triple(p, span),
                    }
                })
            })
    }

    /// Dedupe key identifying one citation by `(resolved_record_id, relation,
    /// anchor)`, used to skip a synthesized edge that an inline link already
    /// covers. Resolves triples so a triple-only inline link and a materialized
    /// record-ID edge for the same citation share a key. `None` when the handle
    /// does not resolve to any record ID in this slice.
    fn covered_key(&self, link: &EvidenceLink) -> Option<(String, String, String)> {
        let rid = self.resolve_cited_id(link)?;
        let anchor = link
            .as_of_commit
            .clone()
            .or_else(|| link.target_git_commit.clone())
            .unwrap_or_default();
        Some((rid.to_owned(), link.relation.clone(), anchor))
    }
}

/// Outcome of resolving a triple citation at a specific commit.
enum TripleResolution<'a> {
    /// A single code node occupied the path/span at the commit.
    Resolved(&'a str),
    /// More than one repository matched — not safely resolvable.
    Ambiguous,
    /// No code node matched the path/span at the commit in this slice.
    Absent,
}

/// Returns the single element of `matches`, or `None` when it is empty or
/// ambiguous (more than one distinct record ID).
fn unique_match<'a>(matches: &BTreeSet<&'a str>) -> Option<&'a str> {
    match matches.len() {
        1 => matches.iter().next().copied(),
        _ => None,
    }
}

/// Repo-relative path and span of a node record, both `None` for non-nodes.
fn node_path_span(record: &GraphRecord) -> (Option<String>, Option<SourceSpan>) {
    match record {
        GraphRecord::Node {
            repo_relative_path,
            span,
            ..
        } => (repo_relative_path.clone(), *span),
        _ => (None, None),
    }
}

/// Borrows the semantic-drift metadata from a node record, if present.
const fn record_semantic_drift(record: &GraphRecord) -> Option<&SemanticDriftMetadata> {
    match record {
        GraphRecord::Node {
            semantic_drift: Some(drift),
            ..
        } => Some(drift),
        _ => None,
    }
}

/// Code-graph node kinds that can be cited as a code handle.
const fn is_code_handle_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::File | NodeKind::Symbol | NodeKind::Module | NodeKind::Import
    )
}

/// Evidence-link relations that cite a codegraph `Commit`/`Change`, not a code
/// handle. These are skipped by relation so an absent target node is never
/// mis-reported as an `unresolved` handle.
fn is_non_handle_relation(relation: &str) -> bool {
    relation == crate::ir::EdgeLabel::ExplainsChange.as_str()
        || relation == crate::ir::EdgeLabel::ChangedIn.as_str()
}

/// Evidence-link relations that unambiguously cite a **code handle** (a file or
/// symbol). Used to synthesize an edge-backed citation when the target node is
/// absent from the slice (tombstoned-and-pruned or omitted) and its kind can no
/// longer be read — these labels are the ones the import/`link-evidence`
/// workflow emits for code, so a stale edge-only note still resolves rather than
/// being dropped. Deliberately narrow (excludes the more general `OBSERVES`,
/// which can target non-code) so an absent non-code target is not mis-flagged.
const fn is_code_handle_relation(label: crate::ir::EdgeLabel) -> bool {
    matches!(
        label,
        crate::ir::EdgeLabel::MentionsSymbol
            | crate::ir::EdgeLabel::TouchedFile
            | crate::ir::EdgeLabel::TouchesFile
    )
}

/// True when an evidence link cites a deterministic code handle that should be
/// classified — a codegraph link that is neither a non-handle relation
/// (`EXPLAINS_CHANGE`/`CHANGED_IN`) nor an explicit non-handle target
/// (`Commit`/`Change`).
fn is_classifiable_code_link(index: &FreshnessIndex<'_>, link: &EvidenceLink) -> bool {
    if link.target_domain != "codegraph" {
        return false;
    }
    if is_non_handle_relation(&link.relation) {
        return false;
    }
    !link
        .target_record_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .is_some_and(|rid| index.is_non_handle_target(rid))
}

/// Observation kinds whose evidence links are checked for freshness.
const fn is_observation_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Observation | NodeKind::Decision)
}

// ── Public API ──────────────────────────────────────────────────────────────────

/// Computes per-evidence-link freshness verdicts for every agent observation
/// that cites a code handle.
///
/// Strictly read-only and deterministic: the returned verdicts are sorted by
/// observation ID, then cited handle, so the same store yields byte-identical
/// output across runs (AC8). No records are created, modified, or deleted.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn evidence_link_freshness(records: &[GraphRecord]) -> Vec<FreshnessVerdictEntry> {
    let index = FreshnessIndex::build(records);
    let mut entries: Vec<FreshnessVerdictEntry> = Vec::new();
    // Freshness reads the history-inclusive store view so superseded *code*
    // versions are available for comparison, but that view also surfaces
    // superseded and re-ingested *memory* rows. Collapse observations to the
    // current view — skip superseded notes (via the index's order-independent
    // superseded set) and emit each observation ID once — so a re-ingest or
    // retained older version cannot yield duplicate or non-current verdicts.
    let superseded_ids = &index.superseded_ids;
    let mut seen_observations: BTreeSet<&str> = BTreeSet::new();
    // Edge-backed citations: an observation can cite code through a graph edge
    // (`OBSERVES`, `MENTIONS_SYMBOL`, `TOUCHED_FILE`, …) rather than an inline
    // evidence link. Synthesize those once and merge them with each note's inline
    // links so edge-only memory is not silently dropped from freshness.
    let edge_citations = index.edge_code_citations(records);

    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            evidence_links,
            agent_id,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            redaction_policy_version,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if !is_observation_kind(*kind) {
            continue;
        }
        let inline_links = evidence_links.as_deref().unwrap_or(&[]);
        let edge_links = edge_citations
            .get(id.as_str())
            .map_or(&[][..], Vec::as_slice);
        // A note with neither inline nor edge-backed code citations has nothing to
        // classify.
        if inline_links.is_empty() && edge_links.is_empty() {
            continue;
        }
        // A retracted note is no longer part of current memory: a tombstoned
        // observation yields no verdicts, matching the current-state reads used by
        // the memory query paths.
        if index.tombstone_by_deleted.contains_key(id.as_str()) {
            continue;
        }
        // A superseded note has been replaced by a newer one; the current view
        // excludes it, just as the store's default read does. Checked against the
        // first-pass set so the marker is honored regardless of row order.
        if superseded_ids.contains(id.as_str()) {
            continue;
        }
        // Dedupe re-ingested physical rows of the same observation (same stable
        // ID ⇒ byte-identical content), so each note is classified once.
        if !seen_observations.insert(id.as_str()) {
            continue;
        }

        let provenance = VerdictProvenance {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            observed_at: observed_at.clone(),
            confidence: confidence.clone(),
            redaction_policy_version: redaction_policy_version.clone(),
        };
        // Anchor order: an explicit `valid_time` (when the fact was true) first,
        // then the recording time `observed_at`/`ingested_at` — most notes carry a
        // recording time but no valid-time, and "drifted since recording" is the
        // whole point, so a recording time is a usable anchor before `untemporal`.
        let obs_valid_time = valid_time
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| observed_at.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| ingested_at.as_deref().filter(|s| !s.is_empty()));

        // Classify every inline evidence link as-is: two inline links to the same
        // handle with distinct recorded spans (e.g. two snippets in one file) are
        // distinct citations and each gets its own verdict — they are never
        // collapsed. Record each inline citation's (record, relation, anchor) key
        // so a *synthesized edge* to the same handle is treated as the same
        // citation rather than a duplicate verdict.
        let mut edge_covered: BTreeSet<(String, String, String)> = BTreeSet::new();
        for link in inline_links {
            if !is_classifiable_code_link(&index, link) {
                continue;
            }
            // Mark this citation covered by its *resolved* record ID — resolving a
            // triple-only link too, so a daemon-materialized record-ID edge for the
            // same citation is recognized as a duplicate and not double-counted.
            if let Some(key) = index.covered_key(link) {
                edge_covered.insert(key);
            }
            entries.push(classify_link(
                &index,
                id,
                kind.as_str(),
                &provenance,
                obs_valid_time,
                link,
            ));
        }
        // Then classify edge-backed citations, skipping any an inline link (or an
        // earlier edge) already covers. A synthesized edge always carries a record
        // ID, so it is deduped by record alone — it has no span to distinguish.
        for link in edge_links {
            if !is_classifiable_code_link(&index, link) {
                continue;
            }
            if let Some(key) = index.covered_key(link)
                && !edge_covered.insert(key)
            {
                continue;
            }
            entries.push(classify_link(
                &index,
                id,
                kind.as_str(),
                &provenance,
                obs_valid_time,
                link,
            ));
        }
    }

    entries.sort_by(|a, b| {
        a.observation_id
            .cmp(&b.observation_id)
            .then_with(|| sort_key(&a.cited_handle).cmp(&sort_key(&b.cited_handle)))
    });
    entries
}

/// Filters verdicts to the stale set (`drifted` + `unresolved`) for the
/// stale-only query mode (AC7).
#[must_use]
pub fn stale_only(entries: Vec<FreshnessVerdictEntry>) -> Vec<FreshnessVerdictEntry> {
    entries
        .into_iter()
        .filter(|e| e.verdict.is_stale())
        .collect()
}

/// Tallies verdicts by class for the report header.
#[must_use]
pub fn verdict_counts(entries: &[FreshnessVerdictEntry]) -> BTreeMap<&'static str, usize> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    counts.insert("current", 0);
    counts.insert("drifted", 0);
    counts.insert("unresolved", 0);
    counts.insert("untemporal", 0);
    for entry in entries {
        *counts.entry(entry.verdict.as_str()).or_insert(0) += 1;
    }
    counts
}

// ── Internal classification ──────────────────────────────────────────────────────

fn sort_key(handle: &CitedHandle) -> (String, String, String, String) {
    (
        handle.target_record_id.clone().unwrap_or_default(),
        handle.repo_relative_path.clone().unwrap_or_default(),
        handle.relation.clone(),
        handle.anchor_commit.clone().unwrap_or_default(),
    )
}

#[allow(clippy::too_many_lines)]
fn classify_link(
    index: &FreshnessIndex<'_>,
    observation_id: &str,
    kind: &str,
    provenance: &VerdictProvenance,
    obs_valid_time: Option<&str>,
    link: &crate::ir::EvidenceLink,
) -> FreshnessVerdictEntry {
    // Freshness anchor: when was the cited state recorded. `as_of_commit` first,
    // then the triple's `target_git_commit`.
    let anchor_commit = link
        .as_of_commit
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| link.target_git_commit.as_deref().filter(|s| !s.is_empty()));

    // Identity-resolution commit: the schema defines `target_git_commit` as the
    // commit at which to resolve the cited node, distinct from the freshness
    // anchor `as_of_commit`. Prefer it so the citation binds to the identity that
    // occupied the path/span at that commit, even if `as_of_commit` later reused it.
    let identity_commit = link
        .target_git_commit
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(anchor_commit);

    // Resolve the cited handle to a code node ID (record ID first, triple next).
    // A triple anchored to a commit is resolved at that commit so the citation
    // stays bound to the identity it named, even if the path/span was later reused
    // by a different symbol; fall back to live resolution only when the anchored
    // version is absent from this slice.
    let cited_id: Option<&str> = link
        .target_record_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            link.target_repo_relative_path.as_deref().and_then(|p| {
                let span = link.target_span.as_ref();
                match identity_commit.map(|commit| index.resolve_triple_at_commit(p, span, commit))
                {
                    Some(TripleResolution::Resolved(id)) => Some(id),
                    // An ambiguous commit-anchored match must not silently fall back
                    // to the live frontier (which could bind to whichever repository
                    // stayed live); leave it unresolved.
                    Some(TripleResolution::Ambiguous) => None,
                    // No version at the anchor commit in this slice: fall back to
                    // resolving against the live frontier.
                    Some(TripleResolution::Absent) | None => index.resolve_triple(p, span),
                }
            })
        });

    let live_versions = cited_id.and_then(|id| index.live_code_by_id.get(id));

    // The reported handle path/span prefers what the evidence link actually
    // recorded (the span the agent cited), then the version at the anchor commit,
    // then any live version — never an arbitrary `versions.first()` (which a
    // history-inclusive read can make an older/unrelated version) when a more
    // authoritative source is available.
    let reference_version = anchor_commit
        .and_then(|commit| {
            live_versions.and_then(|versions| {
                versions
                    .iter()
                    .copied()
                    .find(|r| version_commit(r) == Some(commit))
            })
        })
        .or_else(|| live_versions.and_then(|versions| versions.first().copied()));
    let (version_path, version_span) = reference_version.map_or((None, None), node_path_span);
    let handle_path = link.target_repo_relative_path.clone().or(version_path);
    let handle_span = link.target_span.or(version_span);

    let mut cited_handle = CitedHandle {
        target_record_id: link
            .target_record_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned),
        repo_relative_path: handle_path,
        span: handle_span,
        anchor_commit: anchor_commit.map(ToOwned::to_owned),
        anchor_valid_time: None,
        relation: link.relation.clone(),
        target_domain: link.target_domain.clone(),
    };

    let make =
        |verdict: FreshnessVerdict, trigger: Option<TriggeringHandle>, handle: CitedHandle| {
            FreshnessVerdictEntry {
                observation_id: observation_id.to_owned(),
                kind: kind.to_owned(),
                verdict,
                freshness_lead: verdict.is_stale().then_some(FRESHNESS_LEAD),
                provenance: provenance.clone(),
                cited_handle: handle,
                triggering_handle: trigger,
            }
        };

    // (1) Unresolved is anchor-independent: a handle that points at moved ground
    // is flagged even when the citation carried no anchor.
    if live_versions.is_none_or(Vec::is_empty) {
        let trigger = cited_id
            .and_then(|id| index.tombstone_by_deleted.get(id))
            .map_or(TriggeringHandle::HandleAbsent, |tid| {
                TriggeringHandle::HandleRemoved {
                    tombstone_id: (*tid).to_owned(),
                }
            });
        return make(FreshnessVerdict::Unresolved, Some(trigger), cited_handle);
    }
    let versions = live_versions.expect("live versions present");
    let cited_id = cited_id.expect("resolved id");

    let anchor_descendants = anchor_commit
        .map(|c| index.descendants_of(c))
        .unwrap_or_default();
    // Ancestry is trusted only when *this* anchor commit is in the DAG; otherwise
    // (no parent metadata for the cited handle's history) fall back to timestamps.
    let has_ancestry = anchor_commit.is_some_and(|c| index.has_ancestry_for(c));

    // (1b) Per-anchor liveness. The global tip pruning keeps a handle live when any
    // branch tip still carries its ID, but for a commit-anchored citation the handle
    // must survive on the *anchored* lineage: at the anchor commit or a descendant
    // that is a tip. If the anchored lineage has a frontier in this slice and the
    // handle is absent from it, the symbol was removed downstream of the anchor — a
    // sibling branch still holding it does not make the cited note current.
    if has_ancestry && anchor_commit.is_some() {
        // Tip membership scoped to the cited handle's OWN repository (Codex round-2
        // consistency with #454): the store-wide union could count a foreign repo's
        // tip SHA as on this lineage. Falls back to the union for an unattributable
        // handle, so single-repo / legacy stores are byte-identical.
        let repo_tips = index.repo_tips_for(cited_id);
        let on_lineage_tip = |c: &str| {
            repo_tips.contains(c) && (Some(c) == anchor_commit || anchor_descendants.contains(c))
        };
        let lineage_has_frontier = repo_tips.iter().any(|t| on_lineage_tip(t));
        let live_on_lineage = versions
            .iter()
            .any(|v| version_commit(v).is_some_and(on_lineage_tip));
        if lineage_has_frontier && !live_on_lineage {
            return make(
                FreshnessVerdict::Unresolved,
                Some(TriggeringHandle::HandleAbsent),
                cited_handle,
            );
        }
    }

    // Resolve the anchor valid-time: prefer the cited version recorded at the
    // anchor commit, else the observation's own valid-time.
    let anchor_valid_time: Option<String> = anchor_commit
        .and_then(|commit| version_valid_time(versions, commit))
        .or_else(|| obs_valid_time.map(ToOwned::to_owned));

    // (2) Untemporal: no commit and no valid-time anchor to compare against.
    if anchor_commit.is_none() && anchor_valid_time.is_none() {
        return make(FreshnessVerdict::Untemporal, None, cited_handle);
    }

    // (3) Drifted vs current. Prefer an explicit drift record; fall back to a
    // content change across code-graph versions of the same handle. Commit-anchored
    // comparisons treat any descendant of the anchor commit as later code.
    let trigger = drift_record_trigger(
        index,
        cited_id,
        anchor_commit,
        anchor_valid_time.as_deref(),
        &anchor_descendants,
        has_ancestry,
    )
    .or_else(|| {
        content_change_trigger(
            versions,
            anchor_commit,
            anchor_valid_time.as_deref(),
            &anchor_descendants,
            has_ancestry,
        )
    });

    cited_handle.anchor_valid_time = anchor_valid_time;

    if let Some(trigger) = trigger {
        return make(FreshnessVerdict::Drifted, Some(trigger), cited_handle);
    }

    make(FreshnessVerdict::Current, None, cited_handle)
}

/// Returns the valid-time of the version recorded at `commit`, if present.
fn version_valid_time(versions: &[&GraphRecord], commit: &str) -> Option<String> {
    versions.iter().find_map(|record| match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        } if t.git_commit == commit => Some(t.valid_time.clone()),
        _ => None,
    })
}

/// Finds the earliest drift record proving the cited handle moved after the
/// anchor. Neighbor-safe: keyed on `prior_record_id == cited_id`, so a sibling
/// symbol drifting under the same file never triggers a verdict here (AC5).
fn drift_record_trigger(
    index: &FreshnessIndex<'_>,
    cited_id: &str,
    anchor_commit: Option<&str>,
    anchor_valid_time: Option<&str>,
    anchor_descendants: &BTreeSet<&str>,
    has_ancestry: bool,
) -> Option<TriggeringHandle> {
    let candidates = index.drifts_by_prior.get(cited_id)?;
    let mut best: Option<(&str, &SemanticDriftMetadata)> = None;
    for &(drift_id, drift) in candidates {
        if !after_anchor(
            &drift.after_valid_time,
            &drift.before_git_commit,
            &drift.after_git_commit,
            anchor_commit,
            anchor_valid_time,
            anchor_descendants,
            has_ancestry,
        ) {
            continue;
        }
        let take = match best {
            None => true,
            Some((best_id, best_drift)) => {
                // Earliest post-anchor measurement, by parsed instant then ID.
                matches!(
                    time_cmp(&drift.after_valid_time, &best_drift.after_valid_time)
                        .then_with(|| drift_id.cmp(best_id)),
                    std::cmp::Ordering::Less
                )
            }
        };
        if take {
            best = Some((drift_id, drift));
        }
    }
    best.map(|(drift_id, drift)| TriggeringHandle::DriftRecord {
        drift_record_id: drift_id.to_owned(),
        after_git_commit: drift.after_git_commit.clone(),
        after_valid_time: drift.after_valid_time.clone(),
    })
}

/// Decides whether a drift measurement post-dates the anchor.
fn after_anchor(
    after_valid_time: &str,
    before_git_commit: &str,
    after_git_commit: &str,
    anchor_commit: Option<&str>,
    anchor_valid_time: Option<&str>,
    anchor_descendants: &BTreeSet<&str>,
    has_ancestry: bool,
) -> bool {
    // With commit ancestry available, a drift is post-anchor only when **both** its
    // endpoints lie on the anchored lineage: its `before` commit is the anchor (or
    // a descendant) and its `after` commit is a descendant. Requiring both rejects a
    // merge case where a sibling-branch change `B → M` lands on a merge commit `M`
    // reachable from the anchor `A` while `B` is not — that is not drift of the
    // code on `A`'s lineage. Reachability (not timestamps) is authoritative, so
    // rebased/backdated descendants are still caught.
    if has_ancestry && anchor_commit.is_some() {
        let before_on_lineage = anchor_commit.is_some_and(|c| before_git_commit == c)
            || anchor_descendants.contains(before_git_commit);
        return before_on_lineage && anchor_descendants.contains(after_git_commit);
    }
    // No ancestry to trust. A drift whose "before" state is exactly the anchor
    // commit measured change from the cited version forward — a drift regardless of
    // timestamps (consecutive commits can share a committer timestamp).
    if anchor_commit.is_some_and(|c| before_git_commit == c) {
        return true;
    }
    // Otherwise fall back to a strictly later valid-time.
    if let Some(anchor_vt) = anchor_valid_time {
        return time_after(after_valid_time, anchor_vt);
    }
    false
}

/// Finds the earliest later code-graph version whose content hash differs from
/// the anchor version. Keyed on the same record ID, so only the cited handle's
/// own content is compared — a neighbor changing the file does not count (AC5).
fn content_change_trigger(
    versions: &[&GraphRecord],
    anchor_commit: Option<&str>,
    anchor_valid_time: Option<&str>,
    anchor_descendants: &BTreeSet<&str>,
    has_ancestry: bool,
) -> Option<TriggeringHandle> {
    let anchor_vt = anchor_valid_time?;
    let anchor_version = anchor_commit
        .and_then(|commit| versions.iter().find(|r| version_commit(r) == Some(commit)))
        .copied()
        .or_else(|| at_or_before(versions, anchor_vt))?;

    let commit_anchored = anchor_commit.is_some() && has_ancestry;
    let mut later: Vec<&GraphRecord> = versions
        .iter()
        .copied()
        .filter(|r| {
            if commit_anchored {
                // Ancestry is authoritative: only true descendants of the anchor
                // commit are later code states. A non-descendant later-timestamp
                // version (a side branch or a future-dated parent) is not, and must
                // not produce a false `drifted`. Descendant reachability already
                // covers rebased/backdated children and grandchildren.
                version_commit(r).is_some_and(|c| anchor_descendants.contains(c))
            } else {
                // No commit anchor, or no ancestry to trust: a strictly later
                // valid-time is the only signal, keeping drift detectable on
                // current-tree and partial graphs.
                version_valid(r).is_some_and(|vt| time_after(vt, anchor_vt))
            }
        })
        .collect();
    later.sort_by(|a, b| {
        time_cmp(
            version_valid(a).unwrap_or_default(),
            version_valid(b).unwrap_or_default(),
        )
        .then_with(|| {
            version_commit(a)
                .unwrap_or_default()
                .cmp(version_commit(b).unwrap_or_default())
        })
        // Deterministic tie-break for repeated current-tree scans (issue #204):
        // two non-temporal versions can share a second-resolution `valid_time` and
        // carry no commit, leaving the two keys above equal. Break the tie on the
        // content signature so the earliest differing later version is selected
        // byte-identically across runs.
        .then_with(|| content_hash(a).cmp(&content_hash(b)))
    });

    later.into_iter().find_map(|record| {
        content_differs(anchor_version, record).then(|| {
            // Populate the timestamp from `version_valid`, which falls back to the
            // node-level `valid_time` for current-tree records — otherwise a
            // non-temporal content change would emit an empty `after_valid_time`
            // and lose the proof of when the code moved. The commit stays empty for
            // non-temporal versions (they carry no `git_commit`).
            TriggeringHandle::ContentChange {
                after_git_commit: version_commit(record).unwrap_or_default().to_owned(),
                after_valid_time: version_valid(record).unwrap_or_default().to_owned(),
                content_hash: content_hash(record),
            }
        })
    })
}

/// Latest version whose valid-time is at or before the anchor.
fn at_or_before<'a>(versions: &[&'a GraphRecord], anchor_vt: &str) -> Option<&'a GraphRecord> {
    versions
        .iter()
        .copied()
        .filter(|r| version_valid(r).is_some_and(|vt| !time_after(vt, anchor_vt)))
        .max_by(|a, b| {
            time_cmp(
                version_valid(a).unwrap_or_default(),
                version_valid(b).unwrap_or_default(),
            )
        })
}

const fn version_commit(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        } => Some(t.git_commit.as_str()),
        _ => None,
    }
}

/// Valid-time of a code-graph node version. Prefers the history-replay
/// `temporal.valid_time`; falls back to the node-level `valid_time` stamped on
/// current-tree `scan`/`refresh` records so repeated current-tree snapshots are
/// still ordered and comparable.
const fn version_valid(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        } => Some(t.valid_time.as_str()),
        GraphRecord::Node {
            valid_time: Some(vt),
            ..
        } if !vt.is_empty() => Some(vt.as_str()),
        _ => None,
    }
}

/// Parent commit SHAs recorded on a code-graph node version, empty when absent.
fn version_parents(record: &GraphRecord) -> &[String] {
    match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        } => &t.git_parent_commits,
        _ => &[],
    }
}

/// Content signature of a code-graph node version. The node summary embeds the
/// normalized source body for symbols, so it is a stable, in-record content
/// signal that requires no source re-read (read-only, AC8). For nodes whose
/// summary is name-only (Rust `Module` / `Import`, issue #206) the summary alone
/// misses a body change; those nodes carry a `content_signature` handle over the
/// normalized body, folded in here so a body edit flips the hash. The field is
/// mixed in ONLY when present, so every record without it (every other kind)
/// hashes byte-identically to before.
fn content_hash(record: &GraphRecord) -> String {
    let (summary, content_signature) = match record {
        GraphRecord::Node {
            summary,
            content_signature,
            ..
        } => (summary.as_str(), content_signature.as_deref()),
        _ => ("", None),
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(summary.as_bytes());
    if let Some(signature) = content_signature {
        hasher.update(signature.as_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// The name-only `summary` and optional `content_signature` (issue #206) of a
/// code-graph node version; `("", None)` for any non-node record.
fn summary_and_signature(record: &GraphRecord) -> (&str, Option<&str>) {
    match record {
        GraphRecord::Node {
            summary,
            content_signature,
            ..
        } => (summary.as_str(), content_signature.as_deref()),
        _ => ("", None),
    }
}

/// Whether `record`'s content differs from `anchor` for drift purposes, treating
/// a ONE-SIDED-MISSING `content_signature` as UNKNOWN rather than a change
/// (issue #206 back-compat / Codex finding B). A store upgraded across #206 holds
/// a legacy Module/Import version with `content_signature = None` (hashed
/// summary-only) alongside a post-upgrade rescan of the SAME body carrying
/// `Some(sig)`; folding the signature into the hash on only one side would flip a
/// byte-identical body to a false `drifted`. So:
/// - both versions carry a signature → the full content hash decides (summary +
///   signature), byte-identical to the pre-fix decision;
/// - neither carries one → compare the summary only, byte-identical to the
///   pre-#206 (summary-only) decision;
/// - exactly one carries one (a pre-/post-upgrade pair) → compare the summary
///   only, so the field's mere presence never manufactures drift.
fn content_differs(anchor: &GraphRecord, record: &GraphRecord) -> bool {
    let (anchor_summary, anchor_sig) = summary_and_signature(anchor);
    let (record_summary, record_sig) = summary_and_signature(record);
    if anchor_sig.is_some() && record_sig.is_some() {
        content_hash(anchor) != content_hash(record)
    } else {
        anchor_summary != record_summary
    }
}

/// Compares two RFC 3339 instants, returning true when `later` is strictly after
/// `anchor`. Falls back to byte ordering when either fails to parse.
fn time_after(later: &str, anchor: &str) -> bool {
    time_cmp(later, anchor) == std::cmp::Ordering::Greater
}

/// Orders two RFC 3339 instants by parsed value so mixed UTC offsets compare
/// correctly (`scan-history` preserves Git's committer offset). Falls back to
/// byte ordering only when either value fails to parse.
fn time_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}
