use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::{ResolvedFailureTarget, record_node_kind};
use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind};

// ---------------------------------------------------------------------------
// Transitive outbound callees — `eg query transitive-callees` (issue #253)
// ---------------------------------------------------------------------------

/// Edge labels the transitive-callees walk traverses outbound. This is the
/// `eg query deps` (#123) dependency label set — `CALLS`/`IMPLEMENTS`/
/// `IMPORTS`/`REFERENCES` — so the `--max-depth=1` reachable-plus-unresolved
/// result is exactly the direct outbound dependency set of #123 (AC5). Weak
/// `MENTIONS` and containment/topology labels are excluded.
const TRANSITIVE_CALLEE_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Calls,
    EdgeLabel::Implements,
    EdgeLabel::Imports,
    EdgeLabel::References,
];

/// Borrowed 4-tuple used by the transitive walk's indexes: an outbound edge
/// `(target_id, edge_id, label, resolution)` or a shortest-path discovery
/// pointer `(edge_id, parent_id, label, resolution)`.
type TransitiveEdgeRef<'a> = (&'a str, &'a str, &'static str, Option<CallResolution>);

/// One hop of a concrete connecting dependency path: `source` calls/references
/// `target`, moving one step from the queried symbol toward a reachable node.
#[derive(Debug, Clone, Copy)]
pub struct TransitiveCalleePathStep<'a> {
    /// Record ID of the caller/referencer side of this hop.
    pub source_record_id: &'a str,
    /// Stable record ID of the connecting edge.
    pub edge_record_id: &'a str,
    /// Edge label (`CALLS` / `IMPLEMENTS` / `IMPORTS` / `REFERENCES`).
    pub edge_label: &'static str,
    /// Call resolution status carried by the edge (issues #152/#134), when
    /// the edge is inside the resolution contract.
    pub resolution: Option<CallResolution>,
    /// Record ID of the callee/referenced side of this hop.
    pub target_record_id: &'a str,
}

/// One symbol (or file / import) reachable from the queried symbol through a
/// chain of outbound dependency edges, with its shortest discovered path.
#[derive(Debug, Clone)]
pub struct TransitiveCalleeRow<'a> {
    /// The reachable node record.
    pub record: &'a GraphRecord,
    /// Shortest hop distance from the queried symbol (>= 1).
    pub hop: usize,
    /// Ordered connecting chain from the queried symbol out to this node: the
    /// first step's source is the anchor, the last step's target is this node,
    /// and consecutive steps share their middle record ID.
    pub path: Vec<TransitiveCalleePathStep<'a>>,
    /// Weakest call-resolution status along the path (`unresolved` >
    /// `ambiguous` > `resolved`), or `None` when no step on the path carries
    /// the resolution contract (e.g. a pure `REFERENCES`/`IMPORTS` chain).
    pub path_resolution: Option<CallResolution>,
}

/// Why an outbound edge's target is reported as `unresolved` instead of as a
/// reachable row (mirrors the #123 `deps` category).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UnresolvedCalleeReason {
    /// The call did not resolve to an in-repo definition: the edge targets a
    /// `Diagnostic` marker recording the callee (issues #152/#134) or carries
    /// `resolution: "unresolved"` itself.
    UnresolvedCall,
    /// The edge's target record is not in the graph (dangling target, or a
    /// target that exists only as a tombstone) and the edge carries no
    /// unresolved-call signal of its own.
    MissingTarget,
}

impl UnresolvedCalleeReason {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnresolvedCall => "unresolved_call",
            Self::MissingTarget => "missing_target",
        }
    }
}

