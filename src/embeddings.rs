//! Semantic enrichment helpers for embedding code graph records.

use std::collections::BTreeMap;

use crate::ir::{
    EdgeLabel, EmbeddingModel, GraphRecord, MetricKind, NodeKind, SEMANTIC_SCHEMA_VERSION,
    SelectionBasis, SemanticDriftMetadata, TemporalMetadata, semantic_stable_id,
};

/// Re-export of `AletheiaDB`'s embedding boundary when semantic embedding
/// execution is enabled.
#[cfg(feature = "embeddings")]
pub use aletheiadb::embeddings as aletheia_embeddings;

/// Re-export of `embed_anything` through `AletheiaDB`'s public embedding module.
#[cfg(feature = "embeddings")]
pub use aletheiadb::embeddings::embed_anything;

/// Default embedding model name used by the CLI.
pub const DEFAULT_EMBEDDING_MODEL_NAME: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Provider boundary used by the default embedding model identity.
pub const DEFAULT_EMBEDDING_MODEL_PROVIDER: &str = "aletheiadb_re_export";

/// Content hash for providers that do not expose model bytes to this crate.
pub const DEFAULT_EMBEDDING_MODEL_CONTENT_HASH: &str = "unknown";

/// Model architecture passed through to `AletheiaDB`'s embedding boundary.
pub const DEFAULT_EMBEDDING_MODEL_ARCHITECTURE: &str = "bert";

/// Dense vector dimensions for [`DEFAULT_EMBEDDING_MODEL_NAME`].
pub const DEFAULT_EMBEDDING_MODEL_DIMENSIONS: usize = 384;

const DEFAULT_EMBEDDING_MODEL_DIMENSIONS_U32: u32 = 384;

/// Text unit selected for embedding.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EmbeddingCandidate {
    /// Stable graph record ID.
    pub record_id: String,
    /// Candidate target class, currently `file` or `symbol`.
    pub target: String,
    /// Text to send to an embedding model.
    pub text: String,
    /// Repository-relative path when available.
    pub repo_relative_path: Option<String>,
    /// Human-readable name when available.
    pub name: Option<String>,
    /// Git and bitemporal provenance when this candidate came from history replay.
    pub temporal: Option<TemporalMetadata>,
}

/// Embedding vector associated with one candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateVector {
    /// Candidate that was embedded.
    pub candidate: EmbeddingCandidate,
    /// Dense embedding vector.
    pub vector: Vec<f32>,
}

/// Key for storing an embedding against one physical graph observation.
///
/// Current-tree records use only `record_id`. History-backed records include
/// commit and bitemporal fields so two observations of the same stable symbol
/// can keep different vectors.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct EmbeddingVectorKey {
    record_id: String,
    git_commit: Option<String>,
    valid_time: Option<String>,
    observed_at: Option<String>,
}

/// Dense vectors keyed by physical graph observation.
pub type EmbeddingVectorMap = BTreeMap<EmbeddingVectorKey, Vec<f32>>;

impl EmbeddingVectorKey {
    /// Builds a vector key for an embeddable graph node.
    ///
    /// Returns `None` for edges and tombstones because they are not embedding
    /// candidates.
    #[must_use]
    pub fn from_record(record: &GraphRecord) -> Option<Self> {
        let GraphRecord::Node { id, temporal, .. } = record else {
            return None;
        };
        Some(Self::from_parts(id, temporal.as_ref()))
    }

    /// Builds a vector key from a selected embedding candidate.
    #[must_use]
    pub fn from_candidate(candidate: &EmbeddingCandidate) -> Self {
        Self::from_parts(&candidate.record_id, candidate.temporal.as_ref())
    }

    fn from_parts(record_id: &str, temporal: Option<&TemporalMetadata>) -> Self {
        Self {
            record_id: record_id.to_owned(),
            git_commit: temporal.map(|metadata| metadata.git_commit.clone()),
            valid_time: temporal.map(|metadata| metadata.valid_time.clone()),
            observed_at: temporal.map(|metadata| metadata.observed_at.clone()),
        }
    }
}

/// Selects agent-useful file and symbol summaries for semantic embedding.
///
/// Issue #91 also selects agent-memory observation-class nodes (`Observation`,
/// `Decision`, `Failure`) so prior lessons, decisions, and failures become
/// retrievable by meaning. These memory candidates are embedded into the same
/// vector index; recall queries (`eg query semantic-memory`) keep them
/// trust-separated from deterministic code hits at query time by node kind.
#[must_use]
pub fn embedding_candidates(records: &[GraphRecord]) -> Vec<EmbeddingCandidate> {
    let mut candidates = records
        .iter()
        .filter_map(candidate_from_record)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        EmbeddingVectorKey::from_candidate(left).cmp(&EmbeddingVectorKey::from_candidate(right))
    });
    candidates
}

/// Returns the memory `target` class for an agent-memory observation-class node,
/// or `None` for code-graph and other kinds (issue #91).
const fn memory_target(kind: NodeKind) -> Option<&'static str> {
    match kind {
        NodeKind::Observation => Some("observation"),
        NodeKind::Decision => Some("decision"),
        NodeKind::Failure => Some("failure"),
        _ => None,
    }
}

fn candidate_from_record(record: &GraphRecord) -> Option<EmbeddingCandidate> {
    let GraphRecord::Node {
        id,
        kind,
        repo_relative_path,
        name,
        temporal,
        summary,
        text,
        ..
    } = record
    else {
        return None;
    };

    // Agent-memory observation-class nodes embed their authored body text so a
    // lesson is retrievable by meaning even when it names no symbol (issue #91).
    if let Some(target) = memory_target(*kind) {
        let body = text.as_deref().unwrap_or("").trim();
        if body.is_empty() {
            // No meaningful content to embed; skip rather than embed a template.
            return None;
        }
        return Some(EmbeddingCandidate {
            record_id: id.clone(),
            target: target.to_owned(),
            text: body.to_owned(),
            repo_relative_path: repo_relative_path.clone(),
            name: name.clone(),
            temporal: temporal.clone(),
        });
    }

    let target = match kind {
        NodeKind::File => "file",
        NodeKind::Symbol => "symbol",
        NodeKind::Repository
        | NodeKind::Module
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
        // The scan-coverage summary is counts, not source text (issue #135).
        | NodeKind::ScanCoverage
        // Log-signature nodes carry no source bytes to embed (issues #319/#320).
        | NodeKind::LogSource
        | NodeKind::ErrorSignature
        | NodeKind::LogEvent
        | NodeKind::LogOccurrenceBucket => return None,
    };

    Some(EmbeddingCandidate {
        record_id: id.clone(),
        target: target.to_owned(),
        text: candidate_text(summary, repo_relative_path.as_deref(), name.as_deref()),
        repo_relative_path: repo_relative_path.clone(),
        name: name.clone(),
        temporal: temporal.clone(),
    })
}

/// Emits semantic drift graph records from consecutive candidate vectors for
/// the same file or symbol over Git history.
#[must_use]
pub fn semantic_drift_records(
    vectors: &[CandidateVector],
    model_name: &str,
    threshold: f64,
) -> Vec<GraphRecord> {
    let mut groups = BTreeMap::<String, Vec<&CandidateVector>>::new();
    for vector in vectors {
        if vector.candidate.temporal.is_some() {
            groups
                .entry(entity_key(&vector.candidate))
                .or_default()
                .push(vector);
        }
    }

    let mut records = Vec::new();
    for group in groups.values_mut() {
        group.sort_by(|left, right| {
            let left_temporal = left.candidate.temporal.as_ref();
            let right_temporal = right.candidate.temporal.as_ref();
            left_temporal
                .map(|temporal| (&temporal.valid_time, &temporal.git_commit))
                .cmp(&right_temporal.map(|temporal| (&temporal.valid_time, &temporal.git_commit)))
        });

        for pair in group.windows(2) {
            let [before, after] = pair else {
                continue;
            };
            let Some(score) = cosine_distance(&before.vector, &after.vector) else {
                continue;
            };
            if f64::from(score) < threshold {
                continue;
            }
            records.extend(drift_pair_records(
                before, after, model_name, threshold, score,
            ));
        }
    }

    records.sort_by(|left, right| left.id().cmp(right.id()));
    records
}

