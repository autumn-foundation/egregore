use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// Error returned when resolving a memory record ID or source/session handle.
///
/// Mirrors [`TaskResolveError`]: `Ambiguous` and `Unsupported` are the two
/// machine-readable, non-network failure modes. "Missing" (no match) and
/// "stale" (tombstoned) handles are surfaced by the CLI layer as `no_match` /
/// `stale_handle` envelopes, since both are about store state rather than the
/// handle's syntax.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MemoryResolveError {
    /// The handle resolves to more than one distinct memory claim.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The list of matched memory record IDs (canonical-sorted).
        candidates: Vec<String>,
    },
    /// The handle is empty or a malformed canonical agent-memory ID.
    Unsupported {
        /// The query handle.
        handle: String,
        /// Why the handle is unsupported.
        message: String,
    },
}

/// One stable, machine-readable diagnostic emitted by a memory audit.
///
/// Every diagnostic carries the original source handle so an operator can
/// follow it without the audit inferring a replacement (AC6).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MemoryAuditDiagnostic {
    /// Stable diagnostic code (e.g. `unresolved_evidence_link`).
    pub code: String,
    /// Record ID of the node that carries the issue.
    pub source_record_id: String,
    /// Original handle (record ID, path, or hash) — never an inferred value.
    pub target_handle: String,
    /// Relation that produced the handle, when applicable.
    pub relation: String,
    /// Target domain string, when applicable.
    pub target_domain: String,
}

/// One evidence record reached from the audited memory claim, with the
/// relation (edge label / evidence-link relation) that connected it.
#[derive(Debug, Clone)]
pub struct MemoryEvidenceItem<'a> {
    /// The reached graph record.
    pub record: &'a GraphRecord,
    /// The relation that connected it to the claim (e.g. `CONTRADICTS`).
    pub relation: String,
}

/// Structured memory evidence audit returned by [`memory_audit_context`].
///
/// Sections keep trust classes separate so an agent-authored claim is never
/// presented as source truth or proof by itself (AC3). Every section is
/// canonically ordered by record ID for determinism (AC8).
#[derive(Debug, Default, Clone)]
pub struct MemoryAuditContext<'a> {
    /// The queried memory record ID.
    pub memory_id: String,
    /// The agent-authored claim node(s) under audit.
    pub memory_claim: Vec<&'a GraphRecord>,
    /// `AgentSession` provenance node(s) linked via `AUTHORED_BY`.
    pub agent_sessions: Vec<&'a GraphRecord>,
    /// `Agent` provenance node(s) linked via `SESSION_OF`.
    pub agents: Vec<&'a GraphRecord>,
    /// Supporting evidence that is neither code, project, nor verification
    /// (artifacts, command evidence, other supporting memory).
    pub supporting_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Records connected to the claim via `CONTRADICTS` (either direction).
    pub contradicting_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Records that supersede the claim (`SUPERSEDES` / `superseded_by`).
    pub superseding_records: Vec<MemoryEvidenceItem<'a>>,
    /// Code-graph `File` / `Symbol` handles cited by the claim.
    pub related_code_handles: Vec<MemoryEvidenceItem<'a>>,
    /// Project-domain `Task` / `AcceptanceCriterion` handles cited by the claim.
    pub related_project_handles: Vec<MemoryEvidenceItem<'a>>,
    /// Verification-domain evidence cited by the claim.
    pub verification_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Stable diagnostics (unresolved links, triple-only targets, etc.).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// Records excluded by `--verified-only`, reported rather than dropped (AC5).
    pub excluded: Vec<MemoryEvidenceItem<'a>>,
}

impl MemoryAuditContext<'_> {
    /// Returns `true` when no claim matching the queried ID exists in the store.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.memory_claim.is_empty()
    }
}

pub(super) const fn is_verification_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Verification
            | NodeKind::CommandEvidence
            | NodeKind::TestRun
            | NodeKind::CommandRun
            | NodeKind::CIStatus
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
            | NodeKind::ProofResult
    )
}

pub(super) const fn is_codegraph_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::File
            | NodeKind::Symbol
            | NodeKind::Module
            | NodeKind::Import
            | NodeKind::Commit
            | NodeKind::Change
            | NodeKind::Repository
            // Manifest-declared dependency facts are deterministic code-graph
            // source facts (issue #180 / PR #314 review): memory-audit evidence
            // links classify them as code handles, and failure queries may
            // anchor on their canonical record IDs.
            | NodeKind::DependencyDeclaration
    )
}

