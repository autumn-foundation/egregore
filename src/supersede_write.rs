//! Write-time supersession and contradiction authoring (issue #184).
//!
//! A live agent that learns its earlier note was wrong can now say so on the
//! write path it actually uses: `eg write observation --supersedes <id>` (or
//! `--contradicts <id>`) authors a `SUPERSEDES` (or `CONTRADICTS`) edge from
//! the new observation to a prior observation-class record. The read-side
//! supersession features (#92 recall flagging, #64 single-record audit, #235
//! belief timeline) consume the authored edge; this module only *produces* it.
//!
//! Everything here is pure: the CLI layer performs store I/O (loading the
//! `--graph` / `--data-dir` records) and these functions validate the target,
//! detect idempotency conflicts, and construct the edge record. No function in
//! this module mutates the target record — the write is purely additive, so
//! the prior record's provenance fields (`agent_id`, `observed_at`,
//! `source_handle`) are never rewritten in place.
//!
//! A `CONTRADICTS` edge is a mutual dispute, not a winner declaration: the
//! write asserts only that the authoring agent recorded the new observation as
//! disputing the prior one at `observed_at`. It is a temporal-authorship lead,
//! never a truth claim about which record is correct.

use crate::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, GraphRecord, NodeKind, agent_memory_stable_id,
};
use crate::memory_evidence_health::is_observation_class_kind;
use crate::query::liveness::Liveness;

/// A write-time supersession / contradiction relationship to author.
///
/// The label is restricted to [`EdgeLabel::Supersedes`] and
/// [`EdgeLabel::Contradicts`]; any other label is rejected by
/// [`crate::evidence::build_observation_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersessionTarget {
    /// Stable record ID of the prior observation-class record.
    pub target_record_id: String,
    /// [`EdgeLabel::Supersedes`] or [`EdgeLabel::Contradicts`].
    pub label: EdgeLabel,
}

/// Machine-readable write-time rejection for a supersession/contradiction
/// target.
///
/// The JSON envelope never echoes observation text or payload values — only
/// stable handles (record IDs, kinds) the caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupersessionError {
    /// `--supersedes` / `--contradicts` given without `--graph` or `--data-dir`.
    StoreRequired,
    /// The `--graph` file or `--data-dir` store could not be read or parsed.
    StoreUnreadable {
        /// Which input failed (`"graph"` or `"data_dir"`); the path itself is
        /// not echoed to keep the envelope free of environment-specific noise.
        input: String,
    },
    /// The target ID names no live node record in the store.
    UnknownTarget {
        /// The supplied target handle.
        target_id: String,
    },
    /// The target is a deterministic code-graph fact (node, edge, or tombstone
    /// in the `codegraph:` domain). Deterministic facts are corrected by
    /// re-scan, never by agent supersession.
    CodegraphFact {
        /// The supplied target handle.
        target_id: String,
        /// Node kind (or `edge` / `tombstone`) of the target.
        kind: String,
    },
    /// The target is a live node but not observation-class (`Observation`,
    /// `Decision`, `Failure`).
    NonObservationTarget {
        /// The supplied target handle.
        target_id: String,
        /// Node kind of the target.
        kind: String,
    },
    /// The target is the new observation itself: the only cycle a fresh write
    /// can create.
    SelfTarget {
        /// The supplied target handle (== the new observation's ID).
        target_id: String,
    },
    /// The same observation identity already carries a live edge of the same
    /// relation to a *different* target: the write is not an identical re-run,
    /// so it conflicts instead of duplicating or silently overwriting.
    Conflict {
        /// The new observation's stable ID (the idempotency key).
        source_id: String,
        /// The differing target the live edge already names.
        existing_target_id: String,
        /// The relation of the existing live edge.
        label: EdgeLabel,
    },
}

impl SupersessionError {
    /// Stable machine-readable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::StoreRequired => "supersession_store_required",
            Self::StoreUnreadable { .. } => "supersession_store_unreadable",
            Self::UnknownTarget { .. } => "unknown_supersession_target",
            Self::CodegraphFact { .. } => "supersede_codegraph_fact",
            Self::NonObservationTarget { .. } => "supersede_non_observation_target",
            Self::SelfTarget { .. } => "supersession_cycle",
            Self::Conflict { .. } => "supersession_conflict",
        }
    }

    /// Process exit code: 2 for a handle that names nothing (mirrors
    /// `eg forget`'s handle-not-found), 1 for every other refusal.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::UnknownTarget { .. } => 2,
            _ => 1,
        }
    }

    /// JSON envelope for stderr. `field` names the CLI flag that carried the
    /// target (`"supersedes"` or `"contradicts"`). Only handles are echoed —
    /// never observation text or payload values.
    #[must_use]
    pub fn to_json(&self, field: &str) -> serde_json::Value {
        let mut envelope = serde_json::json!({
            "code": self.code(),
            "field": field,
        });
        let detail = match self {
            Self::StoreRequired => None,
            Self::StoreUnreadable { input } => Some(("input", input.clone())),
            Self::UnknownTarget { target_id } | Self::SelfTarget { target_id } => {
                Some(("target", target_id.clone()))
            }
            Self::CodegraphFact { target_id, kind }
            | Self::NonObservationTarget { target_id, kind } => {
                Some(("target", format!("{target_id} ({kind})")))
            }
            Self::Conflict {
                source_id,
                existing_target_id,
                label,
            } => Some((
                "detail",
                format!(
                    "{source_id} already has a live {} edge to {existing_target_id}",
                    label.as_str()
                ),
            )),
        };
        if let Some((key, value)) = detail {
            envelope[key] = serde_json::Value::String(value);
        }
        envelope
    }
}

