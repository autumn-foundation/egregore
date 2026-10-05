//! Store-wide agent-memory evidence health sweep (issue #185).
//!
//! The agent-memory schema makes `evidence_links` required on every
//! `Observation` ("Must contain at least one link",
//! `docs/schema/agent-memory.md` §3/§5), so claims are backed *at write time*.
//! Code is temporal: across re-scans a cited target gets renamed or deleted
//! (its content-addressed `record_id` changes), or it drifts (a
//! [`SemanticDrift`](crate::ir::NodeKind::SemanticDrift) record already exists
//! for it, #55). Nothing swept the store to ask "which stored agent memories
//! now cite evidence that no longer resolves to live, current source?" —
//! per-record audit (#64) needs an id you already suspect, single-handle deref
//! (#160) checks one handle, and write-time tools (#183) never re-check as
//! the code moves underneath.
//!
//! This module is that sweep. For every live agent-authored observation-class
//! node ([`Observation`](crate::ir::NodeKind::Observation),
//! [`Decision`](crate::ir::NodeKind::Decision),
//! [`Failure`](crate::ir::NodeKind::Failure) —
//! [`Lesson`/`Hypothesis` have no `NodeKind` variants yet; see
//! [`is_observation_class_kind`]) it classifies each `evidence_links` entry
//! into exactly one bucket:
//!
//! - `resolves_live` — a live node with the target `record_id` exists and no
//!   live `SemanticDrift` record applies to it.
//! - `drifted` — a live node with the target `record_id` exists, but a live
//!   `SemanticDrift` record whose `prior_record_id` is the target applies (the
//!   "changed-span verdict": the cited span moved after it was cited). When
//!   several drift records apply, the earliest by `after_valid_time` (tie-broken
//!   by drift record id) is reported as `drift_record_id`.
//! - `dangling` — no live node carries the target `record_id` (absent, or
//!   tombstoned — distinguished by the `tombstoned` flag). A link with no
//!   `target_record_id` at all (unresolved triple) is `dangling`: it resolves
//!   to nothing.
//!
//! It also flags **integrity violations** (issue #185 AC3): the schema requires
//! the denormalized `evidence_links` array and the stored edge representation
//! to agree at write time (`docs/schema/agent-memory.md` §5), so a
//! `(target_record_id, relation)` pair present in one representation but not
//! the other is reported as `array_without_edge` / `edge_without_array`.
//!
//! It is a **read-time measurement** over data the store already holds: it
//! mints nothing, adds no node kind / edge label / trust class / domain, and
//! reads no wall clock or environment. [`run_memory_evidence_health_audit`] is
//! pure and deterministic — no I/O, no printing, byte-identical output across
//! runs (every map is a [`BTreeMap`], every row list is canonically sorted).
//!
//! Output is redaction-safe: rows carry only record IDs, domain/kind/relation
//! wire strings, closed-enum buckets, and counts — never raw source,
//! transcript, command, patch, or text payloads (mirrors #217 and the #188
//! failure-history serializer discipline).
//!
//! Scope boundary (issue #185 Out of Scope): this slice only *reports*. It
//! never repairs or re-points stale links, never deletes or retires rotted
//! records (that is #156's lifecycle job), never re-renders a drift verdict
//! (drift records are read, not minted), and never changes the
//! required-at-write-time `evidence_links` rule.
//!
//! Documented in `docs/cli/memory-evidence-health.md`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::evidence_link_audit::is_integrity_checked_evidence_edge;
use crate::ir::{EvidenceLink, GraphRecord, NodeKind, SemanticDriftMetadata};
use crate::query::liveness::Liveness;

/// Health bucket for one evidence link (issue #185 AC2). Exactly one applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkBucket {
    /// Target record exists and no drift verdict applies.
    ResolvesLive,
    /// Target exists but a `SemanticDrift`/changed-span verdict applies.
    Drifted,
    /// Target `record_id` is absent from the store (or tombstoned).
    Dangling,
}

/// Direction of an array-vs-edge representation disagreement (AC3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityViolationKind {
    /// The denormalized array cites a `(target, relation)` the stored edges lack.
    ArrayWithoutEdge,
    /// A stored evidence edge has no matching denormalized array entry.
    EdgeWithoutArray,
}

/// Per-evidence-link health row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkHealthRow {
    /// Citable `record_id` of the observation-class node carrying the link.
    pub source_record_id: String,
    /// Node kind wire name (`"Observation"`, `"Decision"`, `"Failure"`).
    pub source_kind: String,
    /// Cited target handle (`""` when the link carries no `target_record_id`).
    pub target_record_id: String,
    /// Target domain from the link (`"codegraph"`, `"agent_memory"`, …).
    pub target_domain: String,
    /// Cross-domain edge label wire name from the link (`"OBSERVES"`, …).
    pub relation: String,
    /// Exactly one of `resolves_live` / `drifted` / `dangling`.
    pub bucket: LinkBucket,
    /// The drift record applying to the target (only when `bucket` is `drifted`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_record_id: Option<String>,
    /// Whether the dangling target existed and was tombstoned (`true`) or was
    /// never present (`false`); only when `bucket` is `dangling`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstoned: Option<bool>,
}

