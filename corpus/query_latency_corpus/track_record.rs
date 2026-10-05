//! Per-agent track record over the downstream fate of agent-authored observations.
//!
//! Lane: `eg query track-record` (alias `eg query agents`).
//!
//! This lane ranks agents by what happened *after* they wrote an
//! observation: operator promotion verdicts (approved / edited-then-approved
//! / rejected / deferred / expired), supersession, and the outcome of
//! verification work attributable to their sessions. It is a **read-only,
//! local** query: no network, no embedding store, no mutation.
//!
//! ## Trust separation
//!
//! Three kinds of claims flow through this lane and they must not be
//! conflated; each output section names its own trust basis:
//!
//! 1. **Agent claims** (`agent_authored_claims`) — what an agent stamped on
//!    its own records (`agent_id`, `agent_kind`). Self-reported and untrusted.
//! 2. **Operator decisions** (`recorded_operator_decisions`) — promotion
//!    verdicts (`PromotionDecision`). Human/operator judgments that outrank
//!    agent claims.
//! 3. **Verification evidence** (`verification_evidence`) — verification
//!    records linked to the agent's sessions. Outcomes are classified with
//!    the shared [`crate::query::trust::verification_outcome`]: `passed`,
//!    `failed`, and `inconclusive` are reported as separate buckets so
//!    inconclusive evidence is never silently fail-closed into a pass.
//!
//! Every nonzero bucket enumerates the record IDs behind it so an operator
//! can audit the attribution. Empty stores and agents with no qualifying
//! records produce explicit empty verdicts rather than silence.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::ir::{EdgeLabel, GraphRecord, NodeKind};
use crate::query::RepositoryIndex;
use crate::query::trust::{VerificationOutcome, verification_outcome};

/// Disclaimer attached to every response from this lane.
pub const TRACK_RECORD_DISCLAIMER: &str = "Attribution is by stamped agent_id on live records (last write wins). \
Agent claims, operator promotion decisions, and verification evidence are reported separately and must not be \
conflated: an operator rejection is not an agent failure, and a verification pass is evidence about the artifact, \
not a certification of the agent.";

/// Lane identifier used in JSON envelopes and text headers.
pub const TRACK_RECORD_LANE: &str = "track-record";

/// Trust basis for agent-stamped claims.
const BASIS_AGENT_CLAIMS: &str = "agent_authored_claims";
/// Trust basis for operator promotion decisions.
const BASIS_OPERATOR_DECISIONS: &str = "recorded_operator_decisions";
/// Trust basis for verification evidence.
const BASIS_VERIFICATION: &str = "verification_evidence";

/// Serializable count with the citable record IDs behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitableCount {
    /// Number of records in this bucket.
    pub count: usize,
    /// Canonical record IDs backing the count, sorted ascending.
    pub record_ids: Vec<String>,
    /// Trust basis for this bucket (see module docs).
    pub trust_basis: String,
}

impl CitableCount {
    fn from_ids(ids: &BTreeSet<String>, trust_basis: &'static str) -> Self {
        Self {
            count: ids.len(),
            record_ids: ids.iter().cloned().collect(),
            trust_basis: trust_basis.to_owned(),
        }
    }

    /// An empty count bucket with the given trust basis.
    #[must_use]
    pub fn empty(trust_basis: &'static str) -> Self {
        Self {
            count: 0,
            record_ids: Vec::new(),
            trust_basis: trust_basis.to_owned(),
        }
    }
}

/// Promotion verdict bucket: cites the terminal candidate(s) and the
/// decision record(s) behind the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionVerdictBucket {
    /// Number of candidates with this terminal verdict.
    pub count: usize,
    /// Candidate record IDs, sorted ascending.
    pub candidate_ids: Vec<String>,
    /// Decision record IDs behind the terminal verdicts, sorted ascending.
    pub decision_ids: Vec<String>,
}

