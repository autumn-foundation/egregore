//! Surface recorded contradicting observations on a shared code target
//! (issue #232).
//!
//! This lane is strictly a *surface*: it returns the pairs (or clusters) of
//! records joined by an existing `CONTRADICTS` edge, scoped to a symbol name,
//! a repo-relative file path, or a subsystem path prefix. It never infers
//! semantic disagreement — adjudication stays with the operator.
//!
//! # Party eligibility
//!
//! Only three provenance classes can be a conflict party, drawn from the
//! lane's own vocabulary ([`ConflictPartyClass`]):
//!
//! - `agent_authored` — agent-authored claims (`Observation`, `Decision`,
//!   `Failure`), labeled with the derived [`TrustClass`] (`agent_verified` /
//!   `agent_unverified` / `agent_contradicted`);
//! - `verification` — recorded verification executions (`TestRun`,
//!   `CommandRun`, `CIStatus`, …), labeled `verification_evidence`, so a
//!   passing test contradicting a recorded `Failure` belief is clearly marked
//!   as verification trust and can adjudicate the disagreement;
//! - `user_context` — user-context records (`Preference`, `WorkflowRule`,
//!   …, `PromoteCandidate`), labeled `other` (preference trust is out of
//!   scope for the derived vocabulary).
//!
//! Deterministic code facts (`source_derived`: `Symbol`, `File`, …) are NEVER
//! a conflict party. A `CONTRADICTS` edge touching an ineligible endpoint is
//! skipped whole — the conflict is a pair, and a pair cannot be formed with a
//! party the lane refuses to name.
//!
//! # Resolution
//!
//! A conflict where any party carries `superseded_by` naming a live record is
//! marked `resolved` and excluded by default; `--include-resolved` shows it.
//! Stale disagreements therefore never masquerade as live ones, and the older
//! record is never deleted, hidden, or rewritten — this is a read-time
//! annotation only.
//!
//! # Determinism
//!
//! Conflicts are keyed by the unordered party pair and emitted in
//! record-ID order; parties, edge IDs, citation handles, and `resolved_by`
//! lists are all sorted. Repeated runs over an unchanged store are
//! byte-identical.

use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{RepositoryIndex, TrustClass, TrustIndex, is_verification_kind, record_node_kind};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// Which handle type a conflicts scope resolved from.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ConflictScopeKind {
    /// A code symbol (canonical record ID or exact name).
    Symbol,
    /// A repo-relative file path (or canonical file record ID).
    File,
    /// A repo-relative path prefix (subsystem).
    Subsystem,
}

impl ConflictScopeKind {
    /// Stable wire string for the resolved scope type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::Subsystem => "subsystem",
        }
    }
}

/// A resolved conflicts scope: the code target(s) conflicts are filtered by.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ResolvedConflictScope {
    /// Original handle as provided by the operator.
    pub handle: String,
    /// Which handle type matched.
    pub kind: ConflictScopeKind,
    /// Live code record IDs the scope covers (symbols/files under it).
    pub anchor_ids: BTreeSet<String>,
    /// The shared target handle emitted on every returned party.
    pub target_handle: String,
    /// True when the handle named a record that exists only as a tombstone.
    pub stale: bool,
}

impl ResolvedConflictScope {
    /// Returns true when the handle resolved to nothing live in the store, so
    /// the caller emits a `no_match` (or `stale_handle`) envelope.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor_ids.is_empty()
    }
}

/// Error returned when resolving a conflicts scope.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ConflictScopeError {
    /// The handle matched targets in more than one repository without `--repo`.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The candidate record IDs the handle resolved to.
        candidates: Vec<String>,
    },
    /// The handle is malformed (empty or a malformed canonical ID).
    Unsupported {
        /// The query handle.
        handle: String,
        /// Why the handle is unsupported.
        message: String,
    },
}

