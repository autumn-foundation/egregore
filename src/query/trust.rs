//! Derived per-record trust classes for cross-domain context answers (issue #114).
//!
//! # Domain is not trust
//!
//! Every record already carries a `domain` namespace (issue #3), but `domain` is
//! *where a record lives*, not *how much it should be trusted*. An `agent_memory`
//! `Observation` may be an unverified guess, a guess backed by a passing
//! verification record, or one that has since been contradicted or superseded —
//! all three sit in the same domain. A cross-domain context answer deliberately
//! returns code facts, agent observations, and verification evidence side by
//! side, which is the one place that distinction can collapse: without a label
//! the cheapest path for a consuming agent is to treat every row as equally
//! true.
//!
//! [`TrustClass`] is that label. It is a **deterministic function of the
//! record's node kind plus its evidence and contradiction edges at the queried
//! snapshot** — no wall clock, no ranking, no numeric confidence (confidence
//! already lives on observations and is out of scope here).
//!
//! # Completeness invariant
//!
//! [`TrustIndex::classify`] matches every [`NodeKind`] variant with **no
//! wildcard arm**, mirroring the traversal-membership invariant in
//! [`super::evidence_path`]. A new node kind therefore fails to compile until it
//! is deliberately classified, so a future kind can never silently inherit a
//! truth-bearing label. Kinds with no derivation rule map to
//! [`TrustClass::Other`] — never to `source_derived` or `verification_evidence`.
//!
//! # What a label is not
//!
//! `verification_evidence` marks a *recorded verification execution*, never
//! proof of correctness. `agent_verified` states that the claim cites a live
//! passing verification record, never that the claim is true. `source_derived`
//! states that the record was derived deterministically from source, never that
//! it still matches the working tree (see `eg query context --repo-path` for
//! that).

use std::collections::BTreeMap;

use crate::ir::{EdgeLabel, GraphRecord, NodeKind};
use crate::temporal_status::TemporalResolver;

use super::memory_audit::{
    OutgoingEdgeIndex, TombstonedSet, is_verification_kind, record_node_kind,
    verification_support_indexes,
};

/// Verification `status` values that count as a **passing** outcome when
/// deciding whether a cited verification record upgrades an agent claim to
/// [`TrustClass::AgentVerified`].
///
/// Deliberately closed and compared case-insensitively after trimming. The two
/// spellings in the tree are `pass` (written by `eg capture-tests` and validated
/// by `docs/schema/verification.md`) and `passed` (the vocabulary documented on
/// the `status` field in `crate::ir`); `success` covers CI-shaped imports. Any
/// other value — including an absent one — is **not** treated as passing, so an
/// unrecognized status fails closed to `agent_unverified` rather than
/// overclaiming.
const PASSING_VERIFICATION_STATUSES: &[&str] = &["pass", "passed", "success"];

/// The closed trust vocabulary attached to every record a cross-domain context
/// answer returns (issue #114).
///
/// See the module documentation for the "domain is not trust" distinction and
/// for what each label does *not* claim.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum TrustClass {
    /// Deterministic code-graph or semantic-derived record (`Symbol`, `File`,
    /// `Commit`, `SemanticDrift`, code-graph topology edges, …). Derived from
    /// source by the extractor; never an agent claim.
    SourceDerived,
    /// A recorded verification execution (`TestRun`, `CommandRun`, `CIStatus`,
    /// …). Evidence that a check ran, never proof that the code is correct.
    VerificationEvidence,
    /// Agent-authored claim citing at least one live, **passing** verification
    /// record. The citation is structural, not a truth judgement.
    AgentVerified,
    /// Agent-authored claim with no such citation — a hypothesis.
    AgentUnverified,
    /// Agent-authored claim displaced by a live `CONTRADICTS` or `SUPERSEDES`
    /// relationship. Takes precedence over [`Self::AgentVerified`].
    AgentContradicted,
    /// Imported external work state (`Task`, `PR`, `Review`, …). A claim made by
    /// an issue tracker, not verified by Egregore.
    ProjectState,
    /// Produced bytes (`Artifact`, `PatchArtifact`, `FileEdit`).
    Artifact,
    /// A program's own claim about its execution (`ErrorSignature`, `LogEvent`,
    /// …): deterministically parsed but never verified.
    RuntimeObservation,
    /// No derivation rule applies to this record kind. Never a truth-bearing
    /// label — the deliberate fallback that keeps an unclassified record from
    /// inheriting `source_derived`.
    Other,
}

