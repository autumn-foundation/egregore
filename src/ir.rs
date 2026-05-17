use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Current schema version for emitted graph records.
pub const SCHEMA_VERSION: u32 = 1;

/// Complete in-memory graph emitted by a scan.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    records: Vec<GraphRecord>,
}

impl Graph {
    /// Creates an empty graph.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Adds a record to the graph.
    pub fn push(&mut self, record: GraphRecord) {
        self.records.push(record);
    }

    /// Returns all graph records in insertion order.
    #[must_use]
    pub fn records(&self) -> &[GraphRecord] {
        &self.records
    }

    /// Serializes the graph to canonically ordered JSON Lines.
    ///
    /// Ordering is based on the final serialized lines. This keeps output
    /// byte-stable even if internal scan order changes.
    ///
    /// # Errors
    ///
    /// Returns an error if any record cannot be serialized.
    pub fn to_jsonl(&self) -> Result<String> {
        let mut lines = self
            .records
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        lines.sort_unstable();
        Ok(format!("{}\n", lines.join("\n")))
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

/// One JSONL graph record.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum GraphRecord {
    /// A graph node.
    Node {
        /// Stable record ID.
        id: String,
        /// Node kind.
        kind: NodeKind,
        /// Schema version that produced the record.
        schema_version: u32,
        /// Repository-relative path for file-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        repo_relative_path: Option<String>,
        /// Source span for syntax-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<SourceSpan>,
        /// Human-readable node name.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Language for syntax-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// Language-specific symbol category.
        #[serde(skip_serializing_if = "Option::is_none")]
        symbol_kind: Option<String>,
        /// Git and bitemporal provenance for history-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        temporal: Option<TemporalMetadata>,
        /// Semantic drift details for drift marker nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        semantic_drift: Option<Box<SemanticDriftMetadata>>,
        /// Agent-facing summary.
        summary: String,
    },
    /// A graph edge.
    Edge {
        /// Stable record ID.
        id: String,
        /// Schema version that produced the record.
        schema_version: u32,
        /// Edge label.
        label: EdgeLabel,
        /// Source node ID.
        source: String,
        /// Target node ID.
        target: String,
        /// Optional extraction confidence.
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<String>,
        /// Git and bitemporal provenance for history-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        temporal: Option<TemporalMetadata>,
        /// Agent-facing summary.
        summary: String,
    },
    /// A deleted graph entity marker emitted by incremental scans.
    Tombstone {
        /// Stable record ID for the tombstone.
        id: String,
        /// Schema version that produced the record.
        schema_version: u32,
        /// ID of the graph entity that no longer exists.
        deleted_id: String,
        /// Agent-facing summary.
        summary: String,
    },
}

impl GraphRecord {
    /// Returns the stable record ID.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Node { id, .. } | Self::Edge { id, .. } | Self::Tombstone { id, .. } => id,
        }
    }

    /// Returns the node kind name when this record is a node.
    #[must_use]
    pub const fn node_kind_name(&self) -> Option<&'static str> {
        match self {
            Self::Node { kind, .. } => Some(kind.as_str()),
            Self::Edge { .. } | Self::Tombstone { .. } => None,
        }
    }

    /// Creates a graph node record.
    #[must_use]
    pub const fn node(
        id: String,
        kind: NodeKind,
        repo_relative_path: Option<String>,
        span: Option<SourceSpan>,
        name: Option<String>,
        summary: String,
    ) -> Self {
        Self::Node {
            id,
            kind,
            schema_version: SCHEMA_VERSION,
            repo_relative_path,
            span,
            name,
            language: None,
            symbol_kind: None,
            temporal: None,
            semantic_drift: None,
            summary,
        }
    }

    /// Creates a syntax-backed node record with language metadata.
    #[must_use]
    pub fn syntax_node(
        id: String,
        kind: NodeKind,
        repo_relative_path: String,
        span: SourceSpan,
        name: String,
        language: &str,
        summary: String,
    ) -> Self {
        Self::Node {
            id,
            kind,
            schema_version: SCHEMA_VERSION,
            repo_relative_path: Some(repo_relative_path),
            span: Some(span),
            name: Some(name),
            language: Some(language.to_owned()),
            symbol_kind: None,
            temporal: None,
            semantic_drift: None,
            summary,
        }
    }

    /// Creates a syntax-backed symbol record.
    #[must_use]
    pub fn symbol(
        id: String,
        symbol_kind: &str,
        repo_relative_path: String,
        span: SourceSpan,
        name: String,
        summary: String,
    ) -> Self {
        Self::Node {
            id,
            kind: NodeKind::Symbol,
            schema_version: SCHEMA_VERSION,
            repo_relative_path: Some(repo_relative_path),
            span: Some(span),
            name: Some(name),
            language: Some("rust".to_owned()),
            symbol_kind: Some(symbol_kind.to_owned()),
            temporal: None,
            semantic_drift: None,
            summary,
        }
    }

    /// Creates a graph edge record.
    #[must_use]
    pub fn edge(
        label: EdgeLabel,
        source: String,
        target: String,
        confidence: Option<String>,
        summary: String,
    ) -> Self {
        let label_text = label.as_str();
        let id = stable_id(&["edge", label_text, &source, &target]);
        Self::Edge {
            id,
            schema_version: SCHEMA_VERSION,
            label,
            source,
            target,
            confidence,
            temporal: None,
            summary,
        }
    }

    /// Attaches Git and bitemporal provenance to a node or edge record.
    #[must_use]
    pub fn with_temporal(mut self, temporal_metadata: TemporalMetadata) -> Self {
        match &mut self {
            Self::Node { temporal, .. } | Self::Edge { temporal, .. } => {
                *temporal = Some(temporal_metadata);
            }
            Self::Tombstone { .. } => {}
        }
        self
    }

    /// Attaches semantic drift metadata to a node record.
    #[must_use]
    pub fn with_semantic_drift(mut self, drift: SemanticDriftMetadata) -> Self {
        if let Self::Node { semantic_drift, .. } = &mut self {
            *semantic_drift = Some(Box::new(drift));
        }
        self
    }
}

