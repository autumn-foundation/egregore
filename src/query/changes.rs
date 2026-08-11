use std::collections::{BTreeMap, BTreeSet};

use super::{
    ContextSection, RepositoryIndex, UnresolvedRef, classify_node, evidence_link_triple_handle,
    is_bfs_relay_node, is_cross_domain_label, is_forward_only_label,
};
use crate::ir::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, OutputHandle, PatchHandle, SourceSpan,
    TemporalMetadata,
};

/// Represents a changed file in a commit range.
///
/// Serialization is deliberately bounded to identity/path/span/commit metadata.
/// The backing `GraphRecord` is retained for in-process traversal only and is
/// never emitted, because `File`/`Symbol` summaries from `scan-history` embed
/// normalized source bodies; dumping them would leak whole file/symbol snippets
/// into the response instead of the leads the section promises.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesFileItem<'a> {
    /// The graph record for the file or change (traversal only; not serialized).
    #[serde(skip)]
    pub record: &'a GraphRecord,
    /// Stable record ID of the changed file fact.
    pub record_id: &'a str,
    /// Derived trust class (issue #114). Always present, drawn from the closed
    /// vocabulary in [`super::TrustClass`], so a consumer can require the field
    /// uniformly across every record-shaped row of a `changes` response.
    pub trust: super::TrustClass,
    /// The repository-relative path of the file.
    pub path: &'a str,
    /// Source span of the file fact, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// The Git commit SHA containing this change.
    pub git_commit: &'a str,
}

/// Represents a changed symbol in a commit range.
///
/// Like [`ChangesFileItem`], the raw record is held for traversal but excluded
/// from serialization to keep source bodies out of the response.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesSymbolItem<'a> {
    /// The graph record for the symbol (traversal only; not serialized).
    #[serde(skip)]
    pub record: &'a GraphRecord,
    /// Stable record ID of the changed symbol fact.
    pub record_id: &'a str,
    /// Derived trust class (issue #114). Always present, drawn from the closed
    /// vocabulary in [`super::TrustClass`], so a consumer can require the field
    /// uniformly across every record-shaped row of a `changes` response.
    pub trust: super::TrustClass,
    /// The name of the symbol.
    pub name: &'a str,
    /// The repository-relative path of the symbol definition.
    pub path: &'a str,
    /// Source span of the symbol definition, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// The Git commit SHA containing this change.
    pub git_commit: &'a str,
}

/// Represents a commit in a commit range.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesCommitItem<'a> {
    /// The graph record for the commit.
    pub record: &'a GraphRecord,
    /// Derived trust class (issue #114). Always present, drawn from the closed
    /// vocabulary in [`super::TrustClass`], so a consumer can require the field
    /// uniformly across every record-shaped row of a `changes` response.
    pub trust: super::TrustClass,
    /// The full Git commit SHA.
    pub commit: &'a str,
    /// The commit author timestamp if available.
    pub author_time: Option<&'a str>,
}

/// Represents a tombstone (deleted node marker) associated with a commit range.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesTombstoneItem<'a> {
    /// The graph record for the tombstone.
    pub record: &'a GraphRecord,
    /// Derived trust class (issue #114). Always present, drawn from the closed
    /// vocabulary in [`super::TrustClass`], so a consumer can require the field
    /// uniformly across every record-shaped row of a `changes` response.
    pub trust: super::TrustClass,
    /// The stable ID of the deleted node.
    pub deleted_id: &'a str,
}

/// Represents a semantic drift record in a commit range.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangesDriftItem<'a> {
    /// The graph record for the semantic drift.
    pub record: &'a GraphRecord,
    /// Derived trust class (issue #114). Always present, drawn from the closed
    /// vocabulary in [`super::TrustClass`], so a consumer can require the field
    /// uniformly across every record-shaped row of a `changes` response.
    pub trust: super::TrustClass,
    /// The stable ID of the target node.
    pub target_record_id: &'a str,
    /// The computed drift score.
    pub score: f64,
}

/// Represents a changed code fact that lacks explaining cross-domain evidence.
///
/// Bounded by design: identity, kind, path, and commit only. The node `summary`
/// is deliberately omitted because `File`/`Symbol` summaries from `scan-history`
/// embed normalized source bodies, which must not leak into the response.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct UnexplainedChange<'a> {
    /// The stable ID of the unexplained node.
    pub record_id: &'a str,
    /// Derived trust class (issue #114). Always present, drawn from the closed
    /// vocabulary in [`super::TrustClass`], so a consumer can require the field
    /// uniformly across every record-shaped row of a `changes` response.
    pub trust: super::TrustClass,
    /// The node kind (e.g. "Symbol", "File").
    pub kind: &'a str,
    /// The repository-relative path of the unexplained code fact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<&'a str>,
    /// The Git commit SHA the unexplained snapshot belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<&'a str>,
}

/// One item in the `observations` section of a query response.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextObservation<'a> {
    pub record_id: &'a str,
    pub kind: &'static str,
    /// Derived trust class (issue #114): one of `agent_verified`,
    /// `agent_unverified`, or `agent_contradicted` for an agent-authored claim.
    /// Always present — `domain` says where the record lives, `trust` says how
    /// much weight it has earned. See `crate::query::TrustClass`.
    pub trust: super::TrustClass,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_links: Vec<&'a EvidenceLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

/// One item in the `project_state`, `artifacts`, or `verification_evidence` sections.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextLinkedItem<'a> {
    pub record_id: &'a str,
    pub kind: &'static str,
    /// Derived trust class (issue #114). Always present; drawn from the closed
    /// vocabulary in `crate::query::TrustClass`.
    pub trust: super::TrustClass,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_quality: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_handle: Option<OutputHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_handle: Option<OutputHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_artifact_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_artifact_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunk_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_turn_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_patch_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_handle: Option<PatchHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_bytes_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_bytes_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_files: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_summary: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_base_reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_handle: Option<OutputHandle>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_links: Vec<&'a EvidenceLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_record: Option<Box<Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<&'a str>,
    // GitHub PR-promoted flat Task fields (issue #333; consumed by #334/#338).
    // Plaintext substrate — surfaced verbatim by both the plain and redacted
    // builders (never routed through redaction).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
}

/// Returns a copy of an `OutputHandle` with the `inline` payload stripped,
/// leaving only the bounded hash/size metadata. Used by the redacted change
/// wrappers so captured stdout/stderr bytes never reach the changes response.
fn output_handle_metadata_only(handle: &OutputHandle) -> OutputHandle {
    OutputHandle {
        inline: None,
        hash: handle.hash.clone(),
        bytes: handle.bytes,
    }
}

/// Returns a copy of a `PatchHandle` with the inline patch bytes stripped,
/// leaving only the stored-path handle.
fn patch_handle_metadata_only(handle: &PatchHandle) -> PatchHandle {
    PatchHandle {
        path: handle.path.clone(),
        inline: None,
    }
}

