// clippy::too_long_first_doc_paragraph fires on this module's doc without a span
// (nursery lint span-reporting bug in clippy 0.1.94); suppress at module level.
#![allow(clippy::too_long_first_doc_paragraph)]
//! Typed evidence write workflows for agent observations, command evidence,
//! artifacts, and verification results.
//!
//! This module is the default write contract for interactive clients, SDKs, and
//! the future MCP surface. It enforces provenance, trust separation, and
//! content-addressed idempotency for every accepted write, without requiring
//! callers to construct raw graph records or know AletheiaDB schema details.
//!
//! Raw `eg ingest` remains available for batch importers; see `docs/schema/agent-memory.md`
//! and `docs/schema/verification.md` for the full field specifications.
//!
//! ## Provenance contract (AC2)
//!
//! Every accepted write carries:
//! - `agent_id` — stable agent identity
//! - `session_id` — the active session
//! - `observed_at` — RFC 3339 wall-clock observation time
//! - `source_handle` / `source_artifact_path` + `source_artifact_hash` — citable source
//!
//! Writes missing any required provenance field fail with a [`ProvenanceError`] that
//! names the field and never echoes sensitive payload text.

use std::collections::BTreeMap;
use std::fmt;

use crate::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, EdgeLabel, EvidenceLink, GraphRecord,
    NodeKind, OutputHandle, PatchHandle, Producer, ProducerKind, VERIFICATION_SCHEMA_VERSION,
    agent_memory_stable_id, artifact_stable_id, verification_stable_id,
};

// ── Constants ─────────────────────────────────────────────────────────────────

const INLINE_PAYLOAD_CEILING: u64 = 16 * 1024;

/// `patch_status` values that require a known `base_commit`.
const APPLIED_STATUSES: &[&str] = &["applied_clean", "applied_with_conflicts"];

/// Accepted `patch_status` values (from `docs/schema/agent-actions.md`).
const PATCH_STATUS_VALUES: &[&str] = &[
    "applied_clean",
    "applied_with_conflicts",
    "invalid_syntax",
    "invalid_no_base",
    "rejected_validation",
    "unverified",
    "superseded",
];

// ── Core types ────────────────────────────────────────────────────────────────

/// Required provenance fields shared by all typed evidence write requests.
///
/// Every accepted write must carry `agent_id`, `session_id`, `observed_at`,
/// and either `source_handle` (for agent-memory domain writes) or the
/// `source_artifact_path` / `source_artifact_hash` fields on the specific request type.
///
/// `agent_kind` defaults to `"other"` when left empty; callers SHOULD supply the
/// specific agent kind from the published enum.
#[derive(Debug, Clone)]
pub struct EvidenceProvenance {
    /// Stable agent identity string; must be non-empty.
    pub agent_id: String,
    /// Agent kind from the published enum (e.g. `"claude-code"`). Defaults to `"other"`.
    pub agent_kind: String,
    /// Active session identifier; must be non-empty.
    pub session_id: String,
    /// RFC 3339 wall-clock time of observation; must be non-empty.
    pub observed_at: String,
    /// Citable source artifact path or hash for agent-memory domain writes.
    /// Required for `ObservationRequest` and `ArtifactRequest`.
    pub source_handle: Option<String>,
}

/// Machine-readable provenance or field validation error.
///
/// The `code` is always a stable, machine-parseable string (`"missing_field"` or
/// `"invalid_field"`). The `field` names the exact field path. The error message
/// **never echoes sensitive payload text**.
#[derive(Debug, Eq, PartialEq)]
pub struct ProvenanceError {
    /// Stable error code: `"missing_field"` or `"invalid_field"`.
    pub code: &'static str,
    /// Dot-separated field path (e.g. `"agent_id"`, `"source_handle"`).
    pub field: String,
}

impl ProvenanceError {
    /// Creates a `missing_field` error for the named field.
    #[must_use]
    pub fn missing(field: impl Into<String>) -> Self {
        Self {
            code: "missing_field",
            field: field.into(),
        }
    }

    /// Creates an `invalid_field` error for the named field.
    #[must_use]
    pub fn invalid(field: impl Into<String>) -> Self {
        Self {
            code: "invalid_field",
            field: field.into(),
        }
    }
}

impl fmt::Display for ProvenanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: field '{}' is required", self.code, self.field)
    }
}

/// Outcome of a successful typed evidence write.
///
/// `record_id` is the stable content-addressed ID of the primary evidence node;
/// it can be used as a citation handle in future `EvidenceLink` records.
/// `evidence_handle` is always equal to `record_id` in this implementation.
#[derive(Debug)]
pub struct EvidenceWriteOutcome {
    /// Stable record ID for the primary evidence node (citable handle).
    pub record_id: String,
    /// Citable reference — always equal to `record_id`.
    pub evidence_handle: String,
    /// All graph records in the write batch (nodes + edges).
    pub records: Vec<GraphRecord>,
}

// ── Request types ─────────────────────────────────────────────────────────────

/// Typed request for writing an agent `Observation` node.
///
/// The `evidence_links` must contain at least one entry that anchors the claim
/// to a code-graph node or verification record. An observation without evidence
/// links cannot be accepted per `docs/schema/agent-memory.md`.
pub struct ObservationRequest {
    /// Provenance fields required on every agent-memory write.
    pub provenance: EvidenceProvenance,
    /// Observation body text; must be non-empty.
    pub text: String,
    /// Extraction confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Evidence citations. At least one is required.
    pub evidence_links: Vec<EvidenceLink>,
}

