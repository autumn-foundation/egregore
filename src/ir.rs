use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Current schema version for code-graph records.
pub const SCHEMA_VERSION: u32 = 4;

/// Schema version for agent-memory records (`Agent`, `AgentSession`, `Observation`, etc.).
/// Documented in `docs/schema/agent-memory.md`.
pub const AGENT_MEMORY_SCHEMA_VERSION: u32 = 1;

/// Schema version for verification-domain records (`CommandRun`, `TestRun`, `Verification`, etc.).
/// Documented in `docs/schema/verification.md`.
pub const VERIFICATION_SCHEMA_VERSION: u32 = 1;

/// Schema version for artifact-domain records (`PatchArtifact`, etc.).
/// Documented in `docs/schema/agent-actions.md`.
pub const ARTIFACT_SCHEMA_VERSION: u32 = 1;

/// Schema version for project-domain records (`Task`, `AcceptanceCriterion`, etc.).
/// Documented in `docs/schema/project-graph.md`.
pub const PROJECT_SCHEMA_VERSION: u32 = 1;

/// Schema version for semantic-domain records (`SemanticDrift`, reserved
/// `EmbeddingModel`, reserved `EmbeddingVector`).
/// Documented in `docs/schema/semantic-drift.md`.
pub const SEMANTIC_SCHEMA_VERSION: u32 = 1;

/// Minimum replay tolerance for semantic drift scores.
/// Documented in `docs/schema/semantic-drift.md`.
pub const SEMANTIC_DRIFT_REPLAY_SCORE_TOLERANCE: f64 = 1e-5;

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

/// Output or error handle for verification-domain `CommandRun` records.
///
/// Inline content is bounded by a 16 KiB ceiling.  When the output exceeds
/// that ceiling the `inline` field MUST be `None` and the full content is
/// referenced by `hash` only.  Documented in `docs/schema/verification.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutputHandle {
    /// Inline content (None when bytes exceeds the 16 KiB ceiling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    /// BLAKE3 hash of the full output content.
    pub hash: String,
    /// Total byte length of the output.
    pub bytes: u64,
}

/// Handle for raw patch bytes in an artifact-domain `PatchArtifact`.
///
/// The patch may be inlined only under the 16 KiB ceiling; otherwise the path
/// points at protected artifact storage. Documented in
/// `docs/schema/agent-actions.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PatchHandle {
    /// Path to the stored patch bytes.
    pub path: String,
    /// Redacted inline patch bytes when the payload is small enough.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
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

/// How a `Repository` node's stable ID was determined.
///
/// Documented in `docs/schema/repository-identity.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentitySource {
    /// Derived from the lowest-name-sorted git remote URL (normalized).
    Remote,
    /// Derived from the root commit SHA (no remote available).
    LocalRootCommit,
    /// Derived from the canonical absolute path (no git or no commits).
    /// Not stable across machines; unsafe for shared daemon stores.
    LocalPath,
    /// Supplied directly by the operator via `--repo-id-override`.
    OperatorOverride,
}

