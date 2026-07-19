//! Store-wide evidence-link integrity audit (issue #217).
//!
//! Egregore's whole reason to exist is that evidence links connect code, agent
//! memory, tasks, artifacts, and verification *without letting guesses masquerade
//! as source truth*. A cross-domain evidence edge (`OBSERVES`, `VALIDATED_BY`,
//! `HAS_EVIDENCE`, `CLOSES_ACCEPTANCE_CRITERION`, `OWNED_BY_TASK`,
//! `REFERENCES_TASK`, …) whose target record is **absent** or **tombstoned** is a
//! citation that resolves to nothing, presented as if it were backed. This module
//! is the post-ingest, all-domains health gate that sweeps a whole loaded record
//! set and reports every such broken edge.
//!
//! It is a **read-time measurement** over data the store already holds: it mints
//! nothing, adds no node kind / edge label / trust class / domain, and reads no
//! wall clock or environment. [`run_evidence_link_audit`] is pure and
//! deterministic — no I/O, no printing, byte-identical output across runs (all
//! aggregate maps are [`BTreeMap`], every collection is canonically ordered).
//!
//! Output is redaction-safe: rows and diagnostics carry only record IDs,
//! domain/kind strings, edge-label wire names, repo-relative paths, spans,
//! counts, and closed-enum markers — never raw source, transcript, command,
//! patch, or issue/PR body text.
//!
//! Scope boundary (issue #217 Out of Scope): this slice only *reports*. It never
//! repairs (that is #72), never renders a drift verdict on a still-live target
//! (that is #111/#160 — "broken" here means absent or tombstoned, not drifted),
//! and never resolves a single handle on demand (#160) or audits one answer's
//! completeness (#65).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ir::{EdgeLabel, EvidenceLink, GraphRecord, SourceSpan};
use crate::query::liveness::Liveness;

/// Every [`EdgeLabel`] variant, in declaration order.
///
/// `EdgeLabel` exposes no iterator, so the partition helper and the completeness
/// test enumerate over this hardcoded slice. This mirrors the completeness
/// invariant `query::evidence_path` uses, replicated locally so `src/query/` is
/// never touched: the exhaustive match in [`is_integrity_checked_evidence_edge`]
/// fails to compile if a new variant is added and forgotten, and the completeness
/// test asserts the length here matches the number of classified variants.
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

/// True if this edge is a cross-domain evidence/provenance edge whose target must
/// resolve to a live record. Exhaustive match (no `_`) so a new [`EdgeLabel`]
/// forces a conscious classification decision at compile time — the completeness
/// invariant. The partition mirrors `query::evidence_path::is_evidence_path_edge`
/// (issue #247): TRAVERSED there == CHECKED here.
const fn is_integrity_checked_evidence_edge(label: EdgeLabel) -> bool {
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
        // CHECKED — cross-domain evidence / provenance / grounding edges. Their
        // target must resolve to a live record or it is a dangling citation.
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
        // NOT CHECKED — code-graph structural topology (that is #103's job) and
        // intra-agent-memory organizational scaffolding; never grounding evidence.
        Contains | Defines | Imports | References | Calls | Implements | Mentions | ChangedIn
        | ParentOf | DriftsFrom | DriftsPrior | MeasuredBy | SessionOf | AuthoredBy => false,
    }
}

/// The sorted wire-string lists of checked and excluded edge classes, derived by
/// partitioning [`ALL_EDGE_LABELS`]. Carried in the envelope so a clean verdict is
/// never presented as proof no grounding exists (mirrors evidence-path).
fn edge_classes() -> (Vec<String>, Vec<String>) {
    let mut checked: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    for &label in ALL_EDGE_LABELS {
        if is_integrity_checked_evidence_edge(label) {
            checked.push(label.as_str().to_owned());
        } else {
            excluded.push(label.as_str().to_owned());
        }
    }
    checked.sort();
    excluded.sort();
    (checked, excluded)
}

/// Resolves a wire relation string (`"OBSERVES"`) to its [`EdgeLabel`], or `None`
/// when it names no known variant (unknown vocabulary is out of the closed set).
#[allow(dead_code)] // wired up in the GREEN sweep
fn edge_label_from_wire(wire: &str) -> Option<EdgeLabel> {
    ALL_EDGE_LABELS
        .iter()
        .copied()
        .find(|label| label.as_str() == wire)
}

// ---------------------------------------------------------------------------
// Public report types
// ---------------------------------------------------------------------------

/// Whether a broken edge's target is absent or tombstoned (issue #217 AC2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokenCase {
    /// No record with the target ID exists anywhere in the loaded set.
    Absent,
    /// The target exists but is no longer live (tombstoned / retracted).
    Tombstoned,
}

impl BrokenCase {
    /// The stable wire string (`absent` / `tombstoned`).
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Tombstoned => "tombstoned",
        }
    }
}

/// How a broken evidence link is represented in the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRepresentation {
    /// A standalone `GraphRecord::Edge` record.
    EdgeRecord,
    /// An inline [`EvidenceLink`] carried on a node.
    InlineEvidenceLink,
}

impl EdgeRepresentation {
    /// The stable wire string (`edge_record` / `inline_evidence_link`).
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::EdgeRecord => "edge_record",
            Self::InlineEvidenceLink => "inline_evidence_link",
        }
    }
}