/// Typed request for writing a `CommandRun` (command evidence) record.
///
/// `CommandRun` nodes live in the **verification** domain (`verification:v1:` prefix).
/// Either `source_artifact_path` or `source_artifact_hash` must be non-empty.
pub struct CommandEvidenceRequest {
    /// Provenance fields required on every write.
    pub provenance: EvidenceProvenance,
    /// RFC 3339 wall-clock time the command was executed.
    pub executed_at: String,
    /// Shell exit code.
    pub exit_code: i64,
    /// Optional captured stdout text.
    pub stdout: Option<String>,
    /// Optional captured stderr text.
    pub stderr: Option<String>,
    /// Evidence quality enum: `"verbatim"`, `"summarized"`, or `"referenced_only"`.
    pub evidence_quality: String,
    /// Repository-relative or absolute path to the source artifact.
    pub source_artifact_path: String,
    /// BLAKE3 or SHA256 hex hash of the source artifact.
    pub source_artifact_hash: String,
}

/// Typed request for writing a `PatchArtifact` record.
///
/// `PatchArtifact` nodes live in the **artifact** domain (`artifact:v1:` prefix).
/// Both `source_artifact_path` and `source_artifact_hash` must be non-empty.
pub struct ArtifactRequest {
    /// Provenance fields required on every write.
    pub provenance: EvidenceProvenance,
    /// Raw patch bytes (unified diff).
    pub patch_bytes: Vec<u8>,
    /// Repository-relative target files touched by the patch.
    pub target_files: Vec<String>,
    /// Patch validation status from the published enum.
    pub patch_status: String,
    /// Git commit SHA the patch was authored against, when known.
    pub base_commit: Option<String>,
    /// Repository-relative or absolute path to the source artifact.
    pub source_artifact_path: String,
    /// BLAKE3 or SHA256 hex hash of the source artifact.
    pub source_artifact_hash: String,
    /// Human-readable summary of any validation performed on this artifact.
    pub validation_summary: String,
}

/// Typed request for writing a `Verification` record.
///
/// `Verification` nodes live in the **verification** domain (`verification:v1:` prefix).
/// Both `source_artifact_path` and `source_artifact_hash` must be non-empty.
pub struct VerificationRequest {
    /// Provenance fields required on every write.
    pub provenance: EvidenceProvenance,
    /// RFC 3339 wall-clock time the verification was executed.
    pub executed_at: String,
    /// Verification outcome: `"pass"`, `"fail"`, `"skip"`, `"error"`, or `"timeout"`.
    pub status: String,
    /// Verification subtype: `"test_run"`, `"command_run"`, `"ci_status"`, etc.
    pub verification_kind: String,
    /// Optional captured stdout text (for `test_run` and `command_run`).
    pub stdout: Option<String>,
    /// Evidence quality enum: `"verbatim"`, `"summarized"`, or `"referenced_only"`.
    pub evidence_quality: String,
    /// Repository-relative or absolute path to the source artifact.
    pub source_artifact_path: String,
    /// BLAKE3 or SHA256 hex hash of the source artifact.
    pub source_artifact_hash: String,
    /// Optional stable ID of a `CommandRun` record that produced this result.
    pub linked_command_evidence_id: Option<String>,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Validates the common provenance fields required on every agent-memory write.
///
/// Returns the first missing field as a [`ProvenanceError`], or `Ok(())` when all
/// required fields are present and non-empty. Payload text is never included in
/// the error message.
fn validate_provenance_base(prov: &EvidenceProvenance) -> Result<(), ProvenanceError> {
    if prov.agent_id.is_empty() {
        return Err(ProvenanceError::missing("agent_id"));
    }
    if prov.session_id.is_empty() {
        return Err(ProvenanceError::missing("session_id"));
    }
    if prov.observed_at.is_empty() {
        return Err(ProvenanceError::missing("observed_at"));
    }
    if chrono::DateTime::parse_from_rfc3339(&prov.observed_at).is_err() {
        return Err(ProvenanceError::invalid("observed_at"));
    }
    Ok(())
}

/// Validates that `source_handle` is present and non-empty (required for
/// `ObservationRequest` and `ArtifactRequest` per AC2).
fn validate_source_handle(prov: &EvidenceProvenance) -> Result<(), ProvenanceError> {
    if prov.source_handle.as_deref().is_none_or(str::is_empty) {
        return Err(ProvenanceError::missing("source_handle"));
    }
    Ok(())
}

/// Validates that at least one of `source_artifact_path` or `source_artifact_hash`
/// is non-empty (required for verification and command-evidence writes per AC2).
fn validate_source_artifact(path: &str, hash: &str) -> Result<(), ProvenanceError> {
    if path.is_empty() && hash.is_empty() {
        return Err(ProvenanceError::missing("source_artifact_path"));
    }
    Ok(())
}

/// Validates that both `source_artifact_path` and `source_artifact_hash` are
/// non-empty (required for `PatchArtifact` writes — the embedded daemon calls
/// `required_str` on both fields).
fn validate_source_artifact_both(path: &str, hash: &str) -> Result<(), ProvenanceError> {
    if path.is_empty() {
        return Err(ProvenanceError::missing("source_artifact_path"));
    }
    if hash.is_empty() {
        return Err(ProvenanceError::missing("source_artifact_hash"));
    }
    Ok(())
}

/// Returns the effective `agent_kind`, defaulting to `"other"` when empty.
const fn effective_agent_kind(prov: &EvidenceProvenance) -> &str {
    if prov.agent_kind.is_empty() {
        "other"
    } else {
        prov.agent_kind.as_str()
    }
}

const VALID_AGENT_KINDS: &[&str] = &[
    "codex",
    "claude-code",
    "vantage",
    "rust-swe-agent",
    "human",
    "other",
];

/// Validates the `agent_kind` against the published enum.
fn validate_agent_kind(prov: &EvidenceProvenance) -> Result<(), ProvenanceError> {
    if !VALID_AGENT_KINDS.contains(&effective_agent_kind(prov)) {
        return Err(ProvenanceError::invalid("agent_kind"));
    }
    Ok(())
}

const VALID_EVIDENCE_QUALITY: &[&str] = &["verbatim", "summarized", "referenced_only"];

/// Accepted `evidence_quality` values for verification records (`docs/schema/verification.md`).
const VALID_VERIFICATION_EVIDENCE_QUALITY: &[&str] = &["high", "medium", "low"];

/// Accepted `status` values for verification records (from `docs/schema/verification.md`).
const VALID_VERIFICATION_STATUSES: &[&str] = &["pass", "fail", "skip", "error", "timeout"];

/// Validates the `evidence_quality` field against the agent-memory domain enum.
fn validate_evidence_quality(quality: &str) -> Result<(), ProvenanceError> {
    if !VALID_EVIDENCE_QUALITY.contains(&quality) {
        return Err(ProvenanceError::invalid("evidence_quality"));
    }
    Ok(())
}

/// Validates the `evidence_quality` field against the verification domain enum.
fn validate_verification_evidence_quality(quality: &str) -> Result<(), ProvenanceError> {
    if !VALID_VERIFICATION_EVIDENCE_QUALITY.contains(&quality) {
        return Err(ProvenanceError::invalid("evidence_quality"));
    }
    Ok(())
}

/// Returns the current UTC time as an RFC 3339 string for `ingested_at`.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Builds an `OutputHandle` from optional inline text.
fn output_handle(content: &str) -> OutputHandle {
    let bytes = content.len() as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(content.as_bytes());
    OutputHandle {
        inline: (bytes <= INLINE_PAYLOAD_CEILING).then(|| content.to_owned()),
        hash: hasher.finalize().to_hex().to_string(),
        bytes,
    }
}

/// Builds the producer envelope for the evidence writer.
fn evidence_producer() -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::ObservationWriter,
        producer_components: BTreeMap::new(),
        producer_started_at: now_rfc3339(),
    }
}