/// Identity payload carried on every `Repository` node.
///
/// Describes how the node's stable ID was derived. Present only on
/// `NodeKind::Repository` nodes; absent on all other node kinds.
///
/// Documented in `docs/schema/repository-identity.md`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryIdentityPayload {
    /// How the stable ID was computed.
    pub identity_source: IdentitySource,
    /// Normalized canonical remote URL (when `identity_source = remote`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// Root commit SHA (when `identity_source = local_root_commit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_commit_sha: Option<String>,
    /// Canonical absolute path (when `identity_source = local_path`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
    /// Directory basename; for display only, never an identity input.
    pub basename: String,
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
        /// Source-order ordinal scoped to `(repo_relative_path, symbol_kind, name)`.
        ///
        /// Present on `Symbol` nodes. This is an identity component for symbols,
        /// but `span` is not; see `docs/adr/0004-symbol-identity.md`.
        #[serde(skip_serializing_if = "Option::is_none")]
        disambiguator: Option<u64>,
        /// Git and bitemporal provenance for history-backed records.
        #[serde(skip_serializing_if = "Option::is_none")]
        temporal: Option<TemporalMetadata>,
        /// Semantic drift details for drift marker nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        semantic_drift: Option<Box<SemanticDriftMetadata>>,
        /// Evidence citations for agent-memory nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence_links: Option<Vec<EvidenceLink>>,
        /// Identity payload for `Repository` nodes; absent on all other kinds.
        #[serde(skip_serializing_if = "Option::is_none")]
        repository_identity: Option<Box<RepositoryIdentityPayload>>,
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
        /// RFC 3339 valid time for current-tree (non-history) scan records.
        /// For history-backed records use `temporal.valid_time` instead.
        #[serde(skip_serializing_if = "Option::is_none")]
        valid_time: Option<String>,
        /// Source of the `valid_time` field for current-tree records.
        /// For history-backed records see `temporal.valid_time_source`.
        #[serde(skip_serializing_if = "Option::is_none")]
        valid_time_source: Option<String>,
        // ── Project-domain fields (docs/schema/project-graph.md) ─────────────
        /// Stable entity ID within the project domain across mutations.
        #[serde(skip_serializing_if = "Option::is_none")]
        entity_id: Option<String>,
        /// Task title.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Redacted body handle for task body text.
        #[serde(skip_serializing_if = "Option::is_none")]
        body_handle: Option<Box<OutputHandle>>,
        /// Project task source kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_kind: Option<String>,
        /// Record ID of the `ExternalLink` carrying the source-system handle.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_external_link_id: Option<String>,
        /// Opaque assignee identifiers.
        #[serde(skip_serializing_if = "Option::is_none")]
        assignees: Option<Vec<String>>,
        /// Project labels.
        #[serde(skip_serializing_if = "Option::is_none")]
        labels: Option<Vec<String>>,
        /// Project priority enum value.
        #[serde(skip_serializing_if = "Option::is_none")]
        priority: Option<String>,
        /// Owning `Task` record ID for an `AcceptanceCriterion`.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<String>,
        /// Position within the parent task's AC list.
        #[serde(skip_serializing_if = "Option::is_none")]
        ordinal: Option<u32>,
        /// Optional verification-domain record that closed an AC.
        #[serde(skip_serializing_if = "Option::is_none")]
        verification_link_id: Option<String>,
        /// External system enum value for `ExternalLink`.
        #[serde(skip_serializing_if = "Option::is_none")]
        system: Option<String>,
        /// Canonical URL or `file://` path for `ExternalLink`.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Source-system-native ID for `ExternalLink`.
        #[serde(skip_serializing_if = "Option::is_none")]
        system_native_id: Option<String>,
        /// VCS remote when applicable.
        #[serde(skip_serializing_if = "Option::is_none")]
        repository_remote: Option<String>,
        /// RFC3339 timestamp when an importer first saw this external handle.
        #[serde(skip_serializing_if = "Option::is_none")]
        discovered_at: Option<String>,
        /// RFC3339 store transaction time for project-domain mutations.
        #[serde(skip_serializing_if = "Option::is_none")]
        transaction_time: Option<String>,
        /// Agent-facing summary.
        summary: String,
        // ── Importer provenance fields (absent for non-imported records) ─────
        /// Domain identifier (`"agent_memory"`, `"codegraph"`).
        #[serde(skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
        /// Importer identity for trajectory-imported records (e.g. `"traj-importer"`).
        #[serde(skip_serializing_if = "Option::is_none")]
        importer_id: Option<String>,
        /// Importer version string (e.g. `"0.1.0"`).
        #[serde(skip_serializing_if = "Option::is_none")]
        importer_version: Option<String>,
        /// Repo-relative or fixture-relative path to the raw source artifact.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_artifact_path: Option<String>,
        /// BLAKE3 hex hash of the raw source artifact bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        source_artifact_hash: Option<String>,
        // ── Record-type-specific fields (M2 trajectory import) ───────────────
        /// Patch validation status for `PatchArtifact` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_status: Option<String>,
        /// Git SHA the patch was authored against, when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        base_commit: Option<String>,
        /// Reason `base_commit` is absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        unknown_base_reason: Option<String>,
        /// Repo-relative files touched by the patch.
        #[serde(skip_serializing_if = "Option::is_none")]
        target_files: Option<Vec<String>>,
        /// BLAKE3 hash of the raw patch bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_bytes_hash: Option<String>,
        /// Raw patch byte length.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_bytes_size: Option<u64>,
        /// Storage handle for raw patch bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        patch_handle: Option<Box<PatchHandle>>,
        /// Human-readable validation reason, redacted by policy.
        #[serde(skip_serializing_if = "Option::is_none")]
        validation_summary: Option<String>,
        /// `AgentSession` record ID that produced this patch.
        #[serde(skip_serializing_if = "Option::is_none")]
        producer_session_id: Option<String>,
        /// File edit operation kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        edit_kind: Option<String>,
        /// BLAKE3 hash before a file edit.
        #[serde(skip_serializing_if = "Option::is_none")]
        before_hash: Option<String>,
        /// BLAKE3 hash after a file edit.
        #[serde(skip_serializing_if = "Option::is_none")]
        after_hash: Option<String>,
        /// Rename target path for rename edits.
        #[serde(skip_serializing_if = "Option::is_none")]
        rename_to: Option<String>,
        /// Number of hunks in the edit.
        #[serde(skip_serializing_if = "Option::is_none")]
        hunk_count: Option<u32>,
        /// `PatchArtifact` record ID containing this edit's bytes.
        #[serde(skip_serializing_if = "Option::is_none")]
        linked_patch_id: Option<String>,
        /// `AgentTurn` record ID associated with this action.
        #[serde(skip_serializing_if = "Option::is_none")]
        linked_turn_id: Option<String>,
        /// Invoked tool name.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// Producer-classified tool behavior kind.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_kind: Option<String>,
        /// Redacted one-line argument summary.
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_summary: Option<String>,
        /// Raw arguments handle.
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_handle: Option<Box<OutputHandle>>,
        /// Tool output handle for non-verification output.
        #[serde(skip_serializing_if = "Option::is_none")]
        result_handle: Option<Box<OutputHandle>>,
        /// Verification record produced by this tool call.
        #[serde(skip_serializing_if = "Option::is_none")]
        produced_evidence_id: Option<String>,
        /// RFC 3339 tool start time.
        #[serde(skip_serializing_if = "Option::is_none")]
        started_at: Option<String>,
        /// RFC 3339 tool finish time; absent when interrupted.
        #[serde(skip_serializing_if = "Option::is_none")]
        finished_at: Option<String>,
        /// Failure classification for `Failure` records: `"command_failure"`, `"patch_invalid"`, etc.
        #[serde(skip_serializing_if = "Option::is_none")]
        failure_kind: Option<String>,
        /// Shell exit code for `CommandRun` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        /// Zero-based turn index within an `AgentRun` for `AgentTurn` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_index: Option<u64>,
        // ── Verification-domain fields (docs/schema/verification.md) ─────────
        /// Standard output handle for verification-domain `CommandRun` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        stdout_handle: Option<Box<OutputHandle>>,
        /// Standard error handle for verification-domain `CommandRun` records.
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr_handle: Option<Box<OutputHandle>>,
        /// Evidence quality enum for verification-domain records:
        /// `verbatim`, `summarized`, or `referenced_only`.
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence_quality: Option<String>,
        /// RFC 3339 timestamp when the evidence was produced (verification domain).
        #[serde(skip_serializing_if = "Option::is_none")]
        executed_at: Option<String>,
        /// Verification kind for `Verification` umbrella records:
        /// `test_run`, `command_run`, `ci_status`, etc.
        #[serde(skip_serializing_if = "Option::is_none")]
        verification_kind: Option<String>,
        /// Pass/fail status for verification-domain records:
        /// `passed`, `failed`, `errored`, `skipped`, or `inconclusive`.
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
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
            disambiguator: None,
            temporal: None,
            semantic_drift: None,
            evidence_links: None,
            repository_identity: None,
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
            valid_time: None,
            valid_time_source: None,
            entity_id: None,
            title: None,
            body_handle: None,
            source_kind: None,
            source_external_link_id: None,
            assignees: None,
            labels: None,
            priority: None,
            parent_task_id: None,
            ordinal: None,
            verification_link_id: None,
            system: None,
            url: None,
            system_native_id: None,
            repository_remote: None,
            discovered_at: None,
            transaction_time: None,
            summary,
            domain: None,
            importer_id: None,
            importer_version: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            patch_status: None,
            base_commit: None,
            unknown_base_reason: None,
            target_files: None,
            patch_bytes_hash: None,
            patch_bytes_size: None,
            patch_handle: None,
            validation_summary: None,
            producer_session_id: None,
            edit_kind: None,
            before_hash: None,
            after_hash: None,
            rename_to: None,
            hunk_count: None,
            linked_patch_id: None,
            linked_turn_id: None,
            tool_name: None,
            tool_kind: None,
            arguments_summary: None,
            arguments_handle: None,
            result_handle: None,
            produced_evidence_id: None,
            started_at: None,
            finished_at: None,
            failure_kind: None,
            exit_code: None,
            turn_index: None,
            stdout_handle: None,
            stderr_handle: None,
            evidence_quality: None,
            executed_at: None,
            verification_kind: None,
            status: None,
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
            disambiguator: None,
            temporal: None,
            semantic_drift: None,
            evidence_links: None,
            repository_identity: None,
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
            valid_time: None,
            valid_time_source: None,
            entity_id: None,
            title: None,
            body_handle: None,
            source_kind: None,
            source_external_link_id: None,
            assignees: None,
            labels: None,
            priority: None,
            parent_task_id: None,
            ordinal: None,
            verification_link_id: None,
            system: None,
            url: None,
            system_native_id: None,
            repository_remote: None,
            discovered_at: None,
            transaction_time: None,
            summary,
            domain: None,
            importer_id: None,
            importer_version: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            patch_status: None,
            base_commit: None,
            unknown_base_reason: None,
            target_files: None,
            patch_bytes_hash: None,
            patch_bytes_size: None,
            patch_handle: None,
            validation_summary: None,
            producer_session_id: None,
            edit_kind: None,
            before_hash: None,
            after_hash: None,
            rename_to: None,
            hunk_count: None,
            linked_patch_id: None,
            linked_turn_id: None,
            tool_name: None,
            tool_kind: None,
            arguments_summary: None,
            arguments_handle: None,
            result_handle: None,
            produced_evidence_id: None,
            started_at: None,
            finished_at: None,
            failure_kind: None,
            exit_code: None,
            turn_index: None,
            stdout_handle: None,
            stderr_handle: None,
            evidence_quality: None,
            executed_at: None,
            verification_kind: None,
            status: None,
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
            disambiguator: Some(0),
            temporal: None,
            semantic_drift: None,
            evidence_links: None,
            repository_identity: None,
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
            valid_time: None,
            valid_time_source: None,
            entity_id: None,
            title: None,
            body_handle: None,
            source_kind: None,
            source_external_link_id: None,
            assignees: None,
            labels: None,
            priority: None,
            parent_task_id: None,
            ordinal: None,
            verification_link_id: None,
            system: None,
            url: None,
            system_native_id: None,
            repository_remote: None,
            discovered_at: None,
            transaction_time: None,
            summary,
            domain: None,
            importer_id: None,
            importer_version: None,
            source_artifact_path: None,
            source_artifact_hash: None,
            patch_status: None,
            base_commit: None,
            unknown_base_reason: None,
            target_files: None,
            patch_bytes_hash: None,
            patch_bytes_size: None,
            patch_handle: None,
            validation_summary: None,
            producer_session_id: None,
            edit_kind: None,
            before_hash: None,
            after_hash: None,
            rename_to: None,
            hunk_count: None,
            linked_patch_id: None,
            linked_turn_id: None,
            tool_name: None,
            tool_kind: None,
            arguments_summary: None,
            arguments_handle: None,
            result_handle: None,
            produced_evidence_id: None,
            started_at: None,
            finished_at: None,
            failure_kind: None,
            exit_code: None,
            turn_index: None,
            stdout_handle: None,
            stderr_handle: None,
            evidence_quality: None,
            executed_at: None,
            verification_kind: None,
            status: None,
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

    /// Sets an explicit domain and schema version on a node record.
    #[must_use]
    pub fn with_domain(mut self, domain_name: &str, schema: u32) -> Self {
        if let Self::Node {
            domain,
            schema_version,
            ..
        } = &mut self
        {
            *domain = Some(domain_name.to_owned());
            *schema_version = schema;
        }
        self
    }

    /// Attaches repository identity payload to a `Repository` node.
    #[must_use]
    pub fn with_repository_identity(mut self, payload: RepositoryIdentityPayload) -> Self {
        if let Self::Node {
            repository_identity,
            ..
        } = &mut self
        {
            *repository_identity = Some(Box::new(payload));
        }
        self
    }

    /// Stamps inferred `valid_time` and `valid_time_source` on current-tree scan records.
    ///
    /// Used by `scan_repository_at` to satisfy the rule from
    /// `docs/schema/temporal-selectors.md`: when no commit anchors the record,
    /// set `valid_time` = `transaction_time` and `valid_time_source` =
    /// `"inferred_from_transaction_time"`.
    #[must_use]
    pub fn with_valid_time_inferred(mut self, transaction_time: &str) -> Self {
        if let Self::Node {
            valid_time,
            valid_time_source,
            ..
        } = &mut self
        {
            *valid_time = Some(transaction_time.to_owned());
            *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        }
        self
    }

    /// Stamps explicit base time fields on a node record.
    #[must_use]
    pub fn with_node_time(
        mut self,
        node_valid_time: impl Into<String>,
        node_valid_time_source: impl Into<String>,
        node_ingested_at: impl Into<String>,
    ) -> Self {
        if let Self::Node {
            valid_time,
            valid_time_source,
            ingested_at,
            ..
        } = &mut self
        {
            *valid_time = Some(node_valid_time.into());
            *valid_time_source = Some(node_valid_time_source.into());
            *ingested_at = Some(node_ingested_at.into());
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
    /// Source of the `valid_time` field. See `docs/schema/temporal-selectors.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time_source: Option<String>,
}

/// Reserved graph domains.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// Source-derived code facts.
    CodeGraph,
    /// Agent-authored observations and session state.
    AgentMemory,
    /// Runtime, test, CI, benchmark, and proof evidence.
    Verification,
    /// Durable generated or external artifacts.
    Artifact,
    /// Product, project, task, and acceptance-criterion state.
    Project,
    /// Semantic measurements that require source bytes plus model bytes.
    Semantic,
}

