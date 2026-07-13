//! Pre-ingest referential-integrity validation for code-graph JSONL (issue #103).
//!
//! One read-only pass over an already-parsed record set that asserts the graph
//! is *referentially closed*: every edge endpoint resolves to a present node,
//! typed edges target nodes of an allowed kind, and tombstones do not conflict
//! with live records. It validates structural reference closure only — not
//! parse correctness, semantic accuracy, schema-version compatibility, or
//! whether extraction was complete.
//!
//! Diagnostics are redaction-safe by construction: they carry record IDs,
//! stable defect categories, relation labels, repo-relative paths, spans, and
//! counts — never record summaries, source text, or payload content.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::github::records::{SOURCE_KIND_PR, SOURCE_KIND_REVIEW};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

/// Stable defect category: an edge endpoint that resolves to no node record
/// and no tombstone in the graph.
pub const DANGLING_EDGE_ENDPOINT: &str = "dangling_edge_endpoint";
/// Stable defect category: a typed edge whose target node is present but of a
/// disallowed kind for the relation.
pub const EDGE_TARGET_KIND_VIOLATION: &str = "edge_target_kind_violation";
/// Stable defect category: a typed edge whose source node is present but of a
/// disallowed kind for the relation (issue #327).
///
/// The source-side companion to `edge_target_kind_violation`. The log schema
/// frames every log structural edge directionally, so a schema-correct target
/// with a wrong-kind source — e.g. a `LogOccurrenceBucket —CAPTURED_FROM→
/// LogSource` — is invalid attribution the pre-ingest gate must reject.
pub const EDGE_SOURCE_KIND_VIOLATION: &str = "edge_source_kind_violation";
/// Stable defect category: a reviewer-identity edge whose source node is of the
/// correct kind but carries the wrong (or no) importer `source_kind`
/// attribution (issue #369).
///
/// The finer companion to `edge_source_kind_violation`: that category constrains
/// the source *node kind*, while this one constrains the importer-origin
/// `source_kind` STRING the node carries. The daemon's `validate_project_edge`
/// gates `REVIEWED_BY` on a `github_review` Review source and
/// `REQUESTED_REVIEW_FROM` on a `github_pr` Task source via
/// `require_project_edge_source_kind`; a kind-correct but mis-attributed source
/// (e.g. a `github_issue` Task, or a hand-authored Review with no `source_kind`)
/// is a binding the daemon rejects, so the offline pre-ingest gate must reject
/// it too. Fires only when the source node kind is already valid for the
/// relation, so a wrong-kind source is reported once as
/// `edge_source_kind_violation`, never doubly.
pub const EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION: &str = "edge_source_kind_attribution_violation";
/// Stable defect category: an edge endpoint that resolves only to a tombstone
/// (the record is deleted and no node record with the same ID supersedes it).
pub const EDGE_TO_TOMBSTONED_RECORD: &str = "edge_to_tombstoned_record";
/// Stable defect category: a topology node with no incident edge, invisible
/// to edge-walking queries such as `eg query file`.
pub const ORPHAN_NODE: &str = "orphan_node";
/// Stable defect category: a tombstone whose deleted record is still
/// referenced by at least one live edge.
pub const TOMBSTONE_STRANDS_LIVE_EDGE: &str = "tombstone_strands_live_edge";
/// Stable defect category: a `DependencyDeclaration` with edges but no
/// `File —CONTAINS→` attribution edge from its declaring manifest
/// (PR #314 review).
///
/// The containment chain is what repository scoping walks; the containing
/// `File`'s repo-relative path must equal the dependency node's declared
/// manifest handle — containment by a source file or a different manifest
/// is the same missing chain. Zero-edge nodes are `orphan_node`, so this
/// category covers nodes whose edges never include that containment.
pub const MISSING_CONTAINMENT_EDGE: &str = "missing_containment_edge";
/// Stable defect category: a log-domain node with incident edges missing a
/// required outbound structural edge (issue #327).
///
/// A `LogEvent` requires one `FINGERPRINTED_AS` and one `CAPTURED_FROM`; an
/// `ErrorSignature` requires at least one `CAPTURED_FROM`; a
/// `LogOccurrenceBucket` requires one `AGGREGATES`. Zero-edge log nodes are
/// `orphan_node`, so this covers only incident nodes.
pub const MISSING_LOG_STRUCTURAL_EDGE: &str = "missing_log_structural_edge";
/// Stable defect category: a log-domain node carrying more than one of an
/// exactly-one required outbound structural edge (issue #327).
///
/// For example a `LogEvent` with two distinct `FINGERPRINTED_AS` targets, or a
/// `LogOccurrenceBucket` with two distinct `AGGREGATES` targets. The count is by
/// distinct edge record ID. This fires only for the exactly-one requirements
/// (`LogEvent`'s `FINGERPRINTED_AS` and `LogOccurrenceBucket`'s `AGGREGATES`).
/// It never fires for a `LogEvent`'s or an `ErrorSignature`'s `CAPTURED_FROM`,
/// which is at-least-one: the event/signature ID excludes the source, so one
/// node may legitimately be captured from multiple `LogSource`s (a graph
/// combining two log files — a merged multi-source aggregate, never a
/// duplicate).
pub const DUPLICATE_LOG_STRUCTURAL_EDGE: &str = "duplicate_log_structural_edge";

/// Node kinds that must be reachable through at least one edge.
///
/// `Repository` is the containment root and `Diagnostic` markers legitimately
/// stand alone (extractor warnings carry no edges); `Commit`/`Change` records
/// are always edge-attached by the history producer, and non-code-graph kinds
/// are out of scope for code-graph reference closure.
/// `DependencyDeclaration` facts are always emitted with their manifest
/// `File —CONTAINS→ DependencyDeclaration` attribution edge — the ownership
/// chain `--repo` scoping walks — so an unattached one is a defect
/// (PR #314 review).
/// `LogEvent`, `LogOccurrenceBucket`, and `ErrorSignature` are always emitted
/// attached to their `LogSource` (a signature via its own `CAPTURED_FROM`;
/// issue #319/#327), so an edge-less one is a defect. `LogSource` (a root/sink
/// that may be legitimately edge-less on an empty-log scan) is intentionally
/// excluded to avoid false positives.
const ORPHANABLE_KINDS: [NodeKind; 8] = [
    NodeKind::File,
    NodeKind::Module,
    NodeKind::Symbol,
    NodeKind::Import,
    NodeKind::DependencyDeclaration,
    NodeKind::LogEvent,
    NodeKind::LogOccurrenceBucket,
    NodeKind::ErrorSignature,
];

/// Allowed target node kinds for the typed code-graph relations checked by
/// issue #103, matching what the extractor and history replay actually emit.
const fn allowed_target_kinds(label: EdgeLabel) -> Option<&'static [NodeKind]> {
    match label {
        EdgeLabel::Defines => Some(&[NodeKind::Symbol]),
        EdgeLabel::Contains => Some(&[
            NodeKind::Change,
            NodeKind::Commit,
            // Repository —CONTAINS→ Diagnostic attributes skipped-manifest
            // coverage holes to their repository (issue #180).
            NodeKind::Diagnostic,
            // `File CONTAINS DebtMarker` attributes debt-comment markers
            // (issue #218) to their owning file.
            NodeKind::DebtMarker,
            // Manifest File —CONTAINS→ DependencyDeclaration attaches Cargo
            // dependency facts to their repository topology (issue #180).
            NodeKind::DependencyDeclaration,
            NodeKind::File,
            NodeKind::Module,
            // `File CONTAINS PanicRiskSite`: unwrap/expect panic-risk call
            // sites are contained by their owning file (issue #223).
            NodeKind::PanicRiskSite,
            // `File` CONTAINS `UnsafeSite` attributes unsafe-surface sites to
            // their owning file (issue #222).
            NodeKind::UnsafeSite,
        ]),
        EdgeLabel::Calls | EdgeLabel::Mentions => Some(&[NodeKind::Diagnostic, NodeKind::Symbol]),
        EdgeLabel::Imports => Some(&[NodeKind::Import]),
        // A `Review` may only be anchored to a `Commit` (issue #334). The AC
        // requires this Review→Commit target rule even though the PR-side
        // `MERGED_AS` is intentionally absent here.
        EdgeLabel::ReviewsCommit => Some(&[NodeKind::Commit]),
        // ── Log-signature domain (issues #319 / #322 / #327) ─────────────────
        // `LogEvent —FINGERPRINTED_AS→ ErrorSignature` (an exemplar is
        // fingerprinted as one signature) and `LogOccurrenceBucket —AGGREGATES→
        // ErrorSignature` (an hourly bucket aggregates one signature) both
        // target `ErrorSignature` only (docs/schema/log-graph.md).
        EdgeLabel::FingerprintedAs | EdgeLabel::Aggregates => Some(&[NodeKind::ErrorSignature]),
        // `ErrorSignature`/`LogEvent`/`LogOccurrenceBucket —CAPTURED_FROM→
        // LogSource`: log-domain records are captured from one source.
        EdgeLabel::CapturedFrom => Some(&[NodeKind::LogSource]),
        // `ErrorSignature —FRAME_RESOLVES_TO→ {Symbol|File|Diagnostic}`: the
        // #322 resolution ladder (resolved/ambiguous→Symbol, path_only→File,
        // unresolved→Diagnostic); external frames mint no edge.
        EdgeLabel::FrameResolvesTo => {
            Some(&[NodeKind::Symbol, NodeKind::File, NodeKind::Diagnostic])
        }
        // `ErrorSignature —EMITTED_DURING→ {CommandRun|AgentTurn|AgentSession}`:
        // reserved for #323; the target constraint is frozen now (issue #327).
        EdgeLabel::EmittedDuring => Some(&[
            NodeKind::CommandRun,
            NodeKind::AgentTurn,
            NodeKind::AgentSession,
        ]),
        // A `Review` may only be authored by an `ExternalIdentity`, and a PR
        // `Task` may only request review from an `ExternalIdentity` (issue
        // #335). Both reviewer-identity edges terminate at `ExternalIdentity`
        // only.
        EdgeLabel::ReviewedBy | EdgeLabel::RequestedReviewFrom => {
            Some(&[NodeKind::ExternalIdentity])
        }
        // A `ReviewStateTransition` transitions exactly one `Review` (issue
        // #336): the `TRANSITIONS_REVIEW` edge terminates at a `Review` only.
        EdgeLabel::TransitionsReview => Some(&[NodeKind::Review]),
        _ => None,
    }
}