/// The lane's provenance vocabulary for conflict parties (issue #232).
///
/// This is a *participation* vocabulary, not a ranking: it names which
/// records may appear as a party at all. Each party additionally carries its
/// derived [`TrustClass`] label.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ConflictPartyClass {
    /// Agent-authored claims (`Observation`, `Decision`, `Failure`).
    AgentAuthored,
    /// Recorded verification executions (`TestRun`, `CommandRun`, …).
    Verification,
    /// User-context records (`Preference`, `WorkflowRule`, …).
    UserContext,
}

impl ConflictPartyClass {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentAuthored => "agent_authored",
            Self::Verification => "verification",
            Self::UserContext => "user_context",
        }
    }
}

/// User-context node kinds eligible as conflict parties.
const fn is_user_context_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::PromoteCandidate
            | NodeKind::PromotionPrompt
            | NodeKind::PromotionDecision
            | NodeKind::Preference
            | NodeKind::WorkflowRule
            | NodeKind::NamingDecision
            | NodeKind::Constraint
    )
}

/// Classifies a node record into the lane's party vocabulary, or `None` when
/// the record can never be a conflict party.
///
/// Deterministic code facts (`source_derived`), project state, artifacts,
/// runtime observations, and agent-memory scaffolding are ineligible: the
/// lane surfaces disagreements *between* recorded claims, never a claim
/// against source truth.
#[must_use]
pub fn conflict_party_class(record: &GraphRecord, trust: TrustClass) -> Option<ConflictPartyClass> {
    let kind = record_node_kind(record)?;
    if trust.is_agent_authored() {
        return Some(ConflictPartyClass::AgentAuthored);
    }
    if trust == TrustClass::VerificationEvidence {
        debug_assert!(
            is_verification_kind(kind),
            "verification_evidence trust on non-verification kind {kind:?}"
        );
        return Some(ConflictPartyClass::Verification);
    }
    if is_user_context_kind(kind) {
        return Some(ConflictPartyClass::UserContext);
    }
    None
}

/// Which end of the `CONTRADICTS` edge a party sits on.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ConflictEdgeRole {
    /// The edge's `source` endpoint.
    Source,
    /// The edge's `target` endpoint.
    Target,
}

impl ConflictEdgeRole {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
        }
    }
}

/// One side of a recorded disagreement.
#[derive(Debug, Clone)]
pub struct ConflictParty<'a> {
    /// The party's node record.
    pub record: &'a GraphRecord,
    /// Lane provenance class (`agent_authored` / `verification` / `user_context`).
    pub party_class: ConflictPartyClass,
    /// Derived trust class (issue #114 vocabulary).
    pub trust_class: TrustClass,
    /// Which end of the `CONTRADICTS` edge this party sits on.
    pub edge_role: ConflictEdgeRole,
    /// Citation handles: the record's own ID first, then every evidence-link
    /// target record ID, sorted and deduplicated. Non-empty by construction.
    pub citation_handles: Vec<&'a str>,
    /// The shared target handle the scope resolved to.
    pub target_handle: &'a str,
}

/// One recorded disagreement: the parties joined by `CONTRADICTS` edge(s).
///
/// Grouped by unordered party pair, so several parallel edges between the
/// same two records form one conflict (a "cluster" of edges, still a pair of
/// parties) rather than duplicated rows.
#[derive(Debug, Clone)]
pub struct Conflict<'a> {
    /// Unordered party pair key `(min_record_id, max_record_id)` — the
    /// stable conflict identity; conflicts sort by this key.
    pub key: (&'a str, &'a str),
    /// `CONTRADICTS` edge record IDs joining the pair, sorted.
    pub edge_ids: Vec<&'a str>,
    /// The parties, in record-ID order.
    pub parties: Vec<ConflictParty<'a>>,
    /// True when any party is superseded by a later record.
    pub resolved: bool,
    /// Superseding record IDs (sorted, deduplicated); empty when unresolved.
    pub resolved_by: Vec<&'a str>,
}

/// Deterministic tallies for the conflicts result.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ConflictsCounts {
    /// `CONTRADICTS` edge records seen in the store slice.
    pub contradicts_edges_considered: usize,
    /// Conflicts formed (eligible pairs), before the resolved filter.
    pub conflicts_found: usize,
    /// Conflicts returned after the resolved filter.
    pub conflicts_returned: usize,
    /// Resolved conflicts excluded by the default filter.
    pub conflicts_resolved_excluded: usize,
    /// Edges skipped because an endpoint is not an eligible party
    /// (deterministic code facts, project state, missing endpoints, …).
    pub edges_skipped_ineligible_party: usize,
}