impl Domain {
    /// Returns the serialized domain name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodeGraph => "codegraph",
            Self::AgentMemory => "agent_memory",
            Self::Verification => "verification",
            Self::Artifact => "artifact",
            Self::Project => "project",
            Self::Semantic => "semantic",
        }
    }
}

/// Structured identity for an embedding model.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingModel {
    /// Provider boundary that supplied the model.
    pub provider: String,
    /// Model name from the provider registry.
    pub name: String,
    /// Provider or crate version pin.
    pub version: String,
    /// Dense vector dimensionality.
    pub dim: u32,
    /// BLAKE3 hash of model weights, or `unknown` when unavailable.
    pub content_hash: String,
}

/// Semantic distance metric used by a drift measurement.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// One minus cosine similarity.
    CosineDistance,
    /// Reserved Euclidean distance metric.
    L2Distance,
    /// Reserved learned semantic delta scorer.
    LearnedDeltaV1,
}

impl MetricKind {
    /// Returns the serialized metric kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CosineDistance => "cosine_distance",
            Self::L2Distance => "l2_distance",
            Self::LearnedDeltaV1 => "learned_delta_v1",
        }
    }
}

/// Selection policy that caused a drift record to be emitted.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionBasis {
    /// Emit every drift whose score is greater than or equal to the threshold.
    ThresholdOnly,
    /// Reserved policy: top K drifts per compared pair.
    TopKPerPair,
    /// Reserved policy: top K drifts per symbol.
    TopKPerSymbol,
}

