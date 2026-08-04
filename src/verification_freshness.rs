//! Evidence-freshness verdicts for verification records (issue #111).
//!
//! The verification domain (v1) stamps every run with an anchor —
//! `temporal.git_commit` / `executed_at` / `source_artifact_hash` — and links it
//! to the code it exercised via `FAILED_ON`, `TOUCHED_FILE`, and
//! `MENTIONS_SYMBOL`. Nothing ages those runs against the code they cite: a
//! `TestRun status=pass` recorded 40 commits ago, whose cited symbol has since
//! drifted, is handed back to a consumer as authoritative green with no signal
//! that the code it covered has moved. This module closes that loop with a
//! strictly read-only, deterministic join over records the graph already
//! stores: verification-domain nodes, their code citations, semantic-drift
//! records, code-graph content versions, and tombstones.
//!
//! # Distinct from issue #85
//!
//! `crate::evidence_freshness` (issue #85, `eg query evidence-freshness`) ages
//! **agent-memory** `Observation`/`Decision` citations. This module ages
//! **verification-domain** evidence (`TestRun`, `CIStatus`, `BenchmarkRun`,
//! `CoverageReport`, `ProofResult`) — a different domain and a different trust
//! class (`deterministic-but-runtime-derived`, never agent-authored). The two
//! never share a verdict row. See `docs/cli/verification-freshness.md`.
//!
//! # What this is — and is not
//!
//! A verdict is a **freshness lead, never a re-judgment of pass/fail** (AC3): a
//! `stale`/`unresolved` verdict states only that the verified basis moved,
//! never that the recorded `status` is now wrong, that the code is broken, or
//! that it is correct/safe. The verdict attaches to the verification record's
//! citation of code; the citation's target code fact is never rewritten,
//! hidden, or re-stamped, and the verification record's own `status` field is
//! never rewritten either (AC4).
//!
//! # Verdicts
//!
//! - `current` — every cited symbol/file is unchanged since the record's
//!   anchor.
//! - `stale` — the cited symbol/file changed after the anchor, or the
//!   record's `source_artifact_hash` no longer matches the artefact's current
//!   content.
//! - `unresolved` — the cited handle no longer resolves: removed/renamed or
//!   deleted.
//! - `unanchored` — the record carries neither `temporal.git_commit` nor
//!   `executed_at` to compare against.
//!
//! # Anchor precedence
//!
//! `temporal.git_commit` (with its paired `temporal.valid_time`) is preferred
//! when present; otherwise `executed_at` (a parseable RFC 3339 instant) is
//! used as the anchor instant. Neither present ⇒ `unanchored`. This is a
//! deliberate design decision for this issue: the verification schema
//! documents `git_commit` as the primary anchor, but the one real trunk writer
//! (`eg capture-tests`, issue #165) populates only `executed_at` today, so
//! `executed_at` must be a usable anchor on its own.
//!
//! # Triggers (reused, never re-derived)
//!
//! - **drift record** — a [`SemanticDriftMetadata`] whose `prior_record_id`
//!   equals the cited record ID and whose `after_valid_time` post-dates the
//!   anchor. The triggering handle is the drift record ID.
//! - **content change** — a later code-graph version of the *same* cited
//!   record ID whose content hash differs from the version current at the
//!   anchor. The triggering handle is the later commit plus the content hash.
//! - **artifact hash change** — when `source_artifact_hash` is recorded and
//!   `--repo-path` is supplied, the artefact at `source_artifact_path` is
//!   re-hashed (BLAKE3) and compared; a mismatch is `stale` **regardless of
//!   anchor**, since a direct hash comparison needs no historical ordering.
//!   Without `--repo-path` this trigger is simply not evaluated — never
//!   assumed to pass.
//!
//! # Known limitations
//!
//! Liveness (for the `unresolved` verdict) uses the tip-commit and
//! non-temporal-scan frontiers PER REPOSITORY (mirroring issue #85's
//! #203/#204 pattern via `crate::query::RepositoryIndex`), so a shared
//! multi-repository store where two repositories were scanned/committed at
//! different times does not cross-prune. This does not extend to #85's
//! later refinements: the current-tree "mixed handle" axis (#405, a single
//! record ID carrying BOTH scan-history and current-tree versions) is not
//! specially handled here. Staleness ordering compares recorded valid-time
//! instants, not a commit-ancestry DAG walk, so a rebase that backdates a
//! descendant commit is not detected — a known, documented gap, not a
//! silent wrong answer.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde::Serialize;