impl TrustClass {
    /// Every member of the closed vocabulary, in documentation order.
    pub const ALL: [Self; 9] = [
        Self::SourceDerived,
        Self::VerificationEvidence,
        Self::AgentVerified,
        Self::AgentUnverified,
        Self::AgentContradicted,
        Self::ProjectState,
        Self::Artifact,
        Self::RuntimeObservation,
        Self::Other,
    ];

    /// Returns the stable wire string emitted in the `trust` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceDerived => "source_derived",
            Self::VerificationEvidence => "verification_evidence",
            Self::AgentVerified => "agent_verified",
            Self::AgentUnverified => "agent_unverified",
            Self::AgentContradicted => "agent_contradicted",
            Self::ProjectState => "project_state",
            Self::Artifact => "artifact",
            Self::RuntimeObservation => "runtime_observation",
            Self::Other => "other",
        }
    }

    /// Returns `true` for the three agent-authored classes.
    ///
    /// Used by the answer-surface guarantee: a row in an agent-authored section
    /// must be one of these, and a code or verification row must not be.
    #[must_use]
    pub const fn is_agent_authored(self) -> bool {
        matches!(
            self,
            Self::AgentVerified | Self::AgentUnverified | Self::AgentContradicted
        )
    }
}

impl serde::Serialize for TrustClass {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The trust class of an agent-authored record.
///
/// A separate enum so the agent branch of [`TrustIndex::classify`] is
/// *structurally* unable to return `source_derived` or `verification_evidence`
/// — the mislabel this issue exists to prevent cannot be written, not merely
/// tested against.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AgentTrust {
    Verified,
    Unverified,
    Contradicted,
}

impl From<AgentTrust> for TrustClass {
    fn from(value: AgentTrust) -> Self {
        match value {
            AgentTrust::Verified => Self::AgentVerified,
            AgentTrust::Unverified => Self::AgentUnverified,
            AgentTrust::Contradicted => Self::AgentContradicted,
        }
    }
}

/// Resolves the derived [`TrustClass`] of any record in a graph slice.
///
/// Build once per answer and reuse: construction is a single pass over the
/// slice, and it owns the supersession resolver and verification-support
/// indexes that the agent-authored derivation needs.
pub struct TrustIndex<'a> {
    by_id: BTreeMap<&'a str, &'a GraphRecord>,
    edges_from: OutgoingEdgeIndex<'a>,
    tombstoned: TombstonedSet<'a>,
    resolver: TemporalResolver<'a>,
}

impl<'a> TrustIndex<'a> {
    /// Builds the index over a graph slice.
    ///
    /// The slice must be the same corpus the answer was computed over: trust is
    /// a function of the record *plus its edges at the queried snapshot*, so
    /// classifying against a wider slice would report contradictions the answer
    /// itself does not show.
    #[must_use]
    pub fn build(records: &'a [GraphRecord]) -> Self {
        let (edges_from, tombstoned) = verification_support_indexes(records);
        let by_id: BTreeMap<&str, &GraphRecord> = records
            .iter()
            .filter(|r| matches!(r, GraphRecord::Node { .. }))
            .map(|r| (r.id(), r))
            .collect();
        Self {
            by_id,
            edges_from,
            tombstoned,
            resolver: TemporalResolver::build(records),
        }
    }