// ── Agent and AgentSession node builders ─────────────────────────────────────

/// Builds an `Agent` node for the given `agent_id`.
///
/// Agent nodes carry a stable ID based on `agent_id` and `agent_kind`; the same
/// agent registered with different kinds produces distinct records, preventing
/// mismatched-payload conflicts in the embedded sink.
fn build_agent_node(agent_id: &str, agent_kind: &str) -> GraphRecord {
    let id = agent_memory_stable_id(&["node", "agent", agent_id, agent_kind]);
    GraphRecord::Node {
        id,
        kind: NodeKind::Agent,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        repo_relative_path: None,
        span: None,
        name: Some(agent_id.to_owned()),
        language: None,
        symbol_kind: None,
        disambiguator: None,
        visibility: None,
        signature: None,
        doc: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
        repository_identity: None,
        source_snapshot: None,
        text: None,
        superseded_by: None,
        agent_id: Some(agent_id.to_owned()),
        agent_kind: Some(agent_kind.to_owned()),
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
        summary: format!("Agent {agent_id}"),
        domain: Some("agent_memory".to_owned()),
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
        review_kind: None,
        review_state: None,
        in_reply_to_id: None,
        author: None,
        diff_hunk_handle: None,
        review_side: None,
        user_context: crate::ir::UserContextFields::empty(),
        producer: None,
    }
}

/// Builds an `AgentSession` node for the given session.
#[allow(clippy::too_many_lines)]
fn build_agent_session_node(prov: &EvidenceProvenance, agent_kind: &str) -> GraphRecord {
    let id = agent_memory_stable_id(&[
        "node",
        "agent_session",
        &prov.agent_id,
        &prov.session_id,
        agent_kind,
        &prov.observed_at,
    ]);
    GraphRecord::Node {
        id,
        kind: NodeKind::AgentSession,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        repo_relative_path: None,
        span: None,
        name: Some(prov.session_id.clone()),
        language: None,
        symbol_kind: None,
        disambiguator: None,
        visibility: None,
        signature: None,
        doc: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
        repository_identity: None,
        source_snapshot: None,
        text: None,
        superseded_by: None,
        agent_id: Some(prov.agent_id.clone()),
        agent_kind: Some(agent_kind.to_owned()),
        session_id: Some(prov.session_id.clone()),
        // observed_at is included in the stable ID so two writes with different observed_at
        // produce distinct session nodes. This makes each node payload-invariant for its ID
        // and satisfies the ingest validator's requirement for non-null timestamps on
        // agent-memory nodes other than Agent.
        observed_at: Some(prov.observed_at.clone()),
        ingested_at: Some(prov.observed_at.clone()),
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
        summary: format!("AgentSession {}", prov.session_id),
        domain: Some("agent_memory".to_owned()),
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
        review_kind: None,
        review_state: None,
        in_reply_to_id: None,
        author: None,
        diff_hunk_handle: None,
        review_side: None,
        user_context: crate::ir::UserContextFields::empty(),
        producer: None,
    }
}

/// Builds a `SESSION_OF` edge from an `AgentSession` to an `Agent`.
fn build_session_of_edge(session_id: &str, agent_id: &str) -> GraphRecord {
    let id = agent_memory_stable_id(&["edge", "SESSION_OF", session_id, agent_id]);
    GraphRecord::Edge {
        id,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        label: EdgeLabel::SessionOf,
        source: session_id.to_owned(),
        target: agent_id.to_owned(),
        confidence: None,
        resolution: None,
        temporal: None,
        summary: format!("AgentSession {session_id} SESSION_OF Agent {agent_id}"),
        producer: None,
    }
}

/// Builds an `AUTHORED_BY` edge from a record to its producing session.
fn build_authored_by_edge(record_id: &str, session_id: &str) -> GraphRecord {
    let id = agent_memory_stable_id(&["edge", "AUTHORED_BY", record_id, session_id]);
    GraphRecord::Edge {
        id,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        label: EdgeLabel::AuthoredBy,
        source: record_id.to_owned(),
        target: session_id.to_owned(),
        confidence: None,
        resolution: None,
        temporal: None,
        summary: format!("{record_id} AUTHORED_BY {session_id}"),
        producer: None,
    }
}

// ── Public builder functions ──────────────────────────────────────────────────