use crate::evidence_freshness::{content_differs, content_hash, is_code_handle_kind, time_cmp};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SemanticDriftMetadata, SourceSpan};

// ── Verdict ──────────────────────────────────────────────────────────────────

/// Per-citation freshness verdict for a verification record.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationFreshnessVerdict {
    /// Every cited symbol/file is unchanged since the record's anchor.
    Current,
    /// The cited symbol/file changed after the anchor, or the record's
    /// `source_artifact_hash` no longer matches.
    Stale,
    /// The cited handle no longer resolves: removed/renamed or deleted.
    Unresolved,
    /// The record carries no `git_commit`/`executed_at` anchor.
    Unanchored,
}

impl VerificationFreshnessVerdict {
    /// Wire string value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Unresolved => "unresolved",
            Self::Unanchored => "unanchored",
        }
    }

    /// Returns true for the stale verdicts (`stale` / `unresolved`) — the
    /// documented `--stale-only` query mode (AC7).
    #[must_use]
    pub const fn is_stale(self) -> bool {
        matches!(self, Self::Stale | Self::Unresolved)
    }
}

/// Stable text stamped on every stale/unresolved verdict so the output reads
/// as a freshness lead, never a truth claim (AC3).
pub const FRESHNESS_LEAD: &str = "evidence basis moved since the record's anchor; a reason to \
     re-verify, not proof the recorded result is now wrong";

/// Stable diagnostic code: the store carries no verification-domain records at
/// all (AC7 — never a silent empty success).
pub const NO_VERIFICATION_RECORDS_DIAGNOSTIC: &str = "no_verification_records_in_store";
/// Stable diagnostic code: verification records exist but none cite any code
/// handle (nothing to classify).
pub const NO_VERIFICATION_CODE_CITATIONS_DIAGNOSTIC: &str = "no_verification_code_citations";
/// Stable diagnostic code: `--stale-only` found nothing stale.
pub const NO_STALE_DIAGNOSTIC: &str = "no_stale_verification_records";
/// Stable diagnostic code: `--stale-only` found at least one stale/unresolved
/// row.
pub const STALE_PRESENT_DIAGNOSTIC: &str = "stale_verification_records_present";
/// Stable diagnostic code: the default (non-`--stale-only`) verdict set.
pub const FRESHNESS_VERDICTS_DIAGNOSTIC: &str = "freshness_verdicts";

/// Default row cap when `--limit` is not supplied.
pub const VERIFICATION_FRESHNESS_DEFAULT_LIMIT: usize = 500;
/// Maximum accepted `--limit` value.
pub const VERIFICATION_FRESHNESS_MAX_LIMIT: usize = 1000;

// ── Output types ─────────────────────────────────────────────────────────────

/// The record's own anchor, echoed for context (AC6).
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize)]
pub struct VerificationAnchor {
    /// Anchor commit SHA, when the record carries `temporal.git_commit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// Anchor RFC 3339 instant: the paired `temporal.valid_time` when a
    /// commit anchor is present, else the record's own `executed_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_at: Option<String>,
}

/// The cited code handle plus its anchor (AC6).
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct CitedHandle {
    /// Stable record ID of the cited code node, when the citation names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_record_id: Option<String>,
    /// Repo-relative path of the cited handle, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span of the cited handle, when symbol-level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Anchor commit the citing record was recorded against, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_commit: Option<String>,
    /// Cross-domain edge label the citation was derived from
    /// (`FAILED_ON`/`MENTIONS_SYMBOL`/`TOUCHED_FILE`), or the synthetic
    /// `source_artifact` relation for an artifact-hash citation.
    pub relation: String,
    /// Domain of the cited target: `codegraph` for edge-derived citations,
    /// `verification` for the record's own source artefact.
    pub target_domain: String,
}

