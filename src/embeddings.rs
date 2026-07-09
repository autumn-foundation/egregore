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
        | NodeKind::Retraction => return None,
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
