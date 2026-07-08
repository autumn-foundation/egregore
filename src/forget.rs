//! Operator-facing logical retraction of a persisted record (issue #231).
//!
//! `eg forget <handle>` retracts one record — identified by its stable record
//! ID — from every transaction-time-current read surface, without destroying
//! the bytes. Retraction writes two records through the ordinary adapter
//! boundary:
//!
//! 1. A domain-scoped [`GraphRecord::Tombstone`] whose `deleted_id` is the
//!    target. Every current-state read lane (structural queries, semantic /
//!    vector search, context, task, memory, audit, failures, changes, MCP
//!    tools) already excludes actively tombstoned records, so the exclusion is
//!    enforced by the same machinery the deterministic refresh path uses.
//! 2. A citable [`NodeKind::Retraction`] event node in the agent-memory domain
//!    recording who retracted the target (`agent_id`), when on the
//!    transaction-time axis (`transaction_time`), why (`text`, passed through
//!    redaction policy v1), and the prior record handle (`source_handle`).
//!
//! Retraction is refused for deterministic code-graph facts (`codegraph:`
//! domain: File / Symbol / Import / CALLS edges / Commit / Change and every
//! other scan-derived record); those are reproducible from source and must be
//! corrected with `eg refresh` or a re-scan. It is also refused for tombstones
//! and for retraction events themselves — forgetting the audit trail would
//! turn retraction back into a silent hole.
//!
//! Retraction is logical and bi-temporally honest: the physical record stays
//! in the store, so a transaction-time view predating the retraction still
//! reflects that the record existed then. Re-running `eg forget` on an
//! already-retracted handle is a no-op success returning the original event.

use chrono::Utc;
use serde::Serialize;

use crate::{
    ir::{AGENT_MEMORY_SCHEMA_VERSION, GraphRecord, NodeKind, agent_memory_stable_id},
    redaction::{REDACTION_POLICY_VERSION, is_redacted, redact_value},
    schema_version::domain_from_record_id,
};

/// Stable machine-readable error code for retraction refused on a
/// deterministic code-graph fact.
pub const DETERMINISTIC_CODE_FACT_CODE: &str = "deterministic_code_fact";

/// Request parameters for `eg forget`.
#[derive(Debug, Clone)]
pub struct ForgetRequest {
    /// Stable record ID of the record to retract.
    pub handle: String,
    /// Operator-supplied retraction reason (redaction policy v1 applies).
    pub reason: String,
    /// Operator handle recorded as the retraction actor.
    pub retracted_by: String,
    /// Optional fixed RFC 3339 transaction time for deterministic output.
    /// Defaults to the current wall-clock instant.
    pub transaction_time: Option<String>,
}

/// The auditable retraction event, echoed in the success envelope.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct RetractionEvent {
    /// Stable record ID of the retraction event node.
    pub retraction_id: String,
    /// Stable record ID of the tombstone suppressing the target.
    pub tombstone_id: String,
    /// Stable record ID of the retracted record (the prior record handle).
    pub retracted_record_id: String,
    /// Operator handle recorded as the retraction actor.
    pub retracted_by: String,
    /// RFC 3339 transaction time of the retraction.
    pub retracted_at: String,
    /// Redacted retraction reason.
    pub reason: String,
}

/// Successful retraction outcome.
#[derive(Debug, Clone)]
pub enum ForgetOutcome {
    /// The target was retracted; `records` must be written to the store.
    Retracted {
        /// The auditable retraction event.
        event: RetractionEvent,
        /// Records to persist: the retraction event node, then the tombstone.
        records: Vec<GraphRecord>,
    },
    /// The target was already retracted; nothing to write.
    AlreadyRetracted {
        /// The original auditable retraction event.
        event: RetractionEvent,
    },
}

