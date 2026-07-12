//! Record `schema_version` compatibility checks.
//!
//! This module is the reader-side gate for Egregore record versions. It checks
//! the `(domain, kind, schema_version)` tuple before a JSONL line is treated as a
//! supported `GraphRecord`.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, Domain, EdgeLabel, GraphRecord,
    LOG_SCHEMA_VERSION, PROJECT_SCHEMA_VERSION, SCHEMA_VERSION, SEMANTIC_SCHEMA_VERSION,
    USER_CONTEXT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
};

/// Stable error code for default reader rejection of an unknown record version.
pub const UNKNOWN_SCHEMA_VERSION_CODE: &str = "unknown_schema_version";

/// Domain for the versioned SOC2 control->evidence-class catalog (issue #337).
///
/// The control catalog is a standalone document, not a `GraphRecord`; its tuple
/// is registered here for discoverability but is recognized by
/// [`is_known_control_catalog_schema_version`] rather than
/// [`is_known_record_version`], which only reads `GraphRecord`-derived domains.
pub const CONTROL_CATALOG_DOMAIN: &str = "control_catalog";

/// Kind for the control catalog document (issue #337).
pub const CONTROL_CATALOG_KIND: &str = "ControlCatalog";

/// Current schema version of the control catalog document (issue #337).
pub const CONTROL_CATALOG_SCHEMA_VERSION: u32 = 1;

/// Returns true when this binary knows how to read the control-catalog tuple.
///
/// Kept separate from [`is_known_record_version`] because the control catalog is
/// not a `GraphRecord`; adding a `control_catalog` arm there would corrupt the
/// `GraphRecord` reader path. See [`crate::evidence_pack`] (issue #337).
#[must_use]
pub fn is_known_control_catalog_schema_version(domain: &str, kind: &str, version: u32) -> bool {
    domain == CONTROL_CATALOG_DOMAIN
        && kind == CONTROL_CATALOG_KIND
        && version == CONTROL_CATALOG_SCHEMA_VERSION
}

/// Reader-side schema-version identity scoped per record domain and kind.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct RecordVersion {
    /// Record domain namespace, e.g. `codegraph` or `agent_memory`.
    pub domain: String,
    /// Record kind within the domain, e.g. `Symbol`, `DEFINES`, or `Tombstone`.
    pub kind: String,
    /// Schema version for this `(domain, kind)` pair.
    pub version: u32,
}

impl RecordVersion {
    /// Creates a record-version tuple.
    #[must_use]
    pub fn new(domain: impl Into<String>, kind: impl Into<String>, version: u32) -> Self {
        Self {
            domain: normalize_domain_name(&domain.into()),
            kind: kind.into(),
            version,
        }
    }
}

impl fmt::Display for RecordVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} v{}", self.domain, self.kind, self.version)
    }
}

/// Typed unknown-version diagnostic surfaced by readers and inspect summaries.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnknownSchemaVersion {
    /// Stable error code.
    pub code: String,
    /// Unknown tuple that caused the rejection.
    pub version: RecordVersion,
}

impl UnknownSchemaVersion {
    pub(crate) fn new(version: RecordVersion) -> Self {
        Self {
            code: UNKNOWN_SCHEMA_VERSION_CODE.to_owned(),
            version,
        }
    }
}

impl fmt::Display for UnknownSchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.code, self.version)
    }
}

/// Result of reading one JSONL line with version awareness.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RecordLineRead {
    /// The line contains a supported record and was deserialized.
    Record(Box<GraphRecord>),
    /// The line has a well-formed record-version tuple, but the version is not supported.
    UnknownSchemaVersion(UnknownSchemaVersion),
}

/// Errors produced while checking a JSONL line before record deserialization.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
pub enum RecordReadError {
    /// JSON syntax or concrete record deserialization failed.
    #[error("failed to parse graph JSON: {message}")]
    Json {
        /// Parser error message.
        message: String,
    },
    /// A field needed to form the version tuple was missing.
    #[error("missing required field while checking schema_version: {field}")]
    MissingField {
        /// Missing JSON field path.
        field: String,
    },
    /// A field needed to form the version tuple had the wrong type or range.
    #[error("invalid field while checking schema_version: {field}: {message}")]
    InvalidField {
        /// JSON field path.
        field: String,
        /// Validation detail.
        message: String,
    },
}