/// One outbound edge whose target is not a resolved in-graph node.
///
/// Reported as an explicit `unresolved` category rather than silently dropped,
/// and never counted as reachable (AC4). Unresolved targets discovered from any
/// source node within the depth bound are reported.
#[derive(Debug, Clone, Copy)]
pub struct UnresolvedCalleeRow<'a> {
    /// The `Diagnostic` marker node recording the callee, when present. Its
    /// name is the callee display and its path/span cite the call site.
    pub diagnostic: Option<&'a GraphRecord>,
    /// The raw target record ID carried by the edge.
    pub target_id: &'a str,
    /// Record ID of the reachable source node this edge departs from (the
    /// anchor at hop 1, or a deeper reachable node).
    pub source_id: &'a str,
    /// Hop distance of the source node from the anchor (>= 0; 0 is the anchor).
    pub source_hop: usize,
    /// Stable record ID of the producing edge.
    pub edge_id: &'a str,
    /// Edge label that produced this row.
    pub relation: &'static str,
    /// Call-resolution status carried by the edge, when present.
    pub resolution: Option<CallResolution>,
    /// Why the target is unresolved.
    pub reason: UnresolvedCalleeReason,
}

/// Count of reachable-but-dropped frontier nodes at one depth beyond the
/// `--max-depth` bound.
#[derive(Debug, Clone, Copy)]
pub struct TransitiveCalleeDroppedDepth {
    /// Depth (hop distance) at which these nodes would have been discovered.
    pub depth: usize,
    /// Number of distinct nodes first reachable at that depth.
    pub count: usize,
}

/// Truncation diagnostic emitted when reachable nodes exist beyond the depth
/// bound: nothing is silently omitted, the dropped frontier is counted per
/// depth (AC4).
#[derive(Debug, Clone)]
pub struct TransitiveCalleeTruncation {
    /// The bound in effect.
    pub max_depth: usize,
    /// Dropped frontier counts per depth beyond the bound, ascending.
    pub dropped_frontier: Vec<TransitiveCalleeDroppedDepth>,
    /// Total dropped nodes across all depths beyond the bound.
    pub dropped_total: usize,
}

/// Structured transitive-callees result returned by [`transitive_callees`].
#[derive(Debug)]
pub struct TransitiveCalleesContext<'a> {
    /// The resolved anchor (queried symbol) record.
    pub anchor: &'a GraphRecord,
    /// Reachable rows, canonically ordered by `(hop, record_id)` ascending.
    pub rows: Vec<TransitiveCalleeRow<'a>>,
    /// Unresolved outbound edges discovered within the bound, canonically
    /// ordered by `(relation, target_id, edge_id)` ascending.
    pub unresolved: Vec<UnresolvedCalleeRow<'a>>,
    /// Depth-bound truncation diagnostic, when reachable nodes were dropped.
    pub truncation: Option<TransitiveCalleeTruncation>,
    /// The depth bound used for the walk.
    pub max_depth: usize,
}

