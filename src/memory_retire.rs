//! Agent-memory record retirement from recall (issue #156).
//!
//! Retirement curates *recall*: a retired observation-class record stays in
//! history (unlike [`crate::forget`]'s global retraction, which suppresses the
//! target from every current read surface) but is excluded from the default
//! recall path unless the caller passes `--include-retired`. Retirement is
//! reversible: [`reinstate_from_records`] appends a reinstatement receipt, and
//! the full retire/reinstate trail stays queryable.
//!
//! ## Design: event-sourced, additive
//!
//! Retiring a record never mutates or deletes the target's provenance — the
//! command appends a [`NodeKind::RetirementReceipt`] node (and, for the
//! `superseded` reason, one `SUPERSEDES` edge), and reinstatement appends a
//! [`NodeKind::ReinstatementReceipt`] node. The current recall state of a
//! target is the latest receipt on the transaction-time axis:
//! `active -> retired` via `retire`, `retired -> active` via `reinstate`.
//! [`retirement_states`] resolves that latest-wins state for any set of
//! records, optionally pinned to a transaction-time `as_of` instant, so
//! historical recall (`as_of` before the retirement) still sees the target
//! as active.
//!
//! ## Trust separation
//!
//! Only observation-class agent-memory records ([`Observation`], [`Hypothesis`]
//! — via future kinds — [`Decision`], [`Lesson`] — via future kinds — and
//! [`Failure`]) can be retired. Code-graph facts (`File`, `Symbol`, `Import`,
//! `Call`/`CALLS`, `Commit`) are refused without writing anything: agents must
//! not "retire" code facts they merely disagree with. See issue #184 for the
//! separate supersession-write lane (which is additive and orthogonal —
//! retirement with reason `superseded` additionally authors a `SUPERSEDES`
//! edge for audit consistency, but the recall exclusion comes from the
//! retirement receipt, not from the supersession machinery).
//!
//! [`Observation`]: crate::ir::NodeKind::Observation
//! [`Decision`]: crate::ir::NodeKind::Decision
//! [`Failure`]: crate::ir::NodeKind::Failure
//! [`NodeKind::RetirementReceipt`]: crate::ir::NodeKind::RetirementReceipt
//! [`NodeKind::ReinstatementReceipt`]: crate::ir::NodeKind::ReinstatementReceipt

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};
use serde::Serialize;

use crate::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, EvidenceLink, GraphRecord, NodeKind,
    agent_memory_stable_id,
};
use crate::memory_evidence_health::is_observation_class_kind;
use crate::query::liveness::Liveness;
use crate::schema_version::domain_from_record_id;
use crate::supersede_write::{SupersessionTarget, build_supersession_edge};

// ---------------------------------------------------------------------------
// Retire reasons
// ---------------------------------------------------------------------------

/// Typed retirement reason from the closed issue-#156 set.
///
/// Using a strong enum (not a free-text string) keeps the recall audit
/// machine-readable and makes the per-reason validation gates explicit:
/// `superseded` names a superseding record, `drifted` cites the unresolved
/// evidence handle that invalidated the belief, `contradicted` records a
/// contradicting belief (evidence optional), and `operator_decision` records
/// an operator's explicit curatorial call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RetireReason {
    /// The target was superseded by a newer, active observation-class record
    /// (`--superseded-by` is required and validated).
    Superseded,
    /// The evidence grounding the belief no longer resolves
    /// (`--evidence-handle` is required and must cite an unresolved handle).
    Drifted,
    /// The belief was contradicted by other evidence; the contradicting
    /// record may be cited but is not required.
    Contradicted,
    /// An operator explicitly retired the record; evidence may be cited.
    OperatorDecision,
}

impl RetireReason {
    /// Stable machine string carried on the receipt (`text`) and in JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::Drifted => "drifted",
            Self::Contradicted => "contradicted",
            Self::OperatorDecision => "operator-decision",
        }
    }
}

/// Evidence-link relation recording which record supersedes the retired
/// target.
///
/// A custom relation (not in the [`EdgeLabel`] vocabulary) so the ingest-time
/// dangling-citation quarantine skips it: it documents the retire call, it is
/// not an evidence edge.
pub const SUPERSEDED_BY_RELATION: &str = "SUPERSEDED_BY";
/// Evidence-link relation recording the unresolved evidence handle that
/// invalidated a `drifted` retirement. Custom (not an [`EdgeLabel`]) for the
/// same reason: the handle deliberately does not resolve.
pub const DRIFTED_EVIDENCE_RELATION: &str = "DRIFTED_EVIDENCE";
/// Evidence-link relation recording the contradicting record for a
/// `contradicted` retirement.
pub const CONTRADICTED_BY_RELATION: &str = "CONTRADICTED_BY";
/// Evidence-link relation recording evidence cited for an
/// `operator-decision` retirement.
pub const CITED_EVIDENCE_RELATION: &str = "CITED_EVIDENCE";

// ---------------------------------------------------------------------------
// Request / outcome / error types
// ---------------------------------------------------------------------------

/// Input to [`retire_from_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetireRequest {
    /// Record ID (or target handle resolved by the CLI) of the record to retire.
    pub handle: String,
    /// Typed retirement reason.
    pub reason: RetireReason,
    /// Superseding record ID (required for [`RetireReason::Superseded`]).
    pub superseded_by: Option<String>,
    /// Evidence handle (required for [`RetireReason::Drifted`], optional for
    /// [`RetireReason::Contradicted`] and [`RetireReason::OperatorDecision`]).
    pub evidence_handle: Option<String>,
    /// Operator handle recorded as the retiring actor.
    pub retired_by: String,
    /// Fixed RFC 3339 transaction time for deterministic output; defaults to
    /// the current wall-clock instant.
    pub transaction_time: Option<String>,
}

/// Auditable retirement event returned on a successful (or already-satisfied)
/// retire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetireEvent {
    /// Stable ID of the retirement receipt node.
    pub receipt_id: String,
    /// Record ID of the retired target.
    pub retired_record_id: String,
    /// Operator handle recorded as the retiring actor.
    pub retired_by: String,
    /// Transaction time of the receipt.
    pub retired_at: String,
    /// Typed reason code ([`RetireReason::as_str`]).
    pub reason: String,
    /// Superseding record ID (only for `superseded`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Cited evidence handle (only when one was supplied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_handle: Option<String>,
}

/// Outcome of [`retire_from_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetireOutcome {
    /// The record was retired: `records` holds the receipt node (plus the
    /// `SUPERSEDES` edge for the `superseded` reason) ready to ingest.
    Retired {
        /// Auditable retirement event.
        event: RetireEvent,
        /// Records to ingest (receipt node, then edge when applicable).
        records: Vec<GraphRecord>,
    },
    /// The record was already retired; nothing was written. The `records`
    /// field is present (and always empty) so callers can treat both arms
    /// uniformly.
    AlreadyRetired {
        /// Event reconstructed from the existing latest receipt.
        event: RetireEvent,
        /// Always empty for the idempotent arm.
        records: Vec<GraphRecord>,
    },
}

/// Failure modes of [`retire_from_records`], each with a stable machine code
/// and a CLI exit code (2 for "not found" style errors, 1 otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetireError {
    /// `--retired-by` was empty.
    MissingActor,
    /// `--transaction-time` did not parse as RFC 3339.
    InvalidTransactionTime {
        /// The rejected value.
        value: String,
        /// Human-readable detail.
        message: String,
    },
    /// No live record with the handle exists (absent or tombstoned).
    NotFound {
        /// The rejected handle.
        handle: String,
    },
    /// The target is a code-graph fact, which must never be retired.
    CodegraphFact {
        /// The target record ID.
        record_id: String,
        /// Node kind / edge description.
        kind: String,
    },
    /// The target is not an observation-class agent-memory record.
    NonObservationTarget {
        /// The target record ID.
        record_id: String,
        /// Node kind / edge description.
        kind: String,
    },
    /// The target is an audit receipt or tombstone — retirement only applies
    /// to belief records, never to the audit trail itself.
    UnsupportedTarget {
        /// The target record ID.
        record_id: String,
        /// Human-readable detail.
        detail: String,
    },
    /// `--reason superseded` was given without `--superseded-by`.
    MissingSupersedingRecord,
    /// `--superseded-by` names no live record.
    DanglingSupersedingRecord {
        /// The rejected superseding handle.
        superseded_by: String,
    },
    /// `--superseded-by` names a record outside the observation class.
    SupersedingRecordNotObservation {
        /// The rejected superseding handle.
        superseded_by: String,
        /// Node kind / edge description.
        kind: String,
    },
    /// `--superseded-by` names a retired record (a retired belief cannot
    /// supersede anything).
    SupersedingRecordRetired {
        /// The rejected superseding handle.
        superseded_by: String,
    },
    /// `--superseded-by` names the target itself.
    SupersedingSelf {
        /// The rejected handle.
        handle: String,
    },
    /// `--reason drifted` was given without `--evidence-handle`.
    MissingEvidenceHandle,
    /// The drift evidence handle is not cited by the target.
    UnknownEvidenceHandle {
        /// The rejected handle.
        evidence_handle: String,
    },
    /// The drift evidence handle still resolves to a live record.
    EvidenceStillResolves {
        /// The rejected handle.
        evidence_handle: String,
    },
    /// `--reason` is not one of the four typed reason codes.
    InvalidReason {
        /// The rejected reason string.
        reason: String,
    },
}