/// Helper function to convert a GraphRecord to ContextObservation.
///
/// `trust` carries the [`super::TrustIndex`]-derived class (issue #114); the
/// index is a required argument so no caller can render a context row without a
/// trust label.
#[must_use]
pub fn context_observation<'a>(
    record: &'a GraphRecord,
    trust: &super::TrustIndex<'_>,
) -> Option<ContextObservation<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        summary,
        text,
        agent_id,
        session_id,
        observed_at,
        confidence,
        failure_kind,
        exit_code,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    Some(ContextObservation {
        record_id: id,
        kind: kind.as_str(),
        trust: trust.classify(record),
        summary: summary.to_owned(),
        text: text.as_deref(),
        provenance_handle,
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        failure_kind: failure_kind.as_deref(),
        exit_code: *exit_code,
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        temporal_status: None,
        superseded_by: None,
        contradicted_by: None,
    })
}

/// Helper function to convert a GraphRecord to ContextObservation with redacted payloads.
#[must_use]
pub fn redacted_context_observation<'a>(
    record: &'a GraphRecord,
    trust: &super::TrustIndex<'_>,
) -> Option<ContextObservation<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        agent_id,
        session_id,
        observed_at,
        confidence,
        failure_kind,
        exit_code,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    let summary = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => format!("{} by {a}:{s}", kind.as_str()),
        (Some(a), None) => format!("{} by {a}", kind.as_str()),
        _ => kind.as_str().to_owned(),
    };
    Some(ContextObservation {
        record_id: id,
        kind: kind.as_str(),
        trust: trust.classify(record),
        summary,
        text: None,
        provenance_handle,
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        failure_kind: failure_kind.as_deref(),
        exit_code: *exit_code,
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        temporal_status: None,
        superseded_by: None,
        contradicted_by: None,
    })
}

/// Helper function to convert a GraphRecord to ContextLinkedItem.
///
/// `trust` carries the [`super::TrustIndex`]-derived class (issue #114).
#[must_use]
pub fn context_linked_item<'a>(
    record: &'a GraphRecord,
    trust: &super::TrustIndex<'_>,
) -> Option<ContextLinkedItem<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        text,
        summary,
        status,
        verification_kind,
        exit_code,
        executed_at,
        evidence_quality,
        stdout_handle,
        stderr_handle,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
        edit_kind,
        before_hash,
        after_hash,
        rename_to,
        hunk_count,
        linked_turn_id,
        linked_patch_id,
        patch_status,
        patch_handle,
        patch_bytes_hash,
        patch_bytes_size,
        target_files,
        validation_summary,
        base_commit,
        unknown_base_reason,
        producer_session_id,
        body_handle,
        evidence_links,
        author,
        head_sha,
        head_ref,
        base_ref,
        merge_commit_sha,
        merged_at,
        draft,
        ..
    } = record
    else {
        return None;
    };
    Some(ContextLinkedItem {
        record_id: id,
        kind: kind.as_str(),
        trust: trust.classify(record),
        summary: summary.to_owned(),
        title: title.as_deref(),
        name: name.as_deref(),
        text: text.as_deref(),
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        exit_code: *exit_code,
        executed_at: executed_at.as_deref(),
        evidence_quality: evidence_quality.as_deref(),
        stdout_handle: stdout_handle.as_deref().cloned(),
        stderr_handle: stderr_handle.as_deref().cloned(),
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        edit_kind: edit_kind.as_deref(),
        before_hash: before_hash.as_deref(),
        after_hash: after_hash.as_deref(),
        rename_to: rename_to.as_deref(),
        hunk_count: *hunk_count,
        linked_turn_id: linked_turn_id.as_deref(),
        linked_patch_id: linked_patch_id.as_deref(),
        patch_status: patch_status.as_deref(),
        patch_handle: patch_handle.as_deref().cloned(),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        patch_bytes_size: *patch_bytes_size,
        target_files: target_files.as_deref(),
        validation_summary: validation_summary.as_deref(),
        base_commit: base_commit.as_deref(),
        unknown_base_reason: unknown_base_reason.as_deref(),
        producer_session_id: producer_session_id.as_deref(),
        body_handle: body_handle.as_deref().cloned(),
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        verification_record: None,
        author: author.as_deref(),
        head_sha: head_sha.as_deref(),
        head_ref: head_ref.as_deref(),
        base_ref: base_ref.as_deref(),
        merge_commit_sha: merge_commit_sha.as_deref(),
        merged_at: merged_at.as_deref(),
        draft: *draft,
    })
}

/// Helper function to convert a GraphRecord to ContextLinkedItem with redacted payloads.
#[must_use]
pub fn redacted_context_linked_item<'a>(
    record: &'a GraphRecord,
    trust: &super::TrustIndex<'_>,
) -> Option<ContextLinkedItem<'a>> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        status,
        verification_kind,
        exit_code,
        executed_at,
        evidence_quality,
        stdout_handle,
        stderr_handle,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
        edit_kind,
        before_hash,
        after_hash,
        rename_to,
        hunk_count,
        linked_turn_id,
        linked_patch_id,
        patch_status,
        patch_handle,
        patch_bytes_hash,
        patch_bytes_size,
        target_files,
        base_commit,
        unknown_base_reason,
        producer_session_id,
        body_handle,
        evidence_links,
        author,
        agent_id,
        session_id,
        head_sha,
        head_ref,
        base_ref,
        merge_commit_sha,
        merged_at,
        draft,
        ..
    } = record
    else {
        return None;
    };
    let display_author = author.as_deref().or(agent_id.as_deref());
    let display_session = session_id.as_deref().or(producer_session_id.as_deref());
    let summary = match (display_author, display_session) {
        (Some(a), Some(s)) => format!("{} by {a}:{s}", kind.as_str()),
        (Some(a), None) => format!("{} by {a}", kind.as_str()),
        _ => kind.as_str().to_owned(),
    };
    Some(ContextLinkedItem {
        record_id: id,
        kind: kind.as_str(),
        trust: trust.classify(record),
        summary,
        title: None,
        name: name.as_deref(),
        text: None,
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        exit_code: *exit_code,
        executed_at: executed_at.as_deref(),
        evidence_quality: evidence_quality.as_deref(),
        stdout_handle: stdout_handle.as_deref().map(output_handle_metadata_only),
        stderr_handle: stderr_handle.as_deref().map(output_handle_metadata_only),
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        edit_kind: edit_kind.as_deref(),
        before_hash: before_hash.as_deref(),
        after_hash: after_hash.as_deref(),
        rename_to: rename_to.as_deref(),
        hunk_count: *hunk_count,
        linked_turn_id: linked_turn_id.as_deref(),
        linked_patch_id: linked_patch_id.as_deref(),
        patch_status: patch_status.as_deref(),
        patch_handle: patch_handle.as_deref().map(patch_handle_metadata_only),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        patch_bytes_size: *patch_bytes_size,
        target_files: target_files.as_deref(),
        validation_summary: None,
        base_commit: base_commit.as_deref(),
        unknown_base_reason: unknown_base_reason.as_deref(),
        producer_session_id: producer_session_id.as_deref(),
        body_handle: body_handle.as_deref().map(output_handle_metadata_only),
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        verification_record: None,
        author: author.as_deref(),
        // Plaintext substrate; surfaced verbatim even in the redacted builder.
        head_sha: head_sha.as_deref(),
        head_ref: head_ref.as_deref(),
        base_ref: base_ref.as_deref(),
        merge_commit_sha: merge_commit_sha.as_deref(),
        merged_at: merged_at.as_deref(),
        draft: *draft,
    })
}

