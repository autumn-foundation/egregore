//! Cross-domain evidence witness path (`eg query evidence-path`, issue #247).
//!
//! Answers ONE question — "is record A grounded in record B, and by what chain?"
//! — by tracing the shortest connecting path between two record handles over the
//! graph's cross-domain **evidence/provenance** edge subgraph and returning one
//! deterministic, redaction-safe witness path (or an explicit `no_path` verdict).
//!
//! This lane is a **read-time traversal** — it mints no edge and introduces no
//! new node kind, edge label, or trust class. It reads only the edges already in
//! the graph, partitioned into a TRAVERSED evidence set and an EXCLUDED
//! code-topology set by [`is_evidence_path_edge`], an exhaustive match over every
//! [`EdgeLabel`] variant (so a new variant forces a conscious classification
//! decision at compile time — the completeness invariant).
//!
//! Reachability is **undirected** over that subgraph: a grounding chain
//! legitimately mixes edge directions (an `OBSERVES` edge points memory→code
//! while a `VALIDATED_BY` edge points memory→evidence), so each edge is usable in
//! either direction. Every emitted hop reports the edge's NATIVE `from`/`to` as
//! stored plus a `traversal_direction` (`forward` when the walk moved along the
//! stored edge, `reverse` when against it). The path is the DETERMINISTIC shortest
//! path: fewest hops, ties broken by the lexicographically smallest
//! `(neighbor_record_id, edge_record_id)` at each discovery step.
//!
//! A witness path proves a live evidence-edge chain connects two records; it is
//! never proof the cited code still matches current source, and `EMITTED_DURING`
//! hops are correlation leads, never causation. Output is deterministic and
//! byte-identical across runs; no raw source/transcript/command/patch text ever
//! enters the envelope — only IDs, domains, kinds, edge labels, paths, spans,
//! counts, and basis strings.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ir::{CorrelationBasis, EdgeLabel, GraphRecord, SourceSpan};

/// Always-present advisory stamped on every evidence-path envelope. A witness
/// path is a live-edge chain, NOT proof of current correctness or causation.
pub const EVIDENCE_PATH_DISCLAIMER: &str = "A witness path proves a live \
     evidence-edge chain connects these two records; it is not proof the cited code still \
     matches current source, and EMITTED_DURING hops are correlation leads, never causation. \
     Absence of a path over the traversed evidence-edge classes is not proof no grounding \
     exists: code-graph topology and other excluded edge classes are not traversed by design.";

/// Every [`EdgeLabel`] variant, in declaration order.
///
/// `EdgeLabel` exposes no iterator, so the partition helpers and the
/// completeness test enumerate over this hardcoded slice. The completeness test
/// asserts its length equals the number of classified variants, so a new variant
/// that is added to the enum but forgotten here is caught: the exhaustive match
/// in [`is_evidence_path_edge`] fails to compile first, and the count test fails
/// second.
const ALL_EDGE_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Contains,
    EdgeLabel::Defines,
    EdgeLabel::Imports,
    EdgeLabel::References,
    EdgeLabel::Calls,
    EdgeLabel::Implements,
    EdgeLabel::Mentions,
    EdgeLabel::ChangedIn,
    EdgeLabel::ParentOf,
    EdgeLabel::DriftsFrom,
    EdgeLabel::DriftsPrior,
    EdgeLabel::MeasuredBy,
    EdgeLabel::SessionOf,
    EdgeLabel::AuthoredBy,
    EdgeLabel::HasEvidence,
    EdgeLabel::Observes,
    EdgeLabel::MentionsSymbol,
    EdgeLabel::TouchedFile,
    EdgeLabel::ProducedPatch,
    EdgeLabel::ProducedEvidence,
    EdgeLabel::ValidatedBy,
    EdgeLabel::ClosesAcceptanceCriterion,
    EdgeLabel::OwnedByTask,
    EdgeLabel::ExternalHandle,
    EdgeLabel::TouchesFile,
    EdgeLabel::MergedAs,
    EdgeLabel::ReviewsCommit,
    EdgeLabel::ReviewedBy,
    EdgeLabel::RequestedReviewFrom,
    EdgeLabel::TransitionsReview,
    EdgeLabel::FailedOn,
    EdgeLabel::ExplainsChange,
    EdgeLabel::ReferencesTask,
    EdgeLabel::Contradicts,
    EdgeLabel::Supersedes,
    EdgeLabel::ProposedBy,
    EdgeLabel::PromptedFor,
    EdgeLabel::DecidedOn,
    EdgeLabel::MaterializedAs,
    EdgeLabel::RevokedBy,
    EdgeLabel::ScopedToRepo,
    EdgeLabel::RelatesTo,
    EdgeLabel::FingerprintedAs,
    EdgeLabel::CapturedFrom,
    EdgeLabel::Aggregates,
    EdgeLabel::FrameResolvesTo,
    EdgeLabel::EmittedDuring,
];