impl RetireError {
    /// Stable `snake_case` machine code for diagnostics.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingActor => "retire_missing_actor",
            Self::InvalidTransactionTime { .. } => "retire_invalid_transaction_time",
            Self::NotFound { .. } => "retire_not_found",
            Self::CodegraphFact { .. } => "retire_codegraph_fact",
            Self::NonObservationTarget { .. } => "retire_non_observation_target",
            Self::UnsupportedTarget { .. } => "retire_unsupported_target",
            Self::MissingSupersedingRecord => "retire_missing_superseding_record",
            Self::DanglingSupersedingRecord { .. } => "retire_dangling_superseding_record",
            Self::SupersedingRecordNotObservation { .. } => {
                "retire_superseding_record_not_observation"
            }
            Self::SupersedingRecordRetired { .. } => "retire_superseding_record_retired",
            Self::SupersedingSelf { .. } => "retire_superseding_self",
            Self::MissingEvidenceHandle => "retire_missing_evidence_handle",
            Self::UnknownEvidenceHandle { .. } => "retire_unknown_evidence_handle",
            Self::EvidenceStillResolves { .. } => "retire_evidence_still_resolves",
            Self::InvalidReason { .. } => "retire_invalid_reason",
        }
    }

    /// CLI exit code: 2 for not-found style errors, 1 otherwise.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::NotFound { .. } | Self::DanglingSupersedingRecord { .. } => 2,
            _ => 1,
        }
    }

    /// Human-readable detail for the diagnostic envelope.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingActor => "a non-empty --retired-by actor handle is required".to_owned(),
            Self::InvalidTransactionTime { value, message } => {
                format!("invalid --transaction-time {value:?}: {message}")
            }
            Self::NotFound { handle } => {
                format!("no live agent-memory record with handle {handle:?}")
            }
            Self::CodegraphFact { record_id, kind } => format!(
                "refusing to retire code-graph fact {record_id} ({kind}): \
                 retirement only applies to agent-memory observation-class records"
            ),
            Self::NonObservationTarget { record_id, kind } => {
                format!("refusing to retire non-observation-class record {record_id} ({kind})")
            }
            Self::UnsupportedTarget { record_id, detail } => {
                format!("refusing to retire {record_id}: {detail}")
            }
            Self::MissingSupersedingRecord => {
                "--reason superseded requires --superseded-by <record-id>".to_owned()
            }
            Self::DanglingSupersedingRecord { superseded_by } => {
                format!("--superseded-by {superseded_by:?} names no live record")
            }
            Self::SupersedingRecordNotObservation {
                superseded_by,
                kind,
            } => format!(
                "--superseded-by {superseded_by:?} names a non-observation-class record ({kind})"
            ),
            Self::SupersedingRecordRetired { superseded_by } => format!(
                "--superseded-by {superseded_by:?} is retired; a retired record cannot supersede"
            ),
            Self::SupersedingSelf { handle } => {
                format!("--superseded-by {handle:?} names the target itself")
            }
            Self::MissingEvidenceHandle => {
                "--reason drifted requires --evidence-handle <handle>".to_owned()
            }
            Self::UnknownEvidenceHandle { evidence_handle } => {
                format!("evidence handle {evidence_handle:?} is not cited by the target record")
            }
            Self::EvidenceStillResolves { evidence_handle } => format!(
                "evidence handle {evidence_handle:?} still resolves to a live record; \
                 drifted requires an unresolved handle"
            ),
            Self::InvalidReason { reason } => format!(
                "--reason must be one of superseded, drifted, contradicted, \
                 operator-decision; got {reason:?}"
            ),
        }
    }

    /// Machine-readable diagnostic envelope (`{"ok": false, "error": {...}}`).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut error = serde_json::Map::new();
        error.insert(
            "code".to_owned(),
            serde_json::Value::String(self.code().to_owned()),
        );
        match self {
            Self::MissingActor | Self::MissingSupersedingRecord | Self::MissingEvidenceHandle => {}
            Self::InvalidTransactionTime { value, .. } => {
                error.insert("value".to_owned(), serde_json::Value::String(value.clone()));
            }
            Self::NotFound { handle } | Self::SupersedingSelf { handle } => {
                error.insert(
                    "handle".to_owned(),
                    serde_json::Value::String(handle.clone()),
                );
            }
            Self::CodegraphFact { record_id, kind }
            | Self::NonObservationTarget { record_id, kind } => {
                error.insert(
                    "record_id".to_owned(),
                    serde_json::Value::String(record_id.clone()),
                );
                error.insert("kind".to_owned(), serde_json::Value::String(kind.clone()));
            }
            Self::UnsupportedTarget { record_id, detail } => {
                error.insert(
                    "record_id".to_owned(),
                    serde_json::Value::String(record_id.clone()),
                );
                error.insert(
                    "detail".to_owned(),
                    serde_json::Value::String(detail.clone()),
                );
            }
            Self::DanglingSupersedingRecord { superseded_by }
            | Self::SupersedingRecordRetired { superseded_by } => {
                error.insert(
                    "superseded_by".to_owned(),
                    serde_json::Value::String(superseded_by.clone()),
                );
            }
            Self::SupersedingRecordNotObservation {
                superseded_by,
                kind,
            } => {
                error.insert(
                    "superseded_by".to_owned(),
                    serde_json::Value::String(superseded_by.clone()),
                );
                error.insert("kind".to_owned(), serde_json::Value::String(kind.clone()));
            }
            Self::UnknownEvidenceHandle { evidence_handle }
            | Self::EvidenceStillResolves { evidence_handle } => {
                error.insert(
                    "evidence_handle".to_owned(),
                    serde_json::Value::String(evidence_handle.clone()),
                );
            }
            Self::InvalidReason { reason } => {
                error.insert(
                    "reason".to_owned(),
                    serde_json::Value::String(reason.clone()),
                );
            }
        }
        error.insert(
            "message".to_owned(),
            serde_json::Value::String(self.message()),
        );
        serde_json::json!({ "ok": false, "error": error })
    }
}

/// Input to [`reinstate_from_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReinstateRequest {
    /// Record ID (or target handle resolved by the CLI) of the record to
    /// reinstate.
    pub handle: String,
    /// Free-text reason recorded on the reinstatement receipt.
    pub reason: String,
    /// Operator handle recorded as the reinstating actor.
    pub reinstated_by: String,
    /// Fixed RFC 3339 transaction time for deterministic output; defaults to
    /// the current wall-clock instant.
    pub transaction_time: Option<String>,
}

/// Auditable reinstatement event returned on a successful (or
/// already-satisfied) reinstate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReinstateEvent {
    /// Stable ID of the reinstatement receipt node.
    pub receipt_id: String,
    /// Record ID of the reinstated target.
    pub reinstated_record_id: String,
    /// Operator handle recorded as the reinstating actor.
    pub reinstated_by: String,
    /// Transaction time of the receipt.
    pub reinstated_at: String,
    /// Free-text reinstatement reason.
    pub reason: String,
}

/// Outcome of [`reinstate_from_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReinstateOutcome {
    /// The record was reinstated: `records` holds the reinstatement receipt
    /// node ready to ingest.
    Reinstated {
        /// Auditable reinstatement event.
        event: ReinstateEvent,
        /// Records to ingest (the reinstatement receipt node).
        records: Vec<GraphRecord>,
    },
    /// The record was already active; nothing was written. The `records`
    /// field is present (and always empty) so callers can treat both arms
    /// uniformly.
    AlreadyActive {
        /// Event reconstructed from the existing latest receipt.
        event: ReinstateEvent,
        /// Always empty for the idempotent arm.
        records: Vec<GraphRecord>,
    },
}

/// Failure modes of [`reinstate_from_records`], each with a stable machine
/// code and a CLI exit code (2 for "not found" style errors, 1 otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReinstateError {
    /// `--reinstated-by` was empty.
    MissingActor,
    /// `--transaction-time` did not parse as RFC 3339.
    InvalidTransactionTime {
        /// The rejected value.
        value: String,
        /// Human-readable detail.
        message: String,
    },
    /// No live record with the handle exists (absent or tombstoned).
    NotFound {
        /// The rejected handle.
        handle: String,
    },
    /// The record is not currently retired (no latest retirement receipt).
    NotRetired {
        /// The rejected handle.
        handle: String,
    },
    /// The target is a code-graph fact, which is never retired.
    CodegraphFact {
        /// The target record ID.
        record_id: String,
        /// Node kind / edge description.
        kind: String,
    },
    /// The target is not an observation-class agent-memory record.
    NonObservationTarget {
        /// The target record ID.
        record_id: String,
        /// Node kind / edge description.
        kind: String,
    },
}

impl ReinstateError {
    /// Stable `snake_case` machine code for diagnostics.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingActor => "reinstate_missing_actor",
            Self::InvalidTransactionTime { .. } => "reinstate_invalid_transaction_time",
            Self::NotFound { .. } => "reinstate_not_found",
            Self::NotRetired { .. } => "reinstate_not_retired",
            Self::CodegraphFact { .. } => "reinstate_codegraph_fact",
            Self::NonObservationTarget { .. } => "reinstate_non_observation_target",
        }
    }

    /// CLI exit code: 2 for not-found style errors, 1 otherwise.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::NotFound { .. } => 2,
            _ => 1,
        }
    }

    /// Human-readable detail for the diagnostic envelope.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingActor => "a non-empty --reinstated-by actor handle is required".to_owned(),
            Self::InvalidTransactionTime { value, message } => {
                format!("invalid --transaction-time {value:?}: {message}")
            }
            Self::NotFound { handle } => {
                format!("no live agent-memory record with handle {handle:?}")
            }
            Self::NotRetired { handle } => {
                format!("record {handle:?} is not retired (no latest retirement receipt)")
            }
            Self::CodegraphFact { record_id, kind } => format!(
                "refusing to reinstate code-graph fact {record_id} ({kind}): \
                 code-graph facts are never retired"
            ),
            Self::NonObservationTarget { record_id, kind } => {
                format!("refusing to reinstate non-observation-class record {record_id} ({kind})")
            }
        }
    }

    /// Machine-readable diagnostic envelope (`{"ok": false, "error": {...}}`).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut error = serde_json::Map::new();
        error.insert(
            "code".to_owned(),
            serde_json::Value::String(self.code().to_owned()),
        );
        match self {
            Self::MissingActor => {}
            Self::InvalidTransactionTime { value, .. } => {
                error.insert("value".to_owned(), serde_json::Value::String(value.clone()));
            }
            Self::NotFound { handle } | Self::NotRetired { handle } => {
                error.insert(
                    "handle".to_owned(),
                    serde_json::Value::String(handle.clone()),
                );
            }
            Self::CodegraphFact { record_id, kind }
            | Self::NonObservationTarget { record_id, kind } => {
                error.insert(
                    "record_id".to_owned(),
                    serde_json::Value::String(record_id.clone()),
                );
                error.insert("kind".to_owned(), serde_json::Value::String(kind.clone()));
            }
        }
        error.insert(
            "message".to_owned(),
            serde_json::Value::String(self.message()),
        );
        serde_json::json!({ "ok": false, "error": error })
    }
}