/// Context of changed facts and trust-separated evidence over a commit range.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ChangesContext<'a> {
    /// All files added, modified, or deleted in the commit range.
    pub changed_files: Vec<ChangesFileItem<'a>>,
    /// All syntax symbols added or modified in the commit range.
    pub changed_symbols: Vec<ChangesSymbolItem<'a>>,
    /// Commits within the range.
    pub commits: Vec<ChangesCommitItem<'a>>,
    /// Deleted code graph node markers within the range.
    pub tombstones: Vec<ChangesTombstoneItem<'a>>,
    /// Semantic drift records within the range.
    pub drift_records: Vec<ChangesDriftItem<'a>>,

    /// Subjective agent observations referencing nodes in the range.
    pub observations: Vec<ContextObservation<'a>>,
    /// Task and project management state referencing nodes in the range.
    pub project_state: Vec<ContextLinkedItem<'a>>,
    /// Persistent generated artifacts referencing nodes in the range.
    pub artifacts: Vec<ContextLinkedItem<'a>>,
    /// Verification runs, proof outcomes, and test results referencing nodes in the range.
    pub verification_evidence: Vec<ContextLinkedItem<'a>>,
    /// Changed code facts that do not map to any explaining evidence.
    pub unexplained: Vec<UnexplainedChange<'a>>,
    /// Citations from observations/tasks to absent target records.
    pub unresolved: Vec<UnresolvedRef>,
    /// Corpus this history-analysis lane read (issue #427): `union` over a
    /// scan-history store, `single_snapshot` over a snapshot-less store. The
    /// base/head range is the analysis window; the corpus within it is the
    /// union. The disclosure never changes traversal.
    pub corpus_mode: &'static str,
    /// How the corpus mode was chosen: always `default` for this lane.
    pub corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    pub corpus_disclaimer: String,
}