/// Machine-readable retraction failure.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ForgetError {
    /// `--reason` is empty or whitespace-only.
    MissingReason,
    /// `--retracted-by` is empty or whitespace-only.
    MissingActor,
    /// `--transaction-time` is not a valid RFC 3339 instant.
    InvalidTransactionTime {
        /// The rejected value.
        value: String,
        /// Parse failure detail.
        message: String,
    },
    /// No live record with the requested handle exists in the store.
    NotFound {
        /// The unresolved handle.
        handle: String,
    },
    /// The target is a deterministic code-graph fact; refused.
    DeterministicCodeFact {
        /// Stable record ID of the refused target.
        record_id: String,
        /// Node kind name when the target is a node, `edge` for edges.
        kind: String,
    },
    /// The target is a tombstone or a retraction event; refused.
    UnsupportedTarget {
        /// Stable record ID of the refused target.
        record_id: String,
        /// Why this target class cannot be retracted.
        detail: String,
    },
}

impl ForgetError {
    /// Returns the stable machine-readable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingReason => "missing_reason",
            Self::MissingActor => "missing_actor",
            Self::InvalidTransactionTime { .. } => "invalid_transaction_time",
            Self::NotFound { .. } => "not_found",
            Self::DeterministicCodeFact { .. } => DETERMINISTIC_CODE_FACT_CODE,
            Self::UnsupportedTarget { .. } => "unsupported_target",
        }
    }

    /// Returns the process exit code: 2 for `not_found`, 1 otherwise.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::NotFound { .. } => 2,
            _ => 1,
        }
    }

    /// Returns the machine-readable JSON error envelope.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let detail = match self {
            Self::MissingReason => serde_json::json!({
                "message": "--reason must be a non-empty retraction reason",
            }),
            Self::MissingActor => serde_json::json!({
                "message": "--retracted-by must be a non-empty operator handle",
            }),
            Self::InvalidTransactionTime { value, message } => serde_json::json!({
                "value": value,
                "message": format!("invalid --transaction-time '{value}': {message}"),
            }),
            Self::NotFound { handle } => serde_json::json!({
                "handle": handle,
                "message": format!(
                    "no record with handle '{handle}' exists in the store; \
                     retraction targets one stable record ID"
                ),
            }),
            Self::DeterministicCodeFact { record_id, kind } => serde_json::json!({
                "record_id": record_id,
                "kind": kind,
                "remedy": "eg refresh",
                "message": format!(
                    "'{record_id}' is a deterministic code-graph fact ({kind}); \
                     code facts are reproducible from source and cannot be retracted — \
                     correct them with `eg refresh` or a re-scan"
                ),
            }),
            Self::UnsupportedTarget { record_id, detail } => serde_json::json!({
                "record_id": record_id,
                "message": detail,
            }),
        };
        serde_json::json!({
            "ok": false,
            "error": { "code": self.code(), "detail": detail },
        })
    }
}

/// Builds the deterministic retraction-event record ID for a target handle.
#[must_use]
pub fn retraction_event_id(target_id: &str) -> String {
    agent_memory_stable_id(&["node", "retraction", target_id])
}

/// Builds the deterministic retraction tombstone ID for a target handle.
///
/// The tombstone lives in the same domain (and domain schema version) as the
/// target so reader-side version validation resolves it against the domain the
/// deleted record belongs to.
#[must_use]
pub fn retraction_tombstone_id(target_id: &str) -> (String, u32) {
    let (domain, version) = target_id
        .split_once(":v")
        .and_then(|(domain, rest)| {
            let (version, _) = rest.split_once(':')?;
            Some((domain, version.parse::<u32>().ok()?))
        })
        .unwrap_or(("agent_memory", AGENT_MEMORY_SCHEMA_VERSION));
    let mut hasher = blake3::Hasher::new();
    for part in ["tombstone", "retraction", target_id] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    (
        format!("{domain}:v{version}:{}", hasher.finalize().to_hex()),
        version,
    )
}

/// Validates the request fields shared by every retraction.
fn validate_request(req: &ForgetRequest) -> Result<(), ForgetError> {
    if req.reason.trim().is_empty() {
        return Err(ForgetError::MissingReason);
    }
    if req.retracted_by.trim().is_empty() {
        return Err(ForgetError::MissingActor);
    }
    if let Some(value) = req.transaction_time.as_deref()
        && let Err(error) = chrono::DateTime::parse_from_rfc3339(value)
    {
        return Err(ForgetError::InvalidTransactionTime {
            value: value.to_owned(),
            message: error.to_string(),
        });
    }
    Ok(())
}