/// The handle that proves the cited code moved (AC6). Present only on
/// `stale`/`unresolved` verdicts.
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
        /// Commit SHA of the later version that differs from the anchor
        /// version, when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        after_git_commit: Option<String>,
        /// Valid-time of the later version.
        after_valid_time: String,
        /// Content hash of the later version (`blake3:<hex>`).
        content_hash: String,
    },
    /// The record's `source_artifact_hash` no longer matches the artefact's
    /// current content (requires `--repo-path`).
    ArtifactHashChanged {
        /// The hash recorded on the verification record.
        recorded_hash: String,
        /// The artefact's current BLAKE3 hex hash.
        current_hash: String,
    },
    /// The cited handle was removed/renamed and survives only as a
    /// tombstone.
    HandleRemoved {
        /// Stable tombstone record ID for the deleted handle.
        tombstone_id: String,
    },
    /// The cited handle resolves to nothing live in the store (no live node,
    /// no tombstone — pruned, superseded, or never present in this slice).
    HandleAbsent,
}

/// One per-citation verification-freshness verdict.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct VerificationFreshnessEntry {
    /// Record ID of the verification record under verdict.
    pub verification_record_id: String,
    /// Reported verification kind (`test_run`, `ci_status`, …).
    pub verification_kind: String,
    /// Recorded `status`, echoed verbatim — never reinterpreted (AC4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The freshness verdict.
    pub verdict: VerificationFreshnessVerdict,
    /// Freshness-lead text on stale/unresolved verdicts; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_lead: Option<&'static str>,
    /// The record's own anchor.
    pub anchor: VerificationAnchor,
    /// The cited code handle.
    pub cited_handle: CitedHandle,
    /// The handle proving the cited code moved (stale/unresolved only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggering_handle: Option<TriggeringHandle>,
}

// ── Internal index ───────────────────────────────────────────────────────────

struct VerificationFreshnessIndex<'a> {
    /// Live (non-tombstoned, non-superseded) code-handle node versions,
    /// grouped by record ID.
    live_code_by_id: BTreeMap<&'a str, Vec<&'a GraphRecord>>,
    /// Tombstone record ID, keyed by the deleted record ID (restoration-aware:
    /// a later re-emitted node clears an earlier tombstone).
    tombstone_by_deleted: BTreeMap<&'a str, &'a str>,
    /// Record IDs superseded by a newer version (via `superseded_by` or a
    /// live `SUPERSEDES` edge) -- applies to code handles, drift records,
    /// AND verification records themselves, matching `crate::evidence_freshness`.
    superseded_ids: BTreeSet<&'a str>,
    /// Drift metadata keyed by the prior (cited-side) record ID.
    drifts_by_prior: BTreeMap<&'a str, Vec<(&'a str, &'a SemanticDriftMetadata)>>,
}

