use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{CallResolution, EdgeLabel, GraphRecord};

// ---------------------------------------------------------------------------
// Directed call-path witness — `eg query path <A> <B>` (issue #225)
// ---------------------------------------------------------------------------
//
// SPEC
// ----
// Answer the reachability question between two *named* endpoints: "does `A`
// reach `B` through the call graph, and if so, by what concrete path?" — with a
// single, citable witness path where every hop cites record IDs and
// repo-relative file/span handles.
//
// This lane walks a directed BFS from `A` toward `B` over **resolved `CALLS`
// edges only** — the outbound call direction (`source` calls `target`). An
// edge participates iff it is a `CALLS` edge carrying `resolution == resolved`
// (issues #152/#134). Ambiguous and unresolved `CALLS` edges, `CALLS` edges
// outside the resolution contract (no `resolution` field), and every other
// label (`REFERENCES`, `MENTIONS`, `IMPORTS`, `IMPLEMENTS`, containment
// topology) are excluded. Because ambiguity never silently picks a target, a
// path is only ever asserted over edges the extractor resolved to exactly one
// in-repo definition — so the witness is a call chain the graph is confident
// about, never a fabricated one.
//
// Direction is honored: `path A B` follows outbound `CALLS`, so a one-way edge
// `A -> B` yields a witness for `path A B` and no witness for `path B A`.
//
// TIE-BREAK (deterministic, byte-stable — AC3)
// --------------------------------------------
// Among all minimum-hop directed paths, the witness is selected by a
// level-synchronized BFS from `A`: nodes are discovered level by level, and
// when a node is first reached at its shortest depth by more than one edge, the
// discovery edge with the lexicographically smallest `(source_record_id,
// edge_record_id)` pair is retained as that node's parent pointer. The witness
// is then reconstructed by following parent pointers from `B` back to `A` and
// reversing. This yields exactly one path, byte-identical across repeated runs
// on an unchanged graph.
//
// A path is a reachability LEAD over the extracted call graph — it proves `A`
// names a resolved call chain to `B`, never that control flow reaches `B` at
// runtime; a `no_path` verdict is likewise not proof of non-reachability
// (dynamic dispatch, macro-generated calls, and cross-crate calls are outside
// the extraction contract).
//
// This walker is intentionally private to this module (the BFS is not factored
// into a shared helper), per the issue's single-witness-path slice.

/// One hop of a directed call-path witness: `source` calls `target` over one
/// resolved `CALLS` edge, moving one step from `A` toward `B`.
#[derive(Debug, Clone, Copy)]
pub struct CallPathStep<'a> {
    /// Record ID of the caller side of this hop.
    pub source_record_id: &'a str,
    /// Stable record ID of the connecting `CALLS` edge.
    pub edge_record_id: &'a str,
    /// Edge label — always `CALLS` for this lane.
    pub edge_label: &'static str,
    /// Call-resolution status carried by the edge (always `resolved` here,
    /// since the walk traverses only resolved `CALLS` edges).
    pub resolution: Option<CallResolution>,
    /// Extraction confidence carried by the edge, when present.
    pub confidence: Option<&'a str>,
    /// Record ID of the callee side of this hop.
    pub target_record_id: &'a str,
}

/// A resolved directed call-path result returned by [`call_path`].
#[derive(Debug)]
pub struct CallPathContext<'a> {
    /// The resolved `A` (source) endpoint record.
    pub from: &'a GraphRecord,
    /// The resolved `B` (target) endpoint record.
    pub to: &'a GraphRecord,
    /// Ordered witness chain from `A` out to `B`: the first step's source is
    /// `A`, the last step's target is `B`, and consecutive steps share their
    /// middle record ID. Empty when `A == B` (trivial zero-hop path) **or**
    /// when no directed path exists — the two cases are distinguished by
    /// [`CallPathContext::path_found`].
    pub steps: Vec<CallPathStep<'a>>,
    /// `true` when `A` reaches `B` (including the trivial `A == B` case);
    /// `false` for an explicit `no_path` verdict.
    pub path_found: bool,
}

/// Borrowed outbound edge / discovery pointer:
/// `(other_node_id, edge_id, confidence)`.
type PathEdgeRef<'a> = (&'a str, &'a str, Option<&'a str>);