fn candidate_text(summary: &str, repo_relative_path: Option<&str>, name: Option<&str>) -> String {
    let mut parts = Vec::from([summary.to_owned()]);
    if let Some(path) = repo_relative_path {
        parts.push(format!("path: {path}"));
    }
    if let Some(name) = name {
        parts.push(format!("name: {name}"));
    }
    parts.join("\n")
}

fn drift_pair_records(
    before: &CandidateVector,
    after: &CandidateVector,
    model_name: &str,
    threshold: f64,
    score: f32,
) -> Vec<GraphRecord> {
    let Some(before_temporal) = before.candidate.temporal.as_ref() else {
        return Vec::new();
    };
    let Some(after_temporal) = after.candidate.temporal.as_ref() else {
        return Vec::new();
    };

    let embedding_model = EmbeddingModel {
        provider: DEFAULT_EMBEDDING_MODEL_PROVIDER.to_owned(),
        name: model_name.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        dim: DEFAULT_EMBEDDING_MODEL_DIMENSIONS_U32,
        content_hash: DEFAULT_EMBEDDING_MODEL_CONTENT_HASH.to_owned(),
    };
    let metric_kind = MetricKind::CosineDistance;
    let selection_basis = SelectionBasis::ThresholdOnly;
    let score = f64::from(score);
    let selection_threshold = threshold;
    let selection_threshold_id = selection_threshold.to_string();
    let drift_id = semantic_stable_id(&[
        "semantic",
        "semantic_drift",
        &embedding_model.provider,
        &embedding_model.name,
        &embedding_model.version,
        &embedding_model.content_hash,
        metric_kind.as_str(),
        &selection_threshold_id,
        &before.candidate.record_id,
        &after.candidate.record_id,
        &before_temporal.git_commit,
        &after_temporal.git_commit,
    ]);
    let drift = SemanticDriftMetadata {
        embedding_model,
        target_record_id: after.candidate.record_id.clone(),
        prior_record_id: before.candidate.record_id.clone(),
        before_git_commit: before_temporal.git_commit.clone(),
        after_git_commit: after_temporal.git_commit.clone(),
        before_valid_time: before_temporal.valid_time.clone(),
        after_valid_time: after_temporal.valid_time.clone(),
        metric_kind,
        score,
        selection_threshold,
        selection_basis,
    };
    let node = GraphRecord::node(
        drift_id.clone(),
        NodeKind::SemanticDrift,
        after.candidate.repo_relative_path.clone(),
        None,
        after
            .candidate
            .name
            .clone()
            .or_else(|| after.candidate.repo_relative_path.clone()),
        format!(
            "Semantic drift for {} from {} to {} scored {score}",
            after.candidate.record_id, before_temporal.git_commit, after_temporal.git_commit
        ),
    )
    .with_temporal(after_temporal.clone())
    .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
    .with_node_time(
        after_temporal.valid_time.clone(),
        "after_valid_time",
        after_temporal.observed_at.clone(),
    )
    .with_semantic_drift(drift);
    let target_edge = semantic_edge(
        EdgeLabel::DriftsFrom,
        drift_id.clone(),
        after.candidate.record_id.clone(),
        Some("1.0".to_owned()),
        "Semantic drift measurement target".to_owned(),
    )
    .with_temporal(after_temporal.clone());

    let prior_edge = semantic_edge(
        EdgeLabel::DriftsPrior,
        drift_id,
        before.candidate.record_id.clone(),
        Some("1.0".to_owned()),
        "Semantic drift prior measurement target".to_owned(),
    )
    .with_temporal(before_temporal.clone());

    vec![node, target_edge, prior_edge]
}

fn semantic_edge(
    label: EdgeLabel,
    source: String,
    target: String,
    confidence: Option<String>,
    summary: String,
) -> GraphRecord {
    let id = semantic_stable_id(&["edge", label.as_str(), &source, &target]);
    GraphRecord::Edge {
        id,
        schema_version: SEMANTIC_SCHEMA_VERSION,
        label,
        source,
        target,
        confidence,
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        basis: None,
        is_exhaustive: None,
        temporal: None,
        summary,
        producer: None,
    }
}

fn entity_key(candidate: &EmbeddingCandidate) -> String {
    format!(
        "{}\0{}\0{}",
        candidate.target,
        candidate.repo_relative_path.as_deref().unwrap_or(""),
        candidate.name.as_deref().unwrap_or("")
    )
}

fn cosine_distance(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }

    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (left_value, right_value) in left.iter().zip(right) {
        dot += left_value * right_value;
        left_norm += left_value * left_value;
        right_norm += right_value * right_value;
    }

    if left_norm == 0.0 || right_norm == 0.0 {
        return None;
    }

    let similarity = dot / (left_norm.sqrt() * right_norm.sqrt());
    Some((1.0 - similarity).clamp(0.0, 2.0))
}

// ---------------------------------------------------------------------------
// Issue #104: query-time vector-space compatibility gate
//
// The semantic index stores vectors plus a dimension. Two different models can
// share a dimension, so a dimension check alone lets a model swap, cache change,
// or version bump produce a cosine ranking computed ACROSS INCOMPATIBLE VECTOR
// SPACES and return it as a confident answer. This module persists the producing
// model's identity at `--embed` time and refuses at query time when the query
// embedder's identity does not match it.
//
// Everything below is pure and feature-independent so the whole verification
// matrix (`--no-default-features` included) exercises it, and so a refusal costs
// no embedding-model load.
// ---------------------------------------------------------------------------

/// Exit code for `embedding_identity_unrecorded` — the vector index exists but
/// carries no recorded model identity, so compatibility is UNVERIFIABLE.
pub const EMBEDDING_IDENTITY_UNRECORDED_EXIT_CODE: i32 = 7;

/// Exit code for `embedding_identity_ambiguous` — the index records more than
/// one distinct producing model, so its vectors span several spaces.
pub const EMBEDDING_IDENTITY_AMBIGUOUS_EXIT_CODE: i32 = 8;

/// Exit code for `embedding_dimension_mismatch`.
pub const EMBEDDING_DIMENSION_MISMATCH_EXIT_CODE: i32 = 9;

/// Exit code for `embedding_model_mismatch` — same dimension, different model.
pub const EMBEDDING_MODEL_MISMATCH_EXIT_CODE: i32 = 10;

/// Exit code for `semantic_index_unreadable` — the store holds persisted
/// vector-index files the engine did not load (issue #489).
///
/// A NEW case, not a renumbering: `7`/`8`/`9`/`10` and the exit-`2`
/// `semantic_index_absent` outcome all keep their meanings.
pub const SEMANTIC_INDEX_UNREADABLE_EXIT_CODE: i32 = 11;

/// Operator remedy carried by every identity refusal.
///
/// Points at re-ingest, never at editing the store: the store is the record of
/// what was actually embedded, and mutating it would replace a detectable
/// incompatibility with a silent lie.
pub const EMBEDDING_IDENTITY_REMEDY: &str = "re-ingest the graph into a fresh --data-dir with `eg ingest <graph> --adapter embedded \
     --data-dir <NEW_DIR> --embed` so the index and the query embedder share one vector space; \
     do not edit the store by hand";

/// Operator remedy carried by the unreadable-index refusal (issue #489).
///
/// Distinct from [`EMBEDDING_IDENTITY_REMEDY`] because this state carries an
/// extra hazard the identity refusals do not: re-embedding into THIS store would
/// enable a fresh, EMPTY vector index over the skipped files, and the next
/// persistence cycle would overwrite them — permanently destroying the vectors
/// a repair could otherwise have rebuilt. `eg ingest --embed` therefore refuses
/// this store rather than performing that overwrite.
pub const SEMANTIC_INDEX_UNREADABLE_REMEDY: &str = "re-ingest the graph into a FRESH --data-dir with `eg ingest <graph> --adapter embedded \
     --data-dir <NEW_DIR> --embed`; do not re-embed into this store and do not edit it by hand — \
     enabling a vector index over skipped index files creates an EMPTY index whose next \
     persistence cycle overwrites them, permanently losing the indexed vectors";

/// The identity fields compared by the compatibility gate, in the fixed order
/// they are reported. `dim` is handled separately because a dimension mismatch
/// is its own distinct verdict (AC4).
const COMPARED_IDENTITY_FIELDS: [&str; 4] = ["provider", "name", "version", "content_hash"];

