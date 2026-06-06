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
#[cfg(feature = "embeddings")]
use ::aletheiadb::api::transaction::WriteOps;

#[cfg(feature = "embeddings")]
const SEMANTIC_INITIAL_CANDIDATE_MULTIPLIER: usize = 8;
#[cfg(feature = "embeddings")]
const SEMANTIC_MAX_CANDIDATE_MULTIPLIER: usize = 64;

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
