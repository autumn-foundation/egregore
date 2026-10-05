//! Retrace one agent session's footprint, claims, and verification.
//!
//! Issue #259: `eg query session <id-or-handle>` resolves a live
//! [`AgentSession`](crate::ir::NodeKind::AgentSession) by record ID or imported
//! session handle and returns the session's deterministic footprint (files and
//! symbols reached through `TOUCHED_FILE` / `MENTIONS_SYMBOL` edges), the
//! agent-authored claims linked to it (observations, failures), the artifacts it
//! produced, and the verification evidence linked to it — with a top-level
//! `verification_status` derived **only** from verification-domain evidence.
//!
//! Trust separation is structural:
//!
//! * `touched` carries only deterministic code-graph facts (`File` / `Symbol`
//!   nodes). Agent prose is never admitted here.
//! * `verification_status` is derived only from linked verification records via
//!   the shared [`verification_outcome`](super::trust::verification_outcome)
//!   rule. An agent's claim that "all checks pass" cannot promote the status.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::liveness::Liveness;
use super::trust::{TrustClass, TrustIndex, VerificationOutcome, verification_outcome};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// How far membership edges are followed from the resolved session when
/// gathering linked records. Three hops cover session -> claim -> evidence ->
/// verification without wandering the graph.
const MEMBERSHIP_HOPS: usize = 3;

/// Error resolving the session argument to live session record IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionResolveError {
    /// No live `AgentSession` matches the id or handle.
    NoMatch,
    /// The handle matches more than one live session; the caller must
    /// disambiguate with a record ID.
    Ambiguous,
}

impl std::fmt::Display for SessionResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMatch => write!(f, "no live agent session matches"),
            Self::Ambiguous => write!(f, "handle matches more than one live session"),
        }
    }
}

impl std::error::Error for SessionResolveError {}

/// One deterministic footprint row: a `File` or `Symbol` reached through a
/// `TOUCHED_FILE` / `MENTIONS_SYMBOL` edge from a session member.
#[derive(Debug, Clone)]
pub struct SessionTouchedRow<'a> {
    /// The touched code-graph record.
    pub record: &'a GraphRecord,
    /// The trust class of the touched record (always `source_derived`;
    /// carried explicitly so the wire format never implies otherwise).
    pub trust: TrustClass,
    /// Live record IDs that cite this footprint row (the session-side
    /// endpoints of the touch edges), in deterministic order.
    pub cited_by: Vec<String>,
}

/// One agent-authored claim row (observation or failure) with its citations.
#[derive(Debug, Clone)]
pub struct SessionClaimRow<'a> {
    /// The claim record.
    pub record: &'a GraphRecord,
    /// The claim's trust class (`agent_*`; never influences `touched` or
    /// `verification_status`).
    pub trust: TrustClass,
    /// Citation handles for the claim: live record IDs of the cited records,
    /// falling back to the claim's own record ID when it cites nothing.
    pub citations: Vec<String>,
}

/// One produced-artifact row.
#[derive(Debug, Clone)]
pub struct SessionArtifactRow<'a> {
    /// The artifact record.
    pub record: &'a GraphRecord,
    /// The artifact's trust class.
    pub trust: TrustClass,
    /// Live record IDs this artifact cites (deterministic order).
    pub citations: Vec<String>,
}

/// One verification-evidence row.
#[derive(Debug, Clone)]
pub struct SessionVerificationRow<'a> {
    /// The verification record.
    pub record: &'a GraphRecord,
    /// The record's trust class (always `verification_evidence`).
    pub trust: TrustClass,
    /// Live record IDs this verification record cites (deterministic order).
    pub citations: Vec<String>,
    /// The shared pass/fail/inconclusive outcome for this record.
    pub outcome: VerificationOutcome,
}

/// The top-level verification status, derived only from linked
/// verification-domain evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionVerificationStatus {
    /// At least one linked verification record reports a passing outcome and
    /// none reports a failing one.
    Verified,
    /// At least one linked verification record reports a failing outcome.
    Failed,
    /// No linked verification-domain evidence.
    Unverified,
}

