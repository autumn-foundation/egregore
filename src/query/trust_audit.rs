//! Producer trust-class audit (issue #236).
//!
//! Read-only verification that every persisted record's `producer_kind` is
//! consistent with the trust class its node kind implies:
//!
//! - **code-fact** kinds (deterministic source facts — code graph, history,
//!   drift, and log-signature kinds) may only be written by a
//!   **deterministic**-class producer;
//! - **agent-authored** kinds (agent-memory, verification, user-context,
//!   project, and artifact kinds) may only be written by an
//!   **agent**-class producer.
//!
//! The rule set is documented in `docs/schema/producer-version.md` §10; the
//! classifier here ([`node_trust_class`], [`producer_trust_class`]) is its
//! executable twin — the two must not drift. Like [`TrustIndex::classify`]
//! (issue #114), [`node_trust_class`] matches every [`NodeKind`] variant with
//! **no wildcard arm**, so a future kind fails to compile until it is
//! deliberately placed in a trust class and can never silently inherit one.
//!
//! Audit semantics, per the documented contract:
//!
//! - nodes are audited against their own kind;
//! - edges are audited against the trust class implied by their **source**
//!   node's kind, resolved in-batch last-write-wins via
//!   [`GraphRecord::resolve_node_kind_in_batch`] — the exact semantics of the
//!   daemon's `lookup_node_kind`;
//! - tombstones are audited against the trust class implied by their
//!   **target** record's kind (same in-batch resolution);
//! - an edge or tombstone whose endpoint kind cannot be resolved in the batch
//!   is **not** a trust-class violation — it is skipped (referential
//!   integrity is `eg validate`'s job) — **unless** its producer is
//!   [`ProducerKind::Other`], which fails closed regardless of resolvability;
//! - [`ProducerKind::Other`] (unknown or future variants this binary cannot
//!   classify) is always a violation: an unclassifiable producer fails closed,
//!   never silently passes;
//! - legacy records (`producer: None`) are exempt — a record that predates
//!   the producer envelope cannot be retroactively known to come from a
//!   specific producer.
//!
//! Violations are returned in canonical order — `(record_id, rule)` with
//! byte-wise string ordering — so repeated runs over an unchanged store are
//! byte-identical. A violation is a citable record (`record_id`, `kind`,
//! `producer_kind`, the repo-relative handle/span where applicable, and the
//! broken rule), never synthesized prose.

use serde::Serialize;

use crate::ir::{GraphRecord, NodeKind, ProducerKind, SourceSpan};

/// Trust class implied by a node kind: deterministic source facts versus
/// agent-authored content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeTrustClass {
    /// Deterministic code-fact kinds: may only be written by a
    /// deterministic-class producer.
    CodeFact,
    /// Agent-authored kinds: may only be written by an agent-class producer.
    AgentAuthored,
}

/// Trust class of a producer kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerTrustClass {
    /// Deterministic producers: extraction, history replay, cache refresh,
    /// log-signature import, drift measurement.
    Deterministic,
    /// Agent-class producers: observation, trajectory, and task writers.
    Agent,
}

/// Rule id: a `code_fact` kind written by an `agent`-class producer.
///
/// The headline corruption case: agent-authored content masquerading as
/// deterministic source truth.
pub const RULE_CODE_FACT_WRITTEN_BY_AGENT_PRODUCER: &str = "code_fact_written_by_agent_producer";

/// Rule id: an `agent_authored` kind written by a `deterministic`-class
/// producer.
///
/// The extractor must never author observations, decisions, or project state.
pub const RULE_AGENT_AUTHORED_WRITTEN_BY_DETERMINISTIC_PRODUCER: &str =
    "agent_authored_written_by_deterministic_producer";

/// Rule id: `producer_kind: other` — an unknown or future variant this
/// binary cannot classify.
///
/// Flagged fail-closed: an unclassifiable producer is a violation, never a
/// silent pass.
pub const RULE_UNCLASSIFIABLE_PRODUCER_KIND: &str = "unclassifiable_producer_kind";

