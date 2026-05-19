use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Current schema version for code-graph records.
pub const SCHEMA_VERSION: u32 = 1;

/// Schema version for agent-memory records (`Agent`, `AgentSession`, `Observation`, etc.).
/// Documented in `docs/schema/agent-memory.md`.
pub const AGENT_MEMORY_SCHEMA_VERSION: u32 = 1;

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

/// Agent-memory provenance fields for agent-authored nodes.
///
/// Documented in `docs/schema/agent-memory.md §3`.
/// These fields appear flat in `GraphRecord::Node` JSON.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct NodeProvenance {
    /// Observation body text (Observation nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// ID of the record that supersedes this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Stable agent identity (agent-authored nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Agent kind enum value (agent-authored nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    /// Agent session ID (agent-authored nodes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Wall-clock time the agent observed the fact (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Transaction time when the daemon committed the record (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<String>,
    /// Extraction confidence `[0.0, 1.0]` (Observation, Decision, Lesson).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Artifact path or hash the record was extracted from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_handle: Option<String>,
    /// Redaction policy version when any field passed through redaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_policy_version: Option<String>,
}

/// A typed citation from an agent-memory node to another graph record.
///
/// Evidence links are stored both on the source node (for fast read) and as
/// graph edges (for traversal). Both representations MUST agree at write time;
/// the daemon write applier is the enforcement point.
///
/// The target may be specified either by its stable `target_record_id` or by
/// the `(target_repo_relative_path, target_span, target_git_commit)` triple
/// when the writer cannot compute the stable hash. The daemon write applier
/// resolves the triple at write time.
///
/// Documented in docs/schema/agent-memory.md.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceLink {
    /// Stable record ID of the cited graph node.
    /// Either this or the triple fields below must be present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_record_id: Option<String>,
    /// Domain of the target record (e.g. `"codegraph"`, `"agent_memory"`).
    pub target_domain: String,
    /// Cross-domain edge label (e.g. `"OBSERVES"`, `"MENTIONS_SYMBOL"`).
    pub relation: String,
    /// Extraction confidence formatted in `[0.0, 1.0]`.
    pub confidence: String,
    /// Git commit SHA anchoring a time-specific citation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_commit: Option<String>,
    // ── Triple-based target resolution ────────────────────────────────────────
    /// Repository-relative path of the target node (triple fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_repo_relative_path: Option<String>,
    /// Source span of the target node (triple fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_span: Option<SourceSpan>,
    /// Git commit SHA anchoring the target node lookup (triple fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_git_commit: Option<String>,
}