/// One array-vs-edge representation disagreement (AC3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrityViolationRow {
    /// Citable `record_id` of the observation-class node.
    pub source_record_id: String,
    /// Node kind wire name.
    pub source_kind: String,
    /// Which representation is missing the pair.
    pub violation: IntegrityViolationKind,
    /// The `(target_record_id, relation)` pair that disagrees.
    pub target_record_id: String,
    /// Edge label wire name of the disagreeing pair.
    pub relation: String,
    /// Occurrences of the pair in the denormalized array.
    pub array_count: usize,
    /// Occurrences of the pair among stored evidence edges.
    pub edge_count: usize,
}

/// Store-wide counts, one per bucket (issue #185 AC7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryEvidenceHealthSummary {
    /// True when no link is dangling and no integrity violation exists.
    /// `drifted` links are freshness leads, not rot, so they do not fail the gate.
    pub ok: bool,
    /// Live observation-class nodes carrying at least one evidence link.
    pub sources_checked: usize,
    /// Total evidence links classified.
    pub links_checked: usize,
    /// Links whose target resolves live with no drift verdict.
    pub resolves_live: usize,
    /// Links whose target exists but has a drift verdict.
    pub drifted: usize,
    /// Links whose target `record_id` is absent (or tombstoned).
    pub dangling: usize,
    /// Array-vs-edge representation disagreements.
    pub integrity_violations: usize,
}

/// One newline-delimited JSON output line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryEvidenceHealthLine {
    /// Per-evidence-link health row.
    Link(LinkHealthRow),
    /// Array-vs-edge representation disagreement.
    IntegrityViolation(IntegrityViolationRow),
    /// Store-wide bucket counts (always last).
    Summary(MemoryEvidenceHealthSummary),
}

/// True for agent-authored observation-class node kinds swept by issue #185.
///
/// Covered today: `Observation`, `Decision`, `Failure` — the `NodeKind`
/// variants that exist. The acceptance criteria also name `Lesson` and
/// `Hypothesis` "where present": they have no `NodeKind` variants yet, so
/// there is nothing to sweep. When a future slice adds them, extend this
/// predicate (and the `docs/cli/memory-evidence-health.md` kind list); the
/// exhaustive call sites are this function only.
pub(crate) const fn is_observation_class_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Observation | NodeKind::Decision | NodeKind::Failure
    )
}

/// Live `SemanticDrift` metadata keyed by drift record id (append order: last
/// write wins), then indexed by `prior_record_id`.
fn index_drifts<'a>(
    records: &'a [GraphRecord],
    liveness: &Liveness,
) -> BTreeMap<&'a str, Vec<(&'a str, &'a SemanticDriftMetadata)>> {
    let mut by_id: BTreeMap<&str, &'a SemanticDriftMetadata> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            semantic_drift,
            ..
        } = record
        else {
            continue;
        };
        if *kind != NodeKind::SemanticDrift || liveness.deleted(id) {
            continue;
        }
        if let Some(meta) = semantic_drift {
            by_id.insert(id.as_str(), meta.as_ref());
        }
    }
    let mut by_prior: BTreeMap<&str, Vec<(&str, &SemanticDriftMetadata)>> = BTreeMap::new();
    for (drift_id, meta) in by_id {
        by_prior
            .entry(meta.prior_record_id.as_str())
            .or_default()
            .push((drift_id, meta));
    }
    by_prior
}

/// Earliest drift record applying to `target_id`: smallest `after_valid_time`,
/// tie-broken by drift record id (mirrors #85's earliest-post-anchor pick, but
/// without an anchor — issue #185 classifies against the whole store).
fn earliest_drift<'a>(
    by_prior: &BTreeMap<&'a str, Vec<(&'a str, &'a SemanticDriftMetadata)>>,
    target_id: &str,
) -> Option<&'a str> {
    by_prior.get(target_id).and_then(|candidates| {
        candidates
            .iter()
            .min_by(|(id_a, meta_a), (id_b, meta_b)| {
                meta_a
                    .after_valid_time
                    .cmp(&meta_b.after_valid_time)
                    .then_with(|| id_a.cmp(id_b))
            })
            .map(|(drift_id, _)| *drift_id)
    })
}