/// Refuses target classes that must never be retracted.
fn refuse_unsupported_target(target: &GraphRecord, handle: &str) -> Result<(), ForgetError> {
    // Deterministic code-graph facts are reproducible from source; the
    // correction path is `eg refresh` / a re-scan, never hand deletion.
    if domain_from_record_id(handle).as_deref() == Some("codegraph") {
        let kind = match target {
            GraphRecord::Node { kind, .. } => kind.as_str().to_owned(),
            GraphRecord::Edge { .. } => "edge".to_owned(),
            GraphRecord::Tombstone { .. } => "tombstone".to_owned(),
        };
        return Err(ForgetError::DeterministicCodeFact {
            record_id: handle.to_owned(),
            kind,
        });
    }
    match target {
        GraphRecord::Tombstone { .. } => Err(ForgetError::UnsupportedTarget {
            record_id: handle.to_owned(),
            detail: format!(
                "'{handle}' is a tombstone; deletion markers cannot themselves be retracted"
            ),
        }),
        GraphRecord::Node {
            kind: NodeKind::Retraction,
            ..
        } => Err(ForgetError::UnsupportedTarget {
            record_id: handle.to_owned(),
            detail: format!(
                "'{handle}' is a retraction event; forgetting the audit trail would \
                 turn retraction into a silent hole"
            ),
        }),
        GraphRecord::Node { .. } | GraphRecord::Edge { .. } => Ok(()),
    }
}

/// Builds the citable retraction event node for a resolved retraction.
fn build_event_node(handle: &str, event: &RetractionEvent) -> GraphRecord {
    let mut event_node = GraphRecord::node(
        event.retraction_id.clone(),
        NodeKind::Retraction,
        None,
        None,
        None,
        format!("Retraction event for {handle}"),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut text,
        ref mut agent_id,
        ref mut transaction_time,
        ref mut source_handle,
        ref mut valid_time,
        ref mut valid_time_source,
        ref mut redaction_policy_version,
        ..
    } = event_node
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *text = Some(event.reason.clone());
        *agent_id = Some(event.retracted_by.clone());
        *transaction_time = Some(event.retracted_at.clone());
        *source_handle = Some(handle.to_owned());
        *valid_time = Some(event.retracted_at.clone());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        if is_redacted(&event.reason) || is_redacted(&event.retracted_by) {
            *redaction_policy_version = Some(REDACTION_POLICY_VERSION.to_owned());
        }
    }
    event_node
}