/// One JSONL graph record.
// Node carries 10 optional provenance strings for agent-memory nodes.
// These are None for all code-graph nodes, so the memory cost is only
// paid by agent-memory records that actually populate them.
#[allow(clippy::large_enum_variant)]
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
        /// Evidence citations for agent-memory nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence_links: Option<Vec<EvidenceLink>>,
        // ── Agent-memory provenance fields (absent for code-graph nodes) ─────
        /// Observation body text (Observation nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// ID of the record that supersedes this one.
        #[serde(skip_serializing_if = "Option::is_none")]
        superseded_by: Option<String>,
        /// Stable agent identity (agent-authored nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// Agent kind enum value (agent-authored nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_kind: Option<String>,
        /// Agent session ID (agent-authored nodes).
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Wall-clock time the agent observed the fact (RFC 3339).
        #[serde(skip_serializing_if = "Option::is_none")]
        observed_at: Option<String>,
        /// Transaction time when the daemon committed the record (RFC 3339).
        #[serde(skip_serializing_if = "Option::is_none")]
        ingested_at: Option<String>,
        /// Extraction confidence `[0.0, 1.0]` (Observation, Decision, Lesson).
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<String>,
        /// Artifact path or hash the record was extracted from.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_handle: Option<String>,
        /// Redaction policy version when any field passed through redaction.
        #[serde(skip_serializing_if = "Option::is_none")]
        redaction_policy_version: Option<String>,
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
            evidence_links: None,
            text: None,
            superseded_by: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            confidence: None,
            source_handle: None,
            redaction_policy_version: None,
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
            evidence_links: None,
            text: None,
            superseded_by: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            confidence: None,
            source_handle: None,
            redaction_policy_version: None,
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
            evidence_links: None,
            text: None,
            superseded_by: None,
            agent_id: None,
            agent_kind: None,
            session_id: None,
            observed_at: None,
            ingested_at: None,
            confidence: None,
            source_handle: None,
            redaction_policy_version: None,
            summary,
        }
    }

    /// Creates an agent-memory graph edge with the `agent_memory:v1:` ID prefix.
    #[must_use]
    pub fn agent_memory_edge(
        label: EdgeLabel,
        source: String,
        target: String,
        confidence: Option<String>,
        summary: String,
    ) -> Self {
        let id = agent_memory_stable_id(&["edge", label.as_str(), &source, &target]);
        Self::Edge {
            id,
            schema_version: AGENT_MEMORY_SCHEMA_VERSION,
            label,
            source,
            target,
            confidence,
            temporal: None,
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
    // ── Cross-domain edge registry (docs/schema/agent-memory.md) ─────────────
    /// Agent-memory node observes a code-graph entity.
    Observes,
    /// Agent-memory node mentions a specific symbol.
    MentionsSymbol,
    /// Agent-memory node cites a file that was touched.
    TouchedFile,
    /// Agent-memory node produced a patch artifact.
    ProducedPatch,
    /// Agent-memory node is validated by an evidence record.
    ValidatedBy,
    /// Agent-memory node describes a failure on a code entity.
    FailedOn,
    /// Agent-memory node explains a code change.
    ExplainsChange,
    /// Agent-memory node references a task record.
    ReferencesTask,
    /// Agent-memory node contradicts another record.
    Contradicts,
    /// Agent-memory node supersedes another record.
    Supersedes,
    /// Generic weak relationship between any two records.
    RelatesTo,
}

impl EdgeLabel {
    /// Parses an edge label from its wire string.  Returns `None` for unknown labels.
    #[must_use]
    pub fn from_relation(s: &str) -> Option<Self> {
        match s {
            "CONTAINS" => Some(Self::Contains),
            "DEFINES" => Some(Self::Defines),
            "IMPORTS" => Some(Self::Imports),
            "REFERENCES" => Some(Self::References),
            "CALLS" => Some(Self::Calls),
            "IMPLEMENTS" => Some(Self::Implements),
            "MENTIONS" => Some(Self::Mentions),
            "CHANGED_IN" => Some(Self::ChangedIn),
            "PARENT_OF" => Some(Self::ParentOf),
            "DRIFTS_FROM" => Some(Self::DriftsFrom),
            "SESSION_OF" => Some(Self::SessionOf),
            "AUTHORED_BY" => Some(Self::AuthoredBy),
            "HAS_EVIDENCE" => Some(Self::HasEvidence),
            "OBSERVES" => Some(Self::Observes),
            "MENTIONS_SYMBOL" => Some(Self::MentionsSymbol),
            "TOUCHED_FILE" => Some(Self::TouchedFile),
            "PRODUCED_PATCH" => Some(Self::ProducedPatch),
            "VALIDATED_BY" => Some(Self::ValidatedBy),
            "FAILED_ON" => Some(Self::FailedOn),
            "EXPLAINS_CHANGE" => Some(Self::ExplainsChange),
            "REFERENCES_TASK" => Some(Self::ReferencesTask),
            "CONTRADICTS" => Some(Self::Contradicts),
            "SUPERSEDES" => Some(Self::Supersedes),
            "RELATES_TO" => Some(Self::RelatesTo),
            _ => None,
        }
    }

    /// Returns `true` when this label is permitted in an evidence link citation.
    ///
    /// Code-graph-internal labels (`CONTAINS`, `DEFINES`, `CALLS`, etc.) are
    /// reserved for the extractor and must not appear in evidence links.
    #[must_use]
    pub const fn is_evidence_link_label(self) -> bool {
        matches!(
            self,
            Self::HasEvidence
                | Self::Observes
                | Self::MentionsSymbol
                | Self::TouchedFile
                | Self::ProducedPatch
                | Self::ValidatedBy
                | Self::FailedOn
                | Self::ExplainsChange
                | Self::ReferencesTask
                | Self::Contradicts
                | Self::Supersedes
                | Self::RelatesTo
        )
    }

    /// Returns `true` when this label belongs to the codegraph topology set and
    /// must not appear on agent-memory (`agent_memory:v1:`) edge records.
    ///
    /// Agent-memory structural labels (`SESSION_OF`, `AUTHORED_BY`) and the full
    /// evidence-link registry are permitted; only extractor-specific topology labels
    /// such as `CONTAINS`, `CALLS`, `DEFINES`, etc. are rejected.
    #[must_use]
    pub const fn is_codegraph_topology_label(self) -> bool {
        matches!(
            self,
            Self::Contains
                | Self::Defines
                | Self::Imports
                | Self::References
                | Self::Calls
                | Self::Implements
                | Self::Mentions
                | Self::ChangedIn
                | Self::ParentOf
                | Self::DriftsFrom
        )
    }

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
            Self::Observes => "OBSERVES",
            Self::MentionsSymbol => "MENTIONS_SYMBOL",
            Self::TouchedFile => "TOUCHED_FILE",
            Self::ProducedPatch => "PRODUCED_PATCH",
            Self::ValidatedBy => "VALIDATED_BY",
            Self::FailedOn => "FAILED_ON",
            Self::ExplainsChange => "EXPLAINS_CHANGE",
            Self::ReferencesTask => "REFERENCES_TASK",
            Self::Contradicts => "CONTRADICTS",
            Self::Supersedes => "SUPERSEDES",
            Self::RelatesTo => "RELATES_TO",
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

/// Builds a stable code-graph ID from semantic, repo-relative inputs.
#[must_use]
pub fn stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("codegraph:v{SCHEMA_VERSION}:{}", hasher.finalize().to_hex())
}

/// Builds a stable agent-memory record ID.
///
/// Uses the `agent_memory:v1:` prefix so agent-memory IDs cannot collide with
/// code-graph `codegraph:v1:` IDs even when the content hashes are identical.
/// Documented in docs/schema/agent-memory.md.
#[must_use]
pub fn agent_memory_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "agent_memory:v{AGENT_MEMORY_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}
