//! Recall-time collapse of near-duplicate agent observations (issue #163).
//!
//! `eg query semantic-memory` can now group near-duplicate observations into
//! clusters and return one representative row per cluster instead of every
//! row. The function [`collapse_memory_recall`] is the single, deterministic
//! implementation; both the CLI recall path and the test fixtures call it.
//!
//! # Contract (acceptance criteria summary)
//!
//! * Input: the candidate rows a recall pass already produced, in recall
//!   order. Candidates are owned values, never live graph handles.
//! * Eligibility is conservative: agent-authored observation-class records
//!   only, sharing the same primary cited code target. Distinct temporal
//!   citations (`as_of_commit`) never merge, and a record citing no target at
//!   all is always its own singleton cluster.
//! * Similarity: cosine similarity over stored embedding vectors in
//!   [`CollapseMode::EmbeddingCosine`] (missing vectors fail closed to
//!   singleton behavior), normalized-text equality in
//!   [`CollapseMode::NormalizedText`]. The threshold is a float in
//!   `[0.0, 1.0]`; it is echoed in the answer envelope and monotonic in the
//!   partition sense documented by [`THRESHOLD_MONOTONICITY_DOC`].
//! * Clustering: deterministic connected components over above-threshold
//!   edges, evaluated in candidate order with a smallest-root union-find so
//!   the result is byte-stable across runs.
//! * Representatives: highest confidence, then earliest `observed_at`, then
//!   smallest record ID (mirrors `eg query duplicate-group candidates`).
//! * Output: an answer envelope (requested mode, actual mode, threshold,
//!   source count, representative count) followed by one compact row per
//!   representative carrying `cluster_size`, `member_ids`,
//!   `cluster_observed_at_min`, `cluster_observed_at_max`, and a
//!   `trust_spread` histogram over the #114 trust classes of its members.
//!
//! Recall-time collapse is deliberately weaker than merge: nothing is
//! rewritten, nothing is deleted, and the store fingerprint is untouched.
//! It differs from #94 (composition-health measurement, which explicitly
//! never collapses), from #131 (budget-fit packing, which fits an answer to
//! a token budget but collapses nothing), and from #92 (supersession
//! flagging — orthogonal to clustering: a cluster may mix trust classes and
//! reports that mix in `trust_spread`).

use std::collections::BTreeMap;

use serde::Serialize;

use crate::query::TrustClass;

/// Default cosine-similarity threshold for collapse clustering.
///
/// Pinned as part of the public contract so CLI, tests, and documentation
/// agree on one value. Raising it monotonically refines the partition
/// (splits clusters, never merges); see [`THRESHOLD_MONOTONICITY_DOC`].
pub const DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD: f32 = 0.85;

/// Threshold-monotonicity note echoed in every collapse answer envelope.
///
/// Clustering is deterministic connected components over above-threshold
/// edges, so raising the threshold only ever *removes* edges: clusters can
/// only split, never merge. Lowering it only ever *adds* edges: clusters can
/// only merge, never split.
pub const THRESHOLD_MONOTONICITY_DOC: &str = "raising the threshold monotonically refines the partition (clusters only split, never merge); lowering it monotonically coarsens it (clusters only merge, never split)";

/// Collapse clustering mode (issue #163).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[must_use]
pub enum CollapseMode {
    /// Cosine similarity over stored embedding vectors. Requires the store's
    /// vector index to load; missing vectors fail closed to singleton
    /// behavior rather than being guessed.
    #[serde(rename = "embedding-cosine")]
    EmbeddingCosine,
    /// Normalized-text equality over the stored (redacted) body text.
    /// Works without embeddings and never loads a model.
    #[serde(rename = "normalized-text")]
    NormalizedText,
}

/// The primary code target a memory record cites, as used for collapse
/// eligibility.
///
/// Relation is `OBSERVES` or `MENTIONS_SYMBOL`; the target record ID and the
/// relation are taken from the citation's target. `as_of_commit` is part of
/// the identity: the same record cited at two commits is two different
/// targets, so distinct temporal citations never merge. The relation is NOT
/// part of the cluster identity (issue #163): two observations citing the
/// same target record through different relations (`OBSERVES` vs
/// `MENTIONS_SYMBOL`) are about the same code, so with similar text they are
/// eligible to collapse together. The relation is still carried on the
/// representative row for provenance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PrimaryCitedTarget {
    /// Record ID of the cited code record.
    pub target_record_id: String,
    /// Citation relation: `OBSERVES` or `MENTIONS_SYMBOL`.
    pub relation: String,
    /// Commit the citation was taken against, when recorded.
    pub as_of_commit: Option<String>,
}