/// A stable machine-readable condition attached to the result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ConflictDiagnostic {
    /// Stable diagnostic code (`no_recorded_conflicts`).
    pub code: &'static str,
    /// Bounded human-readable detail (handles and counts only).
    pub detail: String,
}

/// The conflicts answer for a resolved scope.
#[derive(Debug, Default, Clone)]
pub struct ConflictsContext<'a> {
    /// The scope the query resolved.
    pub scope: Option<ResolvedConflictScope>,
    /// Conflicts in scope, in `(min_record_id, max_record_id)` order.
    pub conflicts: Vec<Conflict<'a>>,
    /// Deterministic tallies.
    pub counts: ConflictsCounts,
    /// Stable diagnostics.
    pub diagnostics: Vec<ConflictDiagnostic>,
}

/// Returns `true` when `handle` is a well-formed canonical codegraph record ID.
fn is_well_formed_codegraph_id(handle: &str) -> bool {
    let parts: Vec<&str> = handle.split(':').collect();
    parts.len() == 3
        && parts[0] == "codegraph"
        && parts[1].starts_with('v')
        && parts[1].len() > 1
        && parts[1][1..].chars().all(|c| c.is_ascii_digit())
        && parts[2].len() == 64
        && parts[2].chars().all(|c| c.is_ascii_hexdigit())
}