impl SelectionBasis {
    /// Returns the serialized selection basis.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThresholdOnly => "threshold_only",
            Self::TopKPerPair => "top_k_per_pair",
            Self::TopKPerSymbol => "top_k_per_symbol",
        }
    }
}

/// Structured metadata for a semantic drift measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticDriftMetadata {
    /// Structured embedding model identity.
    pub embedding_model: EmbeddingModel,
    /// Stable graph record ID for the later drift target.
    pub target_record_id: String,
    /// Stable graph record ID for the prior drift target.
    pub prior_record_id: String,
    /// Commit SHA for the earlier embedding.
    pub before_git_commit: String,
    /// Commit SHA for the later embedding.
    pub after_git_commit: String,
    /// Valid time for the earlier embedding.
    pub before_valid_time: String,
    /// Valid time for the later embedding.
    pub after_valid_time: String,
    /// Semantic distance metric.
    pub metric_kind: MetricKind,
    /// Drift score as a JSON number.
    pub score: f64,
    /// Producer threshold that selected this record.
    pub selection_threshold: f64,
    /// Producer selection policy.
    pub selection_basis: SelectionBasis,
}

impl Eq for SemanticDriftMetadata {}

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
    /// Reserved per-model registry record in the semantic domain.
    EmbeddingModel,
    /// Reserved persisted embedding vector record in the semantic domain.
    EmbeddingVector,
    /// Agent process or human actor writing observations.
    Agent,
    /// One agent run or conversation session.
    AgentSession,
    /// Agent-authored memory or discovery.
    Observation,
    /// Project-domain work item.
    Task,
    /// Falsifiable requirement attached to a task.
    AcceptanceCriterion,
    /// Source-system handle for project-domain records.
    ExternalLink,
    /// Long-lived product/repository initiative (project domain, reserved).
    Product,
    /// Bounded area of work under a product (project domain, reserved).
    Project,
    /// Strategy, milestone, or implementation plan (project domain, reserved).
    Plan,
    /// GitHub-specific issue metadata (project domain, reserved).
    GitHubIssue,
    /// GitHub pull-request metadata (project domain, reserved).
    PR,
    /// Review comment, finding, approval, or requested change (project domain, reserved).
    Review,
    /// Local project/task JSONL work item (project domain, reserved).
    LocalTask,
    /// File, patch, report, or generated output linked to work.
    Artifact,
    /// Evidence for a claim, test, or check.
    Verification,
    /// Command output or terminal evidence.
    CommandEvidence,
    // ── M2 trajectory-importer node kinds (docs/schema/agent-memory.md §4) ───
    /// A bounded attempt to complete a task (M2 trajectory import).
    AgentRun,
    /// One turn within an agent run: assistant action + observation (M2).
    AgentTurn,
    /// Structured tool invocation and result handle (M2).
    ToolCall,
    /// Shell command with exit status and output handle (M2).
    CommandRun,
    /// File path, diff handle, and edit provenance (M2).
    FileEdit,
    /// Patch content, validation status, and source trajectory (M2).
    PatchArtifact,
    /// Failed command, invalid patch, or blocked workflow (M2).
    Failure,
    /// Durable decision inferred from explicit context (reserved, agent-memory §4a).
    Decision,
    // ── Verification-domain node kinds (docs/schema/verification.md) ─────────
    /// One test command invocation that ran one or more tests (verification domain).
    TestRun,
    /// CI/CD run status — reserved for M9-adjacent project-graph integration.
    CIStatus,
    /// Criterion/cargo-bench/perf benchmark result — reserved.
    BenchmarkRun,
    /// Coverage tool output with per-file/per-line coverage data — reserved.
    CoverageReport,
    /// Verus/Kani/CBMC/Lean/Coq proof outcome — reserved.
    ProofResult,
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
            Self::EmbeddingModel => "EmbeddingModel",
            Self::EmbeddingVector => "EmbeddingVector",
            Self::Agent => "Agent",
            Self::AgentSession => "AgentSession",
            Self::Observation => "Observation",
            Self::Task => "Task",
            Self::AcceptanceCriterion => "AcceptanceCriterion",
            Self::ExternalLink => "ExternalLink",
            Self::Product => "Product",
            Self::Project => "Project",
            Self::Plan => "Plan",
            Self::GitHubIssue => "GitHubIssue",
            Self::PR => "PR",
            Self::Review => "Review",
            Self::LocalTask => "LocalTask",
            Self::Artifact => "Artifact",
            Self::Verification => "Verification",
            Self::CommandEvidence => "CommandEvidence",
            Self::AgentRun => "AgentRun",
            Self::AgentTurn => "AgentTurn",
            Self::ToolCall => "ToolCall",
            Self::CommandRun => "CommandRun",
            Self::FileEdit => "FileEdit",
            Self::PatchArtifact => "PatchArtifact",
            Self::Failure => "Failure",
            Self::Decision => "Decision",
            Self::TestRun => "TestRun",
            Self::CIStatus => "CIStatus",
            Self::BenchmarkRun => "BenchmarkRun",
            Self::CoverageReport => "CoverageReport",
            Self::ProofResult => "ProofResult",
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
    /// Semantic drift prior-version target.
    DriftsPrior,
    /// Semantic drift measurement model edge.
    MeasuredBy,
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
    /// Agent-memory tool call produced verification evidence.
    ProducedEvidence,
    /// Agent-memory node is validated by an evidence record.
    ValidatedBy,
    /// Project acceptance criterion is closed by verification evidence.
    ClosesAcceptanceCriterion,
    /// Project acceptance criterion belongs to a task.
    OwnedByTask,
    /// Project record points to its external source handle.
    ExternalHandle,
    /// Project task intends to touch a code-graph file.
    TouchesFile,
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
            "DRIFTS_PRIOR" => Some(Self::DriftsPrior),
            "MEASURED_BY" => Some(Self::MeasuredBy),
            "SESSION_OF" => Some(Self::SessionOf),
            "AUTHORED_BY" => Some(Self::AuthoredBy),
            "HAS_EVIDENCE" => Some(Self::HasEvidence),
            "OBSERVES" => Some(Self::Observes),
            "MENTIONS_SYMBOL" => Some(Self::MentionsSymbol),
            "TOUCHED_FILE" => Some(Self::TouchedFile),
            "PRODUCED_PATCH" => Some(Self::ProducedPatch),
            "PRODUCED_EVIDENCE" => Some(Self::ProducedEvidence),
            "VALIDATED_BY" => Some(Self::ValidatedBy),
            "CLOSES_ACCEPTANCE_CRITERION" => Some(Self::ClosesAcceptanceCriterion),
            "OWNED_BY_TASK" => Some(Self::OwnedByTask),
            "EXTERNAL_HANDLE" => Some(Self::ExternalHandle),
            "TOUCHES_FILE" => Some(Self::TouchesFile),
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
                | Self::ProducedEvidence
                | Self::ValidatedBy
                | Self::ClosesAcceptanceCriterion
                | Self::OwnedByTask
                | Self::ExternalHandle
                | Self::TouchesFile
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
                | Self::DriftsPrior
                | Self::MeasuredBy
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
            Self::DriftsPrior => "DRIFTS_PRIOR",
            Self::MeasuredBy => "MEASURED_BY",
            Self::SessionOf => "SESSION_OF",
            Self::AuthoredBy => "AUTHORED_BY",
            Self::HasEvidence => "HAS_EVIDENCE",
            Self::Observes => "OBSERVES",
            Self::MentionsSymbol => "MENTIONS_SYMBOL",
            Self::TouchedFile => "TOUCHED_FILE",
            Self::ProducedPatch => "PRODUCED_PATCH",
            Self::ProducedEvidence => "PRODUCED_EVIDENCE",
            Self::ValidatedBy => "VALIDATED_BY",
            Self::ClosesAcceptanceCriterion => "CLOSES_ACCEPTANCE_CRITERION",
            Self::OwnedByTask => "OWNED_BY_TASK",
            Self::ExternalHandle => "EXTERNAL_HANDLE",
            Self::TouchesFile => "TOUCHES_FILE",
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