/// Builds a typed `Observation` record batch from a provenance-bearing request.
///
/// Validates all required provenance fields and returns a [`ProvenanceError`]
/// if any are missing or invalid. The error names the missing field and never
/// echoes payload text.
///
/// On success, returns an [`EvidenceWriteOutcome`] containing:
/// - A stable `agent_memory:v1:` record ID (the citable evidence handle)
/// - All graph records: `Agent`, `AgentSession`, `Observation` nodes plus
///   `SESSION_OF` and `AUTHORED_BY` edges
///
/// # Errors
///
/// Returns a [`ProvenanceError`] when any required field is missing or invalid.
#[allow(clippy::too_many_lines)]
pub fn build_observation_records(
    req: &ObservationRequest,
) -> Result<EvidenceWriteOutcome, ProvenanceError> {
    validate_provenance_base(&req.provenance)?;
    validate_agent_kind(&req.provenance)?;
    validate_source_handle(&req.provenance)?;

    if req.text.is_empty() {
        return Err(ProvenanceError::missing("text"));
    }
    if req.evidence_links.is_empty() {
        return Err(ProvenanceError::missing("evidence_links"));
    }
    if !(0.0..=1.0).contains(&req.confidence) {
        return Err(ProvenanceError::invalid("confidence"));
    }
    for link in &req.evidence_links {
        let c: f64 = link
            .confidence
            .parse()
            .map_err(|_| ProvenanceError::invalid("evidence_links.confidence"))?;
        if !(0.0..=1.0).contains(&c) {
            return Err(ProvenanceError::invalid("evidence_links.confidence"));
        }
        if link.relation.is_empty() {
            return Err(ProvenanceError::missing("evidence_links.relation"));
        }
        if link.target_domain.is_empty() {
            return Err(ProvenanceError::missing("evidence_links.target_domain"));
        }
        // Reject relation/domain mismatches the daemon would reject.
        if link.relation == EdgeLabel::Observes.as_str() && link.target_domain != "codegraph" {
            return Err(ProvenanceError::invalid("evidence_links.relation"));
        }
        if link.relation == EdgeLabel::ValidatedBy.as_str() && link.target_domain != "verification"
        {
            return Err(ProvenanceError::invalid("evidence_links.relation"));
        }
        // An explicitly-set but empty target_record_id is semantically invalid.
        if link.target_record_id.as_deref() == Some("") {
            return Err(ProvenanceError::missing("evidence_links.target_record_id"));
        }
    }

    let agent_kind = effective_agent_kind(&req.provenance);

    let agent_id_node =
        agent_memory_stable_id(&["node", "agent", &req.provenance.agent_id, agent_kind]);
    let session_node_id = agent_memory_stable_id(&[
        "node",
        "agent_session",
        &req.provenance.agent_id,
        &req.provenance.session_id,
        agent_kind,
        &req.provenance.observed_at,
    ]);

    // Redact free-text before hashing or storing — secrets must not reach the store.
    let redacted_text = crate::redaction::redact_value(&req.text);
    let redaction_policy_version = crate::redaction::is_redacted(&redacted_text)
        .then(|| crate::redaction::REDACTION_POLICY_VERSION.to_owned());

    // Observation ID is content-addressed on agent, session, text, observed_at, confidence,
    // and evidence-link targets so that the same text with different citations, timestamps,
    // or confidence values produces distinct records rather than mismatched-payload conflicts.
    let text_hash = {
        let mut hasher = blake3::Hasher::new();
        hasher.update(redacted_text.as_bytes());
        hasher.finalize().to_hex().to_string()
    };
    // Sort links canonically by all identity fields and hash the full payload so that
    // two observations with the same target but different relation/domain/confidence
    // produce distinct IDs, and same links in different submission order produce the
    // same ID and identical stored arrays.
    let mut sorted_links = req.evidence_links.clone();
    sorted_links.sort_unstable_by(|a, b| {
        a.relation
            .cmp(&b.relation)
            .then(a.target_domain.cmp(&b.target_domain))
            .then(
                a.target_record_id
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.target_record_id.as_deref().unwrap_or("")),
            )
            .then(a.confidence.cmp(&b.confidence))
            .then(
                a.target_repo_relative_path
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.target_repo_relative_path.as_deref().unwrap_or("")),
            )
            .then(
                a.as_of_commit
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.as_of_commit.as_deref().unwrap_or("")),
            )
            .then(
                a.target_git_commit
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.target_git_commit.as_deref().unwrap_or("")),
            )
            .then({
                let a_span = a.target_span.as_ref().map_or((0, 0, 0, 0), |s| {
                    (s.start_byte, s.end_byte, s.start_line, s.end_line)
                });
                let b_span = b.target_span.as_ref().map_or((0, 0, 0, 0), |s| {
                    (s.start_byte, s.end_byte, s.start_line, s.end_line)
                });
                a_span.cmp(&b_span)
            })
    });
    let links_hash = {
        let mut h = blake3::Hasher::new();
        for l in &sorted_links {
            h.update(l.target_record_id.as_deref().unwrap_or("").as_bytes());
            h.update(b"\0");
            h.update(l.relation.as_bytes());
            h.update(b"\0");
            h.update(l.target_domain.as_bytes());
            h.update(b"\0");
            h.update(l.confidence.as_bytes());
            h.update(b"\0");
            h.update(
                l.target_repo_relative_path
                    .as_deref()
                    .unwrap_or("")
                    .as_bytes(),
            );
            h.update(b"\0");
            h.update(l.as_of_commit.as_deref().unwrap_or("").as_bytes());
            h.update(b"\0");
            h.update(l.target_git_commit.as_deref().unwrap_or("").as_bytes());
            h.update(b"\0");
            let span_key = l.target_span.as_ref().map_or_else(String::new, |s| {
                format!(
                    "{}:{}:{}:{}",
                    s.start_byte, s.end_byte, s.start_line, s.end_line
                )
            });
            h.update(span_key.as_bytes());
            h.update(b"\0");
        }
        h.finalize().to_hex().to_string()
    };
    let obs_id = agent_memory_stable_id(&[
        "node",
        "observation",
        &req.provenance.agent_id,
        &req.provenance.session_id,
        &req.provenance.observed_at,
        agent_kind,
        &req.confidence.to_string(),
        &text_hash,
        &links_hash,
        req.provenance.source_handle.as_deref().unwrap_or(""),
    ]);

    // Build the observation node
    let obs_node = GraphRecord::Node {
        id: obs_id.clone(),
        kind: NodeKind::Observation,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        repo_relative_path: None,
        span: None,
        name: None,
        language: None,
        symbol_kind: None,
        disambiguator: None,
        visibility: None,
        signature: None,
        doc: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: Some(sorted_links),
        author_name: None,
        author_email: None,
        repository_identity: None,
        source_snapshot: None,
        text: Some(redacted_text.clone()),
        superseded_by: None,
        agent_id: Some(req.provenance.agent_id.clone()),
        agent_kind: Some(agent_kind.to_owned()),
        session_id: Some(req.provenance.session_id.clone()),
        observed_at: Some(req.provenance.observed_at.clone()),
        ingested_at: Some(req.provenance.observed_at.clone()),
        confidence: Some(req.confidence.to_string()),
        source_handle: req.provenance.source_handle.clone(),
        redaction_policy_version,
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
        summary: format!(
            "Observation by {} in {}: {}",
            req.provenance.agent_id,
            req.provenance.session_id,
            redacted_text.chars().take(60).collect::<String>()
        ),
        domain: Some("agent_memory".to_owned()),
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
        review_kind: None,
        review_state: None,
        in_reply_to_id: None,
        author: None,
        diff_hunk_handle: None,
        review_side: None,
        user_context: crate::ir::UserContextFields::empty(),
        producer: Some(evidence_producer()),
    };

    let agent_node = build_agent_node(&req.provenance.agent_id, agent_kind);
    let session_node = build_agent_session_node(&req.provenance, agent_kind);
    let session_of_edge = build_session_of_edge(&session_node_id, &agent_id_node);
    let authored_by_edge = build_authored_by_edge(&obs_id, &session_node_id);

    let records = vec![
        agent_node,
        session_node,
        session_of_edge,
        obs_node,
        authored_by_edge,
    ];

    Ok(EvidenceWriteOutcome {
        evidence_handle: obs_id.clone(),
        record_id: obs_id,
        records,
    })
}