/// Reports cross-repository ambiguity for a match set, mirroring the sibling
/// lanes: without `--repo`, a handle matching records owned by more than one
/// repository (or mixing owned and unattributed records) is ambiguous.
fn cross_repo_ambiguity(
    matches: &BTreeSet<String>,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Option<Vec<String>> {
    if repo_scope.is_some() {
        return None;
    }
    let owners: BTreeSet<Option<&str>> = matches.iter().map(|id| repo_index.owner_of(id)).collect();
    if owners.len() > 1 {
        Some(matches.iter().cloned().collect())
    } else {
        None
    }
}

/// Resolves a conflicts scope handle.
///
/// Accepts a canonical code record ID, an exact symbol name, a repo-relative
/// file path, or a subsystem path prefix — consistent with the sibling lanes
/// (`query context`, `query failures`).
///
/// # Errors
///
/// Returns [`ConflictScopeError::Unsupported`] for an empty handle or a
/// malformed canonical ID, and [`ConflictScopeError::Ambiguous`] when the
/// handle matches targets in more than one repository without `--repo`.
pub fn resolve_conflict_scope(
    records: &[GraphRecord],
    handle: &str,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<ResolvedConflictScope, ConflictScopeError> {
    if handle.is_empty() {
        return Err(ConflictScopeError::Unsupported {
            handle: handle.to_owned(),
            message: "scope cannot be empty".to_owned(),
        });
    }

    // Latest-write-wins tombstone liveness (issue #421), mirroring the
    // sibling lanes: a record re-ingested after its own tombstone is live
    // again, matching the embedded current-state read.
    let liveness = Liveness::new(records);
    let deleted = |id: &str| liveness.deleted(id);
    let in_scope = |id: &str| -> bool {
        repo_scope.is_none_or(|scope| repo_index.owner_of(id) == Some(scope))
    };

    let empty =
        |kind: ConflictScopeKind, target_handle: String, stale: bool| ResolvedConflictScope {
            handle: handle.to_owned(),
            kind,
            anchor_ids: BTreeSet::new(),
            target_handle,
            stale,
        };
    let resolved =
        |kind: ConflictScopeKind, anchor_ids: BTreeSet<String>, target_handle: String| {
            ResolvedConflictScope {
                handle: handle.to_owned(),
                kind,
                anchor_ids,
                target_handle,
                stale: false,
            }
        };

    // 1) Canonical code record ID (codegraph:vN:<hex>). A malformed canonical
    //    ID is unsupported; a well-formed but absent or out-of-scope ID
    //    resolves to nothing (caller emits no_match).
    if handle.starts_with("codegraph:") {
        if !is_well_formed_codegraph_id(handle) {
            return Err(ConflictScopeError::Unsupported {
                handle: handle.to_owned(),
                message: "malformed canonical codegraph ID".to_owned(),
            });
        }
        if deleted(handle) {
            return Ok(empty(ConflictScopeKind::Symbol, handle.to_owned(), true));
        }
        for r in records {
            if let GraphRecord::Node { id, kind, .. } = r
                && id == handle
                && in_scope(handle)
            {
                let kind = match kind {
                    NodeKind::File => ConflictScopeKind::File,
                    NodeKind::Symbol => ConflictScopeKind::Symbol,
                    _ => return Ok(empty(ConflictScopeKind::Symbol, handle.to_owned(), false)),
                };
                let mut anchor_ids = BTreeSet::new();
                anchor_ids.insert(handle.to_owned());
                return Ok(resolved(kind, anchor_ids, handle.to_owned()));
            }
        }
        return Ok(empty(ConflictScopeKind::Symbol, handle.to_owned(), false));
    }

    let mut saw_tombstoned = false;

    // 2) Repo-relative file path (exact match).
    let mut file_matches: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
            && path == handle
            && in_scope(id)
        {
            if deleted(id.as_str()) {
                saw_tombstoned = true;
            } else {
                file_matches.insert(id.clone());
            }
        }
    }
    if !file_matches.is_empty() {
        if let Some(candidates) = cross_repo_ambiguity(&file_matches, repo_index, repo_scope) {
            return Err(ConflictScopeError::Ambiguous {
                handle: handle.to_owned(),
                candidates,
            });
        }
        // A file scope covers the symbols the file defines: a recorded
        // disagreement about a symbol in the file is a disagreement about
        // the file. Only live symbols join the anchor set.
        let symbol_ids: BTreeSet<&str> = records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Node {
                    id,
                    kind: NodeKind::Symbol,
                    ..
                } if !deleted(id.as_str()) && in_scope(id) => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let mut anchors = file_matches.clone();
        for r in records {
            if let GraphRecord::Edge {
                source,
                target,
                label: EdgeLabel::Defines,
                ..
            } = r
                && !deleted(r.id())
                && file_matches.contains(source.as_str())
                && symbol_ids.contains(target.as_str())
            {
                anchors.insert(target.clone());
            }
        }
        let target_handle = single_or_handle(&file_matches, handle);
        return Ok(resolved(ConflictScopeKind::File, anchors, target_handle));
    }

    // 3) Exact symbol name. Several same-named symbols in one repository form
    //    a multi-ID scope; the same name across repositories is ambiguous.
    let mut symbol_matches: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name: Some(name),
            ..
        } = r
            && name == handle
            && in_scope(id)
        {
            if deleted(id.as_str()) {
                saw_tombstoned = true;
            } else {
                symbol_matches.insert(id.clone());
            }
        }
    }
    if !symbol_matches.is_empty() {
        if let Some(candidates) = cross_repo_ambiguity(&symbol_matches, repo_index, repo_scope) {
            return Err(ConflictScopeError::Ambiguous {
                handle: handle.to_owned(),
                candidates,
            });
        }
        let target_handle = single_or_handle(&symbol_matches, handle);
        return Ok(resolved(
            ConflictScopeKind::Symbol,
            symbol_matches,
            target_handle,
        ));
    }

    // 4) Subsystem path prefix: every live File/Symbol under the prefix.
    //    A trailing slash is normalized away; a bare "/" or empty prefix is
    //    unsupported rather than a whole-store wildcard.
    let normalized = handle.trim_end_matches('/');
    if normalized.is_empty() {
        return Err(ConflictScopeError::Unsupported {
            handle: handle.to_owned(),
            message: "subsystem prefix cannot be empty".to_owned(),
        });
    }
    let under_prefix = |path: &str| super::subsystem::path_is_under_prefix(path, normalized);
    let mut prefix_matches: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind,
            repo_relative_path: Some(path),
            ..
        } = r
            && matches!(kind, NodeKind::File | NodeKind::Symbol)
            && under_prefix(path)
            && in_scope(id)
        {
            if deleted(id.as_str()) {
                saw_tombstoned = true;
            } else {
                prefix_matches.insert(id.clone());
            }
        }
    }
    if !prefix_matches.is_empty() {
        if let Some(candidates) = cross_repo_ambiguity(&prefix_matches, repo_index, repo_scope) {
            return Err(ConflictScopeError::Ambiguous {
                handle: handle.to_owned(),
                candidates,
            });
        }
        return Ok(resolved(
            ConflictScopeKind::Subsystem,
            prefix_matches,
            normalized.to_owned(),
        ));
    }

    // Nothing live matched: stale when the handle only named tombstoned
    // records, otherwise a plain no-match. A prefix that matches no scanned
    // record is "scope not found", distinct from a scanned scope with no
    // recorded conflicts (exit 0, empty list).
    Ok(empty(
        ConflictScopeKind::Subsystem,
        normalized.to_owned(),
        saw_tombstoned,
    ))
}

