//! Derived trust labels for cross-domain context answers (issue #114).
//!
//! A record's `domain` says *where a record lives*; it does not say *how much
//! uncorroborated agent judgement you must accept to believe the row*. Context
//! answers deliberately return code facts, agent observations, and verification
//! evidence side by side, so that distinction has to be carried on the row
//! itself — otherwise the cheapest path for a consuming agent is to treat every
//! row as equally true.
//!
//! [`TrustLabel`] is the closed five-value vocabulary; [`TrustContext`] derives
//! it deterministically from one record slice. Every transport (CLI, daemon,
//! MCP) calls the same derivation over the same slice, so answers cannot
//! disagree.
//!
//! The full contract — vocabulary, derivation table, what counts as a supporting
//! link, what counts as passing, and what is deliberately *not* labeled — is
//! documented in `docs/schema/trust-labels.md`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ir::{EdgeLabel, GraphRecord, NodeKind};
use crate::temporal_status::TemporalResolver;

/// The closed, derived trust vocabulary carried by every record row of a
/// cross-domain context answer (issue #114).
///
/// Distinct from the pre-existing `trust_class` domain vocabulary, which is
/// retained unchanged and answers a different question. See
/// `docs/schema/trust-labels.md` §1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLabel {
    /// Deterministically derived from a source artifact rather than asserted by
    /// an agent. NOT a claim that the content is true, current, or verified.
    SourceDerived,
    /// A recorded verification-domain execution.
    VerificationEvidence,
    /// An agent-authored claim with at least one live, direct, supporting link
    /// to a passing verification record at this snapshot.
    AgentVerified,
    /// An agent-authored claim with no such supporting link. Absence of recorded
    /// corroboration — never evidence that the claim is wrong.
    AgentUnverified,
    /// An agent-authored claim acted on by a live `CONTRADICTS`/`SUPERSEDES`
    /// relationship at this snapshot.
    AgentContradicted,
}

impl TrustLabel {
    /// Every value in the closed vocabulary, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::SourceDerived,
        Self::VerificationEvidence,
        Self::AgentVerified,
        Self::AgentUnverified,
        Self::AgentContradicted,
    ];

    /// The stable wire string for this label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceDerived => "source_derived",
            Self::VerificationEvidence => "verification_evidence",
            Self::AgentVerified => "agent_verified",
            Self::AgentUnverified => "agent_unverified",
            Self::AgentContradicted => "agent_contradicted",
        }
    }
}

/// Evidence-link relations that can carry *supporting* verification evidence.
///
/// Mirrors `is_verified_claim` in [`super::memory_audit`]: a generic link that
/// merely happens to point at a verification record confers nothing.
const SUPPORTING_RELATIONS: [&str; 3] = ["VALIDATED_BY", "HAS_EVIDENCE", "PRODUCED_EVIDENCE"];

/// Verification `status` values that count as passing, ASCII-lowercased and
/// trimmed before comparison.
///
/// `pass` is canonical (`docs/schema/verification.md`, the `eg evidence`
/// write-path validation, and `is_pass_status` in [`super::failure_history`]);
/// `passed` is the one alias that occurs in this repository's own data.
/// Synonyms that appear in no schema and no writer (`success`, `ok`, `green`)
/// are deliberately excluded — inventing them risks reading a non-pass as a pass.
const PASS_STATUSES: [&str; 2] = ["pass", "passed"];

