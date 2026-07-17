use std::collections::BTreeMap;

use super::liveness::Liveness;
use super::{ResolvedFailureTarget, record_node_kind};
use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind};

// ---------------------------------------------------------------------------
// Direct outbound dependencies — `eg query deps` (issue #123)
// ---------------------------------------------------------------------------

/// Edge labels the outbound-dependency query traverses. Weak `MENTIONS` and
/// containment/topology labels are excluded so a row is always a typed
/// dependency relation the extractor asserted for the symbol itself.
const SYMBOL_DEPENDENCY_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Calls,
    EdgeLabel::Implements,
    EdgeLabel::Imports,
    EdgeLabel::References,
];

/// One resolved direct outbound dependency of the queried symbol.
#[derive(Debug, Clone, Copy)]
pub struct SymbolDependencyRow<'a> {
    /// The dependency's live node record (Symbol, Import, Module, …).
    pub record: &'a GraphRecord,
    /// Stable record ID of the producing edge.
    pub edge_id: &'a str,
    /// Edge label that produced this dependency (`CALLS` / `IMPLEMENTS` /
    /// `IMPORTS` / `REFERENCES`).
    pub relation: &'static str,
    /// Call-resolution status carried by the edge (issues #152/#134), when
    /// the edge is inside the resolution contract.
    pub resolution: Option<CallResolution>,
}

/// Why an outbound edge's target is reported as `unresolved` instead of as a
/// resolved dependency row.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UnresolvedDependencyReason {
    /// The call did not resolve to an in-repo definition: the edge targets a
    /// `Diagnostic` marker recording the callee (issues #152/#134) or carries
    /// `resolution: "unresolved"` itself.
    UnresolvedCall,
    /// The edge's target record is not in the graph (dangling target, or a
    /// target that exists only as a tombstone) and the edge carries no
    /// unresolved-call signal of its own.
    MissingTarget,
}

impl UnresolvedDependencyReason {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnresolvedCall => "unresolved_call",
            Self::MissingTarget => "missing_target",
        }
    }
}

/// One outbound edge whose target is not a resolved in-graph dependency,
/// reported as an explicit `unresolved` category rather than silently dropped.
#[derive(Debug, Clone, Copy)]
pub struct UnresolvedDependencyRow<'a> {
    /// The `Diagnostic` marker node recording the callee, when present. Its
    /// name is the callee display and its path/span cite the call site.
    pub diagnostic: Option<&'a GraphRecord>,
    /// The raw target record ID carried by the edge.
    pub target_id: &'a str,
    /// Stable record ID of the producing edge.
    pub edge_id: &'a str,
    /// Edge label that produced this row.
    pub relation: &'static str,
    /// Call-resolution status carried by the edge, when present.
    pub resolution: Option<CallResolution>,
    /// Why the target is unresolved.
    pub reason: UnresolvedDependencyReason,
}

/// Structured outbound-dependency result returned by [`symbol_dependencies`].
#[derive(Debug)]
pub struct SymbolDependenciesContext<'a> {
    /// The resolved anchor (queried symbol) record.
    pub anchor: &'a GraphRecord,
    /// Resolved direct dependencies, canonically ordered by
    /// `(relation, record_id, edge_id)` ascending.
    pub dependencies: Vec<SymbolDependencyRow<'a>>,
    /// Unresolved outbound edges, canonically ordered by
    /// `(relation, target_id, edge_id)` ascending.
    pub unresolved: Vec<UnresolvedDependencyRow<'a>>,
}