/// True if this edge is a cross-domain evidence/provenance edge the witness
/// path may traverse. Exhaustive match (no `_`) so a new [`EdgeLabel`] forces a
/// conscious classification decision at compile time — the completeness
/// invariant. See the module docs.
const fn is_evidence_path_edge(label: EdgeLabel) -> bool {
    use EdgeLabel::{
        Aggregates, AuthoredBy, Calls, CapturedFrom, ChangedIn, ClosesAcceptanceCriterion,
        Contains, Contradicts, DecidedOn, Defines, DriftsFrom, DriftsPrior, EmittedDuring,
        ExplainsChange, ExternalHandle, FailedOn, FingerprintedAs, FrameResolvesTo, HasEvidence,
        Implements, Imports, MaterializedAs, MeasuredBy, Mentions, MentionsSymbol, MergedAs,
        Observes, OwnedByTask, ParentOf, ProducedEvidence, ProducedPatch, PromptedFor, ProposedBy,
        References, ReferencesTask, RelatesTo, RequestedReviewFrom, ReviewedBy, ReviewsCommit,
        RevokedBy, ScopedToRepo, SessionOf, Supersedes, TouchedFile, TouchesFile,
        TransitionsReview, ValidatedBy,
    };
    match label {
        // TRAVERSED — cross-domain evidence / provenance / grounding edges.
        HasEvidence
        | Observes
        | MentionsSymbol
        | TouchedFile
        | ProducedPatch
        | ProducedEvidence
        | ValidatedBy
        | ClosesAcceptanceCriterion
        | OwnedByTask
        | ExternalHandle
        | TouchesFile
        | MergedAs
        | ReviewsCommit
        | ReviewedBy
        | RequestedReviewFrom
        | TransitionsReview
        | FailedOn
        | ExplainsChange
        | ReferencesTask
        | Contradicts
        | Supersedes
        | RelatesTo
        | FrameResolvesTo
        | EmittedDuring
        | MaterializedAs
        | ProposedBy
        | PromptedFor
        | DecidedOn
        | RevokedBy
        | ScopedToRepo
        | FingerprintedAs
        | CapturedFrom
        | Aggregates => true,
        // EXCLUDED — code-graph structural topology (that is the CALLS-walker
        // lane's job) and intra-agent-memory organizational scaffolding; not
        // grounding evidence.
        Contains | Defines | Imports | References | Calls | Implements | Mentions | ChangedIn
        | ParentOf | DriftsFrom | DriftsPrior | MeasuredBy | SessionOf | AuthoredBy => false,
    }
}

/// The sorted wire-string lists of traversed and excluded edge classes, derived
/// by partitioning [`ALL_EDGE_LABELS`] through [`is_evidence_path_edge`]. Carried
/// in the envelope so "no path" is never presented as proof no grounding exists.
fn edge_classes() -> (Vec<String>, Vec<String>) {
    let mut traversed: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    for &label in ALL_EDGE_LABELS {
        if is_evidence_path_edge(label) {
            traversed.push(label.as_str().to_owned());
        } else {
            excluded.push(label.as_str().to_owned());
        }
    }
    traversed.sort();
    excluded.sort();
    (traversed, excluded)
}

/// A universal, redaction-safe projection of one node `&GraphRecord`.
///
/// Carries ONLY bounded handles: record ID, schema version, domain, the derived
/// `trust_class`, node kind, and a repo-relative path + span for code rows. No
/// free-text field is ever emitted, so no raw payload can escape through a row.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct NodeRow {
    /// Stable record ID (citation handle).
    pub record_id: String,
    /// Schema version stamped on the record.
    pub schema_version: u32,
    /// Record domain (`codegraph`, `agent_memory`, `verification`, …).
    pub domain: String,
    /// Trust class derived from the domain — REQUIRED on every row.
    pub trust_class: &'static str,
    /// Node kind name (`Symbol`, `Observation`, `CommandRun`, …).
    pub kind: String,
    /// Repo-relative path when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// The edge object on one witness-path hop.
#[derive(Debug, Clone, Serialize)]
pub struct EdgeStep {
    /// Edge label wire string (`OBSERVES`, `VALIDATED_BY`, …).
    pub label: String,
    /// Stable edge record ID (citation handle for this hop).
    pub edge_record_id: String,
    /// `forward` when the walk moved along the stored edge (native `from` →
    /// native `to`), `reverse` when against it.
    pub traversal_direction: &'static str,
    /// Correlation basis, present ONLY on `EMITTED_DURING` hops:
    /// `content_hash_join` / `temporal_correlation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// Documented confidence for the basis, present alongside it:
    /// `content_hash_join` → `1.0`, `temporal_correlation` → `0.5`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
}

/// One hop of the witness path. `from`/`to` are the edge's NATIVE endpoints as
/// stored; `edge.traversal_direction` records which way the walk moved.
#[derive(Debug, Clone, Serialize)]
pub struct Hop {
    /// Zero-based position in the path (source-to-target order).
    pub index: usize,
    /// Native `from` endpoint node row.
    pub from: NodeRow,
    /// Native `to` endpoint node row.
    pub to: NodeRow,
    /// The connecting evidence edge.
    pub edge: EdgeStep,
}