/// The shared target handle for a match set: the single anchor record ID when
/// the scope resolved to exactly one record, otherwise the handle as given.
fn single_or_handle(matches: &BTreeSet<String>, handle: &str) -> String {
    let mut ids = matches.iter();
    match (ids.next(), ids.next()) {
        (Some(only), None) => only.clone(),
        _ => handle.to_owned(),
    }
}

/// Citation handles for one party: the record's own ID first, then every
/// evidence-link target record ID, sorted and deduplicated. Non-empty by
/// construction — the record ID is always citable.
fn party_citation_handles(record: &GraphRecord) -> Vec<&str> {
    let mut handles = BTreeSet::new();
    handles.insert(record.id());
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = record
    {
        for link in links {
            if let Some(target) = link.target_record_id.as_deref() {
                handles.insert(target);
            }
        }
    }
    handles.into_iter().collect()
}

/// Builds the conflicts answer for a resolved scope.
///
/// Only recorded `CONTRADICTS` edges are surfaced — the lane never infers
/// disagreement. A conflict is in scope when at least one party cites (via an
/// evidence-link target record ID) a code record the scope covers.
#[must_use]
pub fn conflicts_context<'a>(
    records: &'a [GraphRecord],
    scope: &'a ResolvedConflictScope,
    include_resolved: bool,
) -> ConflictsContext<'a> {
    let liveness = Liveness::new(records);
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let trust_index = TrustIndex::build(records);
    let anchors: BTreeSet<&str> = scope.anchor_ids.iter().map(String::as_str).collect();

    let mut counts = ConflictsCounts::default();
    // Group eligible edges by unordered endpoint pair; BTreeMap iteration is
    // already in (min_id, max_id) order, which is the output order.
    let mut pairs: BTreeMap<(&str, &str), Vec<&GraphRecord>> = BTreeMap::new();

    for record in records {
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
        if *label != EdgeLabel::Contradicts {
            continue;
        }
        counts.contradicts_edges_considered += 1;
        if liveness.deleted(id.as_str()) {
            counts.edges_skipped_ineligible_party += 1;
            continue;
        }
        let (Some(source_node), Some(target_node)) =
            (by_id.get(source.as_str()), by_id.get(target.as_str()))
        else {
            counts.edges_skipped_ineligible_party += 1;
            continue;
        };
        if liveness.deleted(source_node.id()) || liveness.deleted(target_node.id()) {
            counts.edges_skipped_ineligible_party += 1;
            continue;
        }
        if source == target {
            // A self-contradiction is not a pair of parties.
            counts.edges_skipped_ineligible_party += 1;
            continue;
        }
        let source_eligible =
            conflict_party_class(source_node, trust_index.classify(source_node)).is_some();
        let target_eligible =
            conflict_party_class(target_node, trust_index.classify(target_node)).is_some();
        if !(source_eligible && target_eligible) {
            // Deterministic code facts (and every other ineligible class) can
            // never be a conflict party: the whole edge is skipped.
            counts.edges_skipped_ineligible_party += 1;
            continue;
        }
        let key = if source < target {
            (source.as_str(), target.as_str())
        } else {
            (target.as_str(), source.as_str())
        };
        pairs.entry(key).or_default().push(record);
    }

    let mut ctx = ConflictsContext {
        scope: Some(scope.clone()),
        ..ConflictsContext::default()
    };

    for ((min_id, max_id), edges) in &pairs {
        // Peel the iteration references once so every downstream use is a
        // plain `&str` / `&GraphRecord` (avoids relying on coercion sites).
        let min_id: &str = min_id;
        let max_id: &str = max_id;
        // The pair was built from endpoints found in `by_id` over the same
        // slice, so the lookups cannot miss; skip defensively regardless —
        // a missing endpoint must never panic the lane.
        let (Some(source_node), Some(target_node)) = (by_id.get(min_id), by_id.get(max_id)) else {
            continue;
        };
        let source_node: &GraphRecord = source_node;
        let target_node: &GraphRecord = target_node;
        // Re-derive classes here (cheap, deterministic) rather than threading
        // them through the grouping map.
        let classes = [
            conflict_party_class(source_node, trust_index.classify(source_node)),
            conflict_party_class(target_node, trust_index.classify(target_node)),
        ];
        let [Some(min_class), Some(max_class)] = classes else {
            continue;
        };
        let nodes = [
            (min_id, source_node, min_class),
            (max_id, target_node, max_class),
        ];

        // Scope filter: at least one party cites a covered code record.
        let in_scope = nodes.iter().any(|(_, node, _)| {
            party_citation_handles(node)
                .iter()
                .any(|h| anchors.contains(h))
        });
        if !in_scope {
            continue;
        }

        counts.conflicts_found += 1;

        // Resolution: any party superseded_by a LIVE later record resolves
        // the conflict. A dangling superseded_by (target absent from the
        // slice) does not resolve — a phantom pointer must not hide a live
        // disagreement.
        let mut resolved_by: BTreeSet<&str> = BTreeSet::new();
        for (_, node, _) in &nodes {
            if let GraphRecord::Node {
                superseded_by: Some(sup_id),
                ..
            } = node
                && let Some(sup_node) = by_id.get(sup_id.as_str())
                && !liveness.deleted(sup_node.id())
            {
                resolved_by.insert(sup_node.id());
            }
        }
        let resolved = !resolved_by.is_empty();
        if resolved && !include_resolved {
            counts.conflicts_resolved_excluded += 1;
            continue;
        }

        let mut edge_ids: Vec<&str> = edges.iter().map(|e| e.id()).collect();
        edge_ids.sort_unstable();

        let parties: Vec<ConflictParty<'_>> = nodes
            .iter()
            .map(|(id, node, class)| {
                let id: &str = id;
                let node: &GraphRecord = node;
                ConflictParty {
                    record: node,
                    party_class: *class,
                    trust_class: trust_index.classify(node),
                    edge_role: edge_role_for(edges, id),
                    citation_handles: party_citation_handles(node),
                    target_handle: scope.target_handle.as_str(),
                }
            })
            .collect();

        ctx.conflicts.push(Conflict {
            key: (min_id, max_id),
            edge_ids,
            parties,
            resolved,
            resolved_by: resolved_by.into_iter().collect(),
        });
        counts.conflicts_returned += 1;
    }

    if ctx.conflicts.is_empty() {
        ctx.diagnostics.push(ConflictDiagnostic {
            code: "no_recorded_conflicts",
            detail: format!(
                "scope '{}' is valid and scanned but has no recorded conflicts",
                scope.handle
            ),
        });
    }

    ctx.counts = counts;
    ctx
}

