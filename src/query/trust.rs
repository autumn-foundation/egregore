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

use crate::ir::{GraphRecord, NodeKind};
use crate::temporal_status::TemporalResolver;

use super::memory_audit::{
    OutgoingEdgeIndex, TombstonedSet, backing_verification_records, verification_support_indexes,
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

/// Verification `status` values that count as a **failing** outcome.
///
/// The mirror of [`PASSING_VERIFICATION_STATUSES`], drawn from the free-string
/// vocabulary `docs/schema/verification.md` documents (`pass` / `fail` / `skip` /
/// `error` / `timeout`) plus the spellings the importers emit. `error` and
/// `timeout` are failing because the check *ran and did not succeed*; `skip` is
/// deliberately absent — a skipped check proves nothing either way and must fall
/// through to the inconclusive outcome rather than be reported as a failure.
///
/// Anything outside BOTH sets is [`VerificationOutcome::Inconclusive`]: neither
/// vocabulary is widened by guessing.
const FAILING_VERIFICATION_STATUSES: &[&str] = &[
    "fail",
    "failed",
    "failure",
    "error",
    "errored",
    "timeout",
    "timed_out",
];

/// The recorded outcome of a verification record: the single pass/fail rule this
/// crate applies.
///
/// Shared so the derived-trust classifier (`agent_verified`) and the
/// acceptance-criterion coverage census (`crate::criteria_coverage`, issue #115)
/// cannot drift into two different notions of "passing". A new consumer must
/// call [`verification_outcome`] rather than re-deriving one.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VerificationOutcome {
    /// The recorded outcome is passing.
    Passing,
    /// The recorded outcome is failing.
    Failing,
    /// The outcome is absent or outside both closed vocabularies. **Not**
    /// passing — the deliberate fail-closed fallback.
    Inconclusive,
}

/// Classifies a record's recorded verification outcome.
///
/// `status` wins when present, compared case-insensitively after trimming. When
/// it is absent an `exit_code` decides (`0` passing, non-zero failing) — the same
/// rule `crate::evidence` uses when deriving a status from a command's exit code.
/// With neither field the outcome is [`VerificationOutcome::Inconclusive`].
#[must_use]
pub fn verification_outcome(record: &GraphRecord) -> VerificationOutcome {
    let GraphRecord::Node {
        status, exit_code, ..
    } = record
    else {
        return VerificationOutcome::Inconclusive;
    };
    if let Some(status) = status.as_deref() {
        let normalized = status.trim().to_ascii_lowercase();
        if PASSING_VERIFICATION_STATUSES.contains(&normalized.as_str()) {
            return VerificationOutcome::Passing;
        }
        if FAILING_VERIFICATION_STATUSES.contains(&normalized.as_str()) {
            return VerificationOutcome::Failing;
        }
        return VerificationOutcome::Inconclusive;
    }
    match exit_code {
        Some(0) => VerificationOutcome::Passing,
        Some(_) => VerificationOutcome::Failing,
        None => VerificationOutcome::Inconclusive,
    }
}