/// One recall candidate eligible for collapse clustering.
///
/// All fields are owned so clustering never holds a live graph handle; the
/// CLI builds these from the recall rows it already has.
#[derive(Debug, Clone)]
pub struct CollapseCandidate {
    /// Stored record ID.
    pub record_id: String,
    /// Stored node kind (`Observation`, `Decision`, `Failure`).
    pub kind: String,
    /// #114 trust class of the candidate.
    pub trust_class: TrustClass,
    /// Whether the record is agent-authored memory. Only agent-authored
    /// candidates cluster; deterministic code facts always stay singletons.
    pub agent_authored: bool,
    /// Primary cited code target, when the record cites one.
    pub primary_target: Option<PrimaryCitedTarget>,
    /// Stored (redacted) body text.
    pub body_text: String,
    /// Stored confidence as a string, as recalled.
    pub confidence_raw: Option<String>,
    /// Stored confidence parsed as a float, for representative ranking.
    pub confidence: Option<f32>,
    /// Stored `observed_at` timestamp, as recalled.
    pub observed_at: Option<String>,
    /// Stored embedding vector, when the mode needs it. Missing vectors
    /// fail closed to singleton behavior in embedding mode.
    pub vector: Option<Vec<f32>>,
    /// Retrieval score from the recall pass, for canonical cluster ordering.
    pub retrieval_score: Option<f32>,
    /// Provenance handle of the record.
    pub source_handle: Option<String>,
    /// Agent that produced the record, if recorded.
    pub agent_id: Option<String>,
    /// Agent kind, if recorded.
    pub agent_kind: Option<String>,
    /// Session that produced the record, if recorded.
    pub session_id: Option<String>,
    /// The record's stored evidence links, as recorded (issue #163: the
    /// representative retains its own provenance, including evidence links).
    pub evidence_links: Option<Vec<crate::EvidenceLink>>,
}

/// Collapse configuration: one mode and one threshold.
#[derive(Debug, Clone)]
pub struct CollapseConfig {
    /// Clustering mode.
    pub mode: CollapseMode,
    /// Similarity threshold in `[0.0, 1.0]`; echoed verbatim in the answer
    /// envelope. Ignored by normalized-text equality clustering.
    pub similarity_threshold: f32,
}

/// One collapse cluster: a representative and its members.
///
/// `members` always starts with the representative; the rest are ordered by
/// the same representative ranking so the answer is byte-deterministic.
#[derive(Debug, Clone)]
pub struct CollapsedCluster {
    /// The chosen representative candidate.
    pub representative: CollapseCandidate,
    /// All members, representative first, in representative-ranking order.
    pub members: Vec<CollapseCandidate>,
    /// Earliest `observed_at` across members, if any member carries one.
    pub observed_at_min: Option<String>,
    /// Latest `observed_at` across members, if any member carries one.
    pub observed_at_max: Option<String>,
    /// Histogram of #114 trust classes across members.
    pub trust_spread: BTreeMap<String, usize>,
}

/// Outcome of [`collapse_memory_recall`].
#[derive(Debug, Clone)]
pub struct CollapseOutcome {
    /// Mode the clustering actually used.
    pub mode: CollapseMode,
    /// Threshold the clustering used.
    pub threshold: f32,
    /// Number of candidates fed in.
    pub source_candidate_count: usize,
    /// Resulting clusters in canonical order: representative retrieval
    /// score descending (`None` last), then representative record ID
    /// ascending.
    pub clusters: Vec<CollapsedCluster>,
}

/// Serializable view of a cluster's primary cited target.
#[derive(Debug, Clone, Serialize)]
pub struct PrimaryCitedTargetView {
    /// Record ID of the cited code record.
    pub record_id: String,
    /// Citation relation: `OBSERVES` or `MENTIONS_SYMBOL`.
    pub relation: String,
    /// Commit the citation was taken against, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_commit: Option<String>,
}

impl From<&PrimaryCitedTarget> for PrimaryCitedTargetView {
    fn from(target: &PrimaryCitedTarget) -> Self {
        Self {
            record_id: target.target_record_id.clone(),
            relation: target.relation.clone(),
            as_of_commit: target.as_of_commit.clone(),
        }
    }
}

