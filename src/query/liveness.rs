//! Shared latest-write-wins liveness gate for `--graph` query lanes (issue #421).
//!
//! A `--graph` JSONL from `scan`/`ingest`/`export` is an APPEND-ONLY history: a
//! non-temporal node or edge re-ingested AFTER its own tombstone revives that id.
//! Embedded `EmbeddedAletheiaSink::read_all_records` (src/adapters/aletheiadb.rs)
//! already collapses to this current state — it drops a tombstone once a later
//! node write (higher `NodeId`) or edge write (higher `egregore_seq`) of the same
//! id supersedes it, both monotonic in write order. Several query lanes, however,
//! historically gated liveness with a set-membership test
//! (`tombstoned.contains(id) && !has_temporal.contains(id)`) that treats EVERY
//! tombstone as permanently active, so a node/edge re-added after its tombstone
//! stayed hidden over `--graph` while `--data-dir` returned it live — the two
//! supported transports DIVERGED.
//!
//! [`Liveness`] centralizes the correct, transport-agnostic rule in one place so
//! those lanes agree with the embedded read. Given the append-ordered record
//! slice it precomputes, in one pass:
//!
//! * `last_write` — the greatest `Vec` index of a `Node`/`Edge` write per id
//!   (CROSS-KIND, drives tombstone liveness);
//! * `last_edge_write` — the greatest `Vec` index of an `Edge` write per id
//!   (EDGE-ONLY, drives edge-version selection);
//! * `last_tomb` — the greatest `Vec` index of a `Tombstone` per `deleted_id`;
//! * `has_temporal` — every id carrying a bitemporal/history version a tombstone
//!   can never suppress.
//!
//! [`Liveness::deleted`] reports a tombstone ACTIVE only when it is the id's most
//! recent write (no later `Node`/`Edge` write of that id follows it), with the
//! `has_temporal` exemption preserved — provably equivalent to the embedded
//! per-tombstone staleness check. [`Liveness::is_latest_edge_version`] selects,
//! for a lane that reads edge metadata (label/resolution/basis) for adjacency,
//! only the LATEST write of a given edge id computed over EDGES ONLY, mirroring
//! embedded `latest_edge_versions` (highest `egregore_seq`). The two write maps
//! are kept SEPARATE because this graph model lets a `Node` and an `Edge`
//! legitimately share one stable record ID (issue #391): keying edge-version
//! selection off the cross-kind map would let a later same-id node write suppress
//! every version of the edge, turning a live adjacency into a phantom absence.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::GraphRecord;

/// Precomputed latest-write-wins liveness view over an append-ordered record
/// slice. See the module docs for the invariant and its equivalence to the
/// embedded current-state read.
pub struct Liveness<'a> {
    /// Greatest index of a `Node`/`Edge` write per id (cross-kind).
    last_write: BTreeMap<&'a str, usize>,
    /// Greatest index of an `Edge` write per id (edge-only).
    last_edge_write: BTreeMap<&'a str, usize>,
    /// Greatest index of a `Node` write per id (node-only).
    last_node_write: BTreeMap<&'a str, usize>,
    /// Greatest index of a `Tombstone` per `deleted_id`.
    last_tomb: BTreeMap<&'a str, usize>,
    /// Ids carrying a bitemporal/history version (never suppressed by a tombstone).
    has_temporal: BTreeSet<&'a str>,
}

impl<'a> Liveness<'a> {
    /// Builds the liveness view in one append-order pass over `records`.
    pub fn new(records: &'a [GraphRecord]) -> Self {
        let mut last_write: BTreeMap<&'a str, usize> = BTreeMap::new();
        let mut last_edge_write: BTreeMap<&'a str, usize> = BTreeMap::new();
        let mut last_node_write: BTreeMap<&'a str, usize> = BTreeMap::new();
        let mut last_tomb: BTreeMap<&'a str, usize> = BTreeMap::new();
        let mut has_temporal: BTreeSet<&'a str> = BTreeSet::new();
        for (index, r) in records.iter().enumerate() {
            match r {
                GraphRecord::Node { id, temporal, .. } => {
                    last_write.insert(id.as_str(), index);
                    last_node_write.insert(id.as_str(), index);
                    if temporal.is_some() {
                        has_temporal.insert(id.as_str());
                    }
                }
                GraphRecord::Edge { id, temporal, .. } => {
                    last_write.insert(id.as_str(), index);
                    last_edge_write.insert(id.as_str(), index);
                    if temporal.is_some() {
                        has_temporal.insert(id.as_str());
                    }
                }
                GraphRecord::Tombstone { deleted_id, .. } => {
                    last_tomb.insert(deleted_id.as_str(), index);
                }
            }
        }
        Self {
            last_write,
            last_edge_write,
            last_node_write,
            last_tomb,
            has_temporal,
        }
    }