/// Git and bitemporal provenance attached to history-backed records.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct TemporalMetadata {
    /// Git commit SHA that supplied the valid-time source tree.
    pub git_commit: String,
    /// Parent commit SHAs observed for this commit.
    pub git_parent_commits: Vec<String>,
    /// Valid time for this record, derived from Git committer time.
    pub valid_time: String,
    /// Git author timestamp when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_time: Option<String>,
    /// Observation timestamp for the replay artifact.
    pub observed_at: String,
}

/// Structured metadata for a semantic drift measurement.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticDriftMetadata {
    /// Embedding model or semantic scorer identifier.
    pub model_id: String,
    /// Stable graph record ID for the drift target.
    pub target_record_id: String,
    /// Commit SHA for the earlier embedding.
    pub before_git_commit: String,
    /// Commit SHA for the later embedding.
    pub after_git_commit: String,
    /// Valid time for the earlier embedding.
    pub before_valid_time: String,
    /// Valid time for the later embedding.
    pub after_valid_time: String,
    /// Cosine distance formatted with stable precision.
    pub score: String,
}

/// Initial graph node kinds.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum NodeKind {
    /// Indexed repository root.
    Repository,
    /// Source file.
    File,
    /// Language module namespace.
    Module,
    /// Language symbol.
    Symbol,
    /// Import or use declaration.
    Import,
    /// Extractor warning or unsupported construct.
    Diagnostic,
    /// Git commit observed during history replay.
    Commit,
    /// File-level change observed in a commit.
    Change,
    /// Semantic movement for a file or symbol over time.
    SemanticDrift,
    /// Agent process or human actor writing observations.
    Agent,
    /// One agent run or conversation session.
    AgentSession,
    /// Agent-authored memory or discovery.
    Observation,
    /// Work item tracked by an agent.
    Task,
    /// File, patch, report, or generated output linked to work.
    Artifact,
    /// Evidence for a claim, test, or check.
    Verification,
    /// Command output or terminal evidence.
    CommandEvidence,
}

impl NodeKind {
    /// Returns the serialized node kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "Repository",
            Self::File => "File",
            Self::Module => "Module",
            Self::Symbol => "Symbol",
            Self::Import => "Import",
            Self::Diagnostic => "Diagnostic",
            Self::Commit => "Commit",
            Self::Change => "Change",
            Self::SemanticDrift => "SemanticDrift",
            Self::Agent => "Agent",
            Self::AgentSession => "AgentSession",
            Self::Observation => "Observation",
            Self::Task => "Task",
            Self::Artifact => "Artifact",
            Self::Verification => "Verification",
            Self::CommandEvidence => "CommandEvidence",
        }
    }
}

/// Initial graph edge labels.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeLabel {
    /// Hierarchical ownership.
    Contains,
    /// Definition ownership.
    Defines,
    /// Import declaration ownership.
    Imports,
    /// Best-effort syntactic reference.
    References,
    /// Best-effort call relationship.
    Calls,
    /// Syntactically resolvable implementation relationship.
    Implements,
    /// Weaker unresolved mention relationship.
    Mentions,
    /// Entity changed in a Git commit.
    ChangedIn,
    /// Git commit ancestry.
    ParentOf,
    /// Semantic drift measurement target.
    DriftsFrom,
    /// Agent session belongs to an agent.
    SessionOf,
    /// Entity was authored by an agent session.
    AuthoredBy,
    /// Entity has supporting evidence.
    HasEvidence,
}

impl EdgeLabel {
    /// Returns the serialized edge label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "CONTAINS",
            Self::Defines => "DEFINES",
            Self::Imports => "IMPORTS",
            Self::References => "REFERENCES",
            Self::Calls => "CALLS",
            Self::Implements => "IMPLEMENTS",
            Self::Mentions => "MENTIONS",
            Self::ChangedIn => "CHANGED_IN",
            Self::ParentOf => "PARENT_OF",
            Self::DriftsFrom => "DRIFTS_FROM",
            Self::SessionOf => "SESSION_OF",
            Self::AuthoredBy => "AUTHORED_BY",
            Self::HasEvidence => "HAS_EVIDENCE",
        }
    }
}

/// Source byte and line span for syntax-backed records.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct SourceSpan {
    /// Start byte, inclusive.
    pub start_byte: usize,
    /// End byte, exclusive.
    pub end_byte: usize,
    /// One-based start line.
    pub start_line: usize,
    /// One-based end line.
    pub end_line: usize,
}

/// Builds a stable ID from semantic, repo-relative inputs.
#[must_use]
pub fn stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("codegraph:v{SCHEMA_VERSION}:{}", hasher.finalize().to_hex())
}
