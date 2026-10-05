//! Bounded local call-neighborhood walk (issue #228).
//!
//! The walk behind `eg query diagram`: a level-synchronized BFS from one
//! resolved symbol anchor over the diagram edge vocabulary, bounded by a hop
//! depth and a node budget. It is a deterministic projection of stored edges —
//! no synthesized relations, no embeddings, no network.
//!
//! Expansion rule (documented in `docs/cli/diagram.md`):
//!
//! - from a `Symbol`: inbound `CALLS` (callers), outbound `CALLS` (callees),
//!   inbound `DEFINES`/`CONTAINS` (containing file/module);
//! - from a `File`/`Module`: outbound `DEFINES`/`CONTAINS` (member symbols),
//!   outbound `IMPORTS` (imported files).
//!
//! The rendered edge set is the induced subgraph over the admitted nodes
//! restricted to the walk vocabulary: every stored edge of those kinds whose
//! endpoints are both rendered appears, and no other edge appears.
//!
//! Liveness follows the shared latest-write-wins gate (`Liveness`, issue
//! #421): a tombstone suppresses an id only when it is the id's most recent
//! write, and only the latest version of a stable edge id supplies adjacency,
//! so `--graph` and `--data-dir` agree.

use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{MemoryAuditDiagnostic, record_node_kind};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// Closed diagram edge vocabulary (issue #228): no new labels, no synthesis.
pub const NEIGHBORHOOD_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Calls,
    EdgeLabel::Defines,
    EdgeLabel::Contains,
    EdgeLabel::Imports,
];

/// Inbound labels expanded from a symbol node: callers and containers.
const SYMBOL_INBOUND: &[EdgeLabel] = &[EdgeLabel::Calls, EdgeLabel::Defines, EdgeLabel::Contains];
/// Outbound labels expanded from a symbol node: callees.
const SYMBOL_OUTBOUND: &[EdgeLabel] = &[EdgeLabel::Calls];
/// Outbound labels expanded from a file/module node: members and imports.
const CONTAINER_OUTBOUND: &[EdgeLabel] =
    &[EdgeLabel::Defines, EdgeLabel::Contains, EdgeLabel::Imports];

/// Borrowed edge reference: `(neighbor_id, edge_id, label)`.
type NeighborhoodEdgeRef<'a> = (&'a str, &'a str, EdgeLabel);

/// Inbound and outbound adjacency maps over live, allowed-vocabulary edges.
type AdjacencyMaps<'a> = (
    BTreeMap<&'a str, Vec<NeighborhoodEdgeRef<'a>>>,
    BTreeMap<&'a str, Vec<NeighborhoodEdgeRef<'a>>>,
);

/// One admitted node: the record plus its BFS hop distance from the anchor.
#[derive(Debug, Clone)]
pub struct NeighborhoodNode<'a> {
    /// The live node record.
    pub record: &'a GraphRecord,
    /// Shortest hop distance from the anchor (0 for the anchor itself).
    pub hop: usize,
}

/// One rendered edge: a stored vocabulary edge with both endpoints admitted.
#[derive(Debug, Clone)]
pub struct NeighborhoodEdge<'a> {
    /// Stable record ID of the stored edge.
    pub edge_record_id: &'a str,
    /// Edge label from the closed vocabulary (`CALLS`, `DEFINES`, ...).
    pub label: &'static str,
    /// Record ID of the edge source.
    pub source_id: &'a str,
    /// Record ID of the edge target.
    pub target_id: &'a str,
}

/// Dropped-frontier count at one depth beyond the `--depth` bound.
#[derive(Debug, Clone, Copy)]
pub struct NeighborhoodDroppedDepth {
    /// Depth (hop distance) at which these nodes would have been discovered.
    pub depth: usize,
    /// Number of distinct nodes first reachable at that depth.
    pub count: usize,
}

/// Truncation diagnostic emitted when reachable nodes exist beyond `--depth`.
#[derive(Debug, Clone)]
pub struct NeighborhoodDepthTruncation {
    /// The depth bound in effect.
    pub max_depth: usize,
    /// Dropped frontier counts per depth beyond the bound, ascending.
    pub dropped_frontier: Vec<NeighborhoodDroppedDepth>,
    /// Total dropped nodes across all depths beyond the bound.
    pub dropped_total: usize,
}

