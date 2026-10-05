//! Map a symbol to the test functions that cover it (issue #126).
//!
//! `covering_tests` walks the inbound `CALLS` closure of one resolved symbol
//! and reports only the reachable nodes the extractor stamped with the
//! test-vs-production role `Test` (issue #238): `#[test]`/`#[bench]`-family
//! functions, members of `#[cfg(test)]` modules, and symbols under `tests/`
//! or `benches/` roots. Non-test callers are excluded from the rows but still
//! traversed — a test can reach the symbol *through* a production helper
//! (`test -> helper -> symbol`), and that test is a transitive covering test.
//!
//! Design notes:
//!
//! - Traversal edges are `CALLS` only. A test *exercises* a symbol when a call
//!   path exists from the test to it; `REFERENCES` (imports, name mentions)
//!   is deliberately excluded so a test that merely imports the symbol is
//!   never reported as covering it. `DEFINES` edges are containment, not
//!   reachability, and are not traversed; every row still carries its
//!   repo-relative file/span handle.
//! - Test identification is deterministic and extractor-authored: the walk
//!   reads the stamped `SymbolRole` and never infers test-ness from names.
//!   Records with no stamped role are never fabricated as tests.
//! - The walk is a level-synchronized BFS bounded by `max_depth` hops, so
//!   every reported test carries its **shortest** hop distance: hop 1 is a
//!   *direct* covering test, hop > 1 is a *transitive* covering test. One
//!   concrete shortest connecting call path is reported per row, chosen
//!   deterministically (minimum `(parent record ID, edge record ID)` at the
//!   discovering depth). Cycles terminate; the anchor never reports itself.
//! - Nodes reachable beyond the bound are counted per depth in
//!   [`TransitiveTruncation`] rather than silently omitted. The truncation
//!   counts every dropped caller (test or production) because it diagnoses
//!   the *walk*, not the row filter.
//! - A symbol with zero covering tests returns `Some` context with empty
//!   `rows` — an explicit empty result, never an error.

use std::collections::{BTreeMap, BTreeSet};

use super::liveness::Liveness;
use super::transitive_callers::{TransitiveDroppedDepth, TransitivePathStep, TransitiveTruncation};
use super::{MemoryAuditDiagnostic, ResolvedFailureTarget, record_node_kind};
use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind, SymbolRole};

/// Edge labels the covering-tests walk traverses inbound: `CALLS` only.
///
/// A test *exercises* a symbol when a call path exists from the test to it.
/// `REFERENCES` (imports, name mentions) is deliberately excluded: a test
/// that merely imports the symbol never covers it. `DEFINES` edges are
/// containment, not reachability, and are not traversed.
const COVERING_TEST_LABELS: &[EdgeLabel] = &[EdgeLabel::Calls];

/// Borrowed 4-tuple used by the walk's indexes: an inbound edge
/// `(source_id, edge_id, label, resolution)` or a shortest-path discovery
/// pointer `(edge_id, parent_id, label, resolution)`.
type CoveringEdgeRef<'a> = (&'a str, &'a str, &'static str, Option<CallResolution>);

/// One test function that can reach the queried symbol, with its hop distance
/// and one concrete shortest connecting call path.
#[derive(Debug, Clone)]
pub struct CoveringTestRow<'a> {
    /// The test symbol record (role `Test`, stamped by the extractor).
    pub record: &'a GraphRecord,
    /// Shortest hop distance from the queried symbol (>= 1).
    pub hop: usize,
    /// Ordered connecting chain from this test down to the target: the first
    /// step's source is this test, the last step's target is the queried
    /// symbol, and consecutive steps share their middle record ID.
    pub path: Vec<TransitivePathStep<'a>>,
    /// Weakest call-resolution status along the path (`unresolved` >
    /// `ambiguous` > `resolved`), or `None` when no step carries the
    /// resolution contract.
    pub path_resolution: Option<CallResolution>,
}

impl CoveringTestRow<'_> {
    /// `true` when the test calls the queried symbol directly (hop 1).
    #[must_use]
    pub const fn is_direct(&self) -> bool {
        self.hop == 1
    }
}

/// Structured covering-tests result returned by [`covering_tests`].
#[derive(Debug)]
pub struct CoveringTestsContext<'a> {
    /// The resolved anchor (queried symbol) record.
    pub anchor: &'a GraphRecord,
    /// Covering test rows, canonically ordered by `(hop, record_id)`
    /// ascending: every direct (hop 1) test before every transitive one.
    pub rows: Vec<CoveringTestRow<'a>>,
    /// Depth-bound truncation diagnostic, when reachable callers were dropped.
    pub truncation: Option<TransitiveTruncation>,
    /// Stable machine-readable diagnostics (dangling edge sources).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// The depth bound used for the walk.
    pub max_depth: usize,
}

