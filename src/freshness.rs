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
//! Documented in `docs/cli/freshness.md`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ir::{GraphRecord, NodeKind, SemanticDriftMetadata, SourceSpan};

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
                    // A retracted (tombstoned) drift record is no longer part of
                    // current memory and must not trigger a `drifted` verdict.
                    if !tombstone_by_deleted.contains_key(id.as_str()) {
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
        let mut all_commits: BTreeSet<&str> = BTreeSet::new();
        let mut parent_commits: BTreeSet<&str> = BTreeSet::new();
        let mut commit_children: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        // Commits that participate in any parent/child edge. Ancestry is trusted
        // (descendant-only comparison) for an anchor only when the anchor itself is
        // in this set; an anchor with no ancestry metadata falls back to timestamps
        // even if unrelated histories in the same store carry edges.
        let mut dag_commits: BTreeSet<&str> = BTreeSet::new();
        for record in records {
            if let Some(commit) = version_commit(record) {
                all_commits.insert(commit);
                for parent in version_parents(record) {
                    parent_commits.insert(parent.as_str());
                    dag_commits.insert(parent.as_str());
                    dag_commits.insert(commit);
                    commit_children
                        .entry(parent.as_str())
                        .or_default()
                        .insert(commit);
                }
            }
        }
        let tips: BTreeSet<&str> = all_commits.difference(&parent_commits).copied().collect();
        if !tips.is_empty() {
            live_code_by_id.retain(|_id, versions| {
                versions.iter().any(|r| {
                    // A non-temporal (current-tree) version keeps the handle live.
                    version_commit(r).is_none_or(|c| tips.contains(c))
                })
            });
        }

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
        }
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

    /// Resolves an unanchored triple `(path, span)` to a live code node ID against
    /// the **frontier** only: a version is matched only when it sits at a tip commit
    /// (or carries no commit, i.e. current-tree). A span that matched only a
    /// historical (non-frontier) version no longer resolves, so a note recorded
    /// against a since-moved span is `unresolved` rather than a stale `current`.
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
                // Match against the frontier only: skip historical (non-tip) versions
                // when the graph carries commit ancestry.
                if !self.tips.is_empty()
                    && !version_commit(record).is_none_or(|c| self.tips.contains(c))
                {
                    continue;
                }
                match kind {
                    NodeKind::File => {
                        file_matches.insert(id.as_str());
                    }
                    // A triple can name any code handle (`Symbol`/`Module`/`Import`),
                    // not just a symbol; match the span for all of them.
                    k if is_code_handle_kind(*k)
                        && span.is_some()
                        && node_span.as_ref() == span =>
                    {
                        span_matches.insert(id.as_str());
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
                NodeKind::File => {
                    file_matches.insert(id.as_str());
                }
                // A triple can name any code handle (`Symbol`/`Module`/`Import`),
                // not just a symbol; match the span for all of them before falling
                // back to the whole file.
                k if is_code_handle_kind(*k) && span.is_some() && node_span.as_ref() == span => {
                    span_matches.insert(id.as_str());
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

    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            evidence_links: Some(links),
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

        for link in links {
            // Only links to the deterministic code graph carry a code handle.
            if link.target_domain != "codegraph" {
                continue;
            }
            // A codegraph link can point at a non-handle record (a `Commit`/`Change`
            // cited by `EXPLAINS_CHANGE`/`CHANGED_IN`); those are valid links, not
            // stale code handles, so they are not classified. Decide by relation
            // first — that holds even when the target node is absent from the slice
            // (so it cannot be classified as a false `unresolved`) — then by the
            // resolved target kind for any other relation.
            if is_non_handle_relation(&link.relation)
                || link
                    .target_record_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .is_some_and(|rid| index.is_non_handle_target(rid))
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
        let on_lineage_tip = |c: &str| {
            index.tips.contains(c) && (Some(c) == anchor_commit || anchor_descendants.contains(c))
        };
        let lineage_has_frontier = index.tips.iter().any(|t| on_lineage_tip(t));
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
    let anchor_hash = content_hash(anchor_version);

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
    });

    later.into_iter().find_map(|record| {
        (content_hash(record) != anchor_hash).then(|| {
            let (commit, vt) = match record {
                GraphRecord::Node {
                    temporal: Some(t), ..
                } => (t.git_commit.clone(), t.valid_time.clone()),
                _ => (String::new(), String::new()),
            };
            TriggeringHandle::ContentChange {
                after_git_commit: commit,
                after_valid_time: vt,
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
/// signal that requires no source re-read (read-only, AC8).
fn content_hash(record: &GraphRecord) -> String {
    let summary = match record {
        GraphRecord::Node { summary, .. } => summary.as_str(),
        _ => "",
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(summary.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
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
