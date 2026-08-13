//! Embedded `AletheiaDB` adapter.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
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
        Producer, RouteAnnotation, SelectionBasis, SemanticDriftMetadata, SourceSpan,
        TemporalMetadata, UserContextFields,
    },
    schema_constraints::{
        ConformanceStatus, ConstraintProfile, DeclarationOutcome, DeclaredConstraint,
        DeclaredProperty, DeclaredTypeToken, DropOutcome, EntityKindToken, LabelConformance,
        ViolationRow,
    },
};
use ::aletheiadb::api::transaction::WriteOps;

#[cfg(feature = "embeddings")]
const SEMANTIC_INITIAL_CANDIDATE_MULTIPLIER: usize = 8;
#[cfg(feature = "embeddings")]
const SEMANTIC_MAX_CANDIDATE_MULTIPLIER: usize = 64;

/// Node property the semantic vector index is built over. One store has exactly
/// one such index, so this name is the whole vocabulary.
#[cfg(feature = "embeddings")]
const EMBEDDING_INDEX_PROPERTY: &str = "embedding";

/// `AletheiaDB`'s persisted per-property vector-index files, in the fixed order
/// they are reported (issue #489).
///
/// Named upstream in `AletheiaDB::rebuild_vector_index`'s contract as exactly
/// the files whose corruption causes an index to be SKIPPED at load. Matching
/// against this fixed table (rather than listing the directory) is what lets a
/// probe result be `&'static str` and therefore reach a diagnostic without any
/// operator-controlled filename riding along.
#[cfg(feature = "embeddings")]
const VECTOR_INDEX_ARTIFACT_FILES: [&str; 4] = [
    "meta.idx",
    "mappings.idx",
    "current.usearch",
    "current.usearch.mappings",
];

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
    /// Root the store was opened from, retained so the vector-index state probe
    /// (issue #489) can tell a skipped-at-load index from one that never
    /// existed. Read-only: never written through, never re-persisted.
    #[cfg_attr(not(feature = "embeddings"), allow(dead_code))]
    data_dir: PathBuf,
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
        // Pin the string-interner cap to Egregore's own constant rather than
        // inheriting AletheiaDB's default (issue #439). `MAX_INTERNED_STRINGS`
        // also bounds the ingest preflight estimate, so configuring the store
        // from the same constant makes "what we refuse" and "what the store
        // actually enforces" the same number by construction — an upstream
        // default change can no longer silently desync them. The interner is
        // process-global and read once at open, so this is a per-process budget.
        config.persistence.max_interned_strings =
            usize::try_from(super::preflight::MAX_INTERNED_STRINGS).unwrap_or(usize::MAX);
        let db = ::aletheiadb::AletheiaDB::with_unified_config(config).map_err(|error| {
            AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: classify_open_error(data_dir, &error.to_string()),
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
            data_dir: data_dir.to_path_buf(),
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
    /// index is incompatible, the store's persisted vector index exists but was
    /// skipped at load (issue #489), or the vector index fails to initialise.
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
            .find(|index| index.property_name == EMBEDDING_INDEX_PROPERTY)
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
            // The store holds persisted index files the engine skipped at load
            // (issue #489). Enabling the index here would register an EMPTY one
            // whose next persistence cycle OVERWRITES those files, permanently
            // destroying vectors that upstream's `rebuild_vector_index` could
            // otherwise have recovered — upstream documents this exact footgun.
            // So refuse before enabling anything: nothing is registered, so
            // nothing is persisted, and the damaged store stays repairable.
            if let crate::embeddings::VectorIndexState::Unreadable { artifacts } =
                probe_persisted_vector_index(data_dir, EMBEDDING_INDEX_PROPERTY)
            {
                return Err(AdapterError::Rejected {
                    record_id: "embedded-store".to_owned(),
                    message: format!(
                        "semantic_index_unreadable: this store's persisted `{EMBEDDING_INDEX_PROPERTY}` vector index \
                         exists on disk ({}) but was skipped at load as corrupted or unreadable; \
                         refusing to embed into it because enabling a fresh index over those files \
                         would overwrite them and permanently lose the indexed vectors. {}",
                        if artifacts.is_empty() {
                            "its index directory is present but holds none of the expected files"
                                .to_owned()
                        } else {
                            format!("persisted index files: {}", artifacts.join(", "))
                        },
                        crate::embeddings::SEMANTIC_INDEX_UNREADABLE_REMEDY
                    ),
                });
            }
            let hnsw = ::aletheiadb::index::vector::hnsw::HnswConfig {
                dimensions,
                metric,
                ..Default::default()
            };
            sink.db
                .enable_vector_index(EMBEDDING_INDEX_PROPERTY, hnsw)
                .map_err(|error| AdapterError::Rejected {
                    record_id: "embedded-store".to_owned(),
                    message: error.to_string(),
                })?;
        }
        Ok(sink)
    }

    /// Returns what the store's `"embedding"` vector index actually IS: loaded,
    /// present-on-disk-but-unreadable, or absent (issue #489).
    ///
    /// `AletheiaDB` 0.2.0 loads per-property vector indexes with error
    /// isolation — a corrupted or unreadable index is SKIPPED with a warning
    /// rather than aborting the load — and a skipped index is simply missing
    /// from `list_vector_indexes()`. So the engine handle alone cannot tell
    /// "never `--embed`ed" from "embedded, and the index is damaged"; the
    /// on-disk probe below supplies the missing bit. The upstream skip warning
    /// goes to the process's stderr as plain text, not through an installable
    /// observability seam, so it is not capturable here.
    ///
    /// This REPLACED a bare `embedding_index_dimensions() -> Option<usize>`
    /// accessor. The `Option` was the bug's shape: every caller had to invent a
    /// meaning for `None`, and all of them chose "never embedded". Callers that
    /// only need "can I search this?" use
    /// [`crate::embeddings::VectorIndexState::dimensions`]; callers that report
    /// to an operator must match the state.
    #[cfg(feature = "embeddings")]
    #[must_use]
    pub fn embedding_index_state(&self) -> crate::embeddings::VectorIndexState {
        use crate::embeddings::VectorIndexState;

        if let Some(index) = self
            .db
            .list_vector_indexes()
            .into_iter()
            .find(|index| index.property_name == EMBEDDING_INDEX_PROPERTY)
        {
            return VectorIndexState::Loaded {
                dimensions: index.dimensions,
            };
        }
        probe_persisted_vector_index(&self.data_dir, EMBEDDING_INDEX_PROPERTY)
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
            // `similarity_search` is AletheiaDB 0.2.0's unified vector-search
            // entry point; an embedding-sourced query with no label/time filter
            // dispatches to exactly the `find_similar_by_embedding` path this
            // call used before the upgrade, so ranking and scores are unchanged.
            let raw = self
                .db
                .similarity_search(
                    ::aletheiadb::SimilarityQuery::from_embedding(query_vector).k(raw_limit),
                )
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
            .map_err(|error| classify_store_error("embedded-store", error.to_string()))
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

    /// Maps each retracted stable ID to the HIGHEST `egregore_seq` among the
    /// tombstones that retracted it — the retraction boundary the log-retained
    /// reads enforce (issue #363).
    ///
    /// A `forget` retraction of a log-domain ID writes a tombstone. When a later
    /// `scan-logs` re-observes the SAME stable ID it appends a new physical
    /// version with a higher `egregore_seq`, superseding the tombstone; the
    /// current-state read then correctly exposes only that post-retraction
    /// observation. The append-only store still holds the pre-retraction physical
    /// version, though, so the retained log sweep must suppress any version at or
    /// below this boundary to avoid resurrecting the forgotten observation. The
    /// MAX seq per deleted ID handles a record retracted more than once.
    ///
    /// # Errors
    ///
    /// Returns an error if a physical tombstone node cannot be read.
    fn log_tombstone_boundary(&self) -> AdapterResult<BTreeMap<String, u64>> {
        let mut boundary: BTreeMap<String, u64> = BTreeMap::new();
        for &tombstone_node_id in self.tombstone_ids.values() {
            let node = self
                .db
                .get_node(tombstone_node_id)
                .map_err(|error| read_back_error("log_tombstone_boundary", error.to_string()))?;
            let Some(deleted_id) = optional_str_property(
                "log_tombstone_boundary",
                "deleted_id",
                node.get_property("deleted_id"),
            )?
            else {
                continue;
            };
            let seq = self
                .tombstone_node_seqs
                .get(&tombstone_node_id)
                .copied()
                .unwrap_or(0);
            boundary
                .entry(deleted_id)
                .and_modify(|current| *current = (*current).max(seq))
                .or_insert(seq);
        }
        Ok(boundary)
    }

    /// Returns the complete, retraction-boundary-correct set of non-temporal
    /// **log-domain** node records (`ErrorSignature` / `LogOccurrenceBucket`) for
    /// the retained read lanes — the single shared implementation behind both
    /// [`Self::read_all_records_log_retained`] and
    /// [`Self::read_all_records_including_superseded_log_retained`] (issue #363).
    ///
    /// Each lane strips its own log-domain node records and appends this set, so
    /// the two can never drift in how they retain, collapse, or suppress log
    /// observations.
    ///
    /// # What the set contains
    ///
    /// Every DISTINCT scan observation of every non-tombstoned log signature /
    /// bucket, keyed by [`log_observation_key`], keeping the LATEST physical
    /// version per `(record_id, observation_key)`. Enrichment-only rewrites
    /// (identical log payload, `FRAME_RESOLVES_TO` / `EMITTED_DURING` /
    /// `REFERENCES_TASK` evidence links added by `resolve-frames` / `link-logs`)
    /// share their scan payload's key and collapse to the single latest (enriched)
    /// version, so occurrence counts are never doubled and resolved frames /
    /// evidence links are preserved. Genuinely distinct scan payloads (differing
    /// `first_seen` / `last_seen` / `occurrence_count`, or differing #322 captured
    /// frames) have distinct keys and are all retained, so the #326 / #324
    /// cross-scan coalescers reconstruct on `--data-dir` exactly what they do on
    /// the concatenated `--graph` JSONL.
    ///
    /// # Retraction boundary (issue #363, Codex P2)
    ///
    /// A physical version whose `egregore_seq` is at or below the LATEST tombstone
    /// written for its stable ID ([`Self::log_tombstone_boundary`]) was written at
    /// or before a `forget` retraction and is suppressed: a re-scan after `forget`
    /// never resurrects a pre-retraction observation. A fully-retracted ID (an
    /// active tombstone with no later re-observation) is in `active_tombstoned`
    /// and skipped entirely, matching the current-state read.
    ///
    /// The set is sorted by `(egregore_seq, record_id)` for determinism; the log
    /// coalescers group by stable ID and are order-insensitive.
    ///
    /// # Errors
    ///
    /// Returns an error if a physical record cannot be read.
    fn retained_log_observation_records(&self) -> AdapterResult<Vec<GraphRecord>> {
        let active_tombstoned = self.active_deleted_ids()?;
        let boundary = self.log_tombstone_boundary()?;
        // Latest-seq record per (stable_id, observation_key). Distinct scan
        // payloads have distinct keys and are all kept; enrichment rewrites share
        // a key and collapse to the newest (enriched) version.
        let mut latest: BTreeMap<(String, String), (u64, GraphRecord)> = BTreeMap::new();
        // Defensive: a log-kind node with no coalesce-relevant payload cannot be
        // an enrichment duplicate of a payload-bearing observation, so retain it.
        let mut keyless: Vec<(u64, GraphRecord)> = Vec::new();
        for node_id in self.db.get_all_node_ids() {
            let node = self.db.get_node(node_id).map_err(|error| {
                read_back_error("retained_log_observation_records", error.to_string())
            })?;
            // Node records only — tombstones and edges are not "node".
            if optional_str_property(
                "retained_log_observation_records",
                "record_type",
                node.get_property("record_type"),
            )?
            .as_deref()
                != Some("node")
            {
                continue;
            }
            // Restrict to the two non-temporal log-domain kinds.
            if !matches!(
                optional_str_property(
                    "retained_log_observation_records",
                    "kind",
                    node.get_property("kind"),
                )?
                .as_deref(),
                Some("ErrorSignature" | "LogOccurrenceBucket")
            ) {
                continue;
            }
            let Some(record_id) = optional_str_property(
                "retained_log_observation_records",
                "codegraph_id",
                node.get_property("codegraph_id"),
            )?
            else {
                continue;
            };
            // Non-temporal only (log signatures/buckets are non-temporal;
            // defensive against a stray temporal log node).
            if !self.node_lookup.non_temporal.contains_key(&record_id) {
                continue;
            }
            // Fully retracted (active tombstone, no later re-observation) → skip.
            if active_tombstoned.contains(record_id.as_str()) {
                continue;
            }
            let seq = optional_str_property(
                "retained_log_observation_records",
                "egregore_seq",
                node.get_property("egregore_seq"),
            )?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
            // Retraction boundary: suppress any version written at or before the
            // latest tombstone for this stable ID (a re-scan after `forget` must
            // not resurrect a pre-retraction observation).
            if boundary
                .get(record_id.as_str())
                .is_some_and(|&max_seq| seq <= max_seq)
            {
                continue;
            }
            let record = self.read_node_record(&record_id, node_id)?;
            match log_observation_key(&record) {
                Some(key) => match latest.entry((record_id, key)) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert((seq, record));
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        if seq >= entry.get().0 {
                            *entry.get_mut() = (seq, record);
                        }
                    }
                },
                None => keyless.push((seq, record)),
            }
        }
        let mut out: Vec<(u64, GraphRecord)> = latest.into_values().collect();
        out.extend(keyless);
        out.sort_by(|(a_seq, a_record), (b_seq, b_record)| {
            a_seq
                .cmp(b_seq)
                .then_with(|| a_record.id().cmp(b_record.id()))
        });
        Ok(out.into_iter().map(|(_, record)| record).collect())
    }

    /// Like [`Self::read_all_records`], but additionally re-emits every
    /// superseded (non-current) physical version of a non-temporal **log-domain**
    /// node (`kind` `ErrorSignature` or `LogOccurrenceBucket`) that is a
    /// **distinct scan observation** (issue #363).
    ///
    /// # Why the log domain needs this
    ///
    /// `ErrorSignature` and `LogOccurrenceBucket` are non-temporal nodes whose
    /// stable ID is content-independent of the `LogSource` they were captured
    /// from: a signature's ID is `(repository, fingerprint_algorithm, template,
    /// severity)` and a bucket's is `(repository, signature, hour, width)`. A
    /// second `scan-logs` ingest of the *same* fingerprint from a *different*
    /// captured log therefore mints another physical node with the SAME stable
    /// ID but its own `first_seen` / `last_seen` / `occurrence_count`. The
    /// current-state read ([`Self::read_all_records`]) collapses those to the
    /// latest write (last-write-wins), which is exactly the duplicate slice the
    /// `--graph` cross-scan coalescers (#326 `log-deltas`, #324 `error-context`)
    /// need to reconstruct the true `first_seen`/`last_seen`/summed occurrence
    /// counts. This method surfaces the superseded physical versions so those
    /// coalescers operate on `--data-dir` exactly as they do on the concatenated
    /// `--graph` JSONL for scans whose captured content differs.
    ///
    /// # Distinct scan observations only — enrichment rewrites are not re-counted
    ///
    /// The standard pipeline `scan-logs -> resolve-frames -> link-logs` ingests
    /// the SAME `ErrorSignature` node more than once into one store: `scan-logs`
    /// writes it bare, then `resolve-frames` / `link-logs` re-emit it enriched
    /// with `FRAME_RESOLVES_TO` / `EMITTED_DURING` / `REFERENCES_TASK` evidence
    /// links while leaving the log payload (`first_seen`/`last_seen`/
    /// `occurrence_count`) untouched. That enriched node differs in content, so
    /// the write appends a new physical version — but it is the SAME scan
    /// observation, not a new one. Re-emitting it would make the coalescers SUM
    /// `occurrence_count` twice (double-counting the most common workflow).
    ///
    /// A superseded version is therefore retained only when its coalesce-relevant
    /// log-payload "observation key" ([`log_observation_key`]) differs from the
    /// current version and from every already-retained superseded version of the
    /// same stable ID. The key is derived from exactly the fields the coalescers
    /// merge (`first_seen`/`last_seen`/`occurrence_count` for signatures,
    /// `bucket_start`/`occurrence_count` for buckets) and deliberately excludes
    /// evidence links and other node metadata, so an enrichment-only rewrite maps
    /// to the same key and is dropped. The current version emitted by
    /// [`Self::read_all_records`] is always the latest write (highest node id),
    /// i.e. the enriched one, so evidence links / resolved frames are preserved.
    ///
    /// # What stays the same
    ///
    /// * Every OTHER non-temporal kind keeps its single current-state record —
    ///   only `ErrorSignature` / `LogOccurrenceBucket` versions are re-emitted —
    ///   so non-log query behaviour over `--data-dir` is byte-for-byte unchanged.
    /// * Byte-identical re-ingests of the same `scan-logs` output are deduped to
    ///   one physical node by [`GraphSink::write_record`]'s idempotent no-op path
    ///   (an unchanged non-temporal write creates no new node), so identical
    ///   re-scans do NOT multiply counts here — the one residual divergence from
    ///   the `--graph` path, where concatenating identical JSONL does.
    /// * Actively tombstoned IDs stay suppressed, matching
    ///   [`Self::read_all_records`].
    /// * A `forget`-retracted log observation is NOT resurrected by a later
    ///   re-scan: [`Self::retained_log_observation_records`] suppresses every
    ///   physical version at or below the latest tombstone for its stable ID
    ///   (the retraction boundary), so only post-retraction re-observations
    ///   survive into the coalesced sum.
    ///
    /// # Implementation
    ///
    /// The log-domain node records are stripped from the current-state read and
    /// replaced by [`Self::retained_log_observation_records`] — the single shared
    /// set both retained lanes append, so the current-state and history-inclusive
    /// variants cannot drift. The set is appended in `(egregore_seq, record_id)`
    /// order after the non-log records; the log coalescers group by stable ID and
    /// are order-insensitive, so no consumer depends on interleaving.
    ///
    /// # Errors
    ///
    /// Returns an error if a physical record cannot be read.
    pub fn read_all_records_log_retained(&self) -> AdapterResult<Vec<GraphRecord>> {
        // Strip the current-state read's log-domain node records and replace them
        // with the shared, retraction-boundary-correct set. This keeps the two
        // retained lanes byte-for-byte identical in log handling and lets the
        // helper suppress pre-retraction observations a naive superseded-version
        // sweep would resurrect (issue #363). Tombstones, edges, temporal
        // snapshots, project nodes and every non-log record pass through
        // untouched.
        let mut records: Vec<GraphRecord> = self
            .read_all_records()?
            .into_iter()
            .filter(|record| !is_log_domain_node(record))
            .collect();
        records.extend(self.retained_log_observation_records()?);
        Ok(records)
    }

    /// Like [`Self::read_all_records_including_superseded`], but collapses
    /// enrichment-only rewrites of a non-temporal **log-domain** node
    /// (`ErrorSignature` / `LogOccurrenceBucket`) to a single physical version
    /// while leaving every OTHER superseded/temporal record fully intact (issue
    /// #363).
    ///
    /// # Why the temporal lane needs its own log-retained variant
    ///
    /// The history-inclusive read
    /// ([`Self::read_all_records_including_superseded`]) is the transaction-time /
    /// valid-time reconstruction surface: it deliberately re-emits EVERY physical
    /// non-temporal node version so a `--tx-as-of` / `--at` / `--as-of` view can
    /// pick the version live at that point. That is exactly right for versioned
    /// facts, but it also re-emits the SAME log observation twice whenever the
    /// standard pipeline `scan-logs -> resolve-frames -> link-logs` rewrites an
    /// `ErrorSignature` to attach `FRAME_RESOLVES_TO` / `EMITTED_DURING` /
    /// `REFERENCES_TASK` evidence links while leaving the log payload
    /// (`first_seen`/`last_seen`/`occurrence_count`) untouched. Both physical
    /// versions share the same scan observation, so the `error-context` (#324)
    /// coalescer — which SUMS `occurrence_count` across duplicate-ID signatures —
    /// double-counts them on the `--at`/`--as-of` `--data-dir` lane, the residual
    /// hole the non-temporal [`Self::read_all_records_log_retained`] already
    /// closes for the current-state lane.
    ///
    /// # What this variant does — and does not — change
    ///
    /// This is a POST-FILTER over
    /// [`Self::read_all_records_including_superseded`], applying the SAME
    /// observation-key dedup ([`log_observation_key`]) that
    /// [`Self::read_all_records_log_retained`] uses, but ONLY to log-domain node
    /// records that carry a coalesce-relevant log payload
    /// (`ErrorSignature` / `LogOccurrenceBucket`). For each such record it keeps
    /// only the LAST occurrence of a given `(stable_id, observation_key)` pair in
    /// the returned order. The wrapped read already emits every non-temporal node
    /// version in ascending `egregore_seq` write order, so the last occurrence is
    /// the LATEST write — i.e. the ENRICHED version carrying the evidence links —
    /// and the earlier bare rewrite is dropped.
    ///
    /// Net effect:
    ///
    /// * Distinct scan OBSERVATIONS survive: two `scan-logs` ingests with
    ///   differing `first_seen`/`last_seen`/`occurrence_count` map to different
    ///   observation keys, so both are retained and the coalescer reconstructs the
    ///   true earliest/latest/summed values exactly as on `--graph`.
    /// * Enrichment-only rewrites (identical log payload, evidence links added)
    ///   collapse to their single latest (enriched) version, so occurrence counts
    ///   are never doubled and the resolved frames / evidence links are preserved.
    /// * Every NON-log record — temporal snapshots, tombstones, edges, project
    ///   nodes, superseded non-log versions — is passed through untouched, so
    ///   `--at`/`--as-of` valid-time reconstruction is byte-for-byte unaffected.
    ///   A log node carrying no coalesce-relevant payload (`LogSource` /
    ///   `LogEvent`, whose [`log_observation_key`] is `None`) is likewise passed
    ///   through unchanged.
    ///
    /// The shared [`Self::read_all_records_including_superseded`] is intentionally
    /// left untouched: its other callers (transaction-time views) require the full
    /// unfiltered version stream.
    ///
    /// # Errors
    ///
    /// Returns an error if a physical record cannot be read.
    pub fn read_all_records_including_superseded_log_retained(
        &self,
    ) -> AdapterResult<Vec<GraphRecord>> {
        // Strip the history-inclusive read's log-domain node records and replace
        // them with the SAME shared, retraction-boundary-correct set the
        // current-state lane uses, so the two lanes cannot drift (issue #363).
        // Non-log temporal snapshots, superseded non-log versions, tombstones,
        // edges and project nodes pass through untouched, so `--at` / `--as-of`
        // valid-time reconstruction is byte-for-byte unaffected. The shared
        // helper also enforces the `forget` retraction boundary, which the raw
        // history read cannot (it deliberately re-emits every physical version,
        // including pre-retraction ones, for transaction-time views).
        let mut records: Vec<GraphRecord> = self
            .read_all_records_including_superseded()?
            .into_iter()
            .filter(|record| !is_log_domain_node(record))
            .collect();
        records.extend(self.retained_log_observation_records()?);
        Ok(records)
    }

    // -----------------------------------------------------------------------
    // Schema constraints (issue #486)
    // -----------------------------------------------------------------------

    /// The store-side node labels and edge types actually present, sorted.
    ///
    /// Read from `AletheiaDB`'s own observed-schema summary in ONE call, so
    /// discovering which of the inventoried labels a store contains costs a
    /// single pass rather than one probe per label.
    ///
    /// # Errors
    ///
    /// Returns an error if the store's schema summary cannot be read.
    pub fn observed_labels(&self) -> AdapterResult<(Vec<String>, Vec<String>)> {
        let schema = self
            .db
            .schema()
            .map_err(|error| read_back_error("observed_labels", error.to_string()))?;
        let mut nodes: Vec<String> = schema
            .node_labels
            .into_iter()
            .map(|entry| entry.label)
            .collect();
        let mut edges: Vec<String> = schema
            .edge_types
            .into_iter()
            .map(|entry| entry.edge_type)
            .collect();
        nodes.sort();
        edges.sort();
        Ok((nodes, edges))
    }

    /// The schema constraints this store currently has declared, sorted.
    #[must_use]
    pub fn declared_schema_constraints(&self) -> Vec<DeclaredConstraint> {
        let mut declared: Vec<DeclaredConstraint> = self
            .db
            .list_schema_constraints()
            .into_iter()
            .map(|descriptor| {
                // The COMPLETE descriptor, not just the key: `dropped_constraints`
                // is a restorable before-image, and a foreign declaration retracted
                // by `--drop --include-foreign` is one Egregore's own `--declare`
                // can never rebuild. See `DeclaredProperty`.
                let mut properties: Vec<DeclaredProperty> = descriptor
                    .properties
                    .into_iter()
                    .map(|property| DeclaredProperty {
                        property: property.property,
                        declared_type: property
                            .declared_type
                            .map(|declared| declared.type_name().to_string()),
                        // `type_name()` collapses `Vector { dim }` to `vector`,
                        // so the pinned dimension is preserved separately.
                        vector_dim: match property.declared_type {
                            Some(::aletheiadb::core::constraint::DeclaredType::Vector { dim }) => {
                                dim
                            }
                            _ => None,
                        },
                        required: property.required,
                        nullable: property.nullable,
                    })
                    .collect();
                properties.sort_by(|a, b| a.property.cmp(&b.property));
                DeclaredConstraint {
                    entity_kind: descriptor.entity_kind,
                    label: descriptor.label,
                    properties,
                }
            })
            .collect();
        declared.sort_by(|a, b| (&a.entity_kind, &a.label).cmp(&(&b.entity_kind, &b.label)));
        declared
    }

    /// Runs the upstream `.dry_run()` conformance scan for `profile` over every
    /// label in `labels`, returning one row per label.
    ///
    /// `dry_run` computes the report and declares NOTHING, so this is safe on a
    /// store opened without the write lease. Only labels the store actually
    /// holds should be passed: upstream's scan is per-label, and a label with
    /// no entities yields a vacuous zero-checked report the caller can
    /// synthesise for free (see [`LabelConformance::not_present`]).
    ///
    /// Upstream samples offending entities by engine-internal `u64` id. Those
    /// are neither citable nor stable across a re-ingest, so each is resolved
    /// back to its `codegraph_id` here; a sample that has no resolvable handle
    /// is COUNTED rather than emitted, so an unstable internal identifier never
    /// reaches the report.
    ///
    /// # Errors
    ///
    /// Returns an error if a conformance scan itself fails.
    pub fn schema_constraint_dry_run(
        &self,
        profile: ConstraintProfile,
        entity_kind: EntityKindToken,
        labels: &[&str],
    ) -> AdapterResult<Vec<LabelConformance>> {
        let mut rows = Vec::with_capacity(labels.len());
        for label in labels {
            let report = self
                .run_schema_constraint(profile, entity_kind, label, true)
                .map_err(|error| {
                    read_back_error(
                        "schema_constraint_dry_run",
                        format!("conformance scan of {label} failed: {error}"),
                    )
                })?;
            rows.push(self.conformance_row(entity_kind, label, &report));
        }
        Ok(rows)
    }

    /// Declares `profile` on every label in `labels`, returning how many
    /// declarations were made.
    ///
    /// Upstream's `enable()` is atomic per label: a non-conforming current
    /// state declares nothing and returns `NonConformingOnEnable`. This method
    /// declares labels one at a time and stops at the first refusal, so a
    /// partial declaration is possible across labels; the caller reports the
    /// count and the refusal together, and `drop_all_schema_constraints`
    /// retracts whatever landed.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Rejected`] naming the refusing label when the
    /// store's current state does not conform.
    #[must_use]
    pub fn declare_schema_constraints(
        &self,
        profile: ConstraintProfile,
        entity_kind: EntityKindToken,
        labels: &[&str],
    ) -> DeclarationOutcome {
        let mut declared = 0usize;
        for label in labels {
            if let Err(error) = self.run_schema_constraint(profile, entity_kind, label, false) {
                return DeclarationOutcome {
                    declared,
                    refusal: Some(format!(
                        "declaring the {} profile on {} {label} was refused: {error}",
                        profile.as_str(),
                        entity_kind.as_str()
                    )),
                };
            }
            declared += 1;
        }
        DeclarationOutcome {
            declared,
            refusal: None,
        }
    }

    /// Drops every schema constraint declared on this store, returning how many
    /// labels were retracted.
    ///
    /// Retracts what is actually DECLARED (read back from the store) rather
    /// than what the current inventory would declare, so a store declared by an
    /// older or newer Egregore is still fully cleaned.
    ///
    /// # Errors
    ///
    /// Returns an error if a retraction fails.
    pub fn drop_schema_constraints(&self, include_foreign: bool) -> AdapterResult<DropOutcome> {
        let mut outcome = DropOutcome::default();
        for descriptor in self.declared_schema_constraints() {
            let kind = match descriptor.entity_kind.as_str() {
                "node" => ::aletheiadb::EntityKind::Node,
                "edge" => ::aletheiadb::EntityKind::Edge,
                // Upstream's tokens are exactly `node`/`edge`. Anything else
                // came from a crafted or future sidecar; guessing `Node` would
                // silently fail to drop a real constraint and under-report, so
                // it is retained and disclosed instead.
                _ => {
                    outcome.foreign_retained.push(descriptor);
                    continue;
                }
            };
            if !include_foreign && !Self::is_writable_label(&descriptor) {
                outcome.foreign_retained.push(descriptor);
                continue;
            }
            // A failure here leaves every EARLIER label already retracted, and the
            // sidecar rewritten. Bailing with `?` would drop `outcome` on the
            // floor, destroying the only surviving before-image of those labels -
            // the precise loss the before-image exists to prevent. Report the
            // partial progress instead, mirroring the declaration path.
            match self.db.drop_schema_constraint(kind, &descriptor.label) {
                Ok(true) => outcome.dropped.push(descriptor),
                Ok(false) => {}
                Err(error) => {
                    outcome.refusal = Some(format!(
                        "dropping schema constraints failed at {}: {error}",
                        descriptor.label
                    ));
                    return Ok(outcome);
                }
            }
        }
        Ok(outcome)
    }

    /// Whether a declaration sits on a label Egregore itself can write.
    fn is_writable_label(descriptor: &DeclaredConstraint) -> bool {
        let inventory = match descriptor.entity_kind.as_str() {
            "edge" => crate::schema_constraints::writable_edge_types(),
            _ => crate::schema_constraints::writable_node_labels(),
        };
        inventory.contains(&descriptor.label.as_str())
    }

    /// Builds and runs the upstream constraint declaration for one
    /// `(profile, kind, label)`, either as a dry run or for real.
    ///
    /// Single point of translation from Egregore's `PropertySpec` vocabulary to
    /// upstream's, and the single point at which `dry_run` is chosen, so the
    /// conformance scan and the real declaration can never evaluate different
    /// constraint sets. (Upstream's builder type is not exported, so the
    /// builder cannot be handed back to the two call sites separately.)
    fn run_schema_constraint(
        &self,
        profile: ConstraintProfile,
        entity_kind: EntityKindToken,
        label: &str,
        dry_run: bool,
    ) -> ::aletheiadb::Result<::aletheiadb::core::constraint::ConformanceReport> {
        let kind = match entity_kind {
            EntityKindToken::Node => ::aletheiadb::EntityKind::Node,
            EntityKindToken::Edge => ::aletheiadb::EntityKind::Edge,
        };
        let mut builder = self.db.schema_constraint(kind, label);
        for spec in profile.specs(entity_kind) {
            let declared = match spec.declared_type {
                DeclaredTypeToken::String => ::aletheiadb::core::constraint::DeclaredType::String,
                DeclaredTypeToken::Integer => ::aletheiadb::core::constraint::DeclaredType::Integer,
            };
            builder = if spec.required {
                builder.require_typed(spec.property, declared)
            } else {
                builder.typed(spec.property, declared)
            };
        }
        if dry_run {
            builder = builder.dry_run();
        }
        builder.enable()
    }

    /// Translates an upstream conformance report into an Egregore row, citing
    /// offending entities by `codegraph_id`.
    fn conformance_row(
        &self,
        entity_kind: EntityKindToken,
        label: &str,
        report: &::aletheiadb::core::constraint::ConformanceReport,
    ) -> LabelConformance {
        let status = if report.total_checked == 0 {
            ConformanceStatus::NotPresent
        } else if report.conforms {
            ConformanceStatus::Conforms
        } else {
            ConformanceStatus::Violates
        };

        let violations = report
            .violations
            .iter()
            .map(|violation| {
                let mut sample_record_ids = Vec::new();
                let mut unresolved_samples = 0usize;
                for id in &violation.sample_ids {
                    match self.resolve_sample_record_id(entity_kind, *id) {
                        Some(record_id) => sample_record_ids.push(record_id),
                        None => unresolved_samples += 1,
                    }
                }
                sample_record_ids.sort();
                sample_record_ids.dedup();
                // Upstream keeps only the first `MAX_CONFORMANCE_SAMPLE_IDS`
                // offenders per group, in the iteration order of a per-process
                // randomly-seeded `DashMap`. Hitting that bound therefore means
                // the SUBSET is arbitrary and varies between runs; falling short
                // of it means every offender was captured, so the sorted list is
                // stable. Only the latter is citable.
                let sample_complete =
                    violation.sample_ids.len() < crate::schema_constraints::ENGINE_SAMPLE_BOUND;
                ViolationRow {
                    property: violation.property.clone(),
                    reason: violation.reason.clone(),
                    sample_record_ids,
                    unresolved_samples,
                    sample_complete,
                }
            })
            .collect();

        LabelConformance {
            entity_kind,
            label: label.to_owned(),
            status,
            checked: report.total_checked,
            non_conforming: report.total_non_conforming,
            violations,
        }
    }

    /// Resolves one engine-internal entity id to its `codegraph_id` handle.
    ///
    /// Returns `None` when the entity carries no `codegraph_id` — which is the
    /// common case here, since a MISSING `codegraph_id` is itself one of the
    /// violations being reported.
    fn resolve_sample_record_id(&self, entity_kind: EntityKindToken, id: u64) -> Option<String> {
        let value = match entity_kind {
            EntityKindToken::Node => {
                let node_id = ::aletheiadb::NodeId::new(id).ok()?;
                let node = self.db.get_node(node_id).ok()?;
                node.get_property("codegraph_id")?.as_str()?.to_owned()
            }
            EntityKindToken::Edge => {
                let edge_id = ::aletheiadb::EdgeId::new(id).ok()?;
                let edge = self.db.get_edge(edge_id).ok()?;
                edge.get_property("codegraph_id")?.as_str()?.to_owned()
            }
        };
        Some(value)
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
                // Preserve the reconstructed canonical record line so `eg export`
                // (issue #155) can re-emit an unknown-version record verbatim.
                // A record physically present via the normal `eg ingest` path
                // always carries a known kind and every required property, so
                // reconstruction succeeds; only an artificially raw-injected node
                // lacking a required property (e.g. `summary`) reconstructs to
                // `None`, which export surfaces as an enumerated skip diagnostic
                // rather than a silent drop. Counting behavior is unchanged: the
                // record is still counted only under `unknown_schema_versions`.
                let raw_line = if record_type.as_deref() == Some("tombstone") {
                    self.read_tombstone_record_internal(&record_id, node_id)
                } else {
                    self.read_node_record_internal(&record_id, node_id)
                }
                .ok()
                .as_ref()
                .and_then(|record| serde_json::to_string(record).ok());
                report.unknown_schema_versions.push(
                    crate::schema_version::UnknownSchemaVersion::new(version)
                        .with_raw_line(raw_line),
                );
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
                    // Preserve the reconstructed canonical edge line for
                    // `eg export` (issue #155); see the node branch above.
                    let raw_line = self
                        .read_edge_record_internal(&codegraph_id, edge_id)
                        .ok()
                        .as_ref()
                        .and_then(|record| serde_json::to_string(record).ok());
                    report.unknown_schema_versions.push(
                        crate::schema_version::UnknownSchemaVersion::new(version)
                            .with_raw_line(raw_line),
                    );
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
            content_signature,
            route,
            temporal,
            semantic_drift,
            evidence_links,
            repository_identity,
            source_snapshot,
            dependency,
            log,
            scan_coverage,
            embedding_model,
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
            review_commit_sha,
            identity_system,
            transition_kind,
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
        builder = insert_optional(builder, "content_signature", content_signature.as_deref());
        if let Some(route) = route
            && let Ok(json) = serde_json::to_string(route)
        {
            builder = builder.insert("route_json", json.as_str());
        }
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
        if let Some(payload) = scan_coverage
            && let Ok(json) = serde_json::to_string(payload.as_ref())
        {
            builder = builder.insert("scan_coverage_json", json.as_str());
        }
        // Vector-index embedding-model identity (issue #104): the queryable
        // index's producing model, persisted so `eg query semantic` can prove
        // the query embedder shares the index's vector space.
        if let Some(payload) = embedding_model
            && let Ok(json) = serde_json::to_string(payload.as_ref())
        {
            builder = builder.insert("embedding_model_json", json.as_str());
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
        builder = insert_optional(builder, "review_commit_sha", review_commit_sha.as_deref());
        builder = insert_optional(builder, "identity_system", identity_system.as_deref());
        builder = insert_optional(builder, "transition_kind", transition_kind.as_deref());
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
            .map_err(|error| classify_store_error(id, error.to_string()))?;
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
            .create_node(crate::schema_constraints::TOMBSTONE_LABEL, properties)
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
            frame_resolution,
            frame_index,
            basis,
            is_exhaustive,
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
        builder = insert_optional(
            builder,
            "frame_resolution",
            frame_resolution.map(crate::ir::FrameResolution::as_str),
        );
        let frame_index_str = frame_index.map(|i| i.to_string());
        builder = insert_optional(builder, "frame_index", frame_index_str.as_deref());
        builder = insert_optional(
            builder,
            "basis",
            basis.map(crate::ir::CorrelationBasis::as_str),
        );
        // Struct-literal exhaustiveness marker on `CONSTRUCTS` edges (issue #443).
        let is_exhaustive_str = is_exhaustive.map(|value| if value { "true" } else { "false" });
        builder = insert_optional(builder, "is_exhaustive", is_exhaustive_str);
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
            content_signature: optional_str_property(
                record_id,
                "content_signature",
                node.get_property("content_signature"),
            )?,
            route: optional_str_property(record_id, "route_json", node.get_property("route_json"))?
                .as_deref()
                .map(serde_json::from_str::<Vec<RouteAnnotation>>)
                .transpose()
                .map_err(|e| read_back_error(record_id, format!("route_json invalid: {e}")))?,
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
            scan_coverage: optional_str_property(
                record_id,
                "scan_coverage_json",
                node.get_property("scan_coverage_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<crate::ir::ScanCoveragePayload>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("scan_coverage_json invalid: {e}")))?
            .map(Box::new),
            embedding_model: optional_str_property(
                record_id,
                "embedding_model_json",
                node.get_property("embedding_model_json"),
            )?
            .as_deref()
            .map(serde_json::from_str::<EmbeddingModel>)
            .transpose()
            .map_err(|e| read_back_error(record_id, format!("embedding_model_json invalid: {e}")))?
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
            review_commit_sha: optional_str_property(
                record_id,
                "review_commit_sha",
                node.get_property("review_commit_sha"),
            )?,
            identity_system: optional_str_property(
                record_id,
                "identity_system",
                node.get_property("identity_system"),
            )?,
            transition_kind: optional_str_property(
                record_id,
                "transition_kind",
                node.get_property("transition_kind"),
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

    // A long but flat field-by-field edge reconstruction; each optional edge
    // property (resolution, frame_resolution, frame_index, basis, is_exhaustive,
    // …) is parsed inline, so the line count exceeds the default lint threshold.
    #[allow(clippy::too_many_lines)]
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
            frame_resolution: optional_str_property(
                record_id,
                "frame_resolution",
                edge.get_property("frame_resolution"),
            )?
            .as_deref()
            .map(|value| {
                crate::ir::FrameResolution::from_wire(value).ok_or_else(|| {
                    read_back_error(record_id, format!("frame_resolution invalid: {value}"))
                })
            })
            .transpose()?,
            frame_index: optional_str_property(
                record_id,
                "frame_index",
                edge.get_property("frame_index"),
            )?
            .as_deref()
            .map(|value| {
                value.parse::<u32>().map_err(|_| {
                    read_back_error(record_id, format!("frame_index invalid: {value}"))
                })
            })
            .transpose()?,
            basis: optional_str_property(record_id, "basis", edge.get_property("basis"))?
                .as_deref()
                .map(|value| {
                    crate::ir::CorrelationBasis::from_wire(value).ok_or_else(|| {
                        read_back_error(record_id, format!("basis invalid: {value}"))
                    })
                })
                .transpose()?,
            is_exhaustive: parse_is_exhaustive_property(
                record_id,
                optional_str_property(
                    record_id,
                    "is_exhaustive",
                    edge.get_property("is_exhaustive"),
                )?
                .as_deref(),
            )?,
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

/// Coalesce-relevant "observation key" of a log-domain node: the FULL scan
/// payload of the node, excluding only post-scan enrichment metadata. Two
/// physical versions of the same stable ID that share this key are the SAME
/// scan observation — e.g. an enrichment rewrite from `resolve-frames` /
/// `link-logs` that adds node-level `evidence_links` but leaves the log payload
/// (`LogPayload`) untouched — and must never be re-counted by
/// [`EmbeddedAletheiaSink::read_all_records_log_retained`]. Returns `None` for a
/// node that carries no coalesce-relevant log payload.
///
/// # Why the full payload, not just the occurrence triple
///
/// Two GENUINELY DISTINCT scan observations of one signature ID can share
/// identical `first_seen`/`last_seen`/`occurrence_count` yet differ in a
/// non-identity SCAN payload field — most notably the `frames` backtrace chain
/// captured at scan time (#322). A key on the occurrence triple alone would
/// treat those as one observation and drop the superseded version, so
/// `--data-dir` would UNDER-SUM versus the `--graph` path which retains and sums
/// both. Keying on the whole payload keeps every distinct scan payload while
/// still collapsing enrichment-only rewrites.
///
/// # Why `evidence_links` is excluded for free
///
/// `evidence_links` is a NODE-level field on [`GraphRecord::Node`], NOT part of
/// [`crate::ir::LogPayload`]. `resolve-frames` / `link-logs` enrichment mutates
/// only that node-level field (via `GraphRecord::with_evidence_links`), never
/// the payload, so serializing the payload naturally excludes enrichment
/// metadata: an enrichment rewrite has an identical payload → identical key →
/// collapses, while a distinct scan payload has a distinct key → is retained.
///
/// # Determinism
///
/// The key is `serde_json::to_string` of the matched payload variant. All log
/// payload structs (and `StackFrame`) are plain scalar/`Option` fields in a
/// fixed declaration order with no maps, so serialization is byte-stable across
/// runs — the same machinery that makes the graph JSONL byte-stable. The enum's
/// `#[serde(tag = "log_kind")]` discriminant keeps signature and bucket keyspaces
/// disjoint. `serde_json` cannot fail for these map-free structs; `.ok()` degrades
/// a theoretically-impossible failure to `None` (retain — never a silent
/// double-count).
///
/// * `ErrorSignature`: the full [`crate::ir::ErrorSignaturePayload`], including
///   the `frames` chain, so two observations differing only in captured frames
///   are retained.
/// * `LogOccurrenceBucket`: the full bucket payload, which since issue #361
///   carries the owning `source_id` as an identity input — so distinct sources
///   get distinct payload keys AND distinct record IDs, while a byte-identical
///   rescan is already write-deduped. This is effectively a no-op for buckets
///   (same ID ⟹ same content), but the rule is applied uniformly.
fn log_observation_key(record: &GraphRecord) -> Option<String> {
    let payload = record.log_payload()?;
    match payload {
        crate::ir::LogPayload::ErrorSignature(_)
        | crate::ir::LogPayload::LogOccurrenceBucket(_) => serde_json::to_string(payload).ok(),
        crate::ir::LogPayload::LogSource(_) | crate::ir::LogPayload::LogEvent(_) => None,
    }
}

/// Returns `true` for a `GraphRecord::Node` whose kind is one of the two
/// non-temporal log-domain kinds (`ErrorSignature` / `LogOccurrenceBucket`) —
/// exactly the node records the retained reads strip and replace with
/// [`EmbeddedAletheiaSink::retained_log_observation_records`] (issue #363).
/// Edges and tombstones report `None` from `node_kind_name` and are never
/// matched, so `LogSource` / `LogEvent` nodes and all non-log records pass
/// through untouched.
fn is_log_domain_node(record: &GraphRecord) -> bool {
    matches!(
        record.node_kind_name(),
        Some("ErrorSignature" | "LogOccurrenceBucket")
    )
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
        "ExternalIdentity" => Ok(NodeKind::ExternalIdentity),
        "ReviewStateTransition" => Ok(NodeKind::ReviewStateTransition),
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
        "ScanCoverage" => Ok(NodeKind::ScanCoverage),
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

/// Parses the persisted `is_exhaustive` marker string on a `CONSTRUCTS` edge
/// (issue #443) back into `Option<bool>`. `None` when absent (every
/// non-`CONSTRUCTS` edge, or a legacy record); a malformed value fails closed.
fn parse_is_exhaustive_property(
    record_id: &str,
    value: Option<&str>,
) -> AdapterResult<Option<bool>> {
    match value {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(other) => Err(read_back_error(
            record_id,
            format!("is_exhaustive invalid: {other}"),
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
        "REVIEWS_COMMIT" => Ok(EdgeLabel::ReviewsCommit),
        "REVIEWED_BY" => Ok(EdgeLabel::ReviewedBy),
        "REQUESTED_REVIEW_FROM" => Ok(EdgeLabel::RequestedReviewFrom),
        "TRANSITIONS_REVIEW" => Ok(EdgeLabel::TransitionsReview),
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
        // Struct-literal construction edge (issue #443).
        "CONSTRUCTS" => Ok(EdgeLabel::Constructs),
        // Route-registration edge (issue #445).
        "REGISTERS_ROUTE" => Ok(EdgeLabel::RegistersRoute),
        _ => Err(read_back_error(
            record_id,
            format!("unknown embedded edge label {label}"),
        )),
    }
}

/// Marker text `AletheiaDB` 0.2.0 emits when it refuses to open a data
/// directory holding an unreplayed pre-v13 WAL tail.
///
/// Upstream this is `StorageError::PreV13WalTailRequiresMigration`, whose
/// `Display` is `"pre-v13 (0.1.x) WAL tail cannot be replayed safely: {reason}"`.
/// Matched on the stable leading phrase rather than the variable `reason`.
const PRE_V13_WAL_TAIL_MARKER: &str = "pre-v13";

/// Augments an embedded-store OPEN failure with Egregore-specific remediation.
///
/// The case that matters is `AletheiaDB` 0.2.0's refusal to replay a pre-v13
/// WAL tail. Upstream's own message is accurate but speaks in `AletheiaDB`
/// terms; an Egregore operator hitting it has a concrete cause — the data dir
/// was last written by an `eg` binary linked against `AletheiaDB` 0.1.x and was
/// not shut down cleanly — and a concrete remedy, so the raw string is prefixed
/// with both rather than passed through bare.
///
/// This is a refusal, not corruption: 0.1.x stored WAL labels as process-local
/// interner ids, and replaying them under 0.2.0's differently-ordered interner
/// would resolve them to unrelated strings. Upstream refuses instead of
/// silently corrupting every entity recovered from the tail. Nothing in the
/// directory is modified by the failed open.
///
/// Any other open error passes through verbatim.
fn classify_open_error(data_dir: &Path, message: &str) -> String {
    if message.contains(PRE_V13_WAL_TAIL_MARKER) {
        return format!(
            "embedded store {} was written by an older Egregore build (AletheiaDB 0.1.x) and \
             still holds an unreplayed write-ahead-log tail, which AletheiaDB 0.2.0 refuses to \
             replay because doing so would silently corrupt every recovered record's labels; \
             nothing was modified. Remedy: re-open the directory once with the previous `eg` \
             build so it drains its own WAL and shuts down cleanly, then re-run this command; \
             if that build is unavailable, re-ingest from JSONL into a FRESH --data-dir \
             (`eg export` against the old store still requires the old build). Upstream detail: \
             {message}",
            data_dir.display()
        );
    }
    message.to_owned()
}

/// Classifies a store write/persist error string, mapping the `AletheiaDB`
/// string-interner capacity overflow (issue #439) to the fatal
/// [`AdapterError::CapacityExceeded`] and everything else to
/// [`AdapterError::Rejected`].
fn classify_store_error(record_id: &str, message: String) -> AdapterError {
    if crate::adapters::is_string_interner_capacity_error(&message) {
        AdapterError::CapacityExceeded {
            resource: "string interner".to_owned(),
            limit: Some(crate::adapters::preflight::MAX_INTERNED_STRINGS),
            detail: message,
        }
    } else {
        AdapterError::Rejected {
            record_id: record_id.to_owned(),
            message,
        }
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
        | NodeKind::ExternalIdentity
        | NodeKind::ReviewStateTransition
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
        | NodeKind::ScanCoverage
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

/// Candidate on-disk directories holding a persisted vector index for
/// `property`, most-likely first (issue #489).
///
/// The `indexes/` segment comes from `durable_config_for_data_dir` — the SAME
/// call `open_inner` configures the store with, so the probe and the engine
/// cannot disagree about which root they are talking about. The inner
/// `indexes/vector/<property>` layout is `IndexPersistenceManager`'s, which is
/// not public API; both the nested and the flattened form are probed so an
/// upstream layout change degrades this to the pre-#489 behavior (report
/// `absent`) rather than to a wrong answer.
#[cfg(feature = "embeddings")]
fn persisted_vector_index_dirs(data_dir: &Path, property: &str) -> Vec<PathBuf> {
    let persistence_root = ::aletheiadb::config::durable_config_for_data_dir(data_dir)
        .persistence
        .data_dir;
    vec![
        persistence_root
            .join("indexes")
            .join("vector")
            .join(property),
        persistence_root.join("vector").join(property),
    ]
}

/// Classifies a store's vector index for `property` when the engine did NOT
/// register it (issue #489).
///
/// A registered index is `Loaded` and never reaches here. What is left is the
/// pair `AletheiaDB` 0.2.0's skip-on-load made indistinguishable through the
/// engine handle: persisted index state present (the index was built and then
/// corrupted, truncated, or made unreadable) versus nothing there at all (the
/// store was never `--embed`ed).
///
/// The property DIRECTORY existing is the deciding signal, not any single file:
/// upstream's loader requires `meta.idx`, so a property directory that survives
/// with any subset of the artifacts — including none — is exactly a directory
/// the loader skipped. Only a store with no such directory is honestly `Absent`.
///
/// Strictly read-only, and deterministic: artifacts are reported in the fixed
/// [`VECTOR_INDEX_ARTIFACT_FILES`] order, never in directory-iteration order.
#[cfg(feature = "embeddings")]
fn probe_persisted_vector_index(
    data_dir: &Path,
    property: &str,
) -> crate::embeddings::VectorIndexState {
    use crate::embeddings::VectorIndexState;

    for dir in persisted_vector_index_dirs(data_dir, property) {
        if !dir.is_dir() {
            continue;
        }
        return VectorIndexState::Unreadable {
            artifacts: VECTOR_INDEX_ARTIFACT_FILES
                .iter()
                .copied()
                .filter(|name| dir.join(name).exists())
                .collect(),
        };
    }
    VectorIndexState::Absent
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
    use std::cell::Cell;
    use std::sync::{Condvar, Mutex};

    /// Default ceiling on concurrently open embedded stores.
    const MAX_CONCURRENT: usize = 2;
    /// Ceiling used when free disk space is below `LOW_DISK_THRESHOLD_BYTES`.
    const MAX_CONCURRENT_LOW_DISK: usize = 1;
    /// Free-space threshold (2 GiB) below which store opens are serialised.
    const LOW_DISK_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

    static OPEN_STORES: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

    thread_local! {
        /// Number of gate permits the current thread already holds. A thread
        /// that holds at least one permit is re-entrant (see [`acquire`]).
        static PERMITS_HELD: Cell<usize> = const { Cell::new(0) };
    }

    /// RAII permit; releases its slot when the owning store is dropped.
    pub struct StorePermit {
        /// Whether this permit owns one of the global slots. Re-entrant permits
        /// (a second store on a thread that already holds one) own none.
        owns_slot: bool,
    }

    impl Drop for StorePermit {
        fn drop(&mut self) {
            PERMITS_HELD.with(|held| held.set(held.get().saturating_sub(1)));
            if !self.owns_slot {
                return;
            }
            let (lock, cvar) = &OPEN_STORES;
            if let Ok(mut count) = lock.lock() {
                *count = count.saturating_sub(1);
                // `notify_all`, not `notify_one`: the ceiling is read per
                // acquire and can differ between waiters (the low-disk probe),
                // so a single notify can be consumed by a waiter whose own
                // ceiling is still unmet — losing the wakeup for a waiter that
                // could have proceeded.
                cvar.notify_all();
            }
        }
    }

    /// Blocks until an embedded-store slot is available, then claims it.
    ///
    /// Re-entrant per thread: a thread that already holds a permit takes a
    /// second one WITHOUT claiming another slot and without blocking. Some
    /// in-crate tests legitimately hold two stores at once on distinct data
    /// dirs (comparing a write-path store against a CLI-path store, for
    /// instance), and a blocking second acquire made the gate a hold-and-wait
    /// cycle: one such test self-deadlocked outright at the low-disk ceiling of
    /// one, and two of them deadlocked against each other at the default
    /// ceiling of two. Because a thread only ever blocks while holding zero
    /// slots, no wait cycle can form.
    ///
    /// The trade: the ceiling now bounds concurrently store-owning THREADS
    /// rather than open stores, so a nesting thread can run more than one flush
    /// thread against a single slot. That is the deliberate cost of making
    /// deadlock impossible — an over-tight bound that hangs for the full CI job
    /// timeout is worse than a slightly loose one.
    pub fn acquire() -> StorePermit {
        if PERMITS_HELD.with(Cell::get) > 0 {
            PERMITS_HELD.with(|held| held.set(held.get() + 1));
            return StorePermit { owns_slot: false };
        }
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
        drop(count);
        PERMITS_HELD.with(|held| held.set(held.get() + 1));
        StorePermit { owns_slot: true }
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

    /// Regression: nested acquires on ONE thread must never block, even past the
    /// gate ceiling. Before the gate was made re-entrant, a test holding two
    /// stores at once (distinct data dirs) formed a hold-and-wait cycle: it
    /// self-deadlocked at the low-disk ceiling of one, and two such tests
    /// deadlocked against each other at the default ceiling of two, hanging the
    /// whole test binary until the CI job timeout killed it.
    ///
    /// The nesting depth deliberately exceeds `MAX_CONCURRENT`, so a
    /// non-re-entrant gate cannot pass by luck. The work runs on a spawned
    /// thread behind a channel deadline so a regression FAILS this test rather
    /// than hanging the suite it is meant to protect.
    #[test]
    fn nested_acquires_on_one_thread_never_block() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let depth = MAX_CONCURRENT + 1;
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let permits: Vec<StorePermit> = (0..depth).map(|_| acquire()).collect();
            // Report only after every nested permit is held simultaneously.
            let _ = tx.send(permits.len());
            drop(permits);
        });

        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(held) => assert_eq!(
                held, depth,
                "every nested permit must be held at once, got {held} of {depth}"
            ),
            Err(error) => panic!(
                "nested acquire past the ceiling of {MAX_CONCURRENT} must not block, \
                 but {depth} nested permits were never all held: {error}"
            ),
        }
        worker.join().expect("gate nesting thread should not panic");
    }

    /// Nesting depth must unwind exactly, so a thread that has released every
    /// permit is once again a first-acquire thread that claims a real slot.
    /// Asserted on the thread-local depth only: the global slot count is shared
    /// with every concurrently running store test, so reading it here would be
    /// racy.
    #[test]
    fn nested_permit_depth_unwinds_to_zero() {
        assert_eq!(
            PERMITS_HELD.with(Cell::get),
            0,
            "a fresh test thread must start at depth zero"
        );
        {
            let _outer = acquire();
            let inner = acquire();
            assert_eq!(PERMITS_HELD.with(Cell::get), 2, "both permits held");
            drop(inner);
            assert_eq!(
                PERMITS_HELD.with(Cell::get),
                1,
                "dropping the nested permit must leave the outer one held"
            );
        }
        assert_eq!(
            PERMITS_HELD.with(Cell::get),
            0,
            "thread-local permit depth must unwind to zero"
        );
    }
}

/// Fixture affordances for tests that must simulate a NON-Egregore writer.
///
/// Gated behind the non-default `test-fixtures` feature, so it is compiled ONLY
/// for the test target and is absent from any production build of the published
/// crate — `#[doc(hidden)]` hides a symbol from rustdoc, it does not remove it,
/// and this module can write a record that bypasses every Egregore invariant.
///
/// It exists because the whole point of the issue #486 backstop is
/// a bad write that reaches the store by a path `eg validate` cannot gate (a
/// hand-edited store, a foreign writer, an adapter regression), and there is no
/// way to produce that condition through Egregore's own write path, which always
/// emits the full `base_properties` spine. Integration tests need it to prove
/// both halves of the contract: that the audit REPORTS such a record, and that a
/// declared store REFUSES it at the commit hook.
#[doc(hidden)]
#[cfg(feature = "test-fixtures")]
pub mod fixtures {
    use std::path::Path;

    use super::{AdapterResult, EmbeddedAletheiaSink};

    /// Writes a raw node under `label` that carries none of the identity spine.
    ///
    /// Takes the ordinary write lease, so it contends with a live writer exactly
    /// as any other write would. Returns the store's error when schema
    /// constraints are declared and reject it — which is what the enforcement
    /// test asserts on.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened, or if a declared schema
    /// constraint refuses the write.
    pub fn plant_nonconforming_node(
        data_dir: impl AsRef<Path>,
        label: &str,
        marker: &str,
    ) -> AdapterResult<()> {
        // Deliberately NO `codegraph_id` / `record_type` / `schema_version`:
        // this is the shape a foreign writer produces, and it is unciteable by
        // construction (there is no handle to resolve).
        plant_raw_node(data_dir, label, &[("planted_marker", marker)])
    }

    /// Writes a raw node that HAS a `codegraph_id` but still violates the spine.
    ///
    /// The distinction matters: `plant_nonconforming_node` produces a record
    /// with no handle at all, so it can only ever land in `unresolved_samples`.
    /// This one is citable, so it exercises the sample-resolution path that
    /// turns an engine-internal entity id back into a record handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened, or if a declared schema
    /// constraint refuses the write.
    pub fn plant_citable_nonconforming_node(
        data_dir: impl AsRef<Path>,
        label: &str,
        codegraph_id: &str,
    ) -> AdapterResult<()> {
        // Carries the identity key but NOT `record_type` / `schema_version`.
        plant_raw_node(data_dir, label, &[("codegraph_id", codegraph_id)])
    }

    /// Writes a raw node carrying the FULL identity spine at an arbitrary
    /// `schema_version`.
    ///
    /// Exists to test the schema-bump contract at the level the constraint
    /// actually operates. `eg ingest` cannot be used for this: Egregore's own
    /// reader rejects an unknown future `(domain, kind, version)` tuple at the
    /// JSONL parse gate, long before any store write, so it would prove the
    /// reader's behaviour rather than the constraint's.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened, or if a declared schema
    /// constraint refuses the write — which is exactly the assertion: a bumped
    /// VALUE must not violate a constraint that declares the TYPE.
    pub fn plant_spine_node_at_version(
        data_dir: impl AsRef<Path>,
        label: &str,
        codegraph_id: &str,
        schema_version: i64,
    ) -> AdapterResult<()> {
        let sink = EmbeddedAletheiaSink::open(data_dir.as_ref())?;
        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", codegraph_id)
            .insert("record_type", "node")
            .insert("schema_version", schema_version)
            .insert("summary", "spine node at an arbitrary schema version")
            .build();
        sink.db
            .create_node(label, properties)
            .map_err(|error| super::AdapterError::Rejected {
                record_id: codegraph_id.to_owned(),
                message: error.to_string(),
            })?;
        Ok(())
    }

    /// Declares a schema constraint on an arbitrary label, simulating another
    /// tool that shares the data dir.
    ///
    /// Exists so the `--drop` scoping contract is testable: Egregore's own
    /// declaration path only ever touches labels in its inventory, so without
    /// this there is no way to create the foreign declaration that `--drop` must
    /// RETAIN rather than destroy.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened or the declaration is
    /// refused.
    pub fn declare_foreign_constraint(
        data_dir: impl AsRef<Path>,
        label: &str,
        property: &str,
    ) -> AdapterResult<()> {
        let sink = EmbeddedAletheiaSink::open(data_dir.as_ref())?;
        sink.db
            .schema_constraint(::aletheiadb::EntityKind::Node, label)
            .typed(
                property,
                ::aletheiadb::core::constraint::DeclaredType::String,
            )
            .enable()
            .map_err(|error| super::AdapterError::Rejected {
                record_id: label.to_owned(),
                message: error.to_string(),
            })?;
        Ok(())
    }

    /// Declares a foreign constraint whose descriptor is deliberately UNLIKE
    /// anything Egregore's own profiles emit: a required `Integer` key and a
    /// dimension-pinned `Vector` key.
    ///
    /// Exists so the before-image contract is testable at full fidelity. Every
    /// Egregore-declared property is a non-required `String`/`Integer`, so a
    /// before-image that silently dropped the type, the required flag, or the
    /// vector dimension would still look correct against Egregore's own labels.
    /// A foreign declaration is the only one `--declare` cannot rebuild, so it
    /// is the only case where the recorded descriptor is the sole route back.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be opened or the declaration is
    /// refused.
    pub fn declare_foreign_typed_constraint(
        data_dir: impl AsRef<Path>,
        label: &str,
        required_int_property: &str,
        vector_property: &str,
        vector_dim: usize,
    ) -> AdapterResult<()> {
        let sink = EmbeddedAletheiaSink::open(data_dir.as_ref())?;
        sink.db
            .schema_constraint(::aletheiadb::EntityKind::Node, label)
            .require_typed(
                required_int_property,
                ::aletheiadb::core::constraint::DeclaredType::Integer,
            )
            .typed(
                vector_property,
                ::aletheiadb::core::constraint::DeclaredType::Vector {
                    dim: Some(vector_dim),
                },
            )
            .enable()
            .map_err(|error| super::AdapterError::Rejected {
                record_id: label.to_owned(),
                message: error.to_string(),
            })?;
        Ok(())
    }

    fn plant_raw_node(
        data_dir: impl AsRef<Path>,
        label: &str,
        properties: &[(&str, &str)],
    ) -> AdapterResult<()> {
        let sink = EmbeddedAletheiaSink::open(data_dir.as_ref())?;
        let mut builder = ::aletheiadb::PropertyMapBuilder::new();
        for (key, value) in properties {
            builder = builder.insert(key, *value);
        }
        sink.db
            .create_node(label, builder.build())
            .map_err(|error| super::AdapterError::Rejected {
                record_id: label.to_owned(),
                message: error.to_string(),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{GraphRecord, SourceSpan, TemporalMetadata, stable_id};

    /// Issue #489: a data dir with no persisted vector-index directory is the
    /// honest "never `--embed`ed" case. Absence must stay absence — inventing
    /// an unreadable-index diagnostic for a store that simply has no index
    /// would be the mirror-image dishonesty of the bug being fixed.
    #[cfg(feature = "embeddings")]
    #[test]
    fn probe_reports_absent_when_no_vector_index_directory_exists() {
        let temp = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            probe_persisted_vector_index(temp.path(), EMBEDDING_INDEX_PROPERTY),
            crate::embeddings::VectorIndexState::Absent
        );
        // A store directory that exists but was never embedded is also absent.
        fs::create_dir_all(temp.path().join("indexes").join("indexes").join("vector"))
            .expect("vector root");
        assert_eq!(
            probe_persisted_vector_index(temp.path(), EMBEDDING_INDEX_PROPERTY),
            crate::embeddings::VectorIndexState::Absent
        );
    }

    /// Issue #489: a per-property index directory that the engine did not
    /// register is exactly the skip condition, and the surviving artifacts are
    /// reported in the fixed declared order — never in directory-iteration
    /// order, which would make the diagnostic non-deterministic.
    #[cfg(feature = "embeddings")]
    #[test]
    fn probe_reports_surviving_artifacts_in_fixed_order() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp
            .path()
            .join("indexes")
            .join("indexes")
            .join("vector")
            .join(EMBEDDING_INDEX_PROPERTY);
        fs::create_dir_all(&dir).expect("index dir");
        // Written in reverse declared order, plus a file outside the table.
        fs::write(dir.join("current.usearch"), b"x").expect("write");
        fs::write(dir.join("meta.idx"), b"x").expect("write");
        fs::write(dir.join("something-else.bin"), b"x").expect("write");
        assert_eq!(
            probe_persisted_vector_index(temp.path(), EMBEDDING_INDEX_PROPERTY),
            crate::embeddings::VectorIndexState::Unreadable {
                artifacts: vec!["meta.idx", "current.usearch"],
            },
            "artifacts follow the declared table order, and unknown filenames never leak"
        );
    }

    /// Issue #489: upstream's loader requires `meta.idx`, so a property
    /// directory that survives with none of the expected files is still a
    /// directory the loader skipped — reported as unreadable with an empty
    /// artifact list rather than degraded back to "absent".
    #[cfg(feature = "embeddings")]
    #[test]
    fn probe_reports_an_empty_index_directory_as_unreadable() {
        let temp = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(
            temp.path()
                .join("indexes")
                .join("indexes")
                .join("vector")
                .join(EMBEDDING_INDEX_PROPERTY),
        )
        .expect("index dir");
        assert_eq!(
            probe_persisted_vector_index(temp.path(), EMBEDDING_INDEX_PROPERTY),
            crate::embeddings::VectorIndexState::Unreadable { artifacts: vec![] }
        );
    }

    /// Issue #439: a store error carrying `AletheiaDB`'s real interner-overflow
    /// Display maps through `classify_store_error` to the fatal
    /// `CapacityExceeded` variant, while an unrelated error stays `Rejected`.
    /// This exercises the exact mapping the `create_node` / `persist_indexes`
    /// sites use, without needing a real cap-clearing ingest (millions of
    /// distinct strings since the 0.2.0 upgrade — far too slow for CI). The
    /// end-to-end overflow-through-a-live-store path is therefore covered only
    /// at this boundary; see `docs/cli/ingest.md`. The sample Display below is
    /// upstream's verbatim format; its numbers are illustrative, and the
    /// classifier matches on the message shape, not the values.
    #[test]
    fn classify_store_error_maps_interner_overflow_to_capacity_exceeded() {
        let upstream =
            "Capacity exceeded for string interner: current=100000, limit=100000 (DoS protection)";
        match classify_store_error("codegraph:v1:abc", upstream.to_owned()) {
            AdapterError::CapacityExceeded {
                resource,
                limit,
                detail,
            } => {
                assert_eq!(resource, "string interner");
                assert_eq!(
                    limit,
                    Some(crate::adapters::preflight::MAX_INTERNED_STRINGS)
                );
                assert_eq!(detail, upstream);
            }
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }

        match classify_store_error("codegraph:v1:abc", "node label mismatch".to_owned()) {
            AdapterError::Rejected { record_id, message } => {
                assert_eq!(record_id, "codegraph:v1:abc");
                assert_eq!(message, "node label mismatch");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

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

    /// Builds an `ErrorSignature` log-domain node with the given valid-time
    /// bounds. Distinct `first_seen`/`last_seen`/`occurrence_count` values keep
    /// the node CONTENT distinct while the caller reuses one stable record ID —
    /// exactly what two `scan-logs` outputs for one repo produce (issue #363).
    fn error_signature_record(
        id: &str,
        first_seen: &str,
        last_seen: &str,
        occurrence_count: u64,
    ) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::ErrorSignature,
            None,
            None,
            Some("error signature".to_owned()),
            format!("Error signature x{occurrence_count}"),
        )
        .with_domain("log", crate::ir::LOG_SCHEMA_VERSION)
        .with_log(crate::ir::LogPayload::ErrorSignature(
            crate::ir::ErrorSignaturePayload {
                fingerprint_algorithm: "template-v1".to_owned(),
                template_excerpt: "template boom".to_owned(),
                severity: "error".to_owned(),
                occurrence_count,
                first_seen: first_seen.to_owned(),
                last_seen: last_seen.to_owned(),
                frames: None,
                repository_id: String::new(),
            },
        ))
        .with_valid_time(first_seen, "log_event_timestamp")
    }

    /// Like [`error_signature_record`] but with a caller-supplied captured
    /// backtrace `frames` chain. Two `scan-logs` observations of the same
    /// signature ID can share identical `first_seen`/`last_seen`/
    /// `occurrence_count` yet differ ONLY in their scan-time captured frames
    /// (a #322 non-identity payload field) — a genuinely distinct observation
    /// the retained read must keep, not an enrichment rewrite (issue #363).
    fn error_signature_record_with_frames(
        id: &str,
        first_seen: &str,
        last_seen: &str,
        occurrence_count: u64,
        frames: Vec<crate::ir::StackFrame>,
    ) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::ErrorSignature,
            None,
            None,
            Some("error signature".to_owned()),
            format!("Error signature x{occurrence_count}"),
        )
        .with_domain("log", crate::ir::LOG_SCHEMA_VERSION)
        .with_log(crate::ir::LogPayload::ErrorSignature(
            crate::ir::ErrorSignaturePayload {
                fingerprint_algorithm: "template-v1".to_owned(),
                template_excerpt: "template boom".to_owned(),
                severity: "error".to_owned(),
                occurrence_count,
                first_seen: first_seen.to_owned(),
                last_seen: last_seen.to_owned(),
                frames: Some(frames),
                repository_id: String::new(),
            },
        ))
        .with_valid_time(first_seen, "log_event_timestamp")
    }

    #[test]
    fn read_all_records_log_retained_surfaces_superseded_log_signature_versions() {
        // Issue #363: two `scan-logs` ingests of the same fingerprint (identical
        // stable ID, DIFFERING captured content) each append a physical
        // `ErrorSignature` node. The current-state read collapses them to the
        // latest, but the log-retained read must surface BOTH so the #326/#324
        // cross-scan coalescers reconstruct on `--data-dir` what they do on the
        // concatenated `--graph` JSONL.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("log-retained-store");
        let sig_id = "log:v3:retained-boom";
        let v1 = error_signature_record(sig_id, "2026-01-01T00:00:00Z", "2026-01-01T05:00:00Z", 3);
        let v2 = error_signature_record(sig_id, "2026-01-02T12:00:00Z", "2026-01-02T13:00:00Z", 5);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&v2).expect("v2 should write");

        let count = |records: &[GraphRecord]| {
            records
                .iter()
                .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
                .count()
        };

        let current = sink.read_all_records().expect("current read");
        assert_eq!(
            count(&current),
            1,
            "current-state read collapses to the latest signature version"
        );

        let retained = sink
            .read_all_records_log_retained()
            .expect("log-retained read");
        assert_eq!(
            count(&retained),
            2,
            "log-retained read surfaces both physical signature versions"
        );
    }

    #[test]
    fn read_all_records_log_retained_dedupes_byte_identical_reingest() {
        // Issue #363 residual divergence: a byte-identical re-ingest of the same
        // `scan-logs` output is an idempotent no-op (no new physical node), so the
        // log-retained read still surfaces exactly ONE record — identical rescans
        // never multiply counts on `--data-dir`.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("log-retained-idempotent-store");
        let sig_id = "log:v3:idempotent-boom";
        let sig = error_signature_record(sig_id, "2026-01-01T00:00:00Z", "2026-01-01T05:00:00Z", 3);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&sig).expect("first write should succeed");
        sink.write_record(&sig)
            .expect("identical re-ingest should be a no-op");

        let retained = sink
            .read_all_records_log_retained()
            .expect("log-retained read");
        let count = retained
            .iter()
            .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
            .count();
        assert_eq!(
            count, 1,
            "a byte-identical re-ingest is deduped to one physical record"
        );
    }

    #[test]
    fn read_all_records_log_retained_retains_distinct_payload_same_window() {
        // Issue #363 (Codex P2): two GENUINELY DISTINCT scan observations of the
        // same signature ID with IDENTICAL first_seen/last_seen/occurrence_count
        // but DIFFERING scan-time captured frames (#322, non-identity) are
        // distinct observations — the retained read must surface BOTH so the
        // #326/#324 coalescers sum both occurrence sets exactly as on the
        // concatenated `--graph` JSONL. A key on the occurrence triple alone
        // collapses them to one (under-count); a key on the full scan payload
        // keeps both.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("log-retained-distinct-payload-store");
        let sig_id = "log:v3:distinct-payload-boom";
        let frames_a = vec![crate::ir::StackFrame {
            frame_index: 0,
            module_path: Some("app::alpha".to_owned()),
            file_path: Some("src/alpha.rs".to_owned()),
            line: Some(10),
        }];
        let frames_b = vec![crate::ir::StackFrame {
            frame_index: 0,
            module_path: Some("app::beta".to_owned()),
            file_path: Some("src/beta.rs".to_owned()),
            line: Some(20),
        }];
        let v1 = error_signature_record_with_frames(
            sig_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T05:00:00Z",
            3,
            frames_a,
        );
        let v2 = error_signature_record_with_frames(
            sig_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T05:00:00Z",
            3,
            frames_b,
        );
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&v2).expect("v2 should write");

        let retained = sink
            .read_all_records_log_retained()
            .expect("log-retained read");
        let count = retained
            .iter()
            .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
            .count();
        assert_eq!(
            count, 2,
            "two distinct scan payloads (same occurrence window, different frames) both survive"
        );
    }

    /// Issue #363: a non-log non-temporal kind re-ingested with differing content
    /// keeps its SINGLE current-state record in the log-retained read — only
    /// `ErrorSignature` / `LogOccurrenceBucket` versions are retained, so non-log
    /// query behaviour is unchanged.
    #[test]
    fn read_all_records_log_retained_leaves_non_log_kinds_collapsed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("log-retained-nonlog-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&current_symbol_record(&symbol_id, "v1", 20))
            .expect("v1 should write");
        sink.write_record(&current_symbol_record(&symbol_id, "v2", 42))
            .expect("v2 should write");

        let retained = sink
            .read_all_records_log_retained()
            .expect("log-retained read");
        let count = retained
            .iter()
            .filter(|r| r.id() == symbol_id && r.node_kind_name() == Some("Symbol"))
            .count();
        assert_eq!(
            count, 1,
            "non-log non-temporal kinds keep their single current-state record"
        );
    }

    /// Issue #363 (Codex P2): the history-inclusive log-retained read collapses
    /// an enrichment-only `ErrorSignature` rewrite (identical log payload,
    /// evidence links added) to its single latest (enriched) version, while the
    /// unfiltered history read re-emits both — the exact double-count source on
    /// the `error-context --at`/`--as-of` `--data-dir` temporal lane.
    #[test]
    fn read_all_records_including_superseded_log_retained_collapses_enrichment_rewrite() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("temporal-enrichment-store");
        let sig_id = "log:v3:enrich-boom";
        let bare =
            error_signature_record(sig_id, "2026-01-01T00:00:00Z", "2026-01-01T05:00:00Z", 7);
        // Same log payload, an evidence link added (as resolve-frames emits).
        let enriched = bare.clone().with_evidence_links(vec![EvidenceLink {
            target_record_id: Some("codegraph:v1:target".to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "FRAME_RESOLVES_TO".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }]);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&bare).expect("bare should write");
        sink.write_record(&enriched)
            .expect("enrichment rewrite should write a new physical version");

        let count = |records: &[GraphRecord]| {
            records
                .iter()
                .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
                .count()
        };

        let unfiltered = sink
            .read_all_records_including_superseded()
            .expect("history read");
        assert_eq!(
            count(&unfiltered),
            2,
            "the unfiltered history read re-emits both physical versions (double-count source)"
        );

        let retained = sink
            .read_all_records_including_superseded_log_retained()
            .expect("log-retained history read");
        let versions: Vec<&GraphRecord> = retained
            .iter()
            .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
            .collect();
        assert_eq!(
            versions.len(),
            1,
            "the enrichment rewrite collapses to one physical version"
        );
        assert!(
            versions[0]
                .evidence_links()
                .is_some_and(|links| !links.is_empty()),
            "the retained version is the ENRICHED one (carries the evidence links)"
        );
    }

    /// Issue #363: distinct scan observations (differing log payload) both survive
    /// the log-retained history read — only same-observation rewrites collapse.
    #[test]
    fn read_all_records_including_superseded_log_retained_retains_distinct_observations() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("temporal-distinct-store");
        let sig_id = "log:v3:distinct-boom";
        let v1 = error_signature_record(sig_id, "2026-01-01T00:00:00Z", "2026-01-01T05:00:00Z", 3);
        let v2 = error_signature_record(sig_id, "2026-01-02T12:00:00Z", "2026-01-02T13:00:00Z", 5);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&v2).expect("v2 should write");

        let retained = sink
            .read_all_records_including_superseded_log_retained()
            .expect("log-retained history read");
        let count = retained
            .iter()
            .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
            .count();
        assert_eq!(
            count, 2,
            "two distinct scan observations both survive the log-retained history read"
        );
    }

    /// Issue #363 (Codex P2): the history-inclusive log-retained read keeps two
    /// scan observations that share `first_seen`/`last_seen`/`occurrence_count` but
    /// differ in captured scan payload (#322 frames) — a key on the occurrence
    /// triple alone would collapse them (under-count), a full-scan-payload key
    /// retains both.
    #[test]
    fn read_all_records_including_superseded_log_retained_retains_distinct_payload_same_window() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("temporal-distinct-payload-store");
        let sig_id = "log:v3:temporal-distinct-payload-boom";
        let frames_a = vec![crate::ir::StackFrame {
            frame_index: 0,
            module_path: Some("app::alpha".to_owned()),
            file_path: Some("src/alpha.rs".to_owned()),
            line: Some(10),
        }];
        let frames_b = vec![crate::ir::StackFrame {
            frame_index: 0,
            module_path: Some("app::beta".to_owned()),
            file_path: Some("src/beta.rs".to_owned()),
            line: Some(20),
        }];
        let v1 = error_signature_record_with_frames(
            sig_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T05:00:00Z",
            4,
            frames_a,
        );
        let v2 = error_signature_record_with_frames(
            sig_id,
            "2026-01-01T00:00:00Z",
            "2026-01-01T05:00:00Z",
            4,
            frames_b,
        );
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&v2).expect("v2 should write");

        let retained = sink
            .read_all_records_including_superseded_log_retained()
            .expect("log-retained history read");
        let count = retained
            .iter()
            .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
            .count();
        assert_eq!(
            count, 2,
            "two distinct scan payloads (same occurrence window, different frames) both survive \
             the log-retained history read"
        );
    }

    /// Issue #363: the log-retained history read leaves non-log temporal versions
    /// fully intact — `--at`/`--as-of` valid-time reconstruction is unaffected.
    #[test]
    fn read_all_records_including_superseded_log_retained_leaves_temporal_versions_intact() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("temporal-nonlog-store");
        let symbol_id = stable_id(&["node", "symbol", "src/lib.rs", "stable"]);
        let v1 = symbol_record(
            &symbol_id,
            "v1",
            TemporalMetadata {
                git_commit: "c1".to_owned(),
                git_parent_commits: vec![],
                valid_time: "2026-01-01T00:00:00Z".to_owned(),
                author_time: Some("2026-01-01T00:00:00Z".to_owned()),
                observed_at: "2026-01-01T00:00:00Z".to_owned(),
                valid_time_source: Some("git_commit_committer_date".to_owned()),
            },
        );
        let v2 = symbol_record(
            &symbol_id,
            "v2",
            TemporalMetadata {
                git_commit: "c2".to_owned(),
                git_parent_commits: vec!["c1".to_owned()],
                valid_time: "2026-01-02T00:00:00Z".to_owned(),
                author_time: Some("2026-01-02T00:00:00Z".to_owned()),
                observed_at: "2026-01-02T00:00:00Z".to_owned(),
                valid_time_source: Some("git_commit_committer_date".to_owned()),
            },
        );
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&v2).expect("v2 should write");

        let unfiltered = sink
            .read_all_records_including_superseded()
            .expect("history read");
        let retained = sink
            .read_all_records_including_superseded_log_retained()
            .expect("log-retained history read");
        let commits = |records: &[GraphRecord]| {
            records
                .iter()
                .filter(|r| r.id() == symbol_id && r.node_kind_name() == Some("Symbol"))
                .count()
        };
        assert_eq!(
            commits(&retained),
            commits(&unfiltered),
            "non-log temporal versions are untouched by the log-retained post-filter"
        );
    }

    /// Sums the `occurrence_count` across every `ErrorSignature` physical
    /// version of `sig_id` in a record slice — the value the #326/#324
    /// cross-scan coalescers reconstruct on the `--data-dir` retained read.
    fn summed_signature_occurrences(records: &[GraphRecord], sig_id: &str) -> u64 {
        records
            .iter()
            .filter(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
            .filter_map(GraphRecord::log_payload)
            .filter_map(|payload| match payload {
                crate::ir::LogPayload::ErrorSignature(sig) => Some(sig.occurrence_count),
                _ => None,
            })
            .sum()
    }

    #[test]
    fn read_all_records_log_retained_honors_forget_retraction_boundary() {
        // Issue #363 (Codex P2): `forget` can tombstone a log-domain
        // `ErrorSignature` (a log ID is non-codegraph, non-temporal). If a LATER
        // `scan-logs` writes the SAME stable ID with a DISTINCT payload, that
        // write supersedes the tombstone (higher seq), so the current-state read
        // correctly exposes only the post-forget version. The log-retained read
        // must NOT resurrect the pre-forget observation from the append-only
        // physical history: superseded versions written at or before the
        // retraction are suppressed.
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("log-retained-forget-boundary-store");
        let sig_id = "log:v3:forget-boundary-boom";
        // v1 (occurrence A=3) → forget/tombstone → v2 (occurrence B=5, distinct).
        let v1 = error_signature_record(sig_id, "2026-01-01T00:00:00Z", "2026-01-01T05:00:00Z", 3);
        // The tombstone mirrors exactly what `eg forget` mints for a log ID: a
        // same-domain `log:v<N>:<hash>` tombstone (issue #231/#363).
        let (tombstone_id, tombstone_version) = crate::forget::retraction_tombstone_id(sig_id);
        let tombstone = GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: tombstone_version,
            deleted_id: sig_id.to_owned(),
            summary: "retracted: leaked value".to_owned(),
            producer: None,
        };
        let v2 = error_signature_record(sig_id, "2026-01-02T12:00:00Z", "2026-01-02T13:00:00Z", 5);
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&tombstone)
            .expect("tombstone should write");
        sink.write_record(&v2)
            .expect("post-forget re-scan should write");

        // Baseline: the current-state read already shows only the post-forget
        // version (the later write superseded the tombstone).
        let current = sink.read_all_records().expect("current read");
        assert_eq!(
            summed_signature_occurrences(&current, sig_id),
            5,
            "current-state read shows only the post-forget observation"
        );

        let retained = sink
            .read_all_records_log_retained()
            .expect("log-retained read");
        assert_eq!(
            summed_signature_occurrences(&retained, sig_id),
            5,
            "the pre-forget observation (occurrence 3) must NOT be resurrected into the sum"
        );

        let history_retained = sink
            .read_all_records_including_superseded_log_retained()
            .expect("log-retained history read");
        assert_eq!(
            summed_signature_occurrences(&history_retained, sig_id),
            5,
            "the history-inclusive log-retained read must also honor the retraction boundary"
        );
    }

    #[test]
    fn read_all_records_log_retained_fully_suppresses_active_tombstone() {
        // Issue #363 (Codex P2): the simple retraction case — a log ID tombstoned
        // with NO later write — stays fully suppressed in both retained reads
        // (regression guard alongside the re-scan-after-forget boundary).
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("log-retained-fully-retracted-store");
        let sig_id = "log:v3:fully-retracted-boom";
        let v1 = error_signature_record(sig_id, "2026-01-01T00:00:00Z", "2026-01-01T05:00:00Z", 3);
        let (tombstone_id, tombstone_version) = crate::forget::retraction_tombstone_id(sig_id);
        let tombstone = GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: tombstone_version,
            deleted_id: sig_id.to_owned(),
            summary: "retracted: leaked value".to_owned(),
            producer: None,
        };
        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
        sink.write_record(&v1).expect("v1 should write");
        sink.write_record(&tombstone)
            .expect("tombstone should write");

        let present = |records: &[GraphRecord]| {
            records
                .iter()
                .any(|r| r.id() == sig_id && r.node_kind_name() == Some("ErrorSignature"))
        };

        let retained = sink
            .read_all_records_log_retained()
            .expect("log-retained read");
        assert!(
            !present(&retained),
            "a fully-retracted log signature stays suppressed in the current-state log-retained read"
        );

        let history_retained = sink
            .read_all_records_including_superseded_log_retained()
            .expect("log-retained history read");
        assert!(
            !present(&history_retained),
            "a fully-retracted log signature stays suppressed in the history log-retained read"
        );
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
    #[allow(clippy::too_many_lines)]
    fn external_identity_node_and_edges_round_trip_through_embedded_store() {
        // Issue #335: an ExternalIdentity node (login in `author`, system in
        // `identity_system`) plus its REVIEWED_BY / REQUESTED_REVIEW_FROM edges
        // survive an embedded write + read-back byte-for-byte.
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("external-identity-store");

        let identity_id = crate::github::records::external_identity_id("github", "octocat");
        let mut identity = GraphRecord::node(
            identity_id.clone(),
            NodeKind::ExternalIdentity,
            None,
            None,
            None,
            "github identity octocat".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            author,
            identity_system,
            ..
        } = &mut identity
        {
            *schema_version = crate::ir::PROJECT_SCHEMA_VERSION;
            *domain = Some("project".to_owned());
            *author = Some("octocat".to_owned());
            *identity_system = Some("github".to_owned());
        }

        let review_id = stable_id(&["node", "review", "pr_review:3:100"]);
        let task_id = stable_id(&["node", "task", "pr:3"]);
        let review_node = GraphRecord::node(
            review_id.clone(),
            NodeKind::Review,
            None,
            None,
            None,
            "review".to_owned(),
        );
        let task_node = GraphRecord::node(
            task_id.clone(),
            NodeKind::Task,
            None,
            None,
            None,
            "task".to_owned(),
        );
        let reviewed = GraphRecord::project_edge(
            EdgeLabel::ReviewedBy,
            review_id,
            identity_id.clone(),
            None,
            "review by octocat".to_owned(),
        );
        let requested = GraphRecord::project_edge(
            EdgeLabel::RequestedReviewFrom,
            task_id,
            identity_id.clone(),
            None,
            "requested review from octocat".to_owned(),
        );
        let reviewed_id = reviewed.id().to_owned();
        let requested_id = requested.id().to_owned();

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("store should open");
        sink.write_record(&identity).expect("identity should write");
        sink.write_record(&review_node)
            .expect("review node should write");
        sink.write_record(&task_node)
            .expect("task node should write");
        sink.write_record(&reviewed)
            .expect("REVIEWED_BY should write");
        sink.write_record(&requested)
            .expect("REQUESTED_REVIEW_FROM should write");

        let records = sink.read_all_records().expect("read_all_records");
        let read_identity = records
            .iter()
            .find(|r| r.id() == identity_id)
            .expect("identity read back");
        let GraphRecord::Node {
            kind,
            author,
            identity_system,
            ..
        } = read_identity
        else {
            panic!("expected node");
        };
        assert_eq!(*kind, NodeKind::ExternalIdentity);
        assert_eq!(author.as_deref(), Some("octocat"), "login round-trips");
        assert_eq!(
            identity_system.as_deref(),
            Some("github"),
            "system round-trips"
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(r, GraphRecord::Edge { id, label: EdgeLabel::ReviewedBy, .. } if id == &reviewed_id)),
            "REVIEWED_BY edge round-trips (no unknown-embedded-edge-label error)"
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(r, GraphRecord::Edge { id, label: EdgeLabel::RequestedReviewFrom, .. } if id == &requested_id)),
            "REQUESTED_REVIEW_FROM edge round-trips"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn review_state_transition_node_and_edge_round_trip_through_embedded_store() {
        // Issue #336: a ReviewStateTransition node (transition_kind, actor login
        // in `author`, timeline:<id> in system_native_id, redacted message in
        // body_handle) plus its TRANSITIONS_REVIEW edge survive an embedded write
        // + read-back byte-for-byte, and inspect counts it under
        // (project, ReviewStateTransition, 1).
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("review-state-transition-store");

        let trans_id = crate::github::records::review_state_transition_id("o/r", 7, 5001);
        let review_id = crate::github::records::dismissed_review_record_id("o/r", 7, 301);
        let mut trans = GraphRecord::node(
            trans_id.clone(),
            NodeKind::ReviewStateTransition,
            None,
            None,
            None,
            "review_dismissed on PR #7".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            author,
            transition_kind,
            system_native_id,
            ..
        } = &mut trans
        {
            *schema_version = crate::ir::PROJECT_SCHEMA_VERSION;
            *domain = Some("project".to_owned());
            *author = Some("maintainer".to_owned());
            *transition_kind = Some("review_dismissed".to_owned());
            *system_native_id = Some("timeline:5001".to_owned());
        }
        let mut review_node = GraphRecord::node(
            review_id.clone(),
            NodeKind::Review,
            None,
            None,
            None,
            "review".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            ..
        } = &mut review_node
        {
            *schema_version = crate::ir::PROJECT_SCHEMA_VERSION;
            *domain = Some("project".to_owned());
        }
        let edge = GraphRecord::project_edge(
            EdgeLabel::TransitionsReview,
            trans_id.clone(),
            review_id,
            None,
            "review 301 dismissed".to_owned(),
        );
        let edge_id = edge.id().to_owned();

        let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("store should open");
        sink.write_record(&trans).expect("transition should write");
        sink.write_record(&review_node)
            .expect("review node should write");
        sink.write_record(&edge)
            .expect("TRANSITIONS_REVIEW should write");

        let records = sink.read_all_records().expect("read_all_records");
        let read = records
            .iter()
            .find(|r| r.id() == trans_id)
            .expect("transition read back");
        let GraphRecord::Node {
            kind,
            author,
            transition_kind,
            system_native_id,
            ..
        } = read
        else {
            panic!("expected node");
        };
        assert_eq!(*kind, NodeKind::ReviewStateTransition);
        assert_eq!(author.as_deref(), Some("maintainer"), "actor round-trips");
        assert_eq!(
            transition_kind.as_deref(),
            Some("review_dismissed"),
            "transition_kind round-trips"
        );
        assert_eq!(system_native_id.as_deref(), Some("timeline:5001"));
        assert!(
            records
                .iter()
                .any(|r| matches!(r, GraphRecord::Edge { id, label: EdgeLabel::TransitionsReview, .. } if id == &edge_id)),
            "TRANSITIONS_REVIEW edge round-trips"
        );
        // The read-back record classifies as a known (project, ReviewStateTransition,
        // 1) tuple — the exact tuple `eg inspect --data-dir` groups it under (AC6;
        // the CLI-level inspect assertion lives in the import behaviour suite).
        assert!(
            crate::schema_version::validate_record_version(read).is_ok(),
            "the transition is a known project schema-version tuple"
        );
    }

    #[test]
    fn read_all_records_revives_reviews_commit_edge_after_tombstone() {
        // Issue #334 (contract #6): the generic revive-after-tombstone fix
        // (#333, round-7) must also cover the new REVIEWS_COMMIT project edge — a
        // re-resolution that cycles resolved-A → superseded → resolved-A re-emits
        // the SAME edge bytes/id and must revive the tombstoned anchor in the
        // current read view, not stay suppressed.
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("reviews-commit-revive-store");
        let review_id = stable_id(&["node", "review", "pr_review:3:100"]);
        let commit_id = stable_id(&["node", "commit", "sha-a"]);
        // REVIEWS_COMMIT project edge to commit A. `project_edge` mints the
        // `project:v1:` id; identical bytes are reconstructed below.
        let edge = GraphRecord::project_edge(
            EdgeLabel::ReviewsCommit,
            review_id.clone(),
            commit_id.clone(),
            None,
            "review anchored to commit sha-a".to_owned(),
        );
        let edge_id = edge.id().to_owned();
        let tombstone_id = stable_id(&["tombstone", &edge_id]);

        // Phase 1: resolved-A — endpoints + live edge.
        {
            let mut sink =
                EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");
            sink.write_record(&file_record(&review_id, "review"))
                .expect("review node should write");
            sink.write_record(&current_symbol_record(&commit_id, "commit", 10))
                .expect("commit node should write");
            sink.write_record(&edge).expect("edge should write");
        }

        // Phase 2: superseded — tombstone the edge; it must now be suppressed.
        {
            let mut sink =
                EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
            sink.write_record(&GraphRecord::Tombstone {
                id: tombstone_id,
                schema_version: crate::ir::PROJECT_SCHEMA_VERSION,
                deleted_id: edge_id.clone(),
                summary: "review anchor superseded".to_owned(),
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

        // Phase 3: re-emit IDENTICAL edge bytes — must revive.
        {
            let mut sink =
                EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should reopen");
            let reemitted = GraphRecord::project_edge(
                EdgeLabel::ReviewsCommit,
                review_id,
                commit_id,
                None,
                "review anchored to commit sha-a".to_owned(),
            );
            assert_eq!(reemitted.id(), edge_id, "re-emit must reconstruct the id");
            sink.write_record(&reemitted)
                .expect("identical edge re-emit should write");
            let records = sink
                .read_all_records()
                .expect("read_all_records should succeed");
            assert!(
                records
                    .iter()
                    .any(|r| matches!(r, GraphRecord::Edge { id, .. } if id == &edge_id)),
                "byte-identical re-emit must revive the tombstoned anchor edge in the current view"
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

    /// `AletheiaDB` 0.2.0 upgrade: the adapter must actually APPLY
    /// `PersistenceConfig.max_interned_strings` at open, not merely inherit the
    /// upstream default. Asserted against the process-global interner's real
    /// capacity, so an accidental removal of the config line fails here rather
    /// than silently reverting the cap to whatever upstream defaults to next.
    #[test]
    fn opening_a_store_configures_the_interner_cap() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("interner-cap-store");
        let _sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        let expected = usize::try_from(crate::adapters::preflight::MAX_INTERNED_STRINGS)
            .expect("cap fits in usize on supported targets");
        assert_eq!(
            ::aletheiadb::core::interning::GLOBAL_INTERNER.max_capacity(),
            expected,
            "open must configure the interner from MAX_INTERNED_STRINGS"
        );
    }

    /// A pre-v13 WAL-tail refusal (new in `AletheiaDB` 0.2.0) must be re-framed in
    /// Egregore terms: name the data dir, say nothing was modified, and give the
    /// drain-on-the-old-build remedy — while preserving the upstream detail.
    #[test]
    fn classify_open_error_explains_pre_v13_wal_tail_refusal() {
        let upstream = "pre-v13 (0.1.x) WAL tail cannot be replayed safely: 3 unreplayed entries";
        let message = classify_open_error(Path::new("/srv/.egregore"), upstream);

        assert!(message.contains("/srv/.egregore"), "must name the data dir");
        assert!(
            message.contains("nothing was modified"),
            "must state the failed open changed nothing"
        );
        assert!(
            message.contains("Remedy:"),
            "must carry an actionable remedy"
        );
        assert!(
            message.contains(upstream),
            "must preserve the upstream detail verbatim"
        );
    }

    /// Every other open failure passes through untouched — the classifier must
    /// not editorialise errors it does not understand.
    #[test]
    fn classify_open_error_passes_through_unrelated_failures() {
        let upstream = "I/O error: permission denied";
        assert_eq!(
            classify_open_error(Path::new("/srv/.egregore"), upstream),
            upstream
        );
    }

    /// The #486 inventory is derived from `NodeKind::ALL` on the ASSUMPTION
    /// that the adapter writes one store-side label per kind, named exactly
    /// `kind.as_str()`. `node_label` is a private `const fn` with one combined
    /// arm today, but a future variant given its own arm returning a different
    /// literal would silently desync the inventory from what is actually
    /// written — the report would then misfile a real label as a foreign
    /// writer's, and `--declare` would leave it unconstrained. This pins the
    /// assumption at the write site.
    #[test]
    fn node_label_is_exactly_the_kind_string_for_every_kind() {
        for kind in NodeKind::ALL {
            assert_eq!(
                node_label(kind),
                kind.as_str(),
                "node_label({kind:?}) must equal its kind string, or the #486 \
                 label inventory no longer describes what the adapter writes"
            );
        }
    }

    /// The tombstone label the adapter writes must be the same constant the
    /// inventory publishes, or tombstones fall outside the declared surface.
    #[test]
    fn tombstone_label_matches_the_inventory_constant() {
        assert_eq!(crate::schema_constraints::TOMBSTONE_LABEL, "Tombstone");
        assert!(
            !NodeKind::ALL
                .iter()
                .any(|kind| kind.as_str() == crate::schema_constraints::TOMBSTONE_LABEL)
        );
    }

    /// Egregore's own sample bound must stay at or below upstream's, or
    /// `sample_complete` would claim completeness it cannot establish.
    #[test]
    fn egregore_sample_cap_does_not_exceed_the_engine_bound() {
        const _: () = assert!(
            crate::schema_constraints::MAX_SAMPLE_RECORD_IDS
                <= crate::schema_constraints::ENGINE_SAMPLE_BOUND,
            "egregore's render cap must not exceed the engine's sample bound"
        );
        assert_eq!(
            crate::schema_constraints::ENGINE_SAMPLE_BOUND,
            ::aletheiadb::core::constraint::MAX_CONFORMANCE_SAMPLE_IDS,
            "the mirrored engine sample bound drifted from upstream, which would \
             silently break the sample_complete determinism rule"
        );
    }

    #[test]
    fn inspect_all_records_tolerates_future_node_kind() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let data_dir = temp.path().join("future-kind-store");
        let sink = EmbeddedAletheiaSink::open(&data_dir).expect("embedded store should open");

        // A future KIND must arrive with a future SCHEMA VERSION to be tolerated;
        // an unknown kind at a *known* version is store damage (a hard error, see
        // `inspect_all_records_fails_on_corrupt_record_of_known_version`).
        let future = crate::ir::SCHEMA_VERSION + 1;
        let record_id = format!("codegraph:v{future}:future-kind-repo");
        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", record_id.as_str())
            .insert("record_type", "node")
            .insert("kind", "NewFutureKind")
            .insert("schema_version", i64::from(future))
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
        assert_eq!(unknown.version.version, future);
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

        let future = crate::ir::SCHEMA_VERSION + 1;
        let record_id = format!("codegraph:v{future}:future-kind-repo");
        let properties = ::aletheiadb::PropertyMapBuilder::new()
            .insert("codegraph_id", record_id.as_str())
            .insert("record_type", "node")
            .insert("kind", "NewFutureKind")
            .insert("schema_version", i64::from(future))
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
        assert_eq!(unknown.version.version, future);
    }
}