    /// Returns the derived trust class for one record.
    ///
    /// Deterministic: for a fixed slice the same record always yields the same
    /// class, so re-running an answer over an unchanged store produces
    /// byte-identical labels.
    #[must_use]
    pub fn classify(&self, record: &GraphRecord) -> TrustClass {
        let kind = match record {
            GraphRecord::Node { kind, .. } => *kind,
            // A code-graph topology edge (DEFINES/CALLS/IMPORTS/…) is an
            // extractor-derived structural fact. Every other edge label is
            // relationship metadata whose trust belongs to its endpoints, not to
            // the edge, so it stays deliberately unlabelled.
            GraphRecord::Edge { label, .. } => {
                return if label.is_codegraph_topology_label() {
                    TrustClass::SourceDerived
                } else {
                    TrustClass::Other
                };
            }
            // A tombstone is a deletion marker, not a claim about the world.
            GraphRecord::Tombstone { .. } => return TrustClass::Other,
        };

        // Exhaustive: no wildcard arm, so a new NodeKind fails to compile here
        // until it is deliberately classified (the completeness invariant).
        match kind {
            // ── agent-authored claims ────────────────────────────────────────
            // Routed through `AgentTrust`, which cannot express a non-agent
            // class, so an agent claim can never be labelled source truth.
            NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
                self.agent_trust(record).into()
            }

            // ── deterministic code-graph and semantic-derived facts ──────────
            NodeKind::Repository
            | NodeKind::File
            | NodeKind::Module
            | NodeKind::Symbol
            | NodeKind::Import
            | NodeKind::Diagnostic
            | NodeKind::PanicRiskSite
            | NodeKind::DebtMarker
            | NodeKind::UnsafeSite
            | NodeKind::DependencyDeclaration
            | NodeKind::ScanCoverage
            | NodeKind::Commit
            | NodeKind::Change
            | NodeKind::SemanticDrift
            | NodeKind::EmbeddingModel
            | NodeKind::EmbeddingVector => TrustClass::SourceDerived,

            // ── recorded verification execution ──────────────────────────────
            NodeKind::Verification
            | NodeKind::CommandEvidence
            | NodeKind::CommandRun
            | NodeKind::TestRun
            | NodeKind::CIStatus
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
            | NodeKind::ProofResult => TrustClass::VerificationEvidence,

            // ── imported external work state ─────────────────────────────────
            NodeKind::Task
            | NodeKind::AcceptanceCriterion
            | NodeKind::ExternalLink
            | NodeKind::Product
            | NodeKind::Project
            | NodeKind::Plan
            | NodeKind::GitHubIssue
            | NodeKind::PR
            | NodeKind::Review
            | NodeKind::ExternalIdentity
            | NodeKind::ReviewStateTransition
            | NodeKind::LocalTask => TrustClass::ProjectState,

            // ── produced bytes ───────────────────────────────────────────────
            NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
                TrustClass::Artifact
            }

            // ── a program's own claim about its execution ────────────────────
            NodeKind::LogSource
            | NodeKind::ErrorSignature
            | NodeKind::LogEvent
            | NodeKind::LogOccurrenceBucket => TrustClass::RuntimeObservation,