impl<'a> VerificationFreshnessIndex<'a> {
    #[allow(clippy::too_many_lines)]
    fn build(records: &'a [GraphRecord]) -> Self {
        // First pass: restoration-aware tombstones (mirrors
        // `crate::evidence_freshness`: a delete followed by a later
        // re-ingest of the same stable ID restores the handle).
        let mut last_record_idx: BTreeMap<&str, usize> = BTreeMap::new();
        let mut last_tombstone: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
        for (idx, record) in records.iter().enumerate() {
            match record {
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
        let mut tombstone_by_deleted: BTreeMap<&str, &str> = BTreeMap::new();
        for (deleted_id, (t_idx, t_id)) in &last_tombstone {
            let restored = last_record_idx
                .get(deleted_id)
                .is_some_and(|n_idx| n_idx > t_idx);
            if !restored {
                tombstone_by_deleted.insert(deleted_id, t_id);
            }
        }

        // Superseded code handles: the node's own `superseded_by` marker
        // (latest physical row wins, so a restored row without the marker
        // clears an earlier supersession) plus a live `SUPERSEDES` edge.
        let mut superseded_ids: BTreeSet<&str> = BTreeSet::new();
        let mut last_node_superseded: BTreeMap<&str, bool> = BTreeMap::new();
        for record in records {
            match record {
                GraphRecord::Node {
                    id, superseded_by, ..
                } => {
                    last_node_superseded.insert(
                        id.as_str(),
                        superseded_by.as_deref().is_some_and(|s| !s.is_empty()),
                    );
                }
                GraphRecord::Edge {
                    id,
                    label: EdgeLabel::Supersedes,
                    source,
                    target,
                    ..
                } if !tombstone_by_deleted.contains_key(id.as_str())
                    && !tombstone_by_deleted.contains_key(source.as_str()) =>
                {
                    // Honor a standalone SUPERSEDES edge only when neither the
                    // edge nor its superseding source node has been retracted
                    // (mirrors `crate::evidence_freshness`) -- otherwise a
                    // retracted supersession would wrongly hide a still-live
                    // target as `unresolved`.
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

        // Per-repository tip-commit and non-temporal scan frontiers (mirrors
        // `crate::evidence_freshness`'s #203/#204 pattern). A shared
        // multi-repository store can hold repository A scanned/committed at
        // a later time than repository B; a STORE-WIDE frontier would prune
        // B's still-current code as if it were stale relative to A's newer
        // timestamps. Each frontier is instead computed PER owning
        // repository (via `RepositoryIndex`'s CONTAINS/DEFINES/IMPORTS
        // containment walk) and a handle is pruned against its OWN
        // repository's frontier, not the store-wide one. An unattributable
        // handle (no reachable owning `Repository` node — including every
        // handle in a legacy/single-repository store with no `Repository`
        // node at all) falls back to the store-wide union, so single-repo
        // stores are unaffected.
        let repo_index = crate::query::RepositoryIndex::build(records);

        let mut per_repo_all: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
        let mut per_repo_parents: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                id,
                temporal: Some(t),
                ..
            } = record
            {
                let owner = repo_index.owner_of(id.as_str());
                per_repo_all
                    .entry(owner)
                    .or_default()
                    .insert(t.git_commit.as_str());
                for parent in &t.git_parent_commits {
                    per_repo_parents
                        .entry(owner)
                        .or_default()
                        .insert(parent.as_str());
                }
            }
        }
        let empty_commits: BTreeSet<&str> = BTreeSet::new();
        let mut per_repo_tips: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
        let mut tips: BTreeSet<&str> = BTreeSet::new();
        for (owner, all) in &per_repo_all {
            let parents = per_repo_parents.get(owner).unwrap_or(&empty_commits);
            let repo_tips: BTreeSet<&str> = all.difference(parents).copied().collect();
            tips.extend(repo_tips.iter().copied());
            per_repo_tips.insert(*owner, repo_tips);
        }

        // Non-temporal SCAN frontier, computed the same way: a repeated
        // current-tree `scan`/`refresh` re-emits every current handle with a
        // node-level `valid_time` but no commit and no `Tombstone` when a
        // handle is deleted between two scans, so WITHOUT this a citation to
        // code deleted in a later scan would be reported `current`/`stale`
        // instead of `unresolved`. The newest valid_time among a
        // repository's own non-temporal code handles AND its `Repository`
        // node snapshots is that repository's frontier (folding in the
        // repository node's own valid_time so an empty latest scan -- the
        // last handle deleted -- still advances it).
        let mut per_repo_frontier: BTreeMap<Option<&str>, &str> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                id,
                temporal: None,
                kind,
                ..
            } = record
                && (is_code_handle_kind(*kind) || matches!(kind, NodeKind::Repository))
                && let Some(vt) = version_valid(record)
            {
                let owner = repo_index.owner_of(id.as_str());
                let cur = per_repo_frontier.entry(owner).or_insert(vt);
                if time_cmp(vt, cur) == std::cmp::Ordering::Greater {
                    *cur = vt;
                }
            }
        }

        let mut live_code_by_id: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node { id, kind, .. } = record
                && is_code_handle_kind(*kind)
                && !tombstone_by_deleted.contains_key(id.as_str())
                && !superseded_ids.contains(id.as_str())
            {
                live_code_by_id.entry(id.as_str()).or_default().push(record);
            }
        }
        // Prune to the frontier on each version's OWN axis, scoped to the
        // HANDLE'S OWN repository: a commit-anchored version is live only at
        // a tip commit of its own repository (empty repo-tips ⇒ nothing to
        // prune against on that axis); a non-temporal version is live only
        // at its own repository's newest scan (no repo frontier ⇒ nothing to
        // prune against on that axis). A handle with EITHER axis satisfied
        // by any of its versions stays live.
        live_code_by_id.retain(|id, versions| {
            let owner = repo_index.owner_of(id);
            let repo_tips = per_repo_tips
                .get(&owner)
                .filter(|s| !s.is_empty())
                .unwrap_or(&tips);
            let frontier = per_repo_frontier.get(&owner).copied();
            versions.iter().any(|r| match r {
                GraphRecord::Node {
                    temporal: Some(t), ..
                } => repo_tips.is_empty() || repo_tips.contains(t.git_commit.as_str()),
                _ => frontier.is_none_or(|f| {
                    version_valid(r).is_some_and(|vt| time_cmp(vt, f) == std::cmp::Ordering::Equal)
                }),
            })
        });

        let mut drifts_by_prior: BTreeMap<&str, Vec<(&str, &SemanticDriftMetadata)>> =
            BTreeMap::new();
        // Drift metadata keyed by the drift record's OWN id, so a
        // `DRIFTS_PRIOR` edge (below) can recover a drift whose
        // `prior_record_id` field is stale relative to the edge.
        let mut drift_meta_by_id: BTreeMap<&str, &SemanticDriftMetadata> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                id,
                kind: NodeKind::SemanticDrift,
                semantic_drift: Some(drift),
                ..
            } = record
                && !tombstone_by_deleted.contains_key(id.as_str())
                && !superseded_ids.contains(id.as_str())
            {
                drift_meta_by_id.insert(id.as_str(), drift);
                drifts_by_prior
                    .entry(drift.prior_record_id.as_str())
                    .or_default()
                    .push((id.as_str(), drift));
            }
        }
        // A `DRIFTS_PRIOR` edge (drift -> cited handle) recovers a drift
        // trigger a stale/mismatched `prior_record_id` field would otherwise
        // miss (mirrors `crate::evidence_freshness`) -- never a sibling's
        // drift, since the edge target IS the cited-side record.
        for record in records {
            if let GraphRecord::Edge {
                id,
                label: EdgeLabel::DriftsPrior,
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
            superseded_ids,
            drifts_by_prior,
        }
    }

    /// The latest live version of `id` by valid-time, when any exist.
    fn latest_live(&self, id: &str) -> Option<&'a GraphRecord> {
        self.live_code_by_id.get(id).and_then(|versions| {
            versions.iter().copied().max_by(|a, b| {
                time_cmp(
                    version_valid(a).unwrap_or_default(),
                    version_valid(b).unwrap_or_default(),
                )
            })
        })
    }

    /// The version of `id` current as of `anchor_valid_time`: the latest live
    /// version whose valid-time is at-or-before the anchor, falling back to
    /// the earliest live version when every version postdates the anchor.
    fn version_at_anchor(&self, id: &str, anchor_valid_time: &str) -> Option<&'a GraphRecord> {
        let versions = self.live_code_by_id.get(id)?;
        versions
            .iter()
            .copied()
            .filter(|r| {
                version_valid(r).is_some_and(|vt| {
                    time_cmp(vt, anchor_valid_time) != std::cmp::Ordering::Greater
                })
            })
            .max_by(|a, b| {
                time_cmp(
                    version_valid(a).unwrap_or_default(),
                    version_valid(b).unwrap_or_default(),
                )
            })
            .or_else(|| {
                versions.iter().copied().min_by(|a, b| {
                    time_cmp(
                        version_valid(a).unwrap_or_default(),
                        version_valid(b).unwrap_or_default(),
                    )
                })
            })
    }
}