/// Resolves a retraction request against the current store view.
///
/// `records` is the transaction-time-current read (`read_all_records`); the
/// caller persists the returned records through the adapter on
/// [`ForgetOutcome::Retracted`].
///
/// # Errors
///
/// Returns a machine-readable [`ForgetError`] when the request is malformed,
/// the handle does not resolve, or the target class is refused.
pub fn retract_from_records(
    records: &[GraphRecord],
    req: &ForgetRequest,
) -> Result<ForgetOutcome, ForgetError> {
    validate_request(req)?;

    let handle = req.handle.trim();
    let retraction_id = retraction_event_id(handle);

    // Idempotency wins before any other resolution: once a retraction event
    // exists for this handle, re-running is a no-op success returning the
    // original event — even though the retracted target is no longer visible
    // in the current-state read.
    if let Some(GraphRecord::Node {
        kind: NodeKind::Retraction,
        text,
        agent_id,
        transaction_time,
        ..
    }) = records.iter().rfind(|record| record.id() == retraction_id)
    {
        let event = RetractionEvent {
            retraction_id,
            tombstone_id: retraction_tombstone_id(handle).0,
            retracted_record_id: handle.to_owned(),
            retracted_by: agent_id.clone().unwrap_or_default(),
            retracted_at: transaction_time.clone().unwrap_or_default(),
            reason: text.clone().unwrap_or_default(),
        };
        return Ok(ForgetOutcome::AlreadyRetracted { event });
    }

    let Some(target) = records.iter().rfind(|record| record.id() == handle) else {
        return Err(ForgetError::NotFound {
            handle: handle.to_owned(),
        });
    };
    refuse_unsupported_target(target, handle)?;

    let retracted_at = req
        .transaction_time
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
    let (tombstone_id, tombstone_version) = retraction_tombstone_id(handle);
    let event = RetractionEvent {
        retraction_id: retraction_id.clone(),
        tombstone_id: tombstone_id.clone(),
        retracted_record_id: handle.to_owned(),
        retracted_by: redact_value(req.retracted_by.trim()),
        retracted_at,
        reason: redact_value(req.reason.trim()),
    };

    let event_node = build_event_node(handle, &event);
    let tombstone = GraphRecord::Tombstone {
        id: tombstone_id,
        schema_version: tombstone_version,
        deleted_id: handle.to_owned(),
        summary: format!("Operator retraction of {handle}; see retraction event {retraction_id}"),
        producer: None,
    };
    Ok(ForgetOutcome::Retracted {
        event,
        // The tombstone is written after the event node so the deletion is the
        // latest write for the target and stays active.
        records: vec![event_node, tombstone],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{EdgeLabel, EvidenceLink, SCHEMA_VERSION, stable_id};

    const TX: &str = "2026-07-01T00:00:00Z";

    fn observation(id: &str, text: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            Some("obs".to_owned()),
            "agent observation".to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            text: node_text,
            agent_id,
            session_id,
            observed_at,
            evidence_links,
            ..
        } = &mut node
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *node_text = Some(text.to_owned());
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some("sess-1".to_owned());
            *observed_at = Some("2026-06-01T00:00:00Z".to_owned());
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some(symbol_id()),
                target_domain: "codegraph".to_owned(),
                relation: "MENTIONS_SYMBOL".to_owned(),
                confidence: "0.9".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: Some("src/lib.rs".to_owned()),
                target_span: None,
                target_git_commit: None,
            }]);
        }
        node
    }

    fn symbol_id() -> String {
        stable_id(&["node", "symbol", "repo", "src/lib.rs", "fn", "parse", "0"])
    }

    fn symbol() -> GraphRecord {
        GraphRecord::node(
            symbol_id(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("parse".to_owned()),
            "fn parse".to_owned(),
        )
    }

    fn calls_edge_id() -> String {
        stable_id(&["edge", "calls", "a", "b"])
    }

    fn calls_edge() -> GraphRecord {
        GraphRecord::Edge {
            id: calls_edge_id(),
            schema_version: SCHEMA_VERSION,
            label: EdgeLabel::Calls,
            source: symbol_id(),
            target: symbol_id(),
            confidence: None,
            resolution: None,
            temporal: None,
            summary: "call edge".to_owned(),
            producer: None,
        }
    }

    fn obs_id() -> String {
        agent_memory_stable_id(&["node", "observation", "sess-1", "0"])
    }

    fn request(handle: &str) -> ForgetRequest {
        ForgetRequest {
            handle: handle.to_owned(),
            reason: "false claim about parser behavior".to_owned(),
            retracted_by: "op-1".to_owned(),
            transaction_time: Some(TX.to_owned()),
        }
    }

    fn seeded() -> Vec<GraphRecord> {
        vec![
            symbol(),
            calls_edge(),
            observation(&obs_id(), "the parser skips empty input"),
        ]
    }

    #[test]
    fn retraction_generates_event_node_and_tombstone() {
        let records = seeded();
        let outcome = retract_from_records(&records, &request(&obs_id())).expect("retracts");
        let ForgetOutcome::Retracted {
            event,
            records: generated,
        } = outcome
        else {
            panic!("expected Retracted outcome");
        };

        assert_eq!(event.retracted_record_id, obs_id());
        assert_eq!(event.retracted_by, "op-1");
        assert_eq!(event.retracted_at, TX);
        assert_eq!(event.reason, "false claim about parser behavior");
        assert_eq!(event.retraction_id, retraction_event_id(&obs_id()));
        assert_eq!(event.tombstone_id, retraction_tombstone_id(&obs_id()).0);

        assert_eq!(
            generated.len(),
            2,
            "exactly one event node and one tombstone"
        );
        let GraphRecord::Node {
            id,
            kind,
            schema_version,
            domain,
            text,
            agent_id,
            transaction_time,
            source_handle,
            ..
        } = &generated[0]
        else {
            panic!("first generated record must be the retraction event node");
        };
        assert_eq!(id, &event.retraction_id);
        assert_eq!(*kind, NodeKind::Retraction);
        assert_eq!(*schema_version, AGENT_MEMORY_SCHEMA_VERSION);
        assert_eq!(domain.as_deref(), Some("agent_memory"));
        assert_eq!(text.as_deref(), Some("false claim about parser behavior"));
        assert_eq!(agent_id.as_deref(), Some("op-1"));
        assert_eq!(transaction_time.as_deref(), Some(TX));
        assert_eq!(source_handle.as_deref(), Some(obs_id().as_str()));

        let GraphRecord::Tombstone {
            id,
            deleted_id,
            schema_version,
            ..
        } = &generated[1]
        else {
            panic!("second generated record must be the tombstone");
        };
        assert_eq!(id, &event.tombstone_id);
        assert_eq!(deleted_id, &obs_id());
        assert_eq!(*schema_version, AGENT_MEMORY_SCHEMA_VERSION);
        assert!(
            event.tombstone_id.starts_with("agent_memory:v1:"),
            "tombstone stays in the target's domain: {}",
            event.tombstone_id
        );
    }

    #[test]
    fn retraction_is_deterministic_across_runs() {
        let records = seeded();
        let first = retract_from_records(&records, &request(&obs_id())).expect("retracts");
        let second = retract_from_records(&records, &request(&obs_id())).expect("retracts");
        let (
            ForgetOutcome::Retracted {
                event: e1,
                records: r1,
            },
            ForgetOutcome::Retracted {
                event: e2,
                records: r2,
            },
        ) = (first, second)
        else {
            panic!("expected Retracted outcomes");
        };
        assert_eq!(e1, e2);
        assert_eq!(r1, r2);
    }

    #[test]
    fn refuses_deterministic_code_fact_node() {
        let records = seeded();
        let err = retract_from_records(&records, &request(&symbol_id()))
            .expect_err("code facts are refused");
        assert_eq!(err.code(), "deterministic_code_fact");
        assert_eq!(err.exit_code(), 1);
        let json = err.to_json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "deterministic_code_fact");
        assert_eq!(json["error"]["detail"]["kind"], "Symbol");
        assert_eq!(json["error"]["detail"]["remedy"], "eg refresh");
        let message = json["error"]["detail"]["message"]
            .as_str()
            .expect("message");
        assert!(
            message.contains("eg refresh"),
            "refusal names the remedy: {message}"
        );
        assert!(
            message.contains("re-scan"),
            "refusal names re-scan: {message}"
        );
    }

    #[test]
    fn refuses_deterministic_code_fact_edge() {
        let records = seeded();
        let err = retract_from_records(&records, &request(&calls_edge_id()))
            .expect_err("code-graph CALLS edges are refused");
        assert_eq!(err.code(), "deterministic_code_fact");
        assert_eq!(err.to_json()["error"]["detail"]["kind"], "edge");
    }

    #[test]
    fn unknown_handle_is_not_found_with_exit_2() {
        let records = seeded();
        let err = retract_from_records(&records, &request("agent_memory:v1:doesnotexist"))
            .expect_err("unknown handles fail");
        assert_eq!(err.code(), "not_found");
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.to_json()["ok"], false);
    }

    #[test]
    fn second_retraction_is_idempotent_no_op() {
        let mut records = seeded();
        let outcome = retract_from_records(&records, &request(&obs_id())).expect("retracts");
        let ForgetOutcome::Retracted {
            event: original,
            records: generated,
        } = outcome
        else {
            panic!("expected Retracted outcome");
        };
        records.extend(generated);

        let mut rerun = request(&obs_id());
        // A different wall-clock instant and reason on the re-run must not
        // produce a second event: the original event wins.
        rerun.transaction_time = Some("2026-07-02T09:00:00Z".to_owned());
        rerun.reason = "different reason".to_owned();
        let second = retract_from_records(&records, &rerun).expect("idempotent success");
        let ForgetOutcome::AlreadyRetracted { event } = second else {
            panic!("expected AlreadyRetracted outcome");
        };
        assert_eq!(event, original, "no duplicate event; original preserved");
    }

    #[test]
    fn reason_passes_through_redaction() {
        let records = seeded();
        let mut req = request(&obs_id());
        req.reason =
            "observation leaked the key sk-ant-api03-0123456789abcdefghijklmnop".to_owned();
        let outcome = retract_from_records(&records, &req).expect("retracts");
        let ForgetOutcome::Retracted {
            event,
            records: generated,
        } = outcome
        else {
            panic!("expected Retracted outcome");
        };
        assert!(
            !event
                .reason
                .contains("sk-ant-api03-0123456789abcdefghijklmnop"),
            "raw secret must not survive into the event: {}",
            event.reason
        );
        assert!(
            event.reason.contains("<REDACTED:"),
            "redaction marker expected"
        );
        let GraphRecord::Node {
            text,
            redaction_policy_version,
            ..
        } = &generated[0]
        else {
            panic!("expected retraction node");
        };
        assert_eq!(text.as_deref(), Some(event.reason.as_str()));
        assert_eq!(
            redaction_policy_version.as_deref(),
            Some(REDACTION_POLICY_VERSION)
        );
    }

    #[test]
    fn refuses_tombstone_and_retraction_event_targets() {
        let mut records = seeded();
        let ForgetOutcome::Retracted {
            event,
            records: generated,
        } = retract_from_records(&records, &request(&obs_id())).expect("retracts")
        else {
            panic!("expected Retracted outcome");
        };
        records.extend(generated);

        let err = retract_from_records(&records, &request(&event.tombstone_id))
            .expect_err("tombstones are refused");
        assert_eq!(err.code(), "unsupported_target");

        let err = retract_from_records(&records, &request(&event.retraction_id))
            .expect_err("retraction events are refused");
        assert_eq!(err.code(), "unsupported_target");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn malformed_requests_are_rejected() {
        let records = seeded();

        let mut req = request(&obs_id());
        req.reason = "   ".to_owned();
        assert_eq!(
            retract_from_records(&records, &req)
                .expect_err("empty reason")
                .code(),
            "missing_reason"
        );

        let mut req = request(&obs_id());
        req.retracted_by = String::new();
        assert_eq!(
            retract_from_records(&records, &req)
                .expect_err("empty actor")
                .code(),
            "missing_actor"
        );

        let mut req = request(&obs_id());
        req.transaction_time = Some("yesterday".to_owned());
        assert_eq!(
            retract_from_records(&records, &req)
                .expect_err("bad timestamp")
                .code(),
            "invalid_transaction_time"
        );
    }

    #[test]
    fn citing_records_are_never_deleted() {
        // A second observation citing the retracted one stays untouched:
        // retraction generates only the event node and the tombstone.
        let mut records = seeded();
        let citing_id = agent_memory_stable_id(&["node", "observation", "sess-1", "1"]);
        let mut citing = observation(&citing_id, "builds on the retracted claim");
        if let GraphRecord::Node { evidence_links, .. } = &mut citing {
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some(obs_id()),
                target_domain: "agent_memory".to_owned(),
                relation: "RELATES_TO".to_owned(),
                confidence: "0.8".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
        records.push(citing);

        let ForgetOutcome::Retracted {
            records: generated, ..
        } = retract_from_records(&records, &request(&obs_id())).expect("retracts")
        else {
            panic!("expected Retracted outcome");
        };
        assert_eq!(generated.len(), 2);
        assert!(
            generated.iter().all(|r| r.id() != citing_id),
            "the citing record is neither rewritten nor tombstoned"
        );
        assert!(
            !generated
                .iter()
                .any(|r| matches!(r, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == &citing_id)),
            "no cascade tombstone for citing records"
        );
    }
}