// ---------------------------------------------------------------------------
// Recall state resolution
// ---------------------------------------------------------------------------

/// Current recall state of an agent-memory record, derived from its receipt
/// trail. The latest receipt on the transaction-time axis wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RetirementState {
    /// The record participates in default recall (no latest retirement
    /// receipt).
    Active,
    /// The record is excluded from default recall by this receipt.
    Retired {
        /// Stable ID of the latest retirement receipt node.
        receipt_id: String,
        /// Typed reason code ([`RetireReason::as_str`]).
        reason: String,
        /// Operator handle recorded as the retiring actor.
        retired_by: String,
        /// Transaction time of the latest retirement receipt.
        retired_at: String,
    },
}

/// Latest non-deleted node (or edge) for a record id, in file order.
///
/// Mirrors the "history-inclusive read" used by the records lane: a record id
/// may appear more than once (append-only updates), and the last occurrence
/// wins unless a tombstone follows it.
fn latest_live_record<'a>(
    records: &'a [GraphRecord],
    liveness: &Liveness,
    id: &str,
) -> Option<&'a GraphRecord> {
    records
        .iter()
        .rfind(|record| record.id() == id)
        .filter(|_| !liveness.deleted(id))
}

/// Whether a handle currently names a live record (exists and not tombstoned).
fn handle_resolves(records: &[GraphRecord], liveness: &Liveness, handle: &str) -> bool {
    latest_live_record(records, liveness, handle).is_some()
}

/// Validate a transaction time, preserving the supplied text byte-for-byte.
///
/// The receipt needs an orderable timestamp and the state resolver needs to
/// compare against it; the raw string is what operators see in receipts and
/// what the CLI echoes back, so it is kept verbatim (trimmed) rather than
/// normalised to a canonical spelling.
fn resolve_transaction_time(value: Option<&str>) -> Result<String, (String, String)> {
    match value {
        None => Ok(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err((
                    raw.to_owned(),
                    "transaction time must not be blank".to_owned(),
                ));
            }
            match DateTime::parse_from_rfc3339(trimmed) {
                Ok(_) => Ok(trimmed.to_owned()),
                Err(err) => Err((raw.to_owned(), err.to_string())),
            }
        }
    }
}

/// Classification of a would-be retire/reinstate target.
enum TargetClass {
    /// Retireable: an observation-class agent-memory belief.
    Observation,
    /// A code-graph fact (immutable extraction, not a belief). Carries the kind.
    CodegraphFact(String),
    /// An agent-memory record outside the observation class. Carries the kind.
    NonObservation(String),
    /// Tombstones, receipts, and other records retirement must never touch.
    /// Carries a human-readable detail.
    Unsupported(String),
}

/// Decide whether a live record may be retired or reinstated.
fn classify_target(record: &GraphRecord) -> TargetClass {
    match record {
        GraphRecord::Tombstone { .. } => TargetClass::Unsupported(
            "tombstones cannot be retired; the target is already retracted".to_owned(),
        ),
        GraphRecord::Edge { label, .. } => {
            let kind = format!("edge {}", label.as_str());
            // Edges are deterministic topology. Code-graph edges are facts;
            // agent-memory edges are not beliefs either.
            match domain_from_record_id(record.id()).as_deref() {
                Some("codegraph") => TargetClass::CodegraphFact(kind),
                _ => TargetClass::NonObservation(kind),
            }
        }
        GraphRecord::Node { kind, .. } => {
            // Retirement is for beliefs. Audit receipts record the operator's
            // own actions; retiring one would rewrite history rather than
            // curate beliefs, so it is refused.
            if matches!(
                kind,
                NodeKind::RetirementReceipt | NodeKind::ReinstatementReceipt | NodeKind::Retraction
            ) {
                return TargetClass::Unsupported(
                    "audit receipts cannot be retired; retirement applies to belief records"
                        .to_owned(),
                );
            }
            if is_observation_class_kind(*kind) {
                return TargetClass::Observation;
            }
            let kind_name = kind.as_str().to_owned();
            let codegraph = matches!(
                domain_from_record_id(record.id()).as_deref(),
                Some("codegraph")
            ) || matches!(
                kind,
                NodeKind::File | NodeKind::Symbol | NodeKind::Import | NodeKind::Commit
            );
            if codegraph {
                TargetClass::CodegraphFact(kind_name)
            } else {
                TargetClass::NonObservation(kind_name)
            }
        }
    }
}

/// Whether a target record cites `handle` in its provenance surface.
///
/// A drifted retirement must name an evidence handle the belief actually
/// cited; the check looks at the record's own evidence links, its source
/// handle, and the artifact provenance fields.
fn target_cites_handle(target: &GraphRecord, handle: &str) -> bool {
    match target {
        GraphRecord::Node {
            evidence_links,
            source_handle,
            source_artifact_path,
            source_artifact_hash,
            ..
        } => {
            if source_handle.as_deref() == Some(handle) {
                return true;
            }
            if source_artifact_path.as_deref() == Some(handle) {
                return true;
            }
            if source_artifact_hash.as_deref() == Some(handle) {
                return true;
            }
            evidence_links
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .any(|link| link.target_record_id.as_deref() == Some(handle))
        }
        _ => false,
    }
}

/// Build an evidence link from a receipt to its reason metadata.
fn receipt_link(target_record_id: String, relation: &str) -> EvidenceLink {
    let domain = domain_from_record_id(&target_record_id).unwrap_or_else(|| "unknown".to_owned());
    EvidenceLink {
        target_record_id: Some(target_record_id),
        target_domain: domain,
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

/// The newest receipt (by transaction time, ties broken by file order) for a
/// target, restricted to receipts at or before `as_of` when pinned.
///
/// Returns the receipt [`GraphRecord`] itself so callers can rebuild the
/// exact event without re-deriving fields.
fn latest_receipt_node<'a>(
    records: &'a [GraphRecord],
    target_id: &str,
    as_of: Option<DateTime<FixedOffset>>,
) -> Option<&'a GraphRecord> {
    let mut best: Option<(Option<DateTime<FixedOffset>>, usize, &'a GraphRecord)> = None;
    for (index, record) in records.iter().enumerate() {
        let GraphRecord::Node {
            kind,
            source_handle,
            transaction_time,
            ..
        } = record
        else {
            continue;
        };
        if !matches!(
            kind,
            NodeKind::RetirementReceipt | NodeKind::ReinstatementReceipt
        ) {
            continue;
        }
        if source_handle.as_deref() != Some(target_id) {
            continue;
        }
        let tx = transaction_time
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
        if let (Some(tx), Some(as_of)) = (tx, as_of)
            && tx > as_of
        {
            continue;
        }
        // Receipts without a transaction time sort before any timed receipt;
        // the writer always stamps one, so this only orders hand-built stores.
        let replace = match &best {
            None => true,
            Some((best_tx, best_index, _)) => (tx, index) > (*best_tx, *best_index),
        };
        if replace {
            best = Some((tx, index, record));
        }
    }
    best.map(|(_, _, record)| record)
}

/// Rebuild a retire event from a retirement receipt node.
pub(crate) fn retire_event_from_node(node: &GraphRecord) -> RetireEvent {
    let mut event = RetireEvent {
        receipt_id: String::new(),
        retired_record_id: String::new(),
        retired_by: String::new(),
        retired_at: String::new(),
        reason: String::new(),
        superseded_by: None,
        evidence_handle: None,
    };
    let GraphRecord::Node {
        id,
        text,
        agent_id,
        transaction_time,
        source_handle,
        evidence_links,
        ..
    } = node
    else {
        return event;
    };
    event.receipt_id.clone_from(id);
    if let Some(handle) = source_handle {
        event.retired_record_id.clone_from(handle);
    }
    if let Some(agent) = agent_id {
        event.retired_by.clone_from(agent);
    }
    if let Some(tx) = transaction_time {
        event.retired_at.clone_from(tx);
    }
    if let Some(reason) = text {
        event.reason.clone_from(reason);
    }
    for link in evidence_links.as_deref().unwrap_or(&[]) {
        match link.relation.as_str() {
            SUPERSEDED_BY_RELATION => {
                event.superseded_by.clone_from(&link.target_record_id);
            }
            DRIFTED_EVIDENCE_RELATION | CONTRADICTED_BY_RELATION | CITED_EVIDENCE_RELATION => {
                // First evidence link wins; receipts carry at most one.
                if event.evidence_handle.is_none() {
                    event.evidence_handle.clone_from(&link.target_record_id);
                }
            }
            _ => {}
        }
    }
    event
}

/// Rebuild a reinstate event from a reinstatement receipt node.
pub(crate) fn reinstate_event_from_node(node: &GraphRecord) -> ReinstateEvent {
    let mut event = ReinstateEvent {
        receipt_id: String::new(),
        reinstated_record_id: String::new(),
        reinstated_by: String::new(),
        reinstated_at: String::new(),
        reason: String::new(),
    };
    let GraphRecord::Node {
        id,
        text,
        agent_id,
        transaction_time,
        source_handle,
        ..
    } = node
    else {
        return event;
    };
    event.receipt_id.clone_from(id);
    if let Some(handle) = source_handle {
        event.reinstated_record_id.clone_from(handle);
    }
    if let Some(agent) = agent_id {
        event.reinstated_by.clone_from(agent);
    }
    if let Some(tx) = transaction_time {
        event.reinstated_at.clone_from(tx);
    }
    if let Some(reason) = text {
        event.reason.clone_from(reason);
    }
    event
}

/// Stamp the shared receipt fields onto a freshly built receipt node.
fn stamp_receipt(
    receipt: &mut GraphRecord,
    target_id: &str,
    body: &str,
    actor: &str,
    tx: &str,
    links: Vec<EvidenceLink>,
) {
    if let GraphRecord::Node {
        schema_version,
        domain,
        text,
        agent_id,
        transaction_time,
        source_handle,
        valid_time,
        valid_time_source,
        evidence_links,
        ..
    } = receipt
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *text = Some(body.to_owned());
        *agent_id = Some(actor.to_owned());
        *transaction_time = Some(tx.to_owned());
        *source_handle = Some(target_id.to_owned());
        *valid_time = Some(tx.to_owned());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        *evidence_links = if links.is_empty() { None } else { Some(links) };
    }
}