/// Valid-time of a code-graph node version: `temporal.valid_time` when
/// present, else the node-level `valid_time` (current-tree `scan`/`refresh`).
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

/// Repo-relative path and span of a node record, `(None, None)` for a
/// non-node record.
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

/// The five verification-domain kinds this issue's AC names explicitly.
const fn is_verification_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::TestRun
            | NodeKind::CIStatus
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
            | NodeKind::ProofResult
    )
}

/// Cross-domain edge labels that cite a code handle from a verification
/// record. Closed set per `docs/schema/verification.md` §6.
const fn is_code_citation_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::FailedOn | EdgeLabel::MentionsSymbol | EdgeLabel::TouchedFile
    )
}

/// Reads `artifact_path` (a `source_artifact_path` value read back from the
/// store — producer-controlled, never trusted) joined onto `root`, but only
/// when the resolved path stays contained under `root`. `source_artifact_path`
/// is documented as repo-relative, but nothing upstream enforces that, so an
/// absolute path (`Path::join` discards `root` entirely for an absolute
/// second operand) or a `..`-escaping relative path must never be honored —
/// this lane must never become a local file-read/existence oracle for
/// arbitrary paths. Returns `None` (never claiming a verdict) when the path
/// escapes `root`, does not exist, or is otherwise unreadable.
fn read_contained_artifact(root: &Path, artifact_path: &str) -> Option<Vec<u8>> {
    let joined = root.join(artifact_path);
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let canonical_joined = std::fs::canonicalize(&joined).ok()?;
    if !canonical_joined.starts_with(&canonical_root) {
        return None;
    }
    std::fs::read(&canonical_joined).ok()
}