/// Reads one JSON object line after checking the record-version tuple.
///
/// Unknown future versions are returned as diagnostics instead of being
/// deserialized into the current `GraphRecord` shape.
///
/// # Errors
///
/// Returns an error when the line is not valid JSON or cannot expose a record
/// type, kind, and `schema_version` tuple.
pub fn read_record_line(line: &str) -> Result<RecordLineRead, RecordReadError> {
    match serde_json::from_str::<GraphRecord>(line) {
        Ok(record) => match validate_record_version(&record) {
            Ok(()) => Ok(RecordLineRead::Record(Box::new(record))),
            Err(unknown) => Ok(RecordLineRead::UnknownSchemaVersion(unknown)),
        },
        Err(record_error) => {
            let value = serde_json::from_str::<Value>(line).map_err(|error| json_error(&error))?;
            let version = record_version_from_value(&value)?;
            if !is_known_record_version(&version) {
                return Ok(RecordLineRead::UnknownSchemaVersion(
                    UnknownSchemaVersion::new(version),
                ));
            }
            Err(json_error(&record_error))
        }
    }
}

/// Returns the reader-side version tuple for an already-deserialized record.
#[must_use]
pub fn record_version(record: &GraphRecord) -> RecordVersion {
    match record {
        GraphRecord::Node {
            id,
            kind,
            schema_version,
            domain,
            ..
        } => {
            let domain = domain
                .as_deref()
                .map(normalize_domain_name)
                .or_else(|| domain_from_record_id(id))
                .unwrap_or_else(|| domain_for_node_kind(kind.as_str()).to_owned());
            RecordVersion::new(domain, kind.as_str(), *schema_version)
        }
        GraphRecord::Edge {
            id,
            label,
            schema_version,
            ..
        } => {
            let domain = domain_from_record_id(id)
                .unwrap_or_else(|| domain_for_edge_label(label.as_str()).to_owned());
            RecordVersion::new(domain, label.as_str(), *schema_version)
        }
        GraphRecord::Tombstone {
            id,
            deleted_id,
            schema_version,
            ..
        } => {
            let domain = domain_from_record_id(id)
                .or_else(|| domain_from_record_id(deleted_id))
                .unwrap_or_else(|| Domain::CodeGraph.as_str().to_owned());
            RecordVersion::new(domain, "Tombstone", *schema_version)
        }
    }
}

/// Validates that an already-deserialized record has a supported version tuple.
///
/// # Errors
///
/// Returns `unknown_schema_version` when the record version is not known to this
/// binary.
pub fn validate_record_version(record: &GraphRecord) -> Result<(), UnknownSchemaVersion> {
    let version = record_version(record);
    if is_known_record_version(&version) {
        Ok(())
    } else {
        Err(UnknownSchemaVersion::new(version))
    }
}

/// Returns true when this binary knows how to read the tuple.
#[must_use]
pub fn is_known_record_version(version: &RecordVersion) -> bool {
    match version.domain.as_str() {
        "codegraph" => (1..=SCHEMA_VERSION).contains(&version.version),
        "agent_memory" => version.version == AGENT_MEMORY_SCHEMA_VERSION,
        "verification" => version.version == VERIFICATION_SCHEMA_VERSION,
        "artifact" => version.version == ARTIFACT_SCHEMA_VERSION,
        "project" => version.version == PROJECT_SCHEMA_VERSION,
        "semantic" => version.version == SEMANTIC_SCHEMA_VERSION,
        "user_context" => version.version == USER_CONTEXT_SCHEMA_VERSION,
        "log" => version.version == LOG_SCHEMA_VERSION,
        _ => false,
    }
}