/// Allowed SOURCE node kinds for the log-domain typed relations (issue #327).
///
/// The SOURCE-side companion to `allowed_target_kinds`. `docs/schema/log-graph.md`
/// frames every log structural edge directionally, so an edge whose target is a
/// schema-correct kind but whose source is not (e.g. a `LogOccurrenceBucket
/// —CAPTURED_FROM→ LogSource`, a source kind the schema never emits) is invalid
/// attribution that the pre-ingest gate must reject. Only the five log labels
/// are constrained; every other label returns `None` (unconstrained) via the
/// `_ => None` arm, so code-graph edges keep their existing source-unconstrained
/// behavior and cannot regress.
const fn allowed_source_kinds(label: EdgeLabel) -> Option<&'static [NodeKind]> {
    match label {
        // `LogEvent —FINGERPRINTED_AS→ ErrorSignature`: only a log exemplar is
        // fingerprinted as a signature (docs/schema/log-graph.md).
        EdgeLabel::FingerprintedAs => Some(&[NodeKind::LogEvent]),
        // `{ErrorSignature|LogEvent} —CAPTURED_FROM→ LogSource`: only a signature
        // or an exemplar is captured from a source — NOT a `LogOccurrenceBucket`,
        // whose `LogSource` is reached transitively via its signature's own
        // `CAPTURED_FROM` (docs/schema/log-graph.md).
        EdgeLabel::CapturedFrom => Some(&[NodeKind::ErrorSignature, NodeKind::LogEvent]),
        // `LogOccurrenceBucket —AGGREGATES→ ErrorSignature`: only an hourly
        // bucket aggregates a signature (docs/schema/log-graph.md).
        EdgeLabel::Aggregates => Some(&[NodeKind::LogOccurrenceBucket]),
        // `ErrorSignature —FRAME_RESOLVES_TO→ …` (#322) and
        // `ErrorSignature —EMITTED_DURING→ …` (reserved #323) both originate at a
        // signature only (docs/schema/log-graph.md). Combined because the source
        // set is identical (clippy `match_same_arms`).
        EdgeLabel::FrameResolvesTo | EdgeLabel::EmittedDuring => Some(&[NodeKind::ErrorSignature]),
        // ── Reviewer-identity domain (issue #335) ───────────────────────────
        // `Review —REVIEWED_BY→ ExternalIdentity`: only a `Review` is authored
        // by a reviewer identity. `Task —REQUESTED_REVIEW_FROM→
        // ExternalIdentity`: only the PR `Task` requests a review. The schema
        // and daemon frame both edges directionally, so a schema-correct
        // `ExternalIdentity` target reached from a wrong-kind source (e.g. a
        // `Task —REVIEWED_BY→` or a `Review —REQUESTED_REVIEW_FROM→`) is invalid
        // attribution the pre-ingest gate must reject.
        EdgeLabel::ReviewedBy => Some(&[NodeKind::Review]),
        EdgeLabel::RequestedReviewFrom => Some(&[NodeKind::Task]),
        // ── Review-state history (issue #336) ───────────────────────────────
        // `ReviewStateTransition —TRANSITIONS_REVIEW→ Review`: only a
        // `ReviewStateTransition` transitions a review, so a schema-correct
        // `Review` target reached from a wrong-kind source is invalid
        // attribution the pre-ingest gate must reject.
        EdgeLabel::TransitionsReview => Some(&[NodeKind::ReviewStateTransition]),
        _ => None,
    }
}

/// The importer `source_kind` a reviewer-identity edge requires on its SOURCE
/// node (issue #369), matching the daemon's `require_project_edge_source_kind`
/// gate in `validate_project_edge`.
///
/// Distinct from `allowed_source_kinds`, which constrains the source NODE KIND:
/// this constrains the finer importer-origin `source_kind` STRING the node
/// carries (`github_review` / `github_pr`), so a kind-correct but mis-attributed
/// source can never mint a reviewer-identity binding the daemon would reject.
/// Only the two reviewer-identity edges are constrained; every other label
/// returns `None` (unconstrained) via the `_ => None` arm.
const fn required_source_kind(label: EdgeLabel) -> Option<&'static str> {
    match label {
        // `Review —REVIEWED_BY→ ExternalIdentity` must originate from an
        // importer-stamped `github_review` Review.
        EdgeLabel::ReviewedBy => Some(SOURCE_KIND_REVIEW),
        // `Task —REQUESTED_REVIEW_FROM→ ExternalIdentity` must originate from a
        // `github_pr` PR Task — never a `github_issue` Task.
        EdgeLabel::RequestedReviewFrom => Some(SOURCE_KIND_PR),
        _ => None,
    }
}

/// One machine-readable referential-integrity diagnostic.
///
/// Field population depends on `code`; unset fields are omitted from JSON.
/// The derived `Ord` doubles as the canonical output order: diagnostics sort
/// by category code first, then by the offending record IDs.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct ValidationDiagnostic {
    /// Stable defect category.
    pub code: &'static str,
    /// Offending edge record ID (edge-side categories).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_id: Option<String>,
    /// Relation label of the offending edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    /// Which endpoint offends: `source` or `target`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<&'static str>,
    /// Referenced ID that resolves to nothing (`dangling_edge_endpoint`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_id: Option<String>,
    /// Present-but-wrong-kind target (`edge_target_kind_violation`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// Observed kind of the violating target node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<&'static str>,
    /// Allowed target kinds for the relation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_kinds: Option<Vec<&'static str>>,
    /// Referenced ID that is tombstoned and unsuperseded
    /// (`edge_to_tombstoned_record`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstoned_id: Option<String>,
    /// Tombstone record ID involved in the defect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone_id: Option<String>,
    /// Deleted record ID named by a stranding tombstone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_id: Option<String>,
    /// Live edge IDs still referencing a tombstoned record, sorted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stranded_edge_ids: Option<Vec<String>>,
    /// Offending node record ID (`orphan_node`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Node kind of the offending record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// Observed importer `source_kind` on the offending source node
    /// (`edge_source_kind_attribution_violation`); absent when the node carries
    /// none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// Importer `source_kind` the relation requires on its source node
    /// (`edge_source_kind_attribution_violation`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_source_kind: Option<&'static str>,
    /// Repo-relative path of the offending or referenced node, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span of the offending or referenced node, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl ValidationDiagnostic {
    const fn new(code: &'static str) -> Self {
        Self {
            code,
            edge_id: None,
            relation: None,
            endpoint: None,
            missing_id: None,
            target_id: None,
            target_kind: None,
            allowed_kinds: None,
            tombstoned_id: None,
            tombstone_id: None,
            deleted_id: None,
            stranded_edge_ids: None,
            record_id: None,
            kind: None,
            source_kind: None,
            required_source_kind: None,
            repo_relative_path: None,
            span: None,
        }
    }

    /// One human-readable line for `--format text`. Redaction-safe: renders
    /// only the same IDs, labels, paths, spans, and counts as the JSON form.
    #[must_use]
    pub fn to_text(&self) -> String {
        fn push(parts: &mut Vec<String>, key: &str, value: Option<&str>) {
            if let Some(value) = value {
                parts.push(format!("{key}={value}"));
            }
        }
        let mut parts = vec![format!("defect {}", self.code)];
        push(&mut parts, "edge", self.edge_id.as_deref());
        push(&mut parts, "relation", self.relation.as_deref());
        push(&mut parts, "endpoint", self.endpoint);
        push(&mut parts, "missing", self.missing_id.as_deref());
        push(&mut parts, "target", self.target_id.as_deref());
        push(&mut parts, "target_kind", self.target_kind);
        if let Some(allowed) = &self.allowed_kinds {
            parts.push(format!("allowed_kinds={}", allowed.join(",")));
        }
        push(&mut parts, "tombstoned", self.tombstoned_id.as_deref());
        push(&mut parts, "tombstone", self.tombstone_id.as_deref());
        push(&mut parts, "deleted", self.deleted_id.as_deref());
        if let Some(stranded) = &self.stranded_edge_ids {
            parts.push(format!("stranded_edges={}", stranded.join(",")));
        }
        push(&mut parts, "record", self.record_id.as_deref());
        push(&mut parts, "kind", self.kind);
        push(&mut parts, "source_kind", self.source_kind.as_deref());
        push(
            &mut parts,
            "required_source_kind",
            self.required_source_kind,
        );
        push(&mut parts, "path", self.repo_relative_path.as_deref());
        if let Some(span) = self.span {
            parts.push(format!("lines={}-{}", span.start_line, span.end_line));
        }
        parts.join(" ")
    }
}

/// Result of one referential-integrity pass over a record set.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ValidationReport {
    /// Diagnostics in canonical order (category code, then record IDs).
    pub diagnostics: Vec<ValidationDiagnostic>,
    /// Total records inspected.
    pub records: usize,
    /// Node records inspected.
    pub nodes: usize,
    /// Edge records inspected.
    pub edges: usize,
    /// Tombstone records inspected.
    pub tombstones: usize,
}

impl ValidationReport {
    /// Returns `true` when the graph is referentially closed.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Deterministic per-ID index over one record set.
///
/// Node IDs may recur (history replay re-emits a node at every commit); the
/// first record in input order supplies the diagnostic citation.
#[derive(Default)]
struct GraphIndex<'a> {
    /// Every kind observed per node ID.
    node_kinds: BTreeMap<&'a str, BTreeSet<NodeKind>>,
    /// The resolved importer `source_kind` per node ID (issue #369), by
    /// last-write-wins to match the daemon's `lookup_node_source_kind` (which
    /// reverse-scans the batch, so the last record for an ID over the same
    /// forward record sequence shadows the earlier ones). A node ID recurring
    /// across history commits with the SAME `source_kind` still resolves to that
    /// value; only the adversarial conflicting-value case differs from a set.
    node_source_kinds: BTreeMap<&'a str, &'a str>,
    /// First node record per ID, for path/span citations.
    node_first: BTreeMap<&'a str, &'a GraphRecord>,
    /// Tombstone record IDs per deleted ID.
    tombstones_by_deleted: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// Node record count.
    nodes: usize,
    /// Edge record count.
    edges: usize,
    /// Tombstone record count.
    tombstones: usize,
}