/// The record's own anchor: `temporal.git_commit`/`temporal.valid_time` when
/// present, else `executed_at`. `None` when neither is usable (`unanchored`).
fn record_anchor(record: &GraphRecord) -> Option<VerificationAnchor> {
    let GraphRecord::Node {
        temporal,
        executed_at,
        ..
    } = record
    else {
        return None;
    };
    if let Some(t) = temporal
        && !t.git_commit.is_empty()
    {
        return Some(VerificationAnchor {
            git_commit: Some(t.git_commit.clone()),
            executed_at: Some(t.valid_time.clone()),
        });
    }
    let executed_at = executed_at.as_deref().filter(|s| !s.is_empty())?;
    if chrono::DateTime::parse_from_rfc3339(executed_at).is_err() {
        return None;
    }
    Some(VerificationAnchor {
        git_commit: None,
        executed_at: Some(executed_at.to_owned()),
    })
}

/// Classifies one citation edge from a verification record to a code handle.
fn classify_citation(
    index: &VerificationFreshnessIndex<'_>,
    anchor: Option<&VerificationAnchor>,
    relation: &'static str,
    target_id: &str,
) -> (
    VerificationFreshnessVerdict,
    CitedHandle,
    Option<TriggeringHandle>,
) {
    // Unresolved is checked independent of the anchor: a citation to gone
    // code is unresolved whether or not the record can be temporally placed.
    if let Some(&tombstone_id) = index.tombstone_by_deleted.get(target_id) {
        let handle = CitedHandle {
            target_record_id: Some(target_id.to_owned()),
            repo_relative_path: None,
            span: None,
            anchor_commit: anchor.and_then(|a| a.git_commit.clone()),
            relation: relation.to_owned(),
            target_domain: "codegraph".to_owned(),
        };
        return (
            VerificationFreshnessVerdict::Unresolved,
            handle,
            Some(TriggeringHandle::HandleRemoved {
                tombstone_id: tombstone_id.to_owned(),
            }),
        );
    }
    let Some(latest) = index.latest_live(target_id) else {
        let handle = CitedHandle {
            target_record_id: Some(target_id.to_owned()),
            repo_relative_path: None,
            span: None,
            anchor_commit: anchor.and_then(|a| a.git_commit.clone()),
            relation: relation.to_owned(),
            target_domain: "codegraph".to_owned(),
        };
        return (
            VerificationFreshnessVerdict::Unresolved,
            handle,
            Some(TriggeringHandle::HandleAbsent),
        );
    };
    let (path, span) = node_path_span(latest);
    let mut handle = CitedHandle {
        target_record_id: Some(target_id.to_owned()),
        repo_relative_path: path,
        span,
        anchor_commit: anchor.and_then(|a| a.git_commit.clone()),
        relation: relation.to_owned(),
        target_domain: "codegraph".to_owned(),
    };

    let Some(anchor) = anchor else {
        return (VerificationFreshnessVerdict::Unanchored, handle, None);
    };
    let Some(anchor_time) = anchor.executed_at.as_deref() else {
        return (VerificationFreshnessVerdict::Unanchored, handle, None);
    };

    // Trigger 1: a semantic-drift record measured after the anchor.
    if let Some(drifts) = index.drifts_by_prior.get(target_id) {
        let latest_drift = drifts
            .iter()
            .filter(|(_, d)| {
                time_cmp(&d.after_valid_time, anchor_time) == std::cmp::Ordering::Greater
            })
            .max_by(|(_, a), (_, b)| {
                time_cmp(&a.after_valid_time, &b.after_valid_time)
                    .then_with(|| a.after_git_commit.cmp(&b.after_git_commit))
            });
        if let Some((drift_id, drift)) = latest_drift {
            return (
                VerificationFreshnessVerdict::Stale,
                handle,
                Some(TriggeringHandle::DriftRecord {
                    drift_record_id: (*drift_id).to_owned(),
                    after_git_commit: drift.after_git_commit.clone(),
                    after_valid_time: drift.after_valid_time.clone(),
                }),
            );
        }
    }

    // Trigger 2: a later live version of the same handle whose content
    // differs from the version current at the anchor.
    if let Some(anchor_version) = index.version_at_anchor(target_id, anchor_time)
        && let Some(later_time) = version_valid(latest)
        && time_cmp(later_time, anchor_time) == std::cmp::Ordering::Greater
        && content_differs(anchor_version, latest)
    {
        // Content differs relative to the version current at the anchor —
        // update the cited handle to the ANCHOR version's own path/span (the
        // identity the citation named), not the drifted frontier version.
        let (anchor_path, anchor_span) = node_path_span(anchor_version);
        handle.repo_relative_path = anchor_path.or(handle.repo_relative_path);
        handle.span = anchor_span.or(handle.span);
        let after_git_commit = match latest {
            GraphRecord::Node {
                temporal: Some(t), ..
            } => Some(t.git_commit.clone()),
            _ => None,
        };
        return (
            VerificationFreshnessVerdict::Stale,
            handle,
            Some(TriggeringHandle::ContentChange {
                after_git_commit,
                after_valid_time: later_time.to_owned(),
                content_hash: content_hash(latest),
            }),
        );
    }

    (VerificationFreshnessVerdict::Current, handle, None)
}