/// Builds a typed `CommandEvidence` record batch (command evidence) from a
/// provenance-bearing request.
///
/// `CommandEvidence` nodes live in the **agent-memory** domain (`agent_memory:v1:` prefix).
/// Either `source_artifact_path` or `source_artifact_hash` must be non-empty.
///
/// # Errors
///
/// Returns a [`ProvenanceError`] when any required field is missing or invalid.
#[allow(clippy::too_many_lines)]
pub fn build_command_evidence_records(
    req: &CommandEvidenceRequest,
) -> Result<EvidenceWriteOutcome, ProvenanceError> {
    validate_provenance_base(&req.provenance)?;
    validate_agent_kind(&req.provenance)?;
    validate_source_artifact(&req.source_artifact_path, &req.source_artifact_hash)?;
    validate_evidence_quality(&req.evidence_quality)?;

    if req.executed_at.is_empty() {
        return Err(ProvenanceError::missing("executed_at"));
    }
    if chrono::DateTime::parse_from_rfc3339(&req.executed_at).is_err() {
        return Err(ProvenanceError::invalid("executed_at"));
    }

    let agent_kind = effective_agent_kind(&req.provenance);

    // Reject oversized stdout/stderr — the writer has no external storage for payloads
    // larger than the inline ceiling, so silently demoting them would produce unrecoverable handles.
    if req
        .stdout
        .as_deref()
        .is_some_and(|s| s.len() as u64 > INLINE_PAYLOAD_CEILING)
    {
        return Err(ProvenanceError::invalid("stdout"));
    }
    if req
        .stderr
        .as_deref()
        .is_some_and(|s| s.len() as u64 > INLINE_PAYLOAD_CEILING)
    {
        return Err(ProvenanceError::invalid("stderr"));
    }

    // Redact stdout/stderr before hashing or inline storage.
    let redacted_stdout = req.stdout.as_deref().map(crate::redaction::redact_value);
    let redacted_stderr = req.stderr.as_deref().map(crate::redaction::redact_value);
    // Redaction can expand content (e.g. short API key → longer marker); reject if the
    // redacted form still won't fit inline — no sidecar persistence is available.
    if redacted_stdout
        .as_deref()
        .is_some_and(|s| s.len() as u64 > INLINE_PAYLOAD_CEILING)
    {
        return Err(ProvenanceError::invalid("stdout"));
    }
    if redacted_stderr
        .as_deref()
        .is_some_and(|s| s.len() as u64 > INLINE_PAYLOAD_CEILING)
    {
        return Err(ProvenanceError::invalid("stderr"));
    }
    let stdout_redacted = redacted_stdout
        .as_deref()
        .is_some_and(crate::redaction::is_redacted);
    let stderr_redacted = redacted_stderr
        .as_deref()
        .is_some_and(crate::redaction::is_redacted);
    let redaction_policy_version = (stdout_redacted || stderr_redacted)
        .then(|| crate::redaction::REDACTION_POLICY_VERSION.to_owned());

    // Stdout and stderr hashes are included in the stable ID so two runs with identical
    // timing but different output produce distinct evidence handles.
    let stdout_hash = {
        let mut h = blake3::Hasher::new();
        h.update(redacted_stdout.as_deref().unwrap_or("").as_bytes());
        h.finalize().to_hex().to_string()
    };

    let stderr_hash = {
        let mut h = blake3::Hasher::new();
        h.update(redacted_stderr.as_deref().unwrap_or("").as_bytes());
        h.finalize().to_hex().to_string()
    };

    // Content-addressed on agent, session, execution time, exit code, source, stdout, and stderr.
    // Both path and hash are included so path-only and hash-only inputs don't collide.
    let cmd_id = agent_memory_stable_id(&[
        "node",
        "command_evidence",
        &req.provenance.agent_id,
        &req.provenance.session_id,
        &req.provenance.observed_at,
        &req.executed_at,
        &req.exit_code.to_string(),
        agent_kind,
        &req.evidence_quality,
        &req.source_artifact_path,
        &req.source_artifact_hash,
        req.provenance.source_handle.as_deref().unwrap_or(""),
        &stdout_hash,
        &stderr_hash,
    ]);

    let stdout_handle = redacted_stdout.as_deref().map(output_handle).map(Box::new);
    let stderr_handle = redacted_stderr.as_deref().map(output_handle).map(Box::new);

    let exit_status = if req.exit_code == 0 { "pass" } else { "fail" };

    let cmd_node = GraphRecord::Node {
        id: cmd_id.clone(),
        kind: NodeKind::CommandEvidence,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        repo_relative_path: None,
        span: None,
        name: None,
        language: None,
        symbol_kind: None,
        disambiguator: None,
        visibility: None,
        signature: None,
        doc: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
        repository_identity: None,
        source_snapshot: None,
        text: None,
        superseded_by: None,
        agent_id: Some(req.provenance.agent_id.clone()),
        agent_kind: Some(agent_kind.to_owned()),
        session_id: Some(req.provenance.session_id.clone()),
        observed_at: Some(req.provenance.observed_at.clone()),
        ingested_at: Some(req.provenance.observed_at.clone()),
        confidence: None,
        source_handle: req.provenance.source_handle.clone(),
        redaction_policy_version,
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
        summary: format!(
            "CommandEvidence by {} in {} at {} (exit {})",
            req.provenance.agent_id, req.provenance.session_id, req.executed_at, req.exit_code
        ),
        domain: Some("agent_memory".to_owned()),
        importer_id: None,
        importer_version: None,
        source_artifact_path: Some(req.source_artifact_path.clone()),
        source_artifact_hash: Some(req.source_artifact_hash.clone()),
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
        exit_code: Some(req.exit_code),
        turn_index: None,
        stdout_handle,
        stderr_handle,
        evidence_quality: Some(req.evidence_quality.clone()),
        executed_at: Some(req.executed_at.clone()),
        verification_kind: None,
        status: Some(exit_status.to_owned()),
        review_kind: None,
        review_state: None,
        in_reply_to_id: None,
        author: None,
        diff_hunk_handle: None,
        review_side: None,
        user_context: crate::ir::UserContextFields::empty(),
        producer: Some(evidence_producer()),
    };

    Ok(EvidenceWriteOutcome {
        evidence_handle: cmd_id.clone(),
        record_id: cmd_id,
        records: vec![cmd_node],
    })
}

