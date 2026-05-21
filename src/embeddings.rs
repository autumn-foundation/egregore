//! Semantic enrichment helpers for embedding code graph records.

use std::collections::BTreeMap;

use crate::ir::{
    EdgeLabel, GraphRecord, NodeKind, SemanticDriftMetadata, TemporalMetadata, stable_id,
};

/// Re-export of `AletheiaDB`'s embedding boundary when semantic embedding
/// execution is enabled.
#[cfg(feature = "embeddings")]
pub use aletheiadb::embeddings as aletheia_embeddings;

/// Re-export of `embed_anything` through `AletheiaDB`'s public embedding module.
#[cfg(feature = "embeddings")]
pub use aletheiadb::embeddings::embed_anything;

/// Default embedding model used by the CLI.
pub const DEFAULT_EMBEDDING_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Model architecture passed through to `AletheiaDB`'s embedding boundary.
pub const DEFAULT_EMBEDDING_MODEL_ARCHITECTURE: &str = "bert";

/// Dense vector dimensions for [`DEFAULT_EMBEDDING_MODEL_ID`].
pub const DEFAULT_EMBEDDING_MODEL_DIMENSIONS: usize = 384;

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

fn candidate_from_record(record: &GraphRecord) -> Option<EmbeddingCandidate> {
    let GraphRecord::Node {
        id,
        kind,
        repo_relative_path,
        name,
        temporal,
        summary,
        ..
    } = record
    else {
        return None;
    };

    let target = match kind {
        NodeKind::File => "file",
        NodeKind::Symbol => "symbol",
        NodeKind::Repository
        | NodeKind::Module
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
        | NodeKind::ProofResult => return None,
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
    model_id: &str,
    threshold: f32,
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
            if score < threshold {
                continue;
            }
            records.extend(drift_pair_records(before, after, model_id, score));
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
    model_id: &str,
    score: f32,
) -> Vec<GraphRecord> {
    let Some(before_temporal) = before.candidate.temporal.as_ref() else {
        return Vec::new();
    };
    let Some(after_temporal) = after.candidate.temporal.as_ref() else {
        return Vec::new();
    };

    let score = format!("{score:.6}");
    let drift_id = stable_id(&[
        "node",
        "semantic_drift",
        model_id,
        &after.candidate.record_id,
        &before_temporal.git_commit,
        &after_temporal.git_commit,
    ]);
    let drift = SemanticDriftMetadata {
        model_id: model_id.to_owned(),
        target_record_id: after.candidate.record_id.clone(),
        before_git_commit: before_temporal.git_commit.clone(),
        after_git_commit: after_temporal.git_commit.clone(),
        before_valid_time: before_temporal.valid_time.clone(),
        after_valid_time: after_temporal.valid_time.clone(),
        score: score.clone(),
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
    .with_semantic_drift(drift);
    let edge = GraphRecord::edge(
        EdgeLabel::DriftsFrom,
        drift_id,
        after.candidate.record_id.clone(),
        Some("1.0".to_owned()),
        "Semantic drift measurement target".to_owned(),
    )
    .with_temporal(after_temporal.clone());

    vec![node, edge]
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