            // ── no derivation rule applies ───────────────────────────────────
            // Agent-memory scaffolding (an `Agent` is an identity, not a claim),
            // trajectory frames, cost accounting, retraction events, and
            // user-context preference records. Preference/promotion trust is
            // explicitly out of scope for this issue — it is user-context policy
            // covered by the promotion flow — so those kinds stay `other` rather
            // than borrowing a truth-bearing label.
            NodeKind::Agent
            | NodeKind::AgentSession
            | NodeKind::AgentRun
            | NodeKind::AgentTurn
            | NodeKind::ToolCall
            | NodeKind::CostUsage
            | NodeKind::Retraction
            | NodeKind::PromoteCandidate
            | NodeKind::PromotionPrompt
            | NodeKind::PromotionDecision
            | NodeKind::Preference
            | NodeKind::WorkflowRule
            | NodeKind::NamingDecision
            | NodeKind::Constraint => TrustClass::Other,
        }
    }

    /// Derives the trust class of an agent-authored claim.
    ///
    /// Precedence, highest first:
    ///
    /// 1. **Displaced** — the claim is superseded or contradicted at this
    ///    snapshot. Resolved by the same [`TemporalResolver`] that drives the
    ///    `temporal_status` field and the `excluded` section, so the derived
    ///    label can never disagree with them. A supersession *cycle* counts as
    ///    displaced for the same reason `apply_supersession` excludes it: the
    ///    chain is broken, so the claim cannot be presented as current.
    ///
    ///    Issue #114's AC3 folds `SUPERSEDES` and `CONTRADICTS` into the single
    ///    `agent_contradicted` label; read it as **displaced** — "another record
    ///    has taken this one's place, do not act on it" — rather than as a claim
    ///    that the two records assert opposite things.
    ///
    /// 2. **Verified** — the claim cites at least one live verification record
    ///    whose recorded status is passing.
    ///
    /// 3. Otherwise **unverified**.
    fn agent_trust(&self, record: &GraphRecord) -> AgentTrust {
        // Re-borrow the id from the index so the resolver gets a `&'a str`; a
        // record absent from the slice cannot carry a supersession relationship
        // recorded in it, so it falls through to the evidence check.
        if let Some((id, _)) = self.by_id.get_key_value(record.id()) {
            let (status, _, _) = self.resolver.resolve_status(id);
            if matches!(status, "superseded" | "contradicted" | "cycle") {
                return AgentTrust::Contradicted;
            }
        }
        if self.cites_passing_verification(record) {
            AgentTrust::Verified
        } else {
            AgentTrust::Unverified
        }
    }

    /// Returns `true` when the claim cites at least one live verification
    /// record whose recorded status is passing.
    ///
    /// The traversal is the one [`super::memory_audit::is_verified_claim`]
    /// performs for `--verified-only` — an outbound `VALIDATED_BY`,
    /// `HAS_EVIDENCE`, or `PRODUCED_EVIDENCE` evidence link or edge onto a live
    /// verification-domain record — so the two surfaces agree on *which* records
    /// back a claim. This adds one refinement mandated by AC3: the backing
    /// record must also be **passing**, so a claim whose only evidence is a
    /// failing run stays `agent_unverified`.
    fn cites_passing_verification(&self, record: &GraphRecord) -> bool {
        self.backing_verification_records(record)
            .into_iter()
            .any(is_passing_verification)
    }

    /// Collects the live verification-domain records a claim cites through a
    /// backing relation, in deterministic slice order.
    fn backing_verification_records(&self, record: &GraphRecord) -> Vec<&'a GraphRecord> {
        let mut out: Vec<&'a GraphRecord> = Vec::new();
        let mut push = |target_id: &str| {
            if self.tombstoned.contains(target_id) {
                return;
            }
            if let Some(target) = self.by_id.get(target_id)
                && record_node_kind(target).is_some_and(is_verification_kind)
            {
                out.push(target);
            }
        };

        if let GraphRecord::Node {
            evidence_links: Some(links),
            ..
        } = record
        {
            for link in links {
                // A backing relation is required: a generic link that merely
                // happens to point at a verification record does not make the
                // claim verified.
                if matches!(
                    link.relation.as_str(),
                    "VALIDATED_BY" | "HAS_EVIDENCE" | "PRODUCED_EVIDENCE"
                ) && let Some(target_id) = link.target_record_id.as_deref()
                {
                    push(target_id);
                }
            }
        }
        if let Some(outgoing) = self.edges_from.get(record.id()) {
            for (label, target) in outgoing {
                if matches!(
                    label,
                    EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence | EdgeLabel::ProducedEvidence
                ) {
                    push(target);
                }
            }
        }
        out
    }
}