pub(super) const fn is_project_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Task
            | NodeKind::AcceptanceCriterion
            | NodeKind::LocalTask
            | NodeKind::GitHubIssue
            | NodeKind::PR
            | NodeKind::Review
            | NodeKind::ExternalLink
            | NodeKind::Product
            | NodeKind::Project
            | NodeKind::Plan
    )
}

/// An agent-authored claim shape eligible for the memory audit subject and for
/// the unverified-observation exclusion filter.
const fn is_agent_claim_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Observation | NodeKind::Decision | NodeKind::Failure
    )
}

pub(super) const fn record_node_kind(record: &GraphRecord) -> Option<NodeKind> {
    match record {
        GraphRecord::Node { kind, .. } => Some(*kind),
        _ => None,
    }
}

/// Final `::`-delimited segment of a symbol name or import path, used for the
/// name-based import resolution in change-impact.
pub(super) fn last_path_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Final segment of one import item, stripping a trailing `as` alias.
/// Returns `None` for globs (`*`), `self`, or empty items.
fn import_item_name(item: &str) -> Option<&str> {
    let base = item.trim();
    let base = base.split(" as ").next().unwrap_or(base).trim();
    let seg = last_path_segment(base).trim();
    if seg.is_empty() || seg == "*" || seg == "self" {
        None
    } else {
        Some(seg)
    }
}

/// Climb inbound `Defines`/`Contains` edges from an owner node until a File or
/// Module is reached, returning that owner and the edge connecting to it. Used
/// so a method owned by an impl-block `Symbol` resolves to its containing file
/// for `containing_context`. Returns `None` if no File/Module owner is found.
pub(super) fn containing_file_or_module<'a>(
    start: &'a GraphRecord,
    start_edge: &'a GraphRecord,
    by_id: &BTreeMap<&'a str, &'a GraphRecord>,
    inbound_edges: &BTreeMap<&'a str, Vec<(&'a str, &'a EdgeLabel, &'a str)>>,
) -> Option<(&'a GraphRecord, &'a GraphRecord)> {
    let mut node = start;
    let mut edge = start_edge;
    // Bound the climb so a malformed cyclic ownership chain cannot loop forever.
    for _ in 0..16 {
        match record_node_kind(node) {
            Some(NodeKind::File | NodeKind::Module) => return Some((node, edge)),
            Some(NodeKind::Symbol) => {
                let (parent, parent_edge) = inbound_edges
                    .get(node.id())
                    .into_iter()
                    .flatten()
                    .find(|&&(_, l, _)| matches!(l, EdgeLabel::Defines | EdgeLabel::Contains))
                    .and_then(|&(eid, _, pid)| Some((*by_id.get(pid)?, *by_id.get(eid)?)))?;
                node = parent;
                edge = parent_edge;
            }
            _ => return None,
        }
    }
    None
}

/// Imported symbol names from a `use` path, expanding a brace group and
/// stripping aliases. `a::b::{X, Y as Z}` → `[X, Y]`; `a::b::C` → `[C]`.
pub(super) fn imported_symbol_names(import_path: &str) -> Vec<&str> {
    let trimmed = import_path.trim();
    trimmed.find('{').map_or_else(
        || import_item_name(trimmed).into_iter().collect(),
        |open| {
            let inner = &trimmed[open + 1..];
            let inner = inner.strip_suffix('}').unwrap_or(inner);
            inner.split(',').filter_map(import_item_name).collect()
        },
    )
}

/// A claim is **verified** when it cites at least one present verification-domain
/// record through an evidence link (`VALIDATED_BY`, `HAS_EVIDENCE`,
/// `PRODUCED_EVIDENCE`) or an equivalent outgoing edge. This is a structural,
/// non-inferential rule over existing contracts — not a truth judgement.
///
/// Shared by the memory-audit `--verified-only` filter and the semantic-memory
/// recall `--verified-only` filter (issue #91) so both surfaces apply the
/// identical rule: a resolvable, non-tombstoned verification record is required;
/// a triple-only citation stub that names no record never counts as verified.
// Kept `pub(crate)`: the `pub use` glob in mod.rs re-exports this at its
// original crate-internal visibility; `pub` would widen it to the public API.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn is_verified_claim(
    record: &GraphRecord,
    by_id: &BTreeMap<&str, &GraphRecord>,
    edges_from: &BTreeMap<&str, Vec<(&EdgeLabel, &str)>>,
    tombstoned: &BTreeSet<&str>,
) -> bool {
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = record
    {
        for link in links {
            // A backing relation is required; a generic link (e.g. RELATES_TO)
            // that merely happens to point at a verification record does not make
            // the claim verified. Tombstoned targets are treated as absent.
            let backed = matches!(
                link.relation.as_str(),
                "VALIDATED_BY" | "HAS_EVIDENCE" | "PRODUCED_EVIDENCE"
            );
            if backed
                && let Some(target_id) = link.target_record_id.as_deref()
                && !tombstoned.contains(target_id)
                && let Some(target) = by_id.get(target_id)
                && record_node_kind(target).is_some_and(is_verification_kind)
            {
                return true;
            }
        }
    }
    if let Some(out) = edges_from.get(record.id()) {
        for (label, target) in out {
            let backed = matches!(
                label,
                EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence | EdgeLabel::ProducedEvidence
            );
            if backed
                && !tombstoned.contains(*target)
                && let Some(target) = by_id.get(*target)
                && record_node_kind(target).is_some_and(is_verification_kind)
            {
                return true;
            }
        }
    }
    false
}