/// The closed trust vocabulary attached to every record a cross-domain context
/// answer returns (issue #114).
///
/// See the module documentation for the "domain is not trust" distinction and
/// for what each label does *not* claim.
// Deliberately NOT `Ord`/`PartialOrd`: the variants are a closed set of
// distinct classes, not a ranking. Deriving an ordering would invite reading
// `SourceDerived < AgentVerified` as "less trusted than", which this module
// explicitly does not assert.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
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
    /// Backs the zero-mislabel assertions in this module's tests and in
    /// `tests/integration/trust_class.rs`: a row in an agent-authored section
    /// must be one of these, and a code or verification row must not be. The
    /// guarantee itself is delivered by [`Self::agent_trust`]'s return type,
    /// not by this predicate.
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
/// A separate enum so the body of [`TrustIndex::agent_trust`] — where all the
/// evidence and contradiction logic lives — cannot name `source_derived` or
/// `verification_evidence` at all. The mislabel this issue exists to prevent
/// cannot be written there, rather than only being tested against afterwards.
/// (The one-line dispatch arm in [`TrustIndex::classify`] is still ordinary
/// code; the zero-mislabel tests cover that.)
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
        // Read the evidence links off the slice's latest version of this id, so
        // two physical versions of one claim can never render as two rows with
        // the same `record_id` and different `trust`. Falls back to the record
        // as handed in when it is absent from the slice.
        let current = self.by_id.get(record.id()).copied().unwrap_or(record);
        if self.cites_passing_verification(current) {
            AgentTrust::Verified
        } else {
            AgentTrust::Unverified
        }
    }

    /// Returns `true` when the claim cites at least one live,
    /// **verification-domain**, **passing** record.
    ///
    /// The traversal is [`backing_verification_records`] — the same single
    /// implementation [`super::memory_audit::is_verified_claim`] uses for
    /// `--verified-only`, so the two surfaces cannot disagree about *which*
    /// records back a claim. Two refinements are layered on top, both required
    /// for `agent_verified` to mean anything:
    ///
    /// 1. The backing record must be **passing** (AC3), so a claim whose only
    ///    evidence is a failing run stays `agent_unverified`.
    /// 2. The backing record must live in the **verification domain**, proven by
    ///    its record-ID prefix — not merely carry a verification-shaped
    ///    `NodeKind`.
    ///
    /// Rule 2 closes a self-certification hole. `NodeKind::CommandEvidence` is a
    /// verification-shaped kind, but `eg command-evidence` mints it with an
    /// `agent_memory:v1:` id and derives its pass/fail purely from the
    /// agent-supplied `--exit-code`. Without the domain gate an agent could
    /// write a `CommandEvidence` claiming exit 0, cite it `VALIDATED_BY` from
    /// its own `Observation`, and have Egregore label that observation
    /// `agent_verified` — an agent-authored claim counted as evidence for
    /// itself, the exact standing invariant the citation audit exists to
    /// protect. The gate mirrors the daemon write path's
    /// `validate_evidence_target_domain` rule, applied here because the
    /// `--graph` read path never enforced it.
    ///
    /// A `CommandEvidence` record still appears in the answer's
    /// `verification_evidence` section (its section membership is unchanged);
    /// it simply cannot *confer* trust on the agent that wrote it.
    fn cites_passing_verification(&self, record: &GraphRecord) -> bool {
        backing_verification_records(record, &self.by_id, &self.edges_from, &self.tombstoned)
            .filter(|backing| is_verification_domain_record(backing))
            .any(is_passing_verification)
    }

    /// Borrows the supersession resolver this index already built.
    ///
    /// Callers render `temporal_status` and the `excluded` section from a
    /// [`TemporalResolver`] over the same slice; sharing this one keeps that to a
    /// single O(n) build per answer and makes it *impossible* for the two to be
    /// constructed over different slices — the hazard [`Self::build`] warns
    /// about.
    #[must_use]
    pub const fn resolver(&self) -> &TemporalResolver<'a> {
        &self.resolver
    }
}

/// Record-ID prefix every verification-domain record carries
/// (`crate::ir::verification_stable_id`).
const VERIFICATION_ID_PREFIX: &str = "verification:v";