/// Builds a typed `PatchArtifact` record batch from a provenance-bearing request.
///
/// `PatchArtifact` nodes live in the **artifact** domain (`artifact:v1:` prefix).
/// Both `source_artifact_path` and `source_artifact_hash` must be non-empty.
///
/// # Errors
///
/// Returns a [`ProvenanceError`] when any required field is missing or invalid.
#[allow(clippy::too_many_lines)]
pub fn build_artifact_records(
    req: &ArtifactRequest,
) -> Result<EvidenceWriteOutcome, ProvenanceError> {
    validate_provenance_base(&req.provenance)?;
    validate_agent_kind(&req.provenance)?;
    validate_source_artifact_both(&req.source_artifact_path, &req.source_artifact_hash)?;

    if !PATCH_STATUS_VALUES.contains(&req.patch_status.as_str()) {
        return Err(ProvenanceError::invalid("patch_status"));
    }
    if req.patch_status == "invalid_syntax" && !req.target_files.is_empty() {
        return Err(ProvenanceError::invalid("target_files"));
    }
    if req.patch_status == "invalid_no_base" && req.base_commit.is_some() {
        return Err(ProvenanceError::invalid("base_commit"));
    }
    // Applied statuses imply a known base — unknown_base_reason is only for unverified/invalid.
    if APPLIED_STATUSES.contains(&req.patch_status.as_str())
        && req.base_commit.as_deref().is_none_or(str::is_empty)
    {
        return Err(ProvenanceError::missing("base_commit"));
    }
    if req.validation_summary.is_empty() {
        return Err(ProvenanceError::missing("validation_summary"));
    }

    let agent_kind = effective_agent_kind(&req.provenance);

    let session_node_id = agent_memory_stable_id(&[
        "node",
        "agent_session",
        &req.provenance.agent_id,
        &req.provenance.session_id,
        agent_kind,
        &req.provenance.observed_at,
    ]);

    // Hash the patch bytes for content addressing
    let patch_hash = {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&req.patch_bytes);
        hasher.finalize().to_hex().to_string()
    };

    // Sort target_files canonically for the ID hash and for storage so that two writes with
    // the same file list in different order produce the same ID and identical stored arrays.
    let mut sorted_target_files = req.target_files.clone();
    sorted_target_files.sort_unstable();
    let target_files_hash = {
        let mut h = blake3::Hasher::new();
        for f in &sorted_target_files {
            h.update(f.as_bytes());
            h.update(b"\0");
        }
        h.finalize().to_hex().to_string()
    };

    // Redact validation_summary before the stable ID so the stored value and the ID
    // inputs are consistent — a re-write with different summary text produces a distinct ID.
    let patch_str = String::from_utf8_lossy(&req.patch_bytes).into_owned();
    let redacted_patch = crate::redaction::redact_value(&patch_str);
    let redacted_validation_summary = crate::redaction::redact_value(&req.validation_summary);
    let validation_summary_hash = {
        let mut h = blake3::Hasher::new();
        h.update(redacted_validation_summary.as_bytes());
        h.finalize().to_hex().to_string()
    };

    // ID covers all stored payload-bearing fields so that any correction to source path,
    // validation summary, base_commit, target_files, or source hash produces a distinct
    // record rather than a mismatched-payload conflict on the existing one.
    let art_id = artifact_stable_id(&[
        "node",
        "patch_artifact",
        &req.provenance.agent_id,
        &req.provenance.session_id,
        &req.provenance.observed_at,
        agent_kind,
        req.provenance.source_handle.as_deref().unwrap_or(""),
        &patch_hash,
        &req.patch_status,
        &req.source_artifact_path,
        &req.source_artifact_hash,
        req.base_commit.as_deref().unwrap_or(""),
        &target_files_hash,
        &validation_summary_hash,
    ]);
    let patch_redacted = crate::redaction::is_redacted(&redacted_patch);
    let summary_redacted = crate::redaction::is_redacted(&redacted_validation_summary);
    let redaction_policy_version = (patch_redacted || summary_redacted)
        .then(|| crate::redaction::REDACTION_POLICY_VERSION.to_owned());
    // patch_bytes_size and patch_bytes_hash both reflect the raw bytes (schema contract).
    // The validator requires inline_len <= patch_bytes_size, so redaction must not expand
    // the patch. Reject if redacted size exceeds raw size — no sidecar persistence is
    // available in this writer to handle a handle-only record safely.
    let raw_bytes_size = req.patch_bytes.len() as u64;
    let redacted_bytes_size = redacted_patch.len() as u64;

    if raw_bytes_size.max(redacted_bytes_size) > INLINE_PAYLOAD_CEILING {
        return Err(ProvenanceError::invalid("patch_bytes"));
    }
    if redacted_bytes_size > raw_bytes_size {
        return Err(ProvenanceError::invalid("patch_bytes"));
    }
    let patch_inline = Some(redacted_patch);
    let patch_handle = Box::new(PatchHandle {
        path: format!("patches/{art_id}.patch"),
        inline: patch_inline,
    });

    // Treat Some("") the same as None — the validator requires unknown_base_reason when
    // base_commit is absent or empty.
    let normalized_base = req.base_commit.as_deref().filter(|bc| !bc.is_empty());
    let (base_commit_field, unknown_base_reason) = normalized_base.map_or_else(
        || (None, Some("unknown_base".to_owned())),
        |bc| (Some(bc.to_owned()), None),
    );

    let art_node = GraphRecord::Node {
        id: art_id.clone(),
        kind: NodeKind::PatchArtifact,
        schema_version: ARTIFACT_SCHEMA_VERSION,
        repo_relative_path: None,
        span: None,
        name: None,
        language: None,
        symbol_kind: None,
        disambiguator: None,
        visibility: None,
        signature: None,
        doc: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
        repository_identity: None,
        source_snapshot: None,
        text: None,
        superseded_by: None,
        agent_id: Some(req.provenance.agent_id.clone()),
        agent_kind: Some(agent_kind.to_owned()),
        session_id: Some(req.provenance.session_id.clone()),
        observed_at: Some(req.provenance.observed_at.clone()),
        ingested_at: Some(req.provenance.observed_at.clone()),
        confidence: None,
        source_handle: req.provenance.source_handle.clone(),
        redaction_policy_version,
        valid_time: Some(req.provenance.observed_at.clone()),
        valid_time_source: Some("produced_at".to_owned()),
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
        summary: format!(
            "PatchArtifact by {} targeting {}",
            req.provenance.agent_id,
            sorted_target_files.join(", ")
        ),
        domain: Some("artifact".to_owned()),
        importer_id: None,
        importer_version: None,
        source_artifact_path: Some(req.source_artifact_path.clone()),
        source_artifact_hash: Some(req.source_artifact_hash.clone()),
        patch_status: Some(req.patch_status.clone()),
        base_commit: base_commit_field,
        unknown_base_reason,
        target_files: Some(sorted_target_files),
        patch_bytes_hash: Some(patch_hash),
        patch_bytes_size: Some(raw_bytes_size),
        patch_handle: Some(patch_handle),
        validation_summary: Some(redacted_validation_summary),
        producer_session_id: Some(session_node_id.clone()),
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
        review_kind: None,
        review_state: None,
        in_reply_to_id: None,
        author: None,
        diff_hunk_handle: None,
        review_side: None,
        user_context: crate::ir::UserContextFields::empty(),
        producer: Some(evidence_producer()),
    };

    // The artifact references a specific AgentSession via producer_session_id; include
    // the Agent and AgentSession nodes so a standalone artifact batch is self-contained
    // and passes validate_agent_session_ref on a fresh store.
    let agent_node = build_agent_node(&req.provenance.agent_id, agent_kind);
    let session_node = build_agent_session_node(&req.provenance, agent_kind);

    let session_of_edge = GraphRecord::agent_memory_edge(
        EdgeLabel::SessionOf,
        session_node_id,
        agent_node.id().to_owned(),
        None,
        format!(
            "{} SESSION_OF {}",
            req.provenance.session_id, req.provenance.agent_id
        ),
    );

    Ok(EvidenceWriteOutcome {
        evidence_handle: art_id.clone(),
        record_id: art_id,
        records: vec![agent_node, session_node, session_of_edge, art_node],
    })
}