/// Truncation diagnostic emitted when the node budget cut the neighborhood.
#[derive(Debug, Clone, Copy)]
pub struct NeighborhoodBudgetTruncation {
    /// The node budget in effect (anchor included).
    pub max_nodes: usize,
    /// Discovered nodes excluded by the budget.
    pub dropped_nodes: usize,
    /// Stored vocabulary edges excluded because an endpoint was dropped.
    pub dropped_edges: usize,
}

/// Structured neighborhood result returned by [`neighborhood`].
#[derive(Debug)]
pub struct Neighborhood<'a> {
    /// The resolved anchor (queried symbol) record.
    pub anchor: &'a GraphRecord,
    /// Admitted nodes, canonically ordered by ascending record ID.
    pub nodes: Vec<NeighborhoodNode<'a>>,
    /// Induced vocabulary edges over the admitted nodes, canonically ordered
    /// by `(source_id, target_id, label, edge_record_id)` ascending.
    pub edges: Vec<NeighborhoodEdge<'a>>,
    /// Depth-bound truncation diagnostic, when reachable nodes were dropped.
    pub depth_truncation: Option<NeighborhoodDepthTruncation>,
    /// Node-budget truncation diagnostic, when discovered nodes were dropped.
    pub budget_truncation: Option<NeighborhoodBudgetTruncation>,
    /// Stable machine-readable diagnostics (dangling edge endpoints).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// The depth bound used for the walk.
    pub max_depth: usize,
    /// The node budget used for the walk.
    pub max_nodes: usize,
}

/// Builds the inbound/outbound vocabulary edge indexes with
/// latest-edge-version semantics (issue #421): over an append-only `--graph`
/// a stable edge id may be re-ingested with changed metadata, and only the
/// latest EDGE write for the id is live.
fn build_indexes<'a>(
    records: &'a [GraphRecord],
    liveness: &Liveness,
    deleted: &dyn Fn(&str) -> bool,
) -> AdjacencyMaps<'a> {
    let mut inbound: BTreeMap<&str, Vec<NeighborhoodEdgeRef<'_>>> = BTreeMap::new();
    let mut outbound: BTreeMap<&str, Vec<NeighborhoodEdgeRef<'_>>> = BTreeMap::new();
    for (index, r) in records.iter().enumerate() {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = r
        {
            if !liveness.is_latest_edge_version(id.as_str(), index) {
                continue;
            }
            if deleted(id.as_str()) || !NEIGHBORHOOD_LABELS.contains(label) {
                continue;
            }
            inbound.entry(target.as_str()).or_default().push((
                source.as_str(),
                id.as_str(),
                *label,
            ));
            outbound.entry(source.as_str()).or_default().push((
                target.as_str(),
                id.as_str(),
                *label,
            ));
        }
    }
    // Deterministic visit order; drop exact duplicates from history views
    // where the same stable edge id recurs across commit snapshots.
    for edges in inbound.values_mut().chain(outbound.values_mut()) {
        edges.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        edges.dedup_by_key(|e| (e.0, e.1));
    }
    (inbound, outbound)
}