/// Outgoing edges keyed by source record ID, used for edge-backed verification.
// Kept `pub(crate)`: the `pub use` glob in mod.rs re-exports this at its
// original crate-internal visibility; `pub` would widen it to the public API.
#[allow(clippy::redundant_pub_crate)]
pub(crate) type OutgoingEdgeIndex<'a> = BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>;

/// Set of tombstoned record IDs, treated as absent during verification checks.
// Kept `pub(crate)`: the `pub use` glob in mod.rs re-exports this at its
// original crate-internal visibility; `pub` would widen it to the public API.
#[allow(clippy::redundant_pub_crate)]
pub(crate) type TombstonedSet<'a> = BTreeSet<&'a str>;

/// Builds the support indexes [`is_verified_claim`] needs: outgoing edges keyed
/// by source record ID (for edge-backed verification) and the set of tombstoned
/// record IDs (treated as absent). Shared so the semantic-memory recall surface
/// (issue #91) applies the exact rule the memory audit does.
#[must_use]
// Kept `pub(crate)`: the `pub use` glob in mod.rs re-exports this at its
// original crate-internal visibility; `pub` would widen it to the public API.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn verification_support_indexes(
    records: &[GraphRecord],
) -> (OutgoingEdgeIndex<'_>, TombstonedSet<'_>) {
    // Latest-write-wins liveness (issues #421/#432): a record (node or edge)
    // re-ingested AFTER its own tombstone is live again over an append-only
    // `--graph`, matching the embedded `--data-dir` read. The `tombstoned` set
    // retains a deleted_id only while its tombstone is still the id's most recent
    // write, and edges are keyed to their latest EDGE version so a stale earlier
    // version never supplies verification-link metadata. See `super::liveness`.
    let liveness = Liveness::new(records);
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .filter(|&id| liveness.deleted(id))
        .collect();

    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    for (index, r) in records.iter().enumerate() {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = r
        {
            if liveness.is_latest_edge_version(id.as_str(), index) && !liveness.deleted(id.as_str())
            {
                edges_from
                    .entry(source.as_str())
                    .or_default()
                    .push((label, target.as_str()));
            }
        }
    }
    (edges_from, tombstoned)
}

/// Pushes an `unresolved_evidence_link` (absent) or `stale_evidence_target`
/// (tombstoned) diagnostic for an edge whose target is not live.
fn push_missing_target(
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
    memory_id: &str,
    target: &str,
    tombstoned: &BTreeSet<&str>,
    relation: &str,
) {
    let code = if tombstoned.contains(target) {
        "stale_evidence_target"
    } else {
        "unresolved_evidence_link"
    };
    diagnostics.push(MemoryAuditDiagnostic {
        code: code.to_owned(),
        source_record_id: memory_id.to_owned(),
        target_handle: target.to_owned(),
        relation: relation.to_owned(),
        target_domain: String::new(),
    });
}

/// The outcome of resolving a memory handle to live claim IDs.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct MemoryResolution {
    /// Live (non-tombstoned) claim IDs the handle resolved to.
    pub matched: BTreeSet<String>,
    /// True when the handle matched at least one claim but every match was
    /// tombstoned (deleted), so the caller should report `stale_handle` rather
    /// than `no_match`.
    pub tombstoned_only: bool,
}