/// Computes the deterministic shortest directed call-path witness from
/// `from_id` to `to_id` over resolved `CALLS` edges (issue #225).
///
/// Returns `None` when either endpoint names no live node in `records`
/// (tombstoned-and-unsuperseded records are treated as absent). When both
/// endpoints are live, returns a [`CallPathContext`]: `path_found == true` with
/// a concrete `steps` chain when `B` is reachable from `A`, `path_found == true`
/// with an empty chain when `A == B` (trivial zero-hop path), and
/// `path_found == false` with an empty chain when no directed resolved-`CALLS`
/// path exists.
///
/// The walk is a level-synchronized BFS; see the module header for the
/// documented tie-break rule. Cycles terminate (a visited set guarantees each
/// node is expanded at most once).
///
/// # Panics
///
/// Panics only on a violated internal invariant: once `B` is discovered, every
/// node on the reconstructed chain has a parent pointer back to `A` by
/// construction of the BFS.
#[must_use]
pub fn call_path<'a>(
    records: &'a [GraphRecord],
    from_id: &str,
    to_id: &str,
) -> Option<CallPathContext<'a>> {
    // ── tombstone / temporal filtering (mirrors transitive_callees) ───────────
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let has_temporal: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            }
            | GraphRecord::Edge {
                id,
                temporal: Some(_),
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let deleted = |id: &str| tombstoned.contains(id) && !has_temporal.contains(id);
    let by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|r| {
            let id = r.id();
            if deleted(id) { None } else { Some((id, r)) }
        })
        .collect();

    let from = by_id.get(from_id).copied()?;
    let to = by_id.get(to_id).copied()?;
    let from_id: &str = from.id();
    let to_id: &str = to.id();

    // Trivial zero-hop path: A and B are the same symbol.
    if from_id == to_id {
        return Some(CallPathContext {
            from,
            to,
            steps: Vec::new(),
            path_found: true,
        });
    }

    // ── outbound resolved-CALLS index: source -> [(target, edge, confidence)] ──
    let mut outbound: BTreeMap<&str, Vec<PathEdgeRef<'a>>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            id,
            label: EdgeLabel::Calls,
            source,
            target,
            confidence,
            resolution: Some(CallResolution::Resolved),
            ..
        } = r
        {
            if deleted(id.as_str()) {
                continue;
            }
            // A CALLS edge to a target missing from the live graph cannot
            // extend a witness path; skip it. A resolved edge to a live node is
            // the only thing that advances the walk.
            if !by_id.contains_key(target.as_str()) {
                continue;
            }
            outbound.entry(source.as_str()).or_default().push((
                target.as_str(),
                id.as_str(),
                confidence.as_deref(),
            ));
        }
    }
    // Deterministic edge visit order; drop exact duplicates from history views
    // where the same stable edge ID recurs across commit snapshots.
    for edges in outbound.values_mut() {
        edges.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        edges.dedup_by_key(|e| (e.0, e.1));
    }

    // ── level-synchronized BFS from A toward B ────────────────────────────────
    // parent[node] = (parent_id, edge_id, confidence): the deterministic
    // shortest-path discovery pointer from A. See the module header tie-break.
    let mut parent: BTreeMap<&str, PathEdgeRef<'a>> = BTreeMap::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    visited.insert(from_id);
    let mut frontier: Vec<&str> = vec![from_id];
    let mut found = false;

    while !frontier.is_empty() && !found {
        // Discover the next level. Keying by target and retaining the minimum
        // (parent_id, edge_id) discovery makes the parent pointer — and thus
        // the witness — independent of frontier iteration order (AC3).
        let mut discoveries: BTreeMap<&str, PathEdgeRef<'a>> = BTreeMap::new();
        for &node in &frontier {
            #[allow(clippy::map_unwrap_or)]
            for &(target_id, edge_id, confidence) in
                outbound.get(node).map(Vec::as_slice).unwrap_or(&[])
            {
                if visited.contains(target_id) {
                    continue;
                }
                let candidate = (node, edge_id, confidence);
                match discoveries.entry(target_id) {
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(candidate);
                    }
                    std::collections::btree_map::Entry::Occupied(mut e) => {
                        let (prev_parent, prev_edge, _) = *e.get();
                        if (candidate.0, candidate.1) < (prev_parent, prev_edge) {
                            e.insert(candidate);
                        }
                    }
                }
            }
        }
        let mut next: Vec<&str> = Vec::with_capacity(discoveries.len());
        for (node, pointer) in discoveries {
            visited.insert(node);
            parent.insert(node, pointer);
            next.push(node);
        }
        if visited.contains(to_id) {
            found = true;
        }
        frontier = next;
    }

    if !found {
        return Some(CallPathContext {
            from,
            to,
            steps: Vec::new(),
            path_found: false,
        });
    }

    // ── reconstruct the witness chain from B back to A, then reverse ──────────
    let mut rev: Vec<CallPathStep<'a>> = Vec::new();
    let mut cursor = to_id;
    while cursor != from_id {
        let &(parent_id, edge_id, confidence) = parent
            .get(cursor)
            .expect("every node discovered before B has a parent pointer");
        rev.push(CallPathStep {
            source_record_id: parent_id,
            edge_record_id: edge_id,
            edge_label: "CALLS",
            resolution: Some(CallResolution::Resolved),
            confidence,
            target_record_id: cursor,
        });
        cursor = parent_id;
    }
    rev.reverse();

    Some(CallPathContext {
        from,
        to,
        steps: rev,
        path_found: true,
    })
}