impl PromotionVerdictBucket {
    fn from_sets(candidates: &BTreeSet<String>, decisions: &BTreeSet<String>) -> Self {
        Self {
            count: candidates.len(),
            candidate_ids: candidates.iter().cloned().collect(),
            decision_ids: decisions.iter().cloned().collect(),
        }
    }
}

/// Promotion verdicts for one agent, with the section trust basis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionOutcomes {
    /// Trust basis for this section (see module docs).
    pub trust_basis: String,
    /// Candidates whose terminal verdict is `approved`.
    pub approved: PromotionVerdictBucket,
    /// Candidates whose terminal verdict is `edited_then_approved`.
    pub edited_then_approved: PromotionVerdictBucket,
    /// Candidates whose terminal verdict is `rejected`.
    pub rejected: PromotionVerdictBucket,
    /// Candidates whose terminal verdict is `deferred`.
    pub deferred: PromotionVerdictBucket,
    /// Candidates whose terminal verdict is `expired`.
    pub expired: PromotionVerdictBucket,
}

/// Verification outcomes for one agent, with the section trust basis.
/// `inconclusive` is its own bucket: never merged into `failed`, never
/// treated as a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationOutcomes {
    /// Trust basis for this section (see module docs).
    pub trust_basis: String,
    /// Verifications with a passing outcome.
    pub passed: CitableCount,
    /// Verifications with a failing outcome.
    pub failed: CitableCount,
    /// Verifications with an inconclusive outcome (fail-closed evidence).
    pub inconclusive: CitableCount,
}

/// Per-agent track record row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTrackRecord {
    /// Canonical agent id (stamped on the agent's records).
    pub agent_id: String,
    /// Agent kind stamped alongside the id (first sorted kind; empty when
    /// no kind was ever stamped).
    pub agent_kind: String,
    /// Observations ever written carrying this agent's stamp.
    pub observations_written: CitableCount,
    /// Terminal promotion verdicts over candidates this agent's
    /// observations supported (deduplicated per candidate).
    pub promotion_outcomes: PromotionOutcomes,
    /// Observations later superseded by another record.
    pub superseded_observations: CitableCount,
    /// Session-attributable verification outcomes.
    pub verification_outcomes: VerificationOutcomes,
}

/// Diagnostic for records that could not be attributed cleanly, or for
/// explicit empty verdicts.
///
/// Diagnostics are emitted alongside — never instead of — the per-agent
/// rows, so silent drops are impossible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRecordDiagnostic {
    /// Machine-readable code: `observations_without_agent_id`,
    /// `candidates_without_terminal_decision`, `verifications_without_session`,
    /// `no_agents`, ...
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// Number of records involved.
    pub count: usize,
    /// Record IDs involved, sorted ascending.
    pub record_ids: Vec<String>,
}

/// Full lane result: per-agent rows plus diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Full track-record report.
pub struct TrackRecordReport {
    /// Per-agent rows, sorted canonically by `agent_id`.
    pub agents: Vec<AgentTrackRecord>,
    /// Diagnostics for records that could not be attributed.
    pub diagnostics: Vec<TrackRecordDiagnostic>,
}

/// Per-agent accumulator while scanning the live view.
#[derive(Debug, Default)]
struct AgentAccum {
    kinds: BTreeSet<String>,
    observations_written: BTreeSet<String>,
    approved_candidates: BTreeSet<String>,
    approved_decisions: BTreeSet<String>,
    edited_candidates: BTreeSet<String>,
    edited_decisions: BTreeSet<String>,
    rejected_candidates: BTreeSet<String>,
    rejected_decisions: BTreeSet<String>,
    deferred_candidates: BTreeSet<String>,
    deferred_decisions: BTreeSet<String>,
    expired_candidates: BTreeSet<String>,
    expired_decisions: BTreeSet<String>,
    superseded: BTreeSet<String>,
    verifications_passed: BTreeSet<String>,
    verifications_failed: BTreeSet<String>,
    verifications_inconclusive: BTreeSet<String>,
}