/// Builds the embedding-model identity the CLI's default embedder corresponds
/// to, from the same compile-time constants [`crate::cli`] builds the embedder
/// from.
///
/// Shared by the semantic-drift records and the vector-index identity node so
/// the two can never describe the "same" model differently.
#[must_use]
pub fn default_embedding_model_identity(dim: usize) -> EmbeddingModel {
    EmbeddingModel {
        provider: DEFAULT_EMBEDDING_MODEL_PROVIDER.to_owned(),
        name: DEFAULT_EMBEDDING_MODEL_NAME.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        dim: u32::try_from(dim).unwrap_or(u32::MAX),
        content_hash: DEFAULT_EMBEDDING_MODEL_CONTENT_HASH.to_owned(),
    }
}

/// Stable record ID of a store's vector-index identity node (issue #104).
///
/// FIXED, not identity-derived: a store has exactly ONE physical vector index,
/// so it has exactly ONE identity, and a later `--embed` write supersedes the
/// earlier one at the same ID (latest-write-wins).
///
/// Deriving the ID from the identity tuple instead would let two identity
/// records coexist — and because `eg forget` refuses every semantic-domain
/// record and `eg forget-repo` never evicts a repo-agnostic one, that state
/// would be UNRECOVERABLE: every semantic query would refuse forever with no
/// in-place remedy. A fixed ID makes every bad state recoverable by re-running
/// `--embed`, while the write-time conflict refusal
/// ([`crate::cli::refuse_conflicting_index_identity`]) is what actually prevents
/// a mixed-model index from being created in the first place.
#[must_use]
pub fn embedding_index_identity_id() -> String {
    semantic_stable_id(&["node", "embedding_model", "vector_index"])
}

/// Builds the `EmbeddingModel` node recording which model produced a store's
/// queryable vector index (issue #104).
///
/// Keyed on the fixed [`embedding_index_identity_id`], so re-embedding
/// supersedes the prior identity rather than accumulating a second record.
#[must_use]
pub fn embedding_index_identity_record(model: &EmbeddingModel) -> GraphRecord {
    GraphRecord::node(
        embedding_index_identity_id(),
        NodeKind::EmbeddingModel,
        None,
        None,
        Some(model.name.clone()),
        format!(
            "Embedding model identity for the queryable vector index: {} {} {} dim={} hash={}",
            model.provider, model.name, model.version, model.dim, model.content_hash
        ),
    )
    .with_domain("semantic", SEMANTIC_SCHEMA_VERSION)
    .with_embedding_model(model.clone())
    // Stamped explicitly: this record is appended AFTER `Graph::stamp_producer`
    // has run over the scanned graph, so without this it would be the one
    // producer-less record in an `--embed` store and would show up under
    // `legacy_pre_v1` in `eg inspect` and `eg query producer-drift`.
    // `drift_engine` is the semantic/embedding producer, whose contract is to
    // populate `embedding_model_id` (`docs/schema/producer-version.md`); it sits
    // in producer-drift's never-flagged non-code bucket, which is correct — a
    // stale identity is fixed by re-embedding, not by re-extraction.
    .with_producer(crate::ir::Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: crate::ir::ProducerKind::DriftEngine,
        producer_components: BTreeMap::from([(
            "embedding_model_id".to_owned(),
            format!("{}/{}", model.provider, model.name),
        )]),
        producer_started_at: crate::PROCESS_STARTED_AT.clone(),
    })
}

/// Collects the distinct, live embedding-model identities recorded for a store's
/// vector index.
///
/// Two collapses happen, in this order, and both matter:
///
/// 1. **Per record ID, latest write wins.** A `--graph` JSONL is an append-only
///    history and can carry several physical versions of one identity ID; only
///    the newest is that ID's current identity. Without this, superseding the
///    fixed-ID identity record would look like TWO identities and produce a
///    spurious `embedding_identity_ambiguous` refusal.
/// 2. **Across IDs, dedupe by the identity tuple**, so two records that describe
///    the very same model count once.
///
/// Liveness follows the shared latest-write-wins gate so `--graph` and
/// `--data-dir` agree on a retracted identity record. The result is
/// deterministically ordered, so downstream diagnostics are byte-stable.
#[must_use]
pub fn indexed_identities(records: &[GraphRecord]) -> Vec<EmbeddingModel> {
    let liveness = crate::query::liveness::Liveness::new(records);
    // Append order is write order, so a later entry for the same ID overwrites.
    let mut latest_per_id: BTreeMap<&str, &EmbeddingModel> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::EmbeddingModel,
            embedding_model: Some(model),
            ..
        } = record
        else {
            continue;
        };
        if liveness.deleted(id) {
            continue;
        }
        latest_per_id.insert(id.as_str(), model.as_ref());
    }
    let mut seen: BTreeMap<(&str, &str, &str, u32, &str), &EmbeddingModel> = BTreeMap::new();
    for model in latest_per_id.into_values() {
        seen.insert(
            (
                model.provider.as_str(),
                model.name.as_str(),
                model.version.as_str(),
                model.dim,
                model.content_hash.as_str(),
            ),
            model,
        );
    }
    seen.into_values().cloned().collect()
}

/// Returns the identity fields on which two embedding-model identities differ,
/// in a fixed declared order so diagnostics are byte-identical across runs.
///
/// `dim` is excluded: a dimension difference is reported as its own distinct
/// verdict rather than folded into the same-dimension mismatch case (AC4).
#[must_use]
pub fn differing_identity_fields(
    left: &EmbeddingModel,
    right: &EmbeddingModel,
) -> Vec<&'static str> {
    let values: [(&str, bool); 4] = [
        ("provider", left.provider != right.provider),
        ("name", left.name != right.name),
        ("version", left.version != right.version),
        ("content_hash", left.content_hash != right.content_hash),
    ];
    debug_assert_eq!(
        values.map(|(field, _)| field),
        COMPARED_IDENTITY_FIELDS,
        "comparison order must match the declared field order"
    );
    values
        .into_iter()
        .filter_map(|(field, differs)| differs.then_some(field))
        .collect()
}

/// What a store's `"embedding"` vector index actually IS, as observed at open
/// time (issue #489).
///
/// `AletheiaDB` 0.2.0 loads per-property vector indexes in parallel WITH ERROR
/// ISOLATION: a corrupted or unreadable index is skipped with a warning instead
/// of aborting the load of the remaining indexes. A skipped index is simply
/// absent from `list_vector_indexes()`, so a dimension probe alone cannot tell
/// "this store was never `--embed`ed" from "this store's index exists on disk
/// and failed to load" — and reporting the second as the first is a data-loss
/// condition dressed up as a benign configuration one. This three-way state is
/// what keeps the two apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorIndexState {
    /// The index loaded and is queryable, holding `dimensions`-wide vectors.
    Loaded {
        /// Physical dimensionality of the loaded index.
        dimensions: usize,
    },
    /// Persisted vector-index state for the property EXISTS on disk but the
    /// engine did not register the property — it was skipped at load as
    /// corrupted or unreadable. Present-but-unreadable, never "absent".
    Unreadable {
        /// The known index artifacts found on disk, in a fixed declared order.
        ///
        /// `&'static str` by construction: these are matched against a fixed
        /// table of `AletheiaDB` index filenames, so no operator-controlled
        /// filename can ever reach a diagnostic through this field. May be
        /// empty when the property directory exists but holds none of them —
        /// which is itself the skip condition, since the loader requires
        /// `meta.idx`.
        artifacts: Vec<&'static str>,
    },
    /// The store has no vector index for the property and no persisted state
    /// for one — it was never ingested with `--embed`.
    Absent,
}

impl VectorIndexState {
    /// Physical dimensionality of the index, or `None` when it did not load.
    ///
    /// `None` deliberately conflates [`Self::Unreadable`] and [`Self::Absent`]:
    /// callers that only need "can I search this?" get one answer, while the
    /// callers that must report WHY match on the state itself.
    #[must_use]
    pub const fn dimensions(&self) -> Option<usize> {
        match self {
            Self::Loaded { dimensions } => Some(*dimensions),
            Self::Unreadable { .. } | Self::Absent => None,
        }
    }

