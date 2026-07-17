use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{
    FailureTargetKind, MemoryAuditDiagnostic, RepositoryIndex, ResolvedFailureTarget,
    containing_file_or_module, imported_symbol_names, last_path_segment, record_node_kind,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// Direction of traversal for one impact lead.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ImpactDirection {
    /// The reached node points *into* the anchor (e.g. a caller of the anchor).
    Inbound,
    /// The anchor points *out* to the reached node (e.g. a callee).
    Outbound,
}

impl ImpactDirection {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }
}

/// One graph-derived impact lead.
///
/// A node reachable from the anchor via a code-topology edge, tagged with the
/// relation, direction, and hop count. Every row is a LEAD to inspect before
/// editing — not proof of breakage.
#[derive(Debug, Clone)]
pub struct ImpactLead<'a> {
    /// The reached code-graph node (Symbol, File, or Module).
    pub record: &'a GraphRecord,
    /// The connecting edge record (for its stable ID and temporal metadata).
    pub edge: &'a GraphRecord,
    /// The EdgeLabel wire string (e.g. "CALLS", "REFERENCES").
    pub relation: &'static str,
    /// Whether the edge is inbound or outbound relative to the anchor.
    pub direction: ImpactDirection,
    /// The anchor record ID that was used as the traversal seed.
    pub anchor_id: &'a str,
    /// Hop distance from the seed anchor (1-based).
    pub hop: usize,
}

/// Truncation metadata emitted when a per-group cap is hit (AC6).
#[derive(Debug, Clone)]
pub struct ImpactTruncation {
    /// Group label (e.g. "direct_callers").
    pub group: &'static str,
    /// Number of leads returned (after cap).
    pub returned: usize,
    /// Total candidates seen before capping.
    pub total: usize,
    /// The depth parameter in effect when the cap fired.
    pub depth: usize,
}

/// Structured change-impact context returned by [`change_impact_context`].
///
/// Every lead vector is canonically ordered by (record_id, edge_id) for
/// determinism (AC7). Absent sections are empty vecs, never omitted, so a
/// consumer can distinguish "checked, none found" from "class was dropped".
#[derive(Debug, Default)]
pub struct ChangeImpactContext<'a> {
    /// Resolved handle type ("symbol" / "file").
    pub target_kind: &'static str,
    /// Resolved anchor record IDs, canonically sorted.
    pub target_ids: Vec<String>,
    /// Symbols that directly call the anchor (inbound `CALLS` edges).
    pub direct_callers: Vec<ImpactLead<'a>>,
    /// Symbols the anchor calls directly (outbound `CALLS` edges).
    pub direct_callees: Vec<ImpactLead<'a>>,
    /// Symbols or files that reference or import the anchor (inbound
    /// `REFERENCES` edges; outbound handled for completeness).
    pub referencing_files: Vec<ImpactLead<'a>>,
    /// Symbols related through `IMPLEMENTS` edges (both directions).
    pub implementation_symbols: Vec<ImpactLead<'a>>,
    /// The containing file/module (inbound `DEFINES`/`CONTAINS` edges).
    pub containing_context: Vec<ImpactLead<'a>>,
    /// Stable machine-readable diagnostics (unresolved edges, unsupported
    /// relations, truncation notices).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// Depth parameter used.
    pub depth: usize,
    /// Per-group truncation records when the lead cap was hit.
    pub truncations: Vec<ImpactTruncation>,
}

/// Maximum impact leads per group before the truncation diagnostic fires.
const MAX_LEADS_PER_GROUP: usize = 200;

/// Code-topology edge labels that the change-impact traversal classifies.
const IMPACT_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Calls,
    EdgeLabel::References,
    EdgeLabel::Imports,
    EdgeLabel::Implements,
    EdgeLabel::Defines,
    EdgeLabel::Contains,
];