impl AgentAccum {
    fn into_record(self, agent_id: String) -> AgentTrackRecord {
        let kind = self.kinds.iter().next().cloned().unwrap_or_default();
        AgentTrackRecord {
            agent_id,
            agent_kind: kind,
            observations_written: CitableCount::from_ids(
                &self.observations_written,
                BASIS_AGENT_CLAIMS,
            ),
            promotion_outcomes: PromotionOutcomes {
                trust_basis: BASIS_OPERATOR_DECISIONS.to_owned(),
                approved: PromotionVerdictBucket::from_sets(
                    &self.approved_candidates,
                    &self.approved_decisions,
                ),
                edited_then_approved: PromotionVerdictBucket::from_sets(
                    &self.edited_candidates,
                    &self.edited_decisions,
                ),
                rejected: PromotionVerdictBucket::from_sets(
                    &self.rejected_candidates,
                    &self.rejected_decisions,
                ),
                deferred: PromotionVerdictBucket::from_sets(
                    &self.deferred_candidates,
                    &self.deferred_decisions,
                ),
                expired: PromotionVerdictBucket::from_sets(
                    &self.expired_candidates,
                    &self.expired_decisions,
                ),
            },
            superseded_observations: CitableCount::from_ids(&self.superseded, BASIS_AGENT_CLAIMS),
            verification_outcomes: VerificationOutcomes {
                trust_basis: BASIS_VERIFICATION.to_owned(),
                passed: CitableCount::from_ids(&self.verifications_passed, BASIS_VERIFICATION),
                failed: CitableCount::from_ids(&self.verifications_failed, BASIS_VERIFICATION),
                inconclusive: CitableCount::from_ids(
                    &self.verifications_inconclusive,
                    BASIS_VERIFICATION,
                ),
            },
        }
    }
}

/// Edge relations that make an agent-domain record cite a repository's code.
/// Mirrors `sessions.rs`'s code-citation relations: a citation is an explicit
/// agent-authored link (mentions / touched / observed / failed-on), never
/// containment topology.
const CITATION_RELATIONS: &[EdgeLabel] = &[
    EdgeLabel::MentionsSymbol,
    EdgeLabel::TouchedFile,
    EdgeLabel::Observes,
    EdgeLabel::FailedOn,
];

