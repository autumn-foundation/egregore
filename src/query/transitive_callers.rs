use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{MemoryAuditDiagnostic, ResolvedFailureTarget, record_node_kind};
use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind};

/// Edge labels the transitive-callers walk traverses inbound. `MENTIONS` and
/// other weak/topology labels are excluded so the depth-1 result is exactly
/// the direct inbound `CALLS`/`REFERENCES` neighbor set of #122/#76.
const TRANSITIVE_CALLER_LABELS: &[EdgeLabel] = &[EdgeLabel::Calls, EdgeLabel::References];

/// Borrowed 4-tuple used by the transitive walk's indexes: an inbound edge
/// `(source_id, edge_id, label, resolution)` or a shortest-path discovery
/// pointer `(edge_id, parent_id, label, resolution)`.
type TransitiveEdgeRef<'a> = (&'a str, &'a str, &'static str, Option<CallResolution>);

/// One hop of a concrete connecting call path: `source` calls/references
/// `target`, moving one step from a reachable node toward the queried symbol.
#[derive(Debug, Clone, Copy)]
pub struct TransitivePathStep<'a> {
    /// Record ID of the caller/referencer side of this hop.
    pub source_record_id: &'a str,
    /// Stable record ID of the connecting edge.
    pub edge_record_id: &'a str,
    /// Edge label (`CALLS` / `REFERENCES`).
    pub edge_label: &'static str,
    /// Call resolution status carried by the edge (issues #152/#134), when
    /// the edge is inside the resolution contract.
    pub resolution: Option<CallResolution>,
    /// Record ID of the callee/referenced side of this hop.
    pub target_record_id: &'a str,
}

/// One symbol (or file) that can reach the queried target, with its shortest
/// discovered connecting path.
#[derive(Debug, Clone)]
pub struct TransitiveCallerRow<'a> {
    /// The reachable node record.
    pub record: &'a GraphRecord,
    /// Shortest hop distance from the queried target (>= 1).
    pub hop: usize,
    /// Ordered connecting chain from this node down to the target: the first
    /// step's source is this node, the last step's target is the queried
    /// symbol, and consecutive steps share their middle record ID.
    pub path: Vec<TransitivePathStep<'a>>,
    /// Weakest call-resolution status along the path (`unresolved` >
    /// `ambiguous` > `resolved`), or `None` when no step on the path carries
    /// the resolution contract (e.g. a pure `REFERENCES` chain).
    pub path_resolution: Option<CallResolution>,
}

/// Count of reachable-but-dropped frontier nodes at one depth beyond the
/// `--max-depth` bound.
#[derive(Debug, Clone, Copy)]
pub struct TransitiveDroppedDepth {
    /// Depth (hop distance) at which these nodes would have been discovered.
    pub depth: usize,
    /// Number of distinct nodes first reachable at that depth.
    pub count: usize,
}

/// Truncation diagnostic emitted when reachable nodes exist beyond the depth
/// bound: nothing is silently omitted, the dropped frontier is counted per
/// depth (AC4).
#[derive(Debug, Clone)]
pub struct TransitiveTruncation {
    /// The bound in effect.
    pub max_depth: usize,
    /// Dropped frontier counts per depth beyond the bound, ascending.
    pub dropped_frontier: Vec<TransitiveDroppedDepth>,
    /// Total dropped nodes across all depths beyond the bound.
    pub dropped_total: usize,
}

/// Structured transitive-callers result returned by [`transitive_callers`].
#[derive(Debug)]
pub struct TransitiveCallersContext<'a> {
    /// The resolved anchor (queried symbol) record.
    pub anchor: &'a GraphRecord,
    /// Reachable rows, canonically ordered by `(hop, record_id)` ascending.
    pub rows: Vec<TransitiveCallerRow<'a>>,
    /// Depth-bound truncation diagnostic, when reachable nodes were dropped.
    pub truncation: Option<TransitiveTruncation>,
    /// Stable machine-readable diagnostics (dangling edge sources).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// The depth bound used for the walk.
    pub max_depth: usize,
}