/// First resolved anchor whose node kind is **not** a code `Symbol`, if any.
///
/// `transitive-callees` accepts only symbol handles: a canonical codegraph ID
/// resolving to a `Module`, `Import`, `Commit`, `Change`, or other node kind
/// maps to [`FailureTargetKind::Symbol`] during handle resolution and must be
/// rejected rather than walked as an empty symbol result.
#[must_use]
pub fn transitive_callees_non_symbol_anchor_kind(
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

/// Walks the transitive outbound `CALLS`/`IMPLEMENTS`/`IMPORTS`/`REFERENCES`
/// closure of one resolved symbol, bounded by `max_depth` hops (issue #253).
///
/// This is the outbound mirror of [`super::transitive_callers`]: identical
/// level-synchronized BFS, shortest-path, cycle, and truncation machinery,
/// traversing the outbound edge direction over the wider #123 dependency label
/// set. Every reported node carries its **shortest** hop distance and one
/// concrete shortest connecting path chosen deterministically (minimum
/// `(parent record ID, edge record ID)` at the discovering depth). A visited
/// set guarantees each node is reported at most once and that cycles (mutual
/// recursion) terminate. The anchor itself is never reported as its own callee.
///
/// An outbound edge whose target is an unresolved-call `Diagnostic` marker,
/// carries an `unresolved` resolution status, or names a record missing from
/// the graph is reported in the explicit
/// [`TransitiveCalleesContext::unresolved`] category — never silently dropped
/// and never counted as a reachable node (AC4). Unresolved targets discovered
/// at any hop within the bound are reported.
///
/// When reachable nodes exist beyond `max_depth` the walk keeps counting
/// (without materializing rows or paths) and reports the dropped frontier per
/// depth in [`TransitiveCalleesContext::truncation`] rather than silently
/// omitting them.
///
/// Rows are reachability leads: a path existing in the graph is never proof
/// that a change breaks a callee. Returns `None` when `anchor_id` names no
/// live node in `records`.
///
/// # Panics
///
/// Panics only on violated internal invariants: every discovered node is a
/// live record with a parent pointer chaining back to the anchor by
/// construction of the BFS.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn transitive_callees<'a>(
    records: &'a [GraphRecord],
    anchor_id: &str,
    max_depth: usize,
) -> Option<TransitiveCalleesContext<'a>> {
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

    // ── outbound edge index: source -> [(target, edge, label, resolution)] ────
    let mut outbound: BTreeMap<&str, Vec<TransitiveEdgeRef<'a>>> = BTreeMap::new();
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
            if deleted(id.as_str()) || !TRANSITIVE_CALLEE_LABELS.contains(label) {
                continue;
            }
            outbound.entry(source.as_str()).or_default().push((
                target.as_str(),
                id.as_str(),
                label.as_str(),
                *resolution,
            ));
        }
    }
    // Deterministic edge visit order; drop exact duplicates from history views
    // where the same stable edge ID recurs across commit snapshots.
    for edges in outbound.values_mut() {
        edges.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        edges.dedup_by_key(|e| (e.0, e.1));
    }

    // ── level-synchronized BFS from the anchor ────────────────────────────────
    // parent[node] = (edge_id, parent_id, edge_label, resolution): the
    // deterministic shortest-path discovery pointer from the anchor.
    let mut parent: BTreeMap<&str, TransitiveEdgeRef<'a>> = BTreeMap::new();
    let mut hop_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    visited.insert(anchor_id);
    let mut frontier: Vec<&str> = vec![anchor_id];

    // Discovers the next BFS level of *reachable* nodes from `frontier` over
    // outbound edges. For every newly reachable node the minimum
    // `(parent_id, edge_id)` discovery is kept so the reported path is
    // deterministic. Unresolved targets (Diagnostic markers, `unresolved`
    // resolution, or missing records) are not reachable nodes and are handled
    // in a separate pass below (AC4); they never seed the frontier.
    let discover_level = |frontier: &[&'a str],
                          visited: &BTreeSet<&str>|
     -> BTreeMap<&'a str, TransitiveEdgeRef<'a>> {
        let mut discoveries: BTreeMap<&str, TransitiveEdgeRef<'_>> = BTreeMap::new();
        for &node in frontier {
            #[allow(clippy::map_unwrap_or)]
            for &(target_id, edge_id, label, resolution) in
                outbound.get(node).map(Vec::as_slice).unwrap_or(&[])
            {
                // A recursive self-call is not a reachability lead about the
                // symbol; the anchor is never its own callee.
                if target_id == anchor_id {
                    continue;
                }
                let Some(rec) = by_id.get(target_id).copied() else {
                    continue;
                };
                // A Diagnostic marker or `unresolved` edge is an unresolved
                // target, never a reachable node.
                if matches!(record_node_kind(rec), Some(NodeKind::Diagnostic))
                    || resolution == Some(CallResolution::Unresolved)
                {
                    continue;
                }
                if visited.contains(target_id) {
                    continue;
                }
                let candidate = (rec.id(), (edge_id, node, label, resolution));
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
        let discoveries = discover_level(&frontier, &visited);
        frontier = discoveries.keys().copied().collect();
        for (node, discovery) in discoveries {
            visited.insert(node);
            hop_of.insert(node, depth);
            parent.insert(node, discovery);
        }
    }

    // ── explicit unresolved category (AC4) ────────────────────────────────────
    // A source node at hop h examines its outbound edges at depth h+1, so its
    // unresolved targets are inside the bound exactly when h < max_depth. The
    // anchor is hop 0 (its edges are examined at depth 1), matching the #123
    // `deps` set at max_depth=1. Keyed for canonical order / dedup across
    // history snapshots.
    let mut unresolved: BTreeMap<(&'static str, &str, &str), UnresolvedCalleeRow<'a>> =
        BTreeMap::new();
    let sources: Vec<(&str, usize)> = std::iter::once((anchor_id, 0usize))
        .chain(hop_of.iter().map(|(&n, &h)| (n, h)))
        .filter(|&(_, h)| h < max_depth)
        .collect();
    for (node, source_hop) in sources {
        #[allow(clippy::map_unwrap_or)]
        for &(target_id, edge_id, label, resolution) in
            outbound.get(node).map(Vec::as_slice).unwrap_or(&[])
        {
            if target_id == anchor_id {
                continue;
            }
            match by_id.get(target_id).copied() {
                Some(rec)
                    if matches!(record_node_kind(rec), Some(NodeKind::Diagnostic))
                        || resolution == Some(CallResolution::Unresolved) =>
                {
                    unresolved
                        .entry((label, target_id, edge_id))
                        .or_insert(UnresolvedCalleeRow {
                            diagnostic: Some(rec),
                            target_id,
                            source_id: node,
                            source_hop,
                            edge_id,
                            relation: label,
                            resolution,
                            reason: UnresolvedCalleeReason::UnresolvedCall,
                        });
                }
                Some(_) => {}
                None => {
                    // An edge already carrying `resolution: "unresolved"`
                    // identifies an unresolved call by itself; the marker record
                    // being absent does not change why the target is unresolved.
                    // `missing_target` is reserved for edges without that signal.
                    let reason = if resolution == Some(CallResolution::Unresolved) {
                        UnresolvedCalleeReason::UnresolvedCall
                    } else {
                        UnresolvedCalleeReason::MissingTarget
                    };
                    unresolved
                        .entry((label, target_id, edge_id))
                        .or_insert(UnresolvedCalleeRow {
                            diagnostic: None,
                            target_id,
                            source_id: node,
                            source_hop,
                            edge_id,
                            relation: label,
                            resolution,
                            reason,
                        });
                }
            }
        }
    }

    // ── dropped-frontier counting beyond the bound (AC4) ─────────────────────
    let mut truncation: Option<TransitiveCalleeTruncation> = None;
    if !frontier.is_empty() {
        let mut dropped_frontier: Vec<TransitiveCalleeDroppedDepth> = Vec::new();
        let mut dropped_total = 0usize;
        let mut count_depth = depth;
        loop {
            let discoveries = discover_level(&frontier, &visited);
            if discoveries.is_empty() {
                break;
            }
            count_depth += 1;
            dropped_frontier.push(TransitiveCalleeDroppedDepth {
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
            truncation = Some(TransitiveCalleeTruncation {
                max_depth,
                dropped_frontier,
                dropped_total,
            });
        }
    }

    // ── path materialization: shortest chain from the anchor to each row ─────
    let mut rows: Vec<TransitiveCalleeRow<'a>> = Vec::with_capacity(hop_of.len());
    for (&node, &hop) in &hop_of {
        let record = by_id
            .get(node)
            .copied()
            .expect("discovered nodes are live records");
        // Build the reversed chain (node -> ... -> anchor) then flip so the
        // path runs anchor -> ... -> node (outbound reading order).
        let mut rev: Vec<TransitiveCalleePathStep<'a>> = Vec::with_capacity(hop);
        let mut cursor = node;
        while cursor != anchor_id {
            let &(edge_id, parent_id, label, resolution) = parent
                .get(cursor)
                .expect("every discovered node has a parent pointer");
            rev.push(TransitiveCalleePathStep {
                source_record_id: parent_id,
                edge_record_id: edge_id,
                edge_label: label,
                resolution,
                target_record_id: cursor,
            });
            cursor = parent_id;
        }
        rev.reverse();
        let path = rev;
        // Weakest resolution wins: CallResolution orders resolved < ambiguous
        // < unresolved, so the maximum present status is the weakest link.
        let path_resolution = path.iter().filter_map(|s| s.resolution).max();
        rows.push(TransitiveCalleeRow {
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

    Some(TransitiveCalleesContext {
        anchor,
        rows,
        unresolved: unresolved.into_values().collect(),
        truncation,
        max_depth,
    })
}

#[cfg(test)]
mod liveness_parity_tests {
    //! Transport-parity regression (issue #421): a node re-ingested AFTER its own
    //! tombstone is live again over `--graph`, matching the embedded current-state
    //! read; a tombstone with no later re-add still deletes its id.
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
        let records = vec![
            sym("codegraph:v5:a"),
            tomb("codegraph:v5:a"),
            sym("codegraph:v5:a"),
        ];
        assert!(
            transitive_callees(&records, "codegraph:v5:a", 5).is_some(),
            "re-ingested anchor after its tombstone must resolve live"
        );
    }

    #[test]
    fn anchor_tombstone_without_reingest_stays_deleted() {
        let records = vec![sym("codegraph:v5:a"), tomb("codegraph:v5:a")];
        assert!(
            transitive_callees(&records, "codegraph:v5:a", 5).is_none(),
            "a tombstone with no later re-ingest still deletes the anchor"
        );
    }

    #[test]
    fn latest_edge_version_supplies_resolution() {
        // Two versions of one stable outbound CALLS edge id: v1 `unresolved`,
        // v2 `resolved`. Only the latest EDGE write (v2) supplies the adjacency,
        // so the callee is reachable — matching the coalesced `--data-dir` read.
        // With a stale earlier version kept, the callee would be shunted to the
        // `unresolved` bucket instead (the Codex P2 divergence).
        let a = sym_named("codegraph:v5:a", "a");
        let b = sym_named("codegraph:v5:b", "b");
        let e1 = calls(
            "codegraph:v5:a",
            "codegraph:v5:b",
            CallResolution::Unresolved,
        );
        let e2 = calls("codegraph:v5:a", "codegraph:v5:b", CallResolution::Resolved);
        let records = vec![a, b, e1, e2];
        let ctx = transitive_callees(&records, "codegraph:v5:a", 5).expect("anchor live");
        assert!(
            ctx.rows.iter().any(|r| r.record.id() == "codegraph:v5:b"),
            "the latest edge version (resolved) makes the target a reachable callee"
        );
        assert!(
            ctx.unresolved
                .iter()
                .all(|u| u.target_id != "codegraph:v5:b"),
            "a superseded earlier edge version must not also report the target unresolved"
        );
    }

    #[test]
    fn edge_reingested_after_tombstone_is_live_adjacency() {
        // A CALLS edge re-ingested after its own tombstone must resurface the
        // callee, matching the embedded latest-edge-version read.
        let a = sym_named("codegraph:v5:a", "a");
        let b = sym_named("codegraph:v5:b", "b");
        let e1 = calls("codegraph:v5:a", "codegraph:v5:b", CallResolution::Resolved);
        let edge_id = e1.id().to_owned();
        let e2 = calls("codegraph:v5:a", "codegraph:v5:b", CallResolution::Resolved);
        let records = vec![a, b, e1, tomb(&edge_id), e2];
        let ctx = transitive_callees(&records, "codegraph:v5:a", 5).expect("anchor live");
        assert!(
            ctx.rows.iter().any(|r| r.record.id() == "codegraph:v5:b"),
            "an edge re-ingested after its tombstone must resurface the callee"
        );
    }
}