/// Short kind description for diagnostics: node kind, or `edge`/`tombstone`.
fn describe_record(record: &GraphRecord) -> String {
    match record {
        GraphRecord::Node { kind, .. } => kind.as_str().to_owned(),
        GraphRecord::Edge { .. } => "edge".to_owned(),
        GraphRecord::Tombstone { .. } => "tombstone".to_owned(),
    }
}

/// Validates a supersession/contradiction target against loaded store records.
///
/// Returns the target's [`NodeKind`] on success. Rejects:
/// - a target ID with no live record ([`SupersessionError::UnknownTarget`];
///   tombstoned targets count as absent — a retracted record cannot be
///   superseded),
/// - a deterministic code-graph fact
///   ([`SupersessionError::CodegraphFact`]),
/// - a live node that is not observation-class
///   ([`SupersessionError::NonObservationTarget`]).
///
/// Trust separation: only observation-class nodes (`Observation`, `Decision`,
/// `Failure` per [`is_observation_class_kind`]) are accepted, so the authored
/// edge only ever connects agent-authored observation-class records.
///
/// # Errors
///
/// Returns [`SupersessionError`] when the target is unknown, tombstoned,
/// a deterministic code-graph fact, not observation-class, or is an edge
/// rather than a node.
pub fn validate_supersession_target(
    records: &[GraphRecord],
    target_id: &str,
) -> Result<NodeKind, SupersessionError> {
    let liveness = Liveness::new(records);
    let target = records.iter().rfind(|record| record.id() == target_id);
    let Some(record) = target else {
        return Err(SupersessionError::UnknownTarget {
            target_id: target_id.to_owned(),
        });
    };
    if liveness.deleted(target_id) {
        return Err(SupersessionError::UnknownTarget {
            target_id: target_id.to_owned(),
        });
    }
    // Deterministic code-graph facts are corrected by re-scan, never by agent
    // supersession — regardless of whether the handle names a node, an edge
    // (e.g. a `CALLS` edge), or a tombstone.
    if target_id.starts_with("codegraph:") {
        return Err(SupersessionError::CodegraphFact {
            target_id: target_id.to_owned(),
            kind: describe_record(record),
        });
    }
    match record {
        GraphRecord::Node { kind, .. } if is_observation_class_kind(*kind) => Ok(*kind),
        _ => Err(SupersessionError::NonObservationTarget {
            target_id: target_id.to_owned(),
            kind: describe_record(record),
        }),
    }
}

/// Detects the idempotency conflict: the same observation identity (the
/// content-addressed ID, i.e. the idempotency key) already carries a live edge
/// of the same relation to a *different* target.
///
/// An identical re-run (same relation, same target) is not a conflict — the
/// write is byte-identical and ingest converges to a no-op. Only the latest
/// live edge version per edge ID participates, matching the read-side
/// latest-write-wins gate.
///
/// # Errors
///
/// Returns [`SupersessionError::Conflict`] when the source already carries a
/// live edge of the same relation to a different target.
pub fn check_supersession_conflict(
    records: &[GraphRecord],
    source_id: &str,
    label: EdgeLabel,
    target_id: &str,
) -> Result<(), SupersessionError> {
    let liveness = Liveness::new(records);
    for (index, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            label: edge_label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if *edge_label != label || source != source_id {
            continue;
        }
        if liveness.deleted(id) || !liveness.is_latest_edge_version(id, index) {
            continue;
        }
        if target != target_id {
            return Err(SupersessionError::Conflict {
                source_id: source_id.to_owned(),
                existing_target_id: target.clone(),
                label,
            });
        }
    }
    Ok(())
}

/// Builds the deterministic `SUPERSEDES` / `CONTRADICTS` edge record.
///
/// The edge ID is content-addressed over the label, source, and target, so
/// an identical re-run mints the identical edge and ingest converges to a
/// no-op. The edge lives in the `agent_memory:` domain; it is authored by
/// the agent (not the daemon) because only the agent knows its earlier note
/// was wrong — the daemon never infers supersession.
///
/// `confidence` is the new observation's confidence. The schema registry
/// requires `confidence` on `CONTRADICTS` edges, so it is always set there;
/// `SUPERSEDES` carries no confidence (matching the denormalized link
/// convention).
#[must_use]
pub fn build_supersession_edge(
    source_id: &str,
    target: &SupersessionTarget,
    confidence: &str,
) -> GraphRecord {
    debug_assert!(matches!(
        target.label,
        EdgeLabel::Supersedes | EdgeLabel::Contradicts
    ));
    let label_str = target.label.as_str();
    let id = agent_memory_stable_id(&["edge", label_str, source_id, &target.target_record_id]);
    GraphRecord::Edge {
        id,
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        label: target.label,
        source: source_id.to_owned(),
        target: target.target_record_id.clone(),
        confidence: match target.label {
            EdgeLabel::Contradicts => Some(confidence.to_owned()),
            _ => None,
        },
        resolution: None,
        frame_resolution: None,
        frame_index: None,
        basis: None,
        call_site_spans: None,
        is_exhaustive: None,
        temporal: None,
        summary: format!("{source_id} {label_str} {}", target.target_record_id),
        producer: None,
    }
}