impl CoveringTestsContext<'_> {
    /// Number of direct (hop 1) covering tests.
    #[must_use]
    pub fn direct_count(&self) -> usize {
        self.rows.iter().filter(|r| r.is_direct()).count()
    }

    /// Number of transitive (hop > 1) covering tests.
    #[must_use]
    pub fn transitive_count(&self) -> usize {
        self.rows.iter().filter(|r| !r.is_direct()).count()
    }
}

/// `true` when the record is a test function: the extractor stamped the
/// deterministic test-vs-production role `Test` (issue #238). Records with
/// no stamped role are never fabricated as tests.
const fn is_test_symbol(record: &GraphRecord) -> bool {
    matches!(record.role(), Some(SymbolRole::Test))
}

/// First resolved anchor whose node kind is **not** a code `Symbol`, if any.
///
/// `covering-tests` accepts only symbol handles, mirroring
/// `transitive_callers_non_symbol_anchor_kind`.
#[must_use]
pub fn covering_tests_non_symbol_anchor_kind(
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

/// Walks the inbound `CALLS` closure of one resolved symbol, bounded by
/// `max_depth` hops, and reports the reachable nodes stamped as tests
/// (issue #126).
///
/// The walk is a level-synchronized BFS over inbound `CALLS` edges, so every
/// reported test carries its **shortest** hop distance and one concrete
/// shortest connecting path chosen deterministically (minimum `(parent
/// record ID, edge record ID)` at the discovering depth). A visited set
/// guarantees each node is reported at most once and that cycles terminate.
/// Non-test callers are traversed (a test may reach the symbol through a
/// production helper) but never reported. The anchor itself is never
/// reported as its own covering test.
///
/// When reachable callers exist beyond `max_depth` the walk keeps counting
/// (without materializing rows or paths) and reports the dropped frontier
/// per depth in [`CoveringTestsContext::truncation`] rather than silently
/// omitting them. The truncation counts every dropped caller (test or
/// production) because it diagnoses the *walk*, not the row filter.
///
/// Rows are reachability leads: a call path from a test to the symbol is
/// static evidence the test may exercise it — never proof of coverage or
/// that the test will fail. A symbol with zero covering tests yields `Some`
/// context with empty `rows`: an explicit empty result. Returns `None`
/// when `anchor_id` names no live node in `records`.
///
/// # Panics
///
/// Panics only on violated internal invariants: every discovered node is a
/// live record with a parent pointer chaining back to the anchor by
/// construction of the BFS.
#[must_use]
pub fn covering_tests<'a>(
    records: &'a [GraphRecord],
    anchor_id: &str,
    max_depth: usize,
) -> Option<CoveringTestsContext<'a>> {
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

    // ── inbound CALLS edge index: target -> [(source, edge, label, resolution)]
    let mut inbound: BTreeMap<&str, Vec<CoveringEdgeRef<'a>>> = BTreeMap::new();
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
            if !liveness.is_latest_edge_version(id.as_str(), index) {
                continue;
            }
            if deleted(id.as_str()) || !COVERING_TEST_LABELS.contains(label) {
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
    for edges in inbound.values_mut() {
        edges.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        edges.dedup_by_key(|e| (e.0, e.1));
    }

    // ── level-synchronized BFS from the anchor ──────────────────────────────
    let mut parent: BTreeMap<&str, CoveringEdgeRef<'a>> = BTreeMap::new();
    let mut hop_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    visited.insert(anchor_id);
    let mut frontier: Vec<&str> = vec![anchor_id];
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    let discover_level = |frontier: &[&'a str],
                          visited: &BTreeSet<&str>,
                          diagnostics: &mut Vec<MemoryAuditDiagnostic>,
                          report: bool|
     -> BTreeMap<&'a str, CoveringEdgeRef<'a>> {
        let mut discoveries: BTreeMap<&str, CoveringEdgeRef<'_>> = BTreeMap::new();
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

    // ── dropped-frontier counting beyond the bound ──────────────────────────
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

    // ── row materialization: test-role nodes only, shortest path each ──────
    let mut rows: Vec<CoveringTestRow<'a>> = Vec::new();
    for (&node, &hop) in &hop_of {
        let record = by_id
            .get(node)
            .copied()
            .expect("discovered nodes are live records");
        if !is_test_symbol(record) {
            continue;
        }
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
        let path_resolution = path.iter().filter_map(|s| s.resolution).max();
        rows.push(CoveringTestRow {
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

    Some(CoveringTestsContext {
        anchor,
        rows,
        truncation,
        diagnostics,
        max_depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CallResolution, NodeKind, SourceSpan, SymbolRole};

    fn span() -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 40,
            start_line: 3,
            end_line: 9,
            start_column: None,
            end_column: None,
        }
    }

    fn sym(id: &str, name: &str, path: &str, role: SymbolRole) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(span()),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
        .with_role(role)
    }

    fn calls(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "call edge".to_owned(),
        )
        .with_resolution(CallResolution::Resolved)
    }

    fn references(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::References,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "reference edge".to_owned(),
        )
    }

    /// Labeled fixture: `anchor` is called directly by a test, a production
    /// fn, and (through the production fn) a second test in another file.
    fn fixture() -> Vec<GraphRecord> {
        let t = SymbolRole::Test;
        let p = SymbolRole::Production;
        vec![
            sym("id:anchor", "anchor_fn", "src/lib.rs", p),
            // Direct test caller (hop 1).
            sym("id:test_direct", "test_direct", "src/lib.rs", t),
            calls("id:test_direct", "id:anchor"),
            // Production helper in another file, called directly by anchor's
            // callers and calling the anchor.
            sym("id:helper", "helper", "src/util.rs", p),
            calls("id:helper", "id:anchor"),
            // Transitive test caller across files: test -> helper -> anchor.
            sym("id:test_transitive", "test_transitive", "tests/it.rs", t),
            calls("id:test_transitive", "id:helper"),
            // Production direct caller: traversed, never reported.
            sym("id:prod_caller", "prod_caller", "src/lib.rs", p),
            calls("id:prod_caller", "id:anchor"),
            // Unrelated production symbol: no path to the anchor.
            sym("id:unrelated", "unrelated", "src/other.rs", p),
            // A test that merely references the anchor: NOT a covering test.
            sym("id:test_ref", "test_ref", "tests/it.rs", t),
            references("id:test_ref", "id:anchor"),
            // A test with no stamped role: never fabricated as a test.
            GraphRecord::node(
                "id:roleless".to_owned(),
                NodeKind::Symbol,
                Some("tests/it.rs".to_owned()),
                Some(span()),
                Some("test_roleless".to_owned()),
                "symbol test_roleless".to_owned(),
            ),
            calls("id:roleless", "id:anchor"),
        ]
    }

    fn row_ids<'a>(ctx: &'a CoveringTestsContext<'a>) -> Vec<&'a str> {
        ctx.rows.iter().map(|r| r.record.id()).collect()
    }

    #[test]
    fn finds_direct_test_caller() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        let ids = row_ids(&ctx);
        assert!(
            ids.contains(&"id:test_direct"),
            "direct test missing: {ids:?}"
        );
        let row = ctx
            .rows
            .iter()
            .find(|r| r.record.id() == "id:test_direct")
            .unwrap();
        assert_eq!(row.hop, 1);
        assert!(row.is_direct());
        assert_eq!(row.path.len(), 1);
        assert_eq!(row.path[0].source_record_id, "id:test_direct");
        assert_eq!(row.path[0].target_record_id, "id:anchor");
    }

    #[test]
    fn finds_transitive_test_caller_across_files() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        let ids = row_ids(&ctx);
        assert!(
            ids.contains(&"id:test_transitive"),
            "transitive test missing: {ids:?}"
        );
        let row = ctx
            .rows
            .iter()
            .find(|r| r.record.id() == "id:test_transitive")
            .unwrap();
        assert_eq!(row.hop, 2);
        assert!(!row.is_direct());
        let chain: Vec<&str> = row.path.iter().map(|s| s.source_record_id).collect();
        assert_eq!(chain, vec!["id:test_transitive", "id:helper"]);
        assert_eq!(row.path.last().unwrap().target_record_id, "id:anchor");
    }

    #[test]
    fn excludes_non_test_caller_from_rows() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        let ids = row_ids(&ctx);
        assert!(
            !ids.contains(&"id:prod_caller"),
            "production caller must be excluded: {ids:?}"
        );
        assert!(
            !ids.contains(&"id:helper"),
            "production helper must be excluded: {ids:?}"
        );
        assert!(
            !ids.contains(&"id:unrelated"),
            "unreachable symbol must be excluded: {ids:?}"
        );
    }

    #[test]
    fn excludes_reference_only_test() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        assert!(
            !row_ids(&ctx).contains(&"id:test_ref"),
            "a test that merely references the symbol is not a covering test"
        );
    }

    #[test]
    fn never_fabricates_test_role_for_unstamped_record() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        assert!(
            !row_ids(&ctx).contains(&"id:roleless"),
            "a record with no stamped role must never be treated as a test"
        );
    }

    #[test]
    fn exact_covering_set_is_stable() {
        // Labeled expectation: the fixture's covering set is exactly the two
        // tests, no more, no less — the >=95% precision/recall contract.
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        let mut ids = row_ids(&ctx);
        ids.sort_unstable();
        assert_eq!(ids, vec!["id:test_direct", "id:test_transitive"]);
    }

    #[test]
    fn zero_covering_tests_is_empty_not_error() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:unrelated", 5).expect("anchor live");
        assert!(ctx.rows.is_empty(), "no tests cover `unrelated`");
        assert_eq!(ctx.direct_count(), 0);
        assert_eq!(ctx.transitive_count(), 0);
    }

    #[test]
    fn rows_ordered_by_hop_then_record_id() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        let keys: Vec<(usize, &str)> = ctx.rows.iter().map(|r| (r.hop, r.record.id())).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn max_depth_bounds_walk_and_counts_truncation() {
        let records = fixture();
        let ctx = covering_tests(&records, "id:anchor", 1).expect("anchor live");
        // test_transitive (hop 2) is beyond the bound; helper is not a test.
        assert_eq!(row_ids(&ctx), vec!["id:test_direct"]);
        let trunc = ctx.truncation.expect("truncation diagnostic");
        assert_eq!(trunc.max_depth, 1);
        // Dropped frontier at depth 2: helper and prod_caller's... helper is
        // already visited at depth 1 via test_direct? No: helper is visited at
        // depth 1 (helper -> anchor). At depth 2 the discoveries are
        // test_transitive (via helper) — one dropped node.
        assert_eq!(trunc.dropped_total, 1);
        assert_eq!(trunc.dropped_frontier.len(), 1);
        assert_eq!(trunc.dropped_frontier[0].depth, 2);
        assert_eq!(trunc.dropped_frontier[0].count, 1);
    }

    #[test]
    fn non_symbol_anchor_is_rejected() {
        let module = GraphRecord::node(
            "id:mod".to_owned(),
            NodeKind::Module,
            Some("src/lib.rs".to_owned()),
            Some(span()),
            Some("mymod".to_owned()),
            "module mymod".to_owned(),
        );
        let target = ResolvedFailureTarget {
            handle: "mymod".to_owned(),
            kind: crate::query::FailureTargetKind::Symbol,
            anchor_ids: std::iter::once("id:mod".to_owned()).collect(),
            seed_failures: BTreeSet::new(),
            stale: false,
        };
        let kind = covering_tests_non_symbol_anchor_kind(&[module], &target);
        assert_eq!(kind, Some(NodeKind::Module));
    }

    #[test]
    fn symbol_anchor_is_not_rejected() {
        let records = fixture();
        let target = ResolvedFailureTarget {
            handle: "anchor_fn".to_owned(),
            kind: crate::query::FailureTargetKind::Symbol,
            anchor_ids: std::iter::once("id:anchor".to_owned()).collect(),
            seed_failures: BTreeSet::new(),
            stale: false,
        };
        assert_eq!(
            covering_tests_non_symbol_anchor_kind(&records, &target),
            None
        );
    }

    #[test]
    fn tombstoned_test_is_excluded() {
        let mut records = fixture();
        records.push(GraphRecord::Tombstone {
            id: "id:tomb_test_direct".to_owned(),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: "id:test_direct".to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        });
        let ctx = covering_tests(&records, "id:anchor", 5).expect("anchor live");
        assert!(
            !row_ids(&ctx).contains(&"id:test_direct"),
            "a tombstoned test must not be reported"
        );
    }

    #[test]
    fn mutual_recursion_cycle_terminates() {
        let t = SymbolRole::Test;
        let p = SymbolRole::Production;
        let records = vec![
            sym("id:a", "a", "src/lib.rs", p),
            sym("id:x", "x", "src/lib.rs", t),
            sym("id:y", "y", "src/lib.rs", t),
            calls("id:x", "id:a"),
            calls("id:x", "id:y"),
            calls("id:y", "id:x"),
        ];
        let ctx = covering_tests(&records, "id:a", 10).expect("anchor live");
        let mut ids = row_ids(&ctx);
        ids.sort_unstable();
        assert_eq!(ids, vec!["id:x", "id:y"], "each cycle member reported once");
        let y = ctx.rows.iter().find(|r| r.record.id() == "id:y").unwrap();
        assert_eq!(y.hop, 2);
    }
}