impl std::fmt::Display for SessionVerificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Verified => write!(f, "verified"),
            Self::Failed => write!(f, "failed"),
            Self::Unverified => write!(f, "unverified"),
        }
    }
}

/// The full retrospective for one resolved session.
#[derive(Debug, Clone)]
pub struct SessionRetrospective<'a> {
    /// The resolved live session record ID.
    pub session_id: String,
    /// Deterministic footprint: files/symbols touched via touch edges.
    pub touched: Vec<SessionTouchedRow<'a>>,
    /// Agent-authored observations linked to the session.
    pub observations: Vec<SessionClaimRow<'a>>,
    /// Agent-authored failure claims linked to the session.
    pub failures: Vec<SessionClaimRow<'a>>,
    /// Produced artifacts linked to the session.
    pub artifacts: Vec<SessionArtifactRow<'a>>,
    /// Verification-domain evidence linked to the session.
    pub verification_evidence: Vec<SessionVerificationRow<'a>>,
    /// Derived only from `verification_evidence`.
    pub verification_status: SessionVerificationStatus,
}

/// A record counts as verification evidence only when it is BOTH a
/// verification-domain node kind and carries a verification-domain record ID:
/// the embedded ingest path does not run the daemon's validator, so kind or ID
/// alone is not sufficient.
fn is_verification_evidence(record: &GraphRecord) -> bool {
    super::trust::is_verification_domain_record(record)
        && super::trust::is_verification_domain_kind(record)
}

/// Adjacency over live edges: source node id -> live out-edges, and target
/// node id -> live in-edges. Membership edges (`SESSION_OF`, `AUTHORED_BY`,
/// ...) point from the member TOWARD the session (e.g. `AgentRun
/// -SESSION_OF-> AgentSession`), so membership is discovered by following
/// in-edges backward from the session; touch edges (`TOUCHED_FILE`,
/// `MENTIONS_SYMBOL`) point from the member outward to the code-graph fact.
struct Adjacency<'a> {
    out_edges: BTreeMap<&'a str, Vec<&'a GraphRecord>>,
    in_edges: BTreeMap<&'a str, Vec<&'a GraphRecord>>,
}

impl<'a> Adjacency<'a> {
    fn build(records: &'a [GraphRecord], liveness: &Liveness<'a>) -> Self {
        let mut out_edges: BTreeMap<&'a str, Vec<&'a GraphRecord>> = BTreeMap::new();
        let mut in_edges: BTreeMap<&'a str, Vec<&'a GraphRecord>> = BTreeMap::new();
        for (index, record) in records.iter().enumerate() {
            let GraphRecord::Edge {
                id, source, target, ..
            } = record
            else {
                continue;
            };
            if liveness.deleted(id) || !liveness.is_latest_edge_version(id, index) {
                continue;
            }
            if liveness.deleted(target) {
                continue;
            }
            out_edges.entry(source.as_str()).or_default().push(record);
            in_edges.entry(target.as_str()).or_default().push(record);
        }
        Self {
            out_edges,
            in_edges,
        }
    }

    fn out(&self, node_id: &str) -> &[&'a GraphRecord] {
        self.out_edges.get(node_id).map_or(&[], Vec::as_slice)
    }

    fn incoming(&self, node_id: &str) -> &[&'a GraphRecord] {
        self.in_edges.get(node_id).map_or(&[], Vec::as_slice)
    }
}

/// Index live nodes by id (latest version of each wins).
fn live_nodes<'a>(
    records: &'a [GraphRecord],
    liveness: &Liveness<'a>,
) -> BTreeMap<&'a str, (usize, &'a GraphRecord)> {
    let mut nodes: BTreeMap<&'a str, (usize, &'a GraphRecord)> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if !matches!(record, GraphRecord::Node { .. }) {
            continue;
        }
        let id = record.id();
        if liveness.deleted(id) || !liveness.is_latest_node_version(id, index) {
            continue;
        }
        nodes.insert(id, (index, record));
    }
    nodes
}

