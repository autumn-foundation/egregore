use std::collections::BTreeSet;

use super::{SymbolContext, record_context};
use crate::ir::{EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SemanticDriftMetadata};

/// A single semantic retrieval lead handed to [`semantic_context_bundle`].
///
/// Decoupled from the embeddings-feature `SemanticMatch` so the bridge — and
/// its tests — need no embedding model: callers (the CLI) convert each
/// `SemanticMatch` into one of these before context resolution. Carries only
/// the bounded retrieval-lead fields (record id, optional name/path/span, and
/// the relevance score), never raw content.
#[derive(Debug, Clone)]
pub struct SemanticLead {
    /// Stable record ID of the matched node.
    pub record_id: String,
    /// Human-readable name when the match carries one (absent for File nodes).
    pub name: Option<String>,
    /// Repository-relative path when available.
    pub repo_relative_path: Option<String>,
    /// Relevance score (higher = more similar).
    pub score: f32,
    /// Source span when available.
    pub span: Option<crate::ir::SourceSpan>,
}

/// How a semantic match anchored its context. Documents how File matches differ
/// from Symbol matches in the response (AC3 of #90).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AnchorKind {
    /// The match resolved to a `Symbol` node.
    Symbol,
    /// The match resolved to a `File` node (no symbol name; defined symbols are
    /// seeded into the context instead).
    File,
    /// The match resolved to some other embeddable node kind.
    Other,
}

impl AnchorKind {
    /// Stable lowercase tag for serialization.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::Other => "other",
        }
    }
}

/// One semantic match expanded into evidence-backed context.
pub struct SemanticMatchContext<'a> {
    /// The retrieval lead (handle + score) that produced this row.
    pub lead: SemanticLead,
    /// Whether the match anchored on a Symbol, File, or other node.
    pub anchor_kind: AnchorKind,
    /// Every candidate record ID when the match name resolves to more than one
    /// live symbol (AC4 — ambiguity is surfaced, not guessed). Sorted and
    /// deduplicated; empty when the match is unambiguous.
    pub candidate_record_ids: Vec<String>,
    /// The trust-separated context anchored on the match's record ID.
    pub context: SymbolContext<'a>,
}

/// The combined natural-language → evidence-backed-context answer.
pub struct SemanticContextBundle<'a> {
    /// One row per lead that cleared the relevance floor, in ranking order.
    pub matches: Vec<SemanticMatchContext<'a>>,
}

impl SemanticContextBundle<'_> {
    /// Returns `true` when no lead cleared the relevance floor — the documented
    /// no-match condition (AC7). Callers MUST check this before reading
    /// `matches`; the CLI maps it to a stable diagnostic and a distinct exit
    /// code rather than an empty success.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.matches.is_empty()
    }
}

/// Bridges ranked semantic leads into evidence-backed context (#90).
///
/// For each lead whose `score` is at or above `min_score` — the documented
/// relevance floor — in the leads' given (already-deterministic) ranking order,
/// resolves [`record_context`] anchored on the lead's record ID. File-typed
/// leads are first-class (defined symbols are seeded); an ambiguous symbol name
/// surfaces every candidate record ID instead of silently picking one. This
/// consumes the existing semantic ranking and symbol-context contracts and adds
/// no new domain, schema, or model. Read-only: it borrows `records` and mutates
/// nothing, and identical inputs produce identical output.
#[must_use]
pub fn semantic_context_bundle<'a>(
    records: &'a [GraphRecord],
    leads: &[SemanticLead],
    min_score: f32,
) -> SemanticContextBundle<'a> {
    let mut matches = Vec::new();
    for lead in leads {
        if lead.score < min_score {
            continue;
        }
        let anchor_kind = match record_kind(records, &lead.record_id) {
            Some(NodeKind::Symbol) => AnchorKind::Symbol,
            Some(NodeKind::File) => AnchorKind::File,
            _ => AnchorKind::Other,
        };
        let candidate_record_ids = lead
            .name
            .as_deref()
            .map(|name| live_symbol_ids_for_name(records, name))
            .filter(|ids| ids.len() > 1)
            .unwrap_or_default();
        let context = record_context(records, &lead.record_id);
        matches.push(SemanticMatchContext {
            lead: lead.clone(),
            anchor_kind,
            candidate_record_ids,
            context,
        });
    }
    SemanticContextBundle { matches }
}

/// Node kind for a record id (from any version present in the slice).
fn record_kind(records: &[GraphRecord], id: &str) -> Option<NodeKind> {
    records.iter().find_map(|r| match r {
        GraphRecord::Node { id: nid, kind, .. } if nid == id => Some(*kind),
        _ => None,
    })
}