    /// Stable machine-readable status label for the three-way state.
    #[must_use]
    pub const fn status(&self) -> &'static str {
        match self {
            Self::Loaded { .. } => "loaded",
            Self::Unreadable { .. } => "unreadable",
            Self::Absent => "absent",
        }
    }

    /// Whether the store holds a semantic index at all — loaded or merely
    /// present on disk.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Loaded { .. } | Self::Unreadable { .. })
    }
}

/// Verdict of the query-time vector-space compatibility gate (issue #104).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexCompatibility {
    /// The query embedder and the index share one vector space; proceed
    /// unchanged.
    Compatible,
    /// The store carries no vector index at all — it was never ingested with
    /// `--embed`. This is NOT an identity failure: telling an operator their
    /// identity is unverifiable when they simply never embedded would be a false
    /// refusal, so this keeps the pre-existing "no semantic index" outcome.
    IndexAbsent,
    /// The vector index EXISTS on disk but the engine skipped it at load as
    /// corrupted or unreadable (issue #489).
    ///
    /// Refused rather than reported as [`Self::IndexAbsent`]: "your embeddings
    /// were never built" when in fact they exist and are damaged is a data-loss
    /// condition reported as a benign configuration one — the same class of
    /// dishonesty [`Self::IdentityUnrecorded`] exists to prevent.
    IndexUnreadable {
        /// Known index artifacts found on disk, in a fixed declared order.
        artifacts: Vec<&'static str>,
        /// Every recorded index identity (possibly empty), deterministically
        /// ordered — which model built the index that no longer loads.
        indexed: Vec<EmbeddingModel>,
        /// The query embedder's identity.
        query: EmbeddingModel,
    },
    /// The vector index exists but records no model identity (a legacy store
    /// embedded before identity stamping). Compatibility is UNVERIFIABLE — never
    /// silently assumed compatible.
    IdentityUnrecorded {
        /// Dimensionality of the physical vector index.
        index_dimensions: usize,
        /// The query embedder's identity.
        query: EmbeddingModel,
    },
    /// The index records more than one distinct producing model, so its vectors
    /// span several spaces and no single ranking is meaningful.
    IdentityAmbiguous {
        /// Every distinct recorded identity, deterministically ordered.
        indexed: Vec<EmbeddingModel>,
        /// The query embedder's identity.
        query: EmbeddingModel,
    },
    /// The query embedder produces vectors of a different dimensionality than
    /// the index holds.
    DimensionMismatch {
        /// Dimensionality of the physical vector index.
        index_dimensions: usize,
        /// Dimensionality the query embedder produces.
        query_dimensions: usize,
        /// EVERY recorded index identity (possibly empty), deterministically
        /// ordered. All are reported rather than an arbitrary first, because the
        /// dimension check runs before the ambiguity check: a store written by
        /// several models AND mismatched on dimension must still disclose every
        /// candidate, per the no-silent-winner doctrine the `CALLS` resolution
        /// ladder and `who-constructs` follow.
        indexed: Vec<EmbeddingModel>,
        /// The query embedder's identity.
        query: EmbeddingModel,
    },
    /// Same dimension, different model — the silent-failure case this gate
    /// exists for.
    ModelMismatch {
        /// The recorded index identity.
        indexed: EmbeddingModel,
        /// The query embedder's identity.
        query: EmbeddingModel,
        /// Identity fields that differ, in declared order.
        differing_fields: Vec<&'static str>,
    },
}

impl IndexCompatibility {
    /// Returns the stable machine-readable code for a refusal, or `None` for the
    /// two non-refusal verdicts ([`Self::Compatible`] and [`Self::IndexAbsent`]).
    #[must_use]
    pub const fn code(&self) -> Option<&'static str> {
        match self {
            Self::Compatible | Self::IndexAbsent => None,
            Self::IndexUnreadable { .. } => Some("semantic_index_unreadable"),
            Self::IdentityUnrecorded { .. } => Some("embedding_identity_unrecorded"),
            Self::IdentityAmbiguous { .. } => Some("embedding_identity_ambiguous"),
            Self::DimensionMismatch { .. } => Some("embedding_dimension_mismatch"),
            Self::ModelMismatch { .. } => Some("embedding_model_mismatch"),
        }
    }

    /// Returns the distinct nonzero exit code for a refusal, or `None` for the
    /// two non-refusal verdicts.
    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Compatible | Self::IndexAbsent => None,
            Self::IndexUnreadable { .. } => Some(SEMANTIC_INDEX_UNREADABLE_EXIT_CODE),
            Self::IdentityUnrecorded { .. } => Some(EMBEDDING_IDENTITY_UNRECORDED_EXIT_CODE),
            Self::IdentityAmbiguous { .. } => Some(EMBEDDING_IDENTITY_AMBIGUOUS_EXIT_CODE),
            Self::DimensionMismatch { .. } => Some(EMBEDDING_DIMENSION_MISMATCH_EXIT_CODE),
            Self::ModelMismatch { .. } => Some(EMBEDDING_MODEL_MISMATCH_EXIT_CODE),
        }
    }

    /// Returns `true` when this verdict refuses the query.
    #[must_use]
    pub const fn is_refusal(&self) -> bool {
        self.code().is_some()
    }

    /// Operator remedy carried by this verdict's refusal envelope.
    ///
    /// Every identity refusal points at a fresh-`--data-dir` re-ingest; the
    /// unreadable-index verdict points at the same re-ingest PLUS the explicit
    /// warning that re-embedding into the damaged store would overwrite the
    /// skipped index files (issue #489).
    ///
    /// Spelled out rather than wildcarded so a future verdict must choose its
    /// remedy explicitly.
    #[must_use]
    pub const fn remedy(&self) -> &'static str {
        match self {
            Self::IndexUnreadable { .. } => SEMANTIC_INDEX_UNREADABLE_REMEDY,
            Self::Compatible
            | Self::IndexAbsent
            | Self::IdentityUnrecorded { .. }
            | Self::IdentityAmbiguous { .. }
            | Self::DimensionMismatch { .. }
            | Self::ModelMismatch { .. } => EMBEDDING_IDENTITY_REMEDY,
        }
    }

    /// Renders the refusal as a stable machine-readable envelope, or `None` for
    /// a non-refusal verdict.
    ///
    /// Allow-list only (AC7): provider, name, version, dim, content hash,
    /// dimensions, differing-field labels, the stable code, and the operator
    /// remedy. Never model bytes, vectors, payloads, or the query text.
    #[must_use]
    pub fn to_error_envelope(&self) -> Option<serde_json::Value> {
        let code = self.code()?;
        // Built as a `Map` rather than mutated through `as_object_mut()` so the
        // renderer carries no panic path at all.
        let mut map = serde_json::Map::new();
        map.insert("code".to_owned(), code.into());
        map.insert("message".to_owned(), self.message().into());
        map.insert("remedy".to_owned(), self.remedy().into());
        match self {
            // `code()` already returned `None` for these, so this arm is dead;
            // it is spelled out rather than wildcarded so a future verdict must
            // be classified here explicitly.
            Self::Compatible | Self::IndexAbsent => return None,
            Self::IndexUnreadable {
                artifacts,
                indexed,
                query,
            } => {
                // `&'static str` values drawn from a fixed filename table, so
                // no operator-controlled text reaches the envelope here.
                map.insert("index_artifacts".to_owned(), serde_json::json!(artifacts));
                map.insert("indexed_models".to_owned(), identities_json(indexed));
                map.insert("query_model".to_owned(), identity_json(query));
            }
            Self::IdentityUnrecorded {
                index_dimensions,
                query,
            } => {
                map.insert("index_dimensions".to_owned(), (*index_dimensions).into());
                map.insert("query_model".to_owned(), identity_json(query));
            }
            Self::IdentityAmbiguous { indexed, query } => {
                map.insert("indexed_models".to_owned(), identities_json(indexed));
                map.insert("query_model".to_owned(), identity_json(query));
            }
            Self::DimensionMismatch {
                index_dimensions,
                query_dimensions,
                indexed,
                query,
            } => {
                map.insert("index_dimensions".to_owned(), (*index_dimensions).into());
                map.insert("query_dimensions".to_owned(), (*query_dimensions).into());
                // Plural, and always present (possibly empty): the dimension
                // check precedes the ambiguity check, so this arm can see several
                // recorded identities and must disclose all of them rather than
                // silently pick one.
                map.insert("indexed_models".to_owned(), identities_json(indexed));
                map.insert("query_model".to_owned(), identity_json(query));
                map.insert("differing_fields".to_owned(), serde_json::json!(["dim"]));
            }
            Self::ModelMismatch {
                indexed,
                query,
                differing_fields,
            } => {
                map.insert("indexed_model".to_owned(), identity_json(indexed));
                map.insert("query_model".to_owned(), identity_json(query));
                map.insert(
                    "differing_fields".to_owned(),
                    serde_json::json!(differing_fields),
                );
            }
        }
        Some(serde_json::json!({ "ok": false, "error": serde_json::Value::Object(map) }))
    }

    /// Human-readable one-line summary naming both identities.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Compatible => "query embedder matches the indexed embedding model".to_owned(),
            Self::IndexAbsent => "store has no semantic vector index".to_owned(),
            Self::IndexUnreadable {
                artifacts, indexed, ..
            } => format!(
                "this store's semantic vector index EXISTS on disk ({}) but the engine did not \
                 load it — it was skipped as corrupted or unreadable, so the index is \
                 present-but-unreadable, NOT absent{}",
                if artifacts.is_empty() {
                    "its index directory is present but holds none of the expected files".to_owned()
                } else {
                    format!("persisted index files: {}", artifacts.join(", "))
                },
                if indexed.is_empty() {
                    String::new()
                } else {
                    format!("; it was built by {}", describe_identities(indexed))
                }
            ),
            Self::IdentityUnrecorded {
                index_dimensions,
                query,
            } => format!(
                "the semantic index (dim {index_dimensions}) records no embedding-model identity, \
                 so its compatibility with the query embedder {} is UNVERIFIABLE; refusing rather \
                 than assuming they match",
                describe_identity(query)
            ),
            Self::IdentityAmbiguous { indexed, .. } => format!(
                "the semantic index records {} distinct embedding-model identities ({}), so its \
                 vectors span several spaces and no single ranking is meaningful",
                indexed.len(),
                describe_identities(indexed)
            ),
            // `index_dimensions == query_dimensions` can only reach a dimension
            // refusal through the recorded-identity contradiction branch, so the
            // two cases are worded distinctly rather than emitting a message
            // that reads as self-contradictory.
            Self::DimensionMismatch {
                index_dimensions,
                query_dimensions,
                indexed,
                ..
            } if index_dimensions == query_dimensions => format!(
                "the recorded embedding-model identity ({}) contradicts the physical semantic \
                 index it describes, which holds {index_dimensions}-dimensional vectors; the index \
                 cannot be trusted to describe itself, so refusing",
                describe_identities(indexed)
            ),
            Self::DimensionMismatch {
                index_dimensions,
                query_dimensions,
                indexed,
                query,
            } => format!(
                "the semantic index holds {index_dimensions}-dimensional vectors{} but the query \
                 embedder {} produces {query_dimensions}-dimensional vectors",
                if indexed.is_empty() {
                    String::new()
                } else {
                    format!(" recorded as from {}", describe_identities(indexed))
                },
                describe_identity(query)
            ),
            Self::ModelMismatch {
                indexed,
                query,
                differing_fields,
            } => format!(
                "the semantic index was produced by {} but the query embedder is {}; they share a \
                 dimension yet differ on {} — a cosine ranking across these vector spaces would be \
                 meaningless",
                describe_identity(indexed),
                describe_identity(query),
                differing_fields.join(", ")
            ),
        }
    }
}