/// One compact representative row for the collapsed answer (issue #163).
///
/// Keeps the stored fields of the real representative record and adds the
/// cluster membership fields so the answer is self-contained: consumers can
/// recover every collapsed member ID without a second query.
#[derive(Debug, Clone, Serialize)]
pub struct CollapsedMemoryRow {
    /// Stored record ID of the representative.
    pub record_id: String,
    /// Stored node kind.
    pub kind: String,
    /// Kept as `"agent_authored"` to match the `eg query semantic-memory`
    /// contract; the fine-grained #114 mix is reported in `trust_spread`.
    pub trust_class: &'static str,
    /// #114 trust class of the representative itself, for text output and
    /// consumers that do not parse the spread.
    pub representative_trust_class: String,
    /// Representative's retrieval score, when the recall pass ranked it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval_score: Option<f32>,
    /// Provenance handle of the representative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_handle: Option<String>,
    /// Agent that produced the representative, if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Agent kind, if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    /// Session that produced the representative, if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Stored confidence of the representative, as recalled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Stored `observed_at` of the representative, as recalled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Representative's stored body text.
    pub memory_text: String,
    /// Number of members in the cluster (always ≥ 1).
    pub cluster_size: usize,
    /// Member record IDs, representative first, remaining IDs in
    /// representative-ranking order.
    pub member_ids: Vec<String>,
    /// Earliest `observed_at` across members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_observed_at_min: Option<String>,
    /// Latest `observed_at` across members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_observed_at_max: Option<String>,
    /// Histogram of #114 trust classes across members.
    pub trust_spread: BTreeMap<String, usize>,
    /// Primary cited code target of the representative, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_cited_target: Option<PrimaryCitedTargetView>,
    /// The representative's stored evidence links, as recorded. Carried
    /// verbatim from the store (record IDs, relations, spans, commits) so
    /// the representative keeps its full provenance; never synthesized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_links: Option<Vec<crate::EvidenceLink>>,
    /// Recall-state label (issue #156). Present only when the caller asked
    /// for retired records to be included in recall; otherwise retired
    /// records never reach the row stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retirement_state: Option<crate::memory_retire::RetirementStateLabel>,
}

/// Collapse envelope emitted before the representative rows.
///
/// The envelope carries the requested mode as the CLI saw it, the mode the
/// clustering actually used, and the threshold, so `auto` degradations are
/// always visible to the caller.
#[derive(Debug, Clone, Serialize)]
pub struct CollapseEnvelope {
    /// Always `true` for a successful collapsed answer.
    pub ok: bool,
    /// The query string, echoed for provenance.
    pub query: String,
    /// Collapse parameters and outcome counts.
    pub collapse: CollapseEnvelopeDetail,
}

/// Detail block of [`CollapseEnvelope`].
#[derive(Debug, Clone, Serialize)]
pub struct CollapseEnvelopeDetail {
    /// Always `true` when an envelope is emitted.
    pub enabled: bool,
    /// Mode the clustering actually used.
    pub mode: CollapseMode,
    /// Mode the caller requested (`auto`, `embedding-cosine`,
    /// `normalized-text`).
    pub mode_requested: String,
    /// Similarity threshold used.
    pub similarity_threshold: f32,
    /// Threshold-monotonicity note; see [`THRESHOLD_MONOTONICITY_DOC`].
    pub threshold_monotonicity: &'static str,
    /// Number of recall candidates fed into clustering.
    pub source_records: usize,
    /// Number of representative rows that follow.
    pub representatives: usize,
    /// `source_records - representatives`.
    pub collapsed_away: usize,
}

/// Deterministic union-find keyed on candidate position.
///
/// `find` path-compresses and `union` keeps the smaller root, so root
/// assignment — and therefore component enumeration — depends only on the
/// order edges were added, which follows candidate order.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(size: usize) -> Self {
        Self {
            parent: (0..size).collect(),
        }
    }

    fn find(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            let root = self.find(self.parent[index]);
            self.parent[index] = root;
        }
        self.parent[index]
    }

    fn union(&mut self, first: usize, second: usize) {
        let (mut root_first, mut root_second) = (self.find(first), self.find(second));
        if root_first == root_second {
            return;
        }
        if root_first > root_second {
            std::mem::swap(&mut root_first, &mut root_second);
        }
        self.parent[root_second] = root_first;
    }
}