/// Errors that can occur during commit range query resolution.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum ChangesError {
    /// The specified commit prefix could not be resolved to any commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// The specified commit prefix was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// The range is reversed (base is a descendant of head).
    ReversedRange {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// There is no ancestor path between base and head.
    NoPath {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// The store history is empty (no commits present).
    EmptyHistory,
}

/// Query context of changed facts and trust-separated evidence over a commit range.
///
/// Walks the commit topology from `head_prefix` back to `base_prefix`, identifies all
/// code facts changed in that range, and aggregates related cross-domain evidence up to 3 hops.
///
/// # Errors
///
/// Returns a [`ChangesError`] if a commit is missing, ambiguous, or the range is reversed or unconnected.
#[allow(clippy::missing_panics_doc)]
pub fn changes_context<'a>(
    records: &'a [GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    repo_scope: Option<&str>,
) -> Result<ChangesContext<'a>, ChangesError> {
    // 0. Check for empty history
    let has_any_commits = records
        .iter()
        .any(|r| matches!(r.node_kind_name(), Some("Commit")));
    if !has_any_commits {
        return Err(ChangesError::EmptyHistory);
    }

    // Resolve repository ownership only when a scope is requested. In a shared
    // store two repositories can carry the same commit SHA; scoping commit
    // resolution and code-fact selection by owning repository keeps one repo's
    // range from mixing in another's files/symbols/evidence. Cross-domain
    // evidence (agent memory, verification) is intentionally not repo-owned in
    // the containment topology, so the BFS that fans out from in-scope seeds is
    // left unscoped — only the source-fact and commit selection is gated.
    let repo_index = repo_scope.map(|_| RepositoryIndex::build(records));
    let in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };

    // 1. Resolve commit prefixes
    let resolve_prefix = |prefix: &str| -> Result<&'a str, ChangesError> {
        let mut matches = Vec::new();
        for r in records {
            if let GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                ..
            } = r
            {
                if sha.to_lowercase().starts_with(&prefix.to_lowercase()) && in_scope(r.id()) {
                    matches.push(sha.as_str());
                }
            }
        }
        matches.sort_unstable();
        matches.dedup();

        if matches.is_empty() {
            return Err(ChangesError::MissingCommit {
                commit_prefix: prefix.to_owned(),
            });
        }
        if matches.len() > 1 {
            let string_matches = matches.iter().map(|s| (*s).to_owned()).collect();
            return Err(ChangesError::AmbiguousCommitPrefix {
                commit_prefix: prefix.to_owned(),
                matches: string_matches,
            });
        }
        Ok(matches.into_iter().next().unwrap())
    };

    let base_sha = resolve_prefix(base_prefix)?;
    let head_sha = resolve_prefix(head_prefix)?;

    // 2. Build parent map
    let mut parent_map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut by_id: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut tombstoned_ids = BTreeSet::new();
    let mut has_any_temporal_version = BTreeSet::new();

    for r in records {
        by_id.insert(r.id(), r);
        if let GraphRecord::Tombstone { deleted_id, .. } = r {
            tombstoned_ids.insert(deleted_id.as_str());
        }
        match r {
            GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            }
            | GraphRecord::Edge {
                id,
                temporal: Some(_),
                ..
            } => {
                has_any_temporal_version.insert(id.as_str());
            }
            _ => {}
        }
    }

    // Commits specify parents via temporal.git_parent_commits or ParentOf edges.
    // When a repository scope is active, only that repository's commit nodes
    // contribute to the topology so a same-SHA commit owned by another repo
    // cannot bleed into the range.
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = r
        {
            if in_scope(r.id()) {
                let entry = parent_map.entry(sha.as_str()).or_default();
                for parent in &t.git_parent_commits {
                    entry.push(parent.as_str());
                }
            }
        }
        if let GraphRecord::Edge {
            label: EdgeLabel::ParentOf,
            source,
            target,
            ..
        } = r
        {
            if !in_scope(source.as_str()) || !in_scope(target.as_str()) {
                continue;
            }
            if let (Some(parent_node), Some(child_node)) =
                (by_id.get(source.as_str()), by_id.get(target.as_str()))
            {
                if let (
                    GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(psha),
                        ..
                    },
                    GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(csha),
                        ..
                    },
                ) = (parent_node, child_node)
                {
                    let entry = parent_map.entry(csha.as_str()).or_default();
                    entry.push(psha.as_str());
                }
            }
        }
    }

    for parents in parent_map.values_mut() {
        parents.sort_unstable();
        parents.dedup();
    }

    // 3. Compute reachable sets
    let get_reachable = |start_sha: &'a str| -> BTreeSet<&'a str> {
        let mut reachable = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut queue = vec![start_sha];
        while let Some(current) = queue.pop() {
            if !visited.insert(current) {
                continue;
            }
            reachable.insert(current);
            if let Some(parents) = parent_map.get(current) {
                for parent in parents {
                    if !visited.contains(*parent) {
                        queue.push(*parent);
                    }
                }
            }
        }
        reachable
    };

    let reachable_head = get_reachable(head_sha);
    let reachable_base = get_reachable(base_sha);

    // 4. Validate range ancestry
    if !reachable_head.contains(base_sha) {
        if reachable_base.contains(head_sha) {
            return Err(ChangesError::ReversedRange {
                base: base_prefix.to_owned(),
                head: head_prefix.to_owned(),
            });
        }
        return Err(ChangesError::NoPath {
            base: base_prefix.to_owned(),
            head: head_prefix.to_owned(),
        });
    }

    let range_commit_shas: BTreeSet<&str> = reachable_head
        .difference(&reachable_base)
        .copied()
        .collect();

    // 5. Gather code facts in the range.
    //
    // History graphs emit a `File`/`Symbol` snapshot for every path in every
    // commit but only attach a `CHANGED_IN` edge when that path actually changed
    // in the commit. The stable source ID of a `CHANGED_IN` edge is reused by
    // every temporal snapshot of the same path/symbol, so seeding from the edge's
    // source ID alone (or gating on a global "any CHANGED_IN edge exists" flag)
    // would report unchanged base/other-commit snapshots as changed and would
    // disable the commit-membership fallback for ranges that legitimately lack
    // those edges.
    //
    // To stay precise we record the exact `(stable source id, commit sha)` pairs
    // that changed, plus the set of range commits that actually carry CHANGED_IN
    // coverage. A snapshot then counts as changed only when its own
    // `(id, git_commit)` pair is marked, and the commit-membership fallback is
    // applied per-commit for range commits that have no CHANGED_IN edges.
    let mut range_target_commit: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        match r {
            GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                ..
            } if range_commit_shas.contains(sha.as_str()) && in_scope(r.id()) => {
                range_target_commit.insert(r.id(), sha.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::Change,
                temporal: Some(t),
                ..
            } if range_commit_shas.contains(t.git_commit.as_str()) && in_scope(r.id()) => {
                range_target_commit.insert(r.id(), t.git_commit.as_str());
            }
            _ => {}
        }
    }

    let mut changed_pairs: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut commits_with_changed_in: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label: EdgeLabel::ChangedIn,
            source,
            target,
            ..
        } = r
        {
            let Some(commit) = range_target_commit.get(target.as_str()) else {
                continue;
            };
            // Coverage is recorded from the presence of an in-range CHANGED_IN
            // edge even when that edge is tombstoned: the history format uses
            // CHANGED_IN, so the legacy commit-membership fallback must stay off.
            // Recording coverage only for live edges would let a retracted edge
            // that is a commit's sole marker re-enable the fallback and report the
            // very snapshot whose CHANGED_IN was revoked as changed.
            commits_with_changed_in.insert(*commit);
            // A CHANGED_IN edge retracted by an active tombstone no longer marks
            // its fact as changed (the has_any_temporal_version exception keeps
            // versioned edges live), so it is excluded from changed_pairs while
            // still counting as coverage above.
            if tombstoned_ids.contains(edge_id.as_str())
                && !has_any_temporal_version.contains(edge_id.as_str())
            {
                continue;
            }
            changed_pairs.insert((source.as_str(), *commit));
        }
    }

    // Coverage is decided at the range level, not per commit. If any commit in
    // the range carries CHANGED_IN edges, the history format records them, so we
    // trust them exactly: a snapshot counts as changed only when its own
    // `(id, commit)` pair is marked. A commit that merely re-emits snapshots
    // without a CHANGED_IN edge (e.g. a doc/config-only commit) then contributes
    // nothing here — only its explicit `Change` records surface via pass 2. The
    // commit-membership fallback applies only when the range carries no
    // CHANGED_IN edges at all (history written without them); this also keeps the
    // fallback scoped to the queried range rather than disabled globally by a
    // single CHANGED_IN edge elsewhere in a shared store.
    let range_uses_changed_in = !commits_with_changed_in.is_empty();
    let is_changed_node = |r: &GraphRecord, t: &TemporalMetadata| -> bool {
        if range_uses_changed_in {
            changed_pairs.contains(&(r.id(), t.git_commit.as_str()))
        } else {
            range_commit_shas.contains(t.git_commit.as_str())
        }
    };

    // Symbol-level change gate. `scan-history` adds a `CHANGED_IN` edge for every
    // `Symbol` snapshot in a touched file, not only the symbol whose body the
    // commit actually edited (see `src/history.rs`). Reporting all of them would
    // send agents to inspect unchanged code, contradicting the section contract.
    // We index each symbol snapshot's body (its summary, which the scanner builds
    // deterministically from source bytes with no commit-specific content) by
    // `(stable id, commit)` and treat a snapshot as a real change only when it
    // differs from — or has no — parent-commit snapshot of the same symbol id.
    // The check is conservative: when the parent topology or parent snapshot is
    // unavailable we cannot prove the body is unchanged, so we keep the row.
    // One trust index for every record-shaped row this lane returns (issue
    // #114), built over the same slice the rows are drawn from.
    let trust = super::TrustIndex::build(records);

    let mut symbol_snapshot_bodies: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Symbol,
            temporal: Some(t),
            summary,
            ..
        } = r
        {
            symbol_snapshot_bodies.insert((r.id(), t.git_commit.as_str()), summary.as_str());
        }
    }
    let symbol_body_changed = |id: &str, commit: &str, summary: &str| -> bool {
        let Some(parents) = parent_map.get(commit) else {
            return true;
        };
        if parents.is_empty() {
            return true;
        }
        let mut saw_parent_snapshot = false;
        for parent in parents {
            if let Some(parent_body) = symbol_snapshot_bodies.get(&(id, *parent)) {
                saw_parent_snapshot = true;
                if *parent_body != summary {
                    return true;
                }
            }
        }
        // Every parent snapshot we could find matched this body: unchanged. If we
        // found none, we cannot prove it unchanged, so report it.
        !saw_parent_snapshot
    };

    let mut changed_files = Vec::new();
    let mut changed_symbols = Vec::new();
    let mut commits = Vec::new();
    let mut drift_records = Vec::new();
    let mut changed_paths = BTreeSet::new();
    let mut added_file_commits = BTreeSet::new();

    // Pass 1: commits, file nodes, symbol nodes, drift records
    for r in records {
        match r {
            GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                temporal,
                ..
            } if range_commit_shas.contains(sha.as_str()) && in_scope(r.id()) => {
                commits.push(ChangesCommitItem {
                    record: r,
                    trust: trust.classify(r),
                    commit: sha,
                    author_time: temporal.as_ref().and_then(|t| t.author_time.as_deref()),
                });
            }
            GraphRecord::Node {
                kind: NodeKind::File,
                repo_relative_path: Some(path),
                span,
                temporal: Some(t),
                ..
            } if is_changed_node(r, t) && in_scope(r.id()) => {
                changed_files.push(ChangesFileItem {
                    record: r,
                    trust: trust.classify(r),
                    record_id: r.id(),
                    path,
                    span: *span,
                    git_commit: &t.git_commit,
                });
                added_file_commits.insert((path.as_str(), t.git_commit.as_str()));
                changed_paths.insert(path.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                name: Some(sym_name),
                repo_relative_path: Some(path),
                span,
                temporal: Some(t),
                summary,
                ..
            } if is_changed_node(r, t)
                && in_scope(r.id())
                && symbol_body_changed(r.id(), t.git_commit.as_str(), summary) =>
            {
                changed_symbols.push(ChangesSymbolItem {
                    record: r,
                    trust: trust.classify(r),
                    record_id: r.id(),
                    name: sym_name,
                    path,
                    span: *span,
                    git_commit: &t.git_commit,
                });
            }
            GraphRecord::Node {
                kind: NodeKind::SemanticDrift,
                temporal,
                semantic_drift: Some(drift),
                ..
            } => {
                let in_range = temporal
                    .as_ref()
                    .is_some_and(|t| range_commit_shas.contains(t.git_commit.as_str()))
                    || range_commit_shas.contains(drift.after_git_commit.as_str());
                if in_range && in_scope(r.id()) {
                    drift_records.push(ChangesDriftItem {
                        record: r,
                        trust: trust.classify(r),
                        target_record_id: &drift.target_record_id,
                        score: drift.score,
                    });
                }
            }
            _ => {}
        }
    }

    // Pass 2: Change nodes. A `Change` is added to `changed_files` only when a
    // File snapshot did not already cover its `(path, commit)`, but its stable id
    // is always recorded as an evidence seed: `EXPLAINS_CHANGE` citations target
    // the `Change`/`Commit`, so an observation explaining a normal Rust
    // modification (which also has a File snapshot) must still be discovered.
    let mut change_seed_ids = BTreeSet::new();
    // `(path, commit)` → `Change` record id, so the `unexplained` check can carry
    // evidence that targets the per-commit `Change` (which the output BFS already
    // surfaces because the change is seeded) through to the `File`/`Symbol` fact
    // for the same `(path, commit)`. Without this a normal Rust modification whose
    // explaining observation cites the de-duped `Change` would appear with its
    // evidence and still be listed as unexplained.
    let mut change_id_by_path_commit: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    // Repo-relative paths deleted in the range. `scan-history` emits no
    // File/Symbol snapshot at a deletion commit and no CHANGED_IN edge for the
    // deleted path, so evidence that cites the deleted code id from an earlier
    // snapshot needs a bridge into the evidence traversal (built below).
    let mut deletion_paths: BTreeSet<&str> = BTreeSet::new();
    // Commits where a deleted path was last live (the deletion commit's parents).
    // Evidence explaining a deletion is normally anchored to the file's prior
    // live commit, which is out of the queried range, so the in-range anchor
    // filter must additionally admit these commits for deletion-bridge targets.
    let mut deletion_live_commits: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Change,
            repo_relative_path: Some(path),
            name,
            span,
            temporal: Some(t),
            ..
        } = r
        {
            if range_commit_shas.contains(t.git_commit.as_str()) && in_scope(r.id()) {
                change_seed_ids.insert(r.id());
                change_id_by_path_commit.insert((path.as_str(), t.git_commit.as_str()), r.id());
                // The Change `name` is "<git status> <path>"; a leading "D"
                // marks a deletion whose prior code ids must be bridged for
                // evidence discovery.
                if name.as_deref().and_then(|n| n.split_whitespace().next()) == Some("D") {
                    deletion_paths.insert(path.as_str());
                    if let Some(parents) = parent_map.get(t.git_commit.as_str()) {
                        deletion_live_commits.extend(parents.iter().copied());
                    }
                }
                if added_file_commits.insert((path.as_str(), t.git_commit.as_str())) {
                    changed_files.push(ChangesFileItem {
                        record: r,
                        trust: trust.classify(r),
                        record_id: r.id(),
                        path,
                        span: *span,
                        git_commit: &t.git_commit,
                    });
                    changed_paths.insert(path.as_str());
                }
            }
        }
    }

    let mut tombstones = Vec::new();
    for r in records {
        if let GraphRecord::Tombstone {
            deleted_id,
            summary,
            ..
        } = r
        {
            // Tombstones carry no commit and cannot be temporally scoped to the
            // range (tracked separately); but when the deleted node's owning
            // repository is known it must still match the requested scope so a
            // sibling repo's deletions never appear.
            let owner_ok = repo_scope.is_none_or(|scope| {
                repo_index
                    .as_ref()
                    .and_then(|index| index.owner_of(deleted_id))
                    .is_none_or(|owner| owner == scope)
            });
            if owner_ok
                && changed_paths
                    .iter()
                    .any(|path| summary.contains(path) || deleted_id.contains(path))
            {
                tombstones.push(ChangesTombstoneItem {
                    record: r,
                    trust: trust.classify(r),
                    deleted_id,
                });
            }
        }
    }

    // 6. Gather cross-domain evidence
    let mut seed_ids = BTreeSet::new();
    for item in &changed_files {
        seed_ids.insert(item.record.id());
    }
    for item in &changed_symbols {
        seed_ids.insert(item.record.id());
    }
    for item in &commits {
        seed_ids.insert(item.record.id());
    }
    for item in &drift_records {
        seed_ids.insert(item.record.id());
    }
    for item in &tombstones {
        seed_ids.insert(item.deleted_id);
    }
    // Change nodes are not output facts, but seeding them lets the BFS reach
    // EXPLAINS_CHANGE evidence that targets the change rather than the File/Symbol.
    seed_ids.extend(change_seed_ids.iter().copied());

    // Bridge prior path-backed `File`/`Symbol` ids for deleted paths into the
    // evidence traversal. A deletion has no in-range code snapshot, but earlier
    // snapshots (and the observations/verification that cite their stable ids)
    // remain in the store, so without this an explained deletion would surface no
    // evidence. These ids seed only the evidence BFS — never `seed_ids` — so they
    // are not reported as changed facts and never appear in `unexplained`. Stale
    // out-of-range citations to the reused id are still excluded by
    // `direct_evidence_link_in_range`.
    let mut deletion_bridge_ids: BTreeSet<&str> = BTreeSet::new();
    if !deletion_paths.is_empty() {
        for r in records {
            if let GraphRecord::Node {
                kind: NodeKind::File | NodeKind::Symbol,
                repo_relative_path: Some(path),
                ..
            } = r
            {
                if deletion_paths.contains(path.as_str()) && in_scope(r.id()) {
                    deletion_bridge_ids.insert(r.id());
                }
            }
        }
    }

    let mut observations = BTreeSet::new();
    let mut project_state = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    let mut verification_evidence = BTreeSet::new();

    let mut present_ids = BTreeSet::new();
    for r in records {
        present_ids.insert(r.id());
    }
    let mut unresolved = Vec::new();

    let mut visited = seed_ids.clone();
    let mut frontier = seed_ids.clone();
    // Deletion bridges expand the evidence traversal without being reported facts.
    for id in &deletion_bridge_ids {
        if visited.insert(id) {
            frontier.insert(id);
        }
    }
    let mut temporal_evidence_scanned = BTreeSet::new();
    let mut evidence_links_scanned = BTreeSet::new();

    let mut edges_from: BTreeMap<&str, Vec<(EdgeLabel, &str)>> = BTreeMap::new();
    let mut edges_to: BTreeMap<&str, Vec<(EdgeLabel, &str)>> = BTreeMap::new();
    let mut evidence_links_to: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    // Admits a direct evidence link for indexing/traversal. Anchored links must
    // cite an in-range commit, except that evidence for a bridged deletion target
    // may instead be anchored to the deleted fact's prior live commit (which is
    // out of range by definition). Unanchored links are always admitted.
    let direct_link_admissible = |link: &EvidenceLink| -> bool {
        link.target_git_commit
            .as_deref()
            .or(link.as_of_commit.as_deref())
            .is_none_or(|commit| {
                range_commit_shas.contains(commit)
                    || (link
                        .target_record_id
                        .as_deref()
                        .is_some_and(|t| deletion_bridge_ids.contains(t))
                        && deletion_live_commits.contains(commit))
            })
    };

    for r in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label,
            source,
            target,
            temporal,
            ..
        } = r
        {
            if tombstoned_ids.contains(edge_id.as_str())
                && !has_any_temporal_version.contains(edge_id.as_str())
            {
                continue;
            }
            // A materialized cross-domain evidence edge (e.g. EXPLAINS_CHANGE,
            // TOUCHED_FILE) carries its commit anchor on temporal metadata. Since
            // history reuses the same stable File/Symbol id across commits, an
            // edge anchored outside the queried range is stale context, so skip it
            // — mirroring `direct_link_admissible` for `EvidenceLink`s. Deletion
            // evidence anchored to the deleted fact's prior live commit (where one
            // endpoint is a deletion-bridge id) is still admitted. Unanchored
            // edges and structural (non-cross-domain) edges are unaffected.
            if is_cross_domain_label(*label) {
                if let Some(t) = temporal {
                    let commit = t.git_commit.as_str();
                    let deletion_ok = (deletion_bridge_ids.contains(source.as_str())
                        || deletion_bridge_ids.contains(target.as_str()))
                        && deletion_live_commits.contains(commit);
                    if !range_commit_shas.contains(commit) && !deletion_ok {
                        continue;
                    }
                }
            }
            edges_from
                .entry(source.as_str())
                .or_default()
                .push((*label, target.as_str()));
            edges_to
                .entry(target.as_str())
                .or_default()
                .push((*label, source.as_str()));
        }
        if let GraphRecord::Node {
            id,
            evidence_links: Some(links),
            ..
        } = r
        {
            for link in links {
                if let Some(tid) = &link.target_record_id {
                    if !direct_link_admissible(link) {
                        continue;
                    }
                    evidence_links_to
                        .entry(tid.as_str())
                        .or_default()
                        .push(id.as_str());
                }
            }
        }
    }

    let classify_and_insert_change = |record_id: &'a str,
                                      observations: &mut BTreeSet<&'a str>,
                                      project_state: &mut BTreeSet<&'a str>,
                                      artifacts: &mut BTreeSet<&'a str>,
                                      verification_evidence: &mut BTreeSet<&'a str>|
     -> bool {
        if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
            return false;
        }
        let Some(rec) = by_id.get(record_id) else {
            return false;
        };
        let GraphRecord::Node { kind, .. } = rec else {
            return false;
        };
        match classify_node(*kind) {
            Some(ContextSection::Observation) => {
                observations.insert(record_id);
                true
            }
            Some(ContextSection::ProjectState) => {
                project_state.insert(record_id);
                true
            }
            Some(ContextSection::Artifact) => {
                artifacts.insert(record_id);
                true
            }
            Some(ContextSection::VerificationEvidence) => {
                verification_evidence.insert(record_id);
                true
            }
            _ => false,
        }
    };

    // BFS loop - 3 hops
    for _hop in 0..3 {
        let mut next_frontier = Vec::new();
        for current in frontier {
            if let Some(outs) = edges_from.get(current) {
                for (label, target) in outs {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if visited.insert(*target) {
                        let was_classified = classify_and_insert_change(
                            target,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        if was_classified
                            || is_bfs_relay_node(
                                target,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(*target);
                        }
                    }
                }
            }

            if let Some(ins) = edges_to.get(current) {
                for (label, source) in ins {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if is_forward_only_label(*label) {
                        continue;
                    }
                    if visited.insert(*source) {
                        let was_classified = classify_and_insert_change(
                            source,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        if was_classified
                            || is_bfs_relay_node(
                                source,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(*source);
                        }
                    }
                }
            }

            if let Some(GraphRecord::Node {
                id: node_id,
                evidence_links: Some(links),
                temporal,
                ..
            }) = by_id.get(current)
            {
                let already_scanned = temporal.as_ref().map_or_else(
                    || !evidence_links_scanned.insert(node_id.as_str()),
                    |t| {
                        let key = format!("{}@{}", node_id, t.git_commit);
                        !temporal_evidence_scanned.insert(key)
                    },
                );
                if !already_scanned {
                    for link in links {
                        if let Some(target_id) = &link.target_record_id {
                            if !direct_link_admissible(link) {
                                continue;
                            }
                            if present_ids.contains(target_id.as_str()) {
                                if visited.insert(target_id.as_str()) {
                                    let was_classified = classify_and_insert_change(
                                        target_id.as_str(),
                                        &mut observations,
                                        &mut project_state,
                                        &mut artifacts,
                                        &mut verification_evidence,
                                    );
                                    if was_classified
                                        || is_bfs_relay_node(
                                            target_id.as_str(),
                                            &by_id,
                                            &tombstoned_ids,
                                            &has_any_temporal_version,
                                        )
                                    {
                                        next_frontier.push(target_id.as_str());
                                    }
                                }
                            } else {
                                unresolved.push(UnresolvedRef {
                                    source_record_id: (*node_id).clone(),
                                    target_handle: target_id.clone(),
                                    relation: link.relation.clone(),
                                    target_domain: link.target_domain.clone(),
                                });
                            }
                        } else if let Some(handle) = evidence_link_triple_handle(link) {
                            // A triple-only citation anchored to an out-of-range
                            // commit is stale context, not an explanation of an
                            // in-range change — mirror the seed-path pass's anchor
                            // check so a stale citation reached here via the BFS is
                            // not re-emitted as unresolved for the new range.
                            let anchor = link
                                .target_git_commit
                                .as_deref()
                                .or(link.as_of_commit.as_deref());
                            if anchor.is_some_and(|commit| !range_commit_shas.contains(commit)) {
                                continue;
                            }
                            unresolved.push(UnresolvedRef {
                                source_record_id: (*node_id).clone(),
                                target_handle: handle,
                                relation: link.relation.clone(),
                                target_domain: link.target_domain.clone(),
                            });
                        }
                    }
                }
            }

            if let Some(sources) = evidence_links_to.get(current) {
                for source in sources {
                    if visited.insert(*source) {
                        let was_classified = classify_and_insert_change(
                            source,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        // A relay such as a ToolCall/AgentTurn can cite a changed
                        // File/Symbol via its own evidence_links; expand it so its
                        // forward PRODUCED_EVIDENCE edges still reach the
                        // CommandRun/TestRun it produced.
                        if was_classified
                            || is_bfs_relay_node(
                                source,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(*source);
                        }
                    }
                }
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier.into_iter().collect();
    }

    let mut output_observations = Vec::new();
    for id in observations {
        if let Some(rec) = by_id.get(id) {
            if let Some(obs) = redacted_context_observation(rec, &trust) {
                output_observations.push(obs);
            }
        }
    }
    let mut output_project_state = Vec::new();
    for id in project_state {
        if let Some(rec) = by_id.get(id) {
            if let Some(item) = redacted_context_linked_item(rec, &trust) {
                output_project_state.push(item);
            }
        }
    }
    let mut output_artifacts = Vec::new();
    for id in artifacts {
        if let Some(rec) = by_id.get(id) {
            if let Some(item) = redacted_context_linked_item(rec, &trust) {
                output_artifacts.push(item);
            }
        }
    }
    let mut output_verification_evidence = Vec::new();
    for id in verification_evidence {
        if let Some(rec) = by_id.get(id) {
            if let Some(item) = redacted_context_linked_item(rec, &trust) {
                output_verification_evidence.push(item);
            }
        }
    }

    // Surface triple-only evidence links (path/span/commit citations with no
    // resolved `target_record_id`) whose cited path matches a changed file or
    // symbol. The BFS above only reaches such citations when their source node is
    // otherwise connected by an edge or a resolved link, so an imported
    // observation that cites a changed path purely by triple — e.g. before
    // `link-evidence` materializes the edge — would be invisible and the change
    // would be reported unexplained even though the graph holds a citation to it.
    // Mirrors `symbol_context`'s seed-path gating.
    let changed_seed_paths: BTreeSet<&str> = changed_files
        .iter()
        .map(|f| f.path)
        .chain(changed_symbols.iter().map(|s| s.path))
        .collect();
    if !changed_seed_paths.is_empty() {
        for r in records {
            let GraphRecord::Node {
                id,
                evidence_links: Some(links),
                ..
            } = r
            else {
                continue;
            };
            if tombstoned_ids.contains(id.as_str())
                && !has_any_temporal_version.contains(id.as_str())
            {
                continue;
            }
            // Under a repository scope, a sibling repo can carry the same path and
            // the same commit SHA. Surface a triple-only citation only when its
            // source node is not owned by a different repository, otherwise a
            // sibling repo's observation would be attached as context for the
            // selected repo's change purely by path/commit match. Records with no
            // owning repository (cross-domain agent memory) are still surfaced.
            let owner_ok = repo_scope.is_none_or(|scope| {
                repo_index
                    .as_ref()
                    .and_then(|index| index.owner_of(id))
                    .is_none_or(|owner| owner == scope)
            });
            if !owner_ok {
                continue;
            }
            for link in links {
                if link.target_record_id.is_some() {
                    continue;
                }
                if !link
                    .target_repo_relative_path
                    .as_deref()
                    .is_some_and(|p| changed_seed_paths.contains(p))
                {
                    continue;
                }
                // If the citation is anchored to a specific commit, only surface
                // it when that commit is within the queried range. A citation to
                // an out-of-range version of the same path is stale context, not
                // an explanation of a change in this range. Exception: a deletion's
                // explaining citation is anchored to the deleted file's prior live
                // commit (out of range); admit it when the cited path is an
                // in-range deletion path, mirroring the direct-link deletion bridge.
                let anchor = link
                    .target_git_commit
                    .as_deref()
                    .or(link.as_of_commit.as_deref());
                if let Some(commit) = anchor {
                    let deletion_ok = link
                        .target_repo_relative_path
                        .as_deref()
                        .is_some_and(|p| deletion_paths.contains(p))
                        && deletion_live_commits.contains(commit);
                    if !range_commit_shas.contains(commit) && !deletion_ok {
                        continue;
                    }
                }
                let Some(handle) = evidence_link_triple_handle(link) else {
                    continue;
                };
                unresolved.push(UnresolvedRef {
                    source_record_id: id.clone(),
                    target_handle: handle,
                    relation: link.relation.clone(),
                    target_domain: link.target_domain.clone(),
                });
            }
        }
    }

    // `commit sha` → `Commit` record id: the second proxy through which an
    // explanation can reach a code fact. An `EXPLAINS_CHANGE` link may target the
    // seeded `Commit` rather than the per-path `Change`.
    let commit_id_by_sha: BTreeMap<&str, &str> =
        commits.iter().map(|c| (c.commit, c.record.id())).collect();

    let linked = |id: &'a str| -> bool {
        is_linked_to_evidence(
            id,
            &by_id,
            &edges_from,
            &edges_to,
            &evidence_links_to,
            &tombstoned_ids,
            &has_any_temporal_version,
            &range_commit_shas,
        )
    };

    let mut unexplained = Vec::new();
    for seed_id in &seed_ids {
        if linked(seed_id) {
            continue;
        }
        let Some(seed) = by_id.get(seed_id).copied() else {
            continue;
        };
        let GraphRecord::Node {
            kind,
            repo_relative_path,
            temporal,
            ..
        } = seed
        else {
            continue;
        };
        if !matches!(kind, NodeKind::File | NodeKind::Symbol) {
            continue;
        }
        // The output BFS seeds the per-commit `Change` and `Commit` for this
        // `(path, commit)`, so an observation that explains the change via either
        // is already emitted. Mirror that here: a code fact whose `Change` or
        // `Commit` proxy carries the evidence is explained, even when no
        // cross-domain edge touches the `File`/`Symbol` node directly. Without
        // this, a normal modification would appear with its explanation and still
        // be reported unexplained.
        let explained_via_proxy = temporal.as_ref().is_some_and(|t| {
            let commit = t.git_commit.as_str();
            let change_proxy = repo_relative_path
                .as_deref()
                .and_then(|p| change_id_by_path_commit.get(&(p, commit)).copied());
            let commit_proxy = commit_id_by_sha.get(commit).copied();
            change_proxy.into_iter().chain(commit_proxy).any(&linked)
        });
        if explained_via_proxy {
            continue;
        }
        unexplained.push(UnexplainedChange {
            record_id: seed_id,
            trust: trust.classify(seed),
            kind: kind.as_str(),
            path: repo_relative_path.as_deref(),
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        });
    }

    changed_files.sort_by(|a, b| {
        a.path
            .cmp(b.path)
            .then_with(|| a.git_commit.cmp(b.git_commit))
            .then_with(|| a.record.id().cmp(b.record.id()))
    });
    changed_symbols.sort_by(|a, b| {
        a.name
            .cmp(b.name)
            .then_with(|| a.path.cmp(b.path))
            .then_with(|| a.git_commit.cmp(b.git_commit))
            .then_with(|| a.record.id().cmp(b.record.id()))
    });
    commits.sort_by(|a, b| a.commit.cmp(b.commit));
    tombstones.sort_by(|a, b| {
        a.deleted_id
            .cmp(b.deleted_id)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });
    drift_records.sort_by(|a, b| {
        a.target_record_id
            .cmp(b.target_record_id)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });

    output_observations.sort_by(|a, b| a.record_id.cmp(b.record_id));
    output_project_state.sort_by(|a, b| a.record_id.cmp(b.record_id));
    output_artifacts.sort_by(|a, b| a.record_id.cmp(b.record_id));
    output_verification_evidence.sort_by(|a, b| a.record_id.cmp(b.record_id));
    unexplained.sort_by(|a, b| a.record_id.cmp(b.record_id));
    unresolved.sort_by(|a, b| {
        a.source_record_id
            .cmp(&b.source_record_id)
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.target_domain.cmp(&b.target_domain))
    });
    // The same triple citation can be reached by both the BFS and the seed-path
    // pass above; collapse exact duplicates after sorting.
    unresolved.dedup();

    let (corpus_mode, corpus_mode_source, corpus_disclaimer) =
        super::disclose_corpus(records, super::CorpusMode::Union);

    Ok(ChangesContext {
        changed_files,
        changed_symbols,
        commits,
        tombstones,
        drift_records,
        observations: output_observations,
        project_state: output_project_state,
        artifacts: output_artifacts,
        verification_evidence: output_verification_evidence,
        unexplained,
        unresolved,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
    })
}

/// Returns false when a direct evidence link is anchored (`target_git_commit` or
/// `as_of_commit`) to a commit outside the queried range. History snapshots reuse
/// the same `File`/`Symbol` id across commits, so a citation anchored to an
/// out-of-range version of a reused id is stale context, not an explanation of an
/// in-range change; indexing or traversing it would let stale evidence both
/// surface and mark the new change explained. Unanchored links are unaffected —
/// their per-commit attribution is tracked as a separate follow-up.
fn direct_evidence_link_in_range(link: &EvidenceLink, range_commit_shas: &BTreeSet<&str>) -> bool {
    link.target_git_commit
        .as_deref()
        .or(link.as_of_commit.as_deref())
        .is_none_or(|commit| range_commit_shas.contains(commit))
}

// Internal evidence-traversal helper: every argument is a borrowed slice of the
// caller's traversal context (indexes plus the queried range), so threading them
// individually is clearer than introducing a context struct used in one place.
#[allow(clippy::too_many_arguments)]
fn is_linked_to_evidence<'a>(
    seed_id: &'a str,
    by_id: &BTreeMap<&'a str, &'a GraphRecord>,
    edges_from: &BTreeMap<&'a str, Vec<(EdgeLabel, &'a str)>>,
    edges_to: &BTreeMap<&'a str, Vec<(EdgeLabel, &'a str)>>,
    evidence_links_to: &BTreeMap<&'a str, Vec<&'a str>>,
    tombstoned_ids: &BTreeSet<&'a str>,
    has_any_temporal_version: &BTreeSet<&'a str>,
    range_commit_shas: &BTreeSet<&'a str>,
) -> bool {
    let mut visited = BTreeSet::new();
    let mut frontier = vec![seed_id];
    visited.insert(seed_id);

    // A node counts as explaining evidence only when it classifies into a
    // non-source output section AND has not been retracted by a current-state
    // tombstone — mirroring `classify_and_insert_change` in the output BFS so the
    // `unexplained` verdict never relies on evidence the output never emits.
    let is_evidence = |node_id: &str| -> bool {
        if node_id == seed_id {
            return false;
        }
        if tombstoned_ids.contains(node_id) && !has_any_temporal_version.contains(node_id) {
            return false;
        }
        if let Some(GraphRecord::Node { kind, .. }) = by_id.get(node_id) {
            return classify_node(*kind).is_some_and(|s| s != ContextSection::SourceFact);
        }
        false
    };

    // Only relay nodes (ToolCall/AgentTurn/AgentRun) may bridge to a further hop,
    // matching the output BFS. A non-relay, non-evidence node such as a Commit or
    // Repository must not be traversed through here, otherwise this helper could
    // mark a change explained by an Observation that the output traversal would
    // never reach (and therefore never emit).
    let can_relay = |node_id: &str| -> bool {
        is_bfs_relay_node(node_id, by_id, tombstoned_ids, has_any_temporal_version)
    };

    for _hop in 0..3 {
        let mut next_frontier = Vec::new();
        for current in frontier {
            if let Some(outs) = edges_from.get(current) {
                for (label, target) in outs {
                    let target = *target;
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if visited.insert(target) {
                        if is_evidence(target) {
                            return true;
                        }
                        if can_relay(target) {
                            next_frontier.push(target);
                        }
                    }
                }
            }

            if let Some(ins) = edges_to.get(current) {
                for (label, source) in ins {
                    let source = *source;
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if is_forward_only_label(*label) {
                        continue;
                    }
                    if visited.insert(source) {
                        if is_evidence(source) {
                            return true;
                        }
                        if can_relay(source) {
                            next_frontier.push(source);
                        }
                    }
                }
            }

            if let Some(GraphRecord::Node {
                evidence_links: Some(links),
                ..
            }) = by_id.get(current)
            {
                for link in links {
                    if let Some(target_id) = &link.target_record_id {
                        if !direct_evidence_link_in_range(link, range_commit_shas) {
                            continue;
                        }
                        if visited.insert(target_id.as_str()) {
                            if is_evidence(target_id.as_str()) {
                                return true;
                            }
                            if can_relay(target_id.as_str()) {
                                next_frontier.push(target_id.as_str());
                            }
                        }
                    }
                }
            }

            if let Some(sources) = evidence_links_to.get(current) {
                for source in sources {
                    let source = *source;
                    if visited.insert(source) {
                        if is_evidence(source) {
                            return true;
                        }
                        if can_relay(source) {
                            next_frontier.push(source);
                        }
                    }
                }
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }
    false
}

// ── Failure-History Queries (Issue #63) ─────────────────────────────────────
//
// Answer the operator-visible question "what failed here before, and what
// evidence proves that failure happened?" — starting from a code or task handle
// and returning prior FAILED attempts as citable local facts. Runtime
// command/test/CI failures (verification domain) stay separate from
// agent-authored `Failure` claims so neither is presented as source truth, and a
// later passing verification on the same handle is surfaced as a separate
// superseding item rather than hiding the older failure (AC4, AC5). This slice
// reuses existing agent-memory, verification, artifact, project, redaction, and
// evidence-link contracts; it introduces no new graph domain, node kind, or edge
// vocabulary (AC10).