/// Resolve the CLI argument to the live session record IDs it names.
///
/// Resolution order: exact live record ID first, then exact `source_handle`
/// match. A handle naming several live sessions is
/// [`SessionResolveError::Ambiguous`]; a handle naming none is
/// [`SessionResolveError::NoMatch`].
///
/// # Errors
///
/// Returns [`SessionResolveError::NoMatch`] when no live `AgentSession`
/// matches the id or handle, or [`SessionResolveError::Ambiguous`] when the
/// handle names more than one live session.
pub fn resolve_session_ids(
    records: &[GraphRecord],
    id_or_handle: &str,
) -> Result<Vec<String>, SessionResolveError> {
    let liveness = Liveness::new(records);
    let nodes = live_nodes(records, &liveness);

    let mut by_id: BTreeSet<String> = BTreeSet::new();
    let mut by_handle: BTreeSet<String> = BTreeSet::new();
    for (id, (_, record)) in &nodes {
        let GraphRecord::Node {
            kind,
            source_handle,
            ..
        } = record
        else {
            continue;
        };
        if *kind != NodeKind::AgentSession {
            continue;
        }
        if *id == id_or_handle {
            by_id.insert((*id).to_owned());
        }
        // The imported session handle is the node's `source_handle` field
        // (set at transcript import); the record ID is matched separately
        // above, so a handle never has to equal a record ID.
        let handle_matches = source_handle.as_deref().is_some_and(|h| h == id_or_handle);
        if handle_matches {
            by_handle.insert((*id).to_owned());
        }
    }

    if !by_id.is_empty() {
        return Ok(by_id.into_iter().collect());
    }
    match by_handle.len() {
        0 => Err(SessionResolveError::NoMatch),
        1 => Ok(by_handle.into_iter().collect()),
        _ => Err(SessionResolveError::Ambiguous),
    }
}

/// Edge labels that confer session membership when traversed outward from a
/// session member.
const fn is_membership_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::SessionOf
            | EdgeLabel::AuthoredBy
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ValidatedBy
    )
}

/// Edge labels through which a session member produces an artifact. These
/// point member -> artifact (e.g. `AgentRun -PRODUCED_PATCH-> PatchArtifact`),
/// so membership discovery follows them *forward*, unlike the membership
/// labels above. The artifacts section must show produced artifacts even for
/// minimal importer graphs that only mint the produce edge, not a redundant
/// `AUTHORED_BY` back-link.
const fn is_produced_artifact_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::ProducedPatch | EdgeLabel::ProducedEvidence
    )
}

/// Edge labels through which a session member touches a code-graph fact.
const fn is_touch_label(label: EdgeLabel) -> bool {
    matches!(label, EdgeLabel::TouchedFile | EdgeLabel::MentionsSymbol)
}

const fn node_kind(record: &GraphRecord) -> Option<NodeKind> {
    match record {
        GraphRecord::Node { kind, .. } => Some(*kind),
        GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => None,
    }
}

/// Live citation handles for `record`: `evidence_links` targets plus the
/// targets of non-membership outgoing edges (e.g. `FAILED_ON`), falling back
/// to the record's own ID when it cites nothing live, so every row carries at
/// least one citation handle.
fn cited_live_ids<'a>(
    record: &'a GraphRecord,
    nodes: &BTreeMap<&'a str, (usize, &'a GraphRecord)>,
    adjacency: &Adjacency<'a>,
) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    // Structured evidence links.
    if let Some(links) = record.evidence_links() {
        for link in links {
            if let Some(target) = link.target_record_id.as_deref() {
                if let Some((_, target_record)) = nodes.get(target) {
                    out.insert(citation_handle(target, target_record));
                }
            }
        }
    }
    // Outgoing edges that are not membership structure (e.g. FAILED_ON,
    // REFUTES): the claim cites its target.
    for edge in adjacency.out(record.id()) {
        let GraphRecord::Edge { label, target, .. } = edge else {
            continue;
        };
        if is_membership_label(*label) || is_touch_label(*label) {
            continue;
        }
        if let Some((_, target_record)) = nodes.get(target.as_str()) {
            out.insert(citation_handle(target.as_str(), target_record));
        }
    }
    if out.is_empty() {
        out.insert(record.id().to_owned());
    }
    out.into_iter().collect()
}