/// The assembled witness path from `source` to `target`.
#[derive(Debug, Clone)]
pub struct EvidencePath {
    /// Source endpoint node row.
    pub source: NodeRow,
    /// Target endpoint node row.
    pub target: NodeRow,
    /// Ordered hops from source to target (length == `hop_count` >= 1).
    pub hops: Vec<Hop>,
    /// Sorted wire strings of the traversed evidence-edge classes.
    pub traversed_edge_classes: Vec<String>,
    /// Sorted wire strings of the excluded (never-traversed) edge classes.
    pub excluded_edge_classes: Vec<String>,
}

/// Which endpoint of a two-record request a problem concerns.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EndpointSide {
    /// The `source_id` (first positional argument).
    Source,
    /// The `target_id` (second positional argument).
    Target,
}

impl EndpointSide {
    /// The wire string (`source` / `target`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
        }
    }
}

/// Failure modes of [`evidence_path`], mapped to CLI exit codes by the caller.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum EvidencePathError {
    /// `source_id == target_id` (exit 1).
    IdenticalEndpoints {
        /// The shared handle as supplied.
        handle: String,
    },
    /// Both endpoints are live but no evidence-edge chain connects them (exit 1).
    /// A machine-readable verdict, NOT a silent empty list.
    NoPath {
        /// Source endpoint node row (boxed to keep the enum small).
        source: Box<NodeRow>,
        /// Target endpoint node row (boxed to keep the enum small).
        target: Box<NodeRow>,
        /// Sorted wire strings of the traversed evidence-edge classes.
        traversed_edge_classes: Vec<String>,
        /// Sorted wire strings of the excluded edge classes.
        excluded_edge_classes: Vec<String>,
    },
    /// An endpoint names no record in the graph (exit 2).
    EndpointNotFound {
        /// Which side is absent.
        side: EndpointSide,
        /// The failing handle as supplied.
        handle: String,
    },
    /// An endpoint names a tombstoned (retracted / deleted) record (exit 2).
    /// A DISTINCT label from `endpoint_not_found`.
    EndpointTombstoned {
        /// Which side is tombstoned.
        side: EndpointSide,
        /// The failing handle as supplied.
        handle: String,
    },
}

/// Maps a record domain to its trust class (mirrors `error_context`).
const fn trust_class_for_domain(domain: &str) -> &'static str {
    match domain.as_bytes() {
        b"codegraph" => "source_fact",
        b"agent_memory" => "agent_observation",
        b"verification" => "verification",
        b"artifact" => "artifact",
        b"project" => "project_state",
        b"log" => "runtime_observation",
        b"semantic" => "semantic",
        b"user_context" => "user_context",
        _ => "unknown",
    }
}

/// Resolves a record's domain from its stamped `domain` field, falling back to
/// its stable-ID prefix.
fn record_domain(record: &GraphRecord) -> String {
    if let GraphRecord::Node {
        domain: Some(domain),
        ..
    } = record
    {
        return domain.clone();
    }
    crate::schema_version::domain_from_record_id(record.id())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Projects one node record into a redaction-safe [`NodeRow`].
fn project_node_row(record: &GraphRecord) -> Option<NodeRow> {
    let GraphRecord::Node {
        id,
        kind,
        schema_version,
        repo_relative_path,
        span,
        ..
    } = record
    else {
        return None;
    };
    let domain = record_domain(record);
    let trust_class = trust_class_for_domain(&domain);
    Some(NodeRow {
        record_id: id.clone(),
        schema_version: *schema_version,
        domain,
        trust_class,
        kind: kind.as_str().to_owned(),
        repo_relative_path: repo_relative_path.clone(),
        span: *span,
    })
}

/// One undirected adjacency entry: the neighbor reachable across a live evidence
/// edge, plus the stored edge's identity and native orientation.
#[derive(Clone, Copy)]
struct Adjacency<'a> {
    /// The other endpoint of this edge (the neighbor).
    neighbor: &'a str,
    /// Stable edge record ID.
    edge_id: &'a str,
    /// Edge label.
    label: EdgeLabel,
    /// Native `from` endpoint as stored.
    native_from: &'a str,
    /// Native `to` endpoint as stored.
    native_to: &'a str,
    /// Correlation basis carried by the edge, when any.
    basis: Option<CorrelationBasis>,
}

/// A shortest-path discovery pointer: how a node was first reached.
#[derive(Clone, Copy)]
struct Discovery<'a> {
    /// The already-visited neighbor the walk arrived from.
    parent: &'a str,
    /// The connecting edge's identity/orientation/basis.
    adj: Adjacency<'a>,
}

