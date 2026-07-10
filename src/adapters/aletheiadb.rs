//! Embedded `AletheiaDB` adapter.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Instant,
};

use chrono::{DateTime, Utc};

#[cfg(feature = "embeddings")]
use crate::embeddings::{EmbeddingVectorKey, EmbeddingVectorMap};
use crate::{
    adapters::{
        AdapterError, AdapterResult, ExpectedRecordState, GraphSink, InspectStoreReport,
        validate_adapter_record_version,
    },
    daemon::StoreLease,
    identity::{is_local_remote_url, repository_id_matches_payload},
    ir::{
        EdgeLabel, EmbeddingModel, EvidenceLink, GraphRecord, IdentitySource, MetricKind, NodeKind,
        Producer, SelectionBasis, SemanticDriftMetadata, SourceSpan, TemporalMetadata,
        UserContextFields,
    },
};
use ::aletheiadb::api::transaction::WriteOps;

#[cfg(feature = "embeddings")]
const SEMANTIC_INITIAL_CANDIDATE_MULTIPLIER: usize = 8;
#[cfg(feature = "embeddings")]
const SEMANTIC_MAX_CANDIDATE_MULTIPLIER: usize = 64;

/// A single result from a semantic similarity search.
#[cfg(feature = "embeddings")]
#[derive(Debug, Clone)]
pub struct SemanticMatch {
    /// Stable codegraph record ID.
    pub record_id: String,
    /// Node kind name (e.g. `File`, `Symbol`, `Observation`), enabling
    /// trust-class separation between deterministic code hits and agent-authored
    /// memory hits at query time (issue #91).
    pub kind: Option<String>,
    /// Human-readable name when available.
    pub name: Option<String>,
    /// Repository-relative path when available.
    pub repo_relative_path: Option<String>,
    /// Cosine similarity score (higher = more similar).
    pub score: f32,
    /// Source span when available.
    pub span: Option<SourceSpan>,
}

/// Graph sink backed by an embedded `AletheiaDB` store.
pub struct EmbeddedAletheiaSink {
    db: ::aletheiadb::AletheiaDB,
    node_lookup: NodeLookupIndex,
    tombstone_ids: BTreeMap<String, ::aletheiadb::NodeId>,
    record_handles: BTreeMap<String, StoredRecord>,
    /// Monotonically increasing sequence counter stamped on every edge and tombstone write.
    /// Enables detecting whether an edge was re-ingested after its tombstone.
    write_seq: u64,
    /// Latest `egregore_seq` stored for each edge `codegraph_id`.
    edge_seqs: BTreeMap<String, u64>,
    /// `egregore_seq` stored on each tombstone node, keyed by `AletheiaDB` `NodeId`.
    tombstone_node_seqs: BTreeMap<::aletheiadb::NodeId, u64>,
    /// Count of physical `AletheiaDB` edges per `codegraph_id` that were written before the
    /// `egregore_seq` system was introduced (i.e., they have no `egregore_seq` property).
    /// Used as a fallback staleness check when both the edge and tombstone lack sequence metadata.
    legacy_edge_counts: BTreeMap<String, usize>,
    _lease: Option<StoreLease>,
    #[cfg(feature = "embeddings")]
    embedding_vectors: EmbeddingVectorMap,
    /// Bounds how many embedded stores run concurrently during in-crate tests
    /// so each store's `GroupCommit` background flush thread stays schedulable.
    /// Held for the store's lifetime; released on drop. Test-only.
    #[cfg(test)]
    _store_gate_permit: embedded_store_gate::StorePermit,
}

#[derive(Debug, Clone, Copy)]
enum StoredRecord {
    Node(::aletheiadb::NodeId),
    Edge(::aletheiadb::EdgeId),
    Tombstone(::aletheiadb::NodeId),
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct TemporalReadKey {
    valid_time: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    git_commit: String,
}

#[derive(Debug, Clone)]
struct ReadBackCandidate<Id> {
    storage_id: Id,
    temporal_key: Option<TemporalReadKey>,
}

#[derive(Debug, Default)]
struct NodeLookupIndex {
    latest: BTreeMap<String, ReadBackCandidate<::aletheiadb::NodeId>>,
    by_commit: BTreeMap<String, BTreeMap<String, ReadBackCandidate<::aletheiadb::NodeId>>>,
    by_observation:
        BTreeMap<String, BTreeMap<TemporalReadKey, ReadBackCandidate<::aletheiadb::NodeId>>>,
    non_temporal: BTreeMap<String, ::aletheiadb::NodeId>,
    single_candidate: BTreeMap<String, ::aletheiadb::NodeId>,
    candidate_counts: BTreeMap<String, usize>,
}

impl NodeLookupIndex {
    fn insert(
        &mut self,
        record_id: String,
        node_id: ::aletheiadb::NodeId,
        temporal_key: Option<TemporalReadKey>,
    ) {
        *self.candidate_counts.entry(record_id.clone()).or_default() += 1;
        self.single_candidate
            .entry(record_id.clone())
            .or_insert(node_id);

        let candidate = ReadBackCandidate {
            storage_id: node_id,
            temporal_key: temporal_key.clone(),
        };
        if should_replace_read_back_candidate(self.latest.get(&record_id), &candidate) {
            self.latest.insert(record_id.clone(), candidate.clone());
        }

        if let Some(key) = temporal_key {
            let observation_candidates = self.by_observation.entry(record_id.clone()).or_default();
            if should_replace_read_back_candidate(observation_candidates.get(&key), &candidate) {
                observation_candidates.insert(key.clone(), candidate.clone());
            }

            let commit_candidates = self.by_commit.entry(record_id).or_default();
            let commit = key.git_commit;
            if should_replace_read_back_candidate(commit_candidates.get(&commit), &candidate) {
                commit_candidates.insert(commit, candidate);
            }
        } else {
            self.non_temporal
                .entry(record_id)
                .and_modify(|current| {
                    if node_id > *current {
                        *current = node_id;
                    }
                })
                .or_insert(node_id);
        }
    }

    fn latest_node(&self, record_id: &str) -> Option<::aletheiadb::NodeId> {
        self.latest
            .get(record_id)
            .map(|candidate| candidate.storage_id)
    }

    fn node_for_commit(&self, record_id: &str, git_commit: &str) -> Option<::aletheiadb::NodeId> {
        self.by_commit
            .get(record_id)
            .and_then(|commits| commits.get(git_commit))
            .map(|candidate| candidate.storage_id)
    }

    fn node_for_observation(
        &self,
        record_id: &str,
        temporal_key: &TemporalReadKey,
    ) -> Option<::aletheiadb::NodeId> {
        self.by_observation
            .get(record_id)
            .and_then(|observations| observations.get(temporal_key))
            .map(|candidate| candidate.storage_id)
    }

    fn endpoint_node(
        &self,
        record_id: &str,
        git_commit: Option<&str>,
    ) -> Result<Option<::aletheiadb::NodeId>, &'static str> {
        if let Some(git_commit) = git_commit
            && let Some(node_id) = self.node_for_commit(record_id, git_commit)
        {
            return Ok(Some(node_id));
        }
        if let Some(node_id) = self.non_temporal.get(record_id).copied() {
            return Ok(Some(node_id));
        }

        match self
            .candidate_counts
            .get(record_id)
            .copied()
            .unwrap_or_default()
        {
            0 => Ok(None),
            1 => Ok(self.single_candidate.get(record_id).copied()),
            _ => Err("has multiple temporal observations and no matching edge commit"),
        }
    }

    #[cfg(test)]
    fn candidate_count(&self, record_id: &str) -> usize {
        self.candidate_counts
            .get(record_id)
            .copied()
            .unwrap_or_default()
    }
}