/// Maximum characters of any single identity field that reach a diagnostic.
///
/// The identity fields are allow-listed by NAME, but their VALUES are read back
/// from the store and are therefore attacker- or operator-controlled: a
/// hand-authored `EmbeddingModel` record can carry megabytes of text, embedded
/// newlines, or ANSI escapes, and the write path runs no redaction validation on
/// this kind. Every other free-text field that reaches a diagnostic in this
/// codebase is bounded the same way (compare `EXCERPT_MAX_CHARS` /
/// `FRAME_TEXT_MAX_CHARS` in `crate::log_graph`).
pub const IDENTITY_FIELD_MAX_CHARS: usize = 128;

/// Maximum recorded identities enumerated in one diagnostic, so a store carrying
/// many identity records cannot produce an unbounded message.
pub const IDENTITY_LIST_MAX: usize = 8;

/// Bounds and sanitizes one identity field for output.
///
/// Control characters (including newline, carriage return, and the ESC that
/// starts an ANSI sequence) become `.`, so a crafted identity can neither forge
/// extra output lines nor drive a terminal; the result is then truncated on a
/// CHARACTER boundary with an explicit `…` marker so truncation is visible
/// rather than silent. Deterministic: same input, same output, always.
#[must_use]
pub fn bounded_identity_field(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| if c.is_control() { '.' } else { c })
        .collect();
    if sanitized.chars().count() <= IDENTITY_FIELD_MAX_CHARS {
        return sanitized;
    }
    let mut out: String = sanitized.chars().take(IDENTITY_FIELD_MAX_CHARS).collect();
    out.push('…');
    out
}

/// Sanitizes one HANDLE value — a record ID, repo-relative path, or manifest
/// citation — read back from a store or graph, **without** truncating it.
///
/// Control characters are neutralized for the same reason as in
/// [`bounded_identity_field`]: the value is operator-controlled, and a newline
/// or ANSI escape reaching a text render forges output lines or drives the
/// reader's terminal. The length cap is deliberately NOT applied, because a
/// truncated handle stops being a citation — an agent cannot look it up, and a
/// prefix of a record ID silently reads like a valid one.
///
/// This is the shared implementation of the two-tier split this codebase draws
/// everywhere it renders store-read values (compare
/// `crate::criteria_coverage`'s `bounded_field` / `handle_field`, and
/// `crate::evidence_pack`'s `sanitize_catalog_text` / `bounded_catalog_field`).
#[must_use]
pub fn sanitized_handle(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { '.' } else { c })
        .collect()
}

/// Renders one identity as the allow-listed JSON object (AC7).
///
/// Field values are bounded and sanitized via [`bounded_identity_field`]; only
/// `dim` (an integer) passes through unfiltered.
fn identity_json(model: &EmbeddingModel) -> serde_json::Value {
    serde_json::json!({
        "provider": bounded_identity_field(&model.provider),
        "name": bounded_identity_field(&model.name),
        "version": bounded_identity_field(&model.version),
        "dim": model.dim,
        "content_hash": bounded_identity_field(&model.content_hash),
    })
}

/// Renders one identity as a bounded human-readable handle for message text.
fn describe_identity(model: &EmbeddingModel) -> String {
    format!(
        "{}/{}@{} (dim {}, hash {})",
        bounded_identity_field(&model.provider),
        bounded_identity_field(&model.name),
        bounded_identity_field(&model.version),
        model.dim,
        bounded_identity_field(&model.content_hash)
    )
}

/// Renders up to [`IDENTITY_LIST_MAX`] identities as an allow-listed JSON array,
/// disclosing any remainder under an explicit `omitted` marker rather than
/// silently truncating.
fn identities_json(models: &[EmbeddingModel]) -> serde_json::Value {
    let mut rendered: Vec<serde_json::Value> = models
        .iter()
        .take(IDENTITY_LIST_MAX)
        .map(identity_json)
        .collect();
    if let Some(rest) = models.len().checked_sub(IDENTITY_LIST_MAX)
        && rest > 0
    {
        rendered.push(serde_json::json!({ "omitted": rest }));
    }
    serde_json::Value::Array(rendered)
}

/// Renders up to [`IDENTITY_LIST_MAX`] identities for message text, disclosing
/// any remainder rather than silently dropping it.
fn describe_identities(models: &[EmbeddingModel]) -> String {
    let shown = models
        .iter()
        .take(IDENTITY_LIST_MAX)
        .map(describe_identity)
        .collect::<Vec<_>>()
        .join(", ");
    match models.len().checked_sub(IDENTITY_LIST_MAX) {
        Some(rest) if rest > 0 => format!("{shown}, and {rest} more"),
        _ => shown,
    }
}