/// Which end of the joining edge(s) a party sits on, from the first edge (in
/// record-ID order) that touches it. Parallel edges share the unordered pair,
/// so orientation is stable.
fn edge_role_for(edges: &[&GraphRecord], party_id: &str) -> ConflictEdgeRole {
    for edge in edges {
        if let GraphRecord::Edge { source, .. } = edge
            && source == party_id
        {
            return ConflictEdgeRole::Source;
        }
    }
    ConflictEdgeRole::Target
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{AGENT_MEMORY_SCHEMA_VERSION, agent_memory_stable_id, stable_id};

    fn repo_index(records: &[GraphRecord]) -> RepositoryIndex {
        RepositoryIndex::build(records)
    }

    fn code_node(id: &str, kind: NodeKind, path: Option<&str>, name: Option<&str>) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            path.map(str::to_owned),
            None,
            name.map(str::to_owned),
            format!("node {id}"),
        )
    }

    fn memory_claim(id: &str, kind: NodeKind) -> GraphRecord {
        let mut node =
            GraphRecord::node(id.to_owned(), kind, None, None, None, format!("claim {id}"));
        if let GraphRecord::Node {
            schema_version,
            agent_id,
            observed_at,
            confidence,
            ..
        } = &mut node
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *agent_id = Some("agent_1".to_owned());
            *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
            *confidence = Some("0.5".to_owned());
        }
        node
    }

    fn contradicts(source: &str, target: &str) -> GraphRecord {
        GraphRecord::agent_memory_edge(
            EdgeLabel::Contradicts,
            source.to_owned(),
            target.to_owned(),
            Some("0.9".to_owned()),
            "disagreement".to_owned(),
        )
    }

    fn single_repo_store() -> Vec<GraphRecord> {
        let repo_id = stable_id(&["node", "Repository", "repo-a"]);
        let file_id = stable_id(&["node", "File", "src/a.rs"]);
        let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "foo"]);
        let obs_id = agent_memory_stable_id(&["observation", "x"]);
        let dec_id = agent_memory_stable_id(&["decision", "x"]);
        vec![
            GraphRecord::node(
                repo_id.clone(),
                NodeKind::Repository,
                None,
                None,
                Some("repo-a".to_owned()),
                "repo".to_owned(),
            ),
            code_node(&file_id, NodeKind::File, Some("src/a.rs"), None),
            code_node(&sym_id, NodeKind::Symbol, Some("src/a.rs"), Some("foo")),
            GraphRecord::edge(
                EdgeLabel::Contains,
                repo_id,
                file_id.clone(),
                None,
                "contains".to_owned(),
            ),
            // Real scans always emit the file-defines-symbol edge; without it
            // the symbol is unattributed and a prefix covering both reads as
            // cross-repository ambiguous (fail closed).
            GraphRecord::edge(
                EdgeLabel::Defines,
                file_id,
                sym_id,
                None,
                "defines".to_owned(),
            ),
            memory_claim(&obs_id, NodeKind::Observation),
            memory_claim(&dec_id, NodeKind::Decision),
            contradicts(&obs_id, &dec_id),
        ]
    }

    #[test]
    fn scope_resolves_symbol_name_file_path_and_prefix() {
        let records = single_repo_store();
        let index = repo_index(&records);
        let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "foo"]);
        let file_id = stable_id(&["node", "File", "src/a.rs"]);

        let scope = resolve_conflict_scope(&records, "foo", &index, None).expect("symbol");
        assert_eq!(scope.kind, ConflictScopeKind::Symbol);
        assert!(scope.anchor_ids.contains(&sym_id));
        assert_eq!(scope.target_handle, sym_id);

        let scope = resolve_conflict_scope(&records, "src/a.rs", &index, None).expect("file");
        assert_eq!(scope.kind, ConflictScopeKind::File);
        assert!(scope.anchor_ids.contains(&file_id));

        let scope = resolve_conflict_scope(&records, "src/", &index, None).expect("prefix");
        assert_eq!(scope.kind, ConflictScopeKind::Subsystem);
        assert!(scope.anchor_ids.contains(&sym_id));
        assert!(scope.anchor_ids.contains(&file_id));
        assert_eq!(scope.target_handle, "src");
    }

    #[test]
    fn empty_handle_is_unsupported() {
        let records = single_repo_store();
        let index = repo_index(&records);
        let err = resolve_conflict_scope(&records, "", &index, None).expect_err("empty");
        assert!(matches!(err, ConflictScopeError::Unsupported { .. }));
    }

    #[test]
    fn unknown_scope_is_empty_not_error() {
        let records = single_repo_store();
        let index = repo_index(&records);
        let scope = resolve_conflict_scope(&records, "src/void/", &index, None).expect("no-match");
        assert!(scope.is_empty());
        assert!(!scope.stale);
    }

    #[test]
    fn code_fact_party_is_ineligible() {
        let records = single_repo_store();
        let trust = TrustIndex::build(&records);
        let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "foo"]);
        let sym = records.iter().find(|r| r.id() == sym_id).expect("symbol");
        assert_eq!(conflict_party_class(sym, trust.classify(sym)), None);
    }

    #[test]
    fn agent_claim_verification_and_user_context_are_eligible() {
        let records = single_repo_store();
        let trust = TrustIndex::build(&records);
        let obs_id = agent_memory_stable_id(&["observation", "x"]);
        let obs = records.iter().find(|r| r.id() == obs_id).expect("obs");
        assert_eq!(
            conflict_party_class(obs, trust.classify(obs)),
            Some(ConflictPartyClass::AgentAuthored)
        );

        let run = GraphRecord::node(
            "verification:v1:abc".to_owned(),
            NodeKind::TestRun,
            None,
            None,
            None,
            "run".to_owned(),
        );
        assert_eq!(
            conflict_party_class(&run, trust.classify(&run)),
            Some(ConflictPartyClass::Verification)
        );

        let pref = GraphRecord::node(
            "user_context:v1:abc".to_owned(),
            NodeKind::Preference,
            None,
            None,
            None,
            "pref".to_owned(),
        );
        assert_eq!(
            conflict_party_class(&pref, trust.classify(&pref)),
            Some(ConflictPartyClass::UserContext)
        );

        let task = GraphRecord::node(
            "project:v1:abc".to_owned(),
            NodeKind::Task,
            None,
            None,
            None,
            "task".to_owned(),
        );
        assert_eq!(conflict_party_class(&task, trust.classify(&task)), None);
    }

    #[test]
    fn superseded_party_marks_conflict_resolved() {
        let mut records = single_repo_store();
        let sym_id = stable_id(&["node", "Symbol", "src/a.rs", "foo"]);
        // Cite the symbol so the conflict is in scope.
        let obs_id = agent_memory_stable_id(&["observation", "x"]);
        let dec_id = agent_memory_stable_id(&["decision", "x"]);
        let new_id = agent_memory_stable_id(&["observation", "y"]);
        for r in &mut records {
            if r.id() == obs_id
                && let GraphRecord::Node { evidence_links, .. } = r
            {
                *evidence_links = Some(vec![crate::EvidenceLink {
                    target_record_id: Some(sym_id.clone()),
                    target_domain: "codegraph".to_owned(),
                    relation: "OBSERVES".to_owned(),
                    confidence: "1.0".to_owned(),
                    as_of_commit: None,
                    target_repo_relative_path: None,
                    target_span: None,
                    target_git_commit: None,
                }]);
            }
        }
        records.push(memory_claim(&new_id, NodeKind::Observation));
        for r in &mut records {
            if r.id() == obs_id
                && let GraphRecord::Node { superseded_by, .. } = r
            {
                *superseded_by = Some(new_id.clone());
            }
        }
        let index = repo_index(&records);
        let scope = resolve_conflict_scope(&records, "foo", &index, None).expect("scope");

        let ctx = conflicts_context(&records, &scope, false);
        assert!(ctx.conflicts.is_empty());
        assert_eq!(ctx.counts.conflicts_resolved_excluded, 1);

        let ctx = conflicts_context(&records, &scope, true);
        assert_eq!(ctx.conflicts.len(), 1);
        assert!(ctx.conflicts[0].resolved);
        assert_eq!(ctx.conflicts[0].resolved_by, vec![new_id.as_str()]);

        let _ = dec_id;
    }
}