/// Classifies a producer kind into its trust class.
///
/// Returns `None` for [`ProducerKind::Other`]: an unknown or future variant
/// this binary cannot classify, which the audit flags fail-closed
/// ([`RULE_UNCLASSIFIABLE_PRODUCER_KIND`]).
#[must_use]
pub const fn producer_trust_class(kind: ProducerKind) -> Option<ProducerTrustClass> {
    match kind {
        ProducerKind::CodeGraphExtractor
        | ProducerKind::HistoryReplay
        | ProducerKind::IncrementalCache
        | ProducerKind::LogImporter
        | ProducerKind::DocImporter
        | ProducerKind::DriftEngine => Some(ProducerTrustClass::Deterministic),
        ProducerKind::TrajImporter
        | ProducerKind::CodexImporter
        | ProducerKind::ClaudeCodeImporter
        | ProducerKind::ObservationWriter
        | ProducerKind::TaskWriter => Some(ProducerTrustClass::Agent),
        ProducerKind::Other => None,
    }
}

/// Classifies a node kind into its trust class: `code_fact` for deterministic
/// source facts, `agent_authored` for everything else.
///
/// Matches every [`NodeKind`] variant with **no wildcard arm** (mirroring
/// [`TrustIndex::classify`], issue #114): adding a `NodeKind` variant fails
/// to compile until it is deliberately placed here, so a future kind can
/// never silently inherit a trust class. The partition is the executable twin
/// of `docs/schema/producer-version.md` §10 — the two must not drift.
#[must_use]
pub const fn node_trust_class(kind: NodeKind) -> NodeTrustClass {
    match kind {
        // ── Deterministic code-fact kinds ──────────────────────────────────
        NodeKind::Repository
        | NodeKind::File
        | NodeKind::Module
        | NodeKind::Symbol
        | NodeKind::Import
        | NodeKind::Diagnostic
        | NodeKind::PanicRiskSite
        | NodeKind::DebtMarker
        | NodeKind::UnsafeSite
        | NodeKind::LintSuppression
        | NodeKind::DependencyDeclaration
        | NodeKind::ScanCoverage
        | NodeKind::HistoryReplayWindow
        | NodeKind::HistoryReplayTip
        | NodeKind::Commit
        | NodeKind::Change
        | NodeKind::SemanticDrift
        | NodeKind::EmbeddingModel
        | NodeKind::EmbeddingVector
        | NodeKind::LogSource
        | NodeKind::ErrorSignature
        | NodeKind::LogEvent
        | NodeKind::LogOccurrenceBucket => NodeTrustClass::CodeFact,
        // ── Agent-authored kinds ───────────────────────────────────────────
        NodeKind::Agent
        | NodeKind::AgentSession
        | NodeKind::Observation
        | NodeKind::AgentRun
        | NodeKind::AgentTurn
        | NodeKind::ToolCall
        | NodeKind::CommandRun
        | NodeKind::FileEdit
        | NodeKind::PatchArtifact
        | NodeKind::Adr
        | NodeKind::Prd
        | NodeKind::PlanDoc
        | NodeKind::Failure
        | NodeKind::Decision
        | NodeKind::CostUsage
        | NodeKind::TestRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult
        | NodeKind::PromoteCandidate
        | NodeKind::PromotionPrompt
        | NodeKind::PromotionDecision
        | NodeKind::Preference
        | NodeKind::WorkflowRule
        | NodeKind::NamingDecision
        | NodeKind::Constraint
        | NodeKind::Retraction
        | NodeKind::RetirementReceipt
        | NodeKind::ReinstatementReceipt
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
        | NodeKind::Artifact
        | NodeKind::Verification
        | NodeKind::CommandEvidence => NodeTrustClass::AgentAuthored,
    }
}

// ── Violations ─────────────────────────────────────────────────────────────