/// Decides whether a semantic query may proceed against a store's vector index
/// (issue #104).
///
/// `state` is what the store's vector index actually is (issue #489); `indexed`
/// is the set of distinct live recorded identities from [`indexed_identities`];
/// `query` is the identity of the embedder the query will be embedded with.
///
/// Precedence is deliberate:
/// 1. index files present but not loaded →
///    [`IndexCompatibility::IndexUnreadable`], checked FIRST because a skipped
///    index has no dimension to compare and must never fall through to the
///    "never embedded" outcome;
/// 2. no index at all → [`IndexCompatibility::IndexAbsent`] (not an identity
///    failure — the store was simply never `--embed`ed);
/// 3. the physical index dimension disagrees with the query embedder → a
///    dimension mismatch, checked against the vectors themselves because that is
///    the ground truth a recorded identity could contradict;
/// 4. no recorded identity → unverifiable;
/// 5. several recorded identities → ambiguous;
/// 6. a recorded `dim` disagreeing with the physical index → still a dimension
///    problem, never silently compatible;
/// 7. any other identity field differing → the same-dimension model mismatch.
#[must_use]
pub fn classify_index_compatibility(
    state: &VectorIndexState,
    indexed: &[EmbeddingModel],
    query: &EmbeddingModel,
) -> IndexCompatibility {
    let index_dimensions = match state {
        VectorIndexState::Loaded { dimensions } => *dimensions,
        VectorIndexState::Unreadable { artifacts } => {
            return IndexCompatibility::IndexUnreadable {
                artifacts: artifacts.clone(),
                indexed: indexed.to_vec(),
                query: query.clone(),
            };
        }
        VectorIndexState::Absent => return IndexCompatibility::IndexAbsent,
    };
    let query_dimensions = query.dim as usize;
    if index_dimensions != query_dimensions {
        return IndexCompatibility::DimensionMismatch {
            index_dimensions,
            query_dimensions,
            indexed: indexed.to_vec(),
            query: query.clone(),
        };
    }
    if indexed.len() > 1 {
        return IndexCompatibility::IdentityAmbiguous {
            indexed: indexed.to_vec(),
            query: query.clone(),
        };
    }
    let Some(recorded) = indexed.first() else {
        return IndexCompatibility::IdentityUnrecorded {
            index_dimensions,
            query: query.clone(),
        };
    };
    // The recorded identity contradicts the physical index it claims to
    // describe. The index cannot be trusted to describe itself, so refuse —
    // `message()` words this case distinctly (it is the only way
    // `index_dimensions == query_dimensions` can reach a dimension refusal).
    if recorded.dim != query.dim {
        return IndexCompatibility::DimensionMismatch {
            index_dimensions,
            query_dimensions,
            indexed: vec![recorded.clone()],
            query: query.clone(),
        };
    }
    let differing_fields = differing_identity_fields(recorded, query);
    if differing_fields.is_empty() {
        IndexCompatibility::Compatible
    } else {
        IndexCompatibility::ModelMismatch {
            indexed: recorded.clone(),
            query: query.clone(),
            differing_fields,
        }
    }
}

#[cfg(test)]
mod index_identity_tests {
    use super::*;

    fn model(name: &str, dim: u32) -> EmbeddingModel {
        EmbeddingModel {
            provider: "aletheiadb_re_export".to_owned(),
            name: name.to_owned(),
            version: "0.1.0".to_owned(),
            dim,
            content_hash: "unknown".to_owned(),
        }
    }

    /// A queryable vector index holding `dim`-wide vectors.
    const fn loaded(dim: usize) -> VectorIndexState {
        VectorIndexState::Loaded { dimensions: dim }
    }

    /// An index whose files are on disk but which the engine skipped at load.
    fn unreadable() -> VectorIndexState {
        VectorIndexState::Unreadable {
            artifacts: vec!["meta.idx", "mappings.idx"],
        }
    }

    /// AC5: matching identities proceed untouched.
    #[test]
    fn matching_identity_is_compatible() {
        let q = model("m", 384);
        assert_eq!(
            classify_index_compatibility(&loaded(384), std::slice::from_ref(&q), &q),
            IndexCompatibility::Compatible
        );
    }

    /// AC3: same dimension, different model name → refuse, naming the field.
    #[test]
    fn same_dim_different_name_is_model_mismatch() {
        let indexed = model("model-a", 384);
        let query = model("model-b", 384);
        let verdict =
            classify_index_compatibility(&loaded(384), std::slice::from_ref(&indexed), &query);
        match verdict {
            IndexCompatibility::ModelMismatch {
                indexed: got,
                differing_fields,
                ..
            } => {
                assert_eq!(got, indexed);
                assert_eq!(differing_fields, vec!["name"]);
            }
            other => panic!("expected ModelMismatch, got {other:?}"),
        }
    }