/// First resolved change-impact anchor whose node kind is **not** a code
/// `Symbol` or `File`, if any.
///
/// `change-impact` accepts only symbol and file handles. A canonical codegraph
/// ID that resolves to a `Repository`, `Module`, `Import`, `Commit`, `Change`,
/// or any other node kind maps to [`FailureTargetKind::Symbol`] during handle
/// resolution, so it must be rejected here rather than traversed as an empty
/// `symbol` result. Returns `None` when every anchor is a `Symbol` or `File`.
#[must_use]
pub fn change_impact_unsupported_anchor_kind(
    records: &[GraphRecord],
    target: &ResolvedFailureTarget,
) -> Option<NodeKind> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    target
        .anchor_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .filter_map(record_node_kind)
        .find(|kind| !matches!(kind, NodeKind::Symbol | NodeKind::File))
}

/// Compute graph-derived change-impact leads for a resolved code handle.
///
/// `target` is produced by [`resolve_failure_handle`] (which implements the
/// full handle resolution contract for AC2). The traversal is bounded by
/// `depth` hops from the anchor set. Every result group is canonically sorted
/// for determinism (AC7); missing edge targets produce diagnostics rather than
/// silently dropping relationship classes (AC6/AC8). When `repo_scope` is set
/// (the caller passed `--repo`), `repo_index` constrains the name-based import
/// resolution to the queried anchors' repositories; an unscoped query does not
/// repo-filter imports.
#[must_use]
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn change_impact_context<'a>(
    records: &'a [GraphRecord],
    target: &ResolvedFailureTarget,
    depth: usize,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> ChangeImpactContext<'a> {
    fn drain_sorted<'a>(
        map: BTreeMap<(&'a str, &'a str), ImpactLead<'a>>,
        group: &'static str,
        cap: usize,
        depth: usize,
        truncations: &mut Vec<ImpactTruncation>,
    ) -> Vec<ImpactLead<'a>> {
        let total = map.len();
        let mut leads: Vec<ImpactLead<'a>> = map.into_values().collect();
        // Order by hop distance first so that, when a group exceeds the cap, the
        // nearest (most immediate) impact leads are preserved and a large
        // further-out neighborhood cannot evict first-hop callers/callees.
        // `(record_id, edge_id)` breaks ties for byte-stable output.
        leads.sort_by(|a, b| {
            a.hop
                .cmp(&b.hop)
                .then_with(|| a.record.id().cmp(b.record.id()))
                .then_with(|| a.edge.id().cmp(b.edge.id()))
        });
        let returned = leads.len().min(cap);
        if total > cap {
            truncations.push(ImpactTruncation {
                group,
                returned,
                total,
                depth,
            });
        }
        leads.into_iter().take(cap).collect()
    }

    // ── latest-write-wins tombstone / temporal liveness (issue #421) ──────────
    // Over an append-only `--graph`, a node re-ingested AFTER its own tombstone
    // is live again; the shared gate reports a tombstone active only when it is
    // the id's most recent write, matching the embedded current-state read so
    // `--graph` and `--data-dir` agree. See `super::liveness`.
    let liveness = Liveness::new(records);
    let deleted = |id: &str| liveness.deleted(id);
    let by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|r| {
            let id = r.id();
            if deleted(id) { None } else { Some((id, r)) }
        })
        .collect();

    // ── edge indexes (inbound and outbound, code-topology labels only) ─────────
    // outbound_edges[source_id] = Vec<(edge_record_id, label, target_id)>
    let mut outbound_edges: BTreeMap<&str, Vec<(&str, &EdgeLabel, &str)>> = BTreeMap::new();
    // inbound_edges[target_id] = Vec<(edge_record_id, label, source_id)>
    let mut inbound_edges: BTreeMap<&str, Vec<(&str, &EdgeLabel, &str)>> = BTreeMap::new();

    for r in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = r
        {
            if deleted(id.as_str()) {
                continue;
            }
            if !IMPACT_LABELS.contains(label) {
                continue;
            }
            outbound_edges.entry(source.as_str()).or_default().push((
                id.as_str(),
                label,
                target.as_str(),
            ));
            inbound_edges.entry(target.as_str()).or_default().push((
                id.as_str(),
                label,
                source.as_str(),
            ));
        }
    }

    let target_kind = match target.kind {
        FailureTargetKind::File => "file",
        FailureTargetKind::Task | FailureTargetKind::Source | FailureTargetKind::Symbol => "symbol",
    };
    let target_ids: Vec<String> = target.anchor_ids.iter().cloned().collect();

    let mut direct_callers: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut direct_callees: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut referencing_files: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut implementation_symbols: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut containing_context: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    // The queried target's own resolved anchor(s). These are never reported as
    // their own impact leads — a back-edge such as a `caller → anchor` CALLS
    // edge, traversed at depth ≥ 2, would otherwise surface the anchor under
    // `direct_callees`, which is not a lead to inspect.
    let original_targets: BTreeSet<&str> = target
        .anchor_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).map(|r| r.id()))
        .collect();

    // ── BFS frontier ──────────────────────────────────────────────────────────
    // For a File handle, seed the symbols it defines/contains so that callers of
    // those symbols are reachable at hop 1 (mirrors subsystem/semantic-context).
    // Follow nested containers transitively — modules (File CONTAINS Module
    // DEFINES fn) and impl-block Symbols whose methods are emitted beneath them —
    // so every symbol declared in the file is seeded. A Symbol handle seeds only
    // itself: its owned children (e.g. an impl block's methods) are not the
    // queried symbol, so their callers/callees must not be reported as direct
    // leads.
    let seed_descendants = matches!(target.kind, FailureTargetKind::File);
    let mut frontier: BTreeSet<&str> = BTreeSet::new();
    for anchor_id in &target.anchor_ids {
        if let Some(id_ref) = by_id.get(anchor_id.as_str()).map(|r| r.id()) {
            frontier.insert(id_ref);
            if !seed_descendants {
                continue;
            }
            let mut containers: Vec<&str> = vec![id_ref];
            let mut expanded: BTreeSet<&str> = BTreeSet::new();
            while let Some(container) = containers.pop() {
                if !expanded.insert(container) {
                    continue;
                }
                #[allow(clippy::map_unwrap_or)]
                for &(_, label, child_id) in outbound_edges
                    .get(container)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    if !matches!(label, EdgeLabel::Defines | EdgeLabel::Contains) {
                        continue;
                    }
                    // Seed the child and recurse into anything it owns. Modules
                    // own nested symbols; impl-block Symbols own their method
                    // Symbols (emitted via `owner_id()`), so a file handle reaches
                    // methods defined in the file.
                    if matches!(
                        by_id.get(child_id).copied().and_then(record_node_kind),
                        Some(NodeKind::Symbol | NodeKind::Module)
                    ) {
                        frontier.insert(child_id);
                        containers.push(child_id);
                    }
                }
            }
        }
    }

    // The target's own anchors and seeded symbols. `containing_context` is
    // reported only for these so the group reflects the target's container,
    // never an intermediate caller/callee file reached at depth ≥ 2.
    let seed_set: BTreeSet<&str> = frontier.iter().copied().collect();

    let mut visited: BTreeSet<&str> = BTreeSet::new();

    for hop in 1..=depth {
        // Stop as soon as the frontier is exhausted so a very large `--depth`
        // does not spin through empty iterations after traversal is complete.
        if frontier.is_empty() {
            break;
        }
        let current_frontier: Vec<&str> = frontier.iter().copied().collect();
        let mut next_frontier: BTreeSet<&str> = BTreeSet::new();

        for &anchor_id in &current_frontier {
            if visited.contains(anchor_id) {
                continue;
            }
            visited.insert(anchor_id);

            // ── Inbound edges ─────────────────────────────────────────────────
            #[allow(clippy::map_unwrap_or)]
            for &(edge_id, label, source_id) in inbound_edges
                .get(anchor_id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let Some(&edge_record) = by_id.get(edge_id) else {
                    continue;
                };
                match label {
                    EdgeLabel::Calls => {
                        // caller → anchor: the source is a direct caller
                        match by_id.get(source_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                direct_callers
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "CALLS",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand callers at next hop (only symbols)
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            // The queried target itself is not a lead about itself.
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "CALLS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::References => {
                        // referencing symbol → anchor
                        match by_id.get(source_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                referencing_files.entry((node.id(), edge_id)).or_insert(
                                    ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "REFERENCES",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    },
                                );
                                // Expand referencing symbols at the next hop so a
                                // wider `--depth` reaches their callers/referrers
                                // (reached symbol nodes expand; file/module owners
                                // do not).
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "REFERENCES".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::Imports => {
                        // file → import: if a file node imports something referencing this anchor
                        match by_id.get(source_id) {
                            Some(&node)
                                if matches!(
                                    record_node_kind(node),
                                    Some(NodeKind::File | NodeKind::Module)
                                ) =>
                            {
                                referencing_files.entry((node.id(), edge_id)).or_insert(
                                    ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "IMPORTS",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    },
                                );
                            }
                            Some(_) => {
                                // Imports from non-file/module source: emit unsupported diagnostic
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unsupported_relation".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "IMPORTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "IMPORTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::Implements => {
                        // impl_sym → anchor (anchor is the trait)
                        match by_id.get(source_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                implementation_symbols
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "IMPLEMENTS",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand implementation symbols at the next hop so
                                // a wider `--depth` reaches their callers/callees.
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "IMPLEMENTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::Defines | EdgeLabel::Contains => {
                        // owner → anchor: containing file/module context.
                        //
                        // Only report the container of the *queried target*
                        // (its own anchors/seeded symbols), never of an
                        // intermediate caller/callee reached at depth ≥ 2, and
                        // only when the owner is a File or Module (a
                        // `Repository CONTAINS File` owner is not containing
                        // code context). Key by owner record id (not edge id)
                        // so a file handle that seeds every defined symbol
                        // reports each owner once.
                        if seed_set.contains(anchor_id) {
                            match by_id.get(source_id) {
                                Some(&node) => {
                                    // Resolve the owner to its File/Module context,
                                    // climbing an impl-block Symbol owner up to the
                                    // file that contains it (a method's container is
                                    // its file, not the impl). A Repository owner
                                    // resolves to nothing and is not reported.
                                    if let Some((ctx, ctx_edge)) = containing_file_or_module(
                                        node,
                                        edge_record,
                                        &by_id,
                                        &inbound_edges,
                                    ) {
                                        let relation = match ctx_edge {
                                            GraphRecord::Edge { label: l, .. } => l.as_str(),
                                            _ => label.as_str(),
                                        };
                                        containing_context.entry((ctx.id(), ctx.id())).or_insert(
                                            ImpactLead {
                                                record: ctx,
                                                edge: ctx_edge,
                                                relation,
                                                direction: ImpactDirection::Inbound,
                                                anchor_id,
                                                hop,
                                            },
                                        );
                                    }
                                }
                                None => {
                                    diagnostics.push(MemoryAuditDiagnostic {
                                        code: "unresolved_edge_target".to_owned(),
                                        source_record_id: edge_id.to_owned(),
                                        target_handle: source_id.to_owned(),
                                        relation: label.as_str().to_owned(),
                                        target_domain: "codegraph".to_owned(),
                                    });
                                }
                            }
                        }
                    }
                    _ => {
                        // Unexpected in-scope label — emit diagnostic
                        diagnostics.push(MemoryAuditDiagnostic {
                            code: "unsupported_relation".to_owned(),
                            source_record_id: edge_id.to_owned(),
                            target_handle: source_id.to_owned(),
                            relation: label.as_str().to_owned(),
                            target_domain: "codegraph".to_owned(),
                        });
                    }
                }
            }

            // ── Outbound edges ────────────────────────────────────────────────
            #[allow(clippy::map_unwrap_or)]
            for &(edge_id, label, target_id) in outbound_edges
                .get(anchor_id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let Some(&edge_record) = by_id.get(edge_id) else {
                    continue;
                };
                match label {
                    EdgeLabel::Calls => {
                        // anchor → callee
                        match by_id.get(target_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                direct_callees
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "CALLS",
                                        direction: ImpactDirection::Outbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand callees at next hop (only symbols)
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            // The queried target itself is not a lead about itself.
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: target_id.to_owned(),
                                    relation: "CALLS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    // Outbound References are the anchor's own dependencies, not
                    // code that points at it. referencing_files is documented as
                    // inbound-only, so outbound references are intentionally not
                    // emitted there (they fall through to the `_` arm below).
                    EdgeLabel::Implements => {
                        // anchor → trait (anchor is an impl block)
                        match by_id.get(target_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                implementation_symbols
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "IMPLEMENTS",
                                        direction: ImpactDirection::Outbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand implementation/trait symbols at the next
                                // hop so a wider `--depth` reaches their neighbors.
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: target_id.to_owned(),
                                    relation: "IMPLEMENTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    // Defines/Contains/Imports outbound = children or import targets,
                    // not inbound leads from the anchor's perspective.
                    _ => {}
                }
            }
        }

        frontier = next_frontier;
    }

    // ── Import resolution (name-based) ─────────────────────────────────────────
    // The Rust extractor records `use` imports as `File/Module --IMPORTS--> Import`
    // nodes (and `Symbol --IMPORTS--> Import` for imports local to an impl), whose
    // name is the imported path; there is no structural edge from the Import node
    // to the symbol it imports. Connect them by matching each imported final path
    // segment — grouped (`a::{X, Y}`) and aliased (`X as Y`) imports expanded — to
    // a seeded anchor symbol's name, then report the importing file/module/symbol
    // as a `referencing_files` lead. Owners are constrained to the queried
    // anchors' repositories so a `--repo`-scoped query never reports a same-named
    // import from another repository. Name-based, so same-name collisions can
    // surface extra leads — consistent with the "leads, not proof" contract.
    let mut anchor_names: BTreeMap<&str, &str> = BTreeMap::new();
    for id in &seed_set {
        if let Some(&node) = by_id.get(*id)
            && matches!(record_node_kind(node), Some(NodeKind::Symbol))
            && let GraphRecord::Node {
                name: Some(name), ..
            } = node
        {
            anchor_names.entry(last_path_segment(name)).or_insert(*id);
        }
    }
    // Import leads are hop-1 neighbours, so they are only produced when at least
    // one hop is requested (a `--depth 0` query reports no impact leads at all).
    if depth >= 1 && !anchor_names.is_empty() {
        // Repositories of the queried anchors. Empty when the store has no
        // repository attribution, in which case import owners are not filtered.
        let anchor_repos: BTreeSet<&str> = original_targets
            .iter()
            .filter_map(|id| repo_index.owner_of(id))
            .collect();
        for r in records {
            let GraphRecord::Node {
                id: import_id,
                kind: NodeKind::Import,
                name: Some(import_name),
                ..
            } = r
            else {
                continue;
            };
            if deleted(import_id.as_str()) {
                continue;
            }
            let Some(&anchor) = imported_symbol_names(import_name)
                .into_iter()
                .find_map(|seg| anchor_names.get(seg))
            else {
                continue;
            };
            #[allow(clippy::map_unwrap_or)]
            for &(edge_id, label, owner_id) in inbound_edges
                .get(import_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                if !matches!(label, EdgeLabel::Imports) {
                    continue;
                }
                let (Some(&owner), Some(&edge_record)) = (by_id.get(owner_id), by_id.get(edge_id))
                else {
                    continue;
                };
                // The owner is the importing file/module, or the owning Symbol
                // for an import local to an impl method. Never report the queried
                // target itself.
                if !matches!(
                    record_node_kind(owner),
                    Some(NodeKind::File | NodeKind::Module | NodeKind::Symbol)
                ) || original_targets.contains(owner.id())
                {
                    continue;
                }
                // Repo scope applies only when the caller passed `--repo`. For a
                // scoped query the owner must resolve to one of the anchors'
                // repositories — an owner with no repository attribution is out of
                // scope and skipped, so an unattributed legacy/generated file
                // cannot leak a cross-repo lead. An unscoped query does not filter,
                // so legitimate cross-repo importers are still reported.
                if repo_scope.is_some()
                    && !anchor_repos.is_empty()
                    && !repo_index
                        .owner_of(owner.id())
                        .is_some_and(|repo| anchor_repos.contains(repo))
                {
                    continue;
                }
                referencing_files
                    .entry((owner.id(), edge_id))
                    .or_insert(ImpactLead {
                        record: owner,
                        edge: edge_record,
                        relation: "IMPORTS",
                        direction: ImpactDirection::Inbound,
                        anchor_id: anchor,
                        hop: 1,
                    });
            }
        }
    }

    // ── Sort all groups canonically and apply per-group cap (AC6/AC7) ──────────
    let mut truncations: Vec<ImpactTruncation> = Vec::new();

    let direct_callers = drain_sorted(
        direct_callers,
        "direct_callers",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let direct_callees = drain_sorted(
        direct_callees,
        "direct_callees",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let referencing_files = drain_sorted(
        referencing_files,
        "referencing_files",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let implementation_symbols = drain_sorted(
        implementation_symbols,
        "implementation_symbols",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let containing_context = drain_sorted(
        containing_context,
        "containing_context",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );

    // ── Sort and dedup diagnostics ────────────────────────────────────────────
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    // Emit neighborhood_truncated diagnostics for each truncation
    for t in &truncations {
        diagnostics.push(MemoryAuditDiagnostic {
            code: "neighborhood_truncated".to_owned(),
            source_record_id: String::new(),
            target_handle: t.group.to_owned(),
            relation: format!(
                "returned={} total={} depth={}",
                t.returned, t.total, t.depth
            ),
            target_domain: String::new(),
        });
    }
    // Re-sort after appending truncation diagnostics
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    ChangeImpactContext {
        target_kind,
        target_ids,
        direct_callers,
        direct_callees,
        referencing_files,
        implementation_symbols,
        containing_context,
        diagnostics,
        depth,
        truncations,
    }
}

// ---------------------------------------------------------------------------
// Transitive inbound reachability — `eg query transitive-callers` (issue #139)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod liveness_parity_tests {
    //! Transport-parity regression (issue #421): a symbol re-ingested AFTER its
    //! own tombstone is live again over `--graph`, so its callers surface as
    //! impact leads — matching the embedded current-state read; a tombstone with
    //! no later re-add still deletes the anchor.
    use super::*;
    use crate::ir::SourceSpan;
    use std::collections::BTreeSet;

    fn sym(id: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn calls(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "calls".to_owned(),
        )
    }

    fn tomb(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v5:tomb_{deleted_id}"),
            schema_version: 5,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    fn target(anchor: &str) -> ResolvedFailureTarget {
        let mut anchor_ids = BTreeSet::new();
        anchor_ids.insert(anchor.to_owned());
        ResolvedFailureTarget {
            handle: anchor.to_owned(),
            kind: FailureTargetKind::Symbol,
            anchor_ids,
            seed_failures: BTreeSet::new(),
            stale: false,
        }
    }

    #[test]
    fn caller_of_reingested_symbol_surfaces_as_impact_lead() {
        // Append order: A, Tombstone(A), A again, caller B, B --CALLS--> A.
        let records = vec![
            sym("codegraph:v5:a", "a"),
            tomb("codegraph:v5:a"),
            sym("codegraph:v5:a", "a"),
            sym("codegraph:v5:b", "b"),
            calls("codegraph:v5:b", "codegraph:v5:a"),
        ];
        let repo_index = RepositoryIndex::build(&records);
        let ctx = change_impact_context(&records, &target("codegraph:v5:a"), 1, &repo_index, None);
        assert!(
            ctx.direct_callers
                .iter()
                .any(|l| l.record.id() == "codegraph:v5:b"),
            "the caller of a symbol revived after its tombstone must surface as an impact lead"
        );
    }

    #[test]
    fn anchor_tombstone_without_reingest_yields_no_leads() {
        let records = vec![
            sym("codegraph:v5:a", "a"),
            tomb("codegraph:v5:a"),
            sym("codegraph:v5:b", "b"),
            calls("codegraph:v5:b", "codegraph:v5:a"),
        ];
        let repo_index = RepositoryIndex::build(&records);
        let ctx = change_impact_context(&records, &target("codegraph:v5:a"), 1, &repo_index, None);
        assert!(
            ctx.direct_callers.is_empty(),
            "a tombstone with no later re-ingest still deletes the anchor, so it has no leads"
        );
    }
}