/// Computes per-citation verification-freshness verdicts.
///
/// Covers every verification record (`TestRun`/`CIStatus`/`BenchmarkRun`/
/// `CoverageReport`/`ProofResult`) that cites code via
/// `FAILED_ON`/`MENTIONS_SYMBOL`/`TOUCHED_FILE`, plus a synthetic
/// `source_artifact` citation when `source_artifact_hash`/
/// `source_artifact_path` are recorded and `repo_path` is supplied.
///
/// Strictly read-only and deterministic: verdicts are sorted by
/// `(verification_record_id, relation, target_record_id)` so the same store
/// and the same `repo_path` contents yield byte-identical output across runs
/// (AC9). No records are created, modified, or deleted; `repo_path`, when
/// given, is only ever read.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn verification_freshness(
    records: &[GraphRecord],
    repo_path: Option<&Path>,
) -> Vec<VerificationFreshnessEntry> {
    let index = VerificationFreshnessIndex::build(records);
    let mut entries: Vec<VerificationFreshnessEntry> = Vec::new();

    // Every code-citation edge whose source is a verification record,
    // deduplicated to one (relation, target) pair per source: a re-ingested
    // or history-inclusive-store-duplicated physical copy of the SAME edge
    // (same label/source/target ⇒ same stable edge ID, per
    // `GraphRecord::edge`) must classify as exactly one citation, never one
    // row per physical copy.
    let mut citations_by_source: BTreeMap<&str, BTreeSet<(&'static str, &str)>> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = record
            && is_code_citation_label(*label)
            && !index.tombstone_by_deleted.contains_key(id.as_str())
        {
            citations_by_source
                .entry(source.as_str())
                .or_default()
                .insert((label.as_str(), target.as_str()));
        }
    }

    // Coalesce repeated physical writes of the same verification record
    // (append-only `--graph` re-ingest, or a `--data-dir` history-inclusive
    // read) to the LATEST write per stable ID before classifying -- an
    // earlier physical version could expose an obsolete `status` or anchor,
    // and classifying every version would duplicate every citation row.
    let mut latest_ver_write: BTreeMap<&str, usize> = BTreeMap::new();
    for (idx, record) in records.iter().enumerate() {
        if let GraphRecord::Node { id, kind, .. } = record
            && is_verification_kind(*kind)
        {
            latest_ver_write.insert(id.as_str(), idx);
        }
    }

    for (idx, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            id,
            kind,
            verification_kind,
            status,
            source_artifact_hash,
            source_artifact_path,
            ..
        } = record
        else {
            continue;
        };
        if !is_verification_kind(*kind) {
            continue;
        }
        if latest_ver_write.get(id.as_str()) != Some(&idx) {
            continue;
        }
        if index.tombstone_by_deleted.contains_key(id.as_str())
            || index.superseded_ids.contains(id.as_str())
        {
            continue;
        }
        let anchor = record_anchor(record);
        let reported_kind = verification_kind
            .clone()
            .unwrap_or_else(|| kind.as_str().to_owned());

        if let Some(citations) = citations_by_source.get(id.as_str()) {
            for (relation, target_id) in citations {
                let (verdict, cited_handle, triggering_handle) =
                    classify_citation(&index, anchor.as_ref(), relation, target_id);
                entries.push(VerificationFreshnessEntry {
                    verification_record_id: id.clone(),
                    verification_kind: reported_kind.clone(),
                    status: status.clone(),
                    freshness_lead: verdict.is_stale().then_some(FRESHNESS_LEAD),
                    verdict,
                    anchor: anchor.clone().unwrap_or_default(),
                    cited_handle,
                    triggering_handle,
                });
            }
        }

        // Synthetic source-artifact citation: only evaluated with an
        // explicit `repo_path`, and only when the record carries both
        // fields. Never claimed `current` without actually reading the file.
        if let (Some(recorded_hash), Some(artifact_path), Some(root)) = (
            source_artifact_hash.as_deref(),
            source_artifact_path.as_deref(),
            repo_path,
        ) && !recorded_hash.is_empty()
            && !artifact_path.is_empty()
            && let Some(bytes) = read_contained_artifact(root, artifact_path)
        {
            let current_hash = blake3::hash(&bytes).to_hex().to_string();
            let cited_handle = CitedHandle {
                target_record_id: None,
                repo_relative_path: Some(artifact_path.to_owned()),
                span: None,
                anchor_commit: anchor.as_ref().and_then(|a| a.git_commit.clone()),
                relation: "source_artifact".to_owned(),
                target_domain: "verification".to_owned(),
            };
            let (verdict, triggering_handle) = if current_hash == recorded_hash {
                (VerificationFreshnessVerdict::Current, None)
            } else {
                (
                    VerificationFreshnessVerdict::Stale,
                    Some(TriggeringHandle::ArtifactHashChanged {
                        recorded_hash: recorded_hash.to_owned(),
                        current_hash,
                    }),
                )
            };
            entries.push(VerificationFreshnessEntry {
                verification_record_id: id.clone(),
                verification_kind: reported_kind.clone(),
                status: status.clone(),
                freshness_lead: verdict.is_stale().then_some(FRESHNESS_LEAD),
                verdict,
                anchor: anchor.clone().unwrap_or_default(),
                cited_handle,
                triggering_handle,
            });
        }
    }

    entries.sort_by(|a, b| {
        a.verification_record_id
            .cmp(&b.verification_record_id)
            .then_with(|| a.cited_handle.relation.cmp(&b.cited_handle.relation))
            .then_with(|| {
                a.cited_handle
                    .target_record_id
                    .cmp(&b.cited_handle.target_record_id)
            })
    });
    entries
}