impl<'a> GraphIndex<'a> {
    fn build(records: &'a [GraphRecord]) -> Self {
        let mut index = Self::default();
        for record in records {
            match record {
                GraphRecord::Node {
                    id,
                    kind,
                    source_kind,
                    ..
                } => {
                    index.nodes += 1;
                    index.node_kinds.entry(id).or_default().insert(*kind);
                    if let Some(source_kind) = source_kind {
                        // Plain overwrite: the last record for an ID wins,
                        // matching the daemon's reverse-scan-of-batch.
                        index.node_source_kinds.insert(id, source_kind.as_str());
                    }
                    index.node_first.entry(id).or_insert(record);
                }
                GraphRecord::Edge { .. } => index.edges += 1,
                GraphRecord::Tombstone { id, deleted_id, .. } => {
                    index.tombstones += 1;
                    index
                        .tombstones_by_deleted
                        .entry(deleted_id)
                        .or_default()
                        .insert(id);
                }
            }
        }
        index
    }

    /// Attaches the cited node's repo-relative path and span, when present.
    fn cite_node(&self, diagnostic: &mut ValidationDiagnostic, id: &str) {
        if let Some(GraphRecord::Node {
            repo_relative_path,
            span,
            ..
        }) = self.node_first.get(id)
        {
            diagnostic.repo_relative_path.clone_from(repo_relative_path);
            diagnostic.span = *span;
        }
    }
}