/// Traces the deterministic shortest evidence-edge witness path between two
/// record handles (issue #247).
///
/// The traversal is an undirected, level-synchronized BFS over the evidence-edge
/// subgraph (the labels classified as evidence/provenance by the exhaustive
/// edge-label partition). Deleted records (tombstoned and not carrying a
/// bitemporal `temporal` version) and edges touching a deleted endpoint are
/// excluded — a current-state view. On ties the lexicographically smallest
/// `(neighbor_record_id, edge_record_id)` discovery is kept, so the reported path
/// is byte-stable. A `visited` set guarantees termination on cycles.
///
/// # Errors
///
/// Returns [`EvidencePathError`] on identical endpoints, an absent or tombstoned
/// endpoint, or two live endpoints with no connecting evidence chain.
///
/// # Panics
///
/// Panics only on a violated internal invariant: by construction every reached
/// node and every native edge endpoint on the reconstructed path is a live node
/// record with a discovery pointer chaining back to the source.
#[allow(clippy::too_many_lines)]
pub fn evidence_path(
    records: &[GraphRecord],
    source_id: &str,
    target_id: &str,
) -> Result<EvidencePath, EvidencePathError> {
    let (traversed_edge_classes, excluded_edge_classes) = edge_classes();

    // Identical endpoints: a witness path needs two distinct records.
    if source_id == target_id {
        return Err(EvidencePathError::IdenticalEndpoints {
            handle: source_id.to_owned(),
        });
    }

    // ── tombstone / temporal liveness gate (mirrors transitive_callers) ───────
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let has_temporal: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            }
            | GraphRecord::Edge {
                id,
                temporal: Some(_),
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let deleted = |id: &str| tombstoned.contains(id) && !has_temporal.contains(id);

    // Live node index: nodes present and not deleted.
    let by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { id, .. } if !deleted(id) => Some((id.as_str(), r)),
            _ => None,
        })
        .collect();

    // Endpoint resolution: Live / Tombstoned / Absent. Source's problem is
    // reported first when both endpoints are bad (deterministic).
    let resolve_endpoint =
        |id: &str, side: EndpointSide| -> Result<&GraphRecord, EvidencePathError> {
            if let Some(record) = by_id.get(id) {
                return Ok(record);
            }
            if deleted(id) {
                return Err(EvidencePathError::EndpointTombstoned {
                    side,
                    handle: id.to_owned(),
                });
            }
            Err(EvidencePathError::EndpointNotFound {
                side,
                handle: id.to_owned(),
            })
        };
    let source_record = resolve_endpoint(source_id, EndpointSide::Source)?;
    let target_record = resolve_endpoint(target_id, EndpointSide::Target)?;
    let source_id: &str = source_record.id();
    let target_id: &str = target_record.id();

    // ── undirected adjacency over live evidence edges ─────────────────────────
    let mut adjacency: BTreeMap<&str, Vec<Adjacency<'_>>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            basis,
            ..
        } = r
        {
            if deleted(id.as_str()) || !is_evidence_path_edge(*label) {
                continue;
            }
            // A live edge must touch two live endpoints to be usable.
            if source == target
                || deleted(source.as_str())
                || deleted(target.as_str())
                || !by_id.contains_key(source.as_str())
                || !by_id.contains_key(target.as_str())
            {
                continue;
            }
            let entry = Adjacency {
                neighbor: target.as_str(),
                edge_id: id.as_str(),
                label: *label,
                native_from: source.as_str(),
                native_to: target.as_str(),
                basis: *basis,
            };
            adjacency.entry(source.as_str()).or_default().push(entry);
            adjacency
                .entry(target.as_str())
                .or_default()
                .push(Adjacency {
                    neighbor: source.as_str(),
                    ..entry
                });
        }
    }
    // Deterministic neighbor visit order; drop exact duplicates (same neighbor +
    // same edge ID) from history views where a stable edge ID recurs.
    for entries in adjacency.values_mut() {
        entries.sort_by(|a, b| (a.neighbor, a.edge_id).cmp(&(b.neighbor, b.edge_id)));
        entries.dedup_by_key(|e| (e.neighbor, e.edge_id));
    }

    // ── level-synchronized undirected BFS from the source ─────────────────────
    let mut parent: BTreeMap<&str, Discovery<'_>> = BTreeMap::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    visited.insert(source_id);
    let mut frontier: Vec<&str> = vec![source_id];

    while !frontier.is_empty() && !visited.contains(target_id) {
        // Keep the minimum (neighbor_id, edge_id) discovery per newly reached node.
        let mut discoveries: BTreeMap<&str, Discovery<'_>> = BTreeMap::new();
        for &node in &frontier {
            let Some(entries) = adjacency.get(node) else {
                continue;
            };
            for &adj in entries {
                if visited.contains(adj.neighbor) {
                    continue;
                }
                let candidate = Discovery { parent: node, adj };
                match discoveries.entry(adj.neighbor) {
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(candidate);
                    }
                    std::collections::btree_map::Entry::Occupied(mut e) => {
                        let prev = *e.get();
                        if (candidate.parent, candidate.adj.edge_id)
                            < (prev.parent, prev.adj.edge_id)
                        {
                            e.insert(candidate);
                        }
                    }
                }
            }
        }
        if discoveries.is_empty() {
            break;
        }
        frontier = discoveries.keys().copied().collect();
        for (node, discovery) in discoveries {
            visited.insert(node);
            parent.insert(node, discovery);
        }
    }

    // No connecting chain: an explicit verdict, never a silent empty path.
    if !visited.contains(target_id) {
        return Err(EvidencePathError::NoPath {
            source: Box::new(project_node_row(source_record).expect("source is a live node")),
            target: Box::new(project_node_row(target_record).expect("target is a live node")),
            traversed_edge_classes,
            excluded_edge_classes,
        });
    }

    // ── reconstruct the chain from target back to source, then reverse ────────
    let mut reversed: Vec<Discovery<'_>> = Vec::new();
    let mut cursor = target_id;
    while cursor != source_id {
        let discovery = *parent
            .get(cursor)
            .expect("every reached node has a discovery pointer");
        reversed.push(discovery);
        cursor = discovery.parent;
    }
    reversed.reverse();

    let mut hops: Vec<Hop> = Vec::with_capacity(reversed.len());
    for (index, discovery) in reversed.into_iter().enumerate() {
        let adj = discovery.adj;
        let from_record = by_id
            .get(adj.native_from)
            .copied()
            .expect("native edge endpoint is a live node");
        let to_record = by_id
            .get(adj.native_to)
            .copied()
            .expect("native edge endpoint is a live node");
        // The walk moved from `discovery.parent` to `adj.neighbor`. When the
        // parent is the stored `from`, it moved along the edge (forward); else
        // it moved against it (reverse).
        let traversal_direction = if discovery.parent == adj.native_from {
            "forward"
        } else {
            "reverse"
        };
        let (basis, confidence) = if adj.label == EdgeLabel::EmittedDuring {
            adj.basis.map_or((None, None), |b| {
                (Some(b.as_str().to_owned()), Some(b.confidence().to_owned()))
            })
        } else {
            (None, None)
        };
        hops.push(Hop {
            index,
            from: project_node_row(from_record).expect("native endpoint is a live node"),
            to: project_node_row(to_record).expect("native endpoint is a live node"),
            edge: EdgeStep {
                label: adj.label.as_str().to_owned(),
                edge_record_id: adj.edge_id.to_owned(),
                traversal_direction,
                basis,
                confidence,
            },
        });
    }

    Ok(EvidencePath {
        source: project_node_row(source_record).expect("source is a live node"),
        target: project_node_row(target_record).expect("target is a live node"),
        hops,
        traversed_edge_classes,
        excluded_edge_classes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{NodeKind, TemporalMetadata};

    // ── fixture helpers ───────────────────────────────────────────────────────

    fn code_symbol(id: &str, path: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn memory_node(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "observation".to_owned(),
        )
        .with_domain("agent_memory", 1)
    }

    fn verification_node(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Verification,
            None,
            None,
            None,
            "verification".to_owned(),
        )
        .with_domain("verification", 1)
    }

    fn evidence_edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            format!("{} edge", label.as_str()),
        )
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v5:tomb_{deleted_id}"),
            schema_version: 5,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    // ── 1. success metric fixture: cross-domain chain spanning >= 3 domains ────

    #[test]
    fn traces_cross_domain_witness_path_deterministically() {
        // agent_memory observation --OBSERVES--> codegraph symbol
        //   --VALIDATED_BY (from memory)--> nothing; instead:
        // memory --VALIDATED_BY--> verification node.
        // Chain: obs --OBSERVES--> sym ; sym <--?  we need sym connected to verif.
        // Use: obs --OBSERVES--> sym, obs --VALIDATED_BY--> verif is NOT sym->verif.
        // Build sym --HAS_EVIDENCE--> verif so the chain obs->sym->verif spans
        // agent_memory, codegraph, verification.
        let obs = memory_node("agent_memory:v1:obs");
        let sym = code_symbol("codegraph:v5:sym", "src/lib.rs", "foo");
        let verif = verification_node("verification:v1:verif");
        let e1 = evidence_edge(EdgeLabel::Observes, obs.id(), sym.id());
        let e2 = evidence_edge(EdgeLabel::HasEvidence, sym.id(), verif.id());
        let records = vec![obs, sym, verif, e1, e2];

        let path = evidence_path(&records, "agent_memory:v1:obs", "verification:v1:verif")
            .expect("path should be found");

        assert_eq!(path.hops.len(), 2, "two-hop chain");
        assert_eq!(path.source.domain, "agent_memory");
        assert_eq!(path.target.domain, "verification");

        // Hop 0: obs --OBSERVES--> sym (forward).
        assert_eq!(path.hops[0].edge.label, "OBSERVES");
        assert_eq!(path.hops[0].from.record_id, "agent_memory:v1:obs");
        assert_eq!(path.hops[0].to.record_id, "codegraph:v5:sym");
        assert_eq!(path.hops[0].edge.traversal_direction, "forward");
        assert_eq!(path.hops[0].to.trust_class, "source_fact");
        assert_eq!(
            path.hops[0].to.repo_relative_path.as_deref(),
            Some("src/lib.rs")
        );

        // Hop 1: sym --HAS_EVIDENCE--> verif (forward).
        assert_eq!(path.hops[1].edge.label, "HAS_EVIDENCE");
        assert_eq!(path.hops[1].from.record_id, "codegraph:v5:sym");
        assert_eq!(path.hops[1].to.record_id, "verification:v1:verif");
        assert_eq!(path.hops[1].edge.traversal_direction, "forward");

        // Spans >= 3 distinct domains across the chain.
        let mut domains: BTreeSet<&str> = BTreeSet::new();
        domains.insert(path.source.domain.as_str());
        domains.insert(path.target.domain.as_str());
        for hop in &path.hops {
            domains.insert(hop.from.domain.as_str());
            domains.insert(hop.to.domain.as_str());
        }
        assert!(domains.len() >= 3, "chain spans >= 3 domains: {domains:?}");

        // Byte-identical across repeated runs.
        let first = serde_json::to_string(&path.hops).unwrap();
        for _ in 0..3 {
            let again = evidence_path(&records, "agent_memory:v1:obs", "verification:v1:verif")
                .expect("stable");
            assert_eq!(serde_json::to_string(&again.hops).unwrap(), first);
        }
    }

    // ── reverse-direction traversal ───────────────────────────────────────────

    #[test]
    fn traverses_edge_against_its_native_direction() {
        // Only edge: verif <--VALIDATED_BY-- obs (stored obs -> verif). Ask for
        // the path verif -> obs, which must walk the edge in reverse.
        let obs = memory_node("agent_memory:v1:obs");
        let verif = verification_node("verification:v1:verif");
        let e1 = evidence_edge(EdgeLabel::ValidatedBy, obs.id(), verif.id());
        let records = vec![obs, verif, e1];

        let path = evidence_path(&records, "verification:v1:verif", "agent_memory:v1:obs")
            .expect("reachable undirected");
        assert_eq!(path.hops.len(), 1);
        // Native from/to as stored (obs -> verif); we walked reverse.
        assert_eq!(path.hops[0].from.record_id, "agent_memory:v1:obs");
        assert_eq!(path.hops[0].to.record_id, "verification:v1:verif");
        assert_eq!(path.hops[0].edge.traversal_direction, "reverse");
    }

    // ── 2. no_path ────────────────────────────────────────────────────────────

    #[test]
    fn no_path_between_disconnected_live_endpoints() {
        let obs = memory_node("agent_memory:v1:obs");
        let verif = verification_node("verification:v1:verif");
        let records = vec![obs, verif];
        let err = evidence_path(&records, "agent_memory:v1:obs", "verification:v1:verif")
            .expect_err("no chain");
        match err {
            EvidencePathError::NoPath {
                traversed_edge_classes,
                excluded_edge_classes,
                ..
            } => {
                assert!(traversed_edge_classes.contains(&"OBSERVES".to_owned()));
                assert!(excluded_edge_classes.contains(&"CALLS".to_owned()));
            }
            other => panic!("expected NoPath, got {other:?}"),
        }
    }

    // ── 3. endpoint_not_found ─────────────────────────────────────────────────

    #[test]
    fn absent_endpoint_is_not_found() {
        let obs = memory_node("agent_memory:v1:obs");
        let records = vec![obs];
        let err = evidence_path(&records, "agent_memory:v1:obs", "verification:v1:ghost")
            .expect_err("target absent");
        assert_eq!(
            err,
            EvidencePathError::EndpointNotFound {
                side: EndpointSide::Target,
                handle: "verification:v1:ghost".to_owned(),
            }
        );
    }

    #[test]
    fn source_problem_reported_first_when_both_bad() {
        let records: Vec<GraphRecord> = vec![];
        let err =
            evidence_path(&records, "codegraph:v5:a", "codegraph:v5:b").expect_err("both absent");
        match err {
            EvidencePathError::EndpointNotFound { side, .. } => {
                assert_eq!(side, EndpointSide::Source);
            }
            other => panic!("expected source-side not_found, got {other:?}"),
        }
    }

    // ── 4. endpoint_tombstoned (distinct from not_found) ──────────────────────

    #[test]
    fn tombstoned_endpoint_is_distinct_from_absent() {
        let obs = memory_node("agent_memory:v1:obs");
        let dead = code_symbol("codegraph:v5:dead", "src/lib.rs", "dead");
        let tomb = tombstone("codegraph:v5:dead");
        let records = vec![obs, dead, tomb];
        let err = evidence_path(&records, "agent_memory:v1:obs", "codegraph:v5:dead")
            .expect_err("target tombstoned");
        assert_eq!(
            err,
            EvidencePathError::EndpointTombstoned {
                side: EndpointSide::Target,
                handle: "codegraph:v5:dead".to_owned(),
            }
        );
        // Distinct label from not_found.
        assert!(!matches!(err, EvidencePathError::EndpointNotFound { .. }));
    }

    #[test]
    fn tombstoned_but_temporal_endpoint_is_still_live() {
        // A tombstoned id that also carries a bitemporal `temporal` version is
        // NOT deleted per the liveness gate — it stays reachable.
        let obs = memory_node("agent_memory:v1:obs");
        let sym =
            code_symbol("codegraph:v5:sym", "src/lib.rs", "foo").with_temporal(TemporalMetadata {
                git_commit: "abcdef".to_owned(),
                git_parent_commits: Vec::new(),
                valid_time: "2026-01-01T00:00:00Z".to_owned(),
                author_time: None,
                observed_at: "2026-01-01T00:00:00Z".to_owned(),
                valid_time_source: None,
            });
        let tomb = tombstone("codegraph:v5:sym");
        let e1 = evidence_edge(EdgeLabel::Observes, obs.id(), sym.id());
        let records = vec![obs, sym, tomb, e1];
        let path = evidence_path(&records, "agent_memory:v1:obs", "codegraph:v5:sym")
            .expect("temporal version keeps the node live");
        assert_eq!(path.hops.len(), 1);
    }

    // ── 5. identical_endpoints ────────────────────────────────────────────────

    #[test]
    fn identical_endpoints_rejected() {
        let obs = memory_node("agent_memory:v1:obs");
        let records = vec![obs];
        let err = evidence_path(&records, "agent_memory:v1:obs", "agent_memory:v1:obs")
            .expect_err("identical");
        assert_eq!(
            err,
            EvidencePathError::IdenticalEndpoints {
                handle: "agent_memory:v1:obs".to_owned(),
            }
        );
    }

    // ── 6. determinism / tie-break ────────────────────────────────────────────

    #[test]
    #[allow(clippy::similar_names)]
    fn tie_break_prefers_min_neighbor_edge_tuple() {
        // Two equal-length (2-hop) paths from src to dst through distinct middles
        // m_a and m_b. The documented tie-break (min neighbor_id, then edge_id)
        // must pick the chain through the lexicographically smaller middle.
        let src = memory_node("agent_memory:v1:src");
        let dst = verification_node("verification:v1:dst");
        let m_a = code_symbol("codegraph:v5:aaa", "src/a.rs", "a");
        let m_b = code_symbol("codegraph:v5:bbb", "src/b.rs", "b");
        let e_src_a = evidence_edge(EdgeLabel::Observes, src.id(), m_a.id());
        let e_src_b = evidence_edge(EdgeLabel::Observes, src.id(), m_b.id());
        let e_a_dst = evidence_edge(EdgeLabel::HasEvidence, m_a.id(), dst.id());
        let e_b_dst = evidence_edge(EdgeLabel::HasEvidence, m_b.id(), dst.id());
        let records = vec![src, dst, m_a, m_b, e_src_a, e_src_b, e_a_dst, e_b_dst];

        let path = evidence_path(&records, "agent_memory:v1:src", "verification:v1:dst")
            .expect("reachable");
        assert_eq!(path.hops.len(), 2);
        // The middle node must be the lexicographically smaller "aaa".
        assert_eq!(path.hops[0].to.record_id, "codegraph:v5:aaa");
        assert_eq!(path.hops[1].from.record_id, "codegraph:v5:aaa");
    }

    // ── 7. EMITTED_DURING basis ───────────────────────────────────────────────

    #[test]
    fn emitted_during_hop_carries_basis_and_confidence() {
        for (basis, wire, confidence) in [
            (
                CorrelationBasis::ContentHashJoin,
                "content_hash_join",
                "1.0",
            ),
            (
                CorrelationBasis::TemporalCorrelation,
                "temporal_correlation",
                "0.5",
            ),
        ] {
            let sig = GraphRecord::node(
                "log:v2:sig".to_owned(),
                NodeKind::ErrorSignature,
                None,
                None,
                None,
                "sig".to_owned(),
            )
            .with_domain("log", 2);
            let run = GraphRecord::node(
                "verification:v1:run".to_owned(),
                NodeKind::CommandRun,
                None,
                None,
                None,
                "run".to_owned(),
            )
            .with_domain("verification", 1);
            let edge =
                evidence_edge(EdgeLabel::EmittedDuring, sig.id(), run.id()).with_basis(basis);
            let records = vec![sig, run, edge];
            let path =
                evidence_path(&records, "log:v2:sig", "verification:v1:run").expect("reachable");
            assert_eq!(path.hops.len(), 1);
            assert_eq!(path.hops[0].edge.label, "EMITTED_DURING");
            assert_eq!(path.hops[0].edge.basis.as_deref(), Some(wire));
            assert_eq!(path.hops[0].edge.confidence.as_deref(), Some(confidence));
        }
    }

    #[test]
    fn non_emitted_during_hop_omits_basis() {
        let obs = memory_node("agent_memory:v1:obs");
        let sym = code_symbol("codegraph:v5:sym", "src/lib.rs", "foo");
        let e1 = evidence_edge(EdgeLabel::Observes, obs.id(), sym.id());
        let records = vec![obs, sym, e1];
        let path =
            evidence_path(&records, "agent_memory:v1:obs", "codegraph:v5:sym").expect("reachable");
        assert!(path.hops[0].edge.basis.is_none());
        assert!(path.hops[0].edge.confidence.is_none());
    }

    // ── 8. excluded edges not traversed ───────────────────────────────────────

    #[test]
    fn code_topology_edge_is_not_traversed() {
        // Two symbols connected ONLY by a CALLS edge (excluded class) → no_path.
        let a = code_symbol("codegraph:v5:a", "src/a.rs", "a");
        let b = code_symbol("codegraph:v5:b", "src/b.rs", "b");
        let calls = evidence_edge(EdgeLabel::Calls, a.id(), b.id());
        let records = vec![a, b, calls];
        let err = evidence_path(&records, "codegraph:v5:a", "codegraph:v5:b")
            .expect_err("CALLS excluded");
        assert!(matches!(err, EvidencePathError::NoPath { .. }));
    }

    // ── 9. multi-repo path ────────────────────────────────────────────────────

    #[test]
    fn path_crosses_repository_boundaries_with_per_node_paths() {
        // A chain whose code nodes belong to two different repositories; each row
        // must carry its own repo-relative path.
        let sym_a = code_symbol("codegraph:v5:repoA_sym", "crates/a/src/lib.rs", "a");
        let obs = memory_node("agent_memory:v1:obs");
        let sym_b = code_symbol("codegraph:v5:repoB_sym", "crates/b/src/lib.rs", "b");
        let e1 = evidence_edge(EdgeLabel::Observes, obs.id(), sym_a.id());
        let e2 = evidence_edge(EdgeLabel::MentionsSymbol, obs.id(), sym_b.id());
        let records = vec![sym_a, obs, sym_b, e1, e2];
        let path = evidence_path(&records, "codegraph:v5:repoA_sym", "codegraph:v5:repoB_sym")
            .expect("reachable through the shared observation");
        // repoA_sym -> obs -> repoB_sym.
        assert_eq!(path.hops.len(), 2);
        assert_eq!(
            path.source.repo_relative_path.as_deref(),
            Some("crates/a/src/lib.rs")
        );
        assert_eq!(
            path.target.repo_relative_path.as_deref(),
            Some("crates/b/src/lib.rs")
        );
    }

    // ── 10. completeness-invariant partition test ─────────────────────────────

    #[test]
    fn edge_label_partition_is_total_and_correct() {
        let (traversed, excluded) = edge_classes();
        // (a) partition is total: every variant lands in exactly one class.
        assert_eq!(
            traversed.len() + excluded.len(),
            ALL_EDGE_LABELS.len(),
            "traversed + excluded must cover every EdgeLabel variant exactly once"
        );
        assert_eq!(ALL_EDGE_LABELS.len(), 47, "EdgeLabel has 47 variants");
        // No overlap between the two class lists.
        for label in &traversed {
            assert!(!excluded.contains(label), "{label} in both classes");
        }

        // (b) spot-check known members of each class.
        assert!(traversed.contains(&"OBSERVES".to_owned()));
        assert!(traversed.contains(&"HAS_EVIDENCE".to_owned()));
        assert!(traversed.contains(&"EMITTED_DURING".to_owned()));
        assert!(excluded.contains(&"CALLS".to_owned()));
        assert!(excluded.contains(&"CONTAINS".to_owned()));
        assert!(excluded.contains(&"DEFINES".to_owned()));
        assert!(excluded.contains(&"AUTHORED_BY".to_owned()));

        // (c) the AC2-named example labels are all traversed.
        for label in [
            "OBSERVES",
            "VALIDATED_BY",
            "HAS_EVIDENCE",
            "PRODUCED_EVIDENCE",
            "CLOSES_ACCEPTANCE_CRITERION",
            "OWNED_BY_TASK",
            "FAILED_ON",
            "EXPLAINS_CHANGE",
            "REFERENCES_TASK",
            "MATERIALIZED_AS",
        ] {
            assert!(
                traversed.contains(&label.to_owned()),
                "AC2 example {label} must be traversed"
            );
        }
    }

    // ── 12. redaction: only bounded fields ever escape ────────────────────────

    #[test]
    fn output_carries_no_free_text() {
        // A memory node with body text and an edge summary must NOT leak either
        // into the serialized path (rows carry no text/summary field).
        let mut obs = memory_node("agent_memory:v1:obs");
        if let GraphRecord::Node { text, .. } = &mut obs {
            *text = Some("SECRET customer name leaked in observation body".to_owned());
        }
        let sym = code_symbol("codegraph:v5:sym", "src/lib.rs", "foo");
        let e1 = evidence_edge(EdgeLabel::Observes, obs.id(), sym.id());
        let records = vec![obs, sym, e1];
        let path =
            evidence_path(&records, "agent_memory:v1:obs", "codegraph:v5:sym").expect("reachable");
        let json = serde_json::to_string(&path.hops).unwrap();
        let summary_json = serde_json::to_string(&path.source).unwrap();
        assert!(!json.contains("SECRET"), "no body text may leak: {json}");
        assert!(
            !json.contains("OBSERVES edge"),
            "no edge summary may leak: {json}"
        );
        assert!(
            !summary_json.contains("SECRET"),
            "no body text in row: {summary_json}"
        );
    }
}
