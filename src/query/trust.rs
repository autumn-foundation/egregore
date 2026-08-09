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

use super::liveness::Liveness;

/// The closed, derived trust vocabulary carried by every record row of a
/// cross-domain context answer (issue #114).
///
/// Distinct from the pre-existing `trust_class` domain vocabulary, which is
/// retained unchanged and answers a different question. See
/// `docs/schema/trust-labels.md` §1.
// Deliberately NOT `PartialOrd`/`Ord`: the five values are a closed set of
// distinct verdicts, not a ranking. Deriving an ordering would invite a consumer
// to "sort by trust" and silently rank, say, a `source_derived` project row
// against an `agent_verified` observation as if the comparison meant something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
///
/// **This mapping is FROZEN.** It gates `eg audit citations`: the trust class a
/// record resolves to decides which citation handle the audit REQUIRES of it,
/// so adding an arm silently changes that gate. Adding `SemanticDrift` here,
/// for instance, makes the audit demand a path/span handle of every drift row
/// and fails the seeded gate. A kind with no arm falls to `"other"`; a surface
/// that needs a better label for such a kind must supply it locally (see
/// `ContextDrift`) rather than widening this function.
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
    /// An asserted claim — agent-authored, or a (deferred) user-context
    /// assertion. The only class that can reach an `agent_*` label.
    AssertedClaim,
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
        // records recovered from the agent's own transcript; `classify_node`
        // returns `None` for them, so no context section renders them today.
        // They are classified here so that a future section which DID render
        // them would get an `agent_*` verdict rather than `source_derived`.
        // Note this alone is not sufficient protection: a `ToolCall` carries a
        // write-path-valid `PRODUCED_EVIDENCE` edge to the `CommandRun` it
        // produced, so a successful non-test command must not read as a pass —
        // that is what `exit_code_may_stand_in_for_status` prevents.
        NodeKind::Observation
        | NodeKind::Decision
        | NodeKind::Failure
        | NodeKind::Agent
        | NodeKind::AgentSession
        | NodeKind::AgentRun
        | NodeKind::AgentTurn
        | NodeKind::ToolCall
        // User-context policy kinds. Issue #114 explicitly DEFERS promotion /
        // preference trust to the promotion flow, and `classify_node` returns
        // `None` for all of them, so none is rendered by a context answer
        // today. The exhaustive match still forces a choice, and the
        // fail-closed one is this branch: a `Preference` or `Constraint` is an
        // ASSERTION, not something derived from a source artifact, so calling
        // it `source_derived` would over-claim. The honest caveat is that the
        // emitted value reads `agent_unverified`, which understates authorship
        // (these are operator-authored) while correctly stating that nothing
        // corroborates them. Whoever renders them must revisit this.
        | NodeKind::PromoteCandidate
        | NodeKind::PromotionPrompt
        | NodeKind::PromotionDecision
        | NodeKind::Preference
        | NodeKind::WorkflowRule
        | NodeKind::NamingDecision
        | NodeKind::Constraint => KindClass::AssertedClaim,

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

/// Kinds for which a recorded `exit_code == 0` may stand in for a missing
/// `status`.
///
/// Deliberately EXCLUDES `CommandRun`/`CommandEvidence`. The trajectory
/// importers mint a status-less `Verification` only for a *test* command
/// (`src/traj.rs`, gated on `is_test_command`), so exit 0 there really does mean
/// "the check passed" — but `src/codex.rs` and `src/claude_code.rs` mint a
/// `CommandRun` for EVERY shell invocation, so a successful `ls` would otherwise
/// be read as verification. A `CommandRun` must therefore say `pass` explicitly.
const fn exit_code_may_stand_in_for_status(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Verification
            | NodeKind::TestRun
            | NodeKind::CIStatus
            | NodeKind::ProofResult
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
    )
}

/// Returns `true` when a verification-domain record counts as **passing**.
///
/// See `docs/schema/trust-labels.md` §6 for the full table. `status` wins when
/// present; the `exit_code == 0` fallback exists because the trajectory
/// importers record only an exit code, and is restricted to the kinds where a
/// zero exit really is a verification verdict. Unknown is never a pass.
fn verification_is_passing(record: &GraphRecord) -> bool {
    let GraphRecord::Node {
        kind,
        status,
        exit_code,
        ..
    } = record
    else {
        return false;
    };
    let recorded = status.as_deref().map(str::trim).filter(|s| !s.is_empty());
    recorded.map_or_else(
        || exit_code_may_stand_in_for_status(*kind) && *exit_code == Some(0),
        |s| PASS_STATUSES.contains(&s.to_ascii_lowercase().as_str()),
    )
}