/// First resolved anchor whose node kind is **not** a code `Symbol`, if any.
///
/// `transitive-callers` accepts only symbol handles: a canonical codegraph ID
/// resolving to a `Module`, `Import`, `Commit`, `Change`, or other node kind
/// maps to [`FailureTargetKind::Symbol`] during handle resolution and must be
/// rejected rather than walked as an empty symbol result.
#[must_use]
pub fn transitive_callers_non_symbol_anchor_kind(
    records: &[GraphRecord],
    target: &ResolvedFailureTarget,
) -> Option<NodeKind> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    target
        .anchor_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .filter_map(record_node_kind)
        .find(|kind| !matches!(kind, NodeKind::Symbol))
}

/// Walks the transitive inbound `CALLS`/`REFERENCES` closure of one resolved
/// symbol, bounded by `max_depth` hops (issue #139).
///
/// The walk is a level-synchronized BFS over inbound edges, so every reported
/// node carries its **shortest** hop distance and one concrete shortest
/// connecting path chosen deterministically (minimum `(parent record ID,
/// edge record ID)` at the discovering depth). A visited set guarantees each
/// node is reported at most once and that cycles (mutual recursion)
/// terminate. The anchor itself is never reported as its own caller.
///
/// When reachable nodes exist beyond `max_depth` the walk keeps counting
/// (without materializing rows or paths) and reports the dropped frontier per
/// depth in [`TransitiveCallersContext::truncation`] rather than silently
/// omitting them.
///
/// Rows are reachability leads: a path existing in the graph is never proof
/// that a change breaks the caller. Returns `None` when `anchor_id` names no
/// live node in `records`.
///
/// # Panics
///
/// Panics only on violated internal invariants: every discovered node is a
/// live record with a parent pointer chaining back to the anchor by
/// construction of the BFS.
#[must_use]
pub fn transitive_callers<'a>(
    records: &'a [GraphRecord],
    anchor_id: &str,
    max_depth: usize,
) -> Option<TransitiveCallersContext<'a>> {
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

    let anchor = by_id.get(anchor_id).copied()?;
    let anchor_id: &str = anchor.id();

    // ── inbound edge index: target -> [(source, edge, label, resolution)] ─────
    let mut inbound: BTreeMap<&str, Vec<TransitiveEdgeRef<'a>>> = BTreeMap::new();
    for (index, r) in records.iter().enumerate() {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            resolution,
            ..
        } = r
        {
            // Latest-write-wins for edge metadata (issue #421): over an
            // append-only `--graph` a stable edge ID may be re-ingested with a
            // changed label/resolution. Only the latest EDGE write for the id is
            // live, mirroring embedded `latest_edge_versions`; keying off the
            // edge-only map means a later Node write sharing the edge's ID
            // (issue #391) cannot suppress it. Without this a stale earlier
            // version (e.g. `unresolved` before a `resolved` re-ingest) could be
            // admitted, diverging from the coalesced `--data-dir` read.
            if !liveness.is_latest_edge_version(id.as_str(), index) {
                continue;
            }
            if deleted(id.as_str()) || !TRANSITIVE_CALLER_LABELS.contains(label) {
                continue;
            }
            inbound.entry(target.as_str()).or_default().push((
                source.as_str(),
                id.as_str(),
                label.as_str(),
                *resolution,
            ));
        }
    }
    // Deterministic edge visit order; drop exact duplicates from history views
    // where the same stable edge ID recurs across commit snapshots.
    for edges in inbound.values_mut() {
        edges.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        edges.dedup_by_key(|e| (e.0, e.1));
    }

    // ── level-synchronized BFS from the anchor ────────────────────────────────
    // parent[node] = (edge_id, parent_id, edge_label, resolution): the
    // deterministic shortest-path discovery pointer toward the anchor.
    let mut parent: BTreeMap<&str, TransitiveEdgeRef<'a>> = BTreeMap::new();
    let mut hop_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    visited.insert(anchor_id);
    let mut frontier: Vec<&str> = vec![anchor_id];
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    // Discovers the next BFS level from `frontier`. For every newly reachable
    // node the minimum `(parent_id, edge_id)` discovery is kept so the
    // reported path is deterministic. Dangling edge sources produce a
    // diagnostic instead of a row when `report` is set (inside the bound).
    let discover_level = |frontier: &[&'a str],
                          visited: &BTreeSet<&str>,
                          diagnostics: &mut Vec<MemoryAuditDiagnostic>,
                          report: bool|
     -> BTreeMap<&'a str, TransitiveEdgeRef<'a>> {
        let mut discoveries: BTreeMap<&str, TransitiveEdgeRef<'_>> = BTreeMap::new();
        for &node in frontier {
            #[allow(clippy::map_unwrap_or)]
            for &(source_id, edge_id, label, resolution) in
                inbound.get(node).map(Vec::as_slice).unwrap_or(&[])
            {
                if visited.contains(source_id) {
                    continue;
                }
                let Some(&source_record) = by_id.get(source_id) else {
                    if report && !deleted(source_id) {
                        diagnostics.push(MemoryAuditDiagnostic {
                            code: "unresolved_edge_source".to_owned(),
                            source_record_id: edge_id.to_owned(),
                            target_handle: source_id.to_owned(),
                            relation: label.to_owned(),
                            target_domain: "codegraph".to_owned(),
                        });
                    }
                    continue;
                };
                let candidate = (source_record.id(), (edge_id, node, label, resolution));
                match discoveries.entry(candidate.0) {
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(candidate.1);
                    }
                    std::collections::btree_map::Entry::Occupied(mut e) => {
                        // Keep the minimum (parent_id, edge_id) discovery.
                        let (prev_edge, prev_parent, ..) = *e.get();
                        if (candidate.1.1, candidate.1.0) < (prev_parent, prev_edge) {
                            e.insert(candidate.1);
                        }
                    }
                }
            }
        }
        discoveries
    };

    let mut depth = 0usize;
    while depth < max_depth {
        if frontier.is_empty() {
            break;
        }
        depth += 1;
        let discoveries = discover_level(&frontier, &visited, &mut diagnostics, true);
        frontier = discoveries.keys().copied().collect();
        for (node, discovery) in discoveries {
            visited.insert(node);
            hop_of.insert(node, depth);
            parent.insert(node, discovery);
        }
    }

    // ── dropped-frontier counting beyond the bound (AC4) ─────────────────────
    let mut truncation: Option<TransitiveTruncation> = None;
    if !frontier.is_empty() {
        let mut dropped_frontier: Vec<TransitiveDroppedDepth> = Vec::new();
        let mut dropped_total = 0usize;
        let mut count_depth = depth;
        loop {
            let discoveries = discover_level(&frontier, &visited, &mut diagnostics, false);
            if discoveries.is_empty() {
                break;
            }
            count_depth += 1;
            dropped_frontier.push(TransitiveDroppedDepth {
                depth: count_depth,
                count: discoveries.len(),
            });
            dropped_total += discoveries.len();
            frontier = discoveries.keys().copied().collect();
            for node in frontier.iter().copied() {
                visited.insert(node);
            }
        }
        if dropped_total > 0 {
            truncation = Some(TransitiveTruncation {
                max_depth,
                dropped_frontier,
                dropped_total,
            });
        }
    }

    // ── path materialization: shortest chain from each row to the anchor ─────
    let mut rows: Vec<TransitiveCallerRow<'a>> = Vec::with_capacity(hop_of.len());
    for (&node, &hop) in &hop_of {
        let record = by_id
            .get(node)
            .copied()
            .expect("discovered nodes are live records");
        let mut path: Vec<TransitivePathStep<'a>> = Vec::with_capacity(hop);
        let mut cursor = node;
        while cursor != anchor_id {
            let &(edge_id, parent_id, label, resolution) = parent
                .get(cursor)
                .expect("every discovered node has a parent pointer");
            path.push(TransitivePathStep {
                source_record_id: cursor,
                edge_record_id: edge_id,
                edge_label: label,
                resolution,
                target_record_id: parent_id,
            });
            cursor = parent_id;
        }
        // Weakest resolution wins: CallResolution orders resolved < ambiguous
        // < unresolved, so the maximum present status is the weakest link.
        let path_resolution = path.iter().filter_map(|s| s.resolution).max();
        rows.push(TransitiveCallerRow {
            record,
            hop,
            path,
            path_resolution,
        });
    }
    rows.sort_by(|a, b| {
        a.hop
            .cmp(&b.hop)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });

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

    Some(TransitiveCallersContext {
        anchor,
        rows,
        truncation,
        diagnostics,
        max_depth,
    })
}