/// Resolves the recall state of every record targeted by a retirement or
/// reinstatement receipt in `records` (issue #156).
///
/// Latest receipt on the transaction-time axis wins; when `as_of` is given,
/// only receipts at or before the pinned instant are considered, so a
/// transaction-time `as_of` before a retirement still sees the target as
/// active.
#[must_use]
pub fn retirement_states(
    records: &[GraphRecord],
    as_of: Option<DateTime<chrono::FixedOffset>>,
) -> BTreeMap<String, RetirementState> {
    let mut targets: BTreeSet<String> = BTreeSet::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::RetirementReceipt | NodeKind::ReinstatementReceipt,
            source_handle: Some(target),
            ..
        } = record
        {
            targets.insert(target.clone());
        }
    }
    targets
        .into_iter()
        .map(|target| {
            // A target whose receipts all postdate the pin was active at the
            // pin: it is still reported, as `Active`, rather than dropped.
            let state = latest_receipt_node(records, &target, as_of).map_or(
                RetirementState::Active,
                |node| match node {
                    GraphRecord::Node {
                        kind: NodeKind::RetirementReceipt,
                        ..
                    } => {
                        let event = retire_event_from_node(node);
                        RetirementState::Retired {
                            receipt_id: event.receipt_id,
                            reason: event.reason,
                            retired_by: event.retired_by,
                            retired_at: event.retired_at,
                        }
                    }
                    _ => RetirementState::Active,
                },
            );
            (target, state)
        })
        .collect()
}

/// One entry of a target's receipt trail: the ordered transaction-time
/// history of retirements and reinstatements (issue #156).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReceiptTrailEntry {
    /// `"retired"` or `"reinstated"`.
    pub action: &'static str,
    /// The rebuilt receipt event, serialized with the event's own fields.
    pub event: serde_json::Value,
}

/// The ordered receipt trail for one target.
///
/// Every retirement and reinstatement receipt whose `source_handle` names
/// `target_id`, ordered by transaction time (file order breaks ties), so the
/// last entry is the receipt that controls the current state. When `as_of`
/// is given, receipts after the pinned instant are excluded, matching
/// [`retirement_states`].
#[must_use]
pub fn receipt_trail(
    records: &[GraphRecord],
    target_id: &str,
    as_of: Option<DateTime<chrono::FixedOffset>>,
) -> Vec<ReceiptTrailEntry> {
    let mut ordered: Vec<(String, usize, &GraphRecord)> = Vec::new();
    for (position, record) in records.iter().enumerate() {
        if let GraphRecord::Node {
            kind: NodeKind::RetirementReceipt | NodeKind::ReinstatementReceipt,
            source_handle: Some(target),
            transaction_time: Some(tx),
            ..
        } = record
        {
            if target != target_id {
                continue;
            }
            if let Some(pin) = as_of {
                let tx_dt = DateTime::parse_from_rfc3339(tx).ok();
                if tx_dt.is_none_or(|dt| dt > pin) {
                    continue;
                }
            }
            ordered.push((tx.clone(), position, record));
        }
    }
    ordered.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    ordered
        .into_iter()
        .map(|(_, _, record)| match record {
            GraphRecord::Node {
                kind: NodeKind::RetirementReceipt,
                ..
            } => ReceiptTrailEntry {
                action: "retired",
                event: serde_json::to_value(retire_event_from_node(record))
                    .unwrap_or(serde_json::Value::Null),
            },
            _ => ReceiptTrailEntry {
                action: "reinstated",
                event: serde_json::to_value(reinstate_event_from_node(record))
                    .unwrap_or(serde_json::Value::Null),
            },
        })
        .collect()
}

/// True when the record is currently retired per its receipt trail.
#[must_use]
pub fn is_retired(records: &[GraphRecord], record_id: &str) -> bool {
    matches!(
        retirement_states(records, None).get(record_id),
        Some(RetirementState::Retired { .. })
    )
}

/// Machine-readable recall-state label for query output (issue #156).
///
/// `state` is `"active"` or `"retired"`; the remaining fields are present
/// only for retired records. Records with no receipts at all label as
/// active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetirementStateLabel {
    /// `"active"` or `"retired"`.
    pub state: &'static str,
    /// Typed reason code ([`RetireReason::as_str`]); retired only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Operator handle recorded as the retiring actor; retired only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_by: Option<String>,
    /// Transaction time of the latest retirement receipt; retired only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<String>,
}

impl From<&RetirementState> for RetirementStateLabel {
    fn from(state: &RetirementState) -> Self {
        match state {
            RetirementState::Active => Self {
                state: "active",
                reason: None,
                retired_by: None,
                retired_at: None,
            },
            RetirementState::Retired {
                reason,
                retired_by,
                retired_at,
                ..
            } => Self {
                state: "retired",
                reason: Some(reason.clone()),
                retired_by: Some(retired_by.clone()),
                retired_at: Some(retired_at.clone()),
            },
        }
    }
}

/// The query-output label for a record id: its resolved state, or active
/// when the record has no receipts.
#[must_use]
pub fn retirement_label(
    states: &BTreeMap<String, RetirementState>,
    record_id: &str,
) -> RetirementStateLabel {
    states.get(record_id).map_or(
        RetirementStateLabel {
            state: "active",
            reason: None,
            retired_by: None,
            retired_at: None,
        },
        RetirementStateLabel::from,
    )
}

// ---------------------------------------------------------------------------
// Stable IDs
// ---------------------------------------------------------------------------

/// Deterministic retirement-receipt ID.
///
/// Every input that distinguishes two retire events feeds the hash — target,
/// transaction time, typed reason, actor, superseder, and evidence handle —
/// so reruns with identical inputs are byte-identical while distinct events
/// never share an ID (e.g. two retirements of one target at one instant with
/// different reasons, which would otherwise overwrite each other's audit
/// trail on ingest).
#[must_use]
pub fn retirement_receipt_id(
    target_id: &str,
    transaction_time: &str,
    reason: &str,
    retired_by: &str,
    superseded_by: Option<&str>,
    evidence_handle: Option<&str>,
) -> String {
    agent_memory_stable_id(&[
        "node",
        "retirement-receipt",
        target_id,
        transaction_time,
        reason,
        retired_by,
        superseded_by.unwrap_or(""),
        evidence_handle.unwrap_or(""),
    ])
}

/// Deterministic reinstatement-receipt ID (same construction as
/// [`retirement_receipt_id`]).
#[must_use]
pub fn reinstatement_receipt_id(
    target_id: &str,
    transaction_time: &str,
    reason: &str,
    reinstated_by: &str,
) -> String {
    agent_memory_stable_id(&[
        "node",
        "reinstatement-receipt",
        target_id,
        transaction_time,
        reason,
        reinstated_by,
    ])
}

// ---------------------------------------------------------------------------
// Core commands
// ---------------------------------------------------------------------------

/// Resolve the retire target: the latest live record for `handle`, verified
/// to be observation-class. Returns the target and its stable id.
///
/// # Errors
///
/// Returns [`RetireError::NotFound`] for unknown or tombstoned handles, and
/// the classification refusal for non-observation targets.
fn resolve_retire_target<'r>(
    records: &'r [GraphRecord],
    liveness: &Liveness,
    handle: &str,
) -> Result<(&'r GraphRecord, String), RetireError> {
    let target =
        latest_live_record(records, liveness, handle).ok_or_else(|| RetireError::NotFound {
            handle: handle.to_owned(),
        })?;
    let target_id = target.id().to_owned();
    match classify_target(target) {
        TargetClass::Observation => {}
        TargetClass::CodegraphFact(kind) => {
            return Err(RetireError::CodegraphFact {
                record_id: target_id,
                kind,
            });
        }
        TargetClass::NonObservation(kind) => {
            return Err(RetireError::NonObservationTarget {
                record_id: target_id,
                kind,
            });
        }
        TargetClass::Unsupported(detail) => {
            return Err(RetireError::UnsupportedTarget {
                record_id: target_id,
                detail,
            });
        }
    }
    Ok((target, target_id))
}