/// Compute the record IDs in scope for `--repo <want>`.
///
/// Code-domain records are in scope by containment
/// ([`RepositoryIndex::owner_of`]). Agent-domain records — agents,
/// observations, candidates, decisions, sessions, runs, verifications — are
/// NOT in the containment walk, so they are scoped by citation instead: an
/// agent is in scope iff any of its observations, candidates, or decisions
/// cites (via [`CITATION_RELATIONS`]) a record the repository owns, and
/// every record attributable to an in-scope agent participates.
///
/// Attribution here is deliberately *unscoped*: scope decisions must not
/// depend on scope decisions, so the citation walk runs over the full live
/// view. Edges are in scope exactly when both endpoints are.
fn scoped_record_ids(
    nodes: &HashMap<&str, &GraphRecord>,
    edges: &HashMap<&str, &GraphRecord>,
    repo_index: &RepositoryIndex,
    want: &str,
) -> BTreeSet<String> {
    // --- 1. Citation adjacency over live edges. ---------------------------
    let mut citations: HashMap<&str, Vec<&str>> = HashMap::new();
    for record in edges.values() {
        let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if CITATION_RELATIONS.contains(label) {
            citations
                .entry(source.as_str())
                .or_default()
                .push(target.as_str());
        }
    }
    let cites_repo = |id: &str| -> bool {
        citations
            .get(id)
            .is_some_and(|targets| targets.iter().any(|t| repo_index.owner_of(t) == Some(want)))
    };

    // --- 2. Unscoped agent attribution (same sources as the lane). ---------
    let mut observation_agent: HashMap<&str, &str> = HashMap::new();
    let mut session_agent: HashMap<&str, String> = HashMap::new();
    let mut stamped_nodes: HashMap<&str, &str> = HashMap::new();
    let mut candidate_support: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut decision_candidate: HashMap<&str, &str> = HashMap::new();
    let mut authored_by: HashMap<&str, Vec<String>> = HashMap::new();
    let mut session_of: HashMap<&str, String> = HashMap::new();

    for (&id, &record) in nodes {
        let GraphRecord::Node {
            kind,
            agent_id,
            user_context,
            ..
        } = record
        else {
            continue;
        };
        match kind {
            NodeKind::Observation => {
                if let Some(stamped) = agent_id {
                    observation_agent.insert(id, stamped.as_str());
                }
            }
            NodeKind::Agent | NodeKind::AgentSession => {
                if let Some(stamped) = agent_id {
                    stamped_nodes.insert(id, stamped.as_str());
                    if *kind == NodeKind::AgentSession {
                        session_agent.insert(id, stamped.clone());
                    }
                }
            }
            NodeKind::PromoteCandidate => {
                if let Some(evidence) = user_context.supporting_evidence.as_ref() {
                    for link in evidence {
                        if let Some(target) = link.target_record_id.as_deref() {
                            candidate_support.entry(id).or_default().push(target);
                        }
                    }
                }
            }
            NodeKind::PromotionDecision => {
                if let Some(candidate_id) = user_context.candidate_id.as_deref() {
                    decision_candidate.insert(id, candidate_id);
                }
            }
            _ => {}
        }
    }
    for record in edges.values() {
        let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        match label {
            EdgeLabel::ProposedBy => {
                candidate_support
                    .entry(source.as_str())
                    .or_default()
                    .push(target.as_str());
            }
            EdgeLabel::AuthoredBy => {
                authored_by
                    .entry(source.as_str())
                    .or_default()
                    .push(target.clone());
            }
            EdgeLabel::SessionOf => {
                session_of.insert(source.as_str(), target.clone());
            }
            _ => {}
        }
    }

    let support_agents = |candidate: &str| -> BTreeSet<&str> {
        candidate_support
            .get(candidate)
            .map(|support| {
                support
                    .iter()
                    .filter_map(|obs| observation_agent.get(obs))
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    };

    // --- 3. Agents in scope: any of their observations, candidates, or
    // decisions cites a record the repository owns. ------------------------
    let mut citing_agents: BTreeSet<&str> = BTreeSet::new();
    for (&observation, &agent) in &observation_agent {
        if cites_repo(observation) {
            citing_agents.insert(agent);
        }
    }
    for &candidate in candidate_support.keys() {
        if cites_repo(candidate) {
            citing_agents.extend(support_agents(candidate));
        }
    }
    for (&decision, &candidate) in &decision_candidate {
        if cites_repo(decision) {
            citing_agents.extend(support_agents(candidate));
        }
    }

    // --- 4. Node scope: containment, or attributable to a citing agent. ---
    let mut in_scope: BTreeSet<String> = BTreeSet::new();
    for (&id, &record) in nodes {
        if repo_index.owner_of(id) == Some(want) {
            in_scope.insert(id.to_owned());
            continue;
        }
        let attributed = match record.node_kind_ref() {
            Some(NodeKind::Observation | NodeKind::Agent | NodeKind::AgentSession) => {
                stamped_nodes
                    .get(id)
                    .is_some_and(|agent| citing_agents.contains(agent))
                    || observation_agent
                        .get(id)
                        .is_some_and(|agent| citing_agents.contains(agent))
            }
            Some(NodeKind::AgentRun) => resolve_agent(id, &session_agent, &session_of)
                .is_some_and(|agent| citing_agents.contains(agent.as_str())),
            Some(NodeKind::PromoteCandidate) => !support_agents(id).is_disjoint(&citing_agents),
            Some(NodeKind::PromotionDecision) => decision_candidate
                .get(id)
                .is_some_and(|candidate| !support_agents(candidate).is_disjoint(&citing_agents)),
            Some(NodeKind::Verification | NodeKind::CommandRun | NodeKind::TestRun) => {
                authored_by.get(id).is_some_and(|targets| {
                    targets.iter().any(|target| {
                        resolve_agent(target.as_str(), &session_agent, &session_of)
                            .is_some_and(|agent| citing_agents.contains(agent.as_str()))
                    })
                })
            }
            _ => false,
        };
        if attributed {
            in_scope.insert(id.to_owned());
        }
    }

    // --- 5. Edges are in scope exactly when both endpoints are. ------------
    for (&id, &record) in edges {
        let GraphRecord::Edge { source, target, .. } = record else {
            continue;
        };
        if in_scope.contains(source.as_str()) && in_scope.contains(target.as_str()) {
            in_scope.insert(id.to_owned());
        }
    }

    in_scope
}

/// Resolve any node to its agent via SESSION_OF chains ending at an
/// AgentSession with a stamped agent_id. Walks at most 8 hops.
fn resolve_agent(
    start: &str,
    session_agent: &HashMap<&str, String>,
    session_of: &HashMap<&str, String>,
) -> Option<String> {
    let mut current = start;
    for _ in 0..8 {
        if let Some(agent) = session_agent.get(current) {
            return Some(agent.clone());
        }
        match session_of.get(current) {
            Some(next) => current = next.as_str(),
            None => return None,
        }
    }
    None
}

/// Compute the per-agent track record over `records`.
///
/// `repo_id` is an optional repository id. When `Some`, code-domain records
/// participate by containment ([`RepositoryIndex::owner_of`]) and
/// agent-domain records participate by citation: an agent is in scope iff
/// any of its observations, candidates, or decisions cites a record the
/// repository owns (see [`scoped_record_ids`]). Pass `None` for the
/// unscoped lane, whose behavior is unchanged.
#[must_use]
pub fn agent_track_record(records: &[GraphRecord], repo_id: Option<&str>) -> TrackRecordReport {
    // Last write wins: process records in order. A node/edge with the same
    // id replaces the earlier one; a tombstone removes the entity (but a
    // later recreation with the same id revives it).
    let mut nodes: HashMap<&str, &GraphRecord> = HashMap::new();
    let mut edges: HashMap<&str, &GraphRecord> = HashMap::new();
    for record in records {
        match record {
            GraphRecord::Node { id, .. } => {
                nodes.insert(id.as_str(), record);
                edges.remove(id.as_str());
            }
            GraphRecord::Edge { id, .. } => {
                edges.insert(id.as_str(), record);
                nodes.remove(id.as_str());
            }
            GraphRecord::Tombstone { deleted_id, .. } => {
                nodes.remove(deleted_id.as_str());
                edges.remove(deleted_id.as_str());
            }
        }
    }

    let repo_index = RepositoryIndex::build(records);
    // Citation-based `--repo` scoping for agent-domain records (see
    // `scoped_record_ids`): computed once, up front, from the unscoped live
    // view. The unscoped lane keeps the old always-true filter.
    let scoped_ids: Option<BTreeSet<String>> =
        repo_id.map(|want| scoped_record_ids(&nodes, &edges, &repo_index, want));
    let in_scope = |id: &str| -> bool { scoped_ids.as_ref().is_none_or(|ids| ids.contains(id)) };

    let mut agents: BTreeMap<String, AgentAccum> = BTreeMap::new();
    let mut diagnostics: Vec<TrackRecordDiagnostic> = Vec::new();

    // --- 1. Observations and agent/session nodes. --------------------------
    let mut observation_agent: HashMap<&str, String> = HashMap::new();
    let mut session_agent: HashMap<&str, String> = HashMap::new();
    let mut observations_without_agent: BTreeSet<String> = BTreeSet::new();

    for (&id, &record) in &nodes {
        if !in_scope(id) {
            continue;
        }
        let GraphRecord::Node {
            kind,
            agent_id,
            agent_kind,
            superseded_by,
            ..
        } = record
        else {
            continue;
        };
        match kind {
            NodeKind::Observation => match agent_id {
                Some(stamped) => {
                    observation_agent.insert(id, stamped.clone());
                    let accum = agents.entry(stamped.clone()).or_default();
                    if let Some(k) = agent_kind {
                        accum.kinds.insert(k.clone());
                    }
                    accum.observations_written.insert(id.to_owned());
                    if superseded_by.is_some() {
                        accum.superseded.insert(id.to_owned());
                    }
                }
                None => {
                    observations_without_agent.insert(id.to_owned());
                }
            },
            NodeKind::AgentSession => {
                if let Some(stamped) = agent_id {
                    session_agent.insert(id, stamped.clone());
                    // A session that never authored anything still proves the
                    // agent exists; capture its kind.
                    let accum = agents.entry(stamped.clone()).or_default();
                    if let Some(k) = agent_kind {
                        accum.kinds.insert(k.clone());
                    }
                }
            }
            NodeKind::Agent => {
                // Idle agents: a live Agent node with a stamped id gets an
                // explicit (possibly empty) row in unscoped queries. Under
                // --repo the Agent node itself is scope-filtered above, so
                // unrelated idle agents are not seeded.
                if repo_id.is_none() {
                    if let Some(stamped) = agent_id {
                        let accum = agents.entry(stamped.clone()).or_default();
                        if let Some(k) = agent_kind {
                            accum.kinds.insert(k.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // --- 2. Candidate support: node links + PROPOSED_BY edges. -------------
    let mut candidate_support: HashMap<&str, BTreeSet<String>> = HashMap::new();
    for (&id, &record) in &nodes {
        if !in_scope(id) {
            continue;
        }
        let GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            ..
        } = record
        else {
            continue;
        };
        let support = candidate_support.entry(id).or_default();
        if let Some(evidence) = user_context.supporting_evidence.as_ref() {
            for link in evidence {
                if let Some(target) = link.target_record_id.as_deref() {
                    if nodes.get(target).and_then(|r| r.node_kind_ref())
                        == Some(NodeKind::Observation)
                        && in_scope(target)
                    {
                        support.insert(target.to_owned());
                    }
                }
            }
        }
    }
    for record in edges.values() {
        let GraphRecord::Edge {
            label: EdgeLabel::ProposedBy,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if !in_scope(source) || !in_scope(target) {
            continue;
        }
        if nodes.get(source.as_str()).and_then(|r| r.node_kind_ref())
            == Some(NodeKind::PromoteCandidate)
            && nodes.get(target.as_str()).and_then(|r| r.node_kind_ref())
                == Some(NodeKind::Observation)
        {
            candidate_support
                .entry(source.as_str())
                .or_default()
                .insert(target.clone());
        }
    }

    // --- 3. Terminal promotion decisions: latest decision wins. ------------
    // candidate id -> (decided_at, record_id, verdict)
    let mut latest_decision: HashMap<&str, (String, String, PromotionVerdict)> = HashMap::new();

    for (&id, &record) in &nodes {
        if !in_scope(id) {
            continue;
        }
        let GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } = record
        else {
            continue;
        };
        let Some(candidate_id) = user_context.candidate_id.as_deref() else {
            continue;
        };
        if !in_scope(candidate_id) {
            continue;
        }
        let verdict = PromotionVerdict::from_outcome(user_context.outcome.as_deref());
        let decided_at = user_context.decided_at.clone().unwrap_or_default();
        let entry = latest_decision
            .entry(candidate_id)
            .or_insert_with(|| (String::new(), String::new(), PromotionVerdict::Unknown));
        // Latest by (decided_at, record_id); RFC3339 timestamps sort
        // lexicographically. Strictly greater wins so the first-seen record
        // wins exact ties deterministically.
        let key = (decided_at.clone(), (*id).to_owned());
        let current = (entry.0.clone(), entry.1.clone());
        if key > current {
            *entry = (decided_at, (*id).to_owned(), verdict);
        }
    }

    // Attribute each candidate's terminal verdict to the agents whose
    // observations supported it: one credit per agent per candidate, no
    // matter how many supporting observations.
    let mut candidates_without_decision: BTreeSet<String> = BTreeSet::new();
    for (candidate_id, support) in &candidate_support {
        let supporting_agents: BTreeSet<&String> = support
            .iter()
            .filter_map(|obs| observation_agent.get(obs.as_str()))
            .collect();
        if supporting_agents.is_empty() {
            continue;
        }
        // Cite only the terminal decision's ID: superseded decisions
        // are never cited.
        let Some((_, decision_id, v)) = latest_decision.get(candidate_id) else {
            candidates_without_decision.insert((*candidate_id).to_owned());
            continue;
        };
        let verdict = *v;
        let mut decision_ids = BTreeSet::new();
        decision_ids.insert(decision_id.clone());
        if verdict == PromotionVerdict::Unknown {
            continue;
        }
        for agent_id in supporting_agents {
            let accum = agents.entry((*agent_id).clone()).or_default();
            let (candidates, decisions) = match verdict {
                PromotionVerdict::Approved => (
                    &mut accum.approved_candidates,
                    &mut accum.approved_decisions,
                ),
                PromotionVerdict::EditedThenApproved => {
                    (&mut accum.edited_candidates, &mut accum.edited_decisions)
                }
                PromotionVerdict::Rejected => (
                    &mut accum.rejected_candidates,
                    &mut accum.rejected_decisions,
                ),
                PromotionVerdict::Deferred => (
                    &mut accum.deferred_candidates,
                    &mut accum.deferred_decisions,
                ),
                PromotionVerdict::Expired => {
                    (&mut accum.expired_candidates, &mut accum.expired_decisions)
                }
                PromotionVerdict::Unknown => continue,
            };
            candidates.insert((*candidate_id).to_owned());
            for did in &decision_ids {
                decisions.insert(did.clone());
            }
        }
    }

    // --- 4. Verification via sessions. -------------------------------------
    // Verification -AUTHORED_BY-> run/session -SESSION_OF-> ... -> AgentSession.
    let mut authored_by: HashMap<&str, Vec<String>> = HashMap::new();
    let mut session_of: HashMap<&str, String> = HashMap::new();
    for record in edges.values() {
        let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if !in_scope(source) || !in_scope(target) {
            continue;
        }
        match label {
            EdgeLabel::AuthoredBy => {
                authored_by
                    .entry(source.as_str())
                    .or_default()
                    .push(target.clone());
            }
            EdgeLabel::SessionOf => {
                session_of.insert(source.as_str(), target.clone());
            }
            _ => {}
        }
    }

    // Resolve any node to its agent via SESSION_OF chains ending at an
    // AgentSession with a stamped agent_id.
    let mut verifications_without_session: BTreeSet<String> = BTreeSet::new();
    for (&id, &record) in &nodes {
        if !in_scope(id) {
            continue;
        }
        if record.node_kind_ref() != Some(NodeKind::Verification)
            && record.node_kind_ref() != Some(NodeKind::CommandRun)
            && record.node_kind_ref() != Some(NodeKind::TestRun)
        {
            continue;
        }
        let mut attributed = false;
        if let Some(targets) = authored_by.get(id) {
            for target in targets {
                if let Some(agent_id) = resolve_agent(target.as_str(), &session_agent, &session_of)
                {
                    attributed = true;
                    let accum = agents.entry(agent_id).or_default();
                    match verification_outcome(record) {
                        VerificationOutcome::Passing => {
                            accum.verifications_passed.insert((*id).to_owned());
                        }
                        VerificationOutcome::Failing => {
                            accum.verifications_failed.insert((*id).to_owned());
                        }
                        VerificationOutcome::Inconclusive => {
                            accum.verifications_inconclusive.insert((*id).to_owned());
                        }
                    }
                    // One attribution per verification: the first resolvable
                    // session wins deterministically.
                    break;
                }
            }
        }
        if !attributed {
            verifications_without_session.insert((*id).to_owned());
        }
    }

    // --- 5. Diagnostics. ----------------------------------------------------
    if !observations_without_agent.is_empty() {
        diagnostics.push(TrackRecordDiagnostic {
            code: "observations_without_agent_id".to_string(),
            message: "Live observations with no stamped agent_id; excluded from per-agent rows."
                .to_string(),
            count: observations_without_agent.len(),
            record_ids: observations_without_agent.iter().cloned().collect(),
        });
    }
    if !candidates_without_decision.is_empty() {
        diagnostics.push(TrackRecordDiagnostic {
            code: "candidates_without_terminal_decision".to_string(),
            message: "Candidates with agent-attributable support but no promotion decision yet; nothing counted."
                .to_string(),
            count: candidates_without_decision.len(),
            record_ids: candidates_without_decision.iter().cloned().collect(),
        });
    }
    if !verifications_without_session.is_empty() {
        diagnostics.push(TrackRecordDiagnostic {
            code: "verifications_without_session".to_string(),
            message:
                "Verification records not linked to an agent session; excluded from per-agent rows."
                    .to_string(),
            count: verifications_without_session.len(),
            record_ids: verifications_without_session.iter().cloned().collect(),
        });
    }

    // Canonical order: BTreeMap iteration is already sorted by agent_id
    // (byte order, matching the test's sort_unstable expectation).
    let agent_rows: Vec<AgentTrackRecord> = agents
        .into_iter()
        .map(|(agent_id, accum)| accum.into_record(agent_id))
        .collect();

    if agent_rows.is_empty() {
        diagnostics.push(TrackRecordDiagnostic {
            code: "no_agents".to_string(),
            message: "No agents with qualifying records in scope; explicit empty verdict."
                .to_string(),
            count: 0,
            record_ids: Vec::new(),
        });
    }

    TrackRecordReport {
        agents: agent_rows,
        diagnostics,
    }
}

/// Promotion verdict normalized from a decision's `user_context.outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromotionVerdict {
    Approved,
    EditedThenApproved,
    Rejected,
    Deferred,
    Expired,
    Unknown,
}

impl PromotionVerdict {
    fn from_outcome(outcome: Option<&str>) -> Self {
        let normalized = outcome.unwrap_or("");
        match normalized.trim() {
            "approved" => Self::Approved,
            "edited_then_approved" | "edited-approved" | "approved_with_edits" => {
                Self::EditedThenApproved
            }
            "rejected" => Self::Rejected,
            "deferred" => Self::Deferred,
            "expired" => Self::Expired,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citable_count_collects_sorted_ids() {
        let ids: BTreeSet<String> = ["b".to_owned(), "a".to_owned()].into_iter().collect();
        let count = CitableCount::from_ids(&ids, BASIS_AGENT_CLAIMS);
        assert_eq!(count.count, 2);
        assert_eq!(count.record_ids, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(count.trust_basis, BASIS_AGENT_CLAIMS);
        let empty = CitableCount::empty(BASIS_VERIFICATION);
        assert_eq!(empty.count, 0);
        assert!(empty.record_ids.is_empty());
    }

    #[test]
    fn verdict_normalization_covers_aliases() {
        assert_eq!(
            PromotionVerdict::from_outcome(Some("approved")),
            PromotionVerdict::Approved
        );
        assert_eq!(
            PromotionVerdict::from_outcome(Some("edited_then_approved")),
            PromotionVerdict::EditedThenApproved
        );
        assert_eq!(
            PromotionVerdict::from_outcome(Some("bogus")),
            PromotionVerdict::Unknown
        );
        assert_eq!(
            PromotionVerdict::from_outcome(None),
            PromotionVerdict::Unknown
        );
    }

    #[test]
    fn disclaimer_is_nonempty_and_lane_is_stable() {
        assert!(!TRACK_RECORD_DISCLAIMER.is_empty());
        assert_eq!(TRACK_RECORD_LANE, "track-record");
    }

    #[test]
    fn empty_input_yields_no_agents_diagnostic() {
        let report = agent_track_record(&[], None);
        assert!(report.agents.is_empty());
        assert!(report.diagnostics.iter().any(|d| d.code == "no_agents"));
    }
}