/// First resolved anchor whose node kind is **not** a code `Symbol`, if any.
///
/// `deps` accepts only symbol handles: a canonical codegraph ID resolving to a
/// `Module`, `Import`, `Commit`, `Change`, or other node kind maps to
/// [`FailureTargetKind::Symbol`] during handle resolution and must be rejected
/// rather than traversed as an empty symbol result.
#[must_use]
pub fn symbol_dependencies_non_symbol_anchor_kind(
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

/// Collects the direct outbound `CALLS`/`IMPLEMENTS`/`IMPORTS`/`REFERENCES`
/// dependencies of one resolved symbol (issue #123).
///
/// Every returned dependency is a live in-graph node reached by exactly one
/// outbound edge from the anchor; the anchor itself (a recursive self-call)
/// is never reported as its own dependency. An outbound edge whose target is
/// an unresolved-call `Diagnostic` marker, carries an `unresolved` resolution
/// status, or names a record missing from the graph is reported in the
/// explicit [`SymbolDependenciesContext::unresolved`] category — never
/// silently dropped (AC3).
///
/// Both result vectors are canonically ordered and duplicate discoveries from
/// history views (the same stable edge ID recurring across commit snapshots)
/// are collapsed, so output over an unchanged store is byte-identical across
/// runs. Rows are dependency leads from parse-derived edges, never proof of
/// runtime behavior. Returns `None` when `anchor_id` names no live node in
/// `records`.
#[must_use]
pub fn symbol_dependencies<'a>(
    records: &'a [GraphRecord],
    anchor_id: &str,
) -> Option<SymbolDependenciesContext<'a>> {
    // ── latest-write-wins tombstone / temporal liveness (issue #421) ──────────
    // Over an append-only `--graph`, a node re-ingested AFTER its own tombstone
    // is live again; the shared gate reports a tombstone active only when it is
    // the id's most recent write, matching the embedded current-state read so
    // `--graph` and `--data-dir` agree. This lane also reads edge
    // labels/resolution for adjacency, so it selects only the latest EDGE write
    // per edge id (`is_latest_edge_version`) below. See `super::liveness`.
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

    // Keyed maps collapse duplicate (target, edge) discoveries from history
    // views where the same stable edge ID recurs across commit snapshots, and
    // give the canonical row order for free.
    let mut dependencies: BTreeMap<(&'static str, &str, &str), SymbolDependencyRow<'a>> =
        BTreeMap::new();
    let mut unresolved: BTreeMap<(&'static str, &str, &str), UnresolvedDependencyRow<'a>> =
        BTreeMap::new();

    for (index, r) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id: edge_id,
            label,
            source,
            target,
            resolution,
            ..
        } = r
        else {
            continue;
        };
        // Latest-write-wins for edge metadata: over an append-only `--graph` a
        // stable edge ID may be re-ingested with changed label/resolution. Only
        // the latest EDGE write for the id is live, mirroring embedded
        // `latest_edge_versions`; keying off the edge-only map means a later Node
        // write sharing the edge's ID (issue #391) cannot suppress it.
        if !liveness.is_latest_edge_version(edge_id.as_str(), index) {
            continue;
        }
        if source != anchor_id
            || deleted(edge_id.as_str())
            || !SYMBOL_DEPENDENCY_LABELS.contains(label)
        {
            continue;
        }
        // A recursive self-call is not a reading-list lead about the symbol.
        if target == anchor_id {
            continue;
        }
        let relation = label.as_str();
        match by_id.get(target.as_str()).copied() {
            Some(node)
                if matches!(record_node_kind(node), Some(NodeKind::Diagnostic))
                    || *resolution == Some(CallResolution::Unresolved) =>
            {
                unresolved
                    .entry((relation, target.as_str(), edge_id.as_str()))
                    .or_insert(UnresolvedDependencyRow {
                        diagnostic: Some(node),
                        target_id: target.as_str(),
                        edge_id: edge_id.as_str(),
                        relation,
                        resolution: *resolution,
                        reason: UnresolvedDependencyReason::UnresolvedCall,
                    });
            }
            Some(node) => {
                dependencies
                    .entry((relation, node.id(), edge_id.as_str()))
                    .or_insert(SymbolDependencyRow {
                        record: node,
                        edge_id: edge_id.as_str(),
                        relation,
                        resolution: *resolution,
                    });
            }
            None => {
                // An edge already carrying `resolution: "unresolved"`
                // identifies an unresolved call by itself; the marker record
                // being absent does not change why the target is unresolved.
                // `missing_target` is reserved for edges without that signal.
                let reason = if *resolution == Some(CallResolution::Unresolved) {
                    UnresolvedDependencyReason::UnresolvedCall
                } else {
                    UnresolvedDependencyReason::MissingTarget
                };
                unresolved
                    .entry((relation, target.as_str(), edge_id.as_str()))
                    .or_insert(UnresolvedDependencyRow {
                        diagnostic: None,
                        target_id: target.as_str(),
                        edge_id: edge_id.as_str(),
                        relation,
                        resolution: *resolution,
                        reason,
                    });
            }
        }
    }

    Some(SymbolDependenciesContext {
        anchor,
        dependencies: dependencies.into_values().collect(),
        unresolved: unresolved.into_values().collect(),
    })
}