/// Run the per-reason validation gates for a retire request, returning the
/// validated `(superseded_by, evidence_handle)` pair.
///
/// # Errors
///
/// Returns the reason-specific refusal when a gate fails; writes nothing.
fn check_retire_reason_gates(
    records: &[GraphRecord],
    liveness: &Liveness,
    target: &GraphRecord,
    handle: &str,
    request: &RetireRequest,
) -> Result<(Option<String>, Option<String>), RetireError> {
    match request.reason {
        RetireReason::Superseded => {
            let superseder = request
                .superseded_by
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or(RetireError::MissingSupersedingRecord)?;
            if superseder == handle {
                return Err(RetireError::SupersedingSelf {
                    handle: handle.to_owned(),
                });
            }
            let superseder_record =
                latest_live_record(records, liveness, superseder).ok_or_else(|| {
                    RetireError::DanglingSupersedingRecord {
                        superseded_by: superseder.to_owned(),
                    }
                })?;
            match classify_target(superseder_record) {
                TargetClass::Observation => {}
                TargetClass::CodegraphFact(kind) | TargetClass::NonObservation(kind) => {
                    return Err(RetireError::SupersedingRecordNotObservation {
                        superseded_by: superseder.to_owned(),
                        kind,
                    });
                }
                TargetClass::Unsupported(detail) => {
                    return Err(RetireError::SupersedingRecordNotObservation {
                        superseded_by: superseder.to_owned(),
                        kind: detail,
                    });
                }
            }
            if is_retired(records, superseder) {
                return Err(RetireError::SupersedingRecordRetired {
                    superseded_by: superseder.to_owned(),
                });
            }
            Ok((Some(superseder.to_owned()), None))
        }
        RetireReason::Drifted => {
            let evidence = request
                .evidence_handle
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or(RetireError::MissingEvidenceHandle)?;
            if !target_cites_handle(target, evidence) {
                return Err(RetireError::UnknownEvidenceHandle {
                    evidence_handle: evidence.to_owned(),
                });
            }
            if handle_resolves(records, liveness, evidence) {
                return Err(RetireError::EvidenceStillResolves {
                    evidence_handle: evidence.to_owned(),
                });
            }
            Ok((None, Some(evidence.to_owned())))
        }
        RetireReason::Contradicted | RetireReason::OperatorDecision => {
            // Evidence is optional here; when supplied it must name a live
            // record so the receipt's citation is not dangling.
            let evidence = request
                .evidence_handle
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(evidence) = evidence
                && !handle_resolves(records, liveness, evidence)
            {
                return Err(RetireError::UnknownEvidenceHandle {
                    evidence_handle: evidence.to_owned(),
                });
            }
            Ok((None, evidence.map(str::to_owned)))
        }
    }
}

/// Retires an agent-memory record from recall (issue #156).
///
/// Appends a [`NodeKind::RetirementReceipt`] node (plus a `SUPERSEDES` edge
/// for the `superseded` reason) without mutating the target. Refusals write
/// nothing.
///
/// # Errors
///
/// Returns a [`RetireError`] refusal for unknown handles, non-observation
/// targets, failed reason gates, or malformed inputs. Nothing is written on
/// any error path.
pub fn retire_from_records(
    records: &[GraphRecord],
    request: &RetireRequest,
) -> Result<RetireOutcome, RetireError> {
    let actor = request.retired_by.trim();
    if actor.is_empty() {
        return Err(RetireError::MissingActor);
    }
    let tx = resolve_transaction_time(request.transaction_time.as_deref())
        .map_err(|(value, message)| RetireError::InvalidTransactionTime { value, message })?;
    let handle = request.handle.trim();
    let liveness = Liveness::new(records);
    let (target, target_id) = resolve_retire_target(records, &liveness, handle)?;

    // Idempotent: the latest receipt already retired it.
    if let Some(node) = latest_receipt_node(records, &target_id, None)
        && let GraphRecord::Node {
            kind: NodeKind::RetirementReceipt,
            ..
        } = node
    {
        return Ok(RetireOutcome::AlreadyRetired {
            event: retire_event_from_node(node),
            records: Vec::new(),
        });
    }

    // Reason gates, in the order the diagnostics report them.
    let (superseded_by, evidence_handle) =
        check_retire_reason_gates(records, &liveness, target, handle, request)?;

    // Build the receipt and any reason metadata links.
    let mut links = Vec::new();
    if let Some(superseder) = &superseded_by {
        links.push(receipt_link(superseder.clone(), SUPERSEDED_BY_RELATION));
    }
    if let Some(evidence) = &evidence_handle {
        let relation = match request.reason {
            RetireReason::Drifted => DRIFTED_EVIDENCE_RELATION,
            RetireReason::Contradicted => CONTRADICTED_BY_RELATION,
            RetireReason::Superseded | RetireReason::OperatorDecision => CITED_EVIDENCE_RELATION,
        };
        links.push(receipt_link(evidence.clone(), relation));
    }
    let receipt_id = retirement_receipt_id(
        &target_id,
        &tx,
        request.reason.as_str(),
        actor,
        superseded_by.as_deref(),
        evidence_handle.as_deref(),
    );
    let mut receipt = GraphRecord::node(
        receipt_id.clone(),
        NodeKind::RetirementReceipt,
        None,
        None,
        None,
        format!("Retirement of {target_id} ({})", request.reason.as_str()),
    );
    stamp_receipt(
        &mut receipt,
        &target_id,
        request.reason.as_str(),
        actor,
        &tx,
        links,
    );

    let mut out_records = vec![receipt];
    if let Some(superseder) = &superseded_by {
        // The superseded reason also records the normal belief-supersession
        // edge, so downstream graph consumers see the same topology the
        // `supersede` flow would have written.
        out_records.push(build_supersession_edge(
            superseder.as_str(),
            &SupersessionTarget {
                target_record_id: target_id.clone(),
                label: EdgeLabel::Supersedes,
            },
            "1.0",
        ));
    }

    Ok(RetireOutcome::Retired {
        event: RetireEvent {
            receipt_id,
            retired_record_id: target_id,
            retired_by: actor.to_owned(),
            retired_at: tx,
            reason: request.reason.as_str().to_owned(),
            superseded_by,
            evidence_handle,
        },
        records: out_records,
    })
}