/// Filters verdicts to the stale set (`stale` + `unresolved`) for the
/// `--stale-only` query mode (AC7).
#[must_use]
pub fn stale_only(entries: Vec<VerificationFreshnessEntry>) -> Vec<VerificationFreshnessEntry> {
    entries
        .into_iter()
        .filter(|e| e.verdict.is_stale())
        .collect()
}

/// Verdict tally across `current`/`stale`/`unresolved`/`unanchored`.
#[must_use]
pub fn verdict_counts(entries: &[VerificationFreshnessEntry]) -> BTreeMap<&'static str, usize> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.verdict.as_str()).or_insert(0) += 1;
    }
    counts
}

/// `true` when the record slice contains at least one of the five
/// verification-domain kinds this lane covers.
#[must_use]
pub fn has_verification_records(records: &[GraphRecord]) -> bool {
    records
        .iter()
        .any(|r| matches!(r, GraphRecord::Node { kind, .. } if is_verification_kind(*kind)))
}

/// Stable record IDs of every code handle this module considers live.
///
/// Covers `Symbol`/`File` handles surviving this module's own liveness rules
/// (tombstone/supersession/per-repository frontier) -- exposed so scope
/// resolution can filter a by-NAME candidate set to live handles before
/// deciding ambiguity, using the SAME liveness this module's own citation
/// classification relies on (never a second, independently-drifting notion
/// of "live"). An explicit record-ID scope is deliberately NOT filtered
/// through this: it must keep resolving a tombstoned/historical handle
/// directly, so a citation to it still shows its `unresolved` verdict.
#[must_use]
pub fn live_code_handle_ids(records: &[GraphRecord]) -> BTreeSet<&str> {
    VerificationFreshnessIndex::build(records)
        .live_code_by_id
        .keys()
        .copied()
        .collect()
}