#[cfg(test)]
mod liveness_parity_tests {
    //! Transport-parity regression (issue #421): over an append-only `--graph`, a
    //! node OR edge re-ingested AFTER its own tombstone is live again — matching
    //! the embedded `--data-dir` current-state read — and only the latest EDGE
    //! write for a stable edge id supplies the adjacency metadata. A tombstone
    //! with no later re-add still deletes its id.
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
            sym("codegraph:v5:a", "a"),
            tomb("codegraph:v5:a"),
            sym("codegraph:v5:a", "a"),
        ];
        assert!(
            symbol_dependencies(&records, "codegraph:v5:a").is_some(),
            "re-ingested anchor after its tombstone must resolve live"
        );
    }

    #[test]
    fn anchor_tombstone_without_reingest_stays_deleted() {
        let records = vec![sym("codegraph:v5:a", "a"), tomb("codegraph:v5:a")];
        assert!(
            symbol_dependencies(&records, "codegraph:v5:a").is_none(),
            "a tombstone with no later re-ingest still deletes the anchor"
        );
    }

    #[test]
    fn edge_reingested_after_tombstone_is_live_adjacency() {
        // A CALLS edge re-ingested after its own tombstone must resurface the
        // dependency, matching the embedded latest-edge-version read.
        let a = sym("codegraph:v5:a", "a");
        let b = sym("codegraph:v5:b", "b");
        let e1 = calls("codegraph:v5:a", "codegraph:v5:b", CallResolution::Resolved);
        let edge_id = e1.id().to_owned();
        let e2 = calls("codegraph:v5:a", "codegraph:v5:b", CallResolution::Resolved);
        let records = vec![a, b, e1, tomb(&edge_id), e2];
        let ctx = symbol_dependencies(&records, "codegraph:v5:a").expect("anchor live");
        assert!(
            ctx.dependencies
                .iter()
                .any(|d| d.record.id() == "codegraph:v5:b"),
            "an edge re-ingested after its tombstone must resurface the dependency"
        );
    }

    #[test]
    fn latest_edge_version_supplies_resolution() {
        // Two versions of one stable CALLS edge id: v1 `unresolved`, v2 `resolved`.
        // Only the latest EDGE write (v2) supplies the row, so the target is a
        // resolved dependency, never also double-reported under `unresolved`.
        let a = sym("codegraph:v5:a", "a");
        let b = sym("codegraph:v5:b", "b");
        let e1 = calls(
            "codegraph:v5:a",
            "codegraph:v5:b",
            CallResolution::Unresolved,
        );
        let e2 = calls("codegraph:v5:a", "codegraph:v5:b", CallResolution::Resolved);
        let records = vec![a, b, e1, e2];
        let ctx = symbol_dependencies(&records, "codegraph:v5:a").expect("anchor live");
        assert!(
            ctx.dependencies
                .iter()
                .any(|d| d.record.id() == "codegraph:v5:b"),
            "the latest edge version (resolved) makes the target a dependency"
        );
        assert!(
            ctx.unresolved
                .iter()
                .all(|u| u.target_id != "codegraph:v5:b"),
            "a superseded earlier edge version must not also report the target unresolved"
        );
    }
}