/// Reinstates a retired agent-memory record to active recall (issue #156).
///
/// Appends a [`NodeKind::ReinstatementReceipt`] node without mutating the
/// target. Refusals write nothing.
///
/// # Errors
///
/// Returns a [`ReinstateError`] refusal for unknown handles, non-observation
/// targets, records that are not retired, or malformed inputs. Nothing is
/// written on any error path.
pub fn reinstate_from_records(
    records: &[GraphRecord],
    request: &ReinstateRequest,
) -> Result<ReinstateOutcome, ReinstateError> {
    let actor = request.reinstated_by.trim();
    if actor.is_empty() {
        return Err(ReinstateError::MissingActor);
    }
    let tx = resolve_transaction_time(request.transaction_time.as_deref())
        .map_err(|(value, message)| ReinstateError::InvalidTransactionTime { value, message })?;
    let handle = request.handle.trim();
    let liveness = Liveness::new(records);
    let target =
        latest_live_record(records, &liveness, handle).ok_or_else(|| ReinstateError::NotFound {
            handle: handle.to_owned(),
        })?;
    let target_id = target.id().to_owned();
    // Only observation-class records can be retired, so only they can be
    // reinstated; anything else never had a retirement to undo.
    match classify_target(target) {
        TargetClass::Observation => {}
        TargetClass::CodegraphFact(kind) => {
            return Err(ReinstateError::CodegraphFact {
                record_id: target_id,
                kind,
            });
        }
        TargetClass::NonObservation(kind) => {
            return Err(ReinstateError::NonObservationTarget {
                record_id: target_id,
                kind,
            });
        }
        TargetClass::Unsupported(detail) => {
            return Err(ReinstateError::NonObservationTarget {
                record_id: target_id,
                kind: detail,
            });
        }
    }

    match latest_receipt_node(records, &target_id, None) {
        Some(GraphRecord::Node {
            kind: NodeKind::RetirementReceipt,
            ..
        }) => {}
        Some(node) => {
            // The newest receipt is a reinstatement (or, defensively, any
            // non-retirement receipt): the record is already active.
            return Ok(ReinstateOutcome::AlreadyActive {
                event: reinstate_event_from_node(node),
                records: Vec::new(),
            });
        }
        None => {
            return Err(ReinstateError::NotRetired {
                handle: handle.to_owned(),
            });
        }
    }

    let receipt_id = reinstatement_receipt_id(&target_id, &tx, &request.reason, actor);
    let mut receipt = GraphRecord::node(
        receipt_id.clone(),
        NodeKind::ReinstatementReceipt,
        None,
        None,
        None,
        format!("Reinstatement of {target_id}"),
    );
    stamp_receipt(
        &mut receipt,
        &target_id,
        &request.reason,
        actor,
        &tx,
        Vec::new(),
    );
    Ok(ReinstateOutcome::Reinstated {
        event: ReinstateEvent {
            receipt_id,
            reinstated_record_id: target_id,
            reinstated_by: actor.to_owned(),
            reinstated_at: tx,
            reason: request.reason.clone(),
        },
        records: vec![receipt],
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{EvidenceLink, NodeKind};
    use crate::query::liveness::Liveness;

    const TX: &str = "2026-09-28T11:00:00Z";

    fn observation(id: &str) -> GraphRecord {
        let mut record = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            Some(format!("observed {id}")),
            None,
            None,
            format!("Observation {id}"),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            transaction_time,
            agent_id,
            session_id,
            observed_at,
            source_handle,
            valid_time,
            valid_time_source,
            ..
        } = &mut record
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
            *transaction_time = Some(TX.to_owned());
            *agent_id = Some("agent".to_owned());
            *session_id = Some("session-1".to_owned());
            *observed_at = Some(TX.to_owned());
            *source_handle = Some(format!("obs:{id}"));
            *valid_time = Some(TX.to_owned());
            *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        }
        record
    }

    fn failure_record(id: &str) -> GraphRecord {
        let mut record = GraphRecord::node(
            id.to_owned(),
            NodeKind::Failure,
            Some(format!("failed {id}")),
            None,
            None,
            format!("Failure {id}"),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            transaction_time,
            agent_id,
            failure_kind,
            ..
        } = &mut record
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *domain = Some("agent_memory".to_owned());
            *transaction_time = Some(TX.to_owned());
            *agent_id = Some("agent".to_owned());
            *failure_kind = Some("timeout".to_owned());
        }
        record
    }

    fn codegraph_symbol() -> GraphRecord {
        GraphRecord::node(
            "codegraph:v1:repo:src/main.rs:main".to_owned(),
            NodeKind::Symbol,
            Some("fn main()".to_owned()),
            None,
            None,
            "Symbol main".to_owned(),
        )
    }

    fn agent_run_node() -> GraphRecord {
        GraphRecord::node(
            "agent_memory:v1:run-1".to_owned(),
            NodeKind::AgentRun,
            None,
            None,
            None,
            "AgentRun run-1".to_owned(),
        )
    }

    fn retire_request(handle: &str, reason: RetireReason) -> RetireRequest {
        RetireRequest {
            handle: handle.to_owned(),
            reason,
            superseded_by: None,
            evidence_handle: None,
            retired_by: "operator".to_owned(),
            transaction_time: Some("2026-09-28T12:00:00Z".to_owned()),
        }
    }

    fn reinstate_request(handle: &str) -> ReinstateRequest {
        ReinstateRequest {
            handle: handle.to_owned(),
            reason: "the operator checked and the belief holds".to_owned(),
            reinstated_by: "operator".to_owned(),
            transaction_time: Some("2026-09-28T13:00:00Z".to_owned()),
        }
    }

    // -- happy paths ---------------------------------------------------------

    #[test]
    fn retire_superseded_happy_path_writes_receipt_and_edge() {
        let old = observation("agent_memory:v1:obs-old");
        let new = observation("agent_memory:v1:obs-new");
        let mut request = retire_request("agent_memory:v1:obs-old", RetireReason::Superseded);
        request.superseded_by = Some("agent_memory:v1:obs-new".to_owned());
        let before_target = old.clone();

        let outcome = retire_from_records(&[old.clone(), new], &request)
            .expect("superseded retire should succeed");
        let RetireOutcome::Retired { event, records } = outcome else {
            panic!("expected Retired outcome");
        };

        // Event fields.
        assert_eq!(event.retired_record_id, "agent_memory:v1:obs-old");
        assert_eq!(event.retired_by, "operator");
        assert_eq!(event.retired_at, "2026-09-28T12:00:00Z");
        assert_eq!(event.reason, "superseded");
        assert_eq!(
            event.superseded_by.as_deref(),
            Some("agent_memory:v1:obs-new")
        );
        assert!(event.evidence_handle.is_none());
        assert_eq!(
            event.receipt_id,
            retirement_receipt_id(
                "agent_memory:v1:obs-old",
                "2026-09-28T12:00:00Z",
                "superseded",
                "operator",
                Some("agent_memory:v1:obs-new"),
                None,
            )
        );

        // Two records: receipt node, then the SUPERSEDES edge.
        assert_eq!(records.len(), 2);
        let GraphRecord::Node {
            kind,
            agent_id,
            transaction_time,
            source_handle,
            evidence_links,
            text,
            ..
        } = &records[0]
        else {
            panic!("first record must be the receipt node");
        };
        assert_eq!(*kind, NodeKind::RetirementReceipt);
        assert_eq!(agent_id.as_deref(), Some("operator"));
        assert_eq!(transaction_time.as_deref(), Some("2026-09-28T12:00:00Z"));
        assert_eq!(source_handle.as_deref(), Some("agent_memory:v1:obs-old"));
        assert_eq!(text.as_deref(), Some("superseded"));
        let links = evidence_links
            .clone()
            .expect("receipt carries evidence links");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].relation, SUPERSEDED_BY_RELATION);
        assert_eq!(
            links[0].target_record_id.as_deref(),
            Some("agent_memory:v1:obs-new")
        );

        let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = &records[1]
        else {
            panic!("second record must be the SUPERSEDES edge");
        };
        assert_eq!(*label, EdgeLabel::Supersedes);
        assert_eq!(source, "agent_memory:v1:obs-new");
        assert_eq!(target, "agent_memory:v1:obs-old");

        // The target's provenance is untouched by retirement.
        assert_eq!(old, before_target, "retirement must not mutate the target");

        // State resolves to retired.
        let mut all = vec![old];
        all.extend(records);
        let states = retirement_states(&all, None);
        assert!(matches!(
            states.get("agent_memory:v1:obs-old"),
            Some(RetirementState::Retired { reason, .. }) if reason == "superseded"
        ));
    }

    #[test]
    fn retire_drifted_happy_path_cites_unresolved_handle() {
        let mut obs = observation("agent_memory:v1:obs-drift");
        // The belief cites an evidence handle that no longer resolves.
        if let GraphRecord::Node { evidence_links, .. } = &mut obs {
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some("codegraph:v1:repo:src/deleted.rs".to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: Some("src/deleted.rs".to_owned()),
                target_span: None,
                target_git_commit: None,
            }]);
        }
        let mut request = retire_request("agent_memory:v1:obs-drift", RetireReason::Drifted);
        request.evidence_handle = Some("codegraph:v1:repo:src/deleted.rs".to_owned());

        let outcome = retire_from_records(std::slice::from_ref(&obs), &request)
            .expect("drifted retire should succeed");
        let RetireOutcome::Retired { event, records } = outcome else {
            panic!("expected Retired outcome");
        };
        assert_eq!(event.reason, "drifted");
        assert_eq!(
            event.evidence_handle.as_deref(),
            Some("codegraph:v1:repo:src/deleted.rs")
        );
        assert_eq!(records.len(), 1, "drifted writes the receipt only");
        let GraphRecord::Node { evidence_links, .. } = &records[0] else {
            panic!("expected receipt node");
        };
        let links = evidence_links
            .clone()
            .expect("receipt carries evidence links");
        assert_eq!(links[0].relation, DRIFTED_EVIDENCE_RELATION);
        assert_eq!(
            links[0].target_record_id.as_deref(),
            Some("codegraph:v1:repo:src/deleted.rs")
        );
    }

    #[test]
    fn retire_operator_decision_needs_no_evidence() {
        let obs = observation("agent_memory:v1:obs-op");
        let outcome = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-op", RetireReason::OperatorDecision),
        )
        .expect("operator-decision retire should succeed");
        let RetireOutcome::Retired { event, records } = outcome else {
            panic!("expected Retired outcome");
        };
        assert_eq!(event.reason, "operator-decision");
        assert_eq!(records.len(), 1);
        assert!(is_retired(
            &[obs, records[0].clone()],
            "agent_memory:v1:obs-op"
        ));
    }

    #[test]
    fn retire_contradicted_with_optional_evidence() {
        let obs = observation("agent_memory:v1:obs-contra");
        let contra = observation("agent_memory:v1:obs-contra-2");
        let mut request = retire_request("agent_memory:v1:obs-contra", RetireReason::Contradicted);
        request.evidence_handle = Some("agent_memory:v1:obs-contra-2".to_owned());
        let outcome = retire_from_records(&[obs, contra], &request)
            .expect("contradicted retire should succeed");
        let RetireOutcome::Retired { event, .. } = outcome else {
            panic!("expected Retired outcome");
        };
        assert_eq!(event.reason, "contradicted");
        assert_eq!(
            event.evidence_handle.as_deref(),
            Some("agent_memory:v1:obs-contra-2")
        );
    }

    #[test]
    fn retire_failure_record_participates_in_recall_state() {
        let fail = failure_record("agent_memory:v1:fail-1");
        let outcome = retire_from_records(
            std::slice::from_ref(&fail),
            &retire_request("agent_memory:v1:fail-1", RetireReason::OperatorDecision),
        )
        .expect("failure records are observation-class and retireable");
        let RetireOutcome::Retired { records, .. } = outcome else {
            panic!("expected Retired outcome");
        };
        assert!(is_retired(
            &[fail, records[0].clone()],
            "agent_memory:v1:fail-1"
        ));
    }

    #[test]
    fn retire_is_idempotent_for_already_retired_record() {
        let obs = observation("agent_memory:v1:obs-idem");
        let first = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-idem", RetireReason::OperatorDecision),
        )
        .expect("first retire should succeed");
        let RetireOutcome::Retired {
            event: first_event,
            records,
        } = first
        else {
            panic!("expected Retired outcome");
        };
        let mut all = vec![obs, records[0].clone()];
        let second = retire_from_records(
            &all,
            &retire_request("agent_memory:v1:obs-idem", RetireReason::OperatorDecision),
        )
        .expect("second retire should be idempotent");
        let RetireOutcome::AlreadyRetired { event, .. } = second else {
            panic!("expected AlreadyRetired outcome");
        };
        assert_eq!(event, first_event);
        all.extend(records);
        let states = retirement_states(&all, None);
        assert!(matches!(
            states.get("agent_memory:v1:obs-idem"),
            Some(RetirementState::Retired { .. })
        ));
    }

    #[test]
    fn reinstate_returns_record_to_active_recall() {
        let obs = observation("agent_memory:v1:obs-rein");
        let retired = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-rein", RetireReason::OperatorDecision),
        )
        .expect("retire should succeed");
        let RetireOutcome::Retired { records, .. } = retired else {
            panic!("expected Retired outcome");
        };
        let mut all = vec![obs, records[0].clone()];

        let outcome = reinstate_from_records(&all, &reinstate_request("agent_memory:v1:obs-rein"))
            .expect("reinstate should succeed");
        let ReinstateOutcome::Reinstated { event, records } = outcome else {
            panic!("expected Reinstated outcome");
        };
        assert_eq!(event.reinstated_record_id, "agent_memory:v1:obs-rein");
        assert_eq!(event.reinstated_by, "operator");
        assert_eq!(event.reinstated_at, "2026-09-28T13:00:00Z");
        assert_eq!(
            event.receipt_id,
            reinstatement_receipt_id(
                "agent_memory:v1:obs-rein",
                "2026-09-28T13:00:00Z",
                "the operator checked and the belief holds",
                "operator",
            )
        );
        assert_eq!(records.len(), 1);
        let GraphRecord::Node { kind, .. } = &records[0] else {
            panic!("expected reinstatement receipt node");
        };
        assert_eq!(*kind, NodeKind::ReinstatementReceipt);

        // Latest receipt wins: retired -> active.
        all.extend(records);
        let states = retirement_states(&all, None);
        assert_eq!(
            states.get("agent_memory:v1:obs-rein"),
            Some(&RetirementState::Active)
        );
    }

    #[test]
    fn retire_after_reinstate_appends_new_receipt() {
        let obs = observation("agent_memory:v1:obs-cycle");
        let mut all = vec![obs];

        // Retire (12:00) -> reinstate (13:00) -> retire (14:00): a full cycle
        // with a reinstatement between the retirements, so each retire is a
        // genuine new write rather than an idempotent no-op.
        let mut req = retire_request("agent_memory:v1:obs-cycle", RetireReason::OperatorDecision);
        req.transaction_time = Some("2026-09-28T12:00:00Z".to_owned());
        let retired = retire_from_records(&all, &req).expect("first retire should succeed");
        let RetireOutcome::Retired { records, .. } = retired else {
            panic!("expected Retired outcome");
        };
        all.extend(records);
        assert!(is_retired(&all, "agent_memory:v1:obs-cycle"));

        let mut rein_req = reinstate_request("agent_memory:v1:obs-cycle");
        rein_req.transaction_time = Some("2026-09-28T13:00:00Z".to_owned());
        let reinstated = reinstate_from_records(&all, &rein_req).expect("reinstate should succeed");
        let ReinstateOutcome::Reinstated { records, .. } = reinstated else {
            panic!("expected Reinstated outcome");
        };
        all.extend(records);
        assert_eq!(
            retirement_states(&all, None).get("agent_memory:v1:obs-cycle"),
            Some(&RetirementState::Active)
        );

        // A second retirement after reinstatement appends a NEW receipt; the
        // history keeps both retirements and the reinstatement.
        let mut req = retire_request("agent_memory:v1:obs-cycle", RetireReason::Contradicted);
        req.transaction_time = Some("2026-09-28T14:00:00Z".to_owned());
        let retired = retire_from_records(&all, &req).expect("second retire should succeed");
        let RetireOutcome::Retired { event, records } = retired else {
            panic!("expected Retired outcome");
        };
        assert_eq!(event.reason, "contradicted");
        all.extend(records);
        let states = retirement_states(&all, None);
        assert!(matches!(
            states.get("agent_memory:v1:obs-cycle"),
            Some(RetirementState::Retired { reason, .. }) if reason == "contradicted"
        ));
        // Full audit trail is intact: 2 retirement receipts + 1 reinstatement.
        let receipt_count = all
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::RetirementReceipt | NodeKind::ReinstatementReceipt,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(receipt_count, 3);
    }

    // -- as_of historical recall ------------------------------------------------

    #[test]
    fn as_of_before_retirement_sees_record_active() {
        let obs = observation("agent_memory:v1:obs-asof");
        let mut req = retire_request("agent_memory:v1:obs-asof", RetireReason::OperatorDecision);
        // Pin the receipt's transaction time: the test must not depend on the
        // wall clock at run time.
        req.transaction_time = Some("2026-09-28T12:00:00Z".to_owned());
        let retired =
            retire_from_records(std::slice::from_ref(&obs), &req).expect("retire should succeed");
        let RetireOutcome::Retired { records, .. } = retired else {
            panic!("expected Retired outcome");
        };
        let all = vec![obs, records[0].clone()];

        let before = DateTime::parse_from_rfc3339("2026-09-28T11:59:59Z").unwrap();
        assert_eq!(
            retirement_states(&all, Some(before)).get("agent_memory:v1:obs-asof"),
            Some(&RetirementState::Active)
        );
        let at = DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z").unwrap();
        assert!(matches!(
            retirement_states(&all, Some(at)).get("agent_memory:v1:obs-asof"),
            Some(RetirementState::Retired { .. })
        ));
        assert!(matches!(
            retirement_states(&all, None).get("agent_memory:v1:obs-asof"),
            Some(RetirementState::Retired { .. })
        ));
    }

    // -- determinism -------------------------------------------------------------

    #[test]
    fn identical_inputs_produce_byte_identical_receipts() {
        let obs = observation("agent_memory:v1:obs-determ");
        let request = retire_request("agent_memory:v1:obs-determ", RetireReason::OperatorDecision);
        let first = retire_from_records(std::slice::from_ref(&obs), &request)
            .expect("retire should succeed");
        let second = retire_from_records(std::slice::from_ref(&obs), &request)
            .expect("retire should succeed");
        let (
            RetireOutcome::Retired { records: r1, .. },
            RetireOutcome::Retired { records: r2, .. },
        ) = (first, second)
        else {
            panic!("expected Retired outcomes");
        };
        let b1 = serde_json::to_string(&r1[0]).unwrap();
        let b2 = serde_json::to_string(&r2[0]).unwrap();
        assert_eq!(b1, b2, "identical inputs must serialize byte-identically");
    }

    // -- refusals ----------------------------------------------------------------

    #[test]
    fn retire_refuses_codegraph_symbol_without_writing() {
        let symbol = codegraph_symbol();
        let err = retire_from_records(
            std::slice::from_ref(&symbol),
            &retire_request(
                "codegraph:v1:repo:src/main.rs:main",
                RetireReason::OperatorDecision,
            ),
        )
        .expect_err("code-graph facts must be refused");
        assert_eq!(err.code(), "retire_codegraph_fact");
        assert_eq!(err.exit_code(), 1);
        assert!(err.message().contains("code-graph fact"));
    }

    #[test]
    fn retire_refuses_non_observation_agent_memory_record() {
        let run = agent_run_node();
        let err = retire_from_records(
            std::slice::from_ref(&run),
            &retire_request("agent_memory:v1:run-1", RetireReason::OperatorDecision),
        )
        .expect_err("non-observation records must be refused");
        assert_eq!(err.code(), "retire_non_observation_target");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn retire_refuses_receipt_targets() {
        let obs = observation("agent_memory:v1:obs-rcpt");
        let retired = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-rcpt", RetireReason::OperatorDecision),
        )
        .expect("retire should succeed");
        let RetireOutcome::Retired { records, .. } = retired else {
            panic!("expected Retired outcome");
        };
        let receipt_id = records[0].id().to_owned();
        let all = vec![obs, records[0].clone()];
        let err = retire_from_records(
            &all,
            &retire_request(&receipt_id, RetireReason::OperatorDecision),
        )
        .expect_err("receipts must be refused as targets");
        assert_eq!(err.code(), "retire_unsupported_target");
    }

    #[test]
    fn retire_unknown_handle_is_not_found() {
        let err = retire_from_records(
            &[],
            &retire_request("agent_memory:v1:nope", RetireReason::OperatorDecision),
        )
        .expect_err("unknown handles must be not-found");
        assert_eq!(err.code(), "retire_not_found");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn retire_tombstoned_target_is_not_found() {
        let obs = observation("agent_memory:v1:obs-tomb");
        let tombstone = GraphRecord::Tombstone {
            id: "agent_memory:v1:tombstone-obs-tomb".to_owned(),
            schema_version: AGENT_MEMORY_SCHEMA_VERSION,
            deleted_id: "agent_memory:v1:obs-tomb".to_owned(),
            summary: "tombstone for obs-tomb".to_owned(),
            producer: None,
        };
        let liveness = Liveness::new(std::slice::from_ref(&tombstone));
        assert!(liveness.deleted("agent_memory:v1:obs-tomb"));
        let err = retire_from_records(
            &[obs, tombstone],
            &retire_request("agent_memory:v1:obs-tomb", RetireReason::OperatorDecision),
        )
        .expect_err("tombstoned targets count as absent");
        assert_eq!(err.code(), "retire_not_found");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn retire_requires_actor_and_valid_transaction_time() {
        let obs = observation("agent_memory:v1:obs-val");
        let mut request = retire_request("agent_memory:v1:obs-val", RetireReason::OperatorDecision);
        request.retired_by = String::new();
        let err = retire_from_records(std::slice::from_ref(&obs), &request)
            .expect_err("empty actor must fail");
        assert_eq!(err.code(), "retire_missing_actor");

        let mut request = retire_request("agent_memory:v1:obs-val", RetireReason::OperatorDecision);
        request.transaction_time = Some("not-a-time".to_owned());
        let err = retire_from_records(std::slice::from_ref(&obs), &request)
            .expect_err("invalid transaction time must fail");
        assert_eq!(err.code(), "retire_invalid_transaction_time");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn invalid_reason_carries_stable_code_and_exit_one() {
        let err = RetireError::InvalidReason {
            reason: "forgotten".to_owned(),
        };
        assert_eq!(err.code(), "retire_invalid_reason");
        assert_eq!(err.exit_code(), 1);
        let envelope = err.to_json();
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["code"], "retire_invalid_reason");
        assert_eq!(envelope["error"]["reason"], "forgotten");
    }

    #[test]
    fn retire_superseded_requires_and_validates_superseding_record() {
        let old = observation("agent_memory:v1:obs-old-2");
        let run = agent_run_node();

        // Missing --superseded-by.
        let err = retire_from_records(
            std::slice::from_ref(&old),
            &retire_request("agent_memory:v1:obs-old-2", RetireReason::Superseded),
        )
        .expect_err("missing superseding record must fail");
        assert_eq!(err.code(), "retire_missing_superseding_record");
        assert_eq!(err.exit_code(), 1);

        // Dangling --superseded-by (exit 2, like not-found).
        let mut request = retire_request("agent_memory:v1:obs-old-2", RetireReason::Superseded);
        request.superseded_by = Some("agent_memory:v1:ghost".to_owned());
        let err = retire_from_records(std::slice::from_ref(&old), &request)
            .expect_err("dangling superseding record must fail");
        assert_eq!(err.code(), "retire_dangling_superseding_record");
        assert_eq!(err.exit_code(), 2);

        // Superseding record outside the observation class.
        let mut request = retire_request("agent_memory:v1:obs-old-2", RetireReason::Superseded);
        request.superseded_by = Some("agent_memory:v1:run-1".to_owned());
        let err = retire_from_records(&[old.clone(), run], &request)
            .expect_err("non-observation superseding record must fail");
        assert_eq!(err.code(), "retire_superseding_record_not_observation");

        // Self-supersession.
        let mut request = retire_request("agent_memory:v1:obs-old-2", RetireReason::Superseded);
        request.superseded_by = Some("agent_memory:v1:obs-old-2".to_owned());
        let err = retire_from_records(std::slice::from_ref(&old), &request)
            .expect_err("self-supersession must fail");
        assert_eq!(err.code(), "retire_superseding_self");

        // Retired records cannot supersede.
        let newer = observation("agent_memory:v1:obs-newer");
        let retire_newer = retire_from_records(
            &[old.clone(), newer.clone()],
            &retire_request("agent_memory:v1:obs-newer", RetireReason::OperatorDecision),
        )
        .expect("retire newer should succeed");
        let RetireOutcome::Retired { records, .. } = retire_newer else {
            panic!("expected Retired outcome");
        };
        let all = vec![old, newer, records[0].clone()];
        let mut request = retire_request("agent_memory:v1:obs-old-2", RetireReason::Superseded);
        request.superseded_by = Some("agent_memory:v1:obs-newer".to_owned());
        let err =
            retire_from_records(&all, &request).expect_err("retired superseding record must fail");
        assert_eq!(err.code(), "retire_superseding_record_retired");
    }

    #[test]
    fn retire_drifted_requires_unresolved_cited_evidence() {
        let mut obs = observation("agent_memory:v1:obs-drift-2");
        if let GraphRecord::Node { evidence_links, .. } = &mut obs {
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some("agent_memory:v1:live-evidence".to_owned()),
                target_domain: "agent_memory".to_owned(),
                relation: "OBSERVES".to_owned(),
                confidence: "1.0".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
        let evidence = observation("agent_memory:v1:live-evidence");

        // Missing handle.
        let err = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-drift-2", RetireReason::Drifted),
        )
        .expect_err("missing evidence handle must fail");
        assert_eq!(err.code(), "retire_missing_evidence_handle");

        // Unknown handle (not cited by the target).
        let mut request = retire_request("agent_memory:v1:obs-drift-2", RetireReason::Drifted);
        request.evidence_handle = Some("agent_memory:v1:not-cited".to_owned());
        let err = retire_from_records(&[obs.clone(), evidence.clone()], &request)
            .expect_err("uncited handle must fail");
        assert_eq!(err.code(), "retire_unknown_evidence_handle");

        // Cited handle that still resolves.
        let mut request = retire_request("agent_memory:v1:obs-drift-2", RetireReason::Drifted);
        request.evidence_handle = Some("agent_memory:v1:live-evidence".to_owned());
        let err = retire_from_records(&[obs, evidence], &request)
            .expect_err("resolving handle must fail for drifted");
        assert_eq!(err.code(), "retire_evidence_still_resolves");
    }

    // -- reinstate refusals -------------------------------------------------------

    #[test]
    fn reinstate_refuses_non_retired_and_unknown_records() {
        let obs = observation("agent_memory:v1:obs-active");
        let err = reinstate_from_records(
            std::slice::from_ref(&obs),
            &reinstate_request("agent_memory:v1:obs-active"),
        )
        .expect_err("active records must be refused");
        assert_eq!(err.code(), "reinstate_not_retired");
        assert_eq!(err.exit_code(), 1);

        let err = reinstate_from_records(&[], &reinstate_request("agent_memory:v1:ghost"))
            .expect_err("unknown handles must be not-found");
        assert_eq!(err.code(), "reinstate_not_found");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn reinstate_requires_actor_and_valid_transaction_time() {
        let obs = observation("agent_memory:v1:obs-reval");
        let retired = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-reval", RetireReason::OperatorDecision),
        )
        .expect("retire should succeed");
        let RetireOutcome::Retired { records, .. } = retired else {
            panic!("expected Retired outcome");
        };
        let all = vec![obs, records[0].clone()];

        let mut request = reinstate_request("agent_memory:v1:obs-reval");
        request.reinstated_by = String::new();
        let err = reinstate_from_records(&all, &request).expect_err("empty actor must fail");
        assert_eq!(err.code(), "reinstate_missing_actor");

        let mut request = reinstate_request("agent_memory:v1:obs-reval");
        request.transaction_time = Some("yesterday-ish".to_owned());
        let err =
            reinstate_from_records(&all, &request).expect_err("invalid transaction time must fail");
        assert_eq!(err.code(), "reinstate_invalid_transaction_time");
    }

    #[test]
    fn reinstate_is_idempotent_for_already_active_record() {
        let obs = observation("agent_memory:v1:obs-reidem");
        let retired = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-reidem", RetireReason::OperatorDecision),
        )
        .expect("retire should succeed");
        let RetireOutcome::Retired { records, .. } = retired else {
            panic!("expected Retired outcome");
        };
        let mut all = vec![obs, records[0].clone()];
        let first = reinstate_from_records(&all, &reinstate_request("agent_memory:v1:obs-reidem"))
            .expect("reinstate should succeed");
        let ReinstateOutcome::Reinstated {
            event: first_event,
            records,
        } = first
        else {
            panic!("expected Reinstated outcome");
        };
        all.extend(records);
        let second = reinstate_from_records(&all, &reinstate_request("agent_memory:v1:obs-reidem"))
            .expect("second reinstate should be idempotent");
        let ReinstateOutcome::AlreadyActive { event, .. } = second else {
            panic!("expected AlreadyActive outcome");
        };
        assert_eq!(event, first_event);
    }

    // -- error envelope shape ------------------------------------------------------

    #[test]
    fn error_envelopes_carry_stable_codes_and_messages() {
        let err = RetireError::DanglingSupersedingRecord {
            superseded_by: "agent_memory:v1:ghost".to_owned(),
        };
        let envelope = err.to_json();
        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error"]["code"],
            "retire_dangling_superseding_record"
        );
        assert_eq!(envelope["error"]["superseded_by"], "agent_memory:v1:ghost");
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no live record")
        );

        let err = ReinstateError::NotRetired {
            handle: "agent_memory:v1:obs-x".to_owned(),
        };
        let envelope = err.to_json();
        assert_eq!(envelope["error"]["code"], "reinstate_not_retired");
        assert_eq!(envelope["error"]["handle"], "agent_memory:v1:obs-x");
    }

    // -- reason codes ---------------------------------------------------------------

    #[test]
    fn reason_codes_are_stable_kebab_case() {
        assert_eq!(RetireReason::Superseded.as_str(), "superseded");
        assert_eq!(RetireReason::Drifted.as_str(), "drifted");
        assert_eq!(RetireReason::Contradicted.as_str(), "contradicted");
        assert_eq!(RetireReason::OperatorDecision.as_str(), "operator-decision");
    }

    // -- receipt IDs ----------------------------------------------------------------

    #[test]
    fn receipt_ids_are_deterministic_and_distinguish_events() {
        let id = || {
            retirement_receipt_id(
                "agent_memory:v1:obs-x",
                "2026-09-28T12:00:00Z",
                "superseded",
                "op-1",
                Some("agent_memory:v1:obs-y"),
                None,
            )
        };
        // Identical inputs rerun byte-identical (AC9).
        assert_eq!(id(), id());
        // Every distinguishing input feeds the hash: two retirements of one
        // target at one instant with different reasons/actors/evidence must
        // never share an ID, or the later ingest would overwrite the earlier
        // receipt's audit trail.
        let different_reason = retirement_receipt_id(
            "agent_memory:v1:obs-x",
            "2026-09-28T12:00:00Z",
            "drifted",
            "op-1",
            None,
            Some("codegraph:v1:repo:src/gone.rs"),
        );
        assert_ne!(id(), different_reason);
        let different_actor = retirement_receipt_id(
            "agent_memory:v1:obs-x",
            "2026-09-28T12:00:00Z",
            "superseded",
            "op-2",
            Some("agent_memory:v1:obs-y"),
            None,
        );
        assert_ne!(id(), different_actor);
        // Reinstatement receipts live in their own ID space.
        let reinstatement = reinstatement_receipt_id(
            "agent_memory:v1:obs-x",
            "2026-09-28T12:00:00Z",
            "re-checked",
            "op-1",
        );
        assert_ne!(id(), reinstatement);
        assert_eq!(
            reinstatement,
            reinstatement_receipt_id(
                "agent_memory:v1:obs-x",
                "2026-09-28T12:00:00Z",
                "re-checked",
                "op-1",
            )
        );
    }

    // -- untouched provenance ---------------------------------------------------------

    #[test]
    fn target_provenance_fields_are_untouched_by_reinstate_too() {
        let obs = observation("agent_memory:v1:obs-prov");
        let before = obs.clone();
        let retired = retire_from_records(
            std::slice::from_ref(&obs),
            &retire_request("agent_memory:v1:obs-prov", RetireReason::OperatorDecision),
        )
        .expect("retire should succeed");
        let RetireOutcome::Retired { records, .. } = retired else {
            panic!("expected Retired outcome");
        };
        let mut all = vec![obs, records[0].clone()];
        let reinstated =
            reinstate_from_records(&all, &reinstate_request("agent_memory:v1:obs-prov"))
                .expect("reinstate should succeed");
        let ReinstateOutcome::Reinstated { records, .. } = reinstated else {
            panic!("expected Reinstated outcome");
        };
        all.extend(records);
        assert_eq!(
            all[0], before,
            "neither retire nor reinstate mutates the target"
        );
        // Provenance fields on the surviving record are exactly as written.
        let GraphRecord::Node {
            source_handle,
            valid_time,
            agent_id,
            session_id,
            ..
        } = &all[0]
        else {
            panic!("expected node");
        };
        assert_eq!(
            source_handle.as_deref(),
            Some("obs:agent_memory:v1:obs-prov")
        );
        assert_eq!(valid_time.as_deref(), Some(TX));
        assert_eq!(agent_id.as_deref(), Some("agent"));
        assert_eq!(session_id.as_deref(), Some("session-1"));
    }
}