/// Returns `true` when a verification record's recorded outcome is passing.
///
/// `status` wins when present. When it is absent, an `exit_code` of `0` counts
/// as passing — the same rule `crate::evidence` uses when it derives a status
/// from a command's exit code. With neither field the outcome is unknown, which
/// is **not** passing.
fn is_passing_verification(record: &GraphRecord) -> bool {
    let GraphRecord::Node {
        status, exit_code, ..
    } = record
    else {
        return false;
    };
    if let Some(status) = status.as_deref() {
        return PASSING_VERIFICATION_STATUSES
            .contains(&status.trim().to_ascii_lowercase().as_str());
    }
    exit_code.is_some_and(|code| code == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EvidenceLink, VERIFICATION_SCHEMA_VERSION,
        agent_memory_stable_id, verification_stable_id,
    };

    fn node(id: String, kind: NodeKind) -> GraphRecord {
        GraphRecord::node(id, kind, None, None, None, "summary".to_owned())
    }

    fn observation(seed: &str) -> GraphRecord {
        let mut record = node(
            agent_memory_stable_id(&["obs", seed]),
            NodeKind::Observation,
        );
        if let GraphRecord::Node { schema_version, .. } = &mut record {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        }
        record
    }

    fn run(seed: &str, run_status: Option<&str>, code: Option<i64>) -> GraphRecord {
        let mut record = node(
            verification_stable_id(&["verification", seed]),
            NodeKind::TestRun,
        );
        if let GraphRecord::Node {
            schema_version,
            status,
            exit_code,
            ..
        } = &mut record
        {
            *schema_version = VERIFICATION_SCHEMA_VERSION;
            *status = run_status.map(ToOwned::to_owned);
            *exit_code = code;
        }
        record
    }

    fn cite(record: &mut GraphRecord, relation: &str, target: &str) {
        let GraphRecord::Node { evidence_links, .. } = record else {
            panic!("node expected");
        };
        evidence_links
            .get_or_insert_with(Vec::new)
            .push(EvidenceLink {
                target_record_id: Some(target.to_owned()),
                target_domain: "verification".to_owned(),
                relation: relation.to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            });
    }

    #[test]
    fn vocabulary_strings_are_unique_and_stable() {
        let mut seen = std::collections::BTreeSet::new();
        for class in TrustClass::ALL {
            assert!(
                seen.insert(class.as_str()),
                "duplicate trust wire string: {}",
                class.as_str()
            );
        }
        assert_eq!(seen.len(), TrustClass::ALL.len());
    }

    #[test]
    fn agent_kinds_can_only_ever_resolve_to_agent_classes() {
        // The structural guarantee: whatever the evidence shape, an agent kind
        // resolves into the agent-authored subset and never into source truth.
        let passing = run("pass", Some("passed"), None);
        let failing = run("fail", Some("failed"), None);

        let plain = observation("plain");
        let mut verified = observation("verified");
        cite(&mut verified, "VALIDATED_BY", passing.id());
        let mut failing_backed = observation("failing_backed");
        cite(&mut failing_backed, "VALIDATED_BY", failing.id());

        let records = vec![
            passing.clone(),
            failing.clone(),
            plain.clone(),
            verified.clone(),
            failing_backed.clone(),
        ];
        let index = TrustIndex::build(&records);
        for record in [&plain, &verified, &failing_backed] {
            assert!(
                index.classify(record).is_agent_authored(),
                "agent record escaped the agent-authored subset"
            );
        }
        assert_eq!(index.classify(&plain), TrustClass::AgentUnverified);
        assert_eq!(index.classify(&verified), TrustClass::AgentVerified);
        assert_eq!(
            index.classify(&failing_backed),
            TrustClass::AgentUnverified,
            "a failing verification record must not confer agent_verified"
        );
        assert_eq!(
            index.classify(&passing),
            TrustClass::VerificationEvidence,
            "a verification record is never an agent class"
        );
    }

    #[test]
    fn generic_relation_onto_a_passing_run_does_not_verify() {
        let passing = run("pass", Some("passed"), None);
        let mut claim = observation("generic");
        cite(&mut claim, "RELATES_TO", passing.id());
        let records = vec![passing, claim.clone()];
        let index = TrustIndex::build(&records);
        assert_eq!(index.classify(&claim), TrustClass::AgentUnverified);
    }

    #[test]
    fn contradiction_dominates_a_passing_verification_link() {
        let passing = run("pass", Some("pass"), None);
        let mut claim = observation("displaced");
        cite(&mut claim, "VALIDATED_BY", passing.id());
        let rebuttal = observation("rebuttal");
        let edge = GraphRecord::agent_memory_edge(
            EdgeLabel::Contradicts,
            rebuttal.id().to_owned(),
            claim.id().to_owned(),
            None,
            "contradicts".to_owned(),
        );
        let records = vec![passing, claim.clone(), rebuttal, edge];
        let index = TrustIndex::build(&records);
        assert_eq!(index.classify(&claim), TrustClass::AgentContradicted);
    }

    #[test]
    fn supersession_is_displaced_too() {
        let claim = observation("old");
        let newer = observation("new");
        let edge = GraphRecord::agent_memory_edge(
            EdgeLabel::Supersedes,
            newer.id().to_owned(),
            claim.id().to_owned(),
            None,
            "supersedes".to_owned(),
        );
        let records = vec![claim.clone(), newer.clone(), edge];
        let index = TrustIndex::build(&records);
        assert_eq!(index.classify(&claim), TrustClass::AgentContradicted);
        assert_eq!(index.classify(&newer), TrustClass::AgentUnverified);
    }

    #[test]
    fn exit_code_zero_verifies_only_when_status_is_absent() {
        let ok = run("ok", None, Some(0));
        let nonzero = run("nonzero", None, Some(1));
        // An explicit failing status wins over a zero exit code.
        let conflicting = run("conflicting", Some("failed"), Some(0));

        let mut a = observation("a");
        cite(&mut a, "HAS_EVIDENCE", ok.id());
        let mut b = observation("b");
        cite(&mut b, "HAS_EVIDENCE", nonzero.id());
        let mut c = observation("c");
        cite(&mut c, "HAS_EVIDENCE", conflicting.id());

        let records = vec![ok, nonzero, conflicting, a.clone(), b.clone(), c.clone()];
        let index = TrustIndex::build(&records);
        assert_eq!(index.classify(&a), TrustClass::AgentVerified);
        assert_eq!(index.classify(&b), TrustClass::AgentUnverified);
        assert_eq!(index.classify(&c), TrustClass::AgentUnverified);
    }

    #[test]
    fn unknown_status_fails_closed_to_unverified() {
        let weird = run("weird", Some("inconclusive"), None);
        let mut claim = observation("weird_claim");
        cite(&mut claim, "VALIDATED_BY", weird.id());
        let records = vec![weird, claim.clone()];
        let index = TrustIndex::build(&records);
        assert_eq!(index.classify(&claim), TrustClass::AgentUnverified);
    }

    #[test]
    fn edge_backed_citation_verifies() {
        let passing = run("pass", Some("passed"), None);
        let claim = observation("edge_backed");
        let edge = GraphRecord::agent_memory_edge(
            EdgeLabel::ValidatedBy,
            claim.id().to_owned(),
            passing.id().to_owned(),
            None,
            "validated by".to_owned(),
        );
        let records = vec![passing, claim.clone(), edge];
        let index = TrustIndex::build(&records);
        assert_eq!(index.classify(&claim), TrustClass::AgentVerified);
    }

    #[test]
    fn topology_edges_are_source_derived_and_other_edges_are_not() {
        let defines = GraphRecord::edge(
            EdgeLabel::Defines,
            "a".to_owned(),
            "b".to_owned(),
            None,
            String::new(),
        );
        let validated = GraphRecord::agent_memory_edge(
            EdgeLabel::ValidatedBy,
            "a".to_owned(),
            "b".to_owned(),
            None,
            String::new(),
        );
        let records = vec![defines.clone(), validated.clone()];
        let index = TrustIndex::build(&records);
        assert_eq!(index.classify(&defines), TrustClass::SourceDerived);
        assert_eq!(index.classify(&validated), TrustClass::Other);
    }

    /// The codebase carries two trust vocabularies and they must not drift.
    ///
    /// `crate::cli::trust_class_for` is the older **static, kind-only** bucket
    /// rendered by non-context lanes (`eg query memory`, `eg query sessions`,
    /// `eg query transaction-time`, the subsystem `log_signatures` rows). The
    /// derived [`TrustClass`] here refines it for context answers: the
    /// code-graph bucket is renamed `source_fact` → `source_derived`, and the
    /// single `agent_authored` bucket splits into the three evidence-derived
    /// agent classes.
    ///
    /// This test pins that correspondence for every kind the legacy function
    /// classifies, so changing one classifier without the other fails here
    /// instead of silently handing two different labels for the same record to
    /// two different lanes. The legacy function's `other` results are exempt:
    /// [`TrustClass`] deliberately classifies more kinds (`Diagnostic`,
    /// `ScanCoverage`, `SemanticDrift`, …) than it does.
    #[test]
    fn derived_and_legacy_trust_vocabularies_do_not_drift() {
        let kinds = [
            NodeKind::Repository,
            NodeKind::File,
            NodeKind::Module,
            NodeKind::Symbol,
            NodeKind::Import,
            NodeKind::Diagnostic,
            NodeKind::PanicRiskSite,
            NodeKind::DebtMarker,
            NodeKind::UnsafeSite,
            NodeKind::DependencyDeclaration,
            NodeKind::ScanCoverage,
            NodeKind::Commit,
            NodeKind::Change,
            NodeKind::SemanticDrift,
            NodeKind::EmbeddingModel,
            NodeKind::EmbeddingVector,
            NodeKind::Agent,
            NodeKind::AgentSession,
            NodeKind::Observation,
            NodeKind::Task,
            NodeKind::AcceptanceCriterion,
            NodeKind::ExternalLink,
            NodeKind::Product,
            NodeKind::Project,
            NodeKind::Plan,
            NodeKind::GitHubIssue,
            NodeKind::PR,
            NodeKind::Review,
            NodeKind::ExternalIdentity,
            NodeKind::ReviewStateTransition,
            NodeKind::LocalTask,
            NodeKind::Artifact,
            NodeKind::Verification,
            NodeKind::CommandEvidence,
            NodeKind::AgentRun,
            NodeKind::AgentTurn,
            NodeKind::ToolCall,
            NodeKind::CommandRun,
            NodeKind::FileEdit,
            NodeKind::PatchArtifact,
            NodeKind::Failure,
            NodeKind::Decision,
            NodeKind::TestRun,
            NodeKind::CIStatus,
            NodeKind::BenchmarkRun,
            NodeKind::CoverageReport,
            NodeKind::ProofResult,
            NodeKind::PromoteCandidate,
            NodeKind::PromotionPrompt,
            NodeKind::PromotionDecision,
            NodeKind::Preference,
            NodeKind::WorkflowRule,
            NodeKind::NamingDecision,
            NodeKind::Constraint,
            NodeKind::CostUsage,
            NodeKind::Retraction,
            NodeKind::LogSource,
            NodeKind::ErrorSignature,
            NodeKind::LogEvent,
            NodeKind::LogOccurrenceBucket,
        ];
        let records: Vec<GraphRecord> = kinds
            .iter()
            .map(|kind| node(format!("id:{kind:?}"), *kind))
            .collect();
        let index = TrustIndex::build(&records);

        for record in &records {
            let legacy = crate::cli::trust_class_for(record);
            let derived = index.classify(record);
            let expected = match legacy {
                "source_fact" => Some(TrustClass::SourceDerived),
                "verification_evidence" => Some(TrustClass::VerificationEvidence),
                // An agent claim with no evidence and no contradiction is
                // `agent_unverified`; that is the bare-record baseline here.
                "agent_authored" => Some(TrustClass::AgentUnverified),
                "project_state" => Some(TrustClass::ProjectState),
                "artifact" => Some(TrustClass::Artifact),
                "runtime_observation" => Some(TrustClass::RuntimeObservation),
                // The legacy classifier leaves these unclassified; the derived
                // one is allowed to be more specific.
                "other" => None,
                unexpected => panic!("unknown legacy trust class: {unexpected}"),
            };
            if let Some(expected) = expected {
                assert_eq!(
                    derived, expected,
                    "derived and legacy trust classes disagree for {record:?}"
                );
            }
        }
    }

    #[test]
    fn classification_is_repeatable() {
        let passing = run("pass", Some("passed"), None);
        let mut claim = observation("repeat");
        cite(&mut claim, "VALIDATED_BY", passing.id());
        let records = vec![passing, claim.clone()];
        let first = TrustIndex::build(&records).classify(&claim);
        for _ in 0..8 {
            assert_eq!(TrustIndex::build(&records).classify(&claim), first);
        }
    }
}