/// Snapshot-scoped index used to derive [`TrustLabel`] for records of one answer.
///
/// Built once per answer from the same record slice the answer was computed
/// over, so the label is "at the queried snapshot" by construction.
///
/// Derivation is order-independent and byte-stable across runs. Every index
/// OWNED here is a `BTree*`, and the supporting-link check is an existential
/// over sorted iteration. The embedded [`TemporalResolver`] is `HashMap`-backed,
/// so the guarantee there rests on a different argument: `resolve_status`
/// returns a scalar status whose value is set-determined, not iteration-ordered
/// — its DFS keeps only the current path in the visited set and pops on
/// backtrack, so it is exhaustive over simple paths and reports a cycle iff one
/// is reachable, and its head set is the order-invariant set of successor-less
/// reachable nodes. Its returned reference lists are sorted before use.
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
        let liveness = Liveness::new(records);

        // Latest physical write per node id, so a SUPERSEDED earlier version of a
        // verification record can never confer `agent_verified` on the strength
        // of a result the store has since replaced. Mirrors the embedded
        // current-state read (and `eg query task`'s own `rfind`-by-id), which
        // matters because `eg export` deliberately re-emits superseded versions:
        // without this, `--graph` and `--data-dir` would disagree.
        let mut latest_node_write: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, record) in records.iter().enumerate() {
            if let GraphRecord::Node { id, .. } = record {
                latest_node_write.insert(id.as_str(), index);
            }
        }

        // A verification record confers `agent_verified` only when it is the
        // latest version of its id, live (not tombstoned, not itself
        // superseded/contradicted), and passing.
        let passing_verifications: BTreeSet<&str> = records
            .iter()
            .enumerate()
            .filter_map(|(index, r)| {
                let GraphRecord::Node { id, kind, .. } = r else {
                    return None;
                };
                if kind_class(*kind) != KindClass::Verification {
                    return None;
                }
                if latest_node_write.get(id.as_str()) != Some(&index) {
                    return None;
                }
                if liveness.deleted(id.as_str()) {
                    return None;
                }
                if resolver.resolve_status(id.as_str()).0 != "current" {
                    return None;
                }
                verification_is_passing(r).then_some(id.as_str())
            })
            .collect();

        let mut supporting_edge_targets: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
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
            // Forward direction only (claim → verification). Backward traversal
            // would let two claims validated by the same run verify each other —
            // the hazard `is_forward_only_label` documents.
            if !matches!(
                label,
                EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence | EdgeLabel::ProducedEvidence
            ) {
                continue;
            }
            // The LINK must be live too, not just its target: `eg forget` can
            // retract an evidence-link edge, and a stale earlier version of an
            // edge id must not shadow its current metadata. Mirrors
            // `memory_audit::verification_support_indexes` and the tombstone gate
            // `symbol_context` already applies to its own traversal.
            if !liveness.is_latest_edge_version(id.as_str(), index) || liveness.deleted(id.as_str())
            {
                continue;
            }
            supporting_edge_targets
                .entry(source.as_str())
                .or_default()
                .insert(target.as_str());
        }

        Self {
            passing_verifications,
            supporting_edge_targets,
            resolver,
        }
    }

    /// Derives the trust label for one record.
    ///
    /// No answer surface labels a non-node record — `topology_edges` and
    /// `unresolved` deliberately carry no label (see
    /// `docs/schema/trust-labels.md` §7) — so the edge/tombstone arm is
    /// unreachable in rendered output. It returns the WEAKEST label rather than
    /// the most authoritative one, so a future caller that reaches it
    /// under-claims instead of over-claiming.
    #[must_use]
    pub fn label_for(&self, record: &GraphRecord) -> TrustLabel {
        let GraphRecord::Node { id, kind, .. } = record else {
            return TrustLabel::AgentUnverified;
        };
        match kind_class(*kind) {
            KindClass::Verification => TrustLabel::VerificationEvidence,
            KindClass::SourceDerived => TrustLabel::SourceDerived,
            KindClass::AssertedClaim => {
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
    fn a_superseded_physical_version_of_a_passing_run_confers_nothing() {
        // `eg export` re-emits superseded versions and `capture-tests` keys a
        // TestRun on (session, commit, suite) with status NOT an identity input,
        // so re-running a suite that went red yields two versions of one id.
        // Only the latest write may decide.
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("pass"), None),
            verification("verification:v1:t", Some("fail"), Some(1)),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified,
            "a stale green version must not outvote the current red one"
        );
    }

    #[test]
    fn a_later_passing_version_does_confer_verified() {
        // The mirror of the case above: red first, green latest.
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("HAS_EVIDENCE", "verification:v1:t")],
            ),
            verification("verification:v1:t", Some("fail"), Some(1)),
            verification("verification:v1:t", Some("pass"), None),
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentVerified
        );
    }

    #[test]
    fn a_tombstoned_supporting_edge_confers_nothing() {
        // `eg forget` can retract an evidence-link edge; the LINK's liveness
        // must be checked, not only the target's.
        let edge = GraphRecord::edge(
            EdgeLabel::ValidatedBy,
            "agent_memory:v1:obs".to_owned(),
            "verification:v1:t".to_owned(),
            Some("1.0".to_owned()),
            "validated".to_owned(),
        );
        let edge_id = edge.id().to_owned();
        let records = vec![
            node("agent_memory:v1:obs", NodeKind::Observation),
            verification("verification:v1:t", Some("pass"), None),
            edge,
            GraphRecord::Tombstone {
                id: format!("{edge_id}:tombstone"),
                schema_version: 1,
                deleted_id: edge_id,
                summary: "retracted evidence link".to_owned(),
                producer: None,
            },
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified,
            "a retracted evidence link must not still confer agent_verified"
        );
    }

    #[test]
    fn a_successful_non_test_command_run_is_not_a_pass() {
        // `codex`/`claude_code` mint a status-less CommandRun for EVERY shell
        // invocation, so a successful `ls` must never read as verification.
        let mut cmd = node("agent_memory:v1:cmd", NodeKind::CommandRun);
        if let GraphRecord::Node {
            ref mut exit_code, ..
        } = cmd
        {
            *exit_code = Some(0);
        }
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("PRODUCED_EVIDENCE", "agent_memory:v1:cmd")],
            ),
            cmd,
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentUnverified,
            "exit 0 on a generic CommandRun is not a verification verdict"
        );
    }

    #[test]
    fn an_explicit_pass_status_on_a_command_run_does_confer_verified() {
        // The exit-code fallback is narrowed, not the explicit status.
        let records = vec![
            with_links(
                node("agent_memory:v1:obs", NodeKind::Observation),
                vec![link("VALIDATED_BY", "verification:v1:cmd")],
            ),
            {
                let mut r = node("verification:v1:cmd", NodeKind::CommandRun);
                if let GraphRecord::Node {
                    ref mut status,
                    ref mut exit_code,
                    ..
                } = r
                {
                    *status = Some("pass".to_owned());
                    *exit_code = Some(0);
                }
                r
            },
        ];
        assert_eq!(
            label(&records, "agent_memory:v1:obs"),
            TrustLabel::AgentVerified
        );
    }

    #[test]
    fn trust_class_and_trust_never_disagree_on_authorship() {
        // A row must never serialize {"trust_class":"other","trust":"agent_*"}
        // or {"trust_class":"agent_authored","trust":"source_derived"}.
        // Restricted to the kinds a context section actually renders
        // (`classify_node`), because `trust_class_for` is a frozen contract that
        // reports `other` for kinds outside its arms — including the
        // agent-memory relay kinds, which no answer surfaces.
        for kind in [
            NodeKind::Observation,
            NodeKind::Decision,
            NodeKind::Failure,
            NodeKind::Symbol,
            NodeKind::File,
            NodeKind::Task,
            NodeKind::Artifact,
            NodeKind::PatchArtifact,
            NodeKind::TestRun,
            NodeKind::CommandRun,
            NodeKind::ErrorSignature,
        ] {
            let records = vec![node("agent_memory:v1:x", kind)];
            let derived = label(&records, "agent_memory:v1:x");
            let class = trust_class_for(&records[0]);
            let derived_is_agent = matches!(
                derived,
                TrustLabel::AgentVerified
                    | TrustLabel::AgentUnverified
                    | TrustLabel::AgentContradicted
            );
            assert_eq!(
                derived_is_agent,
                class == "agent_authored",
                "{kind:?} disagrees: trust_class={class}, trust={}",
                derived.as_str()
            );
            assert_ne!(class, "other", "{kind:?} must have a real trust_class");
        }
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