/// Resolves a memory record ID or source/session handle to live claim IDs.
///
/// Supported handle types (AC2):
/// 1. A canonical memory record ID (`agent_memory:v1:<64-hex>`). When it names
///    an `AgentSession` or `Agent`, it is a scope handle resolving to the claims
///    authored in that scope; a scope with more than one claim returns
///    `Ambiguous` with the candidate IDs (one audit covers one claim).
/// 2. A source artifact / session handle: a string matching a claim node's
///    `source_handle`, `source_artifact_path`, `source_artifact_hash`, or
///    `session_id`.
///
/// Tombstoned (deleted) claims are excluded from `matched` so they neither make
/// a live handle ambiguous nor get audited as current state. When every match
/// was tombstoned, `tombstoned_only` is set so the caller can report
/// `stale_handle` even for a source/session handle (whose text is not the
/// deleted record ID).
///
/// # Errors
///
/// Returns [`MemoryResolveError::Unsupported`] for an empty or malformed
/// canonical ID, and [`MemoryResolveError::Ambiguous`] when the handle resolves
/// to more than one distinct live claim.
#[allow(clippy::too_many_lines)]
pub fn resolve_memory_ids(
    records: &[GraphRecord],
    handle: &str,
) -> Result<MemoryResolution, MemoryResolveError> {
    if handle.is_empty() {
        return Err(MemoryResolveError::Unsupported {
            handle: handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }

    let mut matched: BTreeSet<String> = BTreeSet::new();

    if let Some(rest) = handle.strip_prefix("agent_memory:v1:") {
        let is_valid = rest.len() == 64 && rest.chars().all(|c| c.is_ascii_hexdigit());
        if !is_valid {
            return Err(MemoryResolveError::Unsupported {
                handle: handle.to_owned(),
                message: "malformed canonical agent-memory ID".to_owned(),
            });
        }
        // Find the node carrying this ID.
        let mut subject_kind = None;
        let mut subject_name = None;
        let mut subject_session_id = None;
        let mut subject_agent_id = None;
        for r in records {
            if let GraphRecord::Node {
                id,
                kind,
                name,
                session_id,
                agent_id,
                ..
            } = r
                && id == handle
            {
                subject_kind = Some(*kind);
                subject_name.clone_from(name);
                subject_session_id.clone_from(session_id);
                subject_agent_id.clone_from(agent_id);
            }
        }
        match subject_kind {
            Some(kind) if is_agent_claim_kind(kind) => {
                matched.insert(handle.to_owned());
            }
            Some(NodeKind::AgentSession) => {
                // Prefer the session node's `session_id` field; imported session
                // nodes (e.g. Codex) keep a human summary in `name` while claims
                // store the real key in `session_id`. Fall back to `name`.
                let session_key = subject_session_id.as_deref().or(subject_name.as_deref());
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        session_id: Some(sid),
                        ..
                    } = r
                        && is_agent_claim_kind(*kind)
                        && Some(sid.as_str()) == session_key
                    {
                        matched.insert(id.clone());
                    }
                }
            }
            Some(NodeKind::Agent) => {
                // Prefer the agent node's `agent_id` field; fall back to `name`.
                let agent_key = subject_agent_id.as_deref().or(subject_name.as_deref());
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        agent_id: Some(aid),
                        ..
                    } = r
                        && is_agent_claim_kind(*kind)
                        && Some(aid.as_str()) == agent_key
                    {
                        matched.insert(id.clone());
                    }
                }
            }
            // Present but not an auditable claim, or absent entirely: leave the
            // set empty so the caller emits a `no_match` envelope.
            _ => {}
        }
    } else {
        // Source artifact / session handle.
        for r in records {
            if let GraphRecord::Node {
                id,
                kind,
                session_id,
                source_handle,
                source_artifact_path,
                source_artifact_hash,
                ..
            } = r
                && is_agent_claim_kind(*kind)
                && (source_handle.as_deref() == Some(handle)
                    || source_artifact_path.as_deref() == Some(handle)
                    || source_artifact_hash.as_deref() == Some(handle)
                    || session_id.as_deref() == Some(handle))
            {
                matched.insert(id.clone());
            }
        }
    }

    // Tombstoned (deleted) claims are not part of the current state, so they must
    // not make a live handle ambiguous. Drop them before counting, but remember
    // whether the handle matched anything at all so a handle that pointed only at
    // deleted claims is reported as `stale_handle`, not `no_match`.
    //
    // Latest-write-wins liveness (issues #421/#432): a claim/scope re-ingested
    // AFTER its own tombstone is live again over an append-only `--graph`, so the
    // set retains a deleted_id only while its tombstone is still the id's most
    // recent write — matching the embedded `--data-dir` read. See
    // `super::liveness`.
    let liveness = Liveness::new(records);
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .filter(|&id| liveness.deleted(id))
        .collect();

    // If the queried canonical ID is itself tombstoned — a deleted claim, or a
    // deleted Agent/AgentSession scope node — the handle names a deleted record
    // and is stale, regardless of whether live claims share its scope key.
    if tombstoned.contains(handle) {
        return Ok(MemoryResolution {
            matched: BTreeSet::new(),
            tombstoned_only: true,
        });
    }

    let had_any_match = !matched.is_empty();
    matched.retain(|id| !tombstoned.contains(id.as_str()));
    let tombstoned_only = had_any_match && matched.is_empty();

    // A single audit covers one claim. A scope handle (Agent / AgentSession ID,
    // or a session_id shared by several claims) that resolves to more than one
    // claim is reported as a stable `Ambiguous` diagnostic listing the candidate
    // claim IDs, so the operator can re-query a specific one. This keeps the
    // single-claim audit contract honest rather than silently picking one.
    if matched.len() > 1 {
        return Err(MemoryResolveError::Ambiguous {
            handle: handle.to_owned(),
            candidates: matched.into_iter().collect(),
        });
    }

    Ok(MemoryResolution {
        matched,
        tombstoned_only,
    })
}