/// Maps a record to its **domain** trust class — *where the record lives*.
///
/// This is the pre-existing seven-value vocabulary (`source_fact`,
/// `agent_authored`, `verification_evidence`, `project_state`, `artifact`,
/// `runtime_observation`, `other`) consumed by `eg audit citations`,
/// `eg query memory`, `eg query sessions`, and the log lanes. It is deliberately
/// **not** the same question as [`TrustLabel`], which reports how much
/// uncorroborated agent judgement a row requires; see
/// `docs/schema/trust-labels.md` §1.
///
/// Canonical home for the mapping so the answer surfaces and the CLI cannot
/// drift; `crate::cli::trust_class_for` delegates here.
#[must_use]
pub fn trust_class_for(record: &GraphRecord) -> &'static str {
    let Some(kind) = record.node_kind_name() else {
        return "other";
    };
    match kind {
        "Observation" | "Decision" | "Failure" | "Lesson" => "agent_authored",
        "Verification" | "CommandEvidence" | "CommandRun" | "TestRun" | "CIStatus"
        | "BenchmarkRun" | "CoverageReport" | "ProofResult" => "verification_evidence",
        "File"
        | "Symbol"
        | "Module"
        | "Import"
        | "Commit"
        | "Change"
        | "Repository"
        | "PanicRiskSite"
        | "DebtMarker"
        | "UnsafeSite"
        | "DependencyDeclaration" => "source_fact",
        "Task"
        | "AcceptanceCriterion"
        | "LocalTask"
        | "GitHubIssue"
        | "PR"
        | "Review"
        | "ExternalIdentity"
        | "ReviewStateTransition"
        | "ExternalLink"
        | "Product"
        | "Project"
        | "Plan" => "project_state",
        "Artifact" | "PatchArtifact" | "FileEdit" => "artifact",
        // Runtime log-signature observations (issues #319 / #320): a program's
        // own claim about its execution, deterministically parsed but never
        // verified — never source truth or verification evidence.
        "LogSource" | "ErrorSignature" | "LogEvent" | "LogOccurrenceBucket" => {
            "runtime_observation"
        }
        _ => "other",
    }
}

/// How a [`NodeKind`] participates in trust derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KindClass {
    /// A verification-domain execution record.
    Verification,
    /// An agent-authored claim: the only class that can reach an `agent_*` label.
    AgentClaim,
    /// Everything else — deterministic, not interposed by agent judgement.
    SourceDerived,
}

/// Classifies a [`NodeKind`] for trust derivation.
///
/// Exhaustive with **no wildcard arm** (the issue #247 completeness invariant):
/// a new `NodeKind` fails to compile until it is deliberately classified, so the
/// vocabulary can never silently drift open.
const fn kind_class(kind: NodeKind) -> KindClass {
    match kind {
        // ── verification domain ───────────────────────────────────────────────
        NodeKind::Verification
        | NodeKind::CommandEvidence
        | NodeKind::TestRun
        | NodeKind::CommandRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult => KindClass::Verification,

        // ── agent-authored claims ─────────────────────────────────────────────
        // Observation/Decision/Failure are the claim kinds a context answer
        // returns. The run/turn/tool-call/identity kinds are agent-memory
        // records recovered from the agent's own transcript; they are relay
        // nodes that no context section returns today, but classifying them
        // here keeps the fail-closed direction if that ever changes.
        NodeKind::Observation
        | NodeKind::Decision
        | NodeKind::Failure
        | NodeKind::Agent
        | NodeKind::AgentSession
        | NodeKind::AgentRun
        | NodeKind::AgentTurn
        | NodeKind::ToolCall => KindClass::AgentClaim,

        // ── everything else is deterministically source-derived ───────────────
        // code graph
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
        // semantic
        | NodeKind::SemanticDrift
        | NodeKind::EmbeddingModel
        | NodeKind::EmbeddingVector
        // project
        | NodeKind::Task
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
        | NodeKind::LocalTask
        // artifact
        | NodeKind::Artifact
        | NodeKind::FileEdit
        | NodeKind::PatchArtifact
        // user context / policy
        | NodeKind::PromoteCandidate
        | NodeKind::PromotionPrompt
        | NodeKind::PromotionDecision
        | NodeKind::Preference
        | NodeKind::WorkflowRule
        | NodeKind::NamingDecision
        | NodeKind::Constraint
        // operational
        | NodeKind::CostUsage
        | NodeKind::Retraction
        // runtime log observations (their `trust_class` stays runtime_observation)
        | NodeKind::LogSource
        | NodeKind::ErrorSignature
        | NodeKind::LogEvent
        | NodeKind::LogOccurrenceBucket => KindClass::SourceDerived,
    }
}