    /// AC3: every non-dimension identity field is compared, not just the name.
    #[test]
    fn every_identity_field_is_compared() {
        let base = model("m", 384);
        let cases: [(&str, EmbeddingModel); 4] = [
            (
                "provider",
                EmbeddingModel {
                    provider: "other".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "name",
                EmbeddingModel {
                    name: "other".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "version",
                EmbeddingModel {
                    version: "9.9.9".to_owned(),
                    ..base.clone()
                },
            ),
            (
                "content_hash",
                EmbeddingModel {
                    content_hash: "deadbeef".to_owned(),
                    ..base.clone()
                },
            ),
        ];
        for (field, query) in cases {
            let verdict =
                classify_index_compatibility(&loaded(384), std::slice::from_ref(&base), &query);
            match verdict {
                IndexCompatibility::ModelMismatch {
                    differing_fields, ..
                } => assert_eq!(differing_fields, vec![field], "field {field}"),
                other => panic!("field {field}: expected ModelMismatch, got {other:?}"),
            }
        }
    }

    /// Differing-field reporting is deterministic and in declared field order.
    #[test]
    fn differing_fields_are_reported_in_stable_declared_order() {
        let indexed = model("model-a", 384);
        let query = EmbeddingModel {
            provider: "p2".to_owned(),
            name: "model-b".to_owned(),
            version: "9.9.9".to_owned(),
            dim: 384,
            content_hash: "cafe".to_owned(),
        };
        assert_eq!(
            differing_identity_fields(&indexed, &query),
            vec!["provider", "name", "version", "content_hash"]
        );
    }

    /// AC4: a dimension mismatch is its OWN verdict, distinct from same-dim.
    #[test]
    fn physical_index_dimension_mismatch_is_its_own_verdict() {
        let indexed = model("m", 4);
        let query = model("m", 384);
        match classify_index_compatibility(&loaded(4), &[indexed], &query) {
            IndexCompatibility::DimensionMismatch {
                index_dimensions,
                query_dimensions,
                ..
            } => {
                assert_eq!(index_dimensions, 4);
                assert_eq!(query_dimensions, 384);
            }
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
    }

    /// A recorded identity whose `dim` disagrees with the physical vector index
    /// is still a dimension problem, never silently "compatible".
    #[test]
    fn recorded_identity_dim_disagreeing_with_physical_index_is_dimension_mismatch() {
        let indexed = model("m", 512);
        let query = model("m", 384);
        assert!(matches!(
            classify_index_compatibility(&loaded(384), &[indexed], &query),
            IndexCompatibility::DimensionMismatch { .. }
        ));
    }

    /// AC4: an index with no recorded identity is UNVERIFIABLE, never assumed
    /// compatible — and it is a distinct verdict from both mismatch cases.
    #[test]
    fn index_without_recorded_identity_is_unverifiable() {
        let query = model("m", 384);
        match classify_index_compatibility(&loaded(384), &[], &query) {
            IndexCompatibility::IdentityUnrecorded {
                index_dimensions, ..
            } => {
                assert_eq!(index_dimensions, 384);
            }
            other => panic!("expected IdentityUnrecorded, got {other:?}"),
        }
    }

    /// A store whose vector index was written by two different models cannot be
    /// ranked against either one; refuse rather than pick a winner.
    #[test]
    fn multiple_recorded_identities_are_ambiguous() {
        let query = model("model-a", 384);
        let other = model("model-b", 384);
        match classify_index_compatibility(&loaded(384), &[query.clone(), other], &query) {
            IndexCompatibility::IdentityAmbiguous { indexed, .. } => {
                assert_eq!(indexed.len(), 2);
            }
            other => panic!("expected IdentityAmbiguous, got {other:?}"),
        }
    }

    /// A store that was never `--embed`ed has no vector index at all. That is
    /// the pre-existing "no semantic index" outcome, NOT an identity failure —
    /// telling an operator their identity is unverifiable when they simply never
    /// embedded would be a false refusal.
    #[test]
    fn store_without_a_vector_index_is_index_absent() {
        let query = model("m", 384);
        assert_eq!(
            classify_index_compatibility(&VectorIndexState::Absent, &[], &query),
            IndexCompatibility::IndexAbsent
        );
        assert_eq!(
            classify_index_compatibility(
                &VectorIndexState::Absent,
                std::slice::from_ref(&query),
                &query
            ),
            IndexCompatibility::IndexAbsent
        );
    }

    // ── Issue #489: a skipped index is not an absent one ────────────────────

    /// An index whose files exist on disk but which the engine skipped at load
    /// is REFUSED, never reported as `IndexAbsent`. `AletheiaDB` 0.2.0 skips a
    /// corrupted index instead of aborting the load, so the two states reach
    /// this classifier looking alike through the engine handle — and reporting
    /// a data-loss condition as "you never ran `--embed`" is exactly the class
    /// of dishonesty the identity gate exists to prevent.
    #[test]
    fn unreadable_index_is_refused_not_reported_absent() {
        let query = model("m", 384);
        let verdict = classify_index_compatibility(&unreadable(), &[], &query);
        assert_eq!(verdict.code(), Some("semantic_index_unreadable"));
        assert_eq!(
            verdict.exit_code(),
            Some(SEMANTIC_INDEX_UNREADABLE_EXIT_CODE)
        );
        assert!(verdict.is_refusal());
        assert_ne!(verdict, IndexCompatibility::IndexAbsent);
    }

    /// The unreadable check precedes every identity check: a skipped index has
    /// no dimension to compare, so it must never fall through to a dimension,
    /// unrecorded-identity, or model verdict — nor to `IndexAbsent`.
    #[test]
    fn unreadable_index_takes_precedence_over_every_identity_verdict() {
        let query = model("m", 384);
        for indexed in [
            vec![],
            vec![model("m", 384)],
            vec![model("other", 384)],
            vec![model("m", 4)],
            vec![model("m", 384), model("n", 384)],
        ] {
            assert_eq!(
                classify_index_compatibility(&unreadable(), &indexed, &query).code(),
                Some("semantic_index_unreadable"),
                "an unreadable index outranks every identity verdict"
            );
        }
    }

    /// The refusal discloses the on-disk evidence and the model that built the
    /// now-unreadable index, and points at a FRESH data dir with the overwrite
    /// warning — never at re-embedding in place, which would destroy the files.
    #[test]
    fn unreadable_refusal_discloses_evidence_and_the_overwrite_hazard() {
        let query = model("m", 384);
        let indexed = model("builder", 384);
        let verdict = classify_index_compatibility(&unreadable(), &[indexed], &query);
        let envelope = verdict
            .to_error_envelope()
            .expect("an unreadable index refuses");
        let error = &envelope["error"];
        assert_eq!(error["code"], "semantic_index_unreadable");
        assert_eq!(
            error["index_artifacts"],
            serde_json::json!(["meta.idx", "mappings.idx"])
        );
        assert_eq!(error["indexed_models"][0]["name"], "builder");
        assert_eq!(error["query_model"]["name"], "m");
        assert_eq!(error["remedy"], SEMANTIC_INDEX_UNREADABLE_REMEDY);
        assert_eq!(verdict.remedy(), SEMANTIC_INDEX_UNREADABLE_REMEDY);
        // Allow-list only: nothing beyond the declared keys reaches the wire.
        let keys: Vec<&str> = error
            .as_object()
            .expect("error object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "code",
                "index_artifacts",
                "indexed_models",
                "message",
                "query_model",
                "remedy",
            ]
        );
        let message = verdict.message();
        assert!(
            message.contains("NOT absent"),
            "the message must refuse the absent framing: {message}"
        );
    }

    /// An index directory that survives with none of the expected files is
    /// still the skip condition (upstream's loader requires `meta.idx`), so it
    /// is reported honestly rather than degraded back to "absent".
    #[test]
    fn unreadable_with_no_surviving_artifacts_is_still_unreadable() {
        let query = model("m", 384);
        let state = VectorIndexState::Unreadable { artifacts: vec![] };
        let verdict = classify_index_compatibility(&state, &[], &query);
        assert_eq!(verdict.code(), Some("semantic_index_unreadable"));
        assert!(
            verdict
                .message()
                .contains("holds none of the expected files"),
            "the message must say what was and was not found: {}",
            verdict.message()
        );
    }

    /// The three-way state maps to exactly one status label and one dimension
    /// answer each, and only a loaded index is queryable.
    #[test]
    fn vector_index_state_reports_status_presence_and_dimensions() {
        assert_eq!(loaded(384).status(), "loaded");
        assert_eq!(loaded(384).dimensions(), Some(384));
        assert!(loaded(384).is_present());

        assert_eq!(unreadable().status(), "unreadable");
        assert_eq!(unreadable().dimensions(), None);
        assert!(
            unreadable().is_present(),
            "a skipped index is still an index the store HAS"
        );

        assert_eq!(VectorIndexState::Absent.status(), "absent");
        assert_eq!(VectorIndexState::Absent.dimensions(), None);
        assert!(!VectorIndexState::Absent.is_present());
    }

    /// Every refusal verdict carries a distinct stable machine code and a
    /// distinct nonzero exit code (AC3/AC4).
    #[test]
    fn refusal_codes_and_exit_codes_are_distinct_and_nonzero() {
        let q = model("m", 384);
        let verdicts = [
            classify_index_compatibility(&loaded(384), &[], &q),
            classify_index_compatibility(&loaded(384), &[model("m", 384), model("n", 384)], &q),
            classify_index_compatibility(&loaded(4), &[model("m", 4)], &q),
            classify_index_compatibility(&loaded(384), &[model("n", 384)], &q),
            classify_index_compatibility(&unreadable(), &[model("m", 384)], &q),
        ];
        let mut codes: Vec<&str> = verdicts
            .iter()
            .filter_map(IndexCompatibility::code)
            .collect();
        let mut exits: Vec<i32> = verdicts
            .iter()
            .filter_map(IndexCompatibility::exit_code)
            .collect();
        assert_eq!(codes.len(), 5, "all five verdicts must be refusals");
        assert!(exits.iter().all(|c| *c != 0), "exit codes must be nonzero");
        codes.sort_unstable();
        codes.dedup();
        exits.sort_unstable();
        exits.dedup();
        assert_eq!(codes.len(), 5, "codes must be pairwise distinct");
        assert_eq!(exits.len(), 5, "exit codes must be pairwise distinct");
        // `Compatible` and `IndexAbsent` are not identity refusals.
        assert!(IndexCompatibility::Compatible.code().is_none());
        assert!(IndexCompatibility::IndexAbsent.code().is_none());
    }

    /// AC7: the refusal envelope carries identity handles only — never model
    /// bytes, payloads, or the operator's query text.
    #[test]
    fn refusal_envelope_is_allow_list_only() {
        let indexed = model("model-a", 384);
        let query = model("model-b", 384);
        let verdict = classify_index_compatibility(&loaded(384), &[indexed], &query);
        let envelope = verdict
            .to_error_envelope()
            .expect("a refusal must produce an envelope");
        let error = &envelope["error"];
        let keys: Vec<&str> = error
            .as_object()
            .expect("error is an object")
            .keys()
            .map(String::as_str)
            .collect();
        for key in &keys {
            assert!(
                [
                    "code",
                    "message",
                    "remedy",
                    "indexed_model",
                    "indexed_models",
                    "query_model",
                    "index_dimensions",
                    "differing_fields",
                ]
                .contains(key),
                "unexpected envelope key {key}"
            );
        }
        for model_key in ["indexed_model", "query_model"] {
            if let Some(m) = error.get(model_key) {
                let model_keys: Vec<&str> = m
                    .as_object()
                    .expect("model is an object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                // `serde_json::Map` is a `BTreeMap`, so the rendered key order
                // is sorted and therefore already byte-stable.
                assert_eq!(
                    model_keys,
                    vec!["content_hash", "dim", "name", "provider", "version"],
                    "{model_key} exposes exactly the identity fields and nothing else"
                );
            }
        }
        // The remedy points at re-ingest, never at editing the store (AC6).
        let remedy = error["remedy"].as_str().expect("remedy is a string");
        assert!(
            remedy.contains("--embed"),
            "remedy names re-ingest: {remedy}"
        );
    }

    /// AC8: an identical mismatched query yields byte-identical diagnostics.
    #[test]
    fn refusal_envelope_is_byte_identical_across_runs() {
        let indexed = model("model-a", 384);
        let query = model("model-b", 384);
        let render = || {
            let verdict =
                classify_index_compatibility(&loaded(384), std::slice::from_ref(&indexed), &query);
            serde_json::to_string(&verdict.to_error_envelope().expect("refusal envelope"))
                .expect("envelope serializes")
        };
        let first = render();
        for _ in 0..5 {
            assert_eq!(render(), first);
        }
    }

    /// AC1: the persisted identity record is an `EmbeddingModel` node in the
    /// semantic domain carrying the full identity, with a stable ID derived from
    /// the identity tuple (so two different models leave two records).
    #[test]
    fn identity_record_is_a_semantic_embedding_model_node() {
        let model = model("model-a", 384);
        let record = embedding_index_identity_record(&model);
        let GraphRecord::Node {
            id,
            kind,
            domain,
            schema_version,
            embedding_model,
            ..
        } = &record
        else {
            panic!("identity record must be a node");
        };
        assert_eq!(*kind, NodeKind::EmbeddingModel);
        assert_eq!(domain.as_deref(), Some("semantic"));
        assert_eq!(*schema_version, SEMANTIC_SCHEMA_VERSION);
        assert!(id.starts_with("semantic:v1:"), "unexpected id {id}");
        assert_eq!(embedding_model.as_deref(), Some(&model));
    }

    /// The identity record ID is FIXED, not identity-derived: a store has one
    /// vector index, so it has one identity, and re-embedding supersedes it.
    ///
    /// This is what keeps every bad state recoverable. An identity-derived ID
    /// would let two identity records coexist, and since `eg forget` refuses
    /// every semantic-domain record and `eg forget-repo` never evicts a
    /// repo-agnostic one, that state would be UNRECOVERABLE — reachable through
    /// the documented `eg refresh --embed` workflow after a version bump.
    #[test]
    fn identity_record_id_is_fixed_so_re_embedding_supersedes() {
        let a = model("model-a", 384);
        let b = model("model-b", 384);
        assert_eq!(
            embedding_index_identity_record(&a).id(),
            embedding_index_identity_record(&a).id()
        );
        assert_eq!(
            embedding_index_identity_record(&a).id(),
            embedding_index_identity_record(&b).id(),
            "a store has ONE vector index, so re-embedding must supersede the identity \
             at a fixed ID rather than accumulate a second, unremovable record"
        );
        assert_eq!(
            embedding_index_identity_record(&a).id(),
            embedding_index_identity_id()
        );
    }

    /// Two physical versions of the SAME identity ID collapse to the latest
    /// write — not to a spurious "two identities" ambiguity.
    ///
    /// Over an append-only `--graph` JSONL, superseding the fixed-ID identity
    /// record leaves both versions in the slice. Without last-write-wins, the
    /// ordinary act of re-embedding would make every later query refuse with
    /// `embedding_identity_ambiguous`.
    #[test]
    fn same_id_versions_collapse_to_the_latest_write() {
        let old = model("model-a", 384);
        let new = model("model-b", 384);
        let records = vec![
            embedding_index_identity_record(&old),
            embedding_index_identity_record(&new),
        ];
        assert_eq!(indexed_identities(&records), vec![new]);
    }

    /// Distinct identity IDs are deduplicated by identity tuple, deterministically
    /// ordered, and liveness-gated.
    #[test]
    fn indexed_identities_are_deduped_sorted_and_liveness_gated() {
        let a = model("model-a", 384);
        let b = model("model-b", 384);
        // Distinct IDs, as only a foreign/hand-authored record could produce —
        // the CLI itself always writes the one fixed ID.
        let with_id = |new_id: &str, m: &EmbeddingModel| {
            let mut record = embedding_index_identity_record(m);
            if let GraphRecord::Node { id, .. } = &mut record {
                *id = new_id.to_owned();
            }
            record
        };
        let records = vec![
            with_id("semantic:v1:b", &b),
            with_id("semantic:v1:a", &a),
            with_id("semantic:v1:b2", &b),
        ];
        assert_eq!(indexed_identities(&records), vec![a.clone(), b]);

        let tombstone = |deleted: &str| GraphRecord::Tombstone {
            id: format!("semantic:v1:tomb:{deleted}"),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            deleted_id: deleted.to_owned(),
            summary: "retracted identity fixture".to_owned(),
            producer: None,
        };
        let mut with_tomb = records;
        with_tomb.push(tombstone("semantic:v1:b"));
        with_tomb.push(tombstone("semantic:v1:b2"));
        assert_eq!(indexed_identities(&with_tomb), vec![a]);
    }

    /// A crafted identity cannot forge extra output lines, drive a terminal, or
    /// produce an unbounded diagnostic: values are sanitized and length-capped
    /// before they reach any envelope or message (AC7).
    #[test]
    fn identity_values_are_sanitized_and_bounded_in_diagnostics() {
        let hostile = EmbeddingModel {
            provider: "p\n\rHACKED: fake line".to_owned(),
            name: "\u{1b}[2Kmalicious".to_owned(),
            version: "v".repeat(10_000),
            dim: 384,
            content_hash: "h\tx".to_owned(),
        };
        let query = model("mine", 384);
        let verdict =
            classify_index_compatibility(&loaded(384), std::slice::from_ref(&hostile), &query);
        let envelope = verdict.to_error_envelope().expect("refusal envelope");
        let rendered = serde_json::to_string(&envelope).expect("serializes");
        assert!(
            !rendered.contains('\n') && !rendered.contains('\r') && !rendered.contains('\u{1b}'),
            "control characters must never survive into a diagnostic: {rendered}"
        );
        let version = envelope["error"]["indexed_model"]["version"]
            .as_str()
            .expect("version is a string");
        assert!(
            version.chars().count() <= IDENTITY_FIELD_MAX_CHARS + 1,
            "identity fields must be length-capped, got {} chars",
            version.chars().count()
        );
        assert!(version.ends_with('…'), "truncation must be visible");
        assert!(
            !verdict.message().contains('\n'),
            "the stderr summary must stay a single line"
        );
    }

    /// A dimension mismatch over a store with several recorded identities
    /// discloses EVERY candidate — never silently picks the first, matching the
    /// no-silent-winner doctrine the CALLS ladder and `who-constructs` follow.
    #[test]
    fn dimension_mismatch_discloses_every_recorded_identity() {
        let query = model("mine", 384);
        let verdict =
            classify_index_compatibility(&loaded(4), &[model("a", 4), model("b", 4)], &query);
        let envelope = verdict.to_error_envelope().expect("refusal envelope");
        let listed = envelope["error"]["indexed_models"]
            .as_array()
            .expect("indexed_models is an array");
        assert_eq!(
            listed.len(),
            2,
            "both recorded identities must be disclosed"
        );
        assert_eq!(envelope["error"]["query_dimensions"], 384);
        assert_eq!(envelope["error"]["index_dimensions"], 4);
    }

    /// A recorded identity whose `dim` contradicts the physical index is worded
    /// as the contradiction it is, not as a self-contradictory "384 vs 384"
    /// dimension mismatch.
    #[test]
    fn recorded_identity_contradicting_the_index_is_worded_distinctly() {
        let query = model("mine", 384);
        let verdict = classify_index_compatibility(&loaded(384), &[model("mine", 512)], &query);
        let message = verdict.message();
        assert!(
            message.contains("contradicts the physical semantic index"),
            "unexpected message: {message}"
        );
    }

    /// The query-side identity is derived from the same compile-time constants
    /// the embedder is built from, so the compatible path is self-consistent.
    #[test]
    fn default_identity_uses_the_shared_model_constants() {
        let identity = default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
        assert_eq!(identity.provider, DEFAULT_EMBEDDING_MODEL_PROVIDER);
        assert_eq!(identity.name, DEFAULT_EMBEDDING_MODEL_NAME);
        assert_eq!(identity.content_hash, DEFAULT_EMBEDDING_MODEL_CONTENT_HASH);
        assert_eq!(identity.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(identity.dim as usize, DEFAULT_EMBEDDING_MODEL_DIMENSIONS);
    }
}