/// Normalizes stored memory text for normalized-text equality clustering.
///
/// Case-folds and collapses all whitespace runs (including newlines) to
/// single spaces. Takes already-redacted stored text; redaction happens at
/// recall time, not here.
#[must_use]
pub fn normalize_memory_text(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cosine similarity of two vectors.
///
/// Returns `None` when the vectors are empty, of different lengths, or
/// either has zero norm — every such case fails closed to singleton
/// behavior in embedding mode.
#[must_use]
pub fn cosine_similarity(first: &[f32], second: &[f32]) -> Option<f32> {
    if first.len() != second.len() || first.is_empty() {
        return None;
    }
    let mut dot = 0.0f32;
    let mut first_norm = 0.0f32;
    let mut second_norm = 0.0f32;
    for (x, y) in first.iter().zip(second.iter()) {
        dot += x * y;
        first_norm += x * x;
        second_norm += y * y;
    }
    if first_norm <= 0.0 || second_norm <= 0.0 {
        return None;
    }
    Some(dot / (first_norm.sqrt() * second_norm.sqrt()))
}

/// Representative ranking: highest confidence, then earliest `observed_at`,
/// then smallest record ID.
///
/// This total order is the documented contract (issue #163): it picks the
/// representative deterministically and orders the remaining members in the
/// answer. `None` confidence sorts last; `None` `observed_at` sorts last.
fn representative_order(
    first: &CollapseCandidate,
    second: &CollapseCandidate,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let first_confidence = first.confidence.unwrap_or(f32::NEG_INFINITY);
    let second_confidence = second.confidence.unwrap_or(f32::NEG_INFINITY);
    second_confidence
        .total_cmp(&first_confidence)
        .then_with(|| match (&first.observed_at, &second.observed_at) {
            // Earliest INSTANT first: text order disagrees with time order
            // across differing RFC 3339 offsets. Unparseable stamps fall back
            // to text order.
            (Some(a), Some(b)) => match (
                chrono::DateTime::parse_from_rfc3339(a),
                chrono::DateTime::parse_from_rfc3339(b),
            ) {
                (Ok(left), Ok(right)) => left.cmp(&right),
                _ => a.cmp(b),
            },
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| first.record_id.cmp(&second.record_id))
}

/// Whether two candidates of the same target group are similar enough to
/// merge under `config`.
fn above_threshold(
    first: &CollapseCandidate,
    second: &CollapseCandidate,
    config: &CollapseConfig,
) -> bool {
    match config.mode {
        CollapseMode::EmbeddingCosine => match (&first.vector, &second.vector) {
            (Some(first_vector), Some(second_vector)) => {
                cosine_similarity(first_vector, second_vector)
                    .is_some_and(|similarity| similarity >= config.similarity_threshold)
            }
            _ => false,
        },
        CollapseMode::NormalizedText => {
            normalize_memory_text(&first.body_text) == normalize_memory_text(&second.body_text)
        }
    }
}

/// Collapses recall candidates into deterministic clusters.
///
/// Eligibility: candidates must share the same primary cited target
/// (including `as_of_commit`). Candidates with no cited target are each
/// their own singleton. Only agent-authored candidates of the same kind
/// can merge — callers are expected to filter to observation-class records
/// and the agent-authored flag before calling, and pairs are checked anyway.
///
/// Clustering is deterministic connected components over above-threshold
/// edges added in candidate order; clusters come out in canonical order
/// (representative retrieval score descending, `None` last, then
/// representative record ID ascending).
#[must_use]
pub fn collapse_memory_recall(
    candidates: &[CollapseCandidate],
    config: &CollapseConfig,
) -> CollapseOutcome {
    let mut union = UnionFind::new(candidates.len());
    // Group candidate positions by primary cited target. Each group key is
    // (target record ID, as_of_commit) — deterministic via BTreeMap — so
    // distinct temporal citations never share an edge. The citation relation
    // is deliberately not part of the key (issue #163): eligibility is the
    // same resolved primary target record ID plus similarity.
    let mut target_groups: BTreeMap<(String, Option<String>), Vec<usize>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if let Some(target) = &candidate.primary_target {
            target_groups
                .entry((target.target_record_id.clone(), target.as_of_commit.clone()))
                .or_default()
                .push(index);
        }
    }
    for members in target_groups.values() {
        for (position, &first) in members.iter().enumerate() {
            for &second in &members[position + 1..] {
                if candidates[first].agent_authored
                    && candidates[second].agent_authored
                    && candidates[first].kind == candidates[second].kind
                    && above_threshold(&candidates[first], &candidates[second], config)
                {
                    union.union(first, second);
                }
            }
        }
    }
    // Enumerate components in first-candidate order: roots are the smallest
    // member index, so iterating candidates ascending keeps it stable.
    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..candidates.len() {
        let root = union.find(index);
        components.entry(root).or_default().push(index);
    }
    let mut clusters: Vec<CollapsedCluster> = components
        .into_values()
        .map(|mut positions| {
            positions.sort_by(|&first, &second| {
                representative_order(&candidates[first], &candidates[second])
            });
            let members: Vec<CollapseCandidate> = positions
                .iter()
                .map(|&index| candidates[index].clone())
                .collect();
            let mut observed: Vec<&str> = members
                .iter()
                .filter_map(|member| member.observed_at.as_deref())
                .collect();
            // Order by INSTANT (text order is wrong across differing RFC 3339
            // offsets); the original strings are kept for serialization.
            observed.sort_by(|a, b| {
                match (
                    chrono::DateTime::parse_from_rfc3339(a),
                    chrono::DateTime::parse_from_rfc3339(b),
                ) {
                    (Ok(left), Ok(right)) => left.cmp(&right),
                    _ => a.cmp(b),
                }
            });
            let mut trust_spread: BTreeMap<String, usize> = BTreeMap::new();
            for member in &members {
                *trust_spread
                    .entry(member.trust_class.as_str().to_owned())
                    .or_default() += 1;
            }
            CollapsedCluster {
                representative: members[0].clone(),
                observed_at_min: observed.first().map(|text| (*text).to_owned()),
                observed_at_max: observed.last().map(|text| (*text).to_owned()),
                trust_spread,
                members,
            }
        })
        .collect();
    clusters.sort_by(|first, second| {
        let first_score = first
            .representative
            .retrieval_score
            .unwrap_or(f32::NEG_INFINITY);
        let second_score = second
            .representative
            .retrieval_score
            .unwrap_or(f32::NEG_INFINITY);
        second_score.total_cmp(&first_score).then_with(|| {
            first
                .representative
                .record_id
                .cmp(&second.representative.record_id)
        })
    });
    CollapseOutcome {
        mode: config.mode,
        threshold: config.similarity_threshold,
        source_candidate_count: candidates.len(),
        clusters,
    }
}

/// Renders one compact representative row per cluster.
#[must_use]
pub fn render_collapsed_rows(clusters: &[CollapsedCluster]) -> Vec<CollapsedMemoryRow> {
    clusters
        .iter()
        .map(|cluster| {
            let representative = &cluster.representative;
            CollapsedMemoryRow {
                record_id: representative.record_id.clone(),
                kind: representative.kind.clone(),
                trust_class: "agent_authored",
                representative_trust_class: representative.trust_class.as_str().to_owned(),
                retrieval_score: representative.retrieval_score,
                source_handle: representative.source_handle.clone(),
                agent_id: representative.agent_id.clone(),
                agent_kind: representative.agent_kind.clone(),
                session_id: representative.session_id.clone(),
                confidence: representative.confidence_raw.clone(),
                observed_at: representative.observed_at.clone(),
                memory_text: representative.body_text.clone(),
                cluster_size: cluster.members.len(),
                member_ids: cluster
                    .members
                    .iter()
                    .map(|member| member.record_id.clone())
                    .collect(),
                cluster_observed_at_min: cluster.observed_at_min.clone(),
                cluster_observed_at_max: cluster.observed_at_max.clone(),
                trust_spread: cluster.trust_spread.clone(),
                primary_cited_target: representative
                    .primary_target
                    .as_ref()
                    .map(PrimaryCitedTargetView::from),
                evidence_links: representative.evidence_links.clone(),
                retirement_state: None,
            }
        })
        .collect()
}

/// Builds the collapse answer envelope.
#[must_use]
pub fn collapse_envelope(
    query: &str,
    mode: CollapseMode,
    mode_requested: &str,
    similarity_threshold: f32,
    source_records: usize,
    representatives: usize,
) -> CollapseEnvelope {
    CollapseEnvelope {
        ok: true,
        query: query.to_owned(),
        collapse: CollapseEnvelopeDetail {
            enabled: true,
            mode,
            mode_requested: mode_requested.to_owned(),
            similarity_threshold,
            threshold_monotonicity: THRESHOLD_MONOTONICITY_DOC,
            source_records,
            representatives,
            collapsed_away: source_records.saturating_sub(representatives),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str) -> CollapseCandidate {
        CollapseCandidate {
            record_id: id.to_owned(),
            kind: "Observation".to_owned(),
            trust_class: TrustClass::AgentUnverified,
            agent_authored: true,
            primary_target: Some(PrimaryCitedTarget {
                target_record_id: "code:1".to_owned(),
                relation: "OBSERVES".to_owned(),
                as_of_commit: None,
            }),
            body_text: "same body".to_owned(),
            confidence_raw: None,
            confidence: None,
            observed_at: None,
            vector: None,
            retrieval_score: None,
            source_handle: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            evidence_links: None,
        }
    }

    #[test]
    fn normalize_memory_text_collapses_case_and_whitespace() {
        assert_eq!(
            normalize_memory_text("  The\nQUICK\tbrown   fox "),
            "the quick brown fox"
        );
        assert_eq!(normalize_memory_text(""), "");
    }

    #[test]
    fn cosine_similarity_rejects_degenerate_inputs() {
        assert!(cosine_similarity(&[], &[]).is_none());
        assert!(cosine_similarity(&[1.0], &[1.0, 2.0]).is_none());
        assert!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]).is_none());
    }

    #[test]
    fn cosine_similarity_self_is_one() {
        let similarity = cosine_similarity(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]).unwrap();
        assert!((similarity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn union_find_roots_are_smallest_member_index() {
        let mut union = UnionFind::new(4);
        union.union(3, 1);
        union.union(2, 3);
        assert_eq!(union.find(1), 1);
        assert_eq!(union.find(2), 1);
        assert_eq!(union.find(3), 1);
        assert_eq!(union.find(0), 0);
    }

    #[test]
    fn empty_input_yields_empty_outcome() {
        let outcome = collapse_memory_recall(
            &[],
            &CollapseConfig {
                mode: CollapseMode::NormalizedText,
                similarity_threshold: 0.85,
            },
        );
        assert!(outcome.clusters.is_empty());
        assert_eq!(outcome.source_candidate_count, 0);
    }

    #[test]
    fn targetless_candidates_stay_singletons_even_when_identical() {
        let mut first = candidate("a");
        first.primary_target = None;
        let mut second = candidate("b");
        second.primary_target = None;
        let outcome = collapse_memory_recall(
            &[first, second],
            &CollapseConfig {
                mode: CollapseMode::NormalizedText,
                similarity_threshold: 0.85,
            },
        );
        assert_eq!(outcome.clusters.len(), 2);
    }

    #[test]
    fn representative_prefers_earliest_instant_across_offsets() {
        // `00:30+01:00` is 23:30Z the day before: EARLIER than `00:00Z` as an
        // instant, though it sorts AFTER it as text.
        let mut earlier = candidate("agent_memory:v1:b");
        earlier.observed_at = Some("2026-01-01T00:30:00+01:00".to_owned());
        let mut later = candidate("agent_memory:v1:a");
        later.observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        assert_eq!(
            representative_order(&earlier, &later),
            std::cmp::Ordering::Less,
            "the earlier instant must rank first"
        );
    }

    #[test]
    fn cluster_time_bounds_follow_instants_across_offsets() {
        let mut earlier = candidate("agent_memory:v1:b");
        earlier.observed_at = Some("2026-01-01T00:30:00+01:00".to_owned());
        let mut later = candidate("agent_memory:v1:a");
        later.observed_at = Some("2026-01-01T00:00:00Z".to_owned());
        let outcome = collapse_memory_recall(
            &[earlier, later],
            &CollapseConfig {
                mode: CollapseMode::NormalizedText,
                similarity_threshold: 0.85,
            },
        );
        assert_eq!(outcome.clusters.len(), 1, "same body and target collapse");
        let cluster = &outcome.clusters[0];
        assert_eq!(
            cluster.observed_at_min.as_deref(),
            Some("2026-01-01T00:30:00+01:00")
        );
        assert_eq!(
            cluster.observed_at_max.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }
}