/// Returns `true` when a verification-domain record counts as **passing**.
///
/// See `docs/schema/trust-labels.md` §6 for the full table. `status` wins when
/// present; the `exit_code == 0` fallback exists because the trajectory
/// importers record only an exit code. Unknown is never a pass.
fn verification_is_passing(record: &GraphRecord) -> bool {
    let GraphRecord::Node {
        status, exit_code, ..
    } = record
    else {
        return false;
    };
    match status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => PASS_STATUSES.contains(&s.to_ascii_lowercase().as_str()),
        None => *exit_code == Some(0),
    }
}

/// Snapshot-scoped index used to derive [`TrustLabel`] for records of one answer.
///
/// Built once per answer from the same record slice the answer was computed
/// over, so the label is "at the queried snapshot" by construction. Every index
/// is a `BTree*` — never a `HashMap`/`HashSet` — so derivation is
/// order-independent and byte-stable across runs.
pub struct TrustContext<'a> {
    /// Verification records that are live and passing at this snapshot.
    passing_verifications: BTreeSet<&'a str>,
    /// Record ID → the targets of its outgoing supporting-relation edges.
    supporting_edge_targets: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// Supersession/contradiction resolver over the same slice.
    resolver: TemporalResolver<'a>,
}

impl<'a> TrustContext<'a> {
    /// Builds the derivation index over one record slice.
    #[must_use]
    pub fn build(records: &'a [GraphRecord]) -> Self {
        let resolver = TemporalResolver::build(records);

        let tombstoned: BTreeSet<&str> = records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
                _ => None,
            })
            .collect();

        // A verification record confers `agent_verified` only when it is live
        // (not tombstoned, not itself superseded/contradicted) and passing.
        let passing_verifications: BTreeSet<&str> = records
            .iter()
            .filter_map(|r| {
                let GraphRecord::Node { id, kind, .. } = r else {
                    return None;
                };
                if kind_class(*kind) != KindClass::Verification {
                    return None;
                }
                if tombstoned.contains(id.as_str()) {
                    return None;
                }
                if resolver.resolve_status(id.as_str()).0 != "current" {
                    return None;
                }
                verification_is_passing(r).then_some(id.as_str())
            })
            .collect();

        let mut supporting_edge_targets: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for record in records {
            let GraphRecord::Edge {
                label,
                source,
                target,
                ..
            } = record
            else {
                continue;
            };
            // Forward direction only (claim → verification). Backward traversal
            // would let two claims validated by the same run verify each other —
            // the hazard `is_forward_only_label` documents.
            if matches!(
                label,
                EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence | EdgeLabel::ProducedEvidence
            ) {
                supporting_edge_targets
                    .entry(source.as_str())
                    .or_default()
                    .insert(target.as_str());
            }
        }

        Self {
            passing_verifications,
            supporting_edge_targets,
            resolver,
        }
    }

    /// Derives the trust label for one record.
    ///
    /// A non-node record (edge or tombstone) is reported [`TrustLabel::SourceDerived`];
    /// no answer surface labels those rows (see `docs/schema/trust-labels.md` §7),
    /// so this is a defensive default rather than a rendered value.
    #[must_use]
    pub fn label_for(&self, record: &GraphRecord) -> TrustLabel {
        let GraphRecord::Node { id, kind, .. } = record else {
            return TrustLabel::SourceDerived;
        };
        match kind_class(*kind) {
            KindClass::Verification => TrustLabel::VerificationEvidence,
            KindClass::SourceDerived => TrustLabel::SourceDerived,
            KindClass::AgentClaim => {
                // Contradiction beats verification: a stale green run never
                // rescues a superseded claim, and an unresolvable supersession
                // cycle is reported contradicted rather than merely unverified.
                if matches!(
                    self.resolver.resolve_status(id.as_str()).0,
                    "superseded" | "contradicted" | "cycle"
                ) {
                    return TrustLabel::AgentContradicted;
                }
                if self.has_supporting_passing_link(record) {
                    TrustLabel::AgentVerified
                } else {
                    TrustLabel::AgentUnverified
                }
            }
        }
    }

    /// Returns `true` when the claim carries at least one live, direct,
    /// forward supporting link to a passing verification record.
    ///
    /// An existential over sorted iteration, so a claim carrying both a passing
    /// and a failing link resolves the same way regardless of link order.
    fn has_supporting_passing_link(&self, record: &GraphRecord) -> bool {
        let GraphRecord::Node {
            id, evidence_links, ..
        } = record
        else {
            return false;
        };

        // Representation 1: on-node evidence links.
        if let Some(links) = evidence_links.as_deref() {
            let on_node: BTreeSet<&str> = links
                .iter()
                .filter(|l| SUPPORTING_RELATIONS.contains(&l.relation.as_str()))
                .filter_map(|l| l.target_record_id.as_deref())
                .collect();
            if on_node
                .iter()
                .any(|t| self.passing_verifications.contains(t))
            {
                return true;
            }
        }

        // Representation 2: edge records.
        self.supporting_edge_targets
            .get(id.as_str())
            .is_some_and(|targets| {
                targets
                    .iter()
                    .any(|t| self.passing_verifications.contains(t))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::EvidenceLink;

    fn node(id: &str, kind: NodeKind) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("{kind:?} {id}"),
        )
    }

    fn verification(id: &str, status: Option<&str>, exit_code: Option<i64>) -> GraphRecord {
        let mut r = node(id, NodeKind::TestRun);
        if let GraphRecord::Node {
            status: ref mut s,
            exit_code: ref mut e,
            ..
        } = r
        {
            *s = status.map(str::to_owned);
            *e = exit_code;
        }
        r
    }

    fn link(relation: &str, target: &str) -> EvidenceLink {
        EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: "verification".to_owned(),
            relation: relation.to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    fn with_links(mut record: GraphRecord, links: Vec<EvidenceLink>) -> GraphRecord {
        if let GraphRecord::Node {
            ref mut evidence_links,
            ..
        } = record
        {
            *evidence_links = Some(links);
        }
        record
    }

    fn label(records: &[GraphRecord], id: &str) -> TrustLabel {
        let ctx = TrustContext::build(records);
        let record = records
            .iter()
            .find(|r| r.id() == id)
            .expect("record present in fixture");
        ctx.label_for(record)
    }

    // ── AC1: closed vocabulary ────────────────────────────────────────────────

    #[test]
    fn trust_label_vocabulary_is_exactly_five_closed_values() {
        assert_eq!(TrustLabel::ALL.len(), 5);
        let strings: Vec<&str> = TrustLabel::ALL.iter().map(|l| l.as_str()).collect();
        assert_eq!(
            strings,
            vec![
                "source_derived",
                "verification_evidence",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
            ]
        );
        // serde must agree with as_str, since both reach the wire.
        for l in TrustLabel::ALL {
            let json = serde_json::to_string(&l).expect("serialize");
            assert_eq!(json, format!("\"{}\"", l.as_str()));
        }
        let unique: BTreeSet<&str> = strings.iter().copied().collect();
        assert_eq!(unique.len(), 5, "values must be distinct");
    }

    // ── AC2: non-agent records are never labeled with an agent class ──────────

    #[test]
    fn codegraph_symbol_is_source_derived() {
        let records = vec![node("codegraph:v8:sym", NodeKind::Symbol)];
        assert_eq!(
            label(&records, "codegraph:v8:sym"),
            TrustLabel::SourceDerived
        );
    }

    #[test]
    fn semantic_drift_record_is_source_derived() {
        let records = vec![node("semantic:v1:drift", NodeKind::SemanticDrift)];
        assert_eq!(
            label(&records, "semantic:v1:drift"),
            TrustLabel::SourceDerived
        );
    }

    #[test]
    fn verification_record_is_verification_evidence() {
        let records = vec![
            verification("verification:v1:t", Some("pass"), None),
            node("verification:v1:c", NodeKind::CommandRun),
        ];
        assert_eq!(
            label(&records, "verification:v1:t"),
            TrustLabel::VerificationEvidence
        );
        assert_eq!(
            label(&records, "verification:v1:c"),
            TrustLabel::VerificationEvidence
        );
    }

    #[test]
    fn a_failing_verification_record_is_still_verification_evidence() {
        // Pass/fail decides whether it can *confer* agent_verified; it never
        // changes the verification record's own class.
        let records = vec![verification("verification:v1:red", Some("fail"), Some(1))];
        assert_eq!(
            label(&records, "verification:v1:red"),
            TrustLabel::VerificationEvidence
        );
    }

    #[test]
    fn project_artifact_and_log_records_are_never_labeled_agent() {
        let records = vec![
            node("project:v1:task", NodeKind::Task),
            node("project:v1:pr", NodeKind::PR),
            node("artifact:v1:patch", NodeKind::PatchArtifact),
            node("agent_memory:v1:edit", NodeKind::FileEdit),
            node("log:v3:sig", NodeKind::ErrorSignature),
        ];
        for id in [
            "project:v1:task",
            "project:v1:pr",
            "artifact:v1:patch",
            "agent_memory:v1:edit",
            "log:v3:sig",
        ] {
            let l = label(&records, id);
            assert_eq!(l, TrustLabel::SourceDerived, "{id} must be source_derived");
            assert!(
                !matches!(
                    l,
                    TrustLabel::AgentVerified
                        | TrustLabel::AgentUnverified
                        | TrustLabel::AgentContradicted
                ),
                "{id} must never carry an agent trust class"
            );
        }
    }

    // ── AC3: the three agent resolutions ──────────────────────────────────────

    #[test]
    fn observation_without_evidence_link_is_agent_unverified() {
        let records = vec![node("agent_memory:v1:obs", NodeKind::Observation)];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn observation_with_passing_verification_link_is_agent_verified() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("pass"), None),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentVerified
        );
    }

    #[test]
    fn observation_verified_through_an_edge_record_too() {
        let records = vec![
            node("agent_memory:v1:obs", NodeKind::Observation),
            verification("verification:v1:t", Some("pass"), None),
            GraphRecord::edge(
                EdgeLabel::ValidatedBy,
                "agent_memory:v1:obs".to_owned(),
                "verification:v1:t".to_owned(),
                Some("1.0".to_owned()),
                "validated".to_owned(),
            ),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentVerified
        );
    }

    #[test]
    fn observation_with_contradicts_edge_is_agent_contradicted() {
        let records = vec![
            node("agent_memory:v1:obs", NodeKind::Observation),
            node("agent_memory:v1:other", NodeKind::Observation),
            GraphRecord::edge(
                EdgeLabel::Contradicts,
                "agent_memory:v1:other".to_owned(),
                "agent_memory:v1:obs".to_owned(),
                Some("1.0".to_owned()),
                "contradicts".to_owned(),
            ),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentContradicted
        );
    }

    #[test]
    fn observation_with_supersedes_edge_is_agent_contradicted() {
        let records = vec![
            node("agent_memory:v1:old", NodeKind::Observation),
            node("agent_memory:v1:new", NodeKind::Observation),
            GraphRecord::edge(
                EdgeLabel::Supersedes,
                "agent_memory:v1:new".to_owned(),
                "agent_memory:v1:old".to_owned(),
                Some("1.0".to_owned()),
                "supersedes".to_owned(),
            ),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:old"),
            TrustLabel::AgentContradicted
        );
        assert_eq!(
            label(&records, "agent_memory:v1:new"),
            TrustLabel::AgentUnverified,
            "the superseding record itself is current, not contradicted"
        );
    }

    #[test]
    fn observation_with_superseded_by_field_is_agent_contradicted() {
        let mut old = node("agent_memory:v1:old", NodeKind::Observation);
        if let GraphRecord::Node {
            ref mut superseded_by,
            ..
        } = old
        {
            *superseded_by = Some("agent_memory:v1:new".to_owned());
        }
        let records = vec![old, node("agent_memory:v1:new", NodeKind::Observation)];
        assert_eq!(
            label(&records, "agent_memory:v1:old"),
            TrustLabel::AgentContradicted
        );
    }

    #[test]
    fn observation_with_on_node_contradicts_link_is_agent_contradicted() {
        let mut contra = link("CONTRADICTS", "agent_memory:v1:obs");
        contra.target_domain = "agent_memory".to_owned();
        let records = vec![
            node("agent_memory:v1:obs", NodeKind::Observation),
            with_links(
                node("agent_memory:v1:other", NodeKind::Observation),
                vec![contra],
            ),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentContradicted
        );
    }

    // ── Risk guards ───────────────────────────────────────────────────────────

    #[test]
    fn observation_with_failing_verification_stays_agent_unverified() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("fail"), Some(1)),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified,
            "a red run must never read as agent_verified"
        );
    }

    #[test]
    fn status_wins_over_a_zero_exit_code() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("fail"), Some(0)),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn statusless_verification_passes_on_zero_exit_code_only() {
        // The trajectory importers record only an exit code.
        let green = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("VALIDATED_BY", "agent_memory:v1:ver")],
            ),
            {
                let mut r = node("agent_memory:v1:ver", NodeKind::Verification);
                if let GraphRecord::Node {
                    ref mut exit_code, ..
                } = r
                {
                    *exit_code = Some(0);
                }
                r
            },
        ];
        assert_eq!(
            label(&green, "agent_memory:v1:obs"),
            TrustLabel::AgentVerified
        );

        let red = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("VALIDATED_BY", "agent_memory:v1:ver")],
            ),
            {
                let mut r = node("agent_memory:v1:ver", NodeKind::Verification);
                if let GraphRecord::Node {
                    ref mut exit_code, ..
                } = r
                {
                    *exit_code = Some(1);
                }
                r
            },
        ];
        assert_eq!(
            label(&red, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn unknown_or_absent_status_is_never_a_pass() {
        for status in [None, Some(""), Some("   "), Some("weird"), Some("success")] {
            let records = vec![
                with_links(
                    node("agent_memory:v1:obs", NodeKind::Observation),
                    vec![link("HAS_EVIDENCE", "verification:v1:t")],
                ),
                verification("verification:v1:t", status, None),
            ];
            assert_eq!(
                label(&records, "agent_memory:v1:obs"),
                TrustLabel::AgentUnverified,
                "status {status:?} must not count as a pass"
            );
        }
    }

    #[test]
    fn pass_status_is_case_insensitive_and_trimmed() {
        for status in ["pass", "PASS", " Pass ", "passed", "PASSED"] {
            let records = vec![
                with_links(
                    node("agent_memory:v1:obs", NodeKind::Observation),
                    vec![link("HAS_EVIDENCE", "verification:v1:t")],
                ),
                verification("verification:v1:t", Some(status), None),
            ];
            assert_eq!(
                label(&records, "agent_memory:v1:obs"),
                TrustLabel::AgentVerified,
                "status {status:?} must count as a pass"
            );
        }
    }

    #[test]
    fn a_generic_relation_to_a_passing_run_confers_nothing() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("RELATES_TO", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("pass"), None),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn an_agent_claim_is_never_evidence_for_itself() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "agent_memory:v1:other")],
            ),
            node("agent_memory:v1:other", NodeKind::Observation),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn a_backward_link_does_not_confer_verified() {
        // verification --HAS_EVIDENCE--> observation is not the observation
        // being verified; two claims sharing one run must not verify each other.
        let records = vec![
            node("agent_memory:v1:obs", NodeKind::Observation),
            verification("verification:v1:t", Some("pass"), None),
            GraphRecord::edge(
                EdgeLabel::HasEvidence,
                "verification:v1:t".to_owned(),
                "agent_memory:v1:obs".to_owned(),
                Some("1.0".to_owned()),
                "backward".to_owned(),
            ),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn a_tombstoned_verification_does_not_confer_verified() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("pass"), None),
            GraphRecord::Tombstone {
                id: "verification:v1:t:tombstone".to_owned(),
                schema_version: 1,
                deleted_id: "verification:v1:t".to_owned(),
                summary: "retracted".to_owned(),
                producer: None,
            },
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn a_superseded_verification_does_not_confer_verified() {
        let mut old = verification("verification:v1:old", Some("pass"), None);
        if let GraphRecord::Node {
            ref mut superseded_by,
            ..
        } = old
        {
            *superseded_by = Some("verification:v1:new".to_owned());
        }
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:old")],
            ),
            old,
            verification("verification:v1:new", Some("fail"), None),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn a_missing_verification_target_confers_nothing() {
        let records = vec![with_links(
            node("agent_memory:v1:obs", NodeKind::Observation),
            vec![link("HAS_EVIDENCE", "verification:v1:absent")],
        )];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified
        );
    }

    #[test]
    fn contradiction_beats_a_passing_verification() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("pass"), None),
            node("agent_memory:v1:other", NodeKind::Observation),
            GraphRecord::edge(
                EdgeLabel::Contradicts,
                "agent_memory:v1:other".to_owned(),
                "agent_memory:v1:obs".to_owned(),
                Some("1.0".to_owned()),
                "contradicts".to_owned(),
            ),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentContradicted,
            "a stale green run must not rescue a contradicted claim"
        );
    }

    #[test]
    fn a_supersession_cycle_is_agent_contradicted() {
        let records = vec![
            node("agent_memory:v1:a", NodeKind::Observation),
            node("agent_memory:v1:b", NodeKind::Observation),
            GraphRecord::edge(
                EdgeLabel::Supersedes,
                "agent_memory:v1:a".to_owned(),
                "agent_memory:v1:b".to_owned(),
                Some("1.0".to_owned()),
                "supersedes".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::Supersedes,
                "agent_memory:v1:b".to_owned(),
                "agent_memory:v1:a".to_owned(),
                Some("1.0".to_owned()),
                "supersedes".to_owned(),
            ),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:a"),
            TrustLabel::AgentContradicted
        );
    }

    // ── AC4: determinism ──────────────────────────────────────────────────────

    #[test]
    fn label_is_independent_of_link_and_record_order() {
        let mixed = |links: Vec<EvidenceLink>| {
            vec![
                with_links(node("agent_memory:v1:obs", NodeKind::Observation), links),
                verification("verification:v1:red", Some("fail"), None),
                verification("verification:v1:green", Some("pass"), None),
            ]
        };
        let a = mixed(vec![
            link("HAS_EVIDENCE", "verification:v1:red"),
            link("HAS_EVIDENCE", "verification:v1:green"),
        ]);
        let b = mixed(vec![
            link("HAS_EVIDENCE", "verification:v1:green"),
            link("HAS_EVIDENCE", "verification:v1:red"),
        ]);
        assert_eq!(
            label(&a, "agent_memory:v1:obs"),
            TrustLabel::AgentVerified,
            "one passing link is enough regardless of order"
        );
        assert_eq!(
            label(&a, "agent_memory:v1:obs"),
            label(&b, "agent_memory:v1:obs")
        );

        let mut reversed = a.clone();
        reversed.reverse();
        assert_eq!(
            label(&a, "agent_memory:v1:obs"),
            label(&reversed, "agent_memory:v1:obs"),
            "record order must not change the label"
        );
    }

    #[test]
    fn repeated_derivation_over_an_unchanged_slice_is_stable() {
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("pass"), None),
            node("codegraph:v8:sym", NodeKind::Symbol),
        ];
        let first: Vec<TrustLabel> = {
            let ctx = TrustContext::build(&records);
            records.iter().map(|r| ctx.label_for(r)).collect()
        };
        for _ in 0..16 {
            let ctx = TrustContext::build(&records);
            let again: Vec<TrustLabel> = records.iter().map(|r| ctx.label_for(r)).collect();
            assert_eq!(first, again);
        }
    }
}