/// One producer trust-class violation: a citable record naming the
/// offending record and the rule it broke.
///
/// Carries the `record_id`, node `kind`, `producer_kind`, and the
/// repo-relative handle/span where applicable — never synthesized prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustViolation {
    /// Stable record ID of the offending record — the citable handle.
    pub record_id: String,
    /// `"node"`, `"edge"`, or `"tombstone"`.
    pub record_type: &'static str,
    /// Node kind (nodes), edge label (edges), or target record kind
    /// (tombstones). Absent only when a tombstone's target kind cannot be
    /// resolved in the batch and its producer is `other` (fail-closed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The offending record's serialized producer kind.
    pub producer_kind: &'static str,
    /// The broken rule id.
    pub rule: &'static str,
    /// Repo-relative handle, when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span, when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Edge source node ID (edges only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Edge target node ID (edges only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// Tombstone target record ID (tombstones only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_id: Option<String>,
}

/// The trust-class audit outcome: canonical-ordered violations plus tallies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustAuditReport {
    /// Violations in canonical order: `(record_id, rule)`, byte-wise.
    pub violations: Vec<TrustViolation>,
    /// Nodes + edges + tombstones examined (legacy and unresolvable records
    /// are examined too — they are exempt or skipped, not invisible).
    pub records_scanned: usize,
    /// Node records examined.
    pub nodes_scanned: usize,
    /// Edge records examined.
    pub edges_scanned: usize,
    /// Tombstone records examined.
    pub tombstones_scanned: usize,
    /// Records with no producer envelope: exempt, never violations.
    pub legacy_records_skipped: usize,
}

/// Audits one record against the trust class its (possibly endpoint-implied)
/// node kind implies, returning its violation (if any).
///
/// `implied_kind`: the node's own kind, or the in-batch-resolved kind of the
/// edge's source node / tombstone's target record — `None` when the endpoint
/// kind cannot be resolved. An unresolvable endpoint is skipped (not a
/// violation) unless `producer_kind` is [`ProducerKind::Other`], which fails
/// closed regardless of resolvability.
fn audit_record(
    record: &GraphRecord,
    producer_kind: ProducerKind,
    implied_kind: Option<NodeKind>,
) -> Option<TrustViolation> {
    // Unclassifiable producer kinds fail closed — before any kind comparison,
    // and regardless of endpoint resolvability.
    let producer_class = match producer_trust_class(producer_kind) {
        Some(class) => class,
        None => {
            return Some(violation_for(
                record,
                implied_kind,
                RULE_UNCLASSIFIABLE_PRODUCER_KIND,
            ));
        }
    };
    let kind = implied_kind?;
    let node_class = node_trust_class(kind);
    let rule = match (node_class, producer_class) {
        (NodeTrustClass::CodeFact, ProducerTrustClass::Agent) => {
            RULE_CODE_FACT_WRITTEN_BY_AGENT_PRODUCER
        }
        (NodeTrustClass::AgentAuthored, ProducerTrustClass::Deterministic) => {
            RULE_AGENT_AUTHORED_WRITTEN_BY_DETERMINISTIC_PRODUCER
        }
        (NodeTrustClass::CodeFact, ProducerTrustClass::Deterministic)
        | (NodeTrustClass::AgentAuthored, ProducerTrustClass::Agent) => return None,
    };
    Some(violation_for(record, Some(kind), rule))
}

/// Builds the citable violation record for `record` under `rule`.
fn violation_for(
    record: &GraphRecord,
    kind: Option<NodeKind>,
    rule: &'static str,
) -> TrustViolation {
    let producer_kind = record
        .producer()
        .map_or(ProducerKind::Other.as_str(), |p| p.producer_kind.as_str());
    match record {
        GraphRecord::Node {
            id,
            repo_relative_path,
            span,
            ..
        } => TrustViolation {
            record_id: id.clone(),
            record_type: "node",
            kind: kind.map(NodeKind::as_str).map(str::to_owned),
            producer_kind,
            rule,
            repo_relative_path: repo_relative_path.clone(),
            span: *span,
            source_id: None,
            target_id: None,
            deleted_id: None,
        },
        GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } => TrustViolation {
            record_id: id.clone(),
            record_type: "edge",
            kind: Some(label.as_str().to_owned()),
            producer_kind,
            rule,
            repo_relative_path: None,
            span: None,
            source_id: Some(source.clone()),
            target_id: Some(target.clone()),
            deleted_id: None,
        },
        GraphRecord::Tombstone { id, deleted_id, .. } => TrustViolation {
            record_id: id.clone(),
            record_type: "tombstone",
            kind: kind.map(NodeKind::as_str).map(str::to_owned),
            producer_kind,
            rule,
            repo_relative_path: None,
            span: None,
            source_id: None,
            target_id: None,
            deleted_id: Some(deleted_id.clone()),
        },
    }
}