/// Builds a code-graph ID using an explicit schema version rather than the current one.
///
/// Used when tombstoning records that were produced by an older version of the extractor;
/// the deleted ID must match the prefix that was in use when the record was first written.
#[must_use]
pub(crate) fn versioned_stable_id(version: u32, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("codegraph:v{version}:{}", hasher.finalize().to_hex())
}

/// Builds a stable verification-domain record ID.
///
/// Uses the `verification:v1:` prefix so verification IDs cannot collide with
/// code-graph or agent-memory IDs. Documented in `docs/schema/verification.md`.
#[must_use]
pub fn verification_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.to_ascii_lowercase().as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "verification:v{VERIFICATION_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable artifact-domain record ID.
///
/// Uses the `artifact:v1:` prefix so artifact IDs cannot collide with
/// code-graph, agent-memory, or verification IDs. Documented in
/// `docs/schema/agent-actions.md`.
#[must_use]
pub fn artifact_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "artifact:v{ARTIFACT_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable project-domain record ID.
///
/// Uses the `project:v1:` prefix so project IDs cannot collide with code-graph,
/// agent-memory, artifact, or verification IDs. Documented in
/// `docs/schema/project-graph.md`.
#[must_use]
pub fn project_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "project:v{PROJECT_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable semantic-domain record ID.
///
/// Uses the `semantic:v1:` prefix so semantic records cannot collide with
/// source-derived code-graph IDs. Documented in `docs/schema/semantic-drift.md`.
#[must_use]
pub fn semantic_stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!(
        "semantic:v{SEMANTIC_SCHEMA_VERSION}:{}",
        hasher.finalize().to_hex()
    )
}

/// Builds a stable agent-memory record ID.
///
/// Uses the `agent_memory:v1:` prefix so agent-memory IDs cannot collide with
/// code-graph `codegraph:v2:` IDs even when the content hashes are identical.
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