impl EmbeddedAletheiaSink {
    /// Opens an embedded `AletheiaDB` store rooted at `data_dir` and acquires
    /// the Egregore store lease.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Contended`] when another live writer (embedded
    /// peer or daemon) holds the write lease, or [`AdapterError::Rejected`]
    /// when stale daemon metadata requires repair (run `eg repair run
    /// --confirm`) or `AletheiaDB` cannot open the requested data dir.
    pub fn open(data_dir: impl AsRef<Path>) -> AdapterResult<Self> {
        let data_dir = data_dir.as_ref();
        // AC 8: block embedded opens when stale non-stopped daemon metadata exists.
        // The operator must run `eg repair run --confirm` first to prove exclusive
        // ownership and clean up the stale runtime state.
        if let Some(msg) = crate::repair::embedded_open_repair_gate(data_dir) {
            return Err(AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: msg,
            });
        }
        let lease = acquire_write_lease(data_dir)?;
        Self::open_inner(data_dir, Some(lease))
    }

    pub(crate) fn open_unleased(data_dir: impl AsRef<Path>) -> AdapterResult<Self> {
        let data_dir = data_dir.as_ref();
        Self::open_inner(data_dir, None)
    }

    fn open_inner(data_dir: &Path, lease: Option<StoreLease>) -> AdapterResult<Self> {
        // Test-only: cap concurrent embedded stores (and serialise under disk
        // pressure) before spinning up the store's background flush thread.
        #[cfg(test)]
        let store_gate_permit = embedded_store_gate::acquire();
        let mut config = ::aletheiadb::config::durable_config_for_data_dir(data_dir);
        if is_fresh_data_dir(data_dir) {
            config.persistence.load_on_startup = false;
        }
        let db = ::aletheiadb::AletheiaDB::with_unified_config(config).map_err(|error| {
            AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: error.to_string(),
            }
        })?;
        let mut sink = Self {
            db,
            node_lookup: NodeLookupIndex::default(),
            tombstone_ids: BTreeMap::new(),
            record_handles: BTreeMap::new(),
            write_seq: 0,
            edge_seqs: BTreeMap::new(),
            tombstone_node_seqs: BTreeMap::new(),
            legacy_edge_counts: BTreeMap::new(),
            _lease: lease,
            #[cfg(feature = "embeddings")]
            embedding_vectors: BTreeMap::new(),
            #[cfg(test)]
            _store_gate_permit: store_gate_permit,
        };
        sink.rebuild_lookup_indexes()?;
        Ok(sink)
    }

    /// Opens a store and pre-loads embedding vectors so they are stored in each
    /// node during ingest. Enables an HNSW vector index on the `"embedding"`
    /// property for semantic search when the store does not already have one.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened, the existing embedding
    /// index is incompatible, or the vector index fails to initialise.
    #[cfg(feature = "embeddings")]
    pub fn open_with_embeddings(
        data_dir: impl AsRef<std::path::Path>,
        vectors: EmbeddingVectorMap,
        dimensions: usize,
    ) -> AdapterResult<Self> {
        if dimensions == 0 {
            return Err(AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: "embedding vector dimensions must be greater than zero".to_owned(),
            });
        }
        let data_dir = data_dir.as_ref();
        // AC 8: block embedded opens when stale non-stopped daemon metadata exists,
        // including the `--embed` ingest path. The operator must run
        // `eg repair run --confirm` first to prove exclusive ownership.
        if let Some(msg) = crate::repair::embedded_open_repair_gate(data_dir) {
            return Err(AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: msg,
            });
        }
        let lease = acquire_write_lease(data_dir)?;
        let mut sink = Self::open_inner(data_dir, Some(lease))?;
        sink.embedding_vectors = vectors;
        let metric = ::aletheiadb::index::vector::DistanceMetric::Cosine;
        if let Some(existing) = sink
            .db
            .list_vector_indexes()
            .into_iter()
            .find(|index| index.property_name == "embedding")
        {
            if existing.dimensions != dimensions {
                return Err(AdapterError::Rejected {
                    record_id: "embedded-store".to_owned(),
                    message: format!(
                        "existing embedding vector index has {} dimensions but ingest generated {}",
                        existing.dimensions, dimensions
                    ),
                });
            }
            if existing.distance_metric != metric {
                return Err(AdapterError::Rejected {
                    record_id: "embedded-store".to_owned(),
                    message: format!(
                        "existing embedding vector index uses {:?} but ingest requires {:?}",
                        existing.distance_metric, metric
                    ),
                });
            }
        } else {
            let hnsw = ::aletheiadb::index::vector::hnsw::HnswConfig {
                dimensions,
                metric,
                ..Default::default()
            };
            sink.db
                .enable_vector_index("embedding", hnsw)
                .map_err(|error| AdapterError::Rejected {
                    record_id: "embedded-store".to_owned(),
                    message: error.to_string(),
                })?;
        }
        Ok(sink)
    }

    /// Returns the dimensionality of the persisted `"embedding"` vector index,
    /// or `None` when the store has no semantic index.
    ///
    /// Used by daemon-backed semantic search to distinguish a store that was
    /// never ingested with embeddings (missing index) from a query whose vector
    /// dimensionality disagrees with the index (incompatible dimension), without
    /// leaking raw `AletheiaDB` internals to callers.
    #[cfg(feature = "embeddings")]
    #[must_use]
    pub fn embedding_index_dimensions(&self) -> Option<usize> {
        self.db
            .list_vector_indexes()
            .into_iter()
            .find(|index| index.property_name == "embedding")
            .map(|index| index.dimensions)
    }

    /// Searches for nodes whose stored embedding is most similar to `query_vector`.
    ///
    /// Returns up to `limit` results ordered by descending similarity.
    ///
    /// # Errors
    ///
    /// Returns an error if no vector index exists or the search fails.
    #[cfg(feature = "embeddings")]
    pub fn semantic_search(
        &self,
        query_vector: &[f32],
        limit: usize,
    ) -> AdapterResult<Vec<SemanticMatch>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let candidate_limits =
            semantic_candidate_fetch_limits(limit, self.db.get_all_node_ids().len());
        if candidate_limits.is_empty() {
            return Ok(Vec::new());
        }
        let active_tombstoned = self.active_deleted_ids()?;

        let mut results = Vec::with_capacity(limit);
        for raw_limit in candidate_limits {
            let raw = self
                .db
                .find_similar_by_embedding(query_vector, raw_limit)
                .map_err(|error| AdapterError::ReadBack {
                    record_id: "semantic-search".to_owned(),
                    message: error.to_string(),
                })?;
            let raw_len = raw.len();
            results.clear();
            let mut seen_record_ids = std::collections::BTreeSet::new();
            for (node_id, score) in raw {
                let Ok(node) = self.db.get_node(node_id) else {
                    continue;
                };
                let Some(record_id) = node
                    .get_property("codegraph_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                if active_tombstoned.contains(record_id.as_str()) {
                    continue;
                }
                if node
                    .get_property("superseded_by")
                    .and_then(|v| v.as_str())
                    .is_some_and(|superseded_by| !superseded_by.is_empty())
                {
                    continue;
                }
                if self.node_lookup.latest_node(&record_id) != Some(node_id) {
                    continue;
                }
                if !seen_record_ids.insert(record_id.clone()) {
                    continue;
                }
                let kind = node
                    .get_property("kind")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let name = node
                    .get_property("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let repo_relative_path = node
                    .get_property("repo_relative_path")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let span = span_from_properties(|key| node.get_property(key));
                results.push(SemanticMatch {
                    record_id,
                    kind,
                    name,
                    repo_relative_path,
                    score,
                    span,
                });
                if results.len() == limit {
                    break;
                }
            }
            if results.len() == limit || raw_len < raw_limit {
                break;
            }
        }
        Ok(results)
    }

    /// Persists embedded indexes so a subsequent process can reopen without
    /// replaying from scratch.
    ///
    /// # Errors
    ///
    /// Returns an error when `AletheiaDB` cannot persist its index manifest.
    pub fn persist_indexes(&self) -> AdapterResult<()> {
        self.db
            .persist_indexes()
            .map_err(|error| AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: error.to_string(),
            })
    }

    /// Reads all records from the embedded store for query purposes.
    ///
    /// Returns the latest observation of each node (by `codegraph_id`), all
    /// tombstones, and all edges (deduplicated by `codegraph_id`). The result
    /// mirrors a JSONL graph slice and can be passed directly to the CLI query
    /// helpers.
    ///
    /// # Errors
    ///
    /// Returns an error when the embedded store cannot read a node or edge.
    pub fn read_all_records(&self) -> AdapterResult<Vec<GraphRecord>> {
        // active_deleted_ids uses egregore_seq to correctly detect whether a tombstone has
        // been superseded by a later node or edge write (seq comparison beats a simple count).
        let active_tombstoned = self.active_deleted_ids()?;

        // Determine which tombstone *records* are stale so they are not re-emitted.
        // A tombstone is stale when its deleted_id is NOT in active_tombstoned (the write
        // that it was meant to suppress has been superseded by a newer write).
        let mut stale_tombstone_record_ids = std::collections::BTreeSet::new();
        for (tombstone_record_id, &tombstone_node_id) in &self.tombstone_ids {
            let node = self
                .db
                .get_node(tombstone_node_id)
                .map_err(|error| read_back_error("read_all_records", error.to_string()))?;
            let Some(deleted_id) = optional_str_property(
                "read_all_records",
                "deleted_id",
                node.get_property("deleted_id"),
            )?
            else {
                continue;
            };
            if !active_tombstoned.contains(deleted_id.as_str()) {
                stale_tombstone_record_ids.insert(tombstone_record_id.as_str());
            }
        }

        let mut records = Vec::new();
        let mut emitted_project_node_ids = BTreeSet::new();

        // Project records are mutable and append-with-same-entity-id. Include every
        // physical project node so status/body mutations remain visible through
        // transaction-time queries once that selector is wired up.
        for node_id in self.db.get_all_node_ids() {
            let node = self
                .db
                .get_node(node_id)
                .map_err(|error| read_back_error("read_all_records", error.to_string()))?;
            let Some(record_id) = optional_str_property(
                "read_all_records",
                "codegraph_id",
                node.get_property("codegraph_id"),
            )?
            else {
                continue;
            };
            if !record_id.starts_with("project:v1:")
                || active_tombstoned.contains(record_id.as_str())
                || optional_str_property(
                    "read_all_records",
                    "record_type",
                    node.get_property("record_type"),
                )?
                .as_deref()
                    != Some("node")
            {
                continue;
            }
            emitted_project_node_ids.insert(node_id);
            records.push(self.read_node_record(&record_id, node_id)?);
        }

        // Temporal observations: include ALL commit snapshots even for tombstoned records so
        // that `--at <commit>` queries can resolve past state after a deletion.
        for (record_id, commits) in &self.node_lookup.by_commit {
            for candidate in commits.values() {
                if emitted_project_node_ids.contains(&candidate.storage_id) {
                    continue;
                }
                records.push(self.read_node_record(record_id, candidate.storage_id)?);
            }
        }

        // Non-temporal (current-state) nodes: skip records that have been tombstoned.
        for (record_id, &node_id) in &self.node_lookup.non_temporal {
            if active_tombstoned.contains(record_id.as_str()) {
                continue;
            }
            if emitted_project_node_ids.contains(&node_id) {
                continue;
            }
            records.push(self.read_node_record(record_id, node_id)?);
        }

        // Tombstones: skip stale ones so the CLI deleted_id filter doesn't re-suppress restored records.
        for (record_id, &node_id) in &self.tombstone_ids {
            if stale_tombstone_record_ids.contains(record_id.as_str()) {
                continue;
            }
            records.push(self.read_tombstone_record(record_id, node_id)?);
        }

        for (codegraph_id, edge_id) in self.latest_edge_versions(&active_tombstoned)? {
            records.push(self.read_edge_record(&codegraph_id, edge_id)?);
        }

        Ok(records)
    }

    /// Collapses physical edges to one `AletheiaDB` edge per `codegraph_id`,
    /// skipping tombstoned IDs and preserving first-encounter emit order.
    ///
    /// Edges are append-only: a re-ingest with changed properties (e.g. a
    /// resolution-only upgrade of a pre-existing CALLS edge, issue #152)
    /// appends a second physical edge with the same `codegraph_id` and a
    /// higher `egregore_seq`. Duplicates collapse to the LATEST write —
    /// highest `egregore_seq` wins; a seq-stamped edge beats a legacy edge
    /// without the property; ties (and legacy-vs-legacy) fall back to the
    /// higher `EdgeId`, which the store assigns in write order.
    fn latest_edge_versions(
        &self,
        active_tombstoned: &BTreeSet<String>,
    ) -> AdapterResult<Vec<(String, ::aletheiadb::EdgeId)>> {
        let mut latest_edges: BTreeMap<String, (Option<u64>, ::aletheiadb::EdgeId)> =
            BTreeMap::new();
        let mut edge_emit_order: Vec<String> = Vec::new();
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|error| read_back_error("latest_edge_versions", error.to_string()))?;
                let Some(codegraph_id) = optional_str_property(
                    "latest_edge_versions",
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                else {
                    continue;
                };
                if active_tombstoned.contains(codegraph_id.as_str()) {
                    continue;
                }
                let seq = optional_str_property(
                    "latest_edge_versions",
                    "egregore_seq",
                    edge.get_property("egregore_seq"),
                )?
                .and_then(|s| s.parse::<u64>().ok());
                match latest_edges.entry(codegraph_id.clone()) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert((seq, edge_id));
                        edge_emit_order.push(codegraph_id);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        let (current_seq, current_edge_id) = *entry.get();
                        let candidate_is_later = match (seq, current_seq) {
                            (Some(new), Some(current)) => {
                                new > current || (new == current && edge_id > current_edge_id)
                            }
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            (None, None) => edge_id > current_edge_id,
                        };
                        if candidate_is_later {
                            *entry.get_mut() = (seq, edge_id);
                        }
                    }
                }
            }
        }
        Ok(edge_emit_order
            .into_iter()
            .map(|codegraph_id| {
                let (_, edge_id) = latest_edges[&codegraph_id];
                (codegraph_id, edge_id)
            })
            .collect())
    }

    /// Maps each actively tombstoned record's stable ID to its attribution
    /// parent: a tombstoned edge to its recorded source node, and a
    /// tombstoned containment-edge target to that same source.
    ///
    /// The current-state read ([`Self::read_all_records`]) suppresses
    /// tombstoned edge records entirely — and tombstoned non-temporal nodes
    /// with them — so a consumer holding only that record slice cannot
    /// resolve a tombstone's `deleted_id` to the repository owning it
    /// (issue #234 `--repo` scoping): a deleted *edge* ID needs the edge's
    /// source node, and a deleted *node* ID needs the containment topology
    /// (`CONTAINS`/`DEFINES`/`IMPORTS`) that was tombstoned along with it.
    /// This read-only sweep recovers both from the physical edges the
    /// append-only store still holds: every actively tombstoned edge maps to
    /// its recorded source node, and every tombstoned containment edge
    /// additionally maps its target node to that source, so a consumer can
    /// chase `deleted node → parent → … → repository`. Every physical
    /// version of a stable edge ID shares its endpoints (they participate in
    /// edge identity), so version collapse is unnecessary. Deterministic:
    /// `BTreeMap` ordering, property reads only, no writes.
    ///
    /// # Errors
    ///
    /// Returns an error if a physical record cannot be read.
    pub fn tombstoned_record_parents(&self) -> AdapterResult<BTreeMap<String, String>> {
        let active_tombstoned = self.active_deleted_ids()?;
        let mut parents = BTreeMap::new();
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self.db.get_edge(edge_id).map_err(|error| {
                    read_back_error("tombstoned_record_parents", error.to_string())
                })?;
                let Some(codegraph_id) = optional_str_property(
                    "tombstoned_record_parents",
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                else {
                    continue;
                };
                if !active_tombstoned.contains(codegraph_id.as_str()) {
                    continue;
                }
                let Some(source) = optional_str_property(
                    "tombstoned_record_parents",
                    "source_codegraph_id",
                    edge.get_property("source_codegraph_id"),
                )?
                else {
                    continue;
                };
                // The ownership topology mirrors `RepositoryIndex::build`'s
                // containment adjacency: a tombstoned containment edge is
                // exactly the link the current-state view withheld from the
                // index, so its target's attribution parent is its source.
                let label = optional_str_property(
                    "tombstoned_record_parents",
                    "label",
                    edge.get_property("label"),
                )?;
                if matches!(label.as_deref(), Some("CONTAINS" | "DEFINES" | "IMPORTS"))
                    && let Some(target) = optional_str_property(
                        "tombstoned_record_parents",
                        "target_codegraph_id",
                        edge.get_property("target_codegraph_id"),
                    )?
                {
                    parents.insert(target, source.clone());
                }
                parents.insert(codegraph_id, source);
            }
        }
        Ok(parents)
    }

    /// Like [`Self::read_all_records`], but also emits *superseded* non-temporal
    /// physical nodes — older versions of a stable ID that a later re-ingest
    /// replaced in the current-state index. Non-temporal node versions and
    /// active tombstones are emitted in write (`egregore_seq`) order relative to
    /// each other, so slice order mirrors the append-only JSONL write order that
    /// order-based consumers (the tx resolver's tie-break, evidence freshness's
    /// tombstone-restoration inference) rely on (issues #66, #205).
    ///
    /// Each write creates a new physical node and only repoints the current-state
    /// index, so prior non-temporal versions remain in the database. Current-state
    /// reads ([`Self::read_all_records`]) intentionally collapse to the latest
    /// version per stable ID; transaction-time queries (issue #66) instead need
    /// the prior versions to reconstruct a past store view. This method is used
    /// only by the transaction-time read paths, so non-tx query behaviour is
    /// unchanged.
    ///
    /// Temporal snapshots (already fully emitted via the commit index) and
    /// project nodes (already emitted in full) are not duplicated here.
    ///
    /// # Errors
    ///
    /// Returns an error if a physical record cannot be read.
    pub fn read_all_records_including_superseded(&self) -> AdapterResult<Vec<GraphRecord>> {
        // Start from the current-state read, then drop its current non-temporal
        // node versions and its (active) tombstones: every non-temporal physical
        // version (current, superseded, and active-tombstoned) and every active
        // tombstone is re-emitted below in write order.
        //
        // Ordering matters twice over. First, the transaction-time resolver
        // breaks equal-transaction-time ties between two versions of one stable
        // ID by input order (later wins). `read_all_records` emits the current
        // version first and a naive append would place older superseded versions
        // after it, so a `--tx-as-of` at/after a shared timestamp (e.g. a batch
        // ingest reusing one stamp) would resolve to the stale row. Re-emitting
        // all versions sorted by `egregore_seq` (the store's monotonic write
        // sequence) puts the latest write last, so the resolver's tie-break
        // picks it. Second (issue #205), order-based consumers such as evidence
        // freshness decide whether a tombstone is active by whether any version
        // of its deleted ID appears *after* it, mirroring the append-only JSONL
        // contract. Leaving tombstones in the current-state prefix would place
        // them before the write-ordered node suffix, making every genuine
        // deletion of a non-temporal record look like a restoration; tombstones
        // therefore join the same `egregore_seq`-ordered stream. Stale
        // tombstones (deleted ID re-emitted later) stay dropped, exactly as in
        // `read_all_records`, so the CLI `deleted_id` filter never re-suppresses
        // a restored record.
        let mut tombstones: Vec<GraphRecord> = Vec::new();
        let mut records: Vec<GraphRecord> = Vec::new();
        for record in self.read_all_records()? {
            match &record {
                GraphRecord::Tombstone { .. } => tombstones.push(record),
                // Keep project nodes (emitted in full), temporal nodes and
                // edges; drop current non-temporal node versions, which are
                // re-emitted in write order below.
                GraphRecord::Node {
                    temporal: None, id, ..
                } if !id.starts_with("project:v1:") => {}
                _ => records.push(record),
            }
        }

        // Temporal commit candidates already emitted by read_all_records (one per
        // commit). Their non-latest observations and every non-temporal physical
        // node are (re)collected in the write-ordered sweep below.
        let mut emitted_temporal: BTreeSet<::aletheiadb::NodeId> = BTreeSet::new();
        for commits in self.node_lookup.by_commit.values() {
            emitted_temporal.extend(commits.values().map(|candidate| candidate.storage_id));
        }

        // Sweep every physical node that is not an already-emitted current
        // temporal candidate or a project node, tagging each with its write
        // sequence. This surfaces superseded non-temporal versions, active-
        // tombstoned non-temporal nodes (the tx resolver ignores tombstones, so a
        // view predating a deletion must still see the pre-delete node), and the
        // non-latest temporal observations of a commit.
        let mut versioned: Vec<(u64, GraphRecord)> = Vec::new();
        for node_id in self.db.get_all_node_ids() {
            if emitted_temporal.contains(&node_id) {
                continue;
            }
            let node = self.db.get_node(node_id).map_err(|error| {
                read_back_error("read_all_records_including_superseded", error.to_string())
            })?;
            let Some(record_id) = optional_str_property(
                "read_all_records_including_superseded",
                "codegraph_id",
                node.get_property("codegraph_id"),
            )?
            else {
                continue;
            };
            // Node records only (tombstones and edges are not "node"); project
            // nodes are already emitted in full.
            if optional_str_property(
                "read_all_records_including_superseded",
                "record_type",
                node.get_property("record_type"),
            )?
            .as_deref()
                != Some("node")
                || record_id.starts_with("project:v1:")
            {
                continue;
            }
            // `egregore_seq` is the monotonic per-write sequence; legacy nodes
            // predating it sort first (seq 0), which is the correct write order
            // for any version written before the sequence system existed.
            let seq = optional_str_property(
                "read_all_records_including_superseded",
                "egregore_seq",
                node.get_property("egregore_seq"),
            )?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
            versioned.push((seq, self.read_node_record(&record_id, node_id)?));
        }
        // Active tombstones re-enter at their own write sequence. An active
        // tombstone is by definition the latest write for its deleted ID, so it
        // sorts after every re-emitted physical version of that ID and the
        // order-based restoration inference stays sound (issue #205). Legacy
        // tombstones predating the sequence system sort at 0 alongside legacy
        // nodes; the stable sort keeps them after equal-seq node versions
        // because they are appended below, matching the store's own
        // active/stale determination.
        for tombstone in tombstones {
            let seq = self
                .tombstone_ids
                .get(tombstone.id())
                .and_then(|node_id| self.tombstone_node_seqs.get(node_id))
                .copied()
                .unwrap_or(0);
            versioned.push((seq, tombstone));
        }
        // Stable sort by ascending write sequence: the latest write of any stable
        // ID lands last, so the resolver's later-input-wins tie-break prefers it.
        versioned.sort_by_key(|(seq, _)| *seq);
        records.extend(versioned.into_iter().map(|(_, record)| record));

        Ok(records)
    }

    /// Reads all physical records stored in the database for inspection.
    /// This retrieves every single node, tombstone, and edge physically stored in `AletheiaDB`
    /// without temporal deduplication, tombstone filtering, or schema version validation.
    ///
    /// # Errors
    ///
    /// Returns an error if a physical record cannot be read.
    pub fn inspect_all_records(&self) -> AdapterResult<InspectStoreReport> {
        let mut report = InspectStoreReport::default();

        // 1. Iterate over every single physical node in AletheiaDB
        for node_id in self.db.get_all_node_ids() {
            let node = self
                .db
                .get_node(node_id)
                .map_err(|error| read_back_error("inspect_all_records", error.to_string()))?;
            let Some(record_id) = optional_str_property(
                "inspect_all_records",
                "codegraph_id",
                node.get_property("codegraph_id"),
            )?
            else {
                continue;
            };

            let record_type = optional_str_property(
                "inspect_all_records",
                "record_type",
                node.get_property("record_type"),
            )?;

            let version = Self::node_record_version_from_properties(
                &node,
                &record_id,
                record_type.as_deref(),
            )?;
            let is_known = crate::schema_version::is_known_record_version(&version);

            if is_known {
                let record = if record_type.as_deref() == Some("tombstone") {
                    self.read_tombstone_record_internal(&record_id, node_id)?
                } else {
                    self.read_node_record_internal(&record_id, node_id)?
                };
                report.records.push(record);
            } else {
                report
                    .unknown_schema_versions
                    .push(crate::schema_version::UnknownSchemaVersion::new(version));
            }
        }

        // 2. Iterate over every single physical edge in AletheiaDB
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|error| read_back_error("inspect_all_records", error.to_string()))?;
                let Some(codegraph_id) = optional_str_property(
                    "inspect_all_records",
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                else {
                    continue;
                };

                let version = Self::edge_record_version_from_properties(&edge, &codegraph_id)?;
                let is_known = crate::schema_version::is_known_record_version(&version);

                if is_known {
                    let record = self.read_edge_record_internal(&codegraph_id, edge_id)?;
                    report.records.push(record);
                } else {
                    report
                        .unknown_schema_versions
                        .push(crate::schema_version::UnknownSchemaVersion::new(version));
                }
            }
        }

        Ok(report)
    }

    /// Reads the transaction-time-current serving view of the store with the
    /// same record selection as [`Self::read_all_records`]: the latest
    /// physical version per stable non-temporal ID (superseded prior versions
    /// are never serialized), every current per-commit temporal candidate,
    /// every physical project-node version, active tombstones, and the latest
    /// physical version of each edge. Records suppressed by an active
    /// tombstone (issue #231 retraction) are dropped; stale tombstones —
    /// whose target was revived by a later re-ingest — are dropped too, so
    /// neither the pre-retraction physical version nor a tombstone that
    /// downstream `deleted_id` filters would use to re-suppress the revived
    /// record ever reaches a caller. Unlike `read_all_records`, unknown
    /// `(domain, kind, schema_version)` physical records are tolerated: they
    /// are tallied per physical occurrence (they cannot be version-collapsed
    /// because they never deserialize), never serialized and never an error.
    /// This is the bulk analog of [`Self::read_back_current_until`] and must
    /// back any surface that hands raw records to clients (e.g. the daemon's
    /// `GET /v1/records`).
    ///
    /// # Errors
    ///
    /// Returns an error if a physical record, tombstone, or edge cannot be
    /// read.
    pub fn inspect_current_records(&self) -> AdapterResult<InspectStoreReport> {
        let active_tombstoned = self.active_deleted_ids()?;
        // Physical IDs of the current per-commit temporal candidates:
        // `read_all_records` serves every per-commit candidate (even for
        // tombstoned records, so `--at <commit>` views can resolve past
        // state); non-candidate temporal observations are superseded within
        // their commit and stay unserved.
        let mut current_temporal: BTreeSet<::aletheiadb::NodeId> = BTreeSet::new();
        for commits in self.node_lookup.by_commit.values() {
            current_temporal.extend(commits.values().map(|candidate| candidate.storage_id));
        }

        let mut report = InspectStoreReport::default();

        for node_id in self.db.get_all_node_ids() {
            let node = self
                .db
                .get_node(node_id)
                .map_err(|error| read_back_error("inspect_current_records", error.to_string()))?;
            let Some(record_id) = optional_str_property(
                "inspect_current_records",
                "codegraph_id",
                node.get_property("codegraph_id"),
            )?
            else {
                continue;
            };
            let record_type = optional_str_property(
                "inspect_current_records",
                "record_type",
                node.get_property("record_type"),
            )?;
            let version = Self::node_record_version_from_properties(
                &node,
                &record_id,
                record_type.as_deref(),
            )?;
            if !crate::schema_version::is_known_record_version(&version) {
                report
                    .unknown_schema_versions
                    .push(crate::schema_version::UnknownSchemaVersion::new(version));
                continue;
            }
            if record_type.as_deref() == Some("tombstone") {
                // Serve only the indexed (latest) physical version of an
                // active tombstone. A stale tombstone's target has been
                // revived by a later write; re-serving it would let
                // order-based `deleted_id` consumers re-suppress the revived
                // record (mirrors `read_all_records`).
                if self.tombstone_ids.get(&record_id) != Some(&node_id)
                    || self.stored_tombstone_is_stale(&record_id)?
                {
                    continue;
                }
                report
                    .records
                    .push(self.read_tombstone_record_internal(&record_id, node_id)?);
                continue;
            }
            let is_current = if record_id.starts_with("project:v1:") {
                // Project records are mutable append-with-same-entity-id;
                // every physical version is part of the current view unless
                // the record is actively tombstoned (mirrors
                // `read_all_records`).
                record_type.as_deref() == Some("node")
                    && !active_tombstoned.contains(record_id.as_str())
            } else if current_temporal.contains(&node_id) {
                true
            } else {
                !active_tombstoned.contains(record_id.as_str())
                    && self.node_lookup.non_temporal.get(record_id.as_str()) == Some(&node_id)
            };
            if is_current {
                report
                    .records
                    .push(self.read_node_record_internal(&record_id, node_id)?);
            }
        }

        // Edges: tally every unknown-version physical edge, then serve the
        // latest physical version of each stable edge ID (skipping actively
        // tombstoned IDs), mirroring `read_all_records`' edge collapse.
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self.db.get_edge(edge_id).map_err(|error| {
                    read_back_error("inspect_current_records", error.to_string())
                })?;
                let Some(codegraph_id) = optional_str_property(
                    "inspect_current_records",
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                else {
                    continue;
                };
                let version = Self::edge_record_version_from_properties(&edge, &codegraph_id)?;
                if !crate::schema_version::is_known_record_version(&version) {
                    report
                        .unknown_schema_versions
                        .push(crate::schema_version::UnknownSchemaVersion::new(version));
                }
            }
        }
        for (codegraph_id, edge_id) in self.latest_edge_versions(&active_tombstoned)? {
            let edge = self
                .db
                .get_edge(edge_id)
                .map_err(|error| read_back_error("inspect_current_records", error.to_string()))?;
            let version = Self::edge_record_version_from_properties(&edge, &codegraph_id)?;
            if !crate::schema_version::is_known_record_version(&version) {
                // Already tallied in the physical sweep above.
                continue;
            }
            report
                .records
                .push(self.read_edge_record_internal(&codegraph_id, edge_id)?);
        }

        Ok(report)
    }

    fn node_record_version_from_properties(
        node: &::aletheiadb::Node,
        record_id: &str,
        record_type: Option<&str>,
    ) -> AdapterResult<crate::schema_version::RecordVersion> {
        let schema_version = required_u32_property(
            record_id,
            "schema_version",
            node.get_property("schema_version"),
        )?;

        if record_type == Some("tombstone") {
            let deleted_id =
                optional_str_property(record_id, "deleted_id", node.get_property("deleted_id"))?;
            let domain = crate::schema_version::domain_from_record_id(record_id)
                .or_else(|| {
                    deleted_id
                        .as_ref()
                        .and_then(|d| crate::schema_version::domain_from_record_id(d))
                })
                .unwrap_or_else(|| "codegraph".to_owned());
            Ok(crate::schema_version::RecordVersion::new(
                domain,
                "Tombstone",
                schema_version,
            ))
        } else {
            let kind = required_str_property(record_id, "kind", node.get_property("kind"))?;
            let domain = optional_str_property(record_id, "domain", node.get_property("domain"))?;
            let domain = domain
                .map(|d| crate::schema_version::normalize_domain_name(&d))
                .or_else(|| crate::schema_version::domain_from_record_id(record_id))
                .unwrap_or_else(|| crate::schema_version::domain_for_node_kind(&kind).to_owned());
            Ok(crate::schema_version::RecordVersion::new(
                domain,
                kind,
                schema_version,
            ))
        }
    }

    fn edge_record_version_from_properties(
        edge: &::aletheiadb::Edge,
        record_id: &str,
    ) -> AdapterResult<crate::schema_version::RecordVersion> {
        let label = required_str_property(record_id, "label", edge.get_property("label"))?;
        let schema_version = required_u32_property(
            record_id,
            "schema_version",
            edge.get_property("schema_version"),
        )?;

        let domain = crate::schema_version::domain_from_record_id(record_id)
            .unwrap_or_else(|| crate::schema_version::domain_for_edge_label(&label).to_owned());
        Ok(crate::schema_version::RecordVersion::new(
            domain,
            label,
            schema_version,
        ))
    }

    /// Reads a graph record back by stable ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the embedded store cannot perform read-back.
    pub fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>> {
        <Self as GraphSink>::read_back(self, record_id)
    }

    pub(crate) fn read_back_until(
        &self,
        record_id: &str,
        deadline: Option<Instant>,
    ) -> AdapterResult<Option<GraphRecord>> {
        check_read_deadline(record_id, deadline)?;
        if let Some(node_id) = self.node_lookup.latest_node(record_id) {
            return self.read_node_record(record_id, node_id).map(Some);
        }
        if let Some(node_id) = self.tombstone_ids.get(record_id).copied() {
            return self.read_tombstone_record(record_id, node_id).map(Some);
        }
        if let Some(edge_id) = self.find_edge_id_by_codegraph_id_until(record_id, deadline)? {
            return self.read_edge_record(record_id, edge_id).map(Some);
        }
        Ok(None)
    }

    /// Like [`Self::read_back_until`], but scoped to the
    /// transaction-time-current view (issue #231): a record whose stable ID
    /// is suppressed by an active (non-stale) tombstone resolves to `None`
    /// instead of its physical latest bytes, matching the exclusion
    /// [`Self::read_all_records`] applies to current-state slices. Tombstone
    /// records themselves (and retraction events) resolve normally — active
    /// tombstones are part of the current view and the audit trail.
    ///
    /// Direct-lookup read surfaces (the daemon's `GET /v1/records/{id}` and
    /// the `get_records` query verb) must use this method so a retracted
    /// record cannot be fetched by anyone who still knows its handle.
    /// Write-path verification and internal existence checks keep using
    /// [`Self::read_back`], which reads physical latest state regardless of
    /// tombstones.
    ///
    /// # Errors
    ///
    /// Returns an error when the embedded store cannot perform read-back or
    /// the caller-supplied deadline expires.
    pub(crate) fn read_back_current_until(
        &self,
        record_id: &str,
        deadline: Option<Instant>,
    ) -> AdapterResult<Option<GraphRecord>> {
        let Some(record) = self.read_back_until(record_id, deadline)? else {
            return Ok(None);
        };
        if !matches!(record, GraphRecord::Tombstone { .. })
            && self.active_deleted_ids()?.contains(record_id)
        {
            return Ok(None);
        }
        Ok(Some(record))
    }

    #[cfg(test)]
    pub(crate) fn node_observation_count_for_test(&self, record_id: &str) -> usize {
        self.node_lookup.candidate_count(record_id)
    }

    #[cfg(test)]
    pub(crate) fn edge_observation_count_for_test(&self, record_id: &str) -> usize {
        let mut count = 0;
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                if self
                    .db
                    .get_edge(edge_id)
                    .ok()
                    .and_then(|edge| {
                        edge.get_property("codegraph_id")
                            .and_then(::aletheiadb::PropertyValue::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some(record_id)
                {
                    count += 1;
                }
            }
        }
        count
    }

    /// Forces the schema version of the latest node for a given record ID.
    /// Used only for testing.
    ///
    /// # Errors
    ///
    /// Returns an error if the node cannot be found or if updating the node fails.
    pub fn force_latest_node_schema_version_for_test(
        &self,
        record_id: &str,
        schema_version: u32,
    ) -> AdapterResult<()> {
        let node_id = self
            .node_lookup
            .latest_node(record_id)
            .ok_or_else(|| read_back_error(record_id, "test fixture node is not indexed"))?;
        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert("schema_version", i64::from(schema_version))
            .build();
        self.db
            .write(|tx| tx.update_node(node_id, properties))
            .map_err(|error| AdapterError::Rejected {
                record_id: record_id.to_owned(),
                message: error.to_string(),
            })
    }

    pub(crate) fn expected_record_state(
        &self,
        record: &GraphRecord,
    ) -> AdapterResult<ExpectedRecordState> {
        match record {
            GraphRecord::Node { id, temporal, .. } => {
                if let Some(temporal) = temporal {
                    let Some(temporal_key) = temporal_read_key_from_metadata(id, temporal) else {
                        return self.compare_latest_record(record);
                    };
                    if let Some(node_id) = self.node_lookup.node_for_observation(id, &temporal_key)
                    {
                        return self.compare_node_record(id, node_id, record);
                    }
                    return Ok(ExpectedRecordState::Missing);
                }
                self.compare_latest_record(record)
            }
            GraphRecord::Edge { id, .. } => self.compare_edge_record(id, record),
            GraphRecord::Tombstone { .. } => self.compare_latest_record(record),
        }
    }

    /// Returns true if the embedded graph contains a Repository -> File -> Symbol path.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read operation fails.
    pub fn has_repository_file_symbol_path(&self, repository_id: &str) -> AdapterResult<bool> {
        let Some(repo_node_id) = self.lookup_node_id_by_codegraph_id(repository_id) else {
            return Ok(false);
        };

        for contains_edge_id in self
            .db
            .get_outgoing_edges_with_label(repo_node_id, "CONTAINS")
        {
            let file_node_id = self.db.get_edge_target(contains_edge_id).map_err(|error| {
                AdapterError::ReadBack {
                    record_id: repository_id.to_owned(),
                    message: error.to_string(),
                }
            })?;
            for defines_edge_id in self
                .db
                .get_outgoing_edges_with_label(file_node_id, "DEFINES")
            {
                let symbol_node_id = self.db.get_edge_target(defines_edge_id).map_err(|error| {
                    AdapterError::ReadBack {
                        record_id: repository_id.to_owned(),
                        message: error.to_string(),
                    }
                })?;
                let symbol =
                    self.db
                        .get_node(symbol_node_id)
                        .map_err(|error| AdapterError::ReadBack {
                            record_id: repository_id.to_owned(),
                            message: error.to_string(),
                        })?;
                if symbol.get_property("kind").and_then(|value| value.as_str()) == Some("Symbol") {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Returns true if the embedded graph contains a Commit -> Change <- Symbol path.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read operation fails.
    /// Returns the codegraph IDs of all `Repository` nodes currently in the store.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read operation fails.
    pub fn stored_repository_ids(&self) -> AdapterResult<Vec<String>> {
        let tombstoned = self.active_deleted_ids()?;
        let mut ids = Vec::new();
        for record_id in self.node_lookup.latest.keys() {
            if tombstoned.contains(record_id.as_str()) {
                continue;
            }
            if let Some(record) = self.read_back(record_id)?
                && matches!(
                    record,
                    GraphRecord::Node {
                        kind: NodeKind::Repository,
                        ..
                    }
                )
            {
                ids.push(record_id.clone());
            }
        }
        Ok(ids)
    }

    /// Returns the codegraph IDs of all `Repository` nodes with `identity_source = local_path`.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read operation fails.
    pub fn stored_local_path_repository_ids(&self) -> AdapterResult<Vec<String>> {
        let tombstoned = self.active_deleted_ids()?;
        let mut ids = Vec::new();
        for record_id in self.node_lookup.latest.keys() {
            if tombstoned.contains(record_id.as_str()) {
                continue;
            }
            if let Some(GraphRecord::Node {
                kind: NodeKind::Repository,
                repository_identity,
                ..
            }) = self.read_back(record_id)?
            {
                let is_unsafe = repository_identity.as_deref().is_none_or(|payload| {
                    identity_payload_is_local(payload)
                        || !repository_id_matches_payload(record_id, payload)
                });
                if is_unsafe {
                    ids.push(record_id.clone());
                }
            }
        }
        Ok(ids)
    }

    /// Returns the set of record IDs that have active (non-stale) tombstones.
    fn active_deleted_ids(&self) -> AdapterResult<std::collections::BTreeSet<String>> {
        let mut deleted = std::collections::BTreeSet::new();
        for &tombstone_node_id in self.tombstone_ids.values() {
            let node = self
                .db
                .get_node(tombstone_node_id)
                .map_err(|e| read_back_error("active_deleted_ids", e.to_string()))?;
            let Some(deleted_id) = optional_str_property(
                "active_deleted_ids",
                "deleted_id",
                node.get_property("deleted_id"),
            )?
            else {
                continue;
            };
            if !self.tombstone_node_is_stale(tombstone_node_id, &deleted_id) {
                deleted.insert(deleted_id);
            }
        }
        Ok(deleted)
    }

    /// Returns true when the physical tombstone at `tombstone_node_id` no
    /// longer suppresses `deleted_id` because a newer write of that record
    /// supersedes it.
    fn tombstone_node_is_stale(
        &self,
        tombstone_node_id: ::aletheiadb::NodeId,
        deleted_id: &str,
    ) -> bool {
        // Tombstone is stale if the node record was re-ingested after it (higher NodeId).
        let node_stale = self
            .node_lookup
            .non_temporal
            .get(deleted_id)
            .is_some_and(|&live_node_id| live_node_id > tombstone_node_id);
        // Tombstone is stale if an edge with the same codegraph_id was written AFTER the
        // tombstone (higher egregore_seq). Using seq rather than a simple count correctly
        // handles updates: an edge that was re-written before being tombstoned has a higher
        // write count but a lower seq than the tombstone, so the tombstone is not stale.
        //
        // Four cases based on whether seq metadata is present:
        //   (edge_seq, tombstone_seq): comparison
        //   (Some(e), Some(t)):        e > t   — compare directly
        //   (Some(e), None):           true    — edge written after upgrade ⇒ newer than tombstone
        //   (None, Some(_)):           false   — edge written before upgrade ⇒ older than tombstone
        //   (None, None):              legacy  — fall back to count-based duplicate detection
        let tombstone_seq = self.tombstone_node_seqs.get(&tombstone_node_id).copied();
        let edge_seq = self.edge_seqs.get(deleted_id).copied();
        let edge_stale = match (edge_seq, tombstone_seq) {
            (Some(es), Some(ts)) => es > ts,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => self
                .legacy_edge_counts
                .get(deleted_id)
                .is_some_and(|&count| count > 1),
        };
        node_stale || edge_stale
    }

    /// Returns true when the tombstone record stored under
    /// `tombstone_record_id` is stale: a newer write of its deleted ID
    /// supersedes it, so it no longer suppresses anything.
    fn stored_tombstone_is_stale(&self, tombstone_record_id: &str) -> AdapterResult<bool> {
        let Some(&tombstone_node_id) = self.tombstone_ids.get(tombstone_record_id) else {
            return Ok(false);
        };
        let node = self
            .db
            .get_node(tombstone_node_id)
            .map_err(|e| read_back_error(tombstone_record_id, e.to_string()))?;
        let Some(deleted_id) = optional_str_property(
            tombstone_record_id,
            "deleted_id",
            node.get_property("deleted_id"),
        )?
        else {
            return Ok(false);
        };
        Ok(self.tombstone_node_is_stale(tombstone_node_id, &deleted_id))
    }

    /// Returns true if the store contains any records whose ID does not start with `codegraph:`.
    ///
    /// Scans both node and edge records; edge records are not indexed in `node_lookup`
    /// but may carry `agent_memory:v1:` IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read or edge operation fails.
    pub fn has_non_codegraph_records(&self) -> AdapterResult<bool> {
        let tombstoned = self.active_deleted_ids()?;
        // Check node records (skip tombstoned).
        if self
            .node_lookup
            .latest
            .keys()
            .any(|id| !tombstoned.contains(id.as_str()) && !id.starts_with("codegraph:"))
        {
            return Ok(true);
        }
        // Check non-stale tombstone records whose own codegraph_id is outside the codegraph:
        // namespace (e.g. agent_memory:v1: tombstones).  A non-stale tombstone is emitted by
        // read_all_records(), so it counts as a live non-codegraph record in the store.
        for (tombstone_record_id, &tombstone_node_id) in &self.tombstone_ids {
            if tombstone_record_id.starts_with("codegraph:") {
                continue;
            }
            let node = self
                .db
                .get_node(tombstone_node_id)
                .map_err(|e| read_back_error("has_non_codegraph_records", e.to_string()))?;
            let Some(deleted_id) = optional_str_property(
                "has_non_codegraph_records",
                "deleted_id",
                node.get_property("deleted_id"),
            )?
            else {
                continue;
            };
            // The tombstone is non-stale when its deleted_id appears in the active tombstoned set.
            if tombstoned.contains(deleted_id.as_str()) {
                return Ok(true);
            }
        }
        // Also check edge records (evidence links can carry agent_memory:v1: IDs).
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|e| read_back_error("has_non_codegraph_records", e.to_string()))?;
                let Some(edge_id_str) = optional_str_property(
                    "has_non_codegraph_records",
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                else {
                    continue;
                };
                if !tombstoned.contains(edge_id_str.as_str())
                    && !edge_id_str.starts_with("codegraph:")
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Returns true if the embedded graph contains a Commit -> Change -> Symbol path.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read operation fails.
    pub fn has_commit_change_symbol_path(&self, commit_id: &str) -> AdapterResult<bool> {
        let Some(commit_node_id) = self.lookup_node_id_by_codegraph_id(commit_id) else {
            return Ok(false);
        };

        for contains_edge_id in self
            .db
            .get_outgoing_edges_with_label(commit_node_id, "CONTAINS")
        {
            let change_node_id = self.db.get_edge_target(contains_edge_id).map_err(|error| {
                AdapterError::ReadBack {
                    record_id: commit_id.to_owned(),
                    message: error.to_string(),
                }
            })?;
            let change =
                self.db
                    .get_node(change_node_id)
                    .map_err(|error| AdapterError::ReadBack {
                        record_id: commit_id.to_owned(),
                        message: error.to_string(),
                    })?;
            if change.get_property("kind").and_then(|value| value.as_str()) != Some("Change") {
                continue;
            }

            for changed_edge_id in self
                .db
                .get_incoming_edges_with_label(change_node_id, "CHANGED_IN")
            {
                let symbol_node_id = self.db.get_edge_source(changed_edge_id).map_err(|error| {
                    AdapterError::ReadBack {
                        record_id: commit_id.to_owned(),
                        message: error.to_string(),
                    }
                })?;
                let symbol =
                    self.db
                        .get_node(symbol_node_id)
                        .map_err(|error| AdapterError::ReadBack {
                            record_id: commit_id.to_owned(),
                            message: error.to_string(),
                        })?;
                if symbol.get_property("kind").and_then(|value| value.as_str()) == Some("Symbol") {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }
}

impl GraphSink for EmbeddedAletheiaSink {
    fn write_record(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        validate_adapter_record_version(record)?;
        match record {
            GraphRecord::Node { .. } => self.write_node(record),
            GraphRecord::Edge { .. } => self.write_edge(record),
            GraphRecord::Tombstone { .. } => self.write_tombstone(record),
        }
    }

    fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>> {
        self.read_back_until(record_id, None)
    }

    fn verify_record(&self, record: &GraphRecord) -> AdapterResult<()> {
        let Some(handle) = self.record_handles.get(record.id()).copied() else {
            // Write was skipped (Matched); use cleared comparison to stay consistent with the Matched check.
            return match self.read_back(record.id())? {
                Some(read_back)
                    if read_back.with_cleared_producer_started_at()
                        == record.with_cleared_producer_started_at() =>
                {
                    Ok(())
                }
                Some(_) => Err(AdapterError::ReadBack {
                    record_id: record.id().to_owned(),
                    message: "record mismatch".to_owned(),
                }),
                None => Err(AdapterError::ReadBack {
                    record_id: record.id().to_owned(),
                    message: "record missing after write".to_owned(),
                }),
            };
        };

        match self.read_handle(record.id(), handle)? {
            read_back if read_back == *record => Ok(()),
            _ => Err(AdapterError::ReadBack {
                record_id: record.id().to_owned(),
                message: "record mismatch".to_owned(),
            }),
        }
    }
}

impl EmbeddedAletheiaSink {
    fn rebuild_lookup_indexes(&mut self) -> AdapterResult<()> {
        for node_id in self.db.get_all_node_ids() {
            self.index_stored_node(node_id, "embedded-store")?;
            // Rebuild edge_seqs: track the latest egregore_seq stored on each edge.
            // A higher seq means the edge was written later than something with a lower seq.
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|e| read_back_error("rebuild_lookup_indexes", e.to_string()))?;
                let Some(id) = optional_str_property(
                    "rebuild_lookup_indexes",
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                else {
                    continue;
                };
                let seq_str = optional_str_property(
                    "rebuild_lookup_indexes",
                    "egregore_seq",
                    edge.get_property("egregore_seq"),
                )?;
                match seq_str.as_deref().and_then(|s| s.parse::<u64>().ok()) {
                    Some(seq) => {
                        let entry = self.edge_seqs.entry(id).or_insert(0);
                        if seq > *entry {
                            *entry = seq;
                        }
                        if seq > self.write_seq {
                            self.write_seq = seq;
                        }
                    }
                    None => {
                        // Edge predates egregore_seq; count it for the legacy staleness fallback.
                        *self.legacy_edge_counts.entry(id).or_default() += 1;
                    }
                }
            }
        }
        // Second pass: rebuild tombstone_node_seqs from egregore_seq stored on tombstone nodes.
        let tombstone_node_ids: Vec<::aletheiadb::NodeId> =
            self.tombstone_ids.values().copied().collect();
        for tombstone_node_id in tombstone_node_ids {
            let node = self
                .db
                .get_node(tombstone_node_id)
                .map_err(|e| read_back_error("rebuild_lookup_indexes", e.to_string()))?;
            let Some(seq_str) = optional_str_property(
                "rebuild_lookup_indexes",
                "egregore_seq",
                node.get_property("egregore_seq"),
            )?
            else {
                continue;
            };
            if let Ok(seq) = seq_str.parse::<u64>() {
                self.tombstone_node_seqs.insert(tombstone_node_id, seq);
                if seq > self.write_seq {
                    self.write_seq = seq;
                }
            }
        }
        Ok(())
    }

    fn index_stored_node(
        &mut self,
        node_id: ::aletheiadb::NodeId,
        error_record_id: &str,
    ) -> AdapterResult<()> {
        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| read_back_error(error_record_id, error.to_string()))?;
        let Some(record_id) = optional_str_property(
            error_record_id,
            "codegraph_id",
            node.get_property("codegraph_id"),
        )?
        else {
            return Ok(());
        };
        let record_type = optional_str_property(
            error_record_id,
            "record_type",
            node.get_property("record_type"),
        )?;
        match record_type.as_deref() {
            Some("node") => {
                let temporal_key =
                    temporal_read_key_from_properties(&record_id, |key| node.get_property(key))?;
                self.node_lookup.insert(record_id, node_id, temporal_key);
                // Restore the write-sequence high-water mark so a re-opened store
                // keeps assigning strictly increasing `egregore_seq` to new node
                // writes (matching the edge/tombstone recovery below).
                if let Some(seq) = optional_str_property(
                    error_record_id,
                    "egregore_seq",
                    node.get_property("egregore_seq"),
                )?
                .and_then(|value| value.parse::<u64>().ok())
                    && seq > self.write_seq
                {
                    self.write_seq = seq;
                }
            }
            Some("tombstone") => {
                // A record ID can have several physical tombstone versions
                // (e.g. a retraction tombstone re-issued after its target was
                // revived by a re-ingest). Keep the latest write — highest
                // NodeId, mirroring the non-temporal node index — regardless
                // of storage iteration order, so staleness comparisons see
                // the newest deletion marker after a reopen.
                self.tombstone_ids
                    .entry(record_id)
                    .and_modify(|current| {
                        if node_id > *current {
                            *current = node_id;
                        }
                    })
                    .or_insert(node_id);
            }
            Some(_) | None => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn write_node(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        // Same revive-after-tombstone guard as `write_edge` (#333 Codex round-7):
        // a byte-identical node whose stable ID is actively tombstoned must write
        // a fresh version so the newer NodeId supersedes the tombstone and the
        // current read view surfaces the node again. Without this, an identical
        // re-emit would match and short-circuit, leaving the tombstone latest.
        if self.expected_record_state(record)? == ExpectedRecordState::Matched
            && !self.active_deleted_ids()?.contains(record.id())
        {
            #[cfg(feature = "embeddings")]
            self.backfill_embedding_for_matched_node(record)?;
            return Ok(());
        }
        let GraphRecord::Node {
            id,
            kind,
            schema_version,
            repo_relative_path,
            span,
            name,
            language,
            symbol_kind,
            disambiguator,
            visibility,
            signature,
            doc,
            call_context,
            note,
            temporal,
            semantic_drift,
            evidence_links,
            repository_identity,
            source_snapshot,
            dependency,
            log,
            text,
            superseded_by,
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            source_handle,
            redaction_policy_version,
            author_name,
            author_email,
            valid_time,
            valid_time_source,
            entity_id,
            title,
            body_handle,
            source_kind,
            source_external_link_id,
            assignees,
            labels,
            priority,
            parent_task_id,
            ordinal,
            verification_link_id,
            head_sha,
            head_ref,
            base_ref,
            merge_commit_sha,
            merged_at,
            draft,
            system,
            url,
            system_native_id,
            repository_remote,
            discovered_at,
            transaction_time,
            summary,
            domain,
            importer_id,
            importer_version,
            source_artifact_path,
            source_artifact_hash,
            patch_status,
            base_commit,
            unknown_base_reason,
            target_files,
            patch_bytes_hash,
            patch_bytes_size,
            patch_handle,
            validation_summary,
            producer_session_id,
            edit_kind,
            before_hash,
            after_hash,
            rename_to,
            hunk_count,
            linked_patch_id,
            linked_turn_id,
            tool_name,
            tool_kind,
            arguments_summary,
            arguments_handle,
            result_handle,
            produced_evidence_id,
            started_at,
            finished_at,
            failure_kind,
            exit_code,
            turn_index,
            stdout_handle,
            stderr_handle,
            evidence_quality,
            executed_at,
            verification_kind,
            status,
            review_kind,
            review_state,
            in_reply_to_id,
            author,
            diff_hunk_handle,
            review_side,
            user_context,
            producer,
        } = record
        else {
            unreachable!("write_node called with non-node record");
        };

        // Stamp each physical node write with the store's monotonic write
        // sequence so superseded versions of one stable ID can be ordered by
        // write order on read (the transaction-time resolver breaks equal-
        // transaction-time ties by input order).
        self.write_seq += 1;
        let seq_str = self.write_seq.to_string();
        let mut builder = base_properties(id, "node", *schema_version, summary)
            .insert("kind", kind.as_str())
            .insert("egregore_seq", seq_str.as_str());
        builder = insert_optional(builder, "repo_relative_path", repo_relative_path.as_deref());
        builder = insert_optional(builder, "name", name.as_deref());
        builder = insert_optional(builder, "author_name", author_name.as_deref());
        builder = insert_optional(builder, "author_email", author_email.as_deref());
        builder = insert_optional(builder, "language", language.as_deref());
        builder = insert_optional(builder, "symbol_kind", symbol_kind.as_deref());
        if let Some(disambiguator) = disambiguator {
            builder = builder.insert("disambiguator", disambiguator.to_string().as_str());
        }
        builder = insert_optional(builder, "visibility", visibility.as_deref());
        builder = insert_optional(builder, "signature", signature.as_deref());
        builder = insert_optional(builder, "doc", doc.as_deref());
        builder = insert_optional(builder, "call_context", call_context.as_deref());
        builder = insert_optional(builder, "note", note.as_deref());
        builder = insert_temporal(builder, temporal.as_ref());
        builder = insert_semantic_drift(builder, semantic_drift.as_deref());
        builder = insert_optional(builder, "node_valid_time", valid_time.as_deref());
        builder = insert_optional(
            builder,
            "node_valid_time_source",
            valid_time_source.as_deref(),
        );
        builder = insert_optional(builder, "entity_id", entity_id.as_deref());
        builder = insert_optional(builder, "title", title.as_deref());
        if let Some(handle) = body_handle
            && let Ok(json) = serde_json::to_string(handle.as_ref())
        {
            builder = builder.insert("body_handle_json", json.as_str());
        }
        builder = insert_optional(builder, "source_kind", source_kind.as_deref());
        builder = insert_optional(
            builder,
            "source_external_link_id",
            source_external_link_id.as_deref(),
        );
        if let Some(values) = assignees
            && let Ok(json) = serde_json::to_string(values)
        {
            builder = builder.insert("assignees_json", json.as_str());
        }
        if let Some(values) = labels
            && let Ok(json) = serde_json::to_string(values)
        {
            builder = builder.insert("labels_json", json.as_str());
        }
        builder = insert_optional(builder, "priority", priority.as_deref());
        builder = insert_optional(builder, "parent_task_id", parent_task_id.as_deref());
        if let Some(value) = ordinal {
            builder = builder.insert("ordinal", value.to_string().as_str());
        }
        builder = insert_optional(
            builder,
            "verification_link_id",
            verification_link_id.as_deref(),
        );
        // GitHub PR-promoted flat Task fields (issue #333). Plaintext substrate.
        builder = insert_optional(builder, "head_sha", head_sha.as_deref());
        builder = insert_optional(builder, "head_ref", head_ref.as_deref());
        builder = insert_optional(builder, "base_ref", base_ref.as_deref());
        builder = insert_optional(builder, "merge_commit_sha", merge_commit_sha.as_deref());
        builder = insert_optional(builder, "merged_at", merged_at.as_deref());
        if let Some(value) = draft {
            builder = builder.insert("draft", if *value { "true" } else { "false" });
        }
        builder = insert_optional(builder, "system", system.as_deref());
        builder = insert_optional(builder, "url", url.as_deref());
        builder = insert_optional(builder, "system_native_id", system_native_id.as_deref());
        builder = insert_optional(builder, "repository_remote", repository_remote.as_deref());
        builder = insert_optional(builder, "discovered_at", discovered_at.as_deref());
        builder = insert_optional(builder, "transaction_time", transaction_time.as_deref());
        if let Some(span) = span {
            builder = insert_span(builder, *span);
        }
        if let Some(links) = evidence_links
            && let Ok(json) = serde_json::to_string(links)
        {
            builder = builder.insert("evidence_links_json", json.as_str());
        }
        if let Some(identity) = repository_identity
            && let Ok(json) = serde_json::to_string(identity.as_ref())
        {
            builder = builder.insert("repository_identity_json", json.as_str());
        }
        if let Some(snapshot) = source_snapshot
            && let Ok(json) = serde_json::to_string(snapshot.as_ref())
        {
            builder = builder.insert("source_snapshot_json", json.as_str());
        }
        if let Some(payload) = dependency
            && let Ok(json) = serde_json::to_string(payload.as_ref())
        {
            builder = builder.insert("dependency_json", json.as_str());
        }
        if let Some(payload) = log
            && let Ok(json) = serde_json::to_string(payload.as_ref())
        {
            builder = builder.insert("log_json", json.as_str());
        }
        builder = insert_optional(builder, "text", text.as_deref());
        builder = insert_optional(builder, "superseded_by", superseded_by.as_deref());
        builder = insert_optional(builder, "agent_id", agent_id.as_deref());
        builder = insert_optional(builder, "agent_kind", agent_kind.as_deref());
        builder = insert_optional(builder, "session_id", session_id.as_deref());
        // Use "prov_observed_at" to avoid collision with temporal "observed_at".
        builder = insert_optional(builder, "prov_observed_at", observed_at.as_deref());
        builder = insert_optional(builder, "ingested_at", ingested_at.as_deref());
        builder = insert_optional(builder, "confidence", confidence.as_deref());
        builder = insert_optional(builder, "source_handle", source_handle.as_deref());
        builder = insert_optional(
            builder,
            "redaction_policy_version",
            redaction_policy_version.as_deref(),
        );
        builder = insert_optional(builder, "domain", domain.as_deref());
        builder = insert_optional(builder, "importer_id", importer_id.as_deref());
        builder = insert_optional(builder, "importer_version", importer_version.as_deref());
        builder = insert_optional(
            builder,
            "source_artifact_path",
            source_artifact_path.as_deref(),
        );
        builder = insert_optional(
            builder,
            "source_artifact_hash",
            source_artifact_hash.as_deref(),
        );
        builder = insert_optional(builder, "patch_status", patch_status.as_deref());
        builder = insert_optional(builder, "base_commit", base_commit.as_deref());
        builder = insert_optional(
            builder,
            "unknown_base_reason",
            unknown_base_reason.as_deref(),
        );
        if let Some(files) = target_files
            && let Ok(json) = serde_json::to_string(files)
        {
            builder = builder.insert("target_files_json", json.as_str());
        }
        builder = insert_optional(builder, "patch_bytes_hash", patch_bytes_hash.as_deref());
        if let Some(size) = patch_bytes_size {
            builder = builder.insert("patch_bytes_size", size.to_string().as_str());
        }
        if let Some(handle) = patch_handle
            && let Ok(json) = serde_json::to_string(handle.as_ref())
        {
            builder = builder.insert("patch_handle_json", json.as_str());
        }
        builder = insert_optional(builder, "validation_summary", validation_summary.as_deref());
        builder = insert_optional(
            builder,
            "producer_session_id",
            producer_session_id.as_deref(),
        );
        builder = insert_optional(builder, "edit_kind", edit_kind.as_deref());
        builder = insert_optional(builder, "before_hash", before_hash.as_deref());
        builder = insert_optional(builder, "after_hash", after_hash.as_deref());
        builder = insert_optional(builder, "rename_to", rename_to.as_deref());
        if let Some(count) = hunk_count {
            builder = builder.insert("hunk_count", count.to_string().as_str());
        }
        builder = insert_optional(builder, "linked_patch_id", linked_patch_id.as_deref());
        builder = insert_optional(builder, "linked_turn_id", linked_turn_id.as_deref());
        builder = insert_optional(builder, "tool_name", tool_name.as_deref());
        builder = insert_optional(builder, "tool_kind", tool_kind.as_deref());
        builder = insert_optional(builder, "arguments_summary", arguments_summary.as_deref());
        if let Some(handle) = arguments_handle
            && let Ok(json) = serde_json::to_string(handle.as_ref())
        {
            builder = builder.insert("arguments_handle_json", json.as_str());
        }
        if let Some(handle) = result_handle
            && let Ok(json) = serde_json::to_string(handle.as_ref())
        {
            builder = builder.insert("result_handle_json", json.as_str());
        }
        builder = insert_optional(
            builder,
            "produced_evidence_id",
            produced_evidence_id.as_deref(),
        );
        builder = insert_optional(builder, "started_at", started_at.as_deref());
        builder = insert_optional(builder, "finished_at", finished_at.as_deref());
        builder = insert_optional(builder, "failure_kind", failure_kind.as_deref());
        if let Some(code) = exit_code {
            builder = builder.insert("exit_code", code.to_string().as_str());
        }
        if let Some(idx) = turn_index {
            builder = builder.insert("turn_index", idx.to_string().as_str());
        }
        if let Some(handle) = stdout_handle
            && let Ok(json) = serde_json::to_string(handle.as_ref())
        {
            builder = builder.insert("stdout_handle_json", json.as_str());
        }
        if let Some(handle) = stderr_handle
            && let Ok(json) = serde_json::to_string(handle.as_ref())
        {
            builder = builder.insert("stderr_handle_json", json.as_str());
        }
        builder = insert_optional(builder, "evidence_quality", evidence_quality.as_deref());
        builder = insert_optional(builder, "executed_at", executed_at.as_deref());
        builder = insert_optional(builder, "verification_kind", verification_kind.as_deref());
        builder = insert_optional(builder, "status", status.as_deref());
        builder = insert_optional(builder, "review_kind", review_kind.as_deref());
        builder = insert_optional(builder, "review_state", review_state.as_deref());
        builder = insert_optional(builder, "in_reply_to_id", in_reply_to_id.as_deref());
        builder = insert_optional(builder, "author", author.as_deref());
        if let Some(handle) = diff_hunk_handle
            && let Ok(json) = serde_json::to_string(handle.as_ref())
        {
            builder = builder.insert("diff_hunk_handle_json", json.as_str());
        }
        builder = insert_optional(builder, "review_side", review_side.as_deref());
        if !user_context.is_empty()
            && let Ok(json) = serde_json::to_string(user_context)
        {
            builder = builder.insert("user_context_json", json.as_str());
        }
        if let Some(p) = producer
            && let Ok(json) = serde_json::to_string(p)
        {
            builder = builder.insert("producer_json", json.as_str());
        }
        #[cfg(feature = "embeddings")]
        if let Some(vector) = self.embedding_for_node_write(record) {
            builder = builder.insert_vector("embedding", &vector);
        }

        let node_id = self
            .db
            .create_node(node_label(*kind), builder.build())
            .map_err(|error| AdapterError::Rejected {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| AdapterError::ReadBack {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        if node
            .get_property("codegraph_id")
            .and_then(|value| value.as_str())
            != Some(id.as_str())
        {
            return Err(AdapterError::ReadBack {
                record_id: id.clone(),
                message: "embedded node codegraph_id mismatch".to_owned(),
            });
        }

        let temporal_key = temporal_read_key_from_properties(id, |key| node.get_property(key))?;
        self.node_lookup.insert(id.clone(), node_id, temporal_key);
        self.record_handles
            .insert(id.clone(), StoredRecord::Node(node_id));
        Ok(())
    }

    #[cfg(feature = "embeddings")]
    fn embedding_for_node_write(&self, record: &GraphRecord) -> Option<Vec<f32>> {
        let key = EmbeddingVectorKey::from_record(record)?;
        if let Some(vector) = self.embedding_vectors.get(&key) {
            return Some(vector.clone());
        }
        self.existing_embedding_for_record(record)
    }

    #[cfg(feature = "embeddings")]
    fn existing_embedding_for_record(&self, record: &GraphRecord) -> Option<Vec<f32>> {
        let GraphRecord::Node { id, temporal, .. } = record else {
            return None;
        };
        let node_id = self.node_id_for_observation(id, temporal.as_ref())?;
        self.db
            .get_node(node_id)
            .ok()?
            .get_property("embedding")
            .and_then(::aletheiadb::PropertyValue::as_vector)
            .map(<[f32]>::to_vec)
    }

    #[cfg(feature = "embeddings")]
    fn backfill_embedding_for_matched_node(&self, record: &GraphRecord) -> AdapterResult<()> {
        let GraphRecord::Node { id, temporal, .. } = record else {
            return Ok(());
        };
        let Some(key) = EmbeddingVectorKey::from_record(record) else {
            return Ok(());
        };
        let Some(vector) = self.embedding_vectors.get(&key).cloned() else {
            return Ok(());
        };
        let Some(node_id) = self.node_id_for_observation(id, temporal.as_ref()) else {
            return Ok(());
        };

        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| read_back_error(id, error.to_string()))?;
        if node
            .get_property("embedding")
            .and_then(::aletheiadb::PropertyValue::as_vector)
            .is_some_and(|existing| existing == vector.as_slice())
        {
            return Ok(());
        }

        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert_vector("embedding", &vector)
            .build();
        self.db
            .write(|tx| tx.update_node(node_id, properties))
            .map_err(|error| AdapterError::Rejected {
                record_id: id.clone(),
                message: error.to_string(),
            })?;

        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| read_back_error(id, error.to_string()))?;
        if node
            .get_property("embedding")
            .and_then(::aletheiadb::PropertyValue::as_vector)
            != Some(vector.as_slice())
        {
            return Err(AdapterError::ReadBack {
                record_id: id.clone(),
                message: "embedded node embedding was not persisted".to_owned(),
            });
        }
        Ok(())
    }

    #[cfg(feature = "embeddings")]
    fn node_id_for_observation(
        &self,
        record_id: &str,
        temporal: Option<&TemporalMetadata>,
    ) -> Option<::aletheiadb::NodeId> {
        if let Some(temporal) = temporal
            && let Some(temporal_key) = temporal_read_key_from_metadata(record_id, temporal)
            && let Some(node_id) = self
                .node_lookup
                .node_for_observation(record_id, &temporal_key)
        {
            return Some(node_id);
        }
        if temporal.is_some() {
            None
        } else {
            self.node_lookup.latest_node(record_id)
        }
    }

    fn write_tombstone(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        // A byte-identical tombstone is only a no-op while the stored copy is
        // still active. Once a newer write of the deleted ID supersedes it
        // (a revived record), re-issuing the same tombstone must land as a
        // fresh write so the deletion becomes the latest write again — this
        // is what the `eg forget` repair path relies on (issue #231).
        if self.expected_record_state(record)? == ExpectedRecordState::Matched
            && !self.stored_tombstone_is_stale(record.id())?
        {
            return Ok(());
        }
        let GraphRecord::Tombstone {
            id,
            schema_version,
            deleted_id,
            summary,
            producer,
        } = record
        else {
            unreachable!("write_tombstone called with non-tombstone record");
        };
        self.write_seq += 1;
        let seq = self.write_seq;
        let seq_str = seq.to_string();
        let mut builder = base_properties(id, "tombstone", *schema_version, summary)
            .insert("deleted_id", deleted_id.as_str())
            .insert("egregore_seq", seq_str.as_str());
        if let Some(p) = producer
            && let Ok(json) = serde_json::to_string(p)
        {
            builder = builder.insert("producer_json", json.as_str());
        }
        let properties = builder.build();
        let node_id = self
            .db
            .create_node("Tombstone", properties)
            .map_err(|error| AdapterError::Rejected {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| AdapterError::ReadBack {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        if node
            .get_property("codegraph_id")
            .and_then(|value| value.as_str())
            != Some(id.as_str())
        {
            return Err(AdapterError::ReadBack {
                record_id: id.clone(),
                message: "embedded tombstone codegraph_id mismatch".to_owned(),
            });
        }

        self.tombstone_ids.insert(id.clone(), node_id);
        self.tombstone_node_seqs.insert(node_id, seq);
        self.record_handles
            .insert(id.clone(), StoredRecord::Tombstone(node_id));
        Ok(())
    }

    fn write_edge(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        // A re-emitted edge whose bytes match an existing physical edge is
        // normally a no-op. But when the edge's stable ID is CURRENTLY actively
        // tombstoned, that matching physical edge is being SUPPRESSED by the
        // tombstone; short-circuiting would leave the tombstone the latest event
        // and keep the edge dead (revive-after-tombstone, #333 Codex round-7; cf.
        // the #318 stale-tombstone fix). Force a fresh write so the new
        // observation post-dates the tombstone (higher `egregore_seq`) and the
        // current read view (`read_all_records`) surfaces the edge again. Mirrors
        // the `write_tombstone` staleness short-circuit convention.
        if self.expected_record_state(record)? == ExpectedRecordState::Matched
            && !self.active_deleted_ids()?.contains(record.id())
        {
            return Ok(());
        }

        let GraphRecord::Edge {
            id,
            schema_version,
            label,
            source,
            target,
            confidence,
            resolution,
            temporal,
            summary,
            producer,
        } = record
        else {
            unreachable!("write_edge called with non-edge record");
        };
        let source_id = self.resolve_node_id(id, source, temporal.as_ref(), "source")?;
        let target_id = self.resolve_node_id(id, target, temporal.as_ref(), "target")?;
        self.write_seq += 1;
        let seq = self.write_seq;
        let seq_str = seq.to_string();
        let mut builder = base_properties(id, "edge", *schema_version, summary)
            .insert("label", label.as_str())
            .insert("source_codegraph_id", source.as_str())
            .insert("target_codegraph_id", target.as_str())
            .insert("egregore_seq", seq_str.as_str());
        builder = insert_optional(builder, "confidence", confidence.as_deref());
        builder = insert_optional(
            builder,
            "resolution",
            resolution.map(crate::ir::CallResolution::as_str),
        );
        builder = insert_temporal(builder, temporal.as_ref());
        if let Some(p) = producer
            && let Ok(json) = serde_json::to_string(p)
        {
            builder = builder.insert("producer_json", json.as_str());
        }

        let edge_id = self
            .db
            .create_edge(source_id, target_id, label.as_str(), builder.build())
            .map_err(|error| AdapterError::Rejected {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        let stored_source =
            self.db
                .get_edge_source(edge_id)
                .map_err(|error| AdapterError::ReadBack {
                    record_id: id.clone(),
                    message: error.to_string(),
                })?;
        let stored_target =
            self.db
                .get_edge_target(edge_id)
                .map_err(|error| AdapterError::ReadBack {
                    record_id: id.clone(),
                    message: error.to_string(),
                })?;
        if stored_source != source_id || stored_target != target_id {
            return Err(AdapterError::ReadBack {
                record_id: id.clone(),
                message: "embedded edge endpoint mismatch".to_owned(),
            });
        }

        self.record_handles
            .insert(id.clone(), StoredRecord::Edge(edge_id));
        self.edge_seqs.insert(id.clone(), seq);
        Ok(())
    }

    fn resolve_node_id(
        &self,
        edge_id: &str,
        record_id: &str,
        temporal: Option<&TemporalMetadata>,
        endpoint: &str,
    ) -> AdapterResult<::aletheiadb::NodeId> {
        let edge_git_commit = temporal.map(|metadata| metadata.git_commit.as_str());
        if let Some(node_id) = self
            .node_lookup
            .endpoint_node(record_id, edge_git_commit)
            .map_err(|message| AdapterError::Rejected {
                record_id: edge_id.to_owned(),
                message: format!("{endpoint} node {record_id} {message}"),
            })?
        {
            return Ok(node_id);
        }

        Err(AdapterError::Rejected {
            record_id: edge_id.to_owned(),
            message: format!("{endpoint} node {record_id} has not been written"),
        })
    }

    fn read_handle(&self, record_id: &str, handle: StoredRecord) -> AdapterResult<GraphRecord> {
        match handle {
            StoredRecord::Node(node_id) => self.read_node_record(record_id, node_id),
            StoredRecord::Edge(edge_id) => self.read_edge_record(record_id, edge_id),
            StoredRecord::Tombstone(node_id) => self.read_tombstone_record(record_id, node_id),
        }
    }

    fn lookup_node_id_by_codegraph_id(&self, record_id: &str) -> Option<::aletheiadb::NodeId> {
        self.node_lookup.latest_node(record_id)
    }

    fn find_edge_id_by_codegraph_id_until(
        &self,
        record_id: &str,
        deadline: Option<Instant>,
    ) -> AdapterResult<Option<::aletheiadb::EdgeId>> {
        let mut found = None;
        for node_id in self.db.get_all_node_ids() {
            check_read_deadline(record_id, deadline)?;
            for edge_id in self.db.get_outgoing_edges(node_id) {
                check_read_deadline(record_id, deadline)?;
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|error| read_back_error(record_id, error.to_string()))?;
                if optional_str_property(
                    record_id,
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                .as_deref()
                    == Some(record_id)
                {
                    let candidate = ReadBackCandidate {
                        storage_id: edge_id,
                        temporal_key: temporal_read_key_from_properties(record_id, |key| {
                            edge.get_property(key)
                        })?,
                    };
                    if should_replace_read_back_candidate(found.as_ref(), &candidate) {
                        found = Some(candidate);
                    }
                }
            }
        }
        Ok(found.map(|candidate| candidate.storage_id))
    }

    fn compare_latest_record(&self, record: &GraphRecord) -> AdapterResult<ExpectedRecordState> {
        match self.read_back(record.id())? {
            Some(read_back)
                if read_back.with_cleared_producer_started_at()
                    == record.with_cleared_producer_started_at() =>
            {
                Ok(ExpectedRecordState::Matched)
            }
            Some(_) => Ok(ExpectedRecordState::Mismatched),
            None => Ok(ExpectedRecordState::Missing),
        }
    }

    fn compare_node_record(
        &self,
        record_id: &str,
        node_id: ::aletheiadb::NodeId,
        expected: &GraphRecord,
    ) -> AdapterResult<ExpectedRecordState> {
        let read_back = self.read_node_record(record_id, node_id)?;
        if read_back.with_cleared_producer_started_at()
            == expected.with_cleared_producer_started_at()
        {
            Ok(ExpectedRecordState::Matched)
        } else {
            Ok(ExpectedRecordState::Mismatched)
        }
    }

    fn compare_edge_record(
        &self,
        record_id: &str,
        expected: &GraphRecord,
    ) -> AdapterResult<ExpectedRecordState> {
        let mut saw_same_id = false;
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|error| read_back_error(record_id, error.to_string()))?;
                if optional_str_property(
                    record_id,
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                .as_deref()
                    == Some(record_id)
                {
                    saw_same_id = true;
                    if self
                        .read_edge_record(record_id, edge_id)?
                        .with_cleared_producer_started_at()
                        == expected.with_cleared_producer_started_at()
                    {
                        return Ok(ExpectedRecordState::Matched);
                    }
                }
            }
        }
        if saw_same_id {
            Ok(ExpectedRecordState::Mismatched)
        } else {
            Ok(ExpectedRecordState::Missing)
        }
    }

    #[allow(clippy::too_many_lines)]
    fn read_node_record_internal(
        &self,
        record_id: &str,
        node_id: ::aletheiadb::NodeId,
    ) -> AdapterResult<GraphRecord> {
        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| read_back_error(record_id, error.to_string()))?;
        let id =
            required_str_property(record_id, "codegraph_id", node.get_property("codegraph_id"))?;
        if id != record_id {
            return Err(read_back_error(
                record_id,
                format!("embedded node codegraph_id mismatch: {id}"),
            ));
        }

        let record = GraphRecord::Node {
            id,
            kind: parse_node_kind(
                record_id,
                &required_str_property(record_id, "kind", node.get_property("kind"))?,
            )?,
            schema_version: required_u32_property(
                record_id,
                "schema_version",
                node.get_property("schema_version"),
            )?,
            repo_relative_path: optional_str_property(
                record_id,
                "repo_relative_path",
                node.get_property("repo_relative_path"),
            )?,
            span: source_span_from_properties(record_id, |key| node.get_property(key))?,
            name: optional_str_property(record_id, "name", node.get_property("name"))?,
            language: optional_str_property(record_id, "language", node.get_property("language"))?,
            symbol_kind: optional_str_property(
                record_id,
                "symbol_kind",
                node.get_property("symbol_kind"),
            )?,
            disambiguator: optional_str_property(
                record_id,
                "disambiguator",
                node.get_property("disambiguator"),
            )?
            .as_deref()
            .map(str::parse::<u64>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("disambiguator parse error: {e}")))?,
            visibility: optional_str_property(
                record_id,
                "visibility",
                node.get_property("visibility"),
            )?,
            signature: optional_str_property(
                record_id,
                "signature",
                node.get_property("signature"),
            )?,
            doc: optional_str_property(record_id, "doc", node.get_property("doc"))?,
            call_context: optional_str_property(
                record_id,
                "call_context",
                node.get_property("call_context"),
            )?,
            note: optional_str_property(record_id, "note", node.get_property("note"))?,
            temporal: temporal_from_properties(record_id, |key| node.get_property(key))?,
            semantic_drift: semantic_drift_from_properties(record_id, |key| {
                node.get_property(key)
            })?,
            evidence_links: optional_str_property(
                record_id,
                "evidence_links_json",
                node.get_property("evidence_links_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<Vec<EvidenceLink>>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("evidence_links_json invalid: {e}")))?,
            text: optional_str_property(record_id, "text", node.get_property("text"))?,
            superseded_by: optional_str_property(
                record_id,
                "superseded_by",
                node.get_property("superseded_by"),
            )?,
            agent_id: optional_str_property(record_id, "agent_id", node.get_property("agent_id"))?,
            agent_kind: optional_str_property(
                record_id,
                "agent_kind",
                node.get_property("agent_kind"),
            )?,
            session_id: optional_str_property(
                record_id,
                "session_id",
                node.get_property("session_id"),
            )?,
            // Stored under "prov_observed_at" to avoid collision with temporal "observed_at".
            observed_at: optional_str_property(
                record_id,
                "prov_observed_at",
                node.get_property("prov_observed_at"),
            )?,
            ingested_at: optional_str_property(
                record_id,
                "ingested_at",
                node.get_property("ingested_at"),
            )?,
            confidence: optional_str_property(
                record_id,
                "confidence",
                node.get_property("confidence"),
            )?,
            source_handle: optional_str_property(
                record_id,
                "source_handle",
                node.get_property("source_handle"),
            )?,
            redaction_policy_version: optional_str_property(
                record_id,
                "redaction_policy_version",
                node.get_property("redaction_policy_version"),
            )?,
            author_name: optional_str_property(
                record_id,
                "author_name",
                node.get_property("author_name"),
            )?,
            author_email: optional_str_property(
                record_id,
                "author_email",
                node.get_property("author_email"),
            )?,
            repository_identity: optional_str_property(
                record_id,
                "repository_identity_json",
                node.get_property("repository_identity_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::RepositoryIdentityPayload>)
            .transpose()
            .map_err(|e| {
                read_back_error(record_id, format!("repository_identity_json invalid: {e}"))
            })?
            .map(Box::new),
            source_snapshot: optional_str_property(
                record_id,
                "source_snapshot_json",
                node.get_property("source_snapshot_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::SourceSnapshotPayload>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("source_snapshot_json invalid: {e}")))?
            .map(Box::new),
            dependency: optional_str_property(
                record_id,
                "dependency_json",
                node.get_property("dependency_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::DependencyDeclarationPayload>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("dependency_json invalid: {e}")))?
            .map(Box::new),
            log: optional_str_property(record_id, "log_json", node.get_property("log_json"))?
                .as_deref()
                .map(serde_json::from_str::<crate::ir::LogPayload>)
                .transpose()
                .map_err(|e| read_back_error(record_id, format!("log_json invalid: {e}")))?
                .map(Box::new),
            valid_time: optional_str_property(
                record_id,
                "node_valid_time",
                node.get_property("node_valid_time"),
            )?,
            valid_time_source: optional_str_property(
                record_id,
                "node_valid_time_source",
                node.get_property("node_valid_time_source"),
            )?,
            entity_id: optional_str_property(
                record_id,
                "entity_id",
                node.get_property("entity_id"),
            )?,
            title: optional_str_property(record_id, "title", node.get_property("title"))?,
            body_handle: optional_str_property(
                record_id,
                "body_handle_json",
                node.get_property("body_handle_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::OutputHandle>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("body_handle_json invalid: {e}")))?
            .map(Box::new),
            source_kind: optional_str_property(
                record_id,
                "source_kind",
                node.get_property("source_kind"),
            )?,
            source_external_link_id: optional_str_property(
                record_id,
                "source_external_link_id",
                node.get_property("source_external_link_id"),
            )?,
            assignees: optional_str_property(
                record_id,
                "assignees_json",
                node.get_property("assignees_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<Vec<String>>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("assignees_json invalid: {e}")))?,
            labels: optional_str_property(
                record_id,
                "labels_json",
                node.get_property("labels_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<Vec<String>>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("labels_json invalid: {e}")))?,
            priority: optional_str_property(record_id, "priority", node.get_property("priority"))?,
            parent_task_id: optional_str_property(
                record_id,
                "parent_task_id",
                node.get_property("parent_task_id"),
            )?,
            ordinal: optional_str_property(record_id, "ordinal", node.get_property("ordinal"))?
                .as_deref()
                .map(str::parse::<u32>)
                .transpose()
                .map_err(|e| read_back_error(record_id, format!("ordinal parse error: {e}")))?,
            verification_link_id: optional_str_property(
                record_id,
                "verification_link_id",
                node.get_property("verification_link_id"),
            )?,
            // GitHub PR-promoted flat Task fields (issue #333).
            head_sha: optional_str_property(record_id, "head_sha", node.get_property("head_sha"))?,
            head_ref: optional_str_property(record_id, "head_ref", node.get_property("head_ref"))?,
            base_ref: optional_str_property(record_id, "base_ref", node.get_property("base_ref"))?,
            merge_commit_sha: optional_str_property(
                record_id,
                "merge_commit_sha",
                node.get_property("merge_commit_sha"),
            )?,
            merged_at: optional_str_property(
                record_id,
                "merged_at",
                node.get_property("merged_at"),
            )?,
            draft: optional_str_property(record_id, "draft", node.get_property("draft"))?
                .as_deref()
                .map(|s| s == "true"),
            system: optional_str_property(record_id, "system", node.get_property("system"))?,
            url: optional_str_property(record_id, "url", node.get_property("url"))?,
            system_native_id: optional_str_property(
                record_id,
                "system_native_id",
                node.get_property("system_native_id"),
            )?,
            repository_remote: optional_str_property(
                record_id,
                "repository_remote",
                node.get_property("repository_remote"),
            )?,
            discovered_at: optional_str_property(
                record_id,
                "discovered_at",
                node.get_property("discovered_at"),
            )?,
            transaction_time: optional_str_property(
                record_id,
                "transaction_time",
                node.get_property("transaction_time"),
            )?,
            summary: required_str_property(record_id, "summary", node.get_property("summary"))?,
            domain: optional_str_property(record_id, "domain", node.get_property("domain"))?,
            importer_id: optional_str_property(
                record_id,
                "importer_id",
                node.get_property("importer_id"),
            )?,
            importer_version: optional_str_property(
                record_id,
                "importer_version",
                node.get_property("importer_version"),
            )?,
            source_artifact_path: optional_str_property(
                record_id,
                "source_artifact_path",
                node.get_property("source_artifact_path"),
            )?,
            source_artifact_hash: optional_str_property(
                record_id,
                "source_artifact_hash",
                node.get_property("source_artifact_hash"),
            )?,
            patch_status: optional_str_property(
                record_id,
                "patch_status",
                node.get_property("patch_status"),
            )?,
            base_commit: optional_str_property(
                record_id,
                "base_commit",
                node.get_property("base_commit"),
            )?,
            unknown_base_reason: optional_str_property(
                record_id,
                "unknown_base_reason",
                node.get_property("unknown_base_reason"),
            )?,
            target_files: optional_str_property(
                record_id,
                "target_files_json",
                node.get_property("target_files_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<Vec<String>>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("target_files_json invalid: {e}")))?,
            patch_bytes_hash: optional_str_property(
                record_id,
                "patch_bytes_hash",
                node.get_property("patch_bytes_hash"),
            )?,
            patch_bytes_size: optional_str_property(
                record_id,
                "patch_bytes_size",
                node.get_property("patch_bytes_size"),
            )?
            .as_deref()
            .map(str::parse::<u64>)
            .transpose()
            .map_err(|e| {
                read_back_error(record_id, format!("patch_bytes_size parse error: {e}"))
            })?,
            patch_handle: optional_str_property(
                record_id,
                "patch_handle_json",
                node.get_property("patch_handle_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::PatchHandle>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("patch_handle_json invalid: {e}")))?
            .map(Box::new),
            validation_summary: optional_str_property(
                record_id,
                "validation_summary",
                node.get_property("validation_summary"),
            )?,
            producer_session_id: optional_str_property(
                record_id,
                "producer_session_id",
                node.get_property("producer_session_id"),
            )?,
            edit_kind: optional_str_property(
                record_id,
                "edit_kind",
                node.get_property("edit_kind"),
            )?,
            before_hash: optional_str_property(
                record_id,
                "before_hash",
                node.get_property("before_hash"),
            )?,
            after_hash: optional_str_property(
                record_id,
                "after_hash",
                node.get_property("after_hash"),
            )?,
            rename_to: optional_str_property(
                record_id,
                "rename_to",
                node.get_property("rename_to"),
            )?,
            hunk_count: optional_str_property(
                record_id,
                "hunk_count",
                node.get_property("hunk_count"),
            )?
            .as_deref()
            .map(str::parse::<u32>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("hunk_count parse error: {e}")))?,
            linked_patch_id: optional_str_property(
                record_id,
                "linked_patch_id",
                node.get_property("linked_patch_id"),
            )?,
            linked_turn_id: optional_str_property(
                record_id,
                "linked_turn_id",
                node.get_property("linked_turn_id"),
            )?,
            tool_name: optional_str_property(
                record_id,
                "tool_name",
                node.get_property("tool_name"),
            )?,
            tool_kind: optional_str_property(
                record_id,
                "tool_kind",
                node.get_property("tool_kind"),
            )?,
            arguments_summary: optional_str_property(
                record_id,
                "arguments_summary",
                node.get_property("arguments_summary"),
            )?,
            arguments_handle: optional_str_property(
                record_id,
                "arguments_handle_json",
                node.get_property("arguments_handle_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::OutputHandle>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("arguments_handle_json invalid: {e}")))?
            .map(Box::new),
            result_handle: optional_str_property(
                record_id,
                "result_handle_json",
                node.get_property("result_handle_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::OutputHandle>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("result_handle_json invalid: {e}")))?
            .map(Box::new),
            produced_evidence_id: optional_str_property(
                record_id,
                "produced_evidence_id",
                node.get_property("produced_evidence_id"),
            )?,
            started_at: optional_str_property(
                record_id,
                "started_at",
                node.get_property("started_at"),
            )?,
            finished_at: optional_str_property(
                record_id,
                "finished_at",
                node.get_property("finished_at"),
            )?,
            failure_kind: optional_str_property(
                record_id,
                "failure_kind",
                node.get_property("failure_kind"),
            )?,
            exit_code: optional_str_property(
                record_id,
                "exit_code",
                node.get_property("exit_code"),
            )?
            .as_deref()
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("exit_code parse error: {e}")))?,
            turn_index: optional_str_property(
                record_id,
                "turn_index",
                node.get_property("turn_index"),
            )?
            .as_deref()
            .map(str::parse::<u64>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("turn_index parse error: {e}")))?,
            stdout_handle: optional_str_property(
                record_id,
                "stdout_handle_json",
                node.get_property("stdout_handle_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::OutputHandle>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("stdout_handle_json invalid: {e}")))?
            .map(Box::new),
            stderr_handle: optional_str_property(
                record_id,
                "stderr_handle_json",
                node.get_property("stderr_handle_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::OutputHandle>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("stderr_handle_json invalid: {e}")))?
            .map(Box::new),
            evidence_quality: optional_str_property(
                record_id,
                "evidence_quality",
                node.get_property("evidence_quality"),
            )?,
            executed_at: optional_str_property(
                record_id,
                "executed_at",
                node.get_property("executed_at"),
            )?,
            verification_kind: optional_str_property(
                record_id,
                "verification_kind",
                node.get_property("verification_kind"),
            )?,
            status: optional_str_property(record_id, "status", node.get_property("status"))?,
            review_kind: optional_str_property(
                record_id,
                "review_kind",
                node.get_property("review_kind"),
            )?,
            review_state: optional_str_property(
                record_id,
                "review_state",
                node.get_property("review_state"),
            )?,
            in_reply_to_id: optional_str_property(
                record_id,
                "in_reply_to_id",
                node.get_property("in_reply_to_id"),
            )?,
            author: optional_str_property(record_id, "author", node.get_property("author"))?,
            diff_hunk_handle: optional_str_property(
                record_id,
                "diff_hunk_handle_json",
                node.get_property("diff_hunk_handle_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::OutputHandle>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("diff_hunk_handle_json invalid: {e}")))?
            .map(Box::new),
            review_side: optional_str_property(
                record_id,
                "review_side",
                node.get_property("review_side"),
            )?,
            user_context: optional_str_property(
                record_id,
                "user_context_json",
                node.get_property("user_context_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<UserContextFields>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("user_context_json invalid: {e}")))?
            .unwrap_or_else(UserContextFields::empty),
            producer: optional_str_property(
                record_id,
                "producer_json",
                node.get_property("producer_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<Producer>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("producer_json invalid: {e}")))?,
        };
        Ok(record)
    }

    fn read_node_record(
        &self,
        record_id: &str,
        node_id: ::aletheiadb::NodeId,
    ) -> AdapterResult<GraphRecord> {
        let record = self.read_node_record_internal(record_id, node_id)?;
        validate_adapter_record_version(&record)?;
        Ok(record)
    }

    fn read_tombstone_record_internal(
        &self,
        record_id: &str,
        node_id: ::aletheiadb::NodeId,
    ) -> AdapterResult<GraphRecord> {
        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| read_back_error(record_id, error.to_string()))?;
        let id =
            required_str_property(record_id, "codegraph_id", node.get_property("codegraph_id"))?;
        if id != record_id {
            return Err(read_back_error(
                record_id,
                format!("embedded tombstone codegraph_id mismatch: {id}"),
            ));
        }
        let record_type =
            required_str_property(record_id, "record_type", node.get_property("record_type"))?;
        if record_type != "tombstone" {
            return Err(read_back_error(
                record_id,
                format!("embedded tombstone record_type mismatch: {record_type}"),
            ));
        }

        let record = GraphRecord::Tombstone {
            id,
            schema_version: required_u32_property(
                record_id,
                "schema_version",
                node.get_property("schema_version"),
            )?,
            deleted_id: required_str_property(
                record_id,
                "deleted_id",
                node.get_property("deleted_id"),
            )?,
            summary: required_str_property(record_id, "summary", node.get_property("summary"))?,
            producer: optional_str_property(
                record_id,
                "producer_json",
                node.get_property("producer_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<Producer>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("producer_json invalid: {e}")))?,
        };
        Ok(record)
    }

    fn read_tombstone_record(
        &self,
        record_id: &str,
        node_id: ::aletheiadb::NodeId,
    ) -> AdapterResult<GraphRecord> {
        let record = self.read_tombstone_record_internal(record_id, node_id)?;
        validate_adapter_record_version(&record)?;
        Ok(record)
    }

    fn read_edge_record_internal(
        &self,
        record_id: &str,
        edge_id: ::aletheiadb::EdgeId,
    ) -> AdapterResult<GraphRecord> {
        let edge = self
            .db
            .get_edge(edge_id)
            .map_err(|error| read_back_error(record_id, error.to_string()))?;
        let id =
            required_str_property(record_id, "codegraph_id", edge.get_property("codegraph_id"))?;
        if id != record_id {
            return Err(read_back_error(
                record_id,
                format!("embedded edge codegraph_id mismatch: {id}"),
            ));
        }

        let record = GraphRecord::Edge {
            id,
            schema_version: required_u32_property(
                record_id,
                "schema_version",
                edge.get_property("schema_version"),
            )?,
            label: parse_edge_label(
                record_id,
                &required_str_property(record_id, "label", edge.get_property("label"))?,
            )?,
            source: required_str_property(
                record_id,
                "source_codegraph_id",
                edge.get_property("source_codegraph_id"),
            )?,
            target: required_str_property(
                record_id,
                "target_codegraph_id",
                edge.get_property("target_codegraph_id"),
            )?,
            confidence: optional_str_property(
                record_id,
                "confidence",
                edge.get_property("confidence"),
            )?,
            resolution: optional_str_property(
                record_id,
                "resolution",
                edge.get_property("resolution"),
            )?
            .as_deref()
            .map(|value| {
                crate::ir::CallResolution::from_wire(value).ok_or_else(|| {
                    read_back_error(record_id, format!("resolution invalid: {value}"))
                })
            })
            .transpose()?,
            temporal: temporal_from_properties(record_id, |key| edge.get_property(key))?,
            summary: required_str_property(record_id, "summary", edge.get_property("summary"))?,
            producer: optional_str_property(
                record_id,
                "producer_json",
                edge.get_property("producer_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<Producer>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("producer_json invalid: {e}")))?,
        };
        Ok(record)
    }

    fn read_edge_record(
        &self,
        record_id: &str,
        edge_id: ::aletheiadb::EdgeId,
    ) -> AdapterResult<GraphRecord> {
        let record = self.read_edge_record_internal(record_id, edge_id)?;
        validate_adapter_record_version(&record)?;
        Ok(record)
    }
}

/// Returns `true` if the identity payload indicates that the Repository is machine-local
/// and therefore unsafe for use in a shared store.
///
/// A `Remote` payload is only considered safe when `remote_url` is present and non-local.
/// A `LocalRootCommit` payload is only considered safe when `root_commit_sha` is present and
/// non-empty. A missing payload is treated as unsafe (legacy/unverifiable write path).
fn identity_payload_is_local(payload: &crate::ir::RepositoryIdentityPayload) -> bool {
    match payload.identity_source {
        IdentitySource::LocalPath => true,
        IdentitySource::Remote => payload
            .remote_url
            .as_deref()
            .is_none_or(is_local_remote_url),
        IdentitySource::LocalRootCommit => {
            payload.root_commit_sha.as_deref().is_none_or(str::is_empty)
        }
        IdentitySource::OperatorOverride => false,
    }
}

fn read_back_error(record_id: &str, message: impl Into<String>) -> AdapterError {
    AdapterError::ReadBack {
        record_id: record_id.to_owned(),
        message: message.into(),
    }
}

fn check_read_deadline(record_id: &str, deadline: Option<Instant>) -> AdapterResult<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(AdapterError::TimedOut {
            record_id: record_id.to_owned(),
        });
    }
    Ok(())
}

fn required_str_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<String> {
    optional_str_property(record_id, key, value)?.ok_or_else(|| {
        read_back_error(record_id, format!("missing embedded string property {key}"))
    })
}

fn optional_str_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<String>> {
    value.map_or(Ok(None), |value| {
        value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| {
                read_back_error(
                    record_id,
                    format!("embedded property {key} is not a string"),
                )
            })
    })
}

fn required_u32_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<u32> {
    let raw = value
        .and_then(::aletheiadb::PropertyValue::as_int)
        .ok_or_else(|| {
            read_back_error(
                record_id,
                format!("missing embedded integer property {key}"),
            )
        })?;
    u32::try_from(raw).map_err(|error| {
        read_back_error(
            record_id,
            format!("embedded integer property {key} is out of range: {error}"),
        )
    })
}

fn optional_usize_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<usize>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let raw = value.as_int().ok_or_else(|| {
        read_back_error(
            record_id,
            format!("embedded property {key} is not an integer"),
        )
    })?;
    usize::try_from(raw).map(Some).map_err(|error| {
        read_back_error(
            record_id,
            format!("embedded integer property {key} is out of range: {error}"),
        )
    })
}

fn source_span_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<SourceSpan>> {
    let start_byte = optional_usize_property(record_id, "start_byte", get("start_byte"))?;
    let end_byte = optional_usize_property(record_id, "end_byte", get("end_byte"))?;
    let start_line = optional_usize_property(record_id, "start_line", get("start_line"))?;
    let end_line = optional_usize_property(record_id, "end_line", get("end_line"))?;

    match (start_byte, end_byte, start_line, end_line) {
        (None, None, None, None) => Ok(None),
        (Some(start_byte), Some(end_byte), Some(start_line), Some(end_line)) => {
            Ok(Some(SourceSpan {
                start_byte,
                end_byte,
                start_line,
                end_line,
            }))
        }
        _ => Err(read_back_error(
            record_id,
            "embedded source span is only partially present",
        )),
    }
}

fn temporal_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<TemporalMetadata>> {
    let Some(git_commit) = optional_str_property(record_id, "git_commit", get("git_commit"))?
    else {
        return Ok(None);
    };
    let parents =
        optional_str_property(record_id, "git_parent_commits", get("git_parent_commits"))?
            .map(|parents| parents.split_whitespace().map(ToOwned::to_owned).collect())
            .unwrap_or_default();

    Ok(Some(TemporalMetadata {
        git_commit,
        git_parent_commits: parents,
        valid_time: required_str_property(record_id, "valid_time", get("valid_time"))?,
        author_time: optional_str_property(record_id, "author_time", get("author_time"))?,
        observed_at: required_str_property(record_id, "observed_at", get("observed_at"))?,
        valid_time_source: optional_str_property(
            record_id,
            "valid_time_source",
            get("valid_time_source"),
        )?,
    }))
}

fn temporal_read_key_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<TemporalReadKey>> {
    let Some(git_commit) = optional_str_property(record_id, "git_commit", get("git_commit"))?
    else {
        return Ok(None);
    };

    Ok(Some(TemporalReadKey {
        valid_time: required_rfc3339_property(record_id, "valid_time", get("valid_time"))?,
        observed_at: required_rfc3339_property(record_id, "observed_at", get("observed_at"))?,
        git_commit,
    }))
}

fn temporal_read_key_from_metadata(
    _record_id: &str,
    temporal: &TemporalMetadata,
) -> Option<TemporalReadKey> {
    Some(TemporalReadKey {
        valid_time: DateTime::parse_from_rfc3339(&temporal.valid_time)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .ok()?,
        observed_at: DateTime::parse_from_rfc3339(&temporal.observed_at)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .ok()?,
        git_commit: temporal.git_commit.clone(),
    })
}

fn required_rfc3339_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<DateTime<Utc>> {
    let raw = required_str_property(record_id, key, value)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| {
            read_back_error(
                record_id,
                format!("embedded timestamp property {key} is not RFC3339: {error}"),
            )
        })
}

fn should_replace_read_back_candidate<Id: Ord>(
    current: Option<&ReadBackCandidate<Id>>,
    candidate: &ReadBackCandidate<Id>,
) -> bool {
    let Some(current) = current else {
        return true;
    };

    match (&current.temporal_key, candidate.temporal_key.as_ref()) {
        (None, None) => candidate.storage_id > current.storage_id,
        (None, Some(_)) => true,
        (Some(current_key), Some(candidate_key)) => {
            candidate_key > current_key
                || (candidate_key == current_key && candidate.storage_id > current.storage_id)
        }
        (Some(_), None) => false,
    }
}

fn semantic_drift_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<Box<SemanticDriftMetadata>>> {
    let Some(provider) = optional_str_property(
        record_id,
        "embedding_model_provider",
        get("embedding_model_provider"),
    )?
    else {
        return Ok(None);
    };

    Ok(Some(Box::new(SemanticDriftMetadata {
        embedding_model: EmbeddingModel {
            provider,
            name: required_str_property(
                record_id,
                "embedding_model_name",
                get("embedding_model_name"),
            )?,
            version: required_str_property(
                record_id,
                "embedding_model_version",
                get("embedding_model_version"),
            )?,
            dim: required_u32_property(
                record_id,
                "embedding_model_dim",
                get("embedding_model_dim"),
            )?,
            content_hash: required_str_property(
                record_id,
                "embedding_model_content_hash",
                get("embedding_model_content_hash"),
            )?,
        },
        target_record_id: required_str_property(
            record_id,
            "drift_target_record_id",
            get("drift_target_record_id"),
        )?,
        prior_record_id: required_str_property(
            record_id,
            "drift_prior_record_id",
            get("drift_prior_record_id"),
        )?,
        before_git_commit: required_str_property(
            record_id,
            "before_git_commit",
            get("before_git_commit"),
        )?,
        after_git_commit: required_str_property(
            record_id,
            "after_git_commit",
            get("after_git_commit"),
        )?,
        before_valid_time: required_str_property(
            record_id,
            "before_valid_time",
            get("before_valid_time"),
        )?,
        after_valid_time: required_str_property(
            record_id,
            "after_valid_time",
            get("after_valid_time"),
        )?,
        metric_kind: parse_metric_kind(
            record_id,
            &required_str_property(record_id, "drift_metric_kind", get("drift_metric_kind"))?,
        )?,
        score: required_f64_text_property(record_id, "drift_score", get("drift_score"))?,
        selection_threshold: required_f64_text_property(
            record_id,
            "drift_selection_threshold",
            get("drift_selection_threshold"),
        )?,
        selection_basis: parse_selection_basis(
            record_id,
            &required_str_property(
                record_id,
                "drift_selection_basis",
                get("drift_selection_basis"),
            )?,
        )?,
    })))
}

fn required_f64_text_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<f64> {
    required_str_property(record_id, key, value)?
        .parse::<f64>()
        .map_err(|error| read_back_error(record_id, format!("{key} parse error: {error}")))
}

fn parse_metric_kind(record_id: &str, metric: &str) -> AdapterResult<MetricKind> {
    match metric {
        "cosine_distance" => Ok(MetricKind::CosineDistance),
        "l2_distance" => Ok(MetricKind::L2Distance),
        "learned_delta_v1" => Ok(MetricKind::LearnedDeltaV1),
        _ => Err(read_back_error(
            record_id,
            format!("unknown semantic drift metric_kind {metric}"),
        )),
    }
}

fn parse_selection_basis(record_id: &str, basis: &str) -> AdapterResult<SelectionBasis> {
    match basis {
        "threshold_only" => Ok(SelectionBasis::ThresholdOnly),
        "top_k_per_pair" => Ok(SelectionBasis::TopKPerPair),
        "top_k_per_symbol" => Ok(SelectionBasis::TopKPerSymbol),
        _ => Err(read_back_error(
            record_id,
            format!("unknown semantic drift selection_basis {basis}"),
        )),
    }
}

fn parse_node_kind(record_id: &str, kind: &str) -> AdapterResult<NodeKind> {
    match kind {
        "Repository" => Ok(NodeKind::Repository),
        "File" => Ok(NodeKind::File),
        "Module" => Ok(NodeKind::Module),
        "Symbol" => Ok(NodeKind::Symbol),
        "Import" => Ok(NodeKind::Import),
        "Diagnostic" => Ok(NodeKind::Diagnostic),
        "PanicRiskSite" => Ok(NodeKind::PanicRiskSite),
        "DebtMarker" => Ok(NodeKind::DebtMarker),
        "UnsafeSite" => Ok(NodeKind::UnsafeSite),
        "Commit" => Ok(NodeKind::Commit),
        "Change" => Ok(NodeKind::Change),
        "SemanticDrift" => Ok(NodeKind::SemanticDrift),
        "EmbeddingModel" => Ok(NodeKind::EmbeddingModel),
        "EmbeddingVector" => Ok(NodeKind::EmbeddingVector),
        "Agent" => Ok(NodeKind::Agent),
        "AgentSession" => Ok(NodeKind::AgentSession),
        "Observation" => Ok(NodeKind::Observation),
        "Task" => Ok(NodeKind::Task),
        "AcceptanceCriterion" => Ok(NodeKind::AcceptanceCriterion),
        "ExternalLink" => Ok(NodeKind::ExternalLink),
        "Product" => Ok(NodeKind::Product),
        "Project" => Ok(NodeKind::Project),
        "Plan" => Ok(NodeKind::Plan),
        "GitHubIssue" => Ok(NodeKind::GitHubIssue),
        "PR" => Ok(NodeKind::PR),
        "Review" => Ok(NodeKind::Review),
        "LocalTask" => Ok(NodeKind::LocalTask),
        "Artifact" => Ok(NodeKind::Artifact),
        "Verification" => Ok(NodeKind::Verification),
        "CommandEvidence" => Ok(NodeKind::CommandEvidence),
        "AgentRun" => Ok(NodeKind::AgentRun),
        "AgentTurn" => Ok(NodeKind::AgentTurn),
        "ToolCall" => Ok(NodeKind::ToolCall),
        "CommandRun" => Ok(NodeKind::CommandRun),
        "FileEdit" => Ok(NodeKind::FileEdit),
        "PatchArtifact" => Ok(NodeKind::PatchArtifact),
        "Failure" => Ok(NodeKind::Failure),
        "Decision" => Ok(NodeKind::Decision),
        "TestRun" => Ok(NodeKind::TestRun),
        "CIStatus" => Ok(NodeKind::CIStatus),
        "BenchmarkRun" => Ok(NodeKind::BenchmarkRun),
        "CoverageReport" => Ok(NodeKind::CoverageReport),
        "ProofResult" => Ok(NodeKind::ProofResult),
        "PromoteCandidate" => Ok(NodeKind::PromoteCandidate),
        "PromotionPrompt" => Ok(NodeKind::PromotionPrompt),
        "PromotionDecision" => Ok(NodeKind::PromotionDecision),
        "Preference" => Ok(NodeKind::Preference),
        "WorkflowRule" => Ok(NodeKind::WorkflowRule),
        "NamingDecision" => Ok(NodeKind::NamingDecision),
        "Constraint" => Ok(NodeKind::Constraint),
        "CostUsage" => Ok(NodeKind::CostUsage),
        "Retraction" => Ok(NodeKind::Retraction),
        "DependencyDeclaration" => Ok(NodeKind::DependencyDeclaration),
        // Log-signature node kinds (issues #319 / #320).
        "LogSource" => Ok(NodeKind::LogSource),
        "ErrorSignature" => Ok(NodeKind::ErrorSignature),
        "LogEvent" => Ok(NodeKind::LogEvent),
        "LogOccurrenceBucket" => Ok(NodeKind::LogOccurrenceBucket),
        _ => Err(read_back_error(
            record_id,
            format!("unknown embedded node kind {kind}"),
        )),
    }
}

fn parse_edge_label(record_id: &str, label: &str) -> AdapterResult<EdgeLabel> {
    match label {
        "CONTAINS" => Ok(EdgeLabel::Contains),
        "DEFINES" => Ok(EdgeLabel::Defines),
        "IMPORTS" => Ok(EdgeLabel::Imports),
        "REFERENCES" => Ok(EdgeLabel::References),
        "CALLS" => Ok(EdgeLabel::Calls),
        "IMPLEMENTS" => Ok(EdgeLabel::Implements),
        "MENTIONS" => Ok(EdgeLabel::Mentions),
        "CHANGED_IN" => Ok(EdgeLabel::ChangedIn),
        "PARENT_OF" => Ok(EdgeLabel::ParentOf),
        "DRIFTS_FROM" => Ok(EdgeLabel::DriftsFrom),
        "DRIFTS_PRIOR" => Ok(EdgeLabel::DriftsPrior),
        "MEASURED_BY" => Ok(EdgeLabel::MeasuredBy),
        "SESSION_OF" => Ok(EdgeLabel::SessionOf),
        "AUTHORED_BY" => Ok(EdgeLabel::AuthoredBy),
        "HAS_EVIDENCE" => Ok(EdgeLabel::HasEvidence),
        "OBSERVES" => Ok(EdgeLabel::Observes),
        "MENTIONS_SYMBOL" => Ok(EdgeLabel::MentionsSymbol),
        "TOUCHED_FILE" => Ok(EdgeLabel::TouchedFile),
        "PRODUCED_PATCH" => Ok(EdgeLabel::ProducedPatch),
        "PRODUCED_EVIDENCE" => Ok(EdgeLabel::ProducedEvidence),
        "VALIDATED_BY" => Ok(EdgeLabel::ValidatedBy),
        "CLOSES_ACCEPTANCE_CRITERION" => Ok(EdgeLabel::ClosesAcceptanceCriterion),
        "OWNED_BY_TASK" => Ok(EdgeLabel::OwnedByTask),
        "EXTERNAL_HANDLE" => Ok(EdgeLabel::ExternalHandle),
        "TOUCHES_FILE" => Ok(EdgeLabel::TouchesFile),
        "MERGED_AS" => Ok(EdgeLabel::MergedAs),
        "FAILED_ON" => Ok(EdgeLabel::FailedOn),
        "EXPLAINS_CHANGE" => Ok(EdgeLabel::ExplainsChange),
        "REFERENCES_TASK" => Ok(EdgeLabel::ReferencesTask),
        "CONTRADICTS" => Ok(EdgeLabel::Contradicts),
        "SUPERSEDES" => Ok(EdgeLabel::Supersedes),
        "PROPOSED_BY" => Ok(EdgeLabel::ProposedBy),
        "PROMPTED_FOR" => Ok(EdgeLabel::PromptedFor),
        "DECIDED_ON" => Ok(EdgeLabel::DecidedOn),
        "MATERIALIZED_AS" => Ok(EdgeLabel::MaterializedAs),
        "REVOKED_BY" => Ok(EdgeLabel::RevokedBy),
        "SCOPED_TO_REPO" => Ok(EdgeLabel::ScopedToRepo),
        "RELATES_TO" => Ok(EdgeLabel::RelatesTo),
        // Log-signature edge labels (issues #319 / #320).
        "FINGERPRINTED_AS" => Ok(EdgeLabel::FingerprintedAs),
        "CAPTURED_FROM" => Ok(EdgeLabel::CapturedFrom),
        "AGGREGATES" => Ok(EdgeLabel::Aggregates),
        "FRAME_RESOLVES_TO" => Ok(EdgeLabel::FrameResolvesTo),
        "EMITTED_DURING" => Ok(EdgeLabel::EmittedDuring),
        _ => Err(read_back_error(
            record_id,
            format!("unknown embedded edge label {label}"),
        )),
    }
}

fn base_properties(
    id: &str,
    record_type: &str,
    schema_version: u32,
    summary: &str,
) -> ::aletheiadb::PropertyMapBuilder {
    ::aletheiadb::PropertyMapBuilder::new()
        .insert("codegraph_id", id)
        .insert("record_type", record_type)
        .insert("schema_version", i64::from(schema_version))
        .insert("summary", summary)
}

fn insert_optional(
    builder: ::aletheiadb::PropertyMapBuilder,
    key: &str,
    value: Option<&str>,
) -> ::aletheiadb::PropertyMapBuilder {
    if let Some(value) = value {
        builder.insert(key, value)
    } else {
        builder
    }
}

#[cfg(feature = "embeddings")]
fn span_from_properties<'a, F>(get: F) -> Option<SourceSpan>
where
    F: Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
{
    let start_byte = usize::try_from(get("start_byte")?.as_int()?).ok()?;
    let end_byte = usize::try_from(get("end_byte")?.as_int()?).ok()?;
    let start_line = usize::try_from(get("start_line")?.as_int()?).ok()?;
    let end_line = usize::try_from(get("end_line")?.as_int()?).ok()?;
    Some(SourceSpan {
        start_byte,
        end_byte,
        start_line,
        end_line,
    })
}

fn insert_span(
    builder: ::aletheiadb::PropertyMapBuilder,
    span: SourceSpan,
) -> ::aletheiadb::PropertyMapBuilder {
    builder
        .insert(
            "start_byte",
            i64::try_from(span.start_byte).unwrap_or(i64::MAX),
        )
        .insert("end_byte", i64::try_from(span.end_byte).unwrap_or(i64::MAX))
        .insert(
            "start_line",
            i64::try_from(span.start_line).unwrap_or(i64::MAX),
        )
        .insert("end_line", i64::try_from(span.end_line).unwrap_or(i64::MAX))
}

fn insert_temporal(
    mut builder: ::aletheiadb::PropertyMapBuilder,
    temporal: Option<&crate::ir::TemporalMetadata>,
) -> ::aletheiadb::PropertyMapBuilder {
    if let Some(temporal) = temporal {
        builder = builder
            .insert("git_commit", temporal.git_commit.as_str())
            .insert("valid_time", temporal.valid_time.as_str())
            .insert("observed_at", temporal.observed_at.as_str());
        if let Some(author_time) = &temporal.author_time {
            builder = builder.insert("author_time", author_time.as_str());
        }
        if !temporal.git_parent_commits.is_empty() {
            builder = builder.insert("git_parent_commits", temporal.git_parent_commits.join(" "));
        }
        if let Some(source) = &temporal.valid_time_source {
            builder = builder.insert("valid_time_source", source.as_str());
        }
    }
    builder
}

fn insert_semantic_drift(
    mut builder: ::aletheiadb::PropertyMapBuilder,
    drift: Option<&crate::ir::SemanticDriftMetadata>,
) -> ::aletheiadb::PropertyMapBuilder {
    if let Some(drift) = drift {
        let score = drift.score.to_string();
        let threshold = drift.selection_threshold.to_string();
        builder = builder
            .insert(
                "embedding_model_provider",
                drift.embedding_model.provider.as_str(),
            )
            .insert("embedding_model_name", drift.embedding_model.name.as_str())
            .insert(
                "embedding_model_version",
                drift.embedding_model.version.as_str(),
            )
            .insert("embedding_model_dim", i64::from(drift.embedding_model.dim))
            .insert(
                "embedding_model_content_hash",
                drift.embedding_model.content_hash.as_str(),
            )
            .insert("drift_target_record_id", drift.target_record_id.as_str())
            .insert("drift_prior_record_id", drift.prior_record_id.as_str())
            .insert("before_git_commit", drift.before_git_commit.as_str())
            .insert("after_git_commit", drift.after_git_commit.as_str())
            .insert("before_valid_time", drift.before_valid_time.as_str())
            .insert("after_valid_time", drift.after_valid_time.as_str())
            .insert("drift_metric_kind", drift.metric_kind.as_str())
            .insert("drift_score", score.as_str())
            .insert("drift_selection_threshold", threshold.as_str())
            .insert("drift_selection_basis", drift.selection_basis.as_str());
    }
    builder
}

const fn node_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Repository
        | NodeKind::File
        | NodeKind::Module
        | NodeKind::Symbol
        | NodeKind::Import
        | NodeKind::Diagnostic
        | NodeKind::PanicRiskSite
        | NodeKind::DebtMarker
        | NodeKind::UnsafeSite
        | NodeKind::Commit
        | NodeKind::Change
        | NodeKind::SemanticDrift
        | NodeKind::EmbeddingModel
        | NodeKind::EmbeddingVector
        | NodeKind::Agent
        | NodeKind::AgentSession
        | NodeKind::Observation
        | NodeKind::Task
        | NodeKind::AcceptanceCriterion
        | NodeKind::ExternalLink
        | NodeKind::Product
        | NodeKind::Project
        | NodeKind::Plan
        | NodeKind::GitHubIssue
        | NodeKind::PR
        | NodeKind::Review
        | NodeKind::LocalTask
        | NodeKind::Artifact
        | NodeKind::Verification
        | NodeKind::CommandEvidence
        | NodeKind::AgentRun
        | NodeKind::AgentTurn
        | NodeKind::ToolCall
        | NodeKind::CommandRun
        | NodeKind::FileEdit
        | NodeKind::PatchArtifact
        | NodeKind::Failure
        | NodeKind::Decision
        | NodeKind::TestRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult
        | NodeKind::PromoteCandidate
        | NodeKind::PromotionPrompt
        | NodeKind::PromotionDecision
        | NodeKind::Preference
        | NodeKind::WorkflowRule
        | NodeKind::NamingDecision
        | NodeKind::Constraint
        | NodeKind::CostUsage
        | NodeKind::Retraction
        | NodeKind::DependencyDeclaration
        | NodeKind::LogSource
        | NodeKind::ErrorSignature
        | NodeKind::LogEvent
        | NodeKind::LogOccurrenceBucket => kind.as_str(),
    }
}

#[allow(dead_code)]
const fn _edge_label(label: EdgeLabel) -> &'static str {
    label.as_str()
}

/// Acquires the exclusive embedded write lease for `data_dir`.
///
/// A lease held by another live writer maps to the structured
/// [`AdapterError::Contended`] contract (issue #200): the refusal names the
/// holder when identifiable and always names the remedy, and no partial or
/// interleaved write is performed. Real I/O failures stay
/// [`AdapterError::Rejected`].
fn acquire_write_lease(data_dir: &Path) -> AdapterResult<StoreLease> {
    match StoreLease::try_acquire(data_dir) {
        Ok(Some(lease)) => Ok(lease),
        Ok(None) => Err(AdapterError::Contended {
            data_dir: data_dir.display().to_string(),
            message: write_lease_contention_message(data_dir),
        }),
        Err(error) => Err(AdapterError::Rejected {
            record_id: "embedded-store".to_owned(),
            message: error.to_string(),
        }),
    }
}

/// Builds the contention diagnosis for a held write lease.
///
/// When runtime metadata identifies a running daemon as the holder, the
/// message says so and points writes at the daemon adapter. Otherwise the
/// holder is an unidentified live embedded peer and the message names both
/// remedies: route concurrent writers through the daemon, or retry after the
/// current writer releases the store.
fn write_lease_contention_message(data_dir: &Path) -> String {
    let dir = data_dir.display();
    crate::daemon::live_daemon_holder_hint(data_dir).map_or_else(
        || {
            format!(
                "another live writer holds the exclusive embedded write lease for store {dir}; \
                 no write was performed. Remedy: route concurrent writers through the daemon \
                 (`eg daemon start --data-dir {dir}`, then re-run with `--adapter daemon`), or \
                 retry after the current writer releases the store"
            )
        },
        |holder| {
            format!(
                "{holder} holds the exclusive embedded write lease for store {dir}; \
                 no write was performed. Remedy: route this write through the daemon \
                 (re-run with `--adapter daemon`), or stop it \
                 (`eg daemon stop --data-dir {dir}`) and retry"
            )
        },
    )
}

fn is_fresh_data_dir(data_dir: &Path) -> bool {
    match fs::read_dir(data_dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

#[cfg(feature = "embeddings")]
fn semantic_candidate_fetch_limits(limit: usize, total_nodes: usize) -> Vec<usize> {
    if limit == 0 || total_nodes == 0 {
        return Vec::new();
    }

    let initial_limit = limit
        .saturating_mul(SEMANTIC_INITIAL_CANDIDATE_MULTIPLIER)
        .max(limit)
        .min(total_nodes);
    let max_limit = limit
        .saturating_mul(SEMANTIC_MAX_CANDIDATE_MULTIPLIER)
        .max(initial_limit)
        .min(total_nodes);

    let mut limits = vec![initial_limit];
    let mut current = initial_limit;
    while current < max_limit {
        current = current.saturating_mul(2).min(max_limit);
        if limits.last().copied() == Some(current) {
            break;
        }
        limits.push(current);
    }
    limits
}

/// Test-only concurrency gate for embedded stores.
///
/// Each embedded `AletheiaDB` store runs a `GroupCommit` background flush
/// thread whose write-acknowledgement path has a hard ~10s timeout (a
/// "deadlock detector", not a performance SLA). Under heavy parallel test load
/// — especially when the disk is near-full and `fsync` stalls — that timeout
/// can fire and surface as a spurious `"Group commit timeout"` WAL write
/// failure even though nothing is actually deadlocked.
///
/// This gate bounds how many embedded stores are open at once during in-crate
/// tests, and serialises store opens further when free disk space is low, so
/// the flush threads stay schedulable and `fsync` pressure stays bounded. It
/// is compiled only for `cargo test` of this crate; production code paths and
/// the standalone binary never see it.
#[cfg(test)]
mod embedded_store_gate {
    use std::sync::{Condvar, Mutex};

    /// Default ceiling on concurrently open embedded stores.
    const MAX_CONCURRENT: usize = 2;
    /// Ceiling used when free disk space is below `LOW_DISK_THRESHOLD_BYTES`.
    const MAX_CONCURRENT_LOW_DISK: usize = 1;
    /// Free-space threshold (2 GiB) below which store opens are serialised.
    const LOW_DISK_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

    static OPEN_STORES: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

    /// RAII permit; releases its slot when the owning store is dropped.
    pub struct StorePermit;

    impl Drop for StorePermit {
        fn drop(&mut self) {
            let (lock, cvar) = &OPEN_STORES;
            if let Ok(mut count) = lock.lock() {
                *count = count.saturating_sub(1);
                cvar.notify_one();
            }
        }
    }

    /// Blocks until an embedded-store slot is available, then claims it.
    ///
    /// No single in-crate test holds two stores at once (the exclusive store
    /// lease forces sequential open/reopen on a data dir), so a permit can
    /// never self-deadlock even at a limit of one.
    pub fn acquire() -> StorePermit {
        let limit = if low_disk() {
            MAX_CONCURRENT_LOW_DISK
        } else {
            MAX_CONCURRENT
        };
        let (lock, cvar) = &OPEN_STORES;
        let mut count = lock.lock().expect("store gate mutex poisoned");
        while *count >= limit {
            count = cvar.wait(count).expect("store gate condvar poisoned");
        }
        *count += 1;
        StorePermit
    }

    fn low_disk() -> bool {
        available_bytes().is_some_and(|bytes| bytes < LOW_DISK_THRESHOLD_BYTES)
    }

    /// Best-effort free-space probe for the temp filesystem used by store
    /// fixtures. Returns `None` (treated as "not low") when it cannot be
    /// determined, so an unknown environment never over-serialises.
    #[cfg(unix)]
    fn available_bytes() -> Option<u64> {
        use std::process::Command;
        let dir = std::env::temp_dir();
        let output = Command::new("df").arg("-kP").arg(&dir).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        // Header line, then one data line whose 4th column is available 1K blocks.
        let avail_kb: u64 = text
            .lines()
            .nth(1)?
            .split_whitespace()
            .nth(3)?
            .parse()
            .ok()?;
        Some(avail_kb.saturating_mul(1024))
    }

    #[cfg(not(unix))]
    fn available_bytes() -> Option<u64> {
        None
    }

    #[test]
    fn permits_bound_concurrent_stores() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;
        use std::time::Duration;

        // Each thread holds a permit briefly; the peak number of permits this
        // test ever holds at once must not exceed the gate ceiling, even though
        // it shares the global gate with any concurrently running store tests.
        let peak = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        // All threads must be spawned before any is joined, otherwise the
        // permits would be acquired and released one at a time.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let peak = Arc::clone(&peak);
            let live = Arc::clone(&live);
            handles.push(thread::spawn(move || {
                let _permit = acquire();
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.join().expect("gate test thread should not panic");
        }
        assert!(
            peak.load(Ordering::SeqCst) <= MAX_CONCURRENT,
            "gate must bound concurrently held permits to at most {MAX_CONCURRENT}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{GraphRecord, SourceSpan, TemporalMetadata, stable_id};

    /// Issue #200 AC2: a second embedded writer is refused with the typed
    /// contention error while a live embedded peer holds the write lease, and
    /// the store opens normally once the peer releases it.
    #[test]
    fn second_embedded_open_is_refused_with_contended_error_then_recovers() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("contended-store");
        let first =
            EmbeddedAletheiaSink::open(&data_dir).expect("first embedded open should succeed");

        let error = EmbeddedAletheiaSink::open(&data_dir)
            .err()
            .expect("second concurrent embedded open must be refused");
        let AdapterError::Contended {
            data_dir: contended_dir,
            message,
        } = &error
        else {
            panic!("live-peer contention must be typed AdapterError::Contended, got: {error:?}");
        };
        assert_eq!(contended_dir, &data_dir.display().to_string());
        assert!(
            message.contains("--adapter daemon"),
            "contention error must name the daemon remedy: {message}"
        );
        assert!(
            message.contains("retry"),
            "contention error must name the retry remedy: {message}"
        );
        assert!(
            error
                .to_string()
                .starts_with(crate::adapters::STORE_CONTENDED_CODE),
            "contention display must carry the stable machine code: {error}"
        );

        drop(first);
        EmbeddedAletheiaSink::open(&data_dir)
            .expect("embedded open must succeed after the peer releases the lease");
    }

    /// Issue #200 AC3: an embedded write attempt while a live daemon holds the
    /// exclusive lease is refused with the same contention contract, and the
    /// error names the daemon holder so the remedy is unambiguous.
    #[test]
    fn contention_error_names_live_daemon_when_running_metadata_exists() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("daemon-held-store");
        let _lease = crate::daemon::StoreLease::acquire(&data_dir)
            .expect("test should hold the store lease like a live daemon");
        let runtime_dir = crate::daemon::runtime_dir_for_data_dir(&data_dir);
        std::fs::write(
            runtime_dir.join("egregored.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "pid": 4_242,
                "address": "127.0.0.1:9",
                "token": "contention-test-token",
                "data_dir": data_dir,
                "version": "test",
                "started_at_unix_ms": 0_u64,
                "state": "running",
            }))
            .expect("daemon metadata should serialize"),
        )
        .expect("daemon metadata should write");

        let error = EmbeddedAletheiaSink::open(&data_dir)
            .err()
            .expect("embedded open must be refused while a live daemon holds the lease");
        let AdapterError::Contended { message, .. } = &error else {
            panic!("live-daemon contention must be typed AdapterError::Contended, got: {error:?}");
        };
        assert!(
            message.contains("egregored daemon") && message.contains("4242"),
            "contention error must name the live daemon holder: {message}"
        );
        assert!(
            message.contains("--adapter daemon"),
            "contention error must name the daemon remedy: {message}"
        );
    }

    #[test]
    fn open_rebuilds_endpoint_index_from_persisted_nodes() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("endpoint-index-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let older_symbol = symbol_record(
            &symbol_id,
            "older observed symbol",
            temporal_observed("zzzzzzzz", "2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z"),
        );
        let later_symbol = symbol_record(
            &symbol_id,
            "later observed symbol",
            temporal_observed("aaaaaaaa", "2026-01-01T00:00:00Z", "2026-01-01T00:00:02Z"),
        );
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        sink.write_record(&older_symbol)
            .expect("older symbol should write");
        sink.write_record(&later_symbol)
            .expect("later symbol should write");
        sink.persist_indexes()
            .expect("embedded indexes should persist");
        drop(sink);

        let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");

        assert_eq!(reopened.node_lookup.candidate_count(&symbol_id), 2);
        assert!(
            reopened
                .node_lookup
                .node_for_commit(&symbol_id, "aaaaaaaa")
                .is_some(),
            "reopened sink should resolve exact commit endpoints from a rebuilt index"
        );
        assert!(
            reopened
                .node_lookup
                .node_for_commit(&symbol_id, "zzzzzzzz")
                .is_some(),
            "reopened sink should index every persisted temporal observation once"
        );
    }

    #[test]
    fn duplicate_non_temporal_nodes_replace_endpoint_index_candidate() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("duplicate-current-node-store");
        let file_id = stable_id(&["node", "file", "src/lib.rs"]);
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let original_file = file_record(&file_id, "original current file");
        let updated_file = file_record(&file_id, "updated current file");
        let original_symbol = current_symbol_record(&symbol_id, "original current symbol", 20);
        let updated_symbol = current_symbol_record(&symbol_id, "updated current symbol", 42);
        let edge = GraphRecord::edge(
            EdgeLabel::Defines,
            file_id,
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "updated current edge".to_owned(),
        );
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        sink.write_record(&original_file)
            .expect("original file should write");
        sink.write_record(&original_symbol)
            .expect("original symbol should write");
        sink.write_record(&updated_file)
            .expect("updated file should write");
        sink.write_record(&updated_symbol)
            .expect("updated symbol should write");
        let StoredRecord::Node(updated_symbol_node_id) = sink.record_handles[updated_symbol.id()]
        else {
            panic!("updated symbol handle should point at a node");
        };

        assert_eq!(
            sink.node_lookup.latest_node(&symbol_id),
            Some(updated_symbol_node_id)
        );

        sink.write_record(&edge).expect("edge should write");
        let StoredRecord::Edge(edge_id) = sink.record_handles[edge.id()] else {
            panic!("edge handle should point at an edge");
        };
        let edge_target = sink
            .db
            .get_edge_target(edge_id)
            .expect("edge target should be readable");

        assert_eq!(edge_target, updated_symbol_node_id);
    }

    #[test]
    fn edge_resolution_status_round_trips_through_the_embedded_store() {
        // Issue #152: cross-file CALLS edges carry a `resolution` status; the
        // embedded adapter must persist it and read it back unchanged.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("resolution-round-trip-store");
        let file_id = stable_id(&["node", "file", "src/lib.rs"]);
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let edge = GraphRecord::edge(
            EdgeLabel::Calls,
            file_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "caller calls stable (cross-file, resolved)".to_owned(),
        )
        .with_resolution(crate::ir::CallResolution::Resolved);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        sink.write_record(&file_record(&file_id, "current file"))
            .expect("file should write");
        sink.write_record(&current_symbol_record(&symbol_id, "current symbol", 20))
            .expect("symbol should write");
        sink.write_record(&edge).expect("edge should write");
        let StoredRecord::Edge(edge_id) = sink.record_handles[edge.id()] else {
            panic!("edge handle should point at an edge");
        };
        let read_back = sink
            .read_edge_record(edge.id(), edge_id)
            .expect("edge should read back");

        assert_eq!(
            read_back.resolution(),
            Some(crate::ir::CallResolution::Resolved),
            "resolution status must survive the embedded round trip"
        );
        assert_eq!(read_back, edge, "edge record must round-trip byte-for-byte");
    }

    #[test]
    fn identical_non_temporal_node_write_is_noop() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("duplicate-exact-node-store");
        let file_id = stable_id(&["node", "file", "src/lib.rs"]);
        let record = file_record(&file_id, "current file");
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        sink.write_record(&record)
            .expect("first write should succeed");
        sink.write_record(&record)
            .expect("identical current write should be a no-op");

        assert_eq!(sink.node_lookup.candidate_count(&file_id), 1);
    }

    #[test]
    fn read_all_records_including_superseded_surfaces_prior_non_temporal_versions() {
        // Issue #66: a re-ingest of the same non-temporal stable ID keeps the
        // prior physical version in the store. Current-state reads collapse to the
        // latest, but the transaction-time read path must surface both so a prior
        // store view can be reconstructed.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("superseded-read-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let v1 = current_symbol_record(&symbol_id, "v1", 20)
            .with_transaction_time("2026-01-01T00:00:00Z");
        let v2 = current_symbol_record(&symbol_id, "v2", 42)
            .with_transaction_time("2026-01-03T00:00:00Z");
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&v2).expect("v2 should write");

        let count_symbol_versions = |records: &[GraphRecord]| {
            records
                .iter()
                .filter(|r| r.id() == symbol_id && r.node_kind_name() == Some("Symbol"))
                .count()
        };

        let current = sink.read_all_records().expect("current read");
        assert_eq!(
            count_symbol_versions(&current),
            1,
            "current-state read collapses to the latest version"
        );

        let history = sink
            .read_all_records_including_superseded()
            .expect("history read");
        assert_eq!(
            count_symbol_versions(&history),
            2,
            "history-inclusive read surfaces the superseded prior version"
        );
        // Both transaction-time stamps are present in the history-inclusive read.
        let tx_stamps: BTreeSet<String> = history
            .iter()
            .filter(|r| r.id() == symbol_id)
            .filter_map(|r| crate::query::record_transaction_time(r).map(ToOwned::to_owned))
            .collect();
        assert!(
            tx_stamps.contains("2026-01-01T00:00:00Z")
                && tx_stamps.contains("2026-01-03T00:00:00Z"),
            "both prior and current transaction times must be present, got {tx_stamps:?}"
        );
    }

    /// Issue #333: the six PR-promoted flat `Task` fields survive an embedded
    /// write/read round-trip verbatim (draft as bool; the rest as strings).
    #[test]
    fn pr_promoted_task_fields_survive_embedded_round_trip() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("pr-fields-store");
        let mut task = GraphRecord::node(
            "project:v1:pr-333-task".to_owned(),
            NodeKind::Task,
            None,
            None,
            Some("Promote PR fields".to_owned()),
            "github_pr #333".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            source_kind,
            head_sha,
            head_ref,
            base_ref,
            merge_commit_sha,
            merged_at,
            draft,
            ..
        } = &mut task
        {
            *schema_version = crate::ir::PROJECT_SCHEMA_VERSION;
            *domain = Some("project".to_owned());
            *source_kind = Some("github_pr".to_owned());
            *head_sha = Some("headsha333".to_owned());
            *head_ref = Some("feature-333".to_owned());
            *base_ref = Some("main".to_owned());
            *merge_commit_sha = Some("mergesha333".to_owned());
            *merged_at = Some("2026-07-10T00:00:00Z".to_owned());
            *draft = Some(true);
        }

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&task).expect("task should write");
        sink.persist_indexes().expect("indexes should persist");
        drop(sink);

        let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("store should reopen");
        let records = reopened.read_all_records().expect("read back");
        let GraphRecord::Node {
            head_sha,
            head_ref,
            base_ref,
            merge_commit_sha,
            merged_at,
            draft,
            ..
        } = records
            .iter()
            .find(|r| r.id() == "project:v1:pr-333-task")
            .expect("PR task read back")
        else {
            panic!("read-back record should be a node");
        };
        assert_eq!(head_sha.as_deref(), Some("headsha333"));
        assert_eq!(head_ref.as_deref(), Some("feature-333"));
        assert_eq!(base_ref.as_deref(), Some("main"));
        assert_eq!(merge_commit_sha.as_deref(), Some("mergesha333"));
        assert_eq!(merged_at.as_deref(), Some("2026-07-10T00:00:00Z"));
        assert_eq!(*draft, Some(true));
    }

    #[test]
    fn read_all_records_including_superseded_orders_equal_tx_versions_by_write_order() {
        // Issue #66: two non-temporal versions of one stable ID can share a
        // transaction_time (e.g. a batch ingest reusing a single stamp). A
        // `--tx-as-of` at or after that instant must resolve to the LATEST write,
        // not the superseded row that happens to share the timestamp. The
        // history-inclusive read therefore emits versions in write (`egregore_seq`)
        // order so the resolver's input-order tie-break prefers the latest write.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("equal-tx-order-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let shared_tx = "2026-01-02T00:00:00Z";
        let v1 = current_symbol_record(&symbol_id, "v1", 20).with_transaction_time(shared_tx);
        let v2 = current_symbol_record(&symbol_id, "v2", 42).with_transaction_time(shared_tx);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&v2).expect("v2 should write");

        let history = sink
            .read_all_records_including_superseded()
            .expect("history read");
        let result =
            crate::query::symbol_as_of_transaction_time(&history, "stable", shared_tx, None, None)
                .expect("tx query ok");
        assert_eq!(result.records.len(), 1, "one current version per stable ID");
        let end_byte = match result.records[0] {
            GraphRecord::Node {
                span: Some(span), ..
            } => span.end_byte,
            _ => panic!("expected a Symbol node carrying a span"),
        };
        assert_eq!(
            end_byte, 42,
            "equal-transaction-time tie must resolve to the latest write (v2), not the superseded v1"
        );
    }

    #[test]
    fn history_inclusive_read_surfaces_tombstoned_non_temporal_node() {
        // Issue #66 (#628): an active tombstone hides a non-temporal node from the
        // current-state read, but a transaction-time view that predates the
        // deletion must still see the pre-delete node (the tx resolver ignores
        // tombstones).
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("tombstoned-history-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let symbol = current_symbol_record(&symbol_id, "live", 20)
            .with_transaction_time("2026-01-01T00:00:00Z");
        let tombstone = GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &symbol_id]),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        };
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&symbol).expect("symbol should write");
        sink.write_record(&tombstone)
            .expect("tombstone should write");

        let node_present = |records: &[GraphRecord]| {
            records
                .iter()
                .any(|r| matches!(r, GraphRecord::Node { id, .. } if id == &symbol_id))
        };

        let current = sink.read_all_records().expect("current read");
        assert!(
            !node_present(&current),
            "current-state read hides the tombstoned node"
        );

        let history = sink
            .read_all_records_including_superseded()
            .expect("history read");
        assert!(
            node_present(&history),
            "history-inclusive read must surface the pre-delete node for prior tx views"
        );
    }

    #[test]
    fn history_inclusive_read_orders_active_tombstone_after_deleted_node_versions() {
        // Issue #205: consumers of the history-inclusive read (evidence
        // freshness) infer whether a tombstone is active or superseded from
        // slice order, mirroring the append-only JSONL contract where slice
        // order is write order. The read must therefore emit an active
        // tombstone AFTER every re-emitted physical version of its deleted
        // stable ID — emitting the tombstone in the current-state prefix while
        // the deleted node versions sort into the `egregore_seq` suffix makes
        // a genuine deletion look like a restoration.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("tombstone-order-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let tombstone_id = stable_id(&["tombstone", &symbol_id]);

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        // Two writes of the same stable ID, then a delete: write order is
        // v1 → v2 → tombstone.
        sink.write_record(&current_symbol_record(&symbol_id, "v1", 20))
            .expect("v1 should write");
        sink.write_record(&current_symbol_record(&symbol_id, "v2", 42))
            .expect("v2 should write");
        sink.write_record(&GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        })
        .expect("tombstone should write");

        let history = sink
            .read_all_records_including_superseded()
            .expect("history read");
        let last_node_idx = history
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, GraphRecord::Node { id, .. } if id == &symbol_id))
            .map(|(idx, _)| idx)
            .max()
            .expect("deleted node versions must still be emitted for tx views");
        let tombstone_idx = history
            .iter()
            .position(
                |r| matches!(r, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == &symbol_id),
            )
            .expect("active tombstone must be emitted");
        assert!(
            tombstone_idx > last_node_idx,
            "active tombstone (idx {tombstone_idx}) must be emitted after every physical \
             version of its deleted ID (last at idx {last_node_idx}) so slice order matches \
             write order"
        );
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn semantic_candidate_fetch_limits_are_bounded_multiples_of_query_limit() {
        assert_eq!(
            semantic_candidate_fetch_limits(0, 1_000),
            Vec::<usize>::new()
        );
        assert_eq!(semantic_candidate_fetch_limits(10, 0), Vec::<usize>::new());
        assert_eq!(
            semantic_candidate_fetch_limits(10, 1_000_000),
            vec![80, 160, 320, 640],
            "large stores should not fetch the full corpus for a small semantic limit"
        );
        assert_eq!(
            semantic_candidate_fetch_limits(10, 100),
            vec![80, 100],
            "small stores may exhaust the corpus only after the bounded initial window"
        );
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn node_rewrite_without_embedding_map_preserves_previous_latest_vector() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("preserve-vector-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "preserve-vector"]);
        let original = current_symbol_record(&symbol_id, "original semantic symbol", 20);
        let updated = current_symbol_record(&symbol_id, "updated semantic symbol", 80);
        let mut vectors = EmbeddingVectorMap::new();
        vectors.insert(
            EmbeddingVectorKey::from_record(&original).expect("symbol should be embeddable"),
            vec![1.0, 0.0],
        );
        let mut sink = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
            .expect("semantic store should open");

        sink.write_record(&original)
            .expect("original symbol should write with an embedding");
        sink.embedding_vectors.clear();
        sink.write_record(&updated)
            .expect("updated symbol should write without a fresh embedding");

        let latest_node_id = sink
            .node_lookup
            .latest_node(&symbol_id)
            .expect("latest node should be indexed");
        let latest = sink
            .db
            .get_node(latest_node_id)
            .expect("latest node should be readable");
        assert_eq!(
            latest
                .get_property("embedding")
                .and_then(::aletheiadb::PropertyValue::as_vector),
            Some(&[1.0, 0.0][..]),
            "rewritten latest nodes should inherit prior semantic coverage"
        );
    }

    /// Issue #231: after `eg forget`, the semantic/vector lane must stop
    /// returning the retracted record, exactly like the structural lanes.
    #[cfg(feature = "embeddings")]
    #[test]
    fn semantic_search_excludes_retracted_records() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("retraction-semantic-store");
        let obs_id = crate::ir::agent_memory_stable_id(&["node", "observation", "sess-231", "0"]);
        let mut obs = GraphRecord::node(
            obs_id.clone(),
            NodeKind::Observation,
            None,
            None,
            Some("obs".to_owned()),
            "agent observation".to_owned(),
        );
        if let GraphRecord::Node {
            ref mut schema_version,
            ref mut text,
            ..
        } = obs
        {
            *schema_version = crate::ir::AGENT_MEMORY_SCHEMA_VERSION;
            *text = Some("the parser silently skips empty input".to_owned());
        }
        let mut vectors = EmbeddingVectorMap::new();
        vectors.insert(
            EmbeddingVectorKey::from_record(&obs).expect("observation should be embeddable"),
            vec![1.0, 0.0],
        );
        let mut sink = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
            .expect("semantic store should open");
        sink.write_record(&obs).expect("observation should write");

        let hits = sink
            .semantic_search(&[1.0, 0.0], 5)
            .expect("semantic search should run");
        assert!(
            hits.iter().any(|hit| hit.record_id == obs_id),
            "the observation should be a semantic hit before retraction"
        );

        let records = sink.read_all_records().expect("store should read");
        let request = crate::forget::ForgetRequest {
            handle: obs_id.clone(),
            reason: "false claim about parser behavior".to_owned(),
            retracted_by: "op-1".to_owned(),
            transaction_time: Some("2026-07-01T00:00:00Z".to_owned()),
        };
        let crate::forget::ForgetOutcome::Retracted {
            records: generated, ..
        } = crate::forget::retract_from_records(&records, &request)
            .expect("observation should retract")
        else {
            panic!("expected a Retracted outcome");
        };
        for record in &generated {
            sink.write_record(record)
                .expect("retraction records should write");
        }

        let hits = sink
            .semantic_search(&[1.0, 0.0], 5)
            .expect("semantic search should run");
        assert!(
            hits.iter().all(|hit| hit.record_id != obs_id),
            "retracted records must not surface through the vector lane: {hits:?}"
        );
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn node_id_for_observation_uses_full_temporal_identity() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("full-temporal-identity-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "same-commit"]);
        let first_temporal =
            temporal_observed("aaaaaaaa", "2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z");
        let second_temporal =
            temporal_observed("aaaaaaaa", "2026-01-01T00:00:00Z", "2026-01-01T00:00:02Z");
        let first = symbol_record(
            &symbol_id,
            "first same-commit observation",
            first_temporal.clone(),
        );
        let second = symbol_record(
            &symbol_id,
            "second same-commit observation",
            second_temporal.clone(),
        );
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        sink.write_record(&first)
            .expect("first observation should write");
        sink.write_record(&second)
            .expect("second observation should write");

        let first_node = node_id_for_temporal_properties(
            &sink,
            &symbol_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
        );
        let second_node = node_id_for_temporal_properties(
            &sink,
            &symbol_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:02Z",
        );

        assert_ne!(
            first_node, second_node,
            "fixture should create two physical observations for the same commit"
        );
        assert_eq!(
            sink.node_id_for_observation(&symbol_id, Some(&first_temporal)),
            Some(first_node),
            "semantic observation lookup must resolve the first bitemporal identity"
        );
        assert_eq!(
            sink.node_id_for_observation(&symbol_id, Some(&second_temporal)),
            Some(second_node),
            "semantic observation lookup must resolve the second bitemporal identity"
        );
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn embedding_backfill_uses_full_temporal_observation_identity() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("same-commit-observation-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "same-commit"]);
        let same_commit = "aaaaaaaa";
        let first_temporal =
            temporal_observed(same_commit, "2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z");
        let second_temporal =
            temporal_observed(same_commit, "2026-01-01T00:00:00Z", "2026-01-01T00:00:02Z");
        let first = symbol_record(&symbol_id, "first same-commit observation", first_temporal);
        let second = symbol_record(
            &symbol_id,
            "second same-commit observation",
            second_temporal,
        );

        {
            let mut structural =
                EmbeddedAletheiaSink::open(&data_dir).expect("structural store should open");
            structural
                .write_record(&first)
                .expect("first observation should write");
            structural
                .write_record(&second)
                .expect("second observation should write");
            structural
                .persist_indexes()
                .expect("structural indexes should persist");
        }

        let mut vectors = EmbeddingVectorMap::new();
        vectors.insert(
            EmbeddingVectorKey::from_record(&first).expect("symbol should be embeddable"),
            vec![1.0, 0.0],
        );
        let mut semantic = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
            .expect("semantic store should reopen");
        semantic
            .write_record(&first)
            .expect("matched first observation should be backfilled");

        let first_node = node_id_for_temporal_properties(
            &semantic,
            &symbol_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
        );
        let second_node = node_id_for_temporal_properties(
            &semantic,
            &symbol_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:02Z",
        );
        let first_embedding = semantic
            .db
            .get_node(first_node)
            .expect("first node should be readable")
            .get_property("embedding")
            .and_then(::aletheiadb::PropertyValue::as_vector)
            .map(<[f32]>::to_vec);
        let second_embedding = semantic
            .db
            .get_node(second_node)
            .expect("second node should be readable")
            .get_property("embedding")
            .and_then(::aletheiadb::PropertyValue::as_vector)
            .map(<[f32]>::to_vec);

        assert_eq!(first_embedding.as_deref(), Some(&[1.0, 0.0][..]));
        assert_eq!(
            second_embedding, None,
            "backfill must not write a vector to a different observation from the same commit"
        );
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn temporal_write_without_embedding_map_does_not_inherit_prior_commit_vector() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("temporal-unseen-commit-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "new-commit"]);
        let original = symbol_record(
            &symbol_id,
            "original semantic symbol",
            temporal_observed("aaaaaaaa", "2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z"),
        );
        let new_commit = symbol_record(
            &symbol_id,
            "new commit without fresh embedding",
            temporal_observed("bbbbbbbb", "2026-01-02T00:00:00Z", "2026-01-02T00:00:01Z"),
        );
        let mut vectors = EmbeddingVectorMap::new();
        vectors.insert(
            EmbeddingVectorKey::from_record(&original).expect("symbol should be embeddable"),
            vec![1.0, 0.0],
        );
        let mut sink = EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, 2)
            .expect("semantic store should open");

        sink.write_record(&original)
            .expect("original observation should write with an embedding");
        sink.embedding_vectors.clear();
        sink.write_record(&new_commit)
            .expect("new temporal observation should write without a fresh embedding");

        let new_node = node_id_for_temporal_properties(
            &sink,
            &symbol_id,
            "2026-01-02T00:00:00Z",
            "2026-01-02T00:00:01Z",
        );
        let new_embedding = sink
            .db
            .get_node(new_node)
            .expect("new commit node should be readable")
            .get_property("embedding")
            .and_then(::aletheiadb::PropertyValue::as_vector)
            .map(<[f32]>::to_vec);

        assert_eq!(
            new_embedding, None,
            "non-embed temporal writes must not inherit stale vectors from prior commits"
        );
    }

    #[test]
    fn read_back_until_honors_expired_deadline_before_edge_scan() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("deadline-store");
        let file_id = stable_id(&["node", "file", "src/lib.rs"]);
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let edge = GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "current edge".to_owned(),
        );
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&file_record(&file_id, "file"))
            .expect("file should write");
        sink.write_record(&current_symbol_record(&symbol_id, "symbol", 10))
            .expect("symbol should write");
        sink.write_record(&edge).expect("edge should write");

        let error = sink
            .read_back_until("codegraph:v3:missing-edge", Some(Instant::now()))
            .expect_err("expired deadline should stop the edge scan");
        assert!(matches!(error, AdapterError::TimedOut { .. }));
    }

    fn file_record(id: &str, summary: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            summary.to_owned(),
        )
    }

    fn current_symbol_record(id: &str, summary: &str, end_byte: usize) -> GraphRecord {
        GraphRecord::symbol(
            id.to_owned(),
            "function",
            "src/lib.rs".to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte,
                start_line: 1,
                end_line: 1,
            },
            "stable".to_owned(),
            summary.to_owned(),
        )
    }

    fn symbol_record(id: &str, summary: &str, temporal: TemporalMetadata) -> GraphRecord {
        GraphRecord::symbol(
            id.to_owned(),
            "function",
            "src/lib.rs".to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 20,
                start_line: 1,
                end_line: 1,
            },
            "stable".to_owned(),
            summary.to_owned(),
        )
        .with_temporal(temporal)
    }

    #[test]
    fn read_all_records_includes_historical_observations_of_tombstoned_records() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("tombstone-history-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "gone"]);
        let tombstone_id = stable_id(&["tombstone", &symbol_id]);

        let historical_symbol = symbol_record(
            &symbol_id,
            "symbol that will be deleted",
            temporal_observed("deadbeef", "2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z"),
        );
        let tombstone = GraphRecord::Tombstone {
            id: tombstone_id.clone(),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        };

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&historical_symbol)
            .expect("temporal symbol should write");
        sink.write_record(&tombstone)
            .expect("tombstone should write");

        let records = sink
            .read_all_records()
            .expect("read_all_records should succeed");

        // The historical temporal observation must be present so --at <commit> can resolve it
        let has_historical = records.iter().any(|r| {
            matches!(
                r,
                GraphRecord::Node { id, temporal: Some(t), .. }
                    if id == &symbol_id && t.git_commit == "deadbeef"
            )
        });
        assert!(
            has_historical,
            "historical observation of tombstoned record must appear in read_all_records"
        );

        // The tombstone itself must still appear
        let has_tombstone = records
            .iter()
            .any(|r| matches!(r, GraphRecord::Tombstone { id, .. } if id == &tombstone_id));
        assert!(
            has_tombstone,
            "tombstone record must appear in read_all_records"
        );
    }

    #[test]
    fn read_all_records_includes_restored_node_when_tombstone_is_stale() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("restoration-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "restored"]);
        let tombstone_id = stable_id(&["tombstone", &symbol_id, "v1"]);

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        // 1. Write original node
        sink.write_record(&current_symbol_record(&symbol_id, "original", 10))
            .expect("original symbol should write");
        // 2. Write tombstone marking it deleted
        sink.write_record(&GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        })
        .expect("tombstone should write");
        // 3. Re-ingest the same record (restoration) — new AletheiaDB node, higher NodeId
        sink.write_record(&current_symbol_record(&symbol_id, "restored", 20))
            .expect("restored symbol should write");

        let records = sink
            .read_all_records()
            .expect("read_all_records should succeed");

        let has_live_node = records
            .iter()
            .any(|r| matches!(r, GraphRecord::Node { id, .. } if id == &symbol_id));
        assert!(
            has_live_node,
            "restored node must appear in read_all_records after stale tombstone"
        );
        // Stale tombstone must NOT be in output (so CLI deleted_id filter doesn't erase the node)
        let stale_tombstone_emitted = records.iter().any(
            |r| matches!(r, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == &symbol_id),
        );
        assert!(
            !stale_tombstone_emitted,
            "stale tombstone must not appear in read_all_records output"
        );
    }

    /// `read_back_current_until` is the direct-lookup analog of
    /// `read_all_records` (issue #231): an actively tombstoned record
    /// resolves to `None`, the tombstone itself stays fetchable, and a
    /// record revived by a later re-ingest (stale tombstone) resolves again.
    #[test]
    fn read_back_current_suppresses_actively_tombstoned_record() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("current-read-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "retracted"]);
        let tombstone_id = stable_id(&["tombstone", &symbol_id, "current-read"]);

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&current_symbol_record(&symbol_id, "original", 10))
            .expect("symbol should write");
        sink.write_record(&GraphRecord::Tombstone {
            id: tombstone_id.clone(),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        })
        .expect("tombstone should write");

        // Physical read-back still sees the bytes (write verification lane)…
        assert!(
            sink.read_back(&symbol_id)
                .expect("read_back succeeds")
                .is_some(),
            "physical read_back keeps returning the latest bytes"
        );
        // …but the current-view lookup suppresses the record.
        assert!(
            sink.read_back_current_until(&symbol_id, None)
                .expect("current read succeeds")
                .is_none(),
            "actively tombstoned record must not resolve on the current view"
        );
        // The tombstone itself stays fetchable: it is part of the current view.
        assert!(
            matches!(
                sink.read_back_current_until(&tombstone_id, None)
                    .expect("tombstone read succeeds"),
                Some(GraphRecord::Tombstone { .. })
            ),
            "active tombstone record must stay fetchable"
        );

        // Reviving the record supersedes the tombstone: current view resolves again.
        sink.write_record(&current_symbol_record(&symbol_id, "restored", 20))
            .expect("restored symbol should write");
        assert!(
            sink.read_back_current_until(&symbol_id, None)
                .expect("current read succeeds")
                .is_some(),
            "a record revived past a stale tombstone resolves on the current view"
        );
    }

    /// A re-issued tombstone must keep suppressing its target across store
    /// reopens. Reopen leaves two physical tombstone nodes with the same
    /// record ID; `rebuild_lookup_indexes` must index the latest one
    /// regardless of storage iteration order, or the staleness comparison
    /// resurrects the target nondeterministically. Each scope mirrors one CLI
    /// process (open, write, persist, drop).
    #[test]
    fn reissued_tombstone_still_suppresses_target_after_reopen() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("reopen-re-retraction-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "reopened"]);
        let tombstone = GraphRecord::Tombstone {
            id: stable_id(&["tombstone", &symbol_id, "reopened"]),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        };
        {
            let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("open 1");
            // A second node keeps the store off the single-node NodeId(0)
            // path so ID generation continues monotonically across reopens.
            let other_id = stable_id(&["node", "symbol", "src/lib.rs", "reopened-other"]);
            sink.write_record(&current_symbol_record(&other_id, "other", 5))
                .expect("write other");
            sink.write_record(&current_symbol_record(&symbol_id, "original", 10))
                .expect("write original");
            sink.persist_indexes().expect("persist 1");
        }
        {
            let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("open 2");
            sink.write_record(&tombstone).expect("write tombstone");
            sink.persist_indexes().expect("persist 2");
        }
        {
            let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("open 3");
            sink.write_record(&current_symbol_record(&symbol_id, "revived", 20))
                .expect("write revived");
            sink.persist_indexes().expect("persist 3");
        }
        {
            let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("open 4");
            sink.write_record(&tombstone)
                .expect("re-issue the byte-identical tombstone");
            sink.persist_indexes().expect("persist 4");
        }
        {
            let sink = EmbeddedAletheiaSink::open(&data_dir).expect("open 5");
            let records = sink.read_all_records().expect("read");
            let node_live = records
                .iter()
                .any(|r| matches!(r, GraphRecord::Node { id, .. } if id == &symbol_id));
            assert!(
                !node_live,
                "the revived record must stay suppressed after reopen"
            );
            let tombstone_active = records.iter().any(
                |r| matches!(r, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == &symbol_id),
            );
            assert!(
                tombstone_active,
                "the re-issued tombstone must be emitted as active after reopen"
            );
        }
    }

    #[test]
    fn write_tombstone_reissues_stale_tombstone_after_reingest() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("re-retraction-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "re_retracted"]);
        let tombstone_id = stable_id(&["tombstone", &symbol_id, "v1"]);
        let tombstone = GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "deleted".to_owned(),
            producer: None,
        };

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        // 1. Write original node, then tombstone it.
        sink.write_record(&current_symbol_record(&symbol_id, "original", 10))
            .expect("original symbol should write");
        sink.write_record(&tombstone)
            .expect("tombstone should write");
        // 2. Re-ingest the record: the newer write supersedes the tombstone,
        //    so the record is live again and the stored tombstone is stale.
        sink.write_record(&current_symbol_record(&symbol_id, "revived", 20))
            .expect("revived symbol should write");
        // 3. Re-issue the byte-identical tombstone (the `eg forget` repair
        //    path). The identical-content Matched no-op must not apply to a
        //    stale tombstone: the write has to land as a fresh, active
        //    deletion marker.
        sink.write_record(&tombstone)
            .expect("re-issued tombstone should write");

        let records = sink
            .read_all_records()
            .expect("read_all_records should succeed");
        let node_live = records
            .iter()
            .any(|r| matches!(r, GraphRecord::Node { id, .. } if id == &symbol_id));
        assert!(
            !node_live,
            "a re-issued tombstone must suppress the revived record again"
        );
        let tombstone_active = records.iter().any(
            |r| matches!(r, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == &symbol_id),
        );
        assert!(
            tombstone_active,
            "the re-issued tombstone must be emitted as active"
        );
    }

    #[test]
    fn read_all_records_includes_reingested_edge_when_tombstone_superseded() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("edge-restoration-store");
        let file_id = stable_id(&["node", "file", "src/lib.rs"]);
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "reingested"]);
        let edge = GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "original edge".to_owned(),
        );
        let edge_id = edge.id().to_owned();
        let tombstone_id = stable_id(&["tombstone", &edge_id]);
        // Re-ingested edge: same source/target/label (same codegraph_id) but different summary
        // → write_edge creates a new AletheiaDB edge (count becomes 2)
        let reingested_edge = GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "reingested edge".to_owned(),
        );

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&file_record(&file_id, "file"))
            .expect("file should write");
        sink.write_record(&current_symbol_record(&symbol_id, "symbol", 10))
            .expect("symbol should write");
        sink.write_record(&edge).expect("edge should write");
        sink.write_record(&GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: edge_id.clone(),
            summary: "edge deleted".to_owned(),
            producer: None,
        })
        .expect("tombstone should write");
        sink.write_record(&reingested_edge)
            .expect("reingested edge should write");

        let records = sink
            .read_all_records()
            .expect("read_all_records should succeed");

        let has_edge = records
            .iter()
            .any(|r| matches!(r, GraphRecord::Edge { id, .. } if id == &edge_id));
        assert!(
            has_edge,
            "re-ingested edge must appear in read_all_records when tombstone is superseded"
        );
    }

    #[test]
    fn read_all_records_revives_edge_on_identical_reemit_after_tombstone() {
        // Revive-after-tombstone through the embedded CURRENT read view (#333,
        // Codex round-7): a merge resolution that cycles resolved-A →
        // unresolved/B → resolved-A re-emits the SAME edge bytes + stable id as
        // the first run. Across a PERSISTENT store reopened each phase, the third
        // (byte-identical) re-emit must revive the tombstoned id — otherwise the
        // matching physical edge is short-circuited, the tombstone stays latest,
        // and `read_all_records` keeps suppressing the re-resolved merge link.
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("edge-revive-identical-store");
        let task_id = stable_id(&["node", "task", "pr:7"]);
        let commit_id = stable_id(&["node", "commit", "sha-a"]);
        // MERGED_AS edge to commit A. Identical bytes are reconstructed below.
        let edge = GraphRecord::edge(
            EdgeLabel::MergedAs,
            task_id.clone(),
            commit_id.clone(),
            Some("1.0".to_owned()),
            "PR #7 merged as commit sha-a".to_owned(),
        );
        let edge_id = edge.id().to_owned();
        let tombstone_id = stable_id(&["tombstone", &edge_id]);

        // Phase 1: resolved-A — endpoints + live edge E_A.
        {
            let mut sink =
                EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
            sink.write_record(&file_record(&task_id, "task"))
                .expect("task node should write");
            sink.write_record(&current_symbol_record(&commit_id, "commit", 10))
                .expect("commit node should write");
            sink.write_record(&edge).expect("edge should write");
        }

        // Phase 2: A → unresolved/B — tombstone E_A. It must now be suppressed.
        {
            let mut sink =
                EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
            sink.write_record(&GraphRecord::Tombstone {
                id: tombstone_id,
                schema_version: crate::ir::SCHEMA_VERSION,
                deleted_id: edge_id.clone(),
                summary: "merge resolution superseded".to_owned(),
                producer: None,
            })
            .expect("tombstone should write");
            let records = sink
                .read_all_records()
                .expect("read_all_records should succeed");
            assert!(
                !records
                    .iter()
                    .any(|r| matches!(r, GraphRecord::Edge { id, .. } if id == &edge_id)),
                "edge must be suppressed while its id is actively tombstoned"
            );
        }

        // Phase 3: unresolved/B → resolved-A — re-emit IDENTICAL E_A bytes.
        {
            let mut sink =
                EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
            let reemitted = GraphRecord::edge(
                EdgeLabel::MergedAs,
                task_id,
                commit_id,
                Some("1.0".to_owned()),
                "PR #7 merged as commit sha-a".to_owned(),
            );
            assert_eq!(
                reemitted.id(),
                edge_id,
                "re-emit must reconstruct the same id"
            );
            sink.write_record(&reemitted)
                .expect("identical edge re-emit should write");
            let records = sink
                .read_all_records()
                .expect("read_all_records should succeed");
            assert!(
                records
                    .iter()
                    .any(|r| matches!(r, GraphRecord::Edge { id, .. } if id == &edge_id)),
                "byte-identical re-emit must revive the tombstoned merge edge in the current view"
            );
        }
    }

    #[test]
    fn read_all_records_tombstoned_edge_not_in_output() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("tombstoned-edge-store");
        let file_id = stable_id(&["node", "file", "src/lib.rs"]);
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "edge_target"]);
        let edge = GraphRecord::edge(
            EdgeLabel::Defines,
            file_id.clone(),
            symbol_id.clone(),
            Some("1.0".to_owned()),
            "file defines symbol".to_owned(),
        );
        let edge_id = edge.id().to_owned();
        let tombstone_id = stable_id(&["tombstone", &edge_id]);

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&file_record(&file_id, "file"))
            .expect("file should write");
        sink.write_record(&current_symbol_record(&symbol_id, "symbol", 10))
            .expect("symbol should write");
        sink.write_record(&edge).expect("edge should write");
        sink.write_record(&GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: edge_id.clone(),
            summary: "edge deleted".to_owned(),
            producer: None,
        })
        .expect("tombstone should write");

        let records = sink
            .read_all_records()
            .expect("read_all_records should succeed");

        let has_edge = records
            .iter()
            .any(|r| matches!(r, GraphRecord::Edge { id, .. } if id == &edge_id));
        assert!(
            !has_edge,
            "tombstoned edge must not appear in read_all_records"
        );
    }

    #[test]
    fn read_all_records_returns_latest_edge_after_resolution_only_reingest() {
        // Issue #152 / PR #290 review: a store created before the `resolution`
        // field existed holds an unlabeled CALLS edge. Re-ingesting the same
        // edge with `resolution` set appends a second physical edge with the
        // same codegraph_id (higher egregore_seq). The read path must return
        // the latest duplicate, not the stale unlabeled one.
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("resolution-upgrade-store");
        let source_symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "caller"]);
        let target_symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "callee"]);
        let unlabeled_edge = GraphRecord::edge(
            EdgeLabel::Calls,
            source_symbol_id.clone(),
            target_symbol_id.clone(),
            Some("1.0".to_owned()),
            "caller calls callee".to_owned(),
        );
        let edge_id = unlabeled_edge.id().to_owned();
        let labeled_edge = unlabeled_edge
            .clone()
            .with_resolution(crate::ir::CallResolution::Resolved);

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&current_symbol_record(
            &source_symbol_id,
            "caller symbol",
            10,
        ))
        .expect("caller should write");
        sink.write_record(&current_symbol_record(
            &target_symbol_id,
            "callee symbol",
            10,
        ))
        .expect("callee should write");
        // Legacy store state: the CALLS edge exists without a resolution property.
        sink.write_record(&unlabeled_edge)
            .expect("unlabeled edge should write");
        // Resolution-only upgrade: expected_record_state reports Mismatched, so a
        // second physical edge is appended for the same codegraph_id.
        sink.write_record(&labeled_edge)
            .expect("labeled edge should write");
        assert_eq!(
            sink.edge_observation_count_for_test(&edge_id),
            2,
            "resolution-only re-ingest must append a second physical edge"
        );

        let assert_latest_edge_wins = |sink: &EmbeddedAletheiaSink| {
            let records = sink
                .read_all_records()
                .expect("read_all_records should succeed");
            let resolution = records
                .iter()
                .find_map(|r| match r {
                    GraphRecord::Edge { id, resolution, .. } if id == &edge_id => Some(*resolution),
                    _ => None,
                })
                .expect("edge must appear in read_all_records");
            assert_eq!(
                resolution,
                Some(crate::ir::CallResolution::Resolved),
                "read_all_records must return the latest edge write (highest egregore_seq), \
                 not the stale unlabeled duplicate"
            );
        };
        assert_latest_edge_wins(&sink);

        // Reopen: latest-duplicate selection must survive an index rebuild from
        // persisted properties.
        drop(sink);
        let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
        assert_latest_edge_wins(&sink);
    }

    fn temporal_observed(
        git_commit: &str,
        valid_time: &str,
        observed_at: &str,
    ) -> TemporalMetadata {
        TemporalMetadata {
            git_commit: git_commit.to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: valid_time.to_owned(),
            author_time: Some(valid_time.to_owned()),
            observed_at: observed_at.to_owned(),
            valid_time_source: None,
        }
    }

    #[cfg(feature = "embeddings")]
    fn node_id_for_temporal_properties(
        sink: &EmbeddedAletheiaSink,
        record_id: &str,
        valid_time: &str,
        observed_at: &str,
    ) -> ::aletheiadb::NodeId {
        for node_id in sink.db.get_all_node_ids() {
            let node = sink.db.get_node(node_id).expect("node should be readable");
            if node
                .get_property("codegraph_id")
                .and_then(::aletheiadb::PropertyValue::as_str)
                == Some(record_id)
                && node
                    .get_property("valid_time")
                    .and_then(::aletheiadb::PropertyValue::as_str)
                    == Some(valid_time)
                && node
                    .get_property("observed_at")
                    .and_then(::aletheiadb::PropertyValue::as_str)
                    == Some(observed_at)
            {
                return node_id;
            }
        }
        panic!(
            "node {record_id} with valid_time {valid_time} observed_at {observed_at} should exist"
        );
    }

    #[test]
    fn inspect_all_records_tolerates_future_node_kind() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("future-kind-store");
        let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        let record_id = "codegraph:v6:future-kind-repo";
        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", record_id)
            .insert("record_type", "node")
            .insert("kind", "NewFutureKind")
            .insert("schema_version", 6i64)
            .insert("domain", "codegraph")
            .build();

        sink.db
            .create_node("Repository", properties)
            .expect("should create raw node");

        let report = sink
            .inspect_all_records()
            .expect("inspect_all_records should succeed");
        assert_eq!(report.records.len(), 0);
        assert_eq!(report.unknown_schema_versions.len(), 1);
        let unknown = &report.unknown_schema_versions[0];
        assert_eq!(unknown.version.domain, "codegraph");
        assert_eq!(unknown.version.kind, "NewFutureKind");
        assert_eq!(unknown.version.version, 6);
    }

    #[test]
    fn inspect_all_records_fails_on_corrupt_record_of_known_version() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("corrupt-node-store");
        let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        let record_id = "codegraph:v4:corrupt-node";
        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", record_id)
            .insert("record_type", "node")
            // missing "kind", but has known schema_version
            .insert("schema_version", i64::from(crate::ir::SCHEMA_VERSION))
            .insert("domain", "codegraph")
            .build();

        sink.db
            .create_node("Repository", properties)
            .expect("should create raw node");

        let res = sink.inspect_all_records();
        assert!(
            res.is_err(),
            "Expected inspect_all_records to fail on corrupt record of known version, got {res:?}"
        );
    }

    /// Issue #231 (round 6): the current serving view must collapse physical
    /// versions to the latest write per stable ID. A re-ingested record
    /// leaves its superseded prior version in the store; the physical
    /// inventory (`inspect_all_records`) keeps reporting both, but the
    /// current view must serialize exactly one — the latest.
    #[test]
    fn inspect_current_records_collapses_superseded_versions() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("superseded-current-view-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "superseded"]);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&current_symbol_record(&symbol_id, "original version", 10))
            .expect("original symbol should write");
        sink.write_record(&current_symbol_record(&symbol_id, "updated version", 42))
            .expect("updated symbol should write");

        // Physical inventory keeps every version (embedded `eg inspect --data-dir`).
        let all = sink
            .inspect_all_records()
            .expect("inspect_all_records should succeed");
        assert_eq!(
            all.records
                .iter()
                .filter(|record| record.id() == symbol_id)
                .count(),
            2,
            "physical inventory must keep both versions: {all:?}"
        );

        // The current serving view collapses to the latest write.
        let current = sink
            .inspect_current_records()
            .expect("inspect_current_records should succeed");
        let versions: Vec<_> = current
            .records
            .iter()
            .filter(|record| record.id() == symbol_id)
            .collect();
        assert_eq!(
            versions.len(),
            1,
            "current view must serialize exactly one version per stable ID: {current:?}"
        );
        assert!(
            matches!(
                versions[0],
                GraphRecord::Node { span: Some(span), .. } if span.end_byte == 42
            ),
            "current view must serialize the latest version, got {:?}",
            versions[0]
        );
    }

    /// Issue #231 (round 6): after a tombstoned record is revived by a later
    /// re-ingest, the tombstone is stale and no longer suppresses the stable
    /// ID. The current serving view must then emit only the restored version
    /// — never the pre-retraction physical version — and must drop the stale
    /// tombstone (mirroring `read_all_records`, so downstream `deleted_id`
    /// filters cannot re-suppress the revived record).
    #[test]
    fn inspect_current_records_never_serializes_pre_retraction_version_after_revive() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("revive-current-view-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "revived"]);
        let tombstone_id = stable_id(&["tombstone", &symbol_id, "revive-current-view"]);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&current_symbol_record(&symbol_id, "retracted version", 10))
            .expect("original symbol should write");
        sink.write_record(&GraphRecord::Tombstone {
            id: tombstone_id.clone(),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: symbol_id.clone(),
            summary: "retracted".to_owned(),
            producer: None,
        })
        .expect("tombstone should write");
        sink.write_record(&current_symbol_record(&symbol_id, "restored version", 42))
            .expect("restored symbol should write");

        let current = sink
            .inspect_current_records()
            .expect("inspect_current_records should succeed");
        let versions: Vec<_> = current
            .records
            .iter()
            .filter(|record| record.id() == symbol_id)
            .collect();
        assert_eq!(
            versions.len(),
            1,
            "revived record must appear exactly once on the current view: {current:?}"
        );
        assert!(
            matches!(
                versions[0],
                GraphRecord::Node { span: Some(span), .. } if span.end_byte == 42
            ),
            "current view must serialize the restored version, never the \
             pre-retraction one, got {:?}",
            versions[0]
        );
        assert!(
            !current
                .records
                .iter()
                .any(|record| record.id() == tombstone_id),
            "stale tombstone must not be re-served on the current view: {current:?}"
        );
    }

    /// The current serving view keeps the physical inventory's tolerance for
    /// unknown `(domain, kind, schema_version)` tuples: they are tallied,
    /// never deserialized and never an error.
    #[test]
    fn inspect_current_records_tolerates_future_node_kind() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("future-kind-current-view-store");
        let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        let record_id = "codegraph:v6:future-kind-repo";
        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", record_id)
            .insert("record_type", "node")
            .insert("kind", "NewFutureKind")
            .insert("schema_version", 6i64)
            .insert("domain", "codegraph")
            .build();

        sink.db
            .create_node("Repository", properties)
            .expect("should create raw node");

        let report = sink
            .inspect_current_records()
            .expect("inspect_current_records should succeed");
        assert_eq!(report.records.len(), 0);
        assert_eq!(report.unknown_schema_versions.len(), 1);
        let unknown = &report.unknown_schema_versions[0];
        assert_eq!(unknown.version.domain, "codegraph");
        assert_eq!(unknown.version.kind, "NewFutureKind");
        assert_eq!(unknown.version.version, 6);
    }
}