/// Builds a typed `Verification` record batch from a provenance-bearing request.
///
/// `Verification` nodes live in the **verification** domain (`verification:v1:` prefix).
/// Both `source_artifact_path` and `source_artifact_hash` must be non-empty.
///
/// When `linked_command_evidence_id` is supplied, a `HAS_EVIDENCE` edge links
/// the verification record to the command evidence that produced it.
///
/// # Errors
///
/// Returns a [`ProvenanceError`] when any required field is missing or invalid.
#[allow(clippy::too_many_lines)]
pub fn build_verification_records(
    req: &VerificationRequest,
) -> Result<EvidenceWriteOutcome, ProvenanceError> {
    validate_provenance_base(&req.provenance)?;
    validate_agent_kind(&req.provenance)?;
    validate_source_artifact(&req.source_artifact_path, &req.source_artifact_hash)?;
    validate_verification_evidence_quality(&req.evidence_quality)?;

    if req.executed_at.is_empty() {
        return Err(ProvenanceError::missing("executed_at"));
    }
    if chrono::DateTime::parse_from_rfc3339(&req.executed_at).is_err() {
        return Err(ProvenanceError::invalid("executed_at"));
    }
    if req.status.is_empty() {
        return Err(ProvenanceError::missing("status"));
    }
    if !VALID_VERIFICATION_STATUSES.contains(&req.status.as_str()) {
        return Err(ProvenanceError::invalid("status"));
    }
    if req
        .stdout
        .as_deref()
        .is_some_and(|s| s.len() as u64 > INLINE_PAYLOAD_CEILING)
    {
        return Err(ProvenanceError::invalid("stdout"));
    }

    let agent_kind = effective_agent_kind(&req.provenance);

    // Redact stdout before stable ID so stdout content is part of the identity.
    let redacted_stdout = req.stdout.as_deref().map(crate::redaction::redact_value);
    // Redaction can expand content; reject if the redacted form exceeds the inline ceiling.
    if redacted_stdout
        .as_deref()
        .is_some_and(|s| s.len() as u64 > INLINE_PAYLOAD_CEILING)
    {
        return Err(ProvenanceError::invalid("stdout"));
    }
    let stdout_hash = {
        let mut h = blake3::Hasher::new();
        h.update(redacted_stdout.as_deref().unwrap_or("").as_bytes());
        h.finalize().to_hex().to_string()
    };

    let ver_id = verification_stable_id(&[
        "node",
        "verification",
        &req.provenance.agent_id,
        &req.provenance.session_id,
        &req.provenance.observed_at,
        agent_kind,
        &req.evidence_quality,
        &req.executed_at,
        &req.status,
        &req.verification_kind,
        &req.source_artifact_path,
        &req.source_artifact_hash,
        req.provenance.source_handle.as_deref().unwrap_or(""),
        &stdout_hash,
        req.linked_command_evidence_id.as_deref().unwrap_or(""),
    ]);

    let stdout_redacted = redacted_stdout
        .as_deref()
        .is_some_and(crate::redaction::is_redacted);
    let redaction_policy_version =
        stdout_redacted.then(|| crate::redaction::REDACTION_POLICY_VERSION.to_owned());
    let stdout_handle = redacted_stdout.as_deref().map(output_handle).map(Box::new);

    let ver_node = GraphRecord::Node {
        id: ver_id.clone(),
        kind: NodeKind::Verification,
        schema_version: VERIFICATION_SCHEMA_VERSION,
        repo_relative_path: None,
        span: None,
        name: None,
        language: None,
        symbol_kind: None,
        disambiguator: None,
        visibility: None,
        signature: None,
        doc: None,
        temporal: None,
        semantic_drift: None,
        evidence_links: None,
        author_name: None,
        author_email: None,
        repository_identity: None,
        source_snapshot: None,
        text: None,
        superseded_by: None,
        agent_id: Some(req.provenance.agent_id.to_ascii_lowercase()),
        agent_kind: Some(agent_kind.to_owned()),
        session_id: Some(req.provenance.session_id.to_ascii_lowercase()),
        observed_at: Some(req.provenance.observed_at.clone()),
        ingested_at: Some(req.provenance.observed_at.clone()),
        confidence: None,
        source_handle: req
            .provenance
            .source_handle
            .as_deref()
            .map(str::to_ascii_lowercase),
        redaction_policy_version,
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
        summary: format!(
            "Verification {} by {} in {} at {}",
            req.status,
            req.provenance.agent_id.to_ascii_lowercase(),
            req.provenance.session_id.to_ascii_lowercase(),
            req.executed_at
        ),
        domain: Some("verification".to_owned()),
        importer_id: None,
        importer_version: None,
        source_artifact_path: Some(req.source_artifact_path.to_ascii_lowercase()),
        source_artifact_hash: Some(req.source_artifact_hash.to_ascii_lowercase()),
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
        produced_evidence_id: req.linked_command_evidence_id.clone(),
        started_at: None,
        finished_at: None,
        failure_kind: None,
        exit_code: None,
        turn_index: None,
        stdout_handle,
        stderr_handle: None,
        evidence_quality: Some(req.evidence_quality.clone()),
        executed_at: Some(req.executed_at.clone()),
        verification_kind: Some(req.verification_kind.to_ascii_lowercase()),
        status: Some(req.status.clone()),
        review_kind: None,
        review_state: None,
        in_reply_to_id: None,
        author: None,
        diff_hunk_handle: None,
        review_side: None,
        user_context: crate::ir::UserContextFields::empty(),
        producer: Some(evidence_producer()),
    };

    let mut records = vec![ver_node];

    // If there's a linked command evidence record, validate and emit a HAS_EVIDENCE edge.
    // Empty strings and non-agent_memory IDs are rejected up-front: HAS_EVIDENCE only
    // allows CommandEvidence targets (agent_memory:v1: domain), not CommandRun or other
    // verification-domain nodes, and the daemon rejects mismatched target kinds.
    if let Some(ref cmd_id) = req.linked_command_evidence_id {
        if cmd_id.is_empty() {
            return Err(ProvenanceError::missing("linked_command_evidence_id"));
        }
        if !cmd_id.starts_with("agent_memory:v1:") {
            return Err(ProvenanceError::invalid("linked_command_evidence_id"));
        }
        let has_evidence_edge = GraphRecord::agent_memory_edge(
            EdgeLabel::HasEvidence,
            ver_id.clone(),
            cmd_id.clone(),
            None,
            format!("{ver_id} HAS_EVIDENCE {cmd_id}"),
        );
        records.push(has_evidence_edge);
    }

    Ok(EvidenceWriteOutcome {
        evidence_handle: ver_id.clone(),
        record_id: ver_id,
        records,
    })
}