/// Checks every edge for endpoint resolution, tombstoned references, and typed
/// target kinds. Returns the set of IDs incident to any edge and, per
/// tombstoned ID, the live edges still referencing it.
fn check_edges<'a>(
    records: &'a [GraphRecord],
    index: &GraphIndex<'a>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) -> (BTreeSet<&'a str>, BTreeMap<&'a str, BTreeSet<&'a str>>) {
    let mut incident: BTreeSet<&str> = BTreeSet::new();
    let mut stranded_by_deleted: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for record in records {
        let GraphRecord::Edge {
            id: edge_id,
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        incident.insert(source);
        incident.insert(target);

        for (endpoint, endpoint_id) in [("source", source), ("target", target)] {
            if let Some(tombstone_ids) = index.tombstones_by_deleted.get(endpoint_id.as_str()) {
                stranded_by_deleted
                    .entry(endpoint_id)
                    .or_default()
                    .insert(edge_id);
                // A surviving node record with the same ID supersedes the
                // tombstone for the edge-side check: the reference still
                // resolves, and the conflict is reported on the tombstone.
                if !index.node_kinds.contains_key(endpoint_id.as_str()) {
                    let mut diagnostic = ValidationDiagnostic::new(EDGE_TO_TOMBSTONED_RECORD);
                    diagnostic.edge_id = Some(edge_id.clone());
                    diagnostic.relation = Some(label.as_str().to_owned());
                    diagnostic.endpoint = Some(endpoint);
                    diagnostic.tombstoned_id = Some(endpoint_id.clone());
                    diagnostic.tombstone_id =
                        tombstone_ids.iter().next().map(|id| (*id).to_owned());
                    diagnostics.insert(diagnostic);
                }
            } else if !index.node_kinds.contains_key(endpoint_id.as_str()) {
                let mut diagnostic = ValidationDiagnostic::new(DANGLING_EDGE_ENDPOINT);
                diagnostic.edge_id = Some(edge_id.clone());
                diagnostic.relation = Some(label.as_str().to_owned());
                diagnostic.endpoint = Some(endpoint);
                diagnostic.missing_id = Some(endpoint_id.clone());
                diagnostics.insert(diagnostic);
            }
        }

        // Typed relation target-kind check (present targets only; missing or
        // tombstoned targets are already reported above).
        if let (Some(allowed), Some(kinds)) = (
            allowed_target_kinds(*label),
            index.node_kinds.get(target.as_str()),
        ) && !kinds.iter().any(|kind| allowed.contains(kind))
        {
            let mut diagnostic = ValidationDiagnostic::new(EDGE_TARGET_KIND_VIOLATION);
            diagnostic.edge_id = Some(edge_id.clone());
            diagnostic.relation = Some(label.as_str().to_owned());
            diagnostic.target_id = Some(target.clone());
            diagnostic.target_kind = kinds.iter().next().map(|kind| kind.as_str());
            diagnostic.allowed_kinds = Some(allowed.iter().map(|kind| kind.as_str()).collect());
            index.cite_node(&mut diagnostic, target);
            diagnostics.insert(diagnostic);
        }

        // Typed relation source-kind check (present sources only; a missing or
        // tombstoned source is already reported above). Mirrors the target-kind
        // check for the source endpoint: log structural edges are directional
        // (issue #327), so a schema-correct target with a wrong-kind source is
        // invalid attribution.
        if let (Some(allowed), Some(kinds)) = (
            allowed_source_kinds(*label),
            index.node_kinds.get(source.as_str()),
        ) && !kinds.iter().any(|kind| allowed.contains(kind))
        {
            let mut diagnostic = ValidationDiagnostic::new(EDGE_SOURCE_KIND_VIOLATION);
            diagnostic.edge_id = Some(edge_id.clone());
            diagnostic.relation = Some(label.as_str().to_owned());
            diagnostic.endpoint = Some("source");
            diagnostic.record_id = Some(source.clone());
            diagnostic.kind = kinds.iter().next().map(|kind| kind.as_str());
            diagnostic.allowed_kinds = Some(allowed.iter().map(|kind| kind.as_str()).collect());
            index.cite_node(&mut diagnostic, source);
            diagnostics.insert(diagnostic);
        }

        // Reviewer-identity source-kind attribution check (issue #369). The
        // daemon's `require_project_edge_source_kind` gates REVIEWED_BY on a
        // `github_review` Review source and REQUESTED_REVIEW_FROM on a
        // `github_pr` Task source; the offline validator previously checked only
        // the coarse source node kind, so it green-lit bindings the daemon
        // rejects. Runs only when the source node kind is already valid for the
        // relation (so a wrong-kind source is reported once, as
        // `edge_source_kind_violation`, never doubly), and requires the source
        // node's importer `source_kind` to equal the relation's required value —
        // a wrong or absent attribution is a defect.
        if let (Some(required), Some(kinds)) = (
            required_source_kind(*label),
            index.node_kinds.get(source.as_str()),
        ) && allowed_source_kinds(*label)
            .is_some_and(|allowed| kinds.iter().any(|kind| allowed.contains(kind)))
        {
            let observed = index.node_source_kinds.get(source.as_str()).copied();
            let satisfied = observed == Some(required);
            if !satisfied {
                let mut diagnostic =
                    ValidationDiagnostic::new(EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION);
                diagnostic.edge_id = Some(edge_id.clone());
                diagnostic.relation = Some(label.as_str().to_owned());
                diagnostic.endpoint = Some("source");
                diagnostic.record_id = Some(source.clone());
                diagnostic.source_kind = observed.map(str::to_owned);
                diagnostic.required_source_kind = Some(required);
                index.cite_node(&mut diagnostic, source);
                diagnostics.insert(diagnostic);
            }
        }
    }
    (incident, stranded_by_deleted)
}

/// Reports every tombstone whose deleted record is still referenced by a live
/// edge as source or target.
fn check_tombstones(
    index: &GraphIndex<'_>,
    stranded_by_deleted: &BTreeMap<&str, BTreeSet<&str>>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) {
    for (deleted_id, tombstone_ids) in &index.tombstones_by_deleted {
        let Some(stranded) = stranded_by_deleted.get(deleted_id) else {
            continue;
        };
        for tombstone_id in tombstone_ids {
            let mut diagnostic = ValidationDiagnostic::new(TOMBSTONE_STRANDS_LIVE_EDGE);
            diagnostic.tombstone_id = Some((*tombstone_id).to_owned());
            diagnostic.deleted_id = Some((*deleted_id).to_owned());
            diagnostic.stranded_edge_ids =
                Some(stranded.iter().map(|id| (*id).to_owned()).collect());
            index.cite_node(&mut diagnostic, deleted_id);
            diagnostics.insert(diagnostic);
        }
    }
}

/// Reports every topology node with zero incident edges.
fn check_orphans(
    index: &GraphIndex<'_>,
    incident: &BTreeSet<&str>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) {
    for (id, kinds) in &index.node_kinds {
        if incident.contains(id) {
            continue;
        }
        let Some(orphan_kind) = kinds.iter().find(|kind| ORPHANABLE_KINDS.contains(kind)) else {
            continue;
        };
        let mut diagnostic = ValidationDiagnostic::new(ORPHAN_NODE);
        diagnostic.record_id = Some((*id).to_owned());
        diagnostic.kind = Some(orphan_kind.as_str());
        index.cite_node(&mut diagnostic, id);
        diagnostics.insert(diagnostic);
    }
}

/// Kind-specific containment rule (PR #314 review): every
/// `DependencyDeclaration` must be the target of a `CONTAINS` edge whose
/// source is a `File` node at the SAME repo-relative path as the dependency
/// node's declared manifest handle — the attribution chain `--repo` scoping
/// walks. Any other `File` (a source file, or a different manifest) is the
/// same missing chain. In addition, the containing Files must all belong to
/// ONE repository: same-path manifests exist across repos in a merged
/// store, so a dependency whose containing Files span two `Repository`
/// owners has ambiguous attribution and repository scoping could show the
/// row under the wrong repo. A dependency whose only containment is a
/// single (possibly foreign) repo's manifest is topologically
/// indistinguishable from a legitimate row of that repo — record IDs are
/// opaque — and graphs without `Repository`-owned Files keep the
/// path-equality-only behavior so legacy/partial graphs are not
/// mass-flagged (documented softening). Zero-edge nodes are already flagged
/// as `orphan_node`.
fn check_dependency_containment(
    records: &[GraphRecord],
    index: &GraphIndex<'_>,
    incident: &BTreeSet<&str>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) {
    fn node_path<'a>(index: &'a GraphIndex<'_>, id: &str) -> Option<&'a str> {
        match index.node_first.get(id) {
            Some(GraphRecord::Node {
                repo_relative_path, ..
            }) => repo_relative_path.as_deref(),
            _ => None,
        }
    }
    fn source_has_kind(index: &GraphIndex<'_>, id: &str, kind: NodeKind) -> bool {
        index
            .node_kinds
            .get(id)
            .is_some_and(|kinds| kinds.contains(&kind))
    }
    // Direct `Repository —CONTAINS→ File` ownership, as the scanner emits it.
    let mut file_owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for record in records {
        let GraphRecord::Edge {
            label: EdgeLabel::Contains,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if source_has_kind(index, source, NodeKind::Repository)
            && source_has_kind(index, target, NodeKind::File)
        {
            file_owners.entry(target).or_default().insert(source);
        }
    }
    let mut contained: BTreeSet<&str> = BTreeSet::new();
    let mut dep_owner_repos: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for record in records {
        let GraphRecord::Edge {
            label: EdgeLabel::Contains,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if !source_has_kind(index, source, NodeKind::File)
            || !source_has_kind(index, target, NodeKind::DependencyDeclaration)
        {
            continue;
        }
        // Every containing File's owners count toward the consistency set.
        if let Some(owners) = file_owners.get(source.as_str()) {
            dep_owner_repos
                .entry(target)
                .or_default()
                .extend(owners.iter().copied());
        }
        // The containing file must BE the declaring manifest: its path must
        // equal the dependency node's declared manifest handle.
        let source_path = node_path(index, source);
        if source_path.is_some() && source_path == node_path(index, target) {
            contained.insert(target);
        }
    }
    for (id, kinds) in &index.node_kinds {
        if !kinds.contains(&NodeKind::DependencyDeclaration) || !incident.contains(id) {
            continue;
        }
        let spans_repos = dep_owner_repos
            .get(id)
            .is_some_and(|owners| owners.len() > 1);
        if contained.contains(id) && !spans_repos {
            continue;
        }
        let mut diagnostic = ValidationDiagnostic::new(MISSING_CONTAINMENT_EDGE);
        diagnostic.record_id = Some((*id).to_owned());
        diagnostic.kind = Some(NodeKind::DependencyDeclaration.as_str());
        index.cite_node(&mut diagnostic, id);
        diagnostics.insert(diagnostic);
    }
}

/// Cardinality of a required outbound log structural edge (issue #327).
///
/// Every requirement fires `missing_log_structural_edge` at count 0. The two
/// variants differ only above one: an `ExactlyOne` requirement flags a surplus
/// as `duplicate_log_structural_edge`, while an `AtLeastOne` requirement
/// tolerates any positive count.
#[derive(Clone, Copy, Eq, PartialEq)]
enum LogEdgeCardinality {
    /// Exactly one edge; count > 1 is `duplicate_log_structural_edge`.
    ExactlyOne,
    /// One or more edges; count > 1 is a valid aggregate, never a duplicate.
    AtLeastOne,
}

/// Log-domain structural-completeness rule (issue #327): every incident
/// log-domain node must carry its required OUTBOUND structural edges (the log
/// node is the edge `source`), each with a per-requirement cardinality.
///
/// * `LogEvent` requires exactly one `FINGERPRINTED_AS` (one exemplar is
///   fingerprinted as exactly one signature — two is malformed) and at least
///   one `CAPTURED_FROM`. A `LogEvent` ID excludes the source (it is keyed on
///   repo/signature/valid-time/content-hash, see `src/log_graph.rs`), while
///   each `LogSource` ID is path/hash-distinct, so a graph combining two log
///   files where the same exemplar appears in both legitimately gives one
///   `LogEvent` a distinct `CAPTURED_FROM` per `LogSource` — never a duplicate,
///   the same multi-source-aggregate reason as `ErrorSignature` below.
/// * `ErrorSignature` requires at least one `CAPTURED_FROM` (the extractor
///   always emits `ErrorSignature —CAPTURED_FROM→ LogSource`, so it is itself a
///   required, validated edge — bucket source attribution reached via the
///   signature is thus guaranteed present, not assumed). A signature ID is a
///   repo/fingerprint aggregate that excludes the source (see
///   `src/log_graph.rs`), and `scan-logs` emits a distinct `CAPTURED_FROM` per
///   `LogSource`, so a graph combining two log files that share a fingerprint
///   legitimately gives one signature two `CAPTURED_FROM` edges — never a
///   duplicate.
/// * `LogOccurrenceBucket` requires exactly one `AGGREGATES` (the extractor
///   does not emit a bucket `CAPTURED_FROM`; the source is reached via the
///   signature).
///
/// Only incident nodes are evaluated (gated on the same `incident` set the
/// containment check uses) so a zero-edge log node stays a single
/// `orphan_node` and is never double-reported. Edges are counted by distinct
/// edge record ID, so an identical re-emitted edge record is not a duplicate.
/// A missing edge is `missing_log_structural_edge`; an exactly-one surplus is
/// `duplicate_log_structural_edge` listing the offending edge IDs.
fn check_log_completeness(
    records: &[GraphRecord],
    index: &GraphIndex<'_>,
    incident: &BTreeSet<&str>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) {
    use LogEdgeCardinality::{AtLeastOne, ExactlyOne};

    /// Required outbound structural edges per log node kind with their
    /// cardinality, matching exactly what the issue #319/#320 extractor emits
    /// (`src/log_graph.rs`): a `LogEvent` gets exactly one `FINGERPRINTED_AS`
    /// but at least one `CAPTURED_FROM`, an `ErrorSignature` gets at least one
    /// `CAPTURED_FROM` (one per source it aggregates across), but a
    /// `LogOccurrenceBucket` gets only `AGGREGATES` (its `LogSource` is reached
    /// transitively via the signature's own required `CAPTURED_FROM`).
    /// `LogEvent` `CAPTURED_FROM` is at-least-one for the same
    /// multi-source-aggregate reason as `ErrorSignature`: a `LogEvent` ID
    /// excludes the source, so the same exemplar seen in multiple log files
    /// converges to one event node that captures from each `LogSource`.
    /// Requiring a bucket `CAPTURED_FROM` would false-positive on every real
    /// scan-logs graph.
    const fn required_labels(kind: NodeKind) -> &'static [(EdgeLabel, LogEdgeCardinality)] {
        match kind {
            NodeKind::LogEvent => &[
                (EdgeLabel::FingerprintedAs, ExactlyOne),
                (EdgeLabel::CapturedFrom, AtLeastOne),
            ],
            NodeKind::ErrorSignature => &[(EdgeLabel::CapturedFrom, AtLeastOne)],
            NodeKind::LogOccurrenceBucket => &[(EdgeLabel::Aggregates, ExactlyOne)],
            _ => &[],
        }
    }
    // (source node id, label) -> distinct edge record IDs.
    let mut outbound: BTreeMap<(&str, EdgeLabel), BTreeSet<&str>> = BTreeMap::new();
    for record in records {
        let GraphRecord::Edge {
            id, label, source, ..
        } = record
        else {
            continue;
        };
        outbound.entry((source, *label)).or_default().insert(id);
    }
    for (id, kinds) in &index.node_kinds {
        if !incident.contains(id) {
            continue;
        }
        for kind in kinds {
            for &(label, cardinality) in required_labels(*kind) {
                let count = outbound.get(&(*id, label)).map_or(0, BTreeSet::len);
                let code = match (count, cardinality) {
                    (0, _) => MISSING_LOG_STRUCTURAL_EDGE,
                    // At-least-one requirements accept any positive count; a
                    // signature legitimately captures from multiple sources.
                    (1, _) | (_, AtLeastOne) => continue,
                    (_, ExactlyOne) => DUPLICATE_LOG_STRUCTURAL_EDGE,
                };
                let mut diagnostic = ValidationDiagnostic::new(code);
                diagnostic.record_id = Some((*id).to_owned());
                diagnostic.kind = Some(kind.as_str());
                diagnostic.relation = Some(label.as_str().to_owned());
                if code == DUPLICATE_LOG_STRUCTURAL_EDGE
                    && let Some(edge_ids) = outbound.get(&(*id, label))
                {
                    diagnostic.stranded_edge_ids =
                        Some(edge_ids.iter().map(|edge| (*edge).to_owned()).collect());
                }
                index.cite_node(&mut diagnostic, id);
                diagnostics.insert(diagnostic);
            }
        }
    }
}

/// Validates referential integrity over an already-parsed record set.
///
/// Checks, in one deterministic pass:
///
/// 1. every edge endpoint (source and target) resolves to a node present in
///    the graph (`dangling_edge_endpoint`);
/// 2. every `DEFINES`, `CONTAINS`, `CALLS`, `IMPORTS`, and `MENTIONS` edge
///    target is a node of an allowed kind (`edge_target_kind_violation`), and
///    every log-domain structural edge additionally has a source of an allowed
///    kind (`edge_source_kind_violation`, issue #327);
/// 3. no edge references a tombstoned-and-unsuperseded record — a tombstoned
///    ID with no surviving node record (`edge_to_tombstoned_record`);
/// 4. no record is named by a tombstone yet still referenced by a live edge
///    (`tombstone_strands_live_edge`);
/// 5. no topology node (`File`, `Module`, `Symbol`, `Import`,
///    `DependencyDeclaration`) is orphaned with zero incident edges
///    (`orphan_node`);
/// 6. every `DependencyDeclaration` with incident edges is the target of a
///    `File —CONTAINS→` attribution edge from its declaring manifest — the
///    containing file's path equals the dependency node's manifest handle
///    (`missing_containment_edge`).
///
/// The output is deterministic: diagnostics are deduplicated and sorted in
/// canonical order, so repeated validation of the same input is identical.
#[must_use]
pub fn validate_records(records: &[GraphRecord]) -> ValidationReport {
    let index = GraphIndex::build(records);
    let mut diagnostics: BTreeSet<ValidationDiagnostic> = BTreeSet::new();

    let (incident, stranded_by_deleted) = check_edges(records, &index, &mut diagnostics);
    check_tombstones(&index, &stranded_by_deleted, &mut diagnostics);
    check_orphans(&index, &incident, &mut diagnostics);
    check_dependency_containment(records, &index, &incident, &mut diagnostics);
    check_log_completeness(records, &index, &incident, &mut diagnostics);

    ValidationReport {
        diagnostics: diagnostics.into_iter().collect(),
        records: records.len(),
        nodes: index.nodes,
        edges: index.edges,
        tombstones: index.tombstones,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::SCHEMA_VERSION;

    fn node(id: &str, kind: NodeKind) -> GraphRecord {
        node_at(id, kind, "src/lib.rs")
    }

    fn node_at(id: &str, kind: NodeKind, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            Some(path.to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 1,
            }),
            Some("n".to_owned()),
            "test node".to_owned(),
        )
    }

    /// A node carrying an importer `source_kind` attribution (issue #369).
    fn node_with_source_kind(id: &str, kind: NodeKind, kind_str: &str) -> GraphRecord {
        let mut record = node(id, kind);
        if let GraphRecord::Node { source_kind, .. } = &mut record {
            *source_kind = Some(kind_str.to_owned());
        }
        record
    }

    fn edge(id: &str, label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::Edge {
            id: id.to_owned(),
            schema_version: SCHEMA_VERSION,
            label,
            source: source.to_owned(),
            target: target.to_owned(),
            confidence: None,
            resolution: None,
            frame_resolution: None,
            frame_index: None,
            basis: None,
            temporal: None,
            summary: "test edge".to_owned(),
            producer: None,
        }
    }

    fn tombstone(id: &str, deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: id.to_owned(),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "test tombstone".to_owned(),
            producer: None,
        }
    }

    #[test]
    fn clean_graph_has_no_diagnostics() {
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:sym", NodeKind::Symbol),
            edge("e:def", EdgeLabel::Defines, "n:file", "n:sym"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
        assert_eq!((report.nodes, report.edges, report.tombstones), (2, 1, 0));
    }

    #[test]
    fn contains_edge_to_dependency_declaration_is_allowed() {
        // Issue #180 topology: File(Cargo.toml) —CONTAINS→ DependencyDeclaration,
        // with the containing file AT the dependency's declared manifest path.
        let records = vec![
            node_at("n:manifest", NodeKind::File, "crates/a/Cargo.toml"),
            node_at(
                "n:dep",
                NodeKind::DependencyDeclaration,
                "crates/a/Cargo.toml",
            ),
            edge("e:contains", EdgeLabel::Contains, "n:manifest", "n:dep"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn dependency_containment_by_a_non_manifest_file_is_a_defect() {
        // PR #314 review: any `File` source is not enough — containment by
        // `src/lib.rs` is not the declaring manifest's attribution chain.
        let records = vec![
            node_at("n:dep", NodeKind::DependencyDeclaration, "Cargo.toml"),
            node_at("n:lib", NodeKind::File, "src/lib.rs"),
            edge("e:contains", EdgeLabel::Contains, "n:lib", "n:dep"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![MISSING_CONTAINMENT_EDGE]);
        assert_eq!(report.diagnostics[0].record_id.as_deref(), Some("n:dep"));
    }

    #[test]
    fn dependency_containment_by_a_foreign_manifest_is_a_defect() {
        // Containment by a DIFFERENT manifest (e.g. another crate's or
        // repo's Cargo.toml) is the same missing attribution chain.
        let records = vec![
            node_at(
                "n:dep",
                NodeKind::DependencyDeclaration,
                "crates/a/Cargo.toml",
            ),
            node_at("n:foreign", NodeKind::File, "Cargo.toml"),
            edge("e:contains", EdgeLabel::Contains, "n:foreign", "n:dep"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![MISSING_CONTAINMENT_EDGE]);
        assert_eq!(report.diagnostics[0].record_id.as_deref(), Some("n:dep"));
    }

    #[test]
    fn dependency_containment_spanning_repositories_is_a_defect() {
        // PR #314 review: path equality alone lets a repo-A dependency be
        // contained by repo-B's same-path manifest File. When containing
        // Files tie the dependency to MORE than one Repository, the
        // attribution is ambiguous and repository scoping can show the row
        // under the wrong repo — a defect.
        let records = vec![
            node_at("n:repo-a", NodeKind::Repository, "."),
            node_at("n:repo-b", NodeKind::Repository, "."),
            node_at("n:manifest-a", NodeKind::File, "Cargo.toml"),
            node_at("n:manifest-b", NodeKind::File, "Cargo.toml"),
            node_at("n:dep-a", NodeKind::DependencyDeclaration, "Cargo.toml"),
            node_at("n:dep-b", NodeKind::DependencyDeclaration, "Cargo.toml"),
            edge("e:ra-fa", EdgeLabel::Contains, "n:repo-a", "n:manifest-a"),
            edge("e:rb-fb", EdgeLabel::Contains, "n:repo-b", "n:manifest-b"),
            edge("e:fa-da", EdgeLabel::Contains, "n:manifest-a", "n:dep-a"),
            edge("e:fb-db", EdgeLabel::Contains, "n:manifest-b", "n:dep-b"),
            // The malformed edge: repo B's manifest also claims repo A's dep.
            edge("e:fb-da", EdgeLabel::Contains, "n:manifest-b", "n:dep-a"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![MISSING_CONTAINMENT_EDGE]);
        assert_eq!(report.diagnostics[0].record_id.as_deref(), Some("n:dep-a"));
    }

    #[test]
    fn dependency_containment_within_one_repository_is_clean() {
        // The well-formed merged two-repo graph: each dep contained only by
        // its own repo's manifest File — same paths across repos are fine.
        let records = vec![
            node_at("n:repo-a", NodeKind::Repository, "."),
            node_at("n:repo-b", NodeKind::Repository, "."),
            node_at("n:manifest-a", NodeKind::File, "Cargo.toml"),
            node_at("n:manifest-b", NodeKind::File, "Cargo.toml"),
            node_at("n:dep-a", NodeKind::DependencyDeclaration, "Cargo.toml"),
            node_at("n:dep-b", NodeKind::DependencyDeclaration, "Cargo.toml"),
            edge("e:ra-fa", EdgeLabel::Contains, "n:repo-a", "n:manifest-a"),
            edge("e:rb-fb", EdgeLabel::Contains, "n:repo-b", "n:manifest-b"),
            edge("e:fa-da", EdgeLabel::Contains, "n:manifest-a", "n:dep-a"),
            edge("e:fb-db", EdgeLabel::Contains, "n:manifest-b", "n:dep-b"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn dependency_declaration_without_file_containment_is_a_defect() {
        // PR #314 review: any incident edge is not enough — repository
        // scoping walks specifically File —CONTAINS→ DependencyDeclaration,
        // so a dependency node with only unrelated edges must be flagged.
        let records = vec![
            node("n:dep", NodeKind::DependencyDeclaration),
            node("n:file", NodeKind::File),
            node("n:sym", NodeKind::Symbol),
            edge("e:def", EdgeLabel::Defines, "n:file", "n:sym"),
            edge("e:mention", EdgeLabel::Mentions, "n:dep", "n:sym"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![MISSING_CONTAINMENT_EDGE]);
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.record_id.as_deref(), Some("n:dep"));
        assert_eq!(diagnostic.kind, Some("DependencyDeclaration"));
    }

    #[test]
    fn unattached_dependency_declaration_is_an_orphan() {
        // PR #314 review: a standalone `DependencyDeclaration` without its
        // `File —CONTAINS→ DependencyDeclaration` attribution edge breaks
        // the repository-ownership chain `--repo` scoping relies on.
        let records = vec![node("n:dep", NodeKind::DependencyDeclaration)];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![ORPHAN_NODE]);
        assert_eq!(report.diagnostics[0].kind, Some("DependencyDeclaration"));
    }

    #[test]
    fn dangling_endpoints_report_per_endpoint() {
        let records = vec![edge("e:x", EdgeLabel::Calls, "n:missing-a", "n:missing-b")];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![DANGLING_EDGE_ENDPOINT, DANGLING_EDGE_ENDPOINT]);
        let endpoints: Vec<_> = report
            .diagnostics
            .iter()
            .filter_map(|d| d.endpoint)
            .collect();
        assert_eq!(endpoints, vec!["source", "target"]);
    }

    #[test]
    fn contains_edge_to_unsafe_site_is_allowed() {
        // `File` CONTAINS `UnsafeSite` is what the issue #222 extractor emits;
        // the referential-integrity gate must accept it.
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:unsafe", NodeKind::UnsafeSite),
            edge("e:contains", EdgeLabel::Contains, "n:file", "n:unsafe"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn repository_and_diagnostic_nodes_are_never_orphans() {
        let records = vec![
            node("n:repo", NodeKind::Repository),
            node("n:diag", NodeKind::Diagnostic),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn tombstoned_id_with_surviving_node_is_superseded_for_edge_check() {
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:sym", NodeKind::Symbol),
            edge("e:def", EdgeLabel::Defines, "n:file", "n:sym"),
            tombstone("t:1", "n:sym"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![TOMBSTONE_STRANDS_LIVE_EDGE]);
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.deleted_id.as_deref(), Some("n:sym"));
        assert_eq!(
            diagnostic.stranded_edge_ids.as_deref(),
            Some(&["e:def".to_owned()][..])
        );
    }

    #[test]
    fn diagnostics_sort_canonically_by_code_then_ids() {
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:import", NodeKind::Import),
            node("n:orphan", NodeKind::Symbol),
            edge("e:kind", EdgeLabel::Defines, "n:file", "n:import"),
            edge("e:gone", EdgeLabel::Calls, "n:file", "n:absent"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        assert_eq!(codes, sorted);
        assert!(codes.contains(&DANGLING_EDGE_ENDPOINT));
        assert!(codes.contains(&EDGE_TARGET_KIND_VIOLATION));
        assert!(codes.contains(&ORPHAN_NODE));
    }

    #[test]
    fn reviews_commit_edge_to_commit_is_allowed() {
        // Issue #334: a REVIEWS_COMMIT edge whose target is a Commit passes the
        // typed target-kind check (Review→Commit is the allowed shape).
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:commit", NodeKind::Commit),
            edge("e:anchor", EdgeLabel::ReviewsCommit, "n:review", "n:commit"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "Review→Commit must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn reviews_commit_edge_to_wrong_kind_is_rejected() {
        // Issue #334: a REVIEWS_COMMIT edge targeting a non-Commit node is a
        // target-kind violation.
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:sym", NodeKind::Symbol),
            edge("e:bad", EdgeLabel::ReviewsCommit, "n:review", "n:sym"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "Review→Symbol must be rejected, got {codes:?}"
        );
    }

    // ── Log-domain typed target-kind allow-list (issue #327) ─────────────────

    /// Builds a minimal well-formed log graph clean under every check: a
    /// `LogSource`, an `ErrorSignature` captured from it, a `LogEvent`
    /// fingerprinted+captured, and a `LogOccurrenceBucket` aggregated. The
    /// bucket carries NO `CAPTURED_FROM`: its `LogSource` is reached via the
    /// signature, and a bucket source is a disallowed `CAPTURED_FROM` source
    /// kind (issue #327 source-kind allow-list).
    fn clean_log_records() -> Vec<GraphRecord> {
        vec![
            node("n:source", NodeKind::LogSource),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:event", NodeKind::LogEvent),
            node("n:bucket", NodeKind::LogOccurrenceBucket),
            edge("e:sig-cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
            edge("e:evt-fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
            edge("e:evt-cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
            edge("e:bkt-agg", EdgeLabel::Aggregates, "n:bucket", "n:sig"),
        ]
    }

    #[test]
    fn well_formed_log_graph_is_clean() {
        let report = validate_records(&clean_log_records());
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn all_log_edges_with_correct_sources_are_clean() {
        // Every log structural label exercised with a schema-correct source:
        // FINGERPRINTED_AS(LogEvent→ErrorSignature), CAPTURED_FROM from both an
        // ErrorSignature and a LogEvent, and AGGREGATES(bucket→ErrorSignature).
        // No source-kind violation must fire.
        let records = vec![
            node("n:source", NodeKind::LogSource),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:event", NodeKind::LogEvent),
            node("n:bucket", NodeKind::LogOccurrenceBucket),
            edge("e:evt-fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
            edge("e:sig-cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
            edge("e:evt-cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
            edge("e:bkt-agg", EdgeLabel::Aggregates, "n:bucket", "n:sig"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn bucket_captured_from_source_is_rejected() {
        // The Codex-review bug: a `LogOccurrenceBucket —CAPTURED_FROM→ LogSource`
        // has an allowed TARGET (LogSource) but a disallowed SOURCE kind (a
        // bucket is never a CAPTURED_FROM source). The bucket keeps its required
        // AGGREGATES so the ONLY defect is the source-kind violation.
        let records = vec![
            node("n:bucket", NodeKind::LogOccurrenceBucket),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            edge("e:bkt-agg", EdgeLabel::Aggregates, "n:bucket", "n:sig"),
            edge("e:sig-cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
            edge("e:bkt-cap", EdgeLabel::CapturedFrom, "n:bucket", "n:source"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![EDGE_SOURCE_KIND_VIOLATION], "got {codes:?}");
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.relation.as_deref(), Some("CAPTURED_FROM"));
        assert_eq!(diagnostic.record_id.as_deref(), Some("n:bucket"));
        assert_eq!(diagnostic.edge_id.as_deref(), Some("e:bkt-cap"));
        assert_eq!(diagnostic.endpoint, Some("source"));
        assert_eq!(diagnostic.kind, Some("LogOccurrenceBucket"));
        assert_eq!(
            diagnostic.allowed_kinds.as_deref(),
            Some(&["ErrorSignature", "LogEvent"][..])
        );
    }

    #[test]
    fn fingerprinted_as_from_wrong_source_is_rejected() {
        // FINGERPRINTED_AS must originate at a LogEvent; a LogSource source is a
        // source-kind violation even though the ErrorSignature target is allowed.
        let records = vec![
            node("n:source", NodeKind::LogSource),
            node("n:sig", NodeKind::ErrorSignature),
            edge("e:fp", EdgeLabel::FingerprintedAs, "n:source", "n:sig"),
            edge("e:sig-cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![EDGE_SOURCE_KIND_VIOLATION], "got {codes:?}");
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.relation.as_deref(), Some("FINGERPRINTED_AS"));
        assert_eq!(diagnostic.record_id.as_deref(), Some("n:source"));
        assert_eq!(diagnostic.kind, Some("LogSource"));
        assert_eq!(diagnostic.allowed_kinds.as_deref(), Some(&["LogEvent"][..]));
    }

    #[test]
    fn aggregates_from_wrong_source_is_rejected() {
        // AGGREGATES must originate at a LogOccurrenceBucket; a LogEvent source
        // is a source-kind violation even though the ErrorSignature target is
        // allowed. The event keeps its own required edges so it is otherwise
        // well-formed and only the aggregates source offends.
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            edge("e:evt-fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
            edge("e:evt-cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
            edge("e:sig-cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
            edge("e:agg", EdgeLabel::Aggregates, "n:event", "n:sig"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![EDGE_SOURCE_KIND_VIOLATION], "got {codes:?}");
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.relation.as_deref(), Some("AGGREGATES"));
        assert_eq!(diagnostic.record_id.as_deref(), Some("n:event"));
        assert_eq!(diagnostic.kind, Some("LogEvent"));
        assert_eq!(
            diagnostic.allowed_kinds.as_deref(),
            Some(&["LogOccurrenceBucket"][..])
        );
    }

    #[test]
    fn fingerprinted_as_edge_to_error_signature_is_allowed() {
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:source", NodeKind::LogSource),
            node("n:sig", NodeKind::ErrorSignature),
            edge("e:fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "FINGERPRINTED_AS→ErrorSignature must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn fingerprinted_as_edge_to_wrong_kind_is_rejected() {
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:task", NodeKind::Task),
            edge("e:fp", EdgeLabel::FingerprintedAs, "n:event", "n:task"),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == EDGE_TARGET_KIND_VIOLATION),
            "FINGERPRINTED_AS→Task must be rejected, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn captured_from_edge_to_log_source_is_allowed() {
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "CAPTURED_FROM→LogSource must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn captured_from_edge_to_wrong_kind_is_rejected() {
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            node("n:task", NodeKind::Task),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:sig", "n:task"),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == EDGE_TARGET_KIND_VIOLATION),
            "CAPTURED_FROM→Task must be rejected, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn aggregates_edge_to_error_signature_is_allowed() {
        let records = vec![
            node("n:bucket", NodeKind::LogOccurrenceBucket),
            node("n:source", NodeKind::LogSource),
            node("n:sig", NodeKind::ErrorSignature),
            edge("e:agg", EdgeLabel::Aggregates, "n:bucket", "n:sig"),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:bucket", "n:source"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "AGGREGATES→ErrorSignature must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn aggregates_edge_to_wrong_kind_is_rejected() {
        let records = vec![
            node("n:bucket", NodeKind::LogOccurrenceBucket),
            node("n:task", NodeKind::Task),
            edge("e:agg", EdgeLabel::Aggregates, "n:bucket", "n:task"),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == EDGE_TARGET_KIND_VIOLATION),
            "AGGREGATES→Task must be rejected, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn frame_resolves_to_symbol_file_and_diagnostic_are_allowed() {
        // The #322 resolution ladder targets Symbol (resolved/ambiguous),
        // File (path_only), and Diagnostic (unresolved).
        for (target_id, kind) in [
            ("n:sym", NodeKind::Symbol),
            ("n:file", NodeKind::File),
            ("n:diag", NodeKind::Diagnostic),
        ] {
            let records = vec![
                node("n:sig", NodeKind::ErrorSignature),
                node(target_id, kind),
                edge("e:frame", EdgeLabel::FrameResolvesTo, "n:sig", target_id),
            ];
            let report = validate_records(&records);
            let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
            assert!(
                !codes.contains(&EDGE_TARGET_KIND_VIOLATION),
                "FRAME_RESOLVES_TO→{} must be allowed, got {codes:?}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn frame_resolves_to_wrong_kind_is_rejected() {
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            node("n:task", NodeKind::Task),
            edge("e:frame", EdgeLabel::FrameResolvesTo, "n:sig", "n:task"),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == EDGE_TARGET_KIND_VIOLATION),
            "FRAME_RESOLVES_TO→Task must be rejected, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn frame_resolves_to_tombstoned_symbol_is_flagged() {
        // A FRAME_RESOLVES_TO edge into a tombstoned-and-unsuperseded Symbol
        // is an edge_to_tombstoned_record defect — closure flows for log edges.
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            tombstone("t:sym", "n:sym"),
            edge("e:frame", EdgeLabel::FrameResolvesTo, "n:sig", "n:sym"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_TO_TOMBSTONED_RECORD),
            "frame into a tombstoned symbol must flag, got {codes:?}"
        );
    }

    #[test]
    fn emitted_during_edge_to_command_run_is_allowed() {
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            node("n:cmd", NodeKind::CommandRun),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
            edge("e:emit", EdgeLabel::EmittedDuring, "n:sig", "n:cmd"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "EMITTED_DURING→CommandRun must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn emitted_during_edge_to_wrong_kind_is_rejected() {
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            node("n:task", NodeKind::Task),
            edge("e:emit", EdgeLabel::EmittedDuring, "n:sig", "n:task"),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == EDGE_TARGET_KIND_VIOLATION),
            "EMITTED_DURING→Task must be rejected, got {:?}",
            report.diagnostics
        );
    }

    // ── Log-domain orphan detection (issue #327) ─────────────────────────────

    #[test]
    fn orphaned_log_event_is_flagged() {
        let records = vec![node("n:event", NodeKind::LogEvent)];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![ORPHAN_NODE]);
        assert_eq!(report.diagnostics[0].kind, Some("LogEvent"));
    }

    #[test]
    fn orphaned_log_bucket_is_flagged() {
        let records = vec![node("n:bucket", NodeKind::LogOccurrenceBucket)];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![ORPHAN_NODE]);
        assert_eq!(report.diagnostics[0].kind, Some("LogOccurrenceBucket"));
    }

    // ── Log-domain structural completeness (issue #327) ──────────────────────

    #[test]
    fn log_event_missing_captured_from_is_flagged() {
        // LogEvent with only FINGERPRINTED_AS — the CAPTURED_FROM is missing.
        // The signature carries its own CAPTURED_FROM (as the real extractor
        // emits) so only the event offends.
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            edge("e:sig-cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
            edge("e:fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
        ];
        let report = validate_records(&records);
        let missing: Vec<_> = report
            .diagnostics
            .iter()
            .filter(|d| d.code == MISSING_LOG_STRUCTURAL_EDGE)
            .collect();
        assert_eq!(missing.len(), 1, "got {:?}", report.diagnostics);
        assert_eq!(missing[0].record_id.as_deref(), Some("n:event"));
        assert_eq!(missing[0].kind, Some("LogEvent"));
        assert_eq!(missing[0].relation.as_deref(), Some("CAPTURED_FROM"));
    }

    #[test]
    fn log_event_missing_fingerprinted_as_is_flagged() {
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:source", NodeKind::LogSource),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
        ];
        let report = validate_records(&records);
        let missing: Vec<_> = report
            .diagnostics
            .iter()
            .filter(|d| d.code == MISSING_LOG_STRUCTURAL_EDGE)
            .collect();
        assert_eq!(missing.len(), 1, "got {:?}", report.diagnostics);
        assert_eq!(missing[0].relation.as_deref(), Some("FINGERPRINTED_AS"));
    }

    #[test]
    fn log_bucket_missing_aggregates_is_flagged() {
        let records = vec![
            node("n:bucket", NodeKind::LogOccurrenceBucket),
            node("n:source", NodeKind::LogSource),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:bucket", "n:source"),
        ];
        let report = validate_records(&records);
        let missing: Vec<_> = report
            .diagnostics
            .iter()
            .filter(|d| d.code == MISSING_LOG_STRUCTURAL_EDGE)
            .collect();
        assert_eq!(missing.len(), 1, "got {:?}", report.diagnostics);
        assert_eq!(missing[0].relation.as_deref(), Some("AGGREGATES"));
    }

    #[test]
    fn log_bucket_with_only_aggregates_is_complete() {
        // The extractor emits only AGGREGATES for a bucket — no CAPTURED_FROM.
        // A bucket with just its AGGREGATES edge must not be flagged. The
        // signature carries its own CAPTURED_FROM (as the real extractor emits)
        // so it is not itself a completeness defect.
        let records = vec![
            node("n:bucket", NodeKind::LogOccurrenceBucket),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            edge("e:sig-cap", EdgeLabel::CapturedFrom, "n:sig", "n:source"),
            edge("e:agg", EdgeLabel::Aggregates, "n:bucket", "n:sig"),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|d| d.code != MISSING_LOG_STRUCTURAL_EDGE),
            "bucket with AGGREGATES only must be complete, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn zero_edge_log_event_is_only_an_orphan_not_a_completeness_defect() {
        // An incident-gated completeness check must not double-report a
        // zero-edge log node: it stays a single orphan.
        let records = vec![node("n:event", NodeKind::LogEvent)];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![ORPHAN_NODE]);
    }

    #[test]
    fn log_event_may_capture_from_multiple_sources_is_clean() {
        // A LogEvent ID excludes the source (it is keyed on
        // repo/signature/valid-time/content-hash, `src/log_graph.rs:445-451`),
        // while each LogSource ID is path/hash-distinct, so combining two
        // scan-logs outputs where the same exemplar (same timestamp+template)
        // appears in two files yields ONE LogEvent carrying TWO distinct
        // CAPTURED_FROM edges to two different LogSources — a valid
        // multi-source aggregate, never a `duplicate_log_structural_edge`.
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source-a", NodeKind::LogSource),
            node("n:source-b", NodeKind::LogSource),
            edge("e:fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
            edge("e:cap-a", EdgeLabel::CapturedFrom, "n:event", "n:source-a"),
            edge("e:cap-b", EdgeLabel::CapturedFrom, "n:event", "n:source-b"),
            // The signature carries its own required CAPTURED_FROM to each
            // source so only the event's multi-source capture is under test.
            edge(
                "e:sig-cap-a",
                EdgeLabel::CapturedFrom,
                "n:sig",
                "n:source-a",
            ),
            edge(
                "e:sig-cap-b",
                EdgeLabel::CapturedFrom,
                "n:sig",
                "n:source-b",
            ),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|d| d.code != DUPLICATE_LOG_STRUCTURAL_EDGE),
            "a LogEvent capturing from multiple sources is a valid aggregate, got {:?}",
            report.diagnostics
        );
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn log_event_fingerprinted_to_two_signatures_is_flagged() {
        // FINGERPRINTED_AS stays exactly-one: a LogEvent is fingerprinted as
        // exactly one signature (the target is the source-excluded signature_id,
        // so two DIFFERENT signatures is a genuine malformed duplicate, not a
        // multi-source aggregate). Both signatures carry their own CAPTURED_FROM
        // so only the event's double-fingerprint offends.
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:sig-a", NodeKind::ErrorSignature),
            node("n:sig-b", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            edge("e:fp-a", EdgeLabel::FingerprintedAs, "n:event", "n:sig-a"),
            edge("e:fp-b", EdgeLabel::FingerprintedAs, "n:event", "n:sig-b"),
            edge("e:evt-cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
            edge(
                "e:sig-a-cap",
                EdgeLabel::CapturedFrom,
                "n:sig-a",
                "n:source",
            ),
            edge(
                "e:sig-b-cap",
                EdgeLabel::CapturedFrom,
                "n:sig-b",
                "n:source",
            ),
        ];
        let report = validate_records(&records);
        let dupes: Vec<_> = report
            .diagnostics
            .iter()
            .filter(|d| d.code == DUPLICATE_LOG_STRUCTURAL_EDGE)
            .collect();
        assert_eq!(dupes.len(), 1, "got {:?}", report.diagnostics);
        assert_eq!(dupes[0].record_id.as_deref(), Some("n:event"));
        assert_eq!(dupes[0].relation.as_deref(), Some("FINGERPRINTED_AS"));
        assert_eq!(
            dupes[0].stranded_edge_ids.as_deref(),
            Some(&["e:fp-a".to_owned(), "e:fp-b".to_owned()][..])
        );
    }

    #[test]
    fn duplicate_identical_captured_from_edge_records_are_not_flagged() {
        // Distinct edge RECORDS carrying the same edge ID (re-emitted) count
        // once — dedupe is by edge record ID.
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source", NodeKind::LogSource),
            edge("e:fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|d| d.code != DUPLICATE_LOG_STRUCTURAL_EDGE),
            "identical re-emitted edge records must not count twice, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn error_signature_missing_captured_from_is_flagged() {
        // An ErrorSignature made incident by an inbound FINGERPRINTED_AS from a
        // LogEvent, but carrying NO outbound CAPTURED_FROM, strands its own and
        // (transitively) its buckets' source attribution — a completeness
        // defect, not an orphan.
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            node("n:event", NodeKind::LogEvent),
            node("n:source", NodeKind::LogSource),
            // The event is well-formed so only the signature offends.
            edge("e:evt-fp", EdgeLabel::FingerprintedAs, "n:event", "n:sig"),
            edge("e:evt-cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
        ];
        let report = validate_records(&records);
        let missing: Vec<_> = report
            .diagnostics
            .iter()
            .filter(|d| d.code == MISSING_LOG_STRUCTURAL_EDGE)
            .collect();
        assert_eq!(missing.len(), 1, "got {:?}", report.diagnostics);
        assert_eq!(missing[0].record_id.as_deref(), Some("n:sig"));
        assert_eq!(missing[0].kind, Some("ErrorSignature"));
        assert_eq!(missing[0].relation.as_deref(), Some("CAPTURED_FROM"));
    }

    #[test]
    fn error_signature_may_capture_from_multiple_sources_is_clean() {
        // An ErrorSignature ID is a repo/fingerprint aggregate that excludes the
        // source (`src/log_graph.rs`), and scan-logs emits a distinct
        // CAPTURED_FROM per LogSource. A graph combining two log files that
        // share a fingerprint therefore gives ONE signature TWO CAPTURED_FROM
        // edges to two distinct LogSources — a valid aggregate, never a
        // `duplicate_log_structural_edge`. Both sources also carry their own
        // well-formed exemplar so nothing else offends.
        let records = vec![
            node("n:sig", NodeKind::ErrorSignature),
            node("n:source-a", NodeKind::LogSource),
            node("n:source-b", NodeKind::LogSource),
            node("n:event-a", NodeKind::LogEvent),
            node("n:event-b", NodeKind::LogEvent),
            edge("e:cap-a", EdgeLabel::CapturedFrom, "n:sig", "n:source-a"),
            edge("e:cap-b", EdgeLabel::CapturedFrom, "n:sig", "n:source-b"),
            edge("e:fp-a", EdgeLabel::FingerprintedAs, "n:event-a", "n:sig"),
            edge("e:fp-b", EdgeLabel::FingerprintedAs, "n:event-b", "n:sig"),
            edge(
                "e:evt-cap-a",
                EdgeLabel::CapturedFrom,
                "n:event-a",
                "n:source-a",
            ),
            edge(
                "e:evt-cap-b",
                EdgeLabel::CapturedFrom,
                "n:event-b",
                "n:source-b",
            ),
        ];
        let report = validate_records(&records);
        assert!(
            report
                .diagnostics
                .iter()
                .all(|d| d.code != DUPLICATE_LOG_STRUCTURAL_EDGE),
            "a signature capturing from multiple sources is a valid aggregate, got {:?}",
            report.diagnostics
        );
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn orphaned_error_signature_is_flagged() {
        // A lone ErrorSignature node with no edges at all is an orphan (a real
        // signature always carries its outbound CAPTURED_FROM, so it is never
        // zero-edge in practice).
        let records = vec![node("n:sig", NodeKind::ErrorSignature)];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![ORPHAN_NODE]);
        assert_eq!(report.diagnostics[0].kind, Some("ErrorSignature"));
    }

    #[test]
    fn fingerprinted_as_to_missing_signature_is_dangling() {
        let records = vec![
            node("n:event", NodeKind::LogEvent),
            node("n:source", NodeKind::LogSource),
            edge("e:fp", EdgeLabel::FingerprintedAs, "n:event", "n:absent"),
            edge("e:cap", EdgeLabel::CapturedFrom, "n:event", "n:source"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&DANGLING_EDGE_ENDPOINT),
            "missing FINGERPRINTED_AS target must dangle, got {codes:?}"
        );
    }

    #[test]
    fn reviewer_identity_edges_to_external_identity_are_allowed() {
        // Issue #335: REVIEWED_BY (Review→ExternalIdentity) and
        // REQUESTED_REVIEW_FROM (Task→ExternalIdentity) pass the target check.
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:task", NodeKind::Task),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rb", EdgeLabel::ReviewedBy, "n:review", "n:id"),
            edge("e:rrf", EdgeLabel::RequestedReviewFrom, "n:task", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "reviewer-identity edges to ExternalIdentity must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn reviewer_identity_edges_to_wrong_kind_are_rejected() {
        // Issue #335: a reviewer-identity edge targeting a non-ExternalIdentity
        // node is a target-kind violation.
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:sym", NodeKind::Symbol),
            edge("e:bad", EdgeLabel::ReviewedBy, "n:review", "n:sym"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "REVIEWED_BY→Symbol must be rejected, got {codes:?}"
        );
    }

    #[test]
    fn reviewer_identity_edges_from_correct_source_are_allowed() {
        // Issue #335: REVIEWED_BY originates from a `Review` and
        // REQUESTED_REVIEW_FROM originates from the PR `Task`; well-sourced
        // reviewer-identity edges pass the source-kind check.
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:task", NodeKind::Task),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rb", EdgeLabel::ReviewedBy, "n:review", "n:id"),
            edge("e:rrf", EdgeLabel::RequestedReviewFrom, "n:task", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_SOURCE_KIND_VIOLATION),
            "well-sourced reviewer-identity edges must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn reviewed_by_from_wrong_source_is_rejected() {
        // Issue #335: REVIEWED_BY must originate from a `Review`; a `Task`
        // source is invalid attribution the gate must reject.
        let records = vec![
            node("n:task", NodeKind::Task),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:bad", EdgeLabel::ReviewedBy, "n:task", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_SOURCE_KIND_VIOLATION),
            "Task—REVIEWED_BY→ExternalIdentity must be rejected, got {codes:?}"
        );
    }

    #[test]
    fn requested_review_from_wrong_source_is_rejected() {
        // Issue #335: REQUESTED_REVIEW_FROM must originate from the PR `Task`;
        // a `Review` source is invalid attribution the gate must reject.
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:bad", EdgeLabel::RequestedReviewFrom, "n:review", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_SOURCE_KIND_VIOLATION),
            "Review—REQUESTED_REVIEW_FROM→ExternalIdentity must be rejected, got {codes:?}"
        );
    }

    #[test]
    fn reviewer_identity_edges_with_correct_source_kind_are_clean() {
        // Issue #369: a REVIEWED_BY from a `github_review` Review and a
        // REQUESTED_REVIEW_FROM from a `github_pr` Task carry the importer
        // source_kind the daemon requires, so both pass the parity check.
        let records = vec![
            node_with_source_kind("n:review", NodeKind::Review, "github_review"),
            node_with_source_kind("n:task", NodeKind::Task, "github_pr"),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rb", EdgeLabel::ReviewedBy, "n:review", "n:id"),
            edge("e:rrf", EdgeLabel::RequestedReviewFrom, "n:task", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION),
            "correctly-attributed reviewer-identity edges must be clean, got {codes:?}"
        );
    }

    #[test]
    fn reviewed_by_from_non_github_review_source_kind_is_rejected() {
        // Issue #369: a REVIEWED_BY whose source is a node-kind-correct but
        // non-`github_review` Review (here: no importer source_kind at all —
        // a generic/hand-authored Review) is a binding the daemon rejects, so
        // the offline gate must reject it too. The node-kind check passes
        // (source is a Review), so the attribution defect is the only one.
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rb", EdgeLabel::ReviewedBy, "n:review", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION),
            "REVIEWED_BY from a source lacking source_kind github_review must be rejected, got {codes:?}"
        );
        assert!(
            !codes.contains(&EDGE_SOURCE_KIND_VIOLATION),
            "a node-kind-correct Review source must not also trip the node-kind check, got {codes:?}"
        );
        let defect = report
            .diagnostics
            .iter()
            .find(|d| d.code == EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION)
            .expect("attribution defect present");
        assert_eq!(defect.edge_id.as_deref(), Some("e:rb"));
        assert_eq!(defect.record_id.as_deref(), Some("n:review"));
        assert_eq!(defect.required_source_kind, Some("github_review"));
        assert_eq!(defect.source_kind, None);
    }

    #[test]
    fn requested_review_from_github_issue_task_is_rejected() {
        // Issue #369: the issue's canonical example — a `github_issue` Task
        // (node-kind-correct for REQUESTED_REVIEW_FROM, wrong source_kind) is a
        // binding the daemon rejects, so the offline validator must too.
        let records = vec![
            node_with_source_kind("n:task", NodeKind::Task, "github_issue"),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rrf", EdgeLabel::RequestedReviewFrom, "n:task", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION),
            "REQUESTED_REVIEW_FROM off a github_issue Task must be rejected, got {codes:?}"
        );
        let defect = report
            .diagnostics
            .iter()
            .find(|d| d.code == EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION)
            .expect("attribution defect present");
        assert_eq!(defect.required_source_kind, Some("github_pr"));
        assert_eq!(defect.source_kind.as_deref(), Some("github_issue"));
    }

    #[test]
    fn requested_review_from_shadowed_by_later_github_issue_is_rejected() {
        // Issue #369 parity: the daemon resolves a node's `source_kind` via
        // last-write-wins (`lookup_node_source_kind` reverse-scans the batch),
        // so two Task records sharing one id — first `github_pr`, then
        // `github_issue` — resolve to `github_issue`, and the daemon rejects a
        // REQUESTED_REVIEW_FROM off it. The offline validator must resolve the
        // same single value, not any-match over the set.
        let records = vec![
            node_with_source_kind("n:task", NodeKind::Task, "github_pr"),
            node_with_source_kind("n:task", NodeKind::Task, "github_issue"),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rrf", EdgeLabel::RequestedReviewFrom, "n:task", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION),
            "REQUESTED_REVIEW_FROM off a Task shadowed to github_issue must be rejected, got {codes:?}"
        );
        let defect = report
            .diagnostics
            .iter()
            .find(|d| d.code == EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION)
            .expect("attribution defect present");
        assert_eq!(defect.required_source_kind, Some("github_pr"));
        assert_eq!(defect.source_kind.as_deref(), Some("github_issue"));
    }

    #[test]
    fn reviewed_by_shadowed_by_later_source_kind_is_rejected() {
        // Issue #369 parity: two Review records sharing one id — first
        // `github_review`, then a non-`github_review` attribution — resolve to
        // the later value under last-write-wins, so REVIEWED_BY off it is a
        // binding the daemon rejects and the offline validator must too.
        let records = vec![
            node_with_source_kind("n:review", NodeKind::Review, "github_review"),
            node_with_source_kind("n:review", NodeKind::Review, "local_jsonl"),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rb", EdgeLabel::ReviewedBy, "n:review", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION),
            "REVIEWED_BY off a Review shadowed to local_jsonl must be rejected, got {codes:?}"
        );
        let defect = report
            .diagnostics
            .iter()
            .find(|d| d.code == EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION)
            .expect("attribution defect present");
        assert_eq!(defect.required_source_kind, Some("github_review"));
        assert_eq!(defect.source_kind.as_deref(), Some("local_jsonl"));
    }

    #[test]
    fn reviewer_identity_source_kind_recurring_same_value_stays_clean() {
        // Issue #369 parity guard: a node ID that recurs across history commits
        // with the SAME required source_kind still resolves to that value under
        // last-write-wins, so the legitimate recurring case must stay clean —
        // only the adversarial conflicting-value case changes behavior.
        let records = vec![
            node_with_source_kind("n:task", NodeKind::Task, "github_pr"),
            node_with_source_kind("n:task", NodeKind::Task, "github_pr"),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:rrf", EdgeLabel::RequestedReviewFrom, "n:task", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_SOURCE_KIND_ATTRIBUTION_VIOLATION),
            "a Task recurring with the same github_pr source_kind must stay clean, got {codes:?}"
        );
    }

    #[test]
    fn transitions_review_edge_from_transition_to_review_is_allowed() {
        // Issue #336: TRANSITIONS_REVIEW (ReviewStateTransition→Review) passes
        // both the source-kind and target-kind checks.
        let records = vec![
            node("n:trans", NodeKind::ReviewStateTransition),
            node("n:review", NodeKind::Review),
            edge("e:tr", EdgeLabel::TransitionsReview, "n:trans", "n:review"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&EDGE_TARGET_KIND_VIOLATION)
                && !codes.contains(&EDGE_SOURCE_KIND_VIOLATION),
            "well-formed TRANSITIONS_REVIEW must be allowed, got {codes:?}"
        );
    }

    #[test]
    fn transitions_review_to_wrong_target_is_rejected() {
        // Issue #336: TRANSITIONS_REVIEW must terminate at a Review.
        let records = vec![
            node("n:trans", NodeKind::ReviewStateTransition),
            node("n:id", NodeKind::ExternalIdentity),
            edge("e:bad", EdgeLabel::TransitionsReview, "n:trans", "n:id"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_TARGET_KIND_VIOLATION),
            "TRANSITIONS_REVIEW→ExternalIdentity must be rejected, got {codes:?}"
        );
    }

    #[test]
    fn transitions_review_from_wrong_source_is_rejected() {
        // Issue #336: TRANSITIONS_REVIEW must originate from a
        // ReviewStateTransition; a Review source is invalid attribution.
        let records = vec![
            node("n:review", NodeKind::Review),
            node("n:review2", NodeKind::Review),
            edge(
                "e:bad",
                EdgeLabel::TransitionsReview,
                "n:review",
                "n:review2",
            ),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&EDGE_SOURCE_KIND_VIOLATION),
            "Review—TRANSITIONS_REVIEW→Review must be rejected, got {codes:?}"
        );
    }
}