/// All live `Symbol` record IDs matching `name`, sorted and deduplicated.
///
/// Mirrors the current-state filter used by [`symbol_context`]: a historical
/// (temporal) version survives a current-state tombstone; a tombstoned
/// current-state symbol is excluded.
fn live_symbol_ids_for_name(records: &[GraphRecord], name: &str) -> Vec<String> {
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let mut ids: Vec<String> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name: Some(n),
                temporal,
                ..
            } = r
            else {
                return None;
            };
            if n != name {
                return None;
            }
            let is_historical = temporal.is_some();
            if !is_historical && tombstoned.contains(id.as_str()) {
                return None;
            }
            Some(id.clone())
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Returns `true` for edge labels that cross domain boundaries and therefore
/// signal a meaningful link to a symbol for the context query.
///
/// This is a superset of the previous list: `RelatesTo`, `Contradicts`, and
/// `Supersedes` are now included because they are permitted cross-domain
/// evidence-link labels and can legally appear on graph edges between an
/// agent-memory/project node and a code-graph symbol.
pub(super) const fn is_cross_domain_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::Observes
            | EdgeLabel::MentionsSymbol
            | EdgeLabel::ValidatedBy
            | EdgeLabel::HasEvidence
            | EdgeLabel::ReferencesTask
            | EdgeLabel::FailedOn
            | EdgeLabel::ExplainsChange
            | EdgeLabel::TouchedFile
            | EdgeLabel::TouchesFile
            | EdgeLabel::ProducedPatch
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ClosesAcceptanceCriterion
            | EdgeLabel::RelatesTo
            | EdgeLabel::Contradicts
            | EdgeLabel::Supersedes
            | EdgeLabel::OwnedByTask
            // Reviewer-identity edges (issue #335): a reachable Review/Task
            // links to its author/requested reviewer identity, so the BFS must
            // follow REVIEWED_BY / REQUESTED_REVIEW_FROM to surface the identity.
            | EdgeLabel::ReviewedBy
            | EdgeLabel::RequestedReviewFrom
    )
}

/// Returns `true` for edge labels that should only be traversed in the
/// forward direction (source → target) during BFS.
///
/// These labels all point FROM agent-memory/project nodes TOWARD shared sinks
/// (verification runs, artifacts, tasks). Traversing backward from the sink
/// would pull in unrelated sibling nodes that happen to reference the same
/// sink but have no connection to the queried symbol. For example, if two
/// observations are both validated by the same `CommandRun`, following
/// `VALIDATED_BY` backward from the run would classify the unrelated
/// observation as context.
///
/// `ClosesAcceptanceCriterion` has schema direction AC → Verification. Making
/// it forward-only prevents backward traversal from a Verification sink to
/// unrelated `AcceptanceCriteria` that happen to share the same run.
///
/// `ExplainsChange` is intentionally NOT forward-only: its schema direction
/// is Observation → Symbol/File (the same as `MentionsSymbol`). Backward
/// traversal from the Symbol/File seed is required to discover the explaining
/// Observation.
pub(super) const fn is_forward_only_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::ValidatedBy
            | EdgeLabel::HasEvidence
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ProducedPatch
            | EdgeLabel::ReferencesTask
            | EdgeLabel::ClosesAcceptanceCriterion
            // Reviewer-identity edges point FROM a Review/Task TOWARD a shared
            // identity sink (issue #335). Forward-only so reaching one identity
            // never pulls in sibling reviews/tasks that share the same author.
            | EdgeLabel::ReviewedBy
            | EdgeLabel::RequestedReviewFrom
    )
}

/// Returns `true` when `record_id` identifies a node that should expand the BFS
/// frontier even though it has no output context section.
///
/// "Relay" nodes are infrastructure connectors that bridge classifiable sections:
/// - [`NodeKind::ToolCall`]: `TOUCHED_FILE → File` backward traversal discovers the
///   `ToolCall`; its `PRODUCED_EVIDENCE` forward edges then reach `CommandRun`/`TestRun`.
///
/// Relay expansion is only allowed for nodes that are present in `by_id` and
/// not tombstoned (current-state). Temporal relay nodes with the same stable ID
/// as a current-state tombstone are exempt — the tombstone reflects only the
/// current state; the historical relay must still bridge its edges.
pub(super) fn is_bfs_relay_node(
    record_id: &str,
    by_id: &std::collections::BTreeMap<&str, &GraphRecord>,
    tombstoned_ids: &BTreeSet<&str>,
    has_any_temporal_version: &BTreeSet<&str>,
) -> bool {
    if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
        return false;
    }
    let Some(rec) = by_id.get(record_id) else {
        return false;
    };
    matches!(
        rec,
        GraphRecord::Node {
            kind: NodeKind::ToolCall | NodeKind::AgentTurn | NodeKind::AgentRun,
            ..
        }
    )
}

/// Constructs an unresolved-ref handle string from the triple fields of an
/// `EvidenceLink` that has no `target_record_id`.
///
/// Returns `None` when none of the triple fields are present (link is unusable).
/// Format: `{path}:{start}..{end}@{commit}` when all fields present; subsets
/// when only some are available.
pub(super) fn evidence_link_triple_handle(link: &EvidenceLink) -> Option<String> {
    let path = link.target_repo_relative_path.as_deref()?;
    Some(match (&link.target_span, &link.target_git_commit) {
        (Some(span), Some(commit)) => {
            format!("{path}:{}..{}@{commit}", span.start_line, span.end_line)
        }
        (Some(span), None) => format!("{path}:{}..{}", span.start_line, span.end_line),
        (None, Some(commit)) => format!("{path}@{commit}"),
        (None, None) => path.to_owned(),
    })
}

pub(super) fn semantic_drift(record: &GraphRecord) -> Option<&SemanticDriftMetadata> {
    let GraphRecord::Node {
        kind,
        semantic_drift,
        ..
    } = record
    else {
        return None;
    };
    if *kind == NodeKind::SemanticDrift {
        semantic_drift.as_deref()
    } else {
        None
    }
}

pub(super) const fn drift_score(drift: &SemanticDriftMetadata) -> f64 {
    drift.score
}

// ── Subsystem-scoped cross-domain context query (issue #83) ──────────────────
