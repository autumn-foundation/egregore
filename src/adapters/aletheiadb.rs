//! Embedded `AletheiaDB` adapter.

use std::{collections::BTreeMap, fs, path::Path, time::Instant};

use chrono::{DateTime, Utc};

use crate::{
    adapters::{AdapterError, AdapterResult, ExpectedRecordState, GraphSink},
    daemon::StoreLease,
    identity::{is_local_remote_url, repository_id_matches_payload},
    ir::{
        EdgeLabel, EvidenceLink, GraphRecord, IdentitySource, NodeKind, SemanticDriftMetadata,
        SourceSpan, TemporalMetadata,
    },
};

/// A single result from a semantic similarity search.
#[derive(Debug, Clone)]
pub struct SemanticMatch {
    /// Stable codegraph record ID.
    pub record_id: String,
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
    embedding_vectors: BTreeMap<String, Vec<f32>>,
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
    /// Returns an error when the store is already leased or `AletheiaDB` cannot
    /// open the requested data dir.
    pub fn open(data_dir: impl AsRef<Path>) -> AdapterResult<Self> {
        let data_dir = data_dir.as_ref();
        let lease = StoreLease::acquire(data_dir).map_err(|error| AdapterError::Rejected {
            record_id: "embedded-store".to_owned(),
            message: error.to_string(),
        })?;
        Self::open_inner(data_dir, Some(lease))
    }

    pub(crate) fn open_unleased(data_dir: impl AsRef<Path>) -> AdapterResult<Self> {
        let data_dir = data_dir.as_ref();
        Self::open_inner(data_dir, None)
    }