/// Section a reached evidence record belongs to.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AuditSection {
    Supporting,
    Code,
    Project,
    Verification,
}

const fn classify_audit_target(kind: NodeKind) -> AuditSection {
    if is_verification_kind(kind) {
        AuditSection::Verification
    } else if is_codegraph_kind(kind) {
        AuditSection::Code
    } else if is_project_kind(kind) {
        AuditSection::Project
    } else {
        AuditSection::Supporting
    }
}

/// Builds the evidence audit for a resolved memory claim ID.
///
/// The traversal reads only existing contracts (evidence links + cross-domain
/// edges) and never reads raw transcript bodies or infers replacements for
/// missing handles (AC6, AC11). When `verified_only` is set, unverified
/// agent-authored records are moved out of their sections into `excluded`
/// rather than silently dropped (AC5).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn memory_audit_context<'a>(
    records: &'a [GraphRecord],
    memory_id: &str,
    verified_only: bool,
) -> MemoryAuditContext<'a> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // Latest-write-wins liveness (issues #421/#432): over an append-only
    // `--graph` a record (node or edge) re-ingested AFTER its own tombstone is
    // live again, matching the embedded `--data-dir` current-state read. The
    // shared gate drives both the edge index (only the latest, non-deleted EDGE
    // version supplies evidence/provenance adjacency) and the `tombstoned` set
    // below (a deleted_id is retained only while its tombstone is still the id's
    // most recent write, with the history/temporal exemption). See
    // `super::liveness`.
    let liveness = Liveness::new(records);

    // Outgoing edges keyed by source ID, for verification detection.
    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    for (index, r) in records.iter().enumerate() {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = r
        {
            if liveness.is_latest_edge_version(id.as_str(), index) && !liveness.deleted(id.as_str())
            {
                edges_from
                    .entry(source.as_str())
                    .or_default()
                    .push((label, target.as_str()));
            }
        }
    }

    // Tombstoned IDs are deleted for current-state reads: treat their nodes as
    // absent everywhere in the traversal so a deleted evidence/provenance record
    // is never surfaced as live support (AC6).
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .filter(|&id| liveness.deleted(id))
        .collect();
    let present = |id: &str| -> Option<&'a GraphRecord> {
        if tombstoned.contains(id) {
            None
        } else {
            by_id.get(id).copied()
        }
    };

    let claim_nodes: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| matches!(r, GraphRecord::Node { id, .. } if id == memory_id))
        .collect();

    let mut ctx = MemoryAuditContext {
        memory_id: memory_id.to_owned(),
        memory_claim: claim_nodes.clone(),
        ..Default::default()
    };

    if claim_nodes.is_empty() {
        return ctx;
    }

    // Dedupe maps keyed by record ID; relation is the first one observed.
    let mut supporting: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut contradicting: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut superseding: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut code: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut project: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut verification: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut sessions: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut agents: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    let mut place = |target: &'a GraphRecord, relation: &str| {
        let Some(kind) = record_node_kind(target) else {
            return;
        };
        let item = MemoryEvidenceItem {
            record: target,
            relation: relation.to_owned(),
        };
        match classify_audit_target(kind) {
            AuditSection::Verification => {
                verification.entry(target.id()).or_insert(item);
            }
            AuditSection::Code => {
                code.entry(target.id()).or_insert(item);
            }
            AuditSection::Project => {
                project.entry(target.id()).or_insert(item);
            }
            AuditSection::Supporting => {
                supporting.entry(target.id()).or_insert(item);
            }
        }
    };

    for claim in &claim_nodes {
        // 1) Denormalized evidence links on the claim node.
        if let GraphRecord::Node {
            evidence_links: Some(links),
            ..
        } = claim
        {
            for link in links {
                let Some(target_id) = link.target_record_id.as_deref() else {
                    // Triple-only target: do not resolve heuristically (AC6).
                    let handle = link
                        .target_repo_relative_path
                        .clone()
                        .unwrap_or_else(|| "<triple>".to_owned());
                    diagnostics.push(MemoryAuditDiagnostic {
                        code: "evidence_target_unresolved".to_owned(),
                        source_record_id: memory_id.to_owned(),
                        target_handle: handle,
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                    continue;
                };
                // A tombstoned target is stale for current-state reads; an absent
                // target is unresolved. Either way it is surfaced, not placed.
                let code = if tombstoned.contains(target_id) {
                    "stale_evidence_target"
                } else if by_id.contains_key(target_id) {
                    ""
                } else {
                    "unresolved_evidence_link"
                };
                if !code.is_empty() {
                    diagnostics.push(MemoryAuditDiagnostic {
                        code: code.to_owned(),
                        source_record_id: memory_id.to_owned(),
                        target_handle: target_id.to_owned(),
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                    continue;
                }
                let Some(target) = present(target_id) else {
                    continue;
                };
                // A denormalized `CONTRADICTS` evidence link must reach the
                // contradicting section, same as a `CONTRADICTS` graph edge,
                // so a record that retained only the denormalized link does not
                // mis-report a contradiction as generic support.
                if link.relation == "CONTRADICTS" {
                    contradicting
                        .entry(target.id())
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: target,
                            relation: "CONTRADICTS".to_owned(),
                        });
                } else {
                    place(target, &link.relation);
                }
            }
        }

        // 2) Supersession declared inline on the claim — read even when the
        // claim carries no `evidence_links` (a common stale-claim shape).
        if let GraphRecord::Node {
            superseded_by: Some(sup_id),
            ..
        } = claim
        {
            if let Some(target) = present(sup_id) {
                superseding
                    .entry(target.id())
                    .or_insert_with(|| MemoryEvidenceItem {
                        record: target,
                        relation: "SUPERSEDED_BY".to_owned(),
                    });
            } else {
                let code = if tombstoned.contains(sup_id.as_str()) {
                    "stale_evidence_target"
                } else {
                    "unresolved_evidence_link"
                };
                diagnostics.push(MemoryAuditDiagnostic {
                    code: code.to_owned(),
                    source_record_id: memory_id.to_owned(),
                    target_handle: sup_id.clone(),
                    relation: "SUPERSEDED_BY".to_owned(),
                    target_domain: "agent_memory".to_owned(),
                });
            }
        }

        // 3) Edges touching the claim node.
        for (index, r) in records.iter().enumerate() {
            let GraphRecord::Edge {
                id: edge_id,
                label,
                source,
                target,
                ..
            } = r
            else {
                continue;
            };
            // Latest-write-wins for edge metadata (issues #421/#432): only the
            // latest, non-deleted EDGE version of a stable id supplies claim
            // adjacency, so a superseded or tombstoned-then-revived edge agrees
            // with the embedded `--data-dir` read. See `super::liveness`.
            if !liveness.is_latest_edge_version(edge_id.as_str(), index)
                || liveness.deleted(edge_id.as_str())
            {
                continue;
            }
            let claim_id = claim.id();
            if source == claim_id {
                // AUTHORED_BY provenance is resolved by the chain walk below;
                // code-internal and other labels are not claim evidence.
                let is_evidence = matches!(
                    label,
                    EdgeLabel::Contradicts
                        | EdgeLabel::HasEvidence
                        | EdgeLabel::ValidatedBy
                        | EdgeLabel::Observes
                        | EdgeLabel::MentionsSymbol
                        | EdgeLabel::TouchedFile
                        | EdgeLabel::FailedOn
                        | EdgeLabel::ReferencesTask
                        | EdgeLabel::ExplainsChange
                        | EdgeLabel::ProducedPatch
                        | EdgeLabel::ProducedEvidence
                        | EdgeLabel::RelatesTo
                );
                if !is_evidence {
                    continue;
                }
                let Some(other) = present(target) else {
                    // Edge-only evidence whose target is absent or tombstoned is
                    // surfaced as a diagnostic, like a denormalized link, instead
                    // of silently disappearing.
                    push_missing_target(
                        &mut diagnostics,
                        memory_id,
                        target,
                        &tombstoned,
                        label.as_str(),
                    );
                    continue;
                };
                if matches!(label, EdgeLabel::Contradicts) {
                    contradicting
                        .entry(other.id())
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: other,
                            relation: "CONTRADICTS".to_owned(),
                        });
                } else {
                    place(other, label.as_str());
                }
            } else if target == claim_id {
                if !matches!(label, EdgeLabel::Contradicts | EdgeLabel::Supersedes) {
                    continue;
                }
                let Some(other) = present(source) else {
                    push_missing_target(
                        &mut diagnostics,
                        memory_id,
                        source,
                        &tombstoned,
                        label.as_str(),
                    );
                    continue;
                };
                match label {
                    EdgeLabel::Contradicts => {
                        contradicting
                            .entry(other.id())
                            .or_insert_with(|| MemoryEvidenceItem {
                                record: other,
                                relation: "CONTRADICTS".to_owned(),
                            });
                    }
                    EdgeLabel::Supersedes => {
                        superseding
                            .entry(other.id())
                            .or_insert_with(|| MemoryEvidenceItem {
                                record: other,
                                relation: "SUPERSEDES".to_owned(),
                            });
                    }
                    _ => {}
                }
            }
        }
    }

    // Reverse denormalized links: a newer record may declare the relationship on
    // itself (`SUPERSEDES`/`CONTRADICTS` -> audited claim) without a retained
    // edge. Scan live records' evidence_links for entries targeting the claim so
    // the superseding/contradicting record is not omitted in edge-stripped stores.
    for r in records {
        let GraphRecord::Node {
            id: src_id,
            evidence_links: Some(links),
            ..
        } = r
        else {
            continue;
        };
        if src_id == memory_id || tombstoned.contains(src_id.as_str()) {
            continue;
        }
        for link in links {
            if link.target_record_id.as_deref() != Some(memory_id) {
                continue;
            }
            match link.relation.as_str() {
                "SUPERSEDES" => {
                    superseding
                        .entry(src_id)
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: r,
                            relation: "SUPERSEDES".to_owned(),
                        });
                }
                "CONTRADICTS" => {
                    contradicting
                        .entry(src_id)
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: r,
                            relation: "CONTRADICTS".to_owned(),
                        });
                }
                _ => {}
            }
        }
    }

    // Resolve session/agent provenance by walking the AUTHORED_BY / SESSION_OF
    // chain from each claim. Imported records commonly route a claim through an
    // AgentTurn and AgentRun before reaching the AgentSession, with SESSION_OF on
    // the session→agent edge, so the immediate AUTHORED_BY target is not always
    // the session. The walk classifies every reached node by kind; intermediate
    // turns/runs are traversed but never labelled as a session.
    {
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut frontier: Vec<&str> = claim_nodes.iter().map(|c| c.id()).collect();
        while let Some(id) = frontier.pop() {
            if !visited.insert(id) {
                continue;
            }
            let Some(out) = edges_from.get(id) else {
                continue;
            };
            for (label, target) in out {
                if !matches!(label, EdgeLabel::AuthoredBy | EdgeLabel::SessionOf) {
                    continue;
                }
                if !visited.contains(*target) {
                    frontier.push(*target);
                }
                if let Some(node) = present(target) {
                    match record_node_kind(node) {
                        Some(NodeKind::AgentSession) => {
                            sessions.entry(node.id()).or_insert(node);
                        }
                        Some(NodeKind::Agent) => {
                            agents.entry(node.id()).or_insert(node);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // verified-only filter: move unverified agent-authored records to `excluded`.
    let mut excluded: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    if verified_only {
        for map in [&mut contradicting, &mut superseding, &mut supporting] {
            let drop_ids: Vec<&str> = map
                .iter()
                .filter(|(_, item)| {
                    record_node_kind(item.record).is_some_and(is_agent_claim_kind)
                        && !is_verified_claim(item.record, &by_id, &edges_from, &tombstoned)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in drop_ids {
                if let Some(item) = map.remove(id) {
                    excluded.entry(id).or_insert(item);
                }
            }
        }
    }

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup();

    ctx.agent_sessions = sessions.into_values().collect();
    ctx.agents = agents.into_values().collect();
    ctx.supporting_evidence = supporting.into_values().collect();
    ctx.contradicting_evidence = contradicting.into_values().collect();
    ctx.superseding_records = superseding.into_values().collect();
    ctx.related_code_handles = code.into_values().collect();
    ctx.related_project_handles = project.into_values().collect();
    ctx.verification_evidence = verification.into_values().collect();
    ctx.excluded = excluded.into_values().collect();
    ctx.diagnostics = diagnostics;
    ctx
}

#[cfg(test)]
mod liveness_parity_tests {
    //! Transport-parity regression (issues #421/#432): over an append-only
    //! `--graph`, a claim, an evidence target, or a verification edge re-ingested
    //! AFTER its own tombstone is live again — matching the embedded `--data-dir`
    //! current-state read — while a tombstone with no re-add still deletes its id.
    use super::*;
    use crate::ir::SCHEMA_VERSION;

    fn obs(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            Some("obs".to_owned()),
            "observation".to_owned(),
        )
    }

    fn file(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            "file".to_owned(),
        )
    }

    fn verif(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Verification,
            None,
            None,
            Some("v".to_owned()),
            "verification".to_owned(),
        )
    }

    fn edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            None,
            "edge".to_owned(),
        )
    }

    fn tomb(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v6:tomb_{deleted_id}"),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    fn canonical(seed: char) -> String {
        format!("agent_memory:v1:{}", seed.to_string().repeat(64))
    }

    #[test]
    fn claim_reingested_after_tombstone_resolves_live() {
        let id = canonical('a');
        let records = vec![obs(&id), tomb(&id), obs(&id)];
        let res = resolve_memory_ids(&records, &id).expect("resolve");
        assert!(
            res.matched.contains(id.as_str()),
            "a claim re-ingested after its tombstone must resolve live"
        );
        assert!(!res.tombstoned_only, "a revived claim is not stale-only");
    }

    #[test]
    fn claim_tombstone_without_reingest_is_stale() {
        let id = canonical('a');
        let records = vec![obs(&id), tomb(&id)];
        let res = resolve_memory_ids(&records, &id).expect("resolve");
        assert!(
            res.matched.is_empty(),
            "a deleted claim resolves to nothing"
        );
        assert!(
            res.tombstoned_only,
            "a tombstone with no re-add is reported stale"
        );
    }

    #[test]
    fn revived_evidence_target_is_placed_not_stale() {
        let claim = "agent_memory:v1:claim1";
        let f = "codegraph:v6:file1";
        let records = vec![
            obs(claim),
            file(f),
            edge(EdgeLabel::Observes, claim, f),
            tomb(f),
            file(f),
        ];
        let ctx = memory_audit_context(&records, claim, false);
        assert!(
            ctx.related_code_handles.iter().any(|i| i.record.id() == f),
            "a revived evidence target must be placed as live evidence"
        );
        assert!(
            !ctx.diagnostics
                .iter()
                .any(|d| d.code == "stale_evidence_target"),
            "a revived evidence target must not be reported stale"
        );
    }

    #[test]
    fn evidence_target_tombstone_without_reingest_stays_stale() {
        let claim = "agent_memory:v1:claim1";
        let f = "codegraph:v6:file1";
        let records = vec![
            obs(claim),
            file(f),
            edge(EdgeLabel::Observes, claim, f),
            tomb(f),
        ];
        let ctx = memory_audit_context(&records, claim, false);
        assert!(
            ctx.related_code_handles.is_empty(),
            "a deleted evidence target must not be placed"
        );
        assert!(
            ctx.diagnostics
                .iter()
                .any(|d| d.code == "stale_evidence_target"),
            "a deleted evidence target is reported stale"
        );
    }

    #[test]
    fn verification_edge_reingested_after_tombstone_confers_verification() {
        let claim = "agent_memory:v1:claim1";
        let v = "agent_memory:v1:verif1";
        let e1 = edge(EdgeLabel::ValidatedBy, claim, v);
        let edge_id = e1.id().to_owned();
        let e2 = edge(EdgeLabel::ValidatedBy, claim, v);
        let records = vec![obs(claim), verif(v), e1, tomb(&edge_id), e2];
        let (edges_from, tombstoned) = verification_support_indexes(&records);
        let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
        let claim_node = by_id.get(claim).copied().expect("claim present");
        assert!(
            is_verified_claim(claim_node, &by_id, &edges_from, &tombstoned),
            "a verification edge re-ingested after its tombstone must confer verification"
        );
    }
}
