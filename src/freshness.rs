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
//! - `untemporal` — the observation carries no valid-time/commit anchor to
//!   compare against.
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

use std::collections::BTreeMap;

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
    /// The cited handle no longer resolves (removed/renamed/deleted).
    Unresolved,
    /// The observation carries no valid-time/commit anchor to compare against.
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
}

impl<'a> FreshnessIndex<'a> {
    fn build(records: &'a [GraphRecord]) -> Self {
        let mut live_code_by_id: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
        let mut tombstone_by_deleted: BTreeMap<&str, &str> = BTreeMap::new();
        let mut drifts_by_prior: BTreeMap<&str, Vec<(&str, &SemanticDriftMetadata)>> =
            BTreeMap::new();

        // First pass: tombstones so liveness is decided independent of record order.
        for record in records {
            if let GraphRecord::Tombstone { id, deleted_id, .. } = record {
                tombstone_by_deleted.entry(deleted_id).or_insert(id);
            }
        }

        for record in records {
            match record {
                GraphRecord::Node { id, kind, .. } if is_code_handle_kind(*kind) => {
                    if !tombstone_by_deleted.contains_key(id.as_str()) {
                        live_code_by_id.entry(id).or_default().push(record);
                    }
                }
                GraphRecord::Node {
                    id,
                    kind: NodeKind::SemanticDrift,
                    semantic_drift: Some(drift),
                    ..
                } => {
                    drifts_by_prior
                        .entry(drift.prior_record_id.as_str())
                        .or_default()
                        .push((id.as_str(), drift));
                }
                _ => {}
            }
        }

        Self {
            live_code_by_id,
            tombstone_by_deleted,
            drifts_by_prior,
        }
    }

    /// Resolves a triple `(path, span)` to a live code node ID, preferring a
    /// span-equal Symbol (symbol-level), falling back to the File for the path.
    fn resolve_triple(&self, path: &str, span: Option<&SourceSpan>) -> Option<&'a str> {
        let mut file_match: Option<&str> = None;
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
                match kind {
                    NodeKind::Symbol if span.is_some() && node_span.as_ref() == span => {
                        return Some(id.as_str());
                    }
                    NodeKind::File => file_match = Some(id.as_str()),
                    _ => {}
                }
            }
        }
        file_match
    }
}

/// Code-graph node kinds that can be cited as a code handle.
const fn is_code_handle_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::File | NodeKind::Symbol | NodeKind::Module | NodeKind::Import
    )
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
pub fn evidence_link_freshness(records: &[GraphRecord]) -> Vec<FreshnessVerdictEntry> {
    let index = FreshnessIndex::build(records);
    let mut entries: Vec<FreshnessVerdictEntry> = Vec::new();

    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            evidence_links: Some(links),
            agent_id,
            session_id,
            observed_at,
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

        let provenance = VerdictProvenance {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            observed_at: observed_at.clone(),
            confidence: confidence.clone(),
            redaction_policy_version: redaction_policy_version.clone(),
        };
        let obs_valid_time = valid_time.as_deref().filter(|s| !s.is_empty());

        for link in links {
            // Only links to the deterministic code graph carry a code handle.
            if link.target_domain != "codegraph" {
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

fn classify_link(
    index: &FreshnessIndex<'_>,
    observation_id: &str,
    kind: &str,
    provenance: &VerdictProvenance,
    obs_valid_time: Option<&str>,
    link: &crate::ir::EvidenceLink,
) -> FreshnessVerdictEntry {
    let anchor_commit = link
        .as_of_commit
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| link.target_git_commit.as_deref().filter(|s| !s.is_empty()));

    // Resolve the cited handle to a code node ID (record ID first, triple next).
    let cited_id: Option<&str> = link
        .target_record_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            link.target_repo_relative_path
                .as_deref()
                .and_then(|p| index.resolve_triple(p, link.target_span.as_ref()))
        });

    let live_versions = cited_id.and_then(|id| index.live_code_by_id.get(id));

    // The cited node's repo-relative path / span comes from a live version when
    // resolvable, else from the link's triple.
    let (handle_path, handle_span) = live_versions
        .and_then(|versions| versions.first())
        .and_then(|record| match record {
            GraphRecord::Node {
                repo_relative_path,
                span,
                ..
            } => Some((repo_relative_path.clone(), *span)),
            _ => None,
        })
        .unwrap_or_else(|| (link.target_repo_relative_path.clone(), link.target_span));

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
    // content change across code-graph versions of the same handle.
    let trigger =
        drift_record_trigger(index, cited_id, anchor_commit, anchor_valid_time.as_deref()).or_else(
            || content_change_trigger(versions, anchor_commit, anchor_valid_time.as_deref()),
        );

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
) -> Option<TriggeringHandle> {
    let candidates = index.drifts_by_prior.get(cited_id)?;
    let mut best: Option<(&str, &SemanticDriftMetadata)> = None;
    for &(drift_id, drift) in candidates {
        if !after_anchor(
            &drift.after_valid_time,
            &drift.before_git_commit,
            anchor_commit,
            anchor_valid_time,
        ) {
            continue;
        }
        let take = match best {
            None => true,
            Some((best_id, best_drift)) => {
                (drift.after_valid_time.as_str(), drift_id)
                    < (best_drift.after_valid_time.as_str(), best_id)
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

/// Decides whether a later measurement post-dates the anchor.
fn after_anchor(
    after_valid_time: &str,
    before_git_commit: &str,
    anchor_commit: Option<&str>,
    anchor_valid_time: Option<&str>,
) -> bool {
    if let Some(anchor_vt) = anchor_valid_time {
        return time_after(after_valid_time, anchor_vt);
    }
    // No valid-time anchor: the drift must begin at the anchor commit.
    anchor_commit.is_some_and(|c| before_git_commit == c)
}

/// Finds the earliest later code-graph version whose content hash differs from
/// the anchor version. Keyed on the same record ID, so only the cited handle's
/// own content is compared — a neighbor changing the file does not count (AC5).
fn content_change_trigger(
    versions: &[&GraphRecord],
    anchor_commit: Option<&str>,
    anchor_valid_time: Option<&str>,
) -> Option<TriggeringHandle> {
    let anchor_vt = anchor_valid_time?;
    let anchor_version = anchor_commit
        .and_then(|commit| versions.iter().find(|r| version_commit(r) == Some(commit)))
        .copied()
        .or_else(|| at_or_before(versions, anchor_vt))?;
    let anchor_hash = content_hash(anchor_version);

    let mut later: Vec<&GraphRecord> = versions
        .iter()
        .copied()
        .filter(|r| version_valid(r).is_some_and(|vt| time_after(vt, anchor_vt)))
        .collect();
    later.sort_by(|a, b| {
        version_valid(a)
            .unwrap_or_default()
            .cmp(version_valid(b).unwrap_or_default())
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
            version_valid(a)
                .unwrap_or_default()
                .cmp(version_valid(b).unwrap_or_default())
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

const fn version_valid(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        } => Some(t.valid_time.as_str()),
        _ => None,
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
    match (
        chrono::DateTime::parse_from_rfc3339(later),
        chrono::DateTime::parse_from_rfc3339(anchor),
    ) {
        (Ok(l), Ok(a)) => l > a,
        _ => later > anchor,
    }
}