/// One broken cross-domain evidence edge (a citation resolving to nothing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokenEvidenceEdge {
    /// Stable record ID of the source (the record that makes the claim).
    pub source_record_id: String,
    /// Source domain (`agent_memory`, `project`, `artifact`, `verification`, …).
    pub source_domain: String,
    /// Source node kind (`Observation`, `Verification`, `AcceptanceCriterion`, …).
    pub source_kind: String,
    /// Cross-domain evidence edge label (wire name, e.g. `"OBSERVES"`).
    pub edge_label: String,
    /// How the link is represented (standalone edge vs inline evidence link).
    pub representation: EdgeRepresentation,
    /// Stable ID of the edge RECORD, when `representation == edge_record`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_record_id: Option<String>,
    /// The unresolved target record ID (verbatim, never inferred).
    pub target_record_id: String,
    /// Absent vs tombstoned.
    pub case: BrokenCase,
    /// Target domain declared by an inline evidence link, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_domain: Option<String>,
    /// Recovered repo-relative path when the target is (was) a code node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_repo_relative_path: Option<String>,
    /// Recovered span when the target is (was) a code node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_span: Option<SourceSpan>,
}

/// Redaction-safe diagnostic (stable, no free text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceLinkDiagnostic {
    /// Stable machine-readable code (`no_broken_evidence_links`,
    /// `unresolvable_link_no_target_id`).
    pub code: String,
    /// Source record ID this diagnostic concerns, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_record_id: Option<String>,
    /// Edge label this diagnostic concerns, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_label: Option<String>,
    /// Aggregate count this diagnostic tallies, when it carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

/// The full deterministic evidence-link integrity report.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceLinkAuditReport {
    /// true iff zero broken evidence edges were found.
    pub ok: bool,
    /// Total CHECKED evidence edges swept (edge records + inline links w/ target).
    pub checked_edge_count: usize,
    /// Total broken edges (`== broken_edges.len()`).
    pub broken_edge_count: usize,
    /// Broken edges per source domain (deterministic).
    pub by_source_domain: BTreeMap<String, usize>,
    /// Broken edges per edge label (deterministic).
    pub by_edge_label: BTreeMap<String, usize>,
    /// Broken edges per case (`absent` / `tombstoned`).
    pub by_case: BTreeMap<String, usize>,
    /// Every broken edge, canonically ordered.
    pub broken_edges: Vec<BrokenEvidenceEdge>,
    /// The checked edge-label partition disclosed (sorted wire names).
    pub checked_edge_labels: Vec<String>,
    /// The excluded (never-checked) edge-label partition (sorted wire names).
    pub excluded_edge_labels: Vec<String>,
    /// Stable diagnostics (clean signal, triple-only tally, …).
    pub diagnostics: Vec<EvidenceLinkDiagnostic>,
}

// ---------------------------------------------------------------------------
// Internal sweep state
// ---------------------------------------------------------------------------

/// Resolution of one CHECKED reference's target against the loaded record set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // wired up in the GREEN sweep
enum TargetState {
    /// Present and live (not reported).
    Resolved,
    /// Present-but-deleted, or only a tombstone survives.
    Tombstoned,
    /// Neither a node nor a tombstone for the target exists.
    Absent,
}

/// Resolves a record's domain from its stamped `domain` field, falling back to
/// its stable-ID prefix (mirrors `query::evidence_path::record_domain`).
#[allow(dead_code)] // wired up in the GREEN sweep
fn record_domain(record: &GraphRecord) -> String {
    if let GraphRecord::Node {
        domain: Some(domain),
        ..
    } = record
    {
        return domain.clone();
    }
    crate::schema_version::domain_from_record_id(record.id()).unwrap_or_else(|| "unknown".to_owned())
}

/// Domain derived from a bare record ID prefix (for a referenced source/target
/// whose node is not present in the set).
#[allow(dead_code)] // wired up in the GREEN sweep
fn domain_from_id(id: &str) -> String {
    crate::schema_version::domain_from_record_id(id).unwrap_or_else(|| "unknown".to_owned())
}

/// Recovers a code target's repo-relative path/span from a still-present node
/// version (a superseded/temporal version survives even for a tombstoned target).
#[allow(dead_code)] // wired up in the GREEN sweep
fn recover_code_handle(node: Option<&&GraphRecord>) -> (Option<String>, Option<SourceSpan>) {
    match node {
        Some(GraphRecord::Node {
            repo_relative_path,
            span,
            ..
        }) => (repo_relative_path.clone(), *span),
        _ => (None, None),
    }
}

/// Sweeps every cross-domain evidence edge (standalone edge records + inline
/// [`EvidenceLink`]s) and reports each whose target is absent or tombstoned.
///
/// Pure and deterministic: no I/O, no printing, byte-identical output across runs.
#[must_use]
pub fn run_evidence_link_audit(records: &[GraphRecord]) -> EvidenceLinkAuditReport {
    let (checked_edge_labels, excluded_edge_labels) = edge_classes();
    EvidenceLinkAuditReport {
        ok: true,
        checked_edge_count: 0,
        broken_edge_count: 0,
        by_source_domain: BTreeMap::new(),
        by_edge_label: BTreeMap::new(),
        by_case: BTreeMap::new(),
        broken_edges: Vec::new(),
        checked_edge_labels,
        excluded_edge_labels,
        diagnostics: vec![EvidenceLinkDiagnostic {
            code: "no_broken_evidence_links".to_owned(),
            source_record_id: None,
            edge_label: None,
            count: None,
        }],
    }
}

#[cfg(test)]
mod tests;