    fn open_inner(data_dir: &Path, lease: Option<StoreLease>) -> AdapterResult<Self> {
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
        };
        sink.rebuild_lookup_indexes()?;
        Ok(sink)
    }

    /// Opens a store and pre-loads embedding vectors so they are stored in each
    /// node during ingest. Enables an HNSW vector index on the `"embedding"`
    /// property for semantic search.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened or the vector index fails
    /// to initialise.
    #[cfg(feature = "embeddings")]
    pub fn open_with_embeddings(
        data_dir: impl AsRef<std::path::Path>,
        vectors: BTreeMap<String, Vec<f32>>,
        dimensions: usize,
    ) -> AdapterResult<Self> {
        let data_dir = data_dir.as_ref();
        let lease = StoreLease::acquire(data_dir).map_err(|error| AdapterError::Rejected {
            record_id: "embedded-store".to_owned(),
            message: error.to_string(),
        })?;
        let mut sink = Self::open_inner(data_dir, Some(lease))?;
        sink.embedding_vectors = vectors;
        let hnsw = ::aletheiadb::index::vector::hnsw::HnswConfig {
            dimensions,
            metric: ::aletheiadb::index::vector::DistanceMetric::Cosine,
            ..Default::default()
        };
        sink.db
            .enable_vector_index("embedding", hnsw)
            .map_err(|error| AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: error.to_string(),
            })?;
        Ok(sink)
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
        let raw = self
            .db
            .find_similar_by_embedding(query_vector, limit)
            .map_err(|error| AdapterError::ReadBack {
                record_id: "semantic-search".to_owned(),
                message: error.to_string(),
            })?;

        let mut results = Vec::with_capacity(raw.len());
        for (node_id, score) in raw {
            let Ok(node) = self.db.get_node(node_id) else {
                continue;
            };
            let record_id = node
                .get_property("codegraph_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
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
                name,
                repo_relative_path,
                score,
                span,
            });
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

        // Temporal observations: include ALL commit snapshots even for tombstoned records so
        // that `--at <commit>` queries can resolve past state after a deletion.
        for (record_id, commits) in &self.node_lookup.by_commit {
            for candidate in commits.values() {
                records.push(self.read_node_record(record_id, candidate.storage_id)?);
            }
        }

        // Non-temporal (current-state) nodes: skip records that have been tombstoned.
        for (record_id, &node_id) in &self.node_lookup.non_temporal {
            if active_tombstoned.contains(record_id.as_str()) {
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

        let mut seen_edge_ids = std::collections::BTreeSet::new();
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|error| read_back_error("read_all_records", error.to_string()))?;
                let Some(codegraph_id) = optional_str_property(
                    "read_all_records",
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                else {
                    continue;
                };
                // Deduplicate and skip tombstoned edges.
                if seen_edge_ids.insert(codegraph_id.clone())
                    && !active_tombstoned.contains(codegraph_id.as_str())
                {
                    records.push(self.read_edge_record(&codegraph_id, edge_id)?);
                }
            }
        }

        Ok(records)
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

    pub(crate) fn expected_record_state(
        &self,
        record: &GraphRecord,
    ) -> AdapterResult<ExpectedRecordState> {
        match record {
            GraphRecord::Node { id, temporal, .. } => {
                if let Some(temporal) = temporal
                    && let Some(node_id) =
                        self.node_lookup.node_for_commit(id, &temporal.git_commit)
                {
                    return self.compare_node_record(id, node_id, record);
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
            // Tombstone is stale if the node record was re-ingested after it (higher NodeId).
            let node_stale = self
                .node_lookup
                .non_temporal
                .get(deleted_id.as_str())
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
            let edge_seq = self.edge_seqs.get(deleted_id.as_str()).copied();
            let edge_stale = match (edge_seq, tombstone_seq) {
                (Some(es), Some(ts)) => es > ts,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => self
                    .legacy_edge_counts
                    .get(deleted_id.as_str())
                    .is_some_and(|&count| count > 1),
            };
            if !node_stale && !edge_stale {
                deleted.insert(deleted_id);
            }
        }
        Ok(deleted)
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
            return match self.read_back(record.id())? {
                Some(read_back) if read_back == *record => Ok(()),
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
            }
            Some("tombstone") => {
                self.tombstone_ids.insert(record_id, node_id);
            }
            Some(_) | None => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn write_node(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        if self.expected_record_state(record)? == ExpectedRecordState::Matched {
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
            temporal,
            semantic_drift,
            evidence_links,
            repository_identity,
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
            valid_time,
            valid_time_source,
            summary,
            domain,
            importer_id,
            importer_version,
            source_artifact_path,
            source_artifact_hash,
            patch_status,
            failure_kind,
            exit_code,
            turn_index,
            stdout_handle,
            stderr_handle,
            evidence_quality,
            executed_at,
            verification_kind,
            status,
        } = record
        else {
            unreachable!("write_node called with non-node record");
        };

        let mut builder =
            base_properties(id, "node", *schema_version, summary).insert("kind", kind.as_str());
        builder = insert_optional(builder, "repo_relative_path", repo_relative_path.as_deref());
        builder = insert_optional(builder, "name", name.as_deref());
        builder = insert_optional(builder, "language", language.as_deref());
        builder = insert_optional(builder, "symbol_kind", symbol_kind.as_deref());
        builder = insert_temporal(builder, temporal.as_ref());
        builder = insert_semantic_drift(builder, semantic_drift.as_deref());
        builder = insert_optional(builder, "node_valid_time", valid_time.as_deref());
        builder = insert_optional(
            builder,
            "node_valid_time_source",
            valid_time_source.as_deref(),
        );
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
        #[cfg(feature = "embeddings")]
        if let Some(vector) = self.embedding_vectors.get(id.as_str()) {
            builder = builder.insert_vector("embedding", vector);
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

    fn write_tombstone(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        if self.expected_record_state(record)? == ExpectedRecordState::Matched {
            return Ok(());
        }
        let GraphRecord::Tombstone {
            id,
            schema_version,
            deleted_id,
            summary,
        } = record
        else {
            unreachable!("write_tombstone called with non-tombstone record");
        };
        self.write_seq += 1;
        let seq = self.write_seq;
        let seq_str = seq.to_string();
        let properties = base_properties(id, "tombstone", *schema_version, summary)
            .insert("deleted_id", deleted_id.as_str())
            .insert("egregore_seq", seq_str.as_str())
            .build();
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
        if self.expected_record_state(record)? == ExpectedRecordState::Matched {
            return Ok(());
        }

        let GraphRecord::Edge {
            id,
            schema_version,
            label,
            source,
            target,
            confidence,
            temporal,
            summary,
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
        builder = insert_temporal(builder, temporal.as_ref());

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
            Some(read_back) if read_back == *record => Ok(ExpectedRecordState::Matched),
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
        match self.read_node_record(record_id, node_id)? {
            read_back if read_back == *expected => Ok(ExpectedRecordState::Matched),
            _ => Ok(ExpectedRecordState::Mismatched),
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
                    if self.read_edge_record(record_id, edge_id)? == *expected {
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
    fn read_node_record(
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

        Ok(GraphRecord::Node {
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
        })
    }

    fn read_tombstone_record(
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

        Ok(GraphRecord::Tombstone {
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
        })
    }

    fn read_edge_record(
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

        Ok(GraphRecord::Edge {
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
            temporal: temporal_from_properties(record_id, |key| edge.get_property(key))?,
            summary: required_str_property(record_id, "summary", edge.get_property("summary"))?,
        })
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
    let Some(model_id) =
        optional_str_property(record_id, "semantic_model_id", get("semantic_model_id"))?
    else {
        return Ok(None);
    };

    Ok(Some(Box::new(SemanticDriftMetadata {
        model_id,
        target_record_id: required_str_property(
            record_id,
            "drift_target_record_id",
            get("drift_target_record_id"),
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
        score: required_str_property(record_id, "drift_score", get("drift_score"))?,
    })))
}

fn parse_node_kind(record_id: &str, kind: &str) -> AdapterResult<NodeKind> {
    match kind {
        "Repository" => Ok(NodeKind::Repository),
        "File" => Ok(NodeKind::File),
        "Module" => Ok(NodeKind::Module),
        "Symbol" => Ok(NodeKind::Symbol),
        "Import" => Ok(NodeKind::Import),
        "Diagnostic" => Ok(NodeKind::Diagnostic),
        "Commit" => Ok(NodeKind::Commit),
        "Change" => Ok(NodeKind::Change),
        "SemanticDrift" => Ok(NodeKind::SemanticDrift),
        "Agent" => Ok(NodeKind::Agent),
        "AgentSession" => Ok(NodeKind::AgentSession),
        "Observation" => Ok(NodeKind::Observation),
        "Task" => Ok(NodeKind::Task),
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
        "SESSION_OF" => Ok(EdgeLabel::SessionOf),
        "AUTHORED_BY" => Ok(EdgeLabel::AuthoredBy),
        "HAS_EVIDENCE" => Ok(EdgeLabel::HasEvidence),
        "OBSERVES" => Ok(EdgeLabel::Observes),
        "MENTIONS_SYMBOL" => Ok(EdgeLabel::MentionsSymbol),
        "TOUCHED_FILE" => Ok(EdgeLabel::TouchedFile),
        "PRODUCED_PATCH" => Ok(EdgeLabel::ProducedPatch),
        "VALIDATED_BY" => Ok(EdgeLabel::ValidatedBy),
        "FAILED_ON" => Ok(EdgeLabel::FailedOn),
        "EXPLAINS_CHANGE" => Ok(EdgeLabel::ExplainsChange),
        "REFERENCES_TASK" => Ok(EdgeLabel::ReferencesTask),
        "CONTRADICTS" => Ok(EdgeLabel::Contradicts),
        "SUPERSEDES" => Ok(EdgeLabel::Supersedes),
        "RELATES_TO" => Ok(EdgeLabel::RelatesTo),
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
        builder = builder
            .insert("semantic_model_id", drift.model_id.as_str())
            .insert("drift_target_record_id", drift.target_record_id.as_str())
            .insert("before_git_commit", drift.before_git_commit.as_str())
            .insert("after_git_commit", drift.after_git_commit.as_str())
            .insert("before_valid_time", drift.before_valid_time.as_str())
            .insert("after_valid_time", drift.after_valid_time.as_str())
            .insert("drift_score", drift.score.as_str());
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
        | NodeKind::Commit
        | NodeKind::Change
        | NodeKind::SemanticDrift
        | NodeKind::Agent
        | NodeKind::AgentSession
        | NodeKind::Observation
        | NodeKind::Task
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
        | NodeKind::ProofResult => kind.as_str(),
    }
}

#[allow(dead_code)]
const fn _edge_label(label: EdgeLabel) -> &'static str {
    label.as_str()
}

fn is_fresh_data_dir(data_dir: &Path) -> bool {
    match fs::read_dir(data_dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{GraphRecord, SourceSpan, TemporalMetadata, stable_id};

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
}