/// Audits every record in `records` for producer trust-class consistency.
///
/// Nodes are audited against their own kind; edges against their source
/// node's kind and tombstones against their target record's kind, resolved
/// in-batch last-write-wins ([`GraphRecord::resolve_node_kind_in_batch`] —
/// the exact semantics of the daemon's `lookup_node_kind`). Legacy records
/// (`producer: None`) are exempt. Violations are returned in canonical
/// `(record_id, rule)` byte-wise order, so repeated runs over an unchanged
/// store are byte-identical.
#[must_use]
pub fn trust_audit(records: &[GraphRecord]) -> TrustAuditReport {
    let mut report = TrustAuditReport {
        violations: Vec::new(),
        records_scanned: 0,
        nodes_scanned: 0,
        edges_scanned: 0,
        tombstones_scanned: 0,
        legacy_records_skipped: 0,
    };
    for record in records {
        report.records_scanned += 1;
        // Per-type totals count every record examined — including legacy
        // records, which are exempt from the audit but were still scanned.
        match record {
            GraphRecord::Node { .. } => report.nodes_scanned += 1,
            GraphRecord::Edge { .. } => report.edges_scanned += 1,
            GraphRecord::Tombstone { .. } => report.tombstones_scanned += 1,
        }
        let Some(producer) = record.producer() else {
            report.legacy_records_skipped += 1;
            continue;
        };
        let producer_kind = producer.producer_kind;
        // The implied kind: the node's own kind, or the in-batch-resolved
        // kind of the edge's source / tombstone's target.
        let implied_kind: Option<NodeKind> = match record {
            GraphRecord::Node { kind, .. } => Some(*kind),
            GraphRecord::Edge { source, .. } => {
                GraphRecord::resolve_node_kind_in_batch(source, records).flatten()
            }
            GraphRecord::Tombstone { deleted_id, .. } => {
                GraphRecord::resolve_node_kind_in_batch(deleted_id, records).flatten()
            }
        };
        if let Some(violation) = audit_record(record, producer_kind, implied_kind) {
            report.violations.push(violation);
        }
    }
    report
        .violations
        .sort_by(|a, b| (a.record_id.as_str(), a.rule).cmp(&(b.record_id.as_str(), b.rule)));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{EdgeLabel, SCHEMA_VERSION};

    fn producer_kind_envelope(kind: ProducerKind) -> crate::ir::Producer {
        crate::ir::Producer {
            egregore_version: "0.0.0-trust-audit-unit".to_owned(),
            egregore_git: None,
            producer_kind: kind,
            producer_components: std::collections::BTreeMap::new(),
            producer_started_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn node(id: &str, kind: NodeKind, producer_kind: Option<ProducerKind>) -> GraphRecord {
        let record = GraphRecord::node(id.to_owned(), kind, None, None, None, "fixture".to_owned());
        match producer_kind {
            Some(k) => record.with_producer(producer_kind_envelope(k)),
            None => record,
        }
    }

    #[test]
    fn producer_classes_cover_every_variant() {
        // Every non-`Other` producer kind is classified; `Other` fails closed.
        for kind in [
            ProducerKind::CodeGraphExtractor,
            ProducerKind::HistoryReplay,
            ProducerKind::IncrementalCache,
            ProducerKind::LogImporter,
            ProducerKind::DriftEngine,
            ProducerKind::TrajImporter,
            ProducerKind::CodexImporter,
            ProducerKind::ClaudeCodeImporter,
            ProducerKind::ObservationWriter,
            ProducerKind::TaskWriter,
        ] {
            assert!(
                producer_trust_class(kind).is_some(),
                "{kind:?} must be classified"
            );
        }
        assert_eq!(producer_trust_class(ProducerKind::Other), None);
        assert_eq!(
            producer_trust_class(ProducerKind::CodeGraphExtractor),
            Some(ProducerTrustClass::Deterministic)
        );
        assert_eq!(
            producer_trust_class(ProducerKind::TaskWriter),
            Some(ProducerTrustClass::Agent)
        );
        // `drift_engine` is deterministic by construction: it writes
        // measurements computed from source bytes plus model bytes.
        assert_eq!(
            producer_trust_class(ProducerKind::DriftEngine),
            Some(ProducerTrustClass::Deterministic)
        );
    }

    #[test]
    fn node_classes_partition_every_node_kind() {
        // The classifier is total over `NodeKind::ALL`: the no-wildcard match
        // fails to compile on a new variant, and this pins the documented
        // partition counts (23 code-fact + 45 agent-authored; the three
        // design-doc kinds from issue #149 are agent-authored artifacts).
        let mut code_fact = 0_usize;
        let mut agent_authored = 0_usize;
        for kind in NodeKind::ALL {
            match node_trust_class(kind) {
                NodeTrustClass::CodeFact => code_fact += 1,
                NodeTrustClass::AgentAuthored => agent_authored += 1,
            }
        }
        assert_eq!(NodeKind::ALL.len(), 68);
        assert_eq!(code_fact, 23, "documented code-fact count");
        assert_eq!(agent_authored, 45, "documented agent-authored count");
        assert_eq!(node_trust_class(NodeKind::Symbol), NodeTrustClass::CodeFact);
        assert_eq!(
            node_trust_class(NodeKind::Observation),
            NodeTrustClass::AgentAuthored
        );
        assert_eq!(
            node_trust_class(NodeKind::LogEvent),
            NodeTrustClass::CodeFact,
            "log-signature kinds are deterministic measurements"
        );
    }

    #[test]
    fn agent_producer_under_code_fact_kind_is_flagged() {
        let records = [node(
            "v:1",
            NodeKind::Symbol,
            Some(ProducerKind::ObservationWriter),
        )];
        let report = trust_audit(&records);
        assert_eq!(report.violations.len(), 1);
        let violation = &report.violations[0];
        assert_eq!(violation.record_id, "v:1");
        assert_eq!(violation.record_type, "node");
        assert_eq!(violation.kind.as_deref(), Some("Symbol"));
        assert_eq!(violation.producer_kind, "observation_writer");
        assert_eq!(violation.rule, RULE_CODE_FACT_WRITTEN_BY_AGENT_PRODUCER);
    }

    #[test]
    fn deterministic_producer_under_agent_kind_is_flagged() {
        let records = [node(
            "v:2",
            NodeKind::Observation,
            Some(ProducerKind::CodeGraphExtractor),
        )];
        let report = trust_audit(&records);
        assert_eq!(report.violations.len(), 1);
        assert_eq!(
            report.violations[0].rule,
            RULE_AGENT_AUTHORED_WRITTEN_BY_DETERMINISTIC_PRODUCER
        );
        assert_eq!(report.violations[0].kind.as_deref(), Some("Observation"));
    }

    #[test]
    fn other_producer_fails_closed_even_when_class_would_match() {
        // `Other` is unclassifiable — flagged even on a kind where any
        // classified producer class could be consistent.
        let records = [node("v:3", NodeKind::Symbol, Some(ProducerKind::Other))];
        let report = trust_audit(&records);
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].rule, RULE_UNCLASSIFIABLE_PRODUCER_KIND);
        assert_eq!(report.violations[0].producer_kind, "other");
    }

    #[test]
    fn consistent_pairs_are_silent() {
        // `Vec`, not a stack array: six `GraphRecord`s exceed clippy's
        // 16 KiB `large_stack_arrays` threshold.
        let records = vec![
            node(
                "c:1",
                NodeKind::Symbol,
                Some(ProducerKind::CodeGraphExtractor),
            ),
            node("c:2", NodeKind::Commit, Some(ProducerKind::HistoryReplay)),
            node("c:3", NodeKind::LogEvent, Some(ProducerKind::LogImporter)),
            node(
                "c:4",
                NodeKind::SemanticDrift,
                Some(ProducerKind::DriftEngine),
            ),
            node("c:5", NodeKind::Observation, Some(ProducerKind::TaskWriter)),
            node("c:6", NodeKind::Task, Some(ProducerKind::ObservationWriter)),
        ];
        let report = trust_audit(&records);
        assert!(
            report.violations.is_empty(),
            "consistent producer classes must stay silent"
        );
        assert_eq!(report.records_scanned, 6);
        assert_eq!(report.nodes_scanned, 6);
        assert_eq!(report.legacy_records_skipped, 0);
    }

    #[test]
    fn legacy_records_are_exempt_never_violations() {
        let records = [node("l:1", NodeKind::Symbol, None)];
        let report = trust_audit(&records);
        assert!(report.violations.is_empty());
        assert_eq!(report.legacy_records_skipped, 1);
        assert_eq!(report.records_scanned, 1);
    }

    #[test]
    fn legacy_records_still_count_in_per_type_totals() {
        // Legacy records are exempt from the audit, not invisible: the
        // per-type totals count every record examined.
        let records = [
            node("l:1", NodeKind::Symbol, None),
            node("l:2", NodeKind::Observation, Some(ProducerKind::TaskWriter)),
        ];
        let report = trust_audit(&records);
        assert!(report.violations.is_empty());
        assert_eq!(report.records_scanned, 2);
        assert_eq!(report.nodes_scanned, 2);
        assert_eq!(report.legacy_records_skipped, 1);
    }

    fn edge(
        id_source: &str,
        id_target: &str,
        label: EdgeLabel,
        producer_kind: ProducerKind,
    ) -> GraphRecord {
        GraphRecord::edge(
            label,
            id_source.to_owned(),
            id_target.to_owned(),
            None,
            "fixture edge".to_owned(),
        )
        .with_producer(producer_kind_envelope(producer_kind))
    }

    #[test]
    fn edge_is_audited_against_its_source_node_kind() {
        let records = [
            node(
                "e:src",
                NodeKind::Symbol,
                Some(ProducerKind::CodeGraphExtractor),
            ),
            edge(
                "e:src",
                "e:dst",
                EdgeLabel::Contains,
                ProducerKind::TaskWriter,
            ),
        ];
        let report = trust_audit(&records);
        assert_eq!(report.violations.len(), 1);
        let violation = &report.violations[0];
        assert_eq!(violation.record_type, "edge");
        assert_eq!(violation.kind.as_deref(), Some("CONTAINS"));
        assert_eq!(violation.rule, RULE_CODE_FACT_WRITTEN_BY_AGENT_PRODUCER);
        assert_eq!(violation.source_id.as_deref(), Some("e:src"));
        assert_eq!(violation.target_id.as_deref(), Some("e:dst"));
        assert_eq!(report.edges_scanned, 1);
    }

    #[test]
    fn edge_with_unresolvable_source_is_skipped_not_flagged() {
        let records = [edge(
            "missing:src",
            "missing:dst",
            EdgeLabel::Mentions,
            ProducerKind::ObservationWriter,
        )];
        let report = trust_audit(&records);
        assert!(
            report.violations.is_empty(),
            "unresolvable endpoints are referential-integrity's job, not this audit's"
        );
        assert_eq!(report.edges_scanned, 1);
    }

    #[test]
    fn edge_with_unresolvable_source_and_other_producer_fails_closed() {
        let records = [edge(
            "missing:src",
            "missing:dst",
            EdgeLabel::Mentions,
            ProducerKind::Other,
        )];
        let report = trust_audit(&records);
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].rule, RULE_UNCLASSIFIABLE_PRODUCER_KIND);
        // The edge label is always known — only a tombstone's *target* kind
        // can be unresolvable.
        assert_eq!(report.violations[0].kind.as_deref(), Some("MENTIONS"));
    }

    #[test]
    fn edge_source_resolution_is_last_write_wins() {
        // Two records share the source id; the trailing one shadows — the
        // same semantics as the daemon's `lookup_node_kind`.
        let records = [
            node(
                "w:src",
                NodeKind::Symbol,
                Some(ProducerKind::CodeGraphExtractor),
            ),
            node(
                "w:src",
                NodeKind::Observation,
                Some(ProducerKind::ObservationWriter),
            ),
            edge(
                "w:src",
                "w:dst",
                EdgeLabel::Contains,
                ProducerKind::TaskWriter,
            ),
        ];
        let report = trust_audit(&records);
        // Source resolves to `Observation` (agent-authored); the agent-class
        // edge producer is consistent, so no violation.
        assert!(
            report.violations.is_empty(),
            "last-write-wins source kind must govern the edge audit"
        );
    }

    fn tombstone(id: &str, deleted_id: &str, producer_kind: ProducerKind) -> GraphRecord {
        GraphRecord::Tombstone {
            id: id.to_owned(),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "fixture tombstone".to_owned(),
            producer: None,
        }
        .with_producer(producer_kind_envelope(producer_kind))
    }

    #[test]
    fn tombstone_is_audited_against_its_target_kind() {
        let records = [
            node(
                "t:sym",
                NodeKind::Symbol,
                Some(ProducerKind::CodeGraphExtractor),
            ),
            tombstone("t:tomb", "t:sym", ProducerKind::ObservationWriter),
        ];
        let report = trust_audit(&records);
        assert_eq!(report.violations.len(), 1);
        let violation = &report.violations[0];
        assert_eq!(violation.record_type, "tombstone");
        assert_eq!(violation.kind.as_deref(), Some("Symbol"));
        assert_eq!(violation.rule, RULE_CODE_FACT_WRITTEN_BY_AGENT_PRODUCER);
        assert_eq!(violation.deleted_id.as_deref(), Some("t:sym"));
        assert_eq!(report.tombstones_scanned, 1);
    }

    #[test]
    fn tombstone_with_unresolvable_target_is_skipped_not_flagged() {
        let records = [tombstone(
            "t:orphan",
            "missing:target",
            ProducerKind::ObservationWriter,
        )];
        let report = trust_audit(&records);
        assert!(report.violations.is_empty());
    }

    #[test]
    fn violations_sort_canonically_by_record_id_then_rule() {
        let records = [
            node("z:2", NodeKind::Symbol, Some(ProducerKind::TaskWriter)),
            node(
                "a:1",
                NodeKind::Observation,
                Some(ProducerKind::HistoryReplay),
            ),
            node("m:3", NodeKind::Symbol, Some(ProducerKind::Other)),
        ];
        let report = trust_audit(&records);
        let ids: Vec<&str> = report
            .violations
            .iter()
            .map(|v| v.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a:1", "m:3", "z:2"]);
    }

    #[test]
    fn violation_carries_handle_and_span_where_present() {
        let span = SourceSpan {
            start_byte: 0,
            end_byte: 8,
            start_line: 7,
            end_line: 7,
            start_column: None,
            end_column: None,
        };
        let mut record = GraphRecord::node(
            "h:1".to_owned(),
            NodeKind::Symbol,
            Some("src/main.rs".to_owned()),
            Some(span),
            None,
            "fixture".to_owned(),
        );
        record = record.with_producer(producer_kind_envelope(ProducerKind::TaskWriter));
        let report = trust_audit(std::slice::from_ref(&record));
        assert_eq!(report.violations.len(), 1);
        let violation = &report.violations[0];
        assert_eq!(violation.repo_relative_path.as_deref(), Some("src/main.rs"));
        assert_eq!(violation.span, Some(span));
        // No synthesized prose anywhere in the violation.
        let json = serde_json::to_string(violation).expect("violation serializes");
        assert!(json.contains("\"record_id\":\"h:1\""));
        assert!(json.contains("\"rule\":\"code_fact_written_by_agent_producer\""));
    }
}