/// Returns `true` when a record provably lives in the verification domain.
///
/// Proven by the record's own ID prefix rather than by its `NodeKind`, because
/// a verification-shaped kind is not sufficient: `eg command-evidence` mints
/// `NodeKind::CommandEvidence` under an `agent_memory:v1:` id. Only a record
/// whose identity was minted by the verification domain may confer
/// [`TrustClass::AgentVerified`] on an agent claim.
///
/// The prefix is matched version-agnostically (`verification:v<N>:`) so a future
/// verification schema bump keeps conferring trust rather than silently
/// downgrading every verified claim to `agent_unverified`.
#[must_use]
pub fn is_verification_domain_record(record: &GraphRecord) -> bool {
    record
        .id()
        .strip_prefix(VERIFICATION_ID_PREFIX)
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(version, _)| {
            !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Returns `true` when a verification record's recorded outcome is passing.
///
/// A thin reading of the shared [`verification_outcome`] rule, so this predicate
/// and the acceptance-criterion coverage census can never disagree about what
/// "passing" means.
fn is_passing_verification(record: &GraphRecord) -> bool {
    matches!(verification_outcome(record), VerificationOutcome::Passing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, VERIFICATION_SCHEMA_VERSION,
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

    /// A verification-SHAPED kind minted in the agent-memory domain must not
    /// certify the agent that wrote it.
    ///
    /// `eg command-evidence` mints `NodeKind::CommandEvidence` with an
    /// `agent_memory:v1:` id and derives its pass/fail purely from the
    /// agent-supplied `--exit-code`. Counting it would let an agent write its
    /// own proof — an agent-authored claim used as evidence for itself.
    #[test]
    fn agent_minted_command_evidence_cannot_self_certify() {
        let mut self_signed = node(
            agent_memory_stable_id(&["node", "command_evidence", "self"]),
            NodeKind::CommandEvidence,
        );
        if let GraphRecord::Node {
            schema_version,
            status,
            exit_code,
            ..
        } = &mut self_signed
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *status = Some("pass".to_owned());
            *exit_code = Some(0);
        }
        let mut claim = observation("self_certified");
        cite(&mut claim, "VALIDATED_BY", self_signed.id());

        let records = vec![self_signed.clone(), claim.clone()];
        let index = TrustIndex::build(&records);
        assert_eq!(
            index.classify(&claim),
            TrustClass::AgentUnverified,
            "an agent-minted CommandEvidence must not confer agent_verified"
        );
        // Its own section membership is unchanged — it simply confers nothing.
        assert_eq!(
            index.classify(&self_signed),
            TrustClass::VerificationEvidence
        );

        // The same claim backed by a genuine verification-domain record does
        // resolve to agent_verified, so the gate is on domain, not on kind.
        let genuine = run("genuine", Some("pass"), None);
        let mut backed = observation("domain_backed");
        cite(&mut backed, "VALIDATED_BY", genuine.id());
        let records = vec![genuine, backed.clone()];
        assert_eq!(
            TrustIndex::build(&records).classify(&backed),
            TrustClass::AgentVerified
        );
    }

    /// A retracted `CONTRADICTS` edge stops displacing its target: the
    /// relationship is no longer live at the queried snapshot.
    #[test]
    fn retracted_contradiction_no_longer_displaces() {
        let claim = observation("still_current");
        let rebuttal = observation("withdrawn_rebuttal");
        let edge = GraphRecord::agent_memory_edge(
            EdgeLabel::Contradicts,
            rebuttal.id().to_owned(),
            claim.id().to_owned(),
            None,
            "contradicts".to_owned(),
        );
        let edge_id = edge.id().to_owned();

        // While the edge is live the claim is displaced.
        let live = vec![claim.clone(), rebuttal, edge];
        assert_eq!(
            TrustIndex::build(&live).classify(&claim),
            TrustClass::AgentContradicted
        );

        // Retracting the edge withdraws the relationship.
        let mut retracted = live;
        retracted.push(GraphRecord::Tombstone {
            id: format!("{edge_id}:tombstone"),
            schema_version: AGENT_MEMORY_SCHEMA_VERSION,
            deleted_id: edge_id,
            summary: "withdrawn".to_owned(),
            producer: None,
        });
        assert_eq!(
            TrustIndex::build(&retracted).classify(&claim),
            TrustClass::AgentUnverified,
            "a retracted CONTRADICTS edge must stop displacing its target"
        );
    }

    /// An append-only `--graph` that REWRITES a non-temporal node to drop its
    /// `CONTRADICTS` link must stop displacing the target.
    ///
    /// Neither physical version is tombstoned, so a liveness check alone does
    /// not catch this: the stale earlier version has to lose to the later one by
    /// node-version selection, exactly as edges do. The embedded `--data-dir`
    /// read keeps only the latest node write, so without this the two transports
    /// would disagree.
    #[test]
    fn rewritten_node_dropping_its_contradiction_no_longer_displaces() {
        let claim = observation("target_claim");

        // v1 of the rebuttal contradicts the claim; v2 (same id) withdraws it.
        let mut rebuttal_v1 = observation("rebuttal");
        let GraphRecord::Node { evidence_links, .. } = &mut rebuttal_v1 else {
            panic!("node expected");
        };
        *evidence_links = Some(vec![EvidenceLink {
            target_record_id: Some(claim.id().to_owned()),
            target_domain: "agent_memory".to_owned(),
            relation: "CONTRADICTS".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
        let rebuttal_v2 = observation("rebuttal");
        assert_eq!(
            rebuttal_v1.id(),
            rebuttal_v2.id(),
            "both versions must share one stable id for this to be a rewrite"
        );

        // v1 alone displaces the claim.
        let before = vec![claim.clone(), rebuttal_v1.clone()];
        assert_eq!(
            TrustIndex::build(&before).classify(&claim),
            TrustClass::AgentContradicted
        );

        // Appending v2 (which carries no link) withdraws the relationship.
        let after = vec![claim.clone(), rebuttal_v1, rebuttal_v2];
        assert_eq!(
            TrustIndex::build(&after).classify(&claim),
            TrustClass::AgentUnverified,
            "a stale earlier node version must not resurrect a withdrawn relationship"
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

            // A new verification kind forces a `classify` arm (the exhaustive
            // match will not compile otherwise), but nothing forces the
            // `is_verification_kind` edit that `backing_verification_records`
            // gates on. Without this assertion a record could be labelled
            // `verification_evidence` while claims citing it stayed
            // `agent_unverified`.
            assert_eq!(
                derived == TrustClass::VerificationEvidence,
                super::super::memory_audit::record_node_kind(record)
                    .is_some_and(super::super::memory_audit::is_verification_kind),
                "is_verification_kind and classify disagree for {record:?}"
            );
        }
    }

    /// The two outcome vocabularies must stay disjoint, and everything outside
    /// both must fail closed to `Inconclusive`.
    ///
    /// [`verification_outcome`] is shared with the acceptance-criterion coverage
    /// census (`crate::criteria_coverage`, issue #115), which buckets `Passing`
    /// as *proven* and `Failing` as *failed evidence*. A status landing in both
    /// sets would make those buckets order-dependent; a status silently widening
    /// the passing set would let the census overclaim proof.
    #[test]
    fn passing_and_failing_status_vocabularies_are_disjoint_and_fail_closed() {
        for status in PASSING_VERIFICATION_STATUSES {
            assert!(
                !FAILING_VERIFICATION_STATUSES.contains(status),
                "{status} is in both outcome vocabularies"
            );
        }
        for (status, expected) in [
            ("pass", VerificationOutcome::Passing),
            ("PASSED", VerificationOutcome::Passing),
            ("  success  ", VerificationOutcome::Passing),
            ("fail", VerificationOutcome::Failing),
            ("Error", VerificationOutcome::Failing),
            ("timeout", VerificationOutcome::Failing),
            // A skipped check proves nothing either way — never a failure.
            ("skip", VerificationOutcome::Inconclusive),
            ("inconclusive", VerificationOutcome::Inconclusive),
            ("", VerificationOutcome::Inconclusive),
        ] {
            assert_eq!(
                verification_outcome(&run("v", Some(status), None)),
                expected,
                "status {status:?}"
            );
        }
        // Exit codes decide only when `status` is absent.
        assert_eq!(
            verification_outcome(&run("v", None, Some(0))),
            VerificationOutcome::Passing
        );
        assert_eq!(
            verification_outcome(&run("v", None, Some(1))),
            VerificationOutcome::Failing
        );
        assert_eq!(
            verification_outcome(&run("v", None, None)),
            VerificationOutcome::Inconclusive
        );
        // An explicit status always wins over a conflicting exit code.
        assert_eq!(
            verification_outcome(&run("v", Some("fail"), Some(0))),
            VerificationOutcome::Failing
        );
    }

    /// `is_passing_verification` must be exactly the `Passing` reading of the
    /// shared rule, so the trust classifier and the criteria census can never
    /// disagree about what "passing" means.
    #[test]
    fn is_passing_verification_is_the_shared_rule() {
        for status in [
            Some("pass"),
            Some("passed"),
            Some("success"),
            Some("fail"),
            Some("skip"),
            Some("weird"),
            None,
        ] {
            for code in [None, Some(0), Some(3)] {
                let record = run("v", status, code);
                assert_eq!(
                    is_passing_verification(&record),
                    verification_outcome(&record) == VerificationOutcome::Passing,
                    "status={status:?} exit_code={code:?}"
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