fn record_version_from_value(value: &Value) -> Result<RecordVersion, RecordReadError> {
    let record_type = required_str(value, "record_type")?;
    let version = required_u32(value, "schema_version")?;
    let id = optional_str(value, "id");
    match record_type {
        "node" => {
            let kind = required_str(value, "kind")?;
            let domain = optional_str(value, "domain")
                .map(normalize_domain_name)
                .or_else(|| id.and_then(domain_from_record_id))
                .unwrap_or_else(|| domain_for_node_kind(kind).to_owned());
            Ok(RecordVersion::new(domain, kind, version))
        }
        "edge" => {
            let label = required_str(value, "label")?;
            let domain = id
                .and_then(domain_from_record_id)
                .unwrap_or_else(|| domain_for_edge_label(label).to_owned());
            Ok(RecordVersion::new(domain, label, version))
        }
        "tombstone" => {
            let domain = id
                .and_then(domain_from_record_id)
                .or_else(|| optional_str(value, "deleted_id").and_then(domain_from_record_id))
                .unwrap_or_else(|| Domain::CodeGraph.as_str().to_owned());
            Ok(RecordVersion::new(domain, "Tombstone", version))
        }
        other => Err(RecordReadError::InvalidField {
            field: "record_type".to_owned(),
            message: format!("unsupported record_type `{other}`"),
        }),
    }
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, RecordReadError> {
    value
        .get(field)
        .ok_or_else(|| missing(field))?
        .as_str()
        .ok_or_else(|| invalid(field, "expected string"))
}

fn optional_str<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn required_u32(value: &Value, field: &str) -> Result<u32, RecordReadError> {
    let raw = value
        .get(field)
        .ok_or_else(|| missing(field))?
        .as_u64()
        .ok_or_else(|| invalid(field, "expected non-negative integer"))?;
    u32::try_from(raw).map_err(|error| invalid(field, format!("out of range: {error}")))
}

fn missing(field: &str) -> RecordReadError {
    RecordReadError::MissingField {
        field: field.to_owned(),
    }
}

fn invalid(field: &str, message: impl Into<String>) -> RecordReadError {
    RecordReadError::InvalidField {
        field: field.to_owned(),
        message: message.into(),
    }
}

fn json_error(error: &serde_json::Error) -> RecordReadError {
    RecordReadError::Json {
        message: error.to_string(),
    }
}

pub(crate) fn normalize_domain_name(domain: &str) -> String {
    match domain {
        "code_graph" => Domain::CodeGraph.as_str().to_owned(),
        other => other.to_owned(),
    }
}

pub(crate) fn domain_from_record_id(id: &str) -> Option<String> {
    let prefix = id.split_once(":v").map_or(id, |(prefix, _)| prefix);
    match prefix {
        "codegraph" | "code_graph" => Some(Domain::CodeGraph.as_str().to_owned()),
        "agent_memory" => Some(Domain::AgentMemory.as_str().to_owned()),
        "verification" => Some(Domain::Verification.as_str().to_owned()),
        "artifact" => Some(Domain::Artifact.as_str().to_owned()),
        "project" => Some(Domain::Project.as_str().to_owned()),
        "semantic" => Some(Domain::Semantic.as_str().to_owned()),
        "user_context" => Some(Domain::UserContext.as_str().to_owned()),
        "log" => Some(Domain::Log.as_str().to_owned()),
        _ => None,
    }
}

pub(crate) fn domain_for_node_kind(kind: &str) -> &'static str {
    match kind {
        "SemanticDrift" | "EmbeddingModel" | "EmbeddingVector" => Domain::Semantic.as_str(),
        "PromoteCandidate" | "PromotionPrompt" | "PromotionDecision" | "Preference"
        | "WorkflowRule" | "NamingDecision" | "Constraint" => Domain::UserContext.as_str(),
        "Task"
        | "AcceptanceCriterion"
        | "ExternalLink"
        | "Product"
        | "Project"
        | "Plan"
        | "GitHubIssue"
        | "PR"
        | "Review"
        | "ExternalIdentity"
        | "ReviewStateTransition"
        | "LocalTask" => Domain::Project.as_str(),
        "Artifact" | "PatchArtifact" => Domain::Artifact.as_str(),
        "Verification" | "CommandEvidence" | "CommandRun" | "TestRun" | "CIStatus"
        | "BenchmarkRun" | "CoverageReport" | "ProofResult" => Domain::Verification.as_str(),
        "Agent" | "AgentSession" | "Observation" | "AgentRun" | "AgentTurn" | "ToolCall"
        | "FileEdit" | "Failure" | "Decision" | "CostUsage" | "Retraction" => {
            Domain::AgentMemory.as_str()
        }
        "LogSource" | "ErrorSignature" | "LogEvent" | "LogOccurrenceBucket" => Domain::Log.as_str(),
        _ => Domain::CodeGraph.as_str(),
    }
}

pub(crate) fn domain_for_edge_label(label: &str) -> &'static str {
    match EdgeLabel::from_relation(label) {
        Some(EdgeLabel::DriftsFrom | EdgeLabel::DriftsPrior | EdgeLabel::MeasuredBy) => {
            Domain::Semantic.as_str()
        }
        Some(
            EdgeLabel::ProposedBy
            | EdgeLabel::PromptedFor
            | EdgeLabel::DecidedOn
            | EdgeLabel::MaterializedAs
            | EdgeLabel::RevokedBy
            | EdgeLabel::ScopedToRepo,
        ) => Domain::UserContext.as_str(),
        Some(
            EdgeLabel::FingerprintedAs
            | EdgeLabel::CapturedFrom
            | EdgeLabel::Aggregates
            | EdgeLabel::FrameResolvesTo
            | EdgeLabel::EmittedDuring,
        ) => Domain::Log.as_str(),
        Some(label) if label.is_codegraph_topology_label() => Domain::CodeGraph.as_str(),
        Some(_) | None => Domain::AgentMemory.as_str(),
    }
}