    /// True when `id`'s most recent write is a tombstone — i.e. a tombstone
    /// exists and no later non-tombstone write of that id follows it (or none
    /// exists at all). An id carrying a bitemporal/history version is exempt and
    /// never reported deleted. A tombstone with NO later re-add still deletes its
    /// id (parity with the embedded read).
    pub fn deleted(&self, id: &str) -> bool {
        !self.has_temporal.contains(id)
            && self
                .last_tomb
                .get(id)
                .is_some_and(|&ti| self.last_write.get(id).is_none_or(|&wi| ti > wi))
    }

    /// True when the record at append index `index` is the LATEST write of edge
    /// `edge_id` computed over EDGES ONLY. A lane that reads edge metadata for
    /// adjacency includes an edge record only when this holds, so a stale earlier
    /// version — or a later same-id NODE write (issue #391) — never shadows the
    /// edge's current metadata (mirrors embedded `latest_edge_versions`).
    pub fn is_latest_edge_version(&self, edge_id: &str, index: usize) -> bool {
        self.last_edge_write.get(edge_id) == Some(&index)
    }

    /// True when the record at append index `index` is the LATEST write of node
    /// `node_id` computed over NODES ONLY — the node-side mirror of
    /// [`Self::is_latest_edge_version`].
    ///
    /// A lane that reads a node's own relationship metadata (`superseded_by`,
    /// `evidence_links`) must select only the latest version: an append-only
    /// `--graph` that rewrites a non-temporal node to REMOVE a
    /// `SUPERSEDES`/`CONTRADICTS` link leaves both physical versions live
    /// (neither is tombstoned), so reading every version would resurrect the
    /// withdrawn relationship while the embedded read — which retains only the
    /// latest node write — would not.
    ///
    /// Computed over NODES ONLY for the same reason the edge map is edge-only
    /// (issue #391): a node and an edge may legitimately share one stable record
    /// ID, and keying off the cross-kind map would let a later same-id EDGE
    /// write suppress the node's current metadata.
    ///
    /// Callers must apply this only to NON-temporal nodes: bitemporal history
    /// versions of one id are distinct legitimate snapshots, not stale rewrites.
    pub fn is_latest_node_version(&self, node_id: &str, index: usize) -> bool {
        self.last_node_write.get(node_id) == Some(&index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{EdgeLabel, NodeKind, SourceSpan, TemporalMetadata};

    fn node(id: &str) -> GraphRecord {
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

    fn edge(id_label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            id_label,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "edge".to_owned(),
        )
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v5:tomb_{deleted_id}"),
            schema_version: 5,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    #[test]
    fn tombstone_with_no_reingest_deletes_id() {
        let records = vec![node("codegraph:v5:a"), tombstone("codegraph:v5:a")];
        let liveness = Liveness::new(&records);
        assert!(liveness.deleted("codegraph:v5:a"));
    }

    #[test]
    fn node_reingested_after_tombstone_is_live() {
        // Append order: node, Tombstone(node), node again → latest write wins.
        let records = vec![
            node("codegraph:v5:a"),
            tombstone("codegraph:v5:a"),
            node("codegraph:v5:a"),
        ];
        let liveness = Liveness::new(&records);
        assert!(!liveness.deleted("codegraph:v5:a"));
    }

    #[test]
    fn temporal_version_exempts_from_deletion() {
        let live = node("codegraph:v5:a").with_temporal(TemporalMetadata {
            git_commit: "abcdef".to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: "2026-01-01T00:00:00Z".to_owned(),
            author_time: None,
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        });
        let records = vec![live, tombstone("codegraph:v5:a")];
        let liveness = Liveness::new(&records);
        assert!(!liveness.deleted("codegraph:v5:a"));
    }

    #[test]
    fn absent_id_is_not_deleted() {
        let records: Vec<GraphRecord> = vec![node("codegraph:v5:a")];
        let liveness = Liveness::new(&records);
        assert!(!liveness.deleted("codegraph:v5:ghost"));
    }

    #[test]
    fn latest_edge_version_is_the_greatest_edge_index() {
        let e0 = edge(EdgeLabel::Calls, "codegraph:v5:a", "codegraph:v5:b");
        let e1 = edge(EdgeLabel::Calls, "codegraph:v5:a", "codegraph:v5:b");
        let edge_id = e0.id().to_owned();
        assert_eq!(edge_id, e1.id());
        let records = vec![e0, e1];
        let liveness = Liveness::new(&records);
        assert!(!liveness.is_latest_edge_version(&edge_id, 0));
        assert!(liveness.is_latest_edge_version(&edge_id, 1));
    }

    #[test]
    fn later_node_sharing_edge_id_does_not_shadow_edge_version() {
        // A Node written AFTER an edge with the same stable id must not win the
        // edge-only selection (issue #391): the edge's own index stays latest.
        let e = edge(EdgeLabel::Calls, "codegraph:v5:a", "codegraph:v5:b");
        let shared = e.id().to_owned();
        let shadow = node(&shared);
        let records = vec![e, shadow];
        let liveness = Liveness::new(&records);
        assert!(liveness.is_latest_edge_version(&shared, 0));
    }
}