/// Prefer a human-meaningful handle for a cited record: the file path for
/// File targets, `path#name` for named Symbol targets (so a `FAILED_ON`
/// citation names the code it failed on), else the record ID. The citation
/// must resolve, so the record ID is the fallback, never a fabricated handle.
fn citation_handle(target: &str, target_record: &GraphRecord) -> String {
    match target_record {
        GraphRecord::Node {
            kind: NodeKind::Symbol,
            repo_relative_path: Some(path),
            name: Some(name),
            ..
        } => format!("{path}#{name}"),
        GraphRecord::Node {
            kind: NodeKind::File | NodeKind::Symbol,
            repo_relative_path: Some(path),
            ..
        } => path.clone(),
        _ => target.to_owned(),
    }
}

/// Compute the retrospective for already-resolved live session IDs.
#[must_use]
pub fn retrace_session<'a>(
    records: &'a [GraphRecord],
    session_ids: &'a [String],
) -> SessionRetrospective<'a> {
    let liveness = Liveness::new(records);
    let nodes = live_nodes(records, &liveness);
    let adjacency = Adjacency::build(records, &liveness);
    let trust = TrustIndex::build(records);

    // ── membership: BFS over membership edges, max MEMBERSHIP_HOPS ──────────
    // Membership edges point from the member toward the session, so the BFS
    // follows in-edges backward: session <- run <- turn <- claim. Produce
    // edges (`PRODUCED_PATCH` / `PRODUCED_EVIDENCE`) point member -> artifact,
    // so the BFS additionally follows those forward: member -> artifact.
    let mut members: BTreeSet<&'a str> = BTreeSet::new();
    let mut frontier: Vec<&'a str> = Vec::new();
    for id in session_ids {
        if nodes.contains_key(id.as_str()) {
            members.insert(id.as_str());
            frontier.push(id.as_str());
        }
    }
    for _ in 0..MEMBERSHIP_HOPS {
        let mut next: Vec<&'a str> = Vec::new();
        for node_id in std::mem::take(&mut frontier) {
            // Backward: sources of membership in-edges (member points at the
            // session / its ancestors).
            for edge in adjacency.incoming(node_id) {
                let GraphRecord::Edge { label, source, .. } = edge else {
                    continue;
                };
                if !is_membership_label(*label) {
                    continue;
                }
                if nodes.contains_key(source.as_str()) && members.insert(source.as_str()) {
                    next.push(source.as_str());
                }
            }
            // Forward: targets of produce out-edges (member produced the
            // artifact); see [`is_produced_artifact_label`].
            for edge in adjacency.out(node_id) {
                let GraphRecord::Edge { label, target, .. } = edge else {
                    continue;
                };
                if !is_produced_artifact_label(*label) {
                    continue;
                }
                if nodes.contains_key(target.as_str()) && members.insert(target.as_str()) {
                    next.push(target.as_str());
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    // ── verification records: stamped, or linked via evidence edges ────────
    // Verification writes carry session provenance (`session_id`); they belong
    // to the retrospective even when no membership edge links them. In
    // addition, PRODUCED_EVIDENCE / VALIDATED_BY edges from members to
    // verification records are followed: the evidence section must show the
    // linked verification, not merely derive a status from it.
    let mut verification_ids: BTreeSet<&'a str> = BTreeSet::new();
    for member_id in &members {
        let Some((_, record)) = nodes.get(member_id) else {
            continue;
        };
        if is_verification_evidence(record) {
            verification_ids.insert(member_id);
        }
        // Follow evidence edges outward from members.
        for edge in adjacency.out(member_id) {
            let GraphRecord::Edge { label, target, .. } = edge else {
                continue;
            };
            if !matches!(label, EdgeLabel::ProducedEvidence | EdgeLabel::ValidatedBy) {
                continue;
            }
            if let Some((_, target_record)) = nodes.get(target.as_str()) {
                if is_verification_evidence(target_record) {
                    verification_ids.insert(target.as_str());
                }
            }
        }
    }
    for (id, (_, record)) in &nodes {
        if !is_verification_evidence(record) {
            continue;
        }
        let stamped = match record {
            GraphRecord::Node { session_id, .. } => session_id
                .as_deref()
                .is_some_and(|s| session_ids.iter().any(|wanted| wanted == s)),
            GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => false,
        };
        if stamped {
            verification_ids.insert(id);
        }
    }

    // ── touched: deterministic footprint only ──────────────────────────────
    // Only File/Symbol nodes reached through TOUCHED_FILE / MENTIONS_SYMBOL
    // from a session member. Agent claims never enter this section: the kind
    // gate below admits code-graph facts exclusively.
    let mut touched: BTreeMap<&'a str, BTreeSet<String>> = BTreeMap::new();
    for member_id in &members {
        for edge in adjacency.out(member_id) {
            let GraphRecord::Edge { label, target, .. } = edge else {
                continue;
            };
            if !is_touch_label(*label) {
                continue;
            }
            let Some((_, target_record)) = nodes.get(target.as_str()) else {
                continue;
            };
            if let Some(NodeKind::File | NodeKind::Symbol) = node_kind(target_record) {
                touched
                    .entry(target.as_str())
                    .or_default()
                    .insert((*member_id).to_owned());
            }
        }
    }
    let mut touched_rows: Vec<SessionTouchedRow<'_>> = Vec::new();
    for (id, cited_by) in touched {
        if let Some((_, record)) = nodes.get(id) {
            touched_rows.push(SessionTouchedRow {
                record,
                trust: trust.classify(record),
                cited_by: cited_by.into_iter().collect(),
            });
        }
    }

    // ── claims, artifacts: classify members ────────────────────────────────
    let mut observations: Vec<SessionClaimRow<'_>> = Vec::new();
    let mut failures: Vec<SessionClaimRow<'_>> = Vec::new();
    let mut artifacts: Vec<SessionArtifactRow<'_>> = Vec::new();
    for member_id in &members {
        let Some((_, record)) = nodes.get(member_id) else {
            continue;
        };
        if is_verification_evidence(record) {
            continue;
        }
        let row_trust = trust.classify(record);
        let citations = cited_live_ids(record, &nodes, &adjacency);
        match node_kind(record) {
            Some(NodeKind::Observation) => observations.push(SessionClaimRow {
                record,
                trust: row_trust,
                citations,
            }),
            Some(NodeKind::Failure) => failures.push(SessionClaimRow {
                record,
                trust: row_trust,
                citations,
            }),
            Some(NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit) => {
                artifacts.push(SessionArtifactRow {
                    record,
                    trust: row_trust,
                    citations,
                });
            }
            _ => {}
        }
    }

    // ── verification_evidence rows + status (evidence only) ─────────────────
    // The status is derived ONLY from these rows. Agent prose — including a
    // claim that "all checks pass" — never participates.
    let mut verification_evidence: Vec<SessionVerificationRow<'_>> = Vec::new();
    let mut status = SessionVerificationStatus::Unverified;
    for id in verification_ids {
        let Some((_, record)) = nodes.get(id) else {
            continue;
        };
        let outcome = verification_outcome(record);
        match outcome {
            VerificationOutcome::Failing => status = SessionVerificationStatus::Failed,
            VerificationOutcome::Passing => {
                if status == SessionVerificationStatus::Unverified {
                    status = SessionVerificationStatus::Verified;
                }
            }
            VerificationOutcome::Inconclusive => {}
        }
        verification_evidence.push(SessionVerificationRow {
            record,
            trust: trust.classify(record),
            citations: cited_live_ids(record, &nodes, &adjacency),
            outcome,
        });
    }

    // Deterministic ordering for every section.
    touched_rows.sort_by(|a, b| a.record.id().cmp(b.record.id()));
    observations.sort_by(|a, b| a.record.id().cmp(b.record.id()));
    failures.sort_by(|a, b| a.record.id().cmp(b.record.id()));
    artifacts.sort_by(|a, b| a.record.id().cmp(b.record.id()));
    verification_evidence.sort_by(|a, b| a.record.id().cmp(b.record.id()));

    let session_id = session_ids.first().map_or_else(String::new, Clone::clone);
    SessionRetrospective {
        session_id,
        touched: touched_rows,
        observations,
        failures,
        artifacts,
        verification_evidence,
        verification_status: status,
    }
}