/// Candidate neighbors of `node` under the expansion rule, as
/// `(neighbor_id, edge_id)` pairs. Dangling endpoints produce a diagnostic
/// instead of a discovery.
fn candidates<'a>(
    node: &'a str,
    kind: NodeKind,
    maps: &AdjacencyMaps<'a>,
    visited: &BTreeSet<&str>,
    by_id: &BTreeMap<&'a str, &'a GraphRecord>,
    deleted: &dyn Fn(&str) -> bool,
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
) -> BTreeMap<&'a str, &'a str> {
    let (inbound, outbound) = maps;
    let mut out: BTreeMap<&str, &str> = BTreeMap::new();
    let mut consider = |neighbor: &'a str, edge_id: &'a str, code: &str| {
        if visited.contains(neighbor) {
            return;
        }
        if by_id.contains_key(neighbor) {
            // Keep the minimum edge id per neighbor for determinism.
            out.entry(neighbor)
                .and_modify(|e| {
                    if edge_id < *e {
                        *e = edge_id;
                    }
                })
                .or_insert(edge_id);
        } else if !deleted(neighbor) {
            diagnostics.push(MemoryAuditDiagnostic {
                code: code.to_owned(),
                source_record_id: edge_id.to_owned(),
                target_handle: neighbor.to_owned(),
                relation: String::new(),
                target_domain: "codegraph".to_owned(),
            });
        }
    };
    match kind {
        NodeKind::Symbol => {
            if let Some(edges) = inbound.get(node) {
                for &(source, edge_id, label) in edges {
                    if SYMBOL_INBOUND.contains(&label) {
                        consider(source, edge_id, "unresolved_edge_source");
                    }
                }
            }
            if let Some(edges) = outbound.get(node) {
                for &(target, edge_id, label) in edges {
                    if SYMBOL_OUTBOUND.contains(&label) {
                        consider(target, edge_id, "unresolved_edge_target");
                    }
                }
            }
        }
        NodeKind::File | NodeKind::Module => {
            if let Some(edges) = outbound.get(node) {
                for &(target, edge_id, label) in edges {
                    if CONTAINER_OUTBOUND.contains(&label) {
                        consider(target, edge_id, "unresolved_edge_target");
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// Walks the bounded local call neighborhood of one resolved symbol anchor
/// (issue #228).
///
/// Level-synchronized BFS over the documented expansion rule, so every
/// admitted node carries its shortest hop distance. A visited set guarantees
/// each node is admitted at most once and cycles terminate. The anchor itself
/// is always admitted (callers must pass `max_nodes >= 1`).
///
/// After the walk the induced vocabulary subgraph over the admitted nodes is
/// materialized; when the node budget cut the discovery, the dropped nodes
/// and their incident edges are counted in
/// [`Neighborhood::budget_truncation`] rather than silently omitted. When
/// reachable nodes exist beyond `max_depth`, the dropped frontier is counted
/// per depth in [`Neighborhood::depth_truncation`].
///
/// Returns `None` when `anchor_id` names no live node in `records`.
///
/// # Panics
///
/// Panics only if an internal invariant is violated: every id admitted to the
/// walk is either the anchor (looked up in the live-node index up front) or a
/// candidate neighbor, and [`candidates`] only yields neighbors present in
/// that same index — so the record lookup while building the node list cannot
/// miss.
#[must_use]
pub fn neighborhood<'a>(
    records: &'a [GraphRecord],
    anchor_id: &str,
    max_depth: usize,
    max_nodes: usize,
) -> Option<Neighborhood<'a>> {
    debug_assert!(max_nodes >= 1, "node budget must admit the anchor");
    if max_nodes == 0 {
        return None;
    }

    // ── latest-write-wins tombstone / temporal liveness (issue #421) ──────────
    let liveness = Liveness::new(records);
    let deleted = |id: &str| liveness.deleted(id);
    let by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|r| {
            let id = r.id();
            if deleted(id) { None } else { Some((id, r)) }
        })
        .collect();

    let anchor = by_id.get(anchor_id).copied()?;
    let anchor_id: &str = anchor.id();

    let maps = build_indexes(records, &liveness, &deleted);

    // ── level-synchronized BFS from the anchor ────────────────────────────────
    let mut hop_of: BTreeMap<&str, usize> = BTreeMap::new();
    hop_of.insert(anchor_id, 0);
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    visited.insert(anchor_id);
    let mut frontier: Vec<&str> = vec![anchor_id];
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    let mut depth = 0usize;
    while depth < max_depth {
        if frontier.is_empty() {
            break;
        }
        depth += 1;
        let mut discoveries: BTreeSet<&str> = BTreeSet::new();
        for &node in &frontier {
            let kind = by_id
                .get(node)
                .copied()
                .and_then(record_node_kind)
                .unwrap_or(NodeKind::Diagnostic);
            for (neighbor, _) in candidates(
                node,
                kind,
                &maps,
                &visited,
                &by_id,
                &deleted,
                &mut diagnostics,
            ) {
                // `discoveries` only needs the neighbor set: the edge id is
                // kept minimum-per-neighbor inside `candidates` for
                // determinism, and only surfaces in diagnostics.
                discoveries.insert(neighbor);
            }
        }
        frontier = discoveries.iter().copied().collect();
        for node in frontier.iter().copied() {
            visited.insert(node);
            hop_of.insert(node, depth);
        }
    }

    // ── dropped-frontier counting beyond the depth bound ──────────────────────
    let mut depth_truncation: Option<NeighborhoodDepthTruncation> = None;
    if !frontier.is_empty() {
        let mut dropped_frontier: Vec<NeighborhoodDroppedDepth> = Vec::new();
        let mut dropped_total = 0usize;
        let mut count_depth = depth;
        loop {
            let mut discoveries: BTreeSet<&str> = BTreeSet::new();
            for &node in &frontier {
                let kind = by_id
                    .get(node)
                    .copied()
                    .and_then(record_node_kind)
                    .unwrap_or(NodeKind::Diagnostic);
                for (neighbor, _) in candidates(
                    node,
                    kind,
                    &maps,
                    &visited,
                    &by_id,
                    &deleted,
                    &mut Vec::new(),
                ) {
                    discoveries.insert(neighbor);
                }
            }
            if discoveries.is_empty() {
                break;
            }
            count_depth += 1;
            dropped_frontier.push(NeighborhoodDroppedDepth {
                depth: count_depth,
                count: discoveries.len(),
            });
            dropped_total += discoveries.len();
            frontier = discoveries.iter().copied().collect();
            for node in frontier.iter().copied() {
                visited.insert(node);
            }
        }
        if dropped_total > 0 {
            depth_truncation = Some(NeighborhoodDepthTruncation {
                max_depth,
                dropped_frontier,
                dropped_total,
            });
        }
    }

    // ── node budget: admit in (hop, record ID) order ──────────────────────────
    let mut discovered: Vec<(&str, usize)> = hop_of.iter().map(|(&id, &hop)| (id, hop)).collect();
    discovered.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    let admitted: BTreeSet<&str> = discovered
        .iter()
        .take(max_nodes)
        .map(|(id, _)| *id)
        .collect();

    // ── induced vocabulary subgraph over the admitted nodes ───────────────────
    let mut edges: Vec<NeighborhoodEdge<'_>> = Vec::new();
    let mut full_edge_count = 0usize;
    for (index, r) in records.iter().enumerate() {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = r
        {
            if !liveness.is_latest_edge_version(id.as_str(), index) {
                continue;
            }
            if deleted(id.as_str()) || !NEIGHBORHOOD_LABELS.contains(label) {
                continue;
            }
            let (source, target) = (source.as_str(), target.as_str());
            if !hop_of.contains_key(source) || !hop_of.contains_key(target) {
                continue;
            }
            full_edge_count += 1;
            if admitted.contains(source) && admitted.contains(target) {
                edges.push(NeighborhoodEdge {
                    edge_record_id: id.as_str(),
                    label: label.as_str(),
                    source_id: source,
                    target_id: target,
                });
            }
        }
    }
    // Deterministic edge visit order; drop exact duplicates from history views
    // where the same stable edge id recurs across commit snapshots.
    edges.sort_by(|a, b| {
        (a.source_id, a.target_id, a.label, a.edge_record_id).cmp(&(
            b.source_id,
            b.target_id,
            b.label,
            b.edge_record_id,
        ))
    });
    edges.dedup_by_key(|e| (e.source_id, e.target_id, e.edge_record_id));

    let budget_truncation = if admitted.len() < discovered.len() {
        Some(NeighborhoodBudgetTruncation {
            max_nodes,
            dropped_nodes: discovered.len() - admitted.len(),
            dropped_edges: full_edge_count - edges.len(),
        })
    } else {
        None
    };

    let mut nodes: Vec<NeighborhoodNode<'_>> = admitted
        .iter()
        .map(|id| NeighborhoodNode {
            record: by_id
                .get(id)
                .copied()
                .expect("admitted nodes are live records"),
            hop: hop_of
                .get(id)
                .copied()
                .expect("admitted nodes were discovered"),
        })
        .collect();
    // Canonical node order: ascending record ID; the renderer assigns node
    // keys in this order.
    nodes.sort_by(|a, b| a.record.id().cmp(b.record.id()));

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
    });

    Some(Neighborhood {
        anchor,
        nodes,
        edges,
        depth_truncation,
        budget_truncation,
        diagnostics,
        max_depth,
        max_nodes,
    })
}

#[cfg(test)]
mod tests {
    //! Unit tests for the neighborhood walk: expansion rule, ordering, and
    //! truncation accounting on hand-built record sets.
    use super::*;
    use crate::ir::SourceSpan;

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
                start_column: None,
                end_column: None,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn file(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("file {path}"),
        )
    }

    fn calls(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            None,
            "calls".to_owned(),
        )
    }

    fn defines(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Defines,
            source.to_owned(),
            target.to_owned(),
            None,
            "defines".to_owned(),
        )
    }

    fn node_ids<'x>(n: &'x Neighborhood<'x>) -> Vec<&'x str> {
        n.nodes.iter().map(|node| node.record.id()).collect()
    }

    #[test]
    fn depth_one_covers_callers_callees_and_container() {
        let records = vec![
            sym("s:anchor", "anchor"),
            sym("s:caller", "caller"),
            sym("s:callee", "callee"),
            sym("s:unrelated", "unrelated"),
            file("f:lib", "src/lib.rs"),
            calls("s:caller", "s:anchor"),
            calls("s:anchor", "s:callee"),
            defines("f:lib", "s:anchor"),
            defines("f:lib", "s:unrelated"),
        ];
        let n = neighborhood(&records, "s:anchor", 1, 100).expect("anchor live");
        // `s:unrelated` is not reachable at depth 1; `f:lib` IS admitted via
        // the DEFINES container edge, and the induced subgraph then also
        // carries f:lib DEFINES s:unrelated? No — s:unrelated is not admitted,
        // so that edge is excluded.
        assert_eq!(
            node_ids(&n),
            vec!["f:lib", "s:anchor", "s:callee", "s:caller"]
        );
        let labels: Vec<(&str, &str, &str)> = n
            .edges
            .iter()
            .map(|e| (e.source_id, e.target_id, e.label))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("f:lib", "s:anchor", "DEFINES"),
                ("s:anchor", "s:callee", "CALLS"),
                ("s:caller", "s:anchor", "CALLS"),
            ]
        );
        assert!(n.depth_truncation.is_none());
        assert!(n.budget_truncation.is_none());
    }

    #[test]
    fn container_expansion_reaches_members_and_imports_at_depth_two() {
        let records = vec![
            sym("s:anchor", "anchor"),
            sym("s:sibling", "sibling"),
            file("f:lib", "src/lib.rs"),
            file("f:dep", "src/dep.rs"),
            defines("f:lib", "s:anchor"),
            defines("f:lib", "s:sibling"),
            GraphRecord::edge(
                EdgeLabel::Imports,
                "f:lib".to_owned(),
                "f:dep".to_owned(),
                None,
                "imports".to_owned(),
            ),
        ];
        let n = neighborhood(&records, "s:anchor", 2, 100).expect("anchor live");
        assert_eq!(
            node_ids(&n),
            vec!["f:dep", "f:lib", "s:anchor", "s:sibling"]
        );
        // Depth 1 must not include the sibling or the import.
        let n1 = neighborhood(&records, "s:anchor", 1, 100).expect("anchor live");
        assert_eq!(node_ids(&n1), vec!["f:lib", "s:anchor"]);
        assert!(n1.depth_truncation.is_some());
    }

    #[test]
    fn budget_admits_in_hop_then_record_id_order() {
        let records = vec![
            sym("s:anchor", "anchor"),
            sym("s:c1", "c1"),
            sym("s:c2", "c2"),
            sym("s:c3", "c3"),
            calls("s:c1", "s:anchor"),
            calls("s:c2", "s:anchor"),
            calls("s:c3", "s:anchor"),
        ];
        // Budget 3: anchor + the two smallest caller IDs; c3 dropped with its
        // one incident edge.
        let n = neighborhood(&records, "s:anchor", 1, 3).expect("anchor live");
        assert_eq!(node_ids(&n), vec!["s:anchor", "s:c1", "s:c2"]);
        let trunc = n.budget_truncation.expect("budget truncated");
        assert_eq!(trunc.max_nodes, 3);
        assert_eq!(trunc.dropped_nodes, 1);
        assert_eq!(trunc.dropped_edges, 1);
    }

    #[test]
    fn anchor_without_neighbors_yields_anchor_only() {
        let records = vec![sym("s:anchor", "anchor")];
        let n = neighborhood(&records, "s:anchor", 1, 100).expect("anchor live");
        assert_eq!(node_ids(&n), vec!["s:anchor"]);
        assert!(n.edges.is_empty());
    }

    #[test]
    fn tombstoned_anchor_resolves_to_none() {
        let records = vec![
            sym("s:anchor", "anchor"),
            GraphRecord::Tombstone {
                id: "t:1".to_owned(),
                schema_version: crate::ir::SCHEMA_VERSION,
                deleted_id: "s:anchor".to_owned(),
                summary: "removed".to_owned(),
                producer: None,
            },
        ];
        assert!(neighborhood(&records, "s:anchor", 1, 100).is_none());
    }
}