/// Latest node version per id (append order: last write wins) plus the set of
/// tombstoned record ids.
fn latest_nodes_and_tombstones(
    records: &[GraphRecord],
) -> (BTreeMap<&str, &GraphRecord>, BTreeSet<&str>) {
    let mut nodes_by_id: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut tombstoned: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        match record {
            GraphRecord::Node { id, .. } => {
                nodes_by_id.insert(id.as_str(), record);
            }
            GraphRecord::Tombstone { deleted_id, .. } => {
                tombstoned.insert(deleted_id.as_str());
            }
            GraphRecord::Edge { .. } => {}
        }
    }
    (nodes_by_id, tombstoned)
}

/// Live, latest-version, integrity-checked evidence edges per source node — the
/// stored edge representation of `evidence_links` (AC3). Single pass: the
/// latest physical version of each edge id counts, tombstoned edges do not
/// (mirrors #217's `collect_checked_refs`).
fn integrity_checked_evidence_edges<'a>(
    records: &'a [GraphRecord],
    liveness: &Liveness,
) -> BTreeMap<&'a str, BTreeMap<(&'a str, &'a str), usize>> {
    let mut edges_by_source: BTreeMap<&str, BTreeMap<(&str, &str), usize>> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if !is_integrity_checked_evidence_edge(*label)
            || !liveness.is_latest_edge_version(id, index)
            || liveness.deleted(id)
        {
            continue;
        }
        *edges_by_source
            .entry(source.as_str())
            .or_default()
            .entry((target.as_str(), label.as_str()))
            .or_default() += 1;
    }
    edges_by_source
}

/// Live observation-class source nodes, in stable id order (`BTreeMap`).
fn live_observation_sources<'a>(
    nodes_by_id: &BTreeMap<&'a str, &'a GraphRecord>,
    liveness: &Liveness,
) -> BTreeMap<&'a str, &'a GraphRecord> {
    let mut sources: BTreeMap<&'a str, &'a GraphRecord> = BTreeMap::new();
    for (id, record) in nodes_by_id {
        let GraphRecord::Node { kind, .. } = record else {
            continue;
        };
        if is_observation_class_kind(*kind) && !liveness.deleted(id) {
            sources.insert(id, record);
        }
    }
    sources
}

/// Classifies every evidence link of every live observation-class node into
/// exactly one [`LinkBucket`], flags array-vs-edge representation disagreements,
/// and appends the summary line.
///
/// Pure and deterministic: byte-identical output across runs on an unchanged
/// record slice.
#[must_use]
pub fn run_memory_evidence_health_audit(records: &[GraphRecord]) -> Vec<MemoryEvidenceHealthLine> {
    let liveness = Liveness::new(records);
    let (nodes_by_id, tombstoned) = latest_nodes_and_tombstones(records);
    let drifts_by_prior = index_drifts(records, &liveness);
    let edges_by_source = integrity_checked_evidence_edges(records, &liveness);
    let sources = live_observation_sources(&nodes_by_id, &liveness);

    let mut link_rows: Vec<LinkHealthRow> = Vec::new();
    let mut violation_rows: Vec<IntegrityViolationRow> = Vec::new();

    for (source_id, source) in &sources {
        let GraphRecord::Node {
            kind,
            evidence_links,
            ..
        } = source
        else {
            continue;
        };
        let source_kind = kind.as_str().to_owned();

        // ── Per-link bucket classification (AC2) ──────────────────────────
        if let Some(links) = evidence_links {
            for link in links {
                let target_id = link.target_record_id.as_deref().unwrap_or("");
                let bucket_row = classify_link(
                    source_id,
                    &source_kind,
                    link,
                    target_id,
                    &nodes_by_id,
                    &tombstoned,
                    &liveness,
                    &drifts_by_prior,
                );
                link_rows.push(bucket_row);
            }
        }

        // ── Array-vs-edge integrity check (AC3) ──────────────────────────
        let empty: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        let edge_counts = edges_by_source.get(source_id).unwrap_or(&empty);
        violation_rows.extend(check_representation_agreement(
            source_id,
            &source_kind,
            evidence_links.as_deref(),
            edge_counts,
        ));
    }

    // Canonical ordering: byte-identical across runs regardless of physical
    // store iteration order (BTreeMap already sorts sources; rows sort here).
    link_rows.sort_by(|a, b| {
        (&a.source_record_id, &a.target_record_id, &a.relation).cmp(&(
            &b.source_record_id,
            &b.target_record_id,
            &b.relation,
        ))
    });
    violation_rows.sort_by(|a, b| {
        (
            &a.source_record_id,
            a.violation,
            &a.target_record_id,
            &a.relation,
        )
            .cmp(&(
                &b.source_record_id,
                b.violation,
                &b.target_record_id,
                &b.relation,
            ))
    });

    let resolves_live = link_rows
        .iter()
        .filter(|row| row.bucket == LinkBucket::ResolvesLive)
        .count();
    let drifted = link_rows
        .iter()
        .filter(|row| row.bucket == LinkBucket::Drifted)
        .count();
    let dangling = link_rows
        .iter()
        .filter(|row| row.bucket == LinkBucket::Dangling)
        .count();

    let summary = MemoryEvidenceHealthSummary {
        // `drifted` is a freshness lead, not rot: the gate fails only on
        // dangling citations and representation corruption.
        ok: dangling == 0 && violation_rows.is_empty(),
        sources_checked: sources.len(),
        links_checked: link_rows.len(),
        resolves_live,
        drifted,
        dangling,
        integrity_violations: violation_rows.len(),
    };

    let mut lines: Vec<MemoryEvidenceHealthLine> =
        Vec::with_capacity(link_rows.len() + violation_rows.len() + 1);
    lines.extend(link_rows.into_iter().map(MemoryEvidenceHealthLine::Link));
    lines.extend(
        violation_rows
            .into_iter()
            .map(MemoryEvidenceHealthLine::IntegrityViolation),
    );
    lines.push(MemoryEvidenceHealthLine::Summary(summary));
    lines
}