#[cfg(test)]
mod liveness_parity_tests {
    //! Transport-parity regression (issue #421): over an append-only `--graph`, a
    //! node re-ingested AFTER its own tombstone is live again — matching the
    //! embedded `--data-dir` current-state read — while a tombstone with no later
    //! re-add still deletes its id.
    use super::*;
    use crate::ir::SourceSpan;

    fn sym(id: &str) -> GraphRecord {
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
            Some("foo".to_owned()),
            "symbol foo".to_owned(),
        )
    }

    fn sym_named(id: &str, name: &str) -> GraphRecord {
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

    fn calls(source: &str, target: &str, resolution: CallResolution) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "calls".to_owned(),
        )
        .with_resolution(resolution)
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

    #[test]
    fn anchor_reingested_after_tombstone_resolves_live() {
        // Append order: node, Tombstone(node), node again → latest write wins, so
        // `--graph` agrees with the coalesced embedded current-state read.
        let records = vec![
            sym("codegraph:v5:a"),
            tomb("codegraph:v5:a"),
            sym("codegraph:v5:a"),
        ];
        assert!(
            transitive_callers(&records, "codegraph:v5:a", 5).is_some(),
            "re-ingested anchor after its tombstone must resolve live"
        );
    }

    #[test]
    fn anchor_tombstone_without_reingest_stays_deleted() {
        let records = vec![sym("codegraph:v5:a"), tomb("codegraph:v5:a")];
        assert!(
            transitive_callers(&records, "codegraph:v5:a", 5).is_none(),
            "a tombstone with no later re-ingest still deletes the anchor"
        );
    }

    #[test]
    fn latest_edge_version_supplies_resolution() {
        // Two versions of one stable inbound CALLS edge id (b → a): v1
        // `unresolved`, v2 `resolved`. Only the latest EDGE write (v2) supplies
        // the adjacency, so the caller row's weakest path resolution is
        // `resolved` — matching the coalesced `--data-dir` read. With a stale
        // earlier version kept, the row would report `unresolved` (the Codex P2
        // divergence).
        let a = sym_named("codegraph:v5:a", "a");
        let b = sym_named("codegraph:v5:b", "b");
        let e1 = calls(
            "codegraph:v5:b",
            "codegraph:v5:a",
            CallResolution::Unresolved,
        );
        let e2 = calls("codegraph:v5:b", "codegraph:v5:a", CallResolution::Resolved);
        let records = vec![a, b, e1, e2];
        let ctx = transitive_callers(&records, "codegraph:v5:a", 5).expect("anchor live");
        let row = ctx
            .rows
            .iter()
            .find(|r| r.record.id() == "codegraph:v5:b")
            .expect("b is a caller of a");
        assert_eq!(
            row.path_resolution,
            Some(CallResolution::Resolved),
            "the latest edge version (resolved) must supply the caller path resolution"
        );
    }

    #[test]
    fn edge_reingested_after_tombstone_is_live_adjacency() {
        // An inbound CALLS edge re-ingested after its own tombstone must
        // resurface the caller, matching the embedded latest-edge-version read.
        let a = sym_named("codegraph:v5:a", "a");
        let b = sym_named("codegraph:v5:b", "b");
        let e1 = calls("codegraph:v5:b", "codegraph:v5:a", CallResolution::Resolved);
        let edge_id = e1.id().to_owned();
        let e2 = calls("codegraph:v5:b", "codegraph:v5:a", CallResolution::Resolved);
        let records = vec![a, b, e1, tomb(&edge_id), e2];
        let ctx = transitive_callers(&records, "codegraph:v5:a", 5).expect("anchor live");
        assert!(
            ctx.rows.iter().any(|r| r.record.id() == "codegraph:v5:b"),
            "an edge re-ingested after its tombstone must resurface the caller"
        );
    }
}