/// Classifies one evidence link into exactly one [`LinkBucket`].
#[allow(clippy::too_many_arguments)]
fn classify_link(
    source_id: &str,
    source_kind: &str,
    link: &EvidenceLink,
    target_id: &str,
    nodes_by_id: &BTreeMap<&str, &GraphRecord>,
    tombstoned: &BTreeSet<&str>,
    liveness: &Liveness,
    drifts_by_prior: &BTreeMap<&str, Vec<(&str, &SemanticDriftMetadata)>>,
) -> LinkHealthRow {
    let target_live = !target_id.is_empty()
        && nodes_by_id.contains_key(target_id)
        && !liveness.deleted(target_id);
    let (bucket, drift_record_id, tombstoned_flag) = if target_live {
        earliest_drift(drifts_by_prior, target_id)
            .map_or((LinkBucket::ResolvesLive, None, None), |drift_id| {
                (LinkBucket::Drifted, Some(drift_id.to_owned()), None)
            })
    } else {
        // Dangling: distinguish "existed and was tombstoned" from "never seen".
        let was_present = !target_id.is_empty()
            && (nodes_by_id.contains_key(target_id) || tombstoned.contains(target_id));
        (LinkBucket::Dangling, None, Some(was_present))
    };
    LinkHealthRow {
        source_record_id: source_id.to_owned(),
        source_kind: source_kind.to_owned(),
        target_record_id: target_id.to_owned(),
        target_domain: link.target_domain.clone(),
        relation: link.relation.clone(),
        bucket,
        drift_record_id,
        tombstoned: tombstoned_flag,
    }
}

/// Compares a node's denormalized `evidence_links` array against its stored
/// edge representation (AC3): the multiset of `(target_record_id, relation)`
/// pairs in the array must equal the multiset of `(target, label)` pairs among
/// the node's live, latest-version, integrity-checked evidence edges.
///
/// Only edges whose label is evidence-checked per
/// [`is_integrity_checked_evidence_edge`] count (the #217 partition —
/// code-graph structural topology and intra-agent-memory scaffolding are never
/// grounding evidence). `edge_counts` is the precomputed per-source slice of
/// that edge index.
fn check_representation_agreement(
    source_id: &str,
    source_kind: &str,
    evidence_links: Option<&[EvidenceLink]>,
    edge_counts: &BTreeMap<(&str, &str), usize>,
) -> Vec<IntegrityViolationRow> {
    let mut array_counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    if let Some(links) = evidence_links {
        for link in links {
            let target = link.target_record_id.as_deref().unwrap_or("");
            *array_counts
                .entry((target, link.relation.as_str()))
                .or_default() += 1;
        }
    }

    let mut keys: BTreeSet<(&str, &str)> = BTreeSet::new();
    keys.extend(array_counts.keys().copied());
    keys.extend(edge_counts.keys().copied());

    let mut violations = Vec::new();
    for (target, relation) in keys {
        let array_count = array_counts.get(&(target, relation)).copied().unwrap_or(0);
        let edge_count = edge_counts.get(&(target, relation)).copied().unwrap_or(0);
        let violation = match array_count.cmp(&edge_count) {
            Ordering::Greater => Some(IntegrityViolationKind::ArrayWithoutEdge),
            Ordering::Less => Some(IntegrityViolationKind::EdgeWithoutArray),
            Ordering::Equal => None,
        };
        if let Some(violation) = violation {
            violations.push(IntegrityViolationRow {
                source_record_id: source_id.to_owned(),
                source_kind: source_kind.to_owned(),
                violation,
                target_record_id: target.to_owned(),
                relation: relation.to_owned(),
                array_count,
                edge_count,
            });
        }
    }
    violations
}

#[cfg(test)]
mod tests;
