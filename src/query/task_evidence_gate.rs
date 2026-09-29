//! Task completion evidence gate (issue #147).
//!
//! Pure, read-only, deterministic: for one task, every linked
//! `AcceptanceCriterion` is checked against the verification records that
//! close it. A criterion is satisfied only by a **passing** verification
//! record — the shared [`crate::query::trust::verification_outcome`] rule —
//! whose cited code has **not drifted** since the run, reusing the issue #111
//! drift signal ([`crate::verification_freshness`]). The task verdict is
//! `ready` only when every criterion is satisfied.
//!
//! Trust separation: the gate reads recorded evidence only. It never executes
//! tests, proofs, or commands, never re-judges a recorded `status`, and never
//! mutates the task. A `ready: true` verdict is a completion-gating lead —
//! "every acceptance criterion has live passing evidence" — never a claim the
//! task is correct, safe, or complete. Fail-closed throughout: evidence that
//! cannot be confirmed live (drifted, removed/renamed, or unanchored) does not
//! satisfy a criterion, and any linked record whose outcome is not passing
//! blocks the gate even when other records pass.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::liveness::Liveness;
use super::trust::{VerificationOutcome, verification_outcome};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};
use crate::verification_freshness::{
    FRESHNESS_LEAD, TriggeringHandle, VerificationFreshnessEntry, VerificationFreshnessVerdict,
    is_verification_kind, verification_freshness,
};

/// Why a criterion does not satisfy the gate (closed vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotReadyReason {
    /// No live verification record is linked to the criterion.
    MissingEvidence,
    /// At least one linked record's recorded outcome is not passing.
    FailingEvidence,
    /// Every linked passing record's cited code drifted since the run, was
    /// removed/renamed, or cannot be anchored — no confirmed-live evidence.
    StaleEvidence,
}

impl NotReadyReason {
    /// The stable snake_case reason used in JSON output and text rendering.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingEvidence => "missing_evidence",
            Self::FailingEvidence => "failing_evidence",
            Self::StaleEvidence => "stale_evidence",
        }
    }
}

/// The gated task's identity row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateTaskRow {
    /// Stable record ID of the task.
    pub record_id: String,
    /// Human-readable title, when the record carries one.
    pub title: Option<String>,
    /// Author-written status string, when the record carries one.
    pub status: Option<String>,
}

/// One cited code handle backing a verification record, with its own #111
/// freshness verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceGateCitation {
    /// Stable record ID of the cited code node, when the citation names one.
    pub target_record_id: Option<String>,
    /// Repo-relative path of the cited handle, when known.
    pub repo_relative_path: Option<String>,
    /// Source span of the cited handle, when symbol-level.
    pub span: Option<SourceSpan>,
    /// Cross-domain edge label the citation was derived from
    /// (`FAILED_ON`/`MENTIONS_SYMBOL`/`TOUCHED_FILE`), or the synthetic
    /// `source_artifact` relation.
    pub relation: String,
    /// The #111 freshness verdict for this citation.
    pub verdict: VerificationFreshnessVerdict,
    /// The handle proving the cited code moved (stale/unresolved only).
    pub triggering_handle: Option<TriggeringHandle>,
}

/// One verification record linked to a criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceGateItem {
    /// Stable record ID of the verification record.
    pub record_id: String,
    /// Reported verification kind (`test_run`, `ci_status`, …).
    pub verification_kind: String,
    /// Recorded `status`, echoed verbatim — never reinterpreted.
    pub status: Option<String>,
    /// Whether the recorded outcome counts as passing under the shared
    /// [`verification_outcome`] rule.
    pub pass: bool,
    /// Freshness of the record's cited code basis, rolled up worst-case over
    /// its citations: any `stale`/`unresolved` citation ⇒ `stale`; else any
    /// `unanchored` ⇒ `unanchored`; else `current`. A record citing no code
    /// is `current` by vacuity.
    pub freshness: VerificationFreshnessVerdict,
    /// Freshness-lead text on stale/unresolved records; absent otherwise.
    pub freshness_lead: Option<&'static str>,
    /// The cited code handles, each carrying its own verdict.
    pub citations: Vec<EvidenceGateCitation>,
}

/// The gate row for one acceptance criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CriterionGateRow {
    /// Stable `AcceptanceCriterion` record ID (always cited).
    pub record_id: String,
    /// Position within the parent task's AC list, when recorded.
    pub ordinal: Option<u32>,
    /// The criterion text, when the record carries it.
    pub text: Option<String>,
    /// Whether a passing, non-drifted verification record backs this criterion.
    pub satisfied: bool,
    /// Why the criterion is not satisfied; absent when satisfied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_ready_reason: Option<NotReadyReason>,
    /// The linked live verification records, ascending record ID.
    pub evidence: Vec<EvidenceGateItem>,
}

/// Counts for the evidence-gate report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskEvidenceGateCounts {
    /// Linked acceptance criteria seen.
    pub criteria: usize,
    /// Criteria with a passing, non-drifted verification record.
    pub satisfied: usize,
    /// Criteria without one.
    pub unsatisfied: usize,
    /// Live verification records linked across all criteria.
    pub evidence_records: usize,
}

/// The evidence-gate verdict for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskEvidenceGateReport {
    /// The gated task's identity row.
    pub task: GateTaskRow,
    /// Top-level verdict: true only when every criterion is satisfied.
    /// Vacuously true when the task has no acceptance criteria.
    pub ready: bool,
    /// Per-criterion rows, ordered by (ordinal, record ID).
    pub criteria: Vec<CriterionGateRow>,
    /// Aggregate counts.
    pub counts: TaskEvidenceGateCounts,
}

/// Outcome of evidence-gate resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvidenceGateOutcome {
    /// The task resolved; the report carries the verdict (ready or not).
    Ready(TaskEvidenceGateReport),
    /// No live `Task` record with that ID exists.
    NoSuchTask,
}

/// CLI exit-code contract (issue #147).
///
/// `0` = a verdict was produced (including `ready: false`), `2` = no such
/// task. Malformed input / read failures are `1`, decided by the CLI.
#[must_use]
pub const fn task_evidence_gate_exit_code(outcome: &TaskEvidenceGateOutcome) -> i32 {
    match outcome {
        TaskEvidenceGateOutcome::Ready(_) => 0,
        TaskEvidenceGateOutcome::NoSuchTask => 2,
    }
}

/// Drift severity for the worst-case freshness rollup. Higher is worse;
/// anything above `current` fails the gate.
const fn freshness_severity(verdict: VerificationFreshnessVerdict) -> u8 {
    match verdict {
        VerificationFreshnessVerdict::Current => 0,
        VerificationFreshnessVerdict::Unanchored => 1,
        VerificationFreshnessVerdict::Unresolved => 2,
        VerificationFreshnessVerdict::Stale => 3,
    }
}

/// Roll up a record's per-citation #111 verdicts to one freshness flag:
/// worst citation wins. The freshness-lead text rides on stale/unresolved
/// only, mirroring the #111 module's own contract.
fn rollup_freshness(
    entries: &[&VerificationFreshnessEntry],
) -> (VerificationFreshnessVerdict, Option<&'static str>) {
    let mut worst = VerificationFreshnessVerdict::Current;
    for entry in entries {
        if freshness_severity(entry.verdict) > freshness_severity(worst) {
            worst = entry.verdict;
        }
    }
    let lead = if worst.is_stale() {
        Some(FRESHNESS_LEAD)
    } else {
        None
    };
    (worst, lead)
}

/// Closed fallback for a verification record's kind label when the record
/// carries no `verification_kind` field of its own.
const fn verification_kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::TestRun => "test_run",
        NodeKind::CommandRun => "command_run",
        NodeKind::CIStatus => "ci_status",
        NodeKind::BenchmarkRun => "benchmark_run",
        NodeKind::CoverageReport => "coverage_report",
        NodeKind::ProofResult => "proof_result",
        NodeKind::Verification => "verification",
        _ => "unknown",
    }
}

/// Resolve the evidence-gate verdict for one task record ID.
///
/// `task_id` is the resolved canonical task ID (handle dereferencing is the
/// CLI's job, reusing `crate::query::task_evidence_gate`'s sibling
/// `resolve_task_ids`). Read-only and deterministic: criteria are ordered by
/// (ordinal, record ID), evidence by record ID, and the #111 verdicts are
/// computed over the unchanged store, so repeated runs serialize
/// byte-identically.
pub fn task_evidence_gate_report(
    records: &[GraphRecord],
    task_id: &str,
) -> TaskEvidenceGateOutcome {
    let liveness = Liveness::new(records);

    // ── Latest live node version per record ID ──────────────────────────
    let mut live_nodes: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if let GraphRecord::Node { id, .. } = record
            && !liveness.deleted(id.as_str())
            && liveness.is_latest_node_version(id.as_str(), index)
        {
            live_nodes.insert(id.as_str(), record);
        }
    }
    // ── Latest live edges for the two relations this lane follows ───────
    // `closes`: AC -> verification IDs via CLOSES_ACCEPTANCE_CRITERION.
    // `owned`: task -> AC IDs via OWNED_BY_TASK.
    let mut closes: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut owned: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if liveness.deleted(id.as_str()) || !liveness.is_latest_edge_version(id.as_str(), index) {
            continue;
        }
        match label {
            EdgeLabel::ClosesAcceptanceCriterion => {
                closes
                    .entry(source.as_str())
                    .or_default()
                    .insert(target.as_str());
            }
            EdgeLabel::OwnedByTask => {
                owned
                    .entry(target.as_str())
                    .or_default()
                    .insert(source.as_str());
            }
            _ => {}
        }
    }

    // ── The task ────────────────────────────────────────────────────────
    let task_record = match live_nodes.get(task_id) {
        Some(
            node @ GraphRecord::Node {
                kind: NodeKind::Task,
                ..
            },
        ) => *node,
        _ => return TaskEvidenceGateOutcome::NoSuchTask,
    };
    let GraphRecord::Node { title, status, .. } = task_record else {
        return TaskEvidenceGateOutcome::NoSuchTask;
    };

    // ── The task's acceptance criteria ──────────────────────────────────
    // By `parent_task_id` field, or by live OWNED_BY_TASK edge (AC -> task).
    let mut criterion_ids: BTreeSet<&str> = BTreeSet::new();
    for (id, node) in &live_nodes {
        if let GraphRecord::Node {
            kind: NodeKind::AcceptanceCriterion,
            parent_task_id: Some(parent),
            ..
        } = node
            && parent.as_str() == task_id
        {
            criterion_ids.insert(id);
        }
    }
    if let Some(ac_ids) = owned.get(task_id) {
        for ac_id in ac_ids {
            if let Some(GraphRecord::Node {
                kind: NodeKind::AcceptanceCriterion,
                ..
            }) = live_nodes.get(ac_id)
            {
                criterion_ids.insert(ac_id);
            }
        }
    }

    // ── #111 drift verdicts, computed once for the whole store ──────────
    // No `--repo-path`: the artifact-hash trigger needs a filesystem root and
    // this lane stays store-local and read-only.
    let freshness_entries = verification_freshness(records, None, None);
    let mut entries_by_record: BTreeMap<&str, Vec<&VerificationFreshnessEntry>> = BTreeMap::new();
    for entry in &freshness_entries {
        entries_by_record
            .entry(entry.verification_record_id.as_str())
            .or_default()
            .push(entry);
    }

    // ── Per-criterion gate rows ─────────────────────────────────────────
    let mut criteria: Vec<CriterionGateRow> = Vec::new();
    for ac_id in criterion_ids {
        let Some(ac_node) = live_nodes.get(ac_id) else {
            continue;
        };
        let GraphRecord::Node {
            ordinal,
            text,
            verification_link_id,
            ..
        } = ac_node
        else {
            continue;
        };

        // Linked live verification records: the CLOSES_ACCEPTANCE_CRITERION
        // edge (AC -> Verification) or the denormalized `verification_link_id`
        // field. Tombstoned / non-verification targets are not evidence.
        let mut verification_ids: BTreeSet<&str> = BTreeSet::new();
        if let Some(targets) = closes.get(ac_id) {
            verification_ids.extend(targets.iter().copied());
        }
        if let Some(linked) = verification_link_id {
            verification_ids.insert(linked.as_str());
        }
        let mut evidence: Vec<EvidenceGateItem> = Vec::new();
        for verification_id in verification_ids {
            let Some(verification_node) = live_nodes.get(verification_id) else {
                continue;
            };
            let GraphRecord::Node {
                kind,
                domain,
                verification_kind,
                status: record_status,
                ..
            } = verification_node
            else {
                continue;
            };
            if !is_verification_kind(verification_id, *kind, domain.as_deref()) {
                continue;
            }
            let pass = verification_outcome(verification_node) == VerificationOutcome::Passing;
            let empty: Vec<&VerificationFreshnessEntry> = Vec::new();
            let cited = entries_by_record
                .get(verification_id)
                .map_or(empty.as_slice(), Vec::as_slice);
            let (freshness, freshness_lead) = rollup_freshness(cited);
            let citations: Vec<EvidenceGateCitation> = cited
                .iter()
                .map(|entry| EvidenceGateCitation {
                    target_record_id: entry.cited_handle.target_record_id.clone(),
                    repo_relative_path: entry.cited_handle.repo_relative_path.clone(),
                    span: entry.cited_handle.span,
                    relation: entry.cited_handle.relation.clone(),
                    verdict: entry.verdict,
                    triggering_handle: entry.triggering_handle.clone(),
                })
                .collect();
            evidence.push(EvidenceGateItem {
                record_id: verification_id.to_owned(),
                verification_kind: verification_kind
                    .clone()
                    .unwrap_or_else(|| verification_kind_name(*kind).to_owned()),
                status: record_status.clone(),
                pass,
                freshness,
                freshness_lead,
                citations,
            });
        }

        // Gate logic, fail-closed. A recorded non-passing outcome dominates:
        // it is hard evidence against completion even when other records pass.
        let (satisfied, not_ready_reason) = if evidence.is_empty() {
            (false, Some(NotReadyReason::MissingEvidence))
        } else if evidence.iter().any(|item| !item.pass) {
            (false, Some(NotReadyReason::FailingEvidence))
        } else if evidence
            .iter()
            .any(|item| item.pass && item.freshness == VerificationFreshnessVerdict::Current)
        {
            (true, None)
        } else {
            (false, Some(NotReadyReason::StaleEvidence))
        };
        criteria.push(CriterionGateRow {
            record_id: (*ac_id).to_owned(),
            ordinal: *ordinal,
            text: text.clone(),
            satisfied,
            not_ready_reason,
            evidence,
        });
    }
    // Deterministic order: ordinal first, then record ID.
    criteria.sort_by(|a, b| {
        (a.ordinal.unwrap_or(u32::MAX), a.record_id.as_str())
            .cmp(&(b.ordinal.unwrap_or(u32::MAX), b.record_id.as_str()))
    });

    let satisfied = criteria.iter().filter(|row| row.satisfied).count();
    let report = TaskEvidenceGateReport {
        task: GateTaskRow {
            record_id: task_id.to_owned(),
            title: title.clone(),
            status: status.clone(),
        },
        ready: criteria.iter().all(|row| row.satisfied),
        counts: TaskEvidenceGateCounts {
            criteria: criteria.len(),
            satisfied,
            unsatisfied: criteria.len() - satisfied,
            evidence_records: criteria.iter().map(|row| row.evidence.len()).sum(),
        },
        criteria,
    };
    TaskEvidenceGateOutcome::Ready(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        EdgeLabel, EmbeddingModel, GraphRecord, MetricKind, NodeKind, SelectionBasis,
        SemanticDriftMetadata, SourceSpan, VERIFICATION_SCHEMA_VERSION, project_stable_id,
        verification_stable_id,
    };
    use crate::query::task_evidence_gate::{
        NotReadyReason, TaskEvidenceGateOutcome, task_evidence_gate_exit_code,
        task_evidence_gate_report,
    };
    use crate::verification_freshness::VerificationFreshnessVerdict;

    const fn span(start_line: usize, end_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line,
            end_line,
            start_column: None,
            end_column: None,
        }
    }

    fn task_node(id: &str, title: &str, status: &str) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            NodeKind::Task,
            None,
            None,
            Some("t1".to_owned()),
            format!("Task: {title}"),
        );
        if let GraphRecord::Node {
            title: t,
            status: s,
            ..
        } = &mut rec
        {
            *t = Some(title.to_owned());
            *s = Some(status.to_owned());
        }
        rec
    }

    fn ac_node(id: &str, task_id: &str, ordinal: u32, text: &str) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            NodeKind::AcceptanceCriterion,
            None,
            None,
            Some("ac1".to_owned()),
            format!("AcceptanceCriterion: {text}"),
        );
        if let GraphRecord::Node {
            parent_task_id: p,
            ordinal: o,
            text: t,
            ..
        } = &mut rec
        {
            *p = Some(task_id.to_owned());
            *o = Some(ordinal);
            *t = Some(text.to_owned());
        }
        rec
    }

    fn ver_run(slug: &str, status: &str, executed_at: Option<&str>) -> (String, GraphRecord) {
        let id = verification_stable_id(&["verification", slug]);
        let mut rec = GraphRecord::node(
            id.clone(),
            NodeKind::TestRun,
            None,
            None,
            None,
            format!("verification {slug}"),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            status: st,
            executed_at: ea,
            ..
        } = &mut rec
        {
            *schema_version = VERIFICATION_SCHEMA_VERSION;
            *domain = Some("verification".to_owned());
            *st = Some(status.to_owned());
            *ea = executed_at.map(str::to_owned);
        }
        (id, rec)
    }

    fn closes_edge(ac_id: &str, ver_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::ClosesAcceptanceCriterion,
            ac_id.to_owned(),
            ver_id.to_owned(),
            None,
            "closes".to_owned(),
        )
    }

    fn code_file(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some(path.to_owned()),
            Some(span(1, 100)),
            Some(path.to_owned()),
            format!("Source file {path}"),
        )
    }

    fn cite_edge(ver_id: &str, file_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::TouchedFile,
            ver_id.to_owned(),
            file_id.to_owned(),
            None,
            "cites".to_owned(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn drift_record(
        drift_id: &str,
        prior_id: &str,
        target_id: &str,
        before_vt: &str,
        after_vt: &str,
    ) -> GraphRecord {
        let drift = SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "p".to_owned(),
                name: "m".to_owned(),
                version: "v".to_owned(),
                dim: 8,
                content_hash: "h".to_owned(),
            },
            target_record_id: target_id.to_owned(),
            prior_record_id: prior_id.to_owned(),
            before_git_commit: "before".to_owned(),
            after_git_commit: "after".to_owned(),
            before_valid_time: before_vt.to_owned(),
            after_valid_time: after_vt.to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score: 0.7,
            selection_threshold: 0.2,
            selection_basis: SelectionBasis::ThresholdOnly,
        };
        GraphRecord::node(
            drift_id.to_owned(),
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "semantic drift".to_owned(),
        )
        .with_domain("semantic", crate::ir::SEMANTIC_SCHEMA_VERSION)
        .with_semantic_drift(drift)
    }

    fn tombstone_of(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("tombstone:{deleted_id}"),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "deleted".to_owned(),
            producer: None,
        }
    }

    fn ready_report(records: &[GraphRecord], task_id: &str) -> TaskEvidenceGateReport {
        match task_evidence_gate_report(records, task_id) {
            TaskEvidenceGateOutcome::Ready(report) => report,
            TaskEvidenceGateOutcome::NoSuchTask => {
                panic!("expected a verdict for task {task_id}, got NoSuchTask")
            }
        }
    }

    /// AC9: unknown task id yields NoSuchTask, whose exit code is 2.
    #[test]
    fn unknown_task_yields_no_such_task_with_exit_code_2() {
        let records: Vec<GraphRecord> = Vec::new();
        let outcome = task_evidence_gate_report(&records, "project:v1:does-not-exist");
        assert_eq!(outcome, TaskEvidenceGateOutcome::NoSuchTask);
        assert_eq!(task_evidence_gate_exit_code(&outcome), 2);
    }

    /// AC3: a criterion with no linked verification evidence blocks completion
    /// with reason `missing_evidence`.
    #[test]
    fn criterion_without_evidence_is_missing_evidence() {
        let task_id = project_stable_id(&["project", "Task", "gate-t1"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t1", "1"]);
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the widget renders"),
        ];

        let report = ready_report(&records, &task_id);
        assert!(
            !report.ready,
            "task with unevidenced criterion is not ready"
        );
        assert_eq!(report.criteria.len(), 1);
        let row = &report.criteria[0];
        assert_eq!(row.record_id, ac_id);
        assert_eq!(row.text.as_deref(), Some("the widget renders"));
        assert!(!row.satisfied);
        assert_eq!(row.not_ready_reason, Some(NotReadyReason::MissingEvidence));
        assert_eq!(
            row.not_ready_reason.map(NotReadyReason::as_str),
            Some("missing_evidence")
        );
        assert!(row.evidence.is_empty());
        assert_eq!(
            task_evidence_gate_exit_code(&TaskEvidenceGateOutcome::Ready(report)),
            0
        );
    }

    /// AC6: every criterion backed by a passing, non-drifted verification
    /// record yields `ready: true` (top-level boolean, AC1).
    #[test]
    fn passing_fresh_evidence_makes_task_ready() {
        let task_id = project_stable_id(&["project", "Task", "gate-t2"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t2", "1"]);
        let (ver_id, ver) = ver_run("gate-pass", "passed", Some("2026-01-01T00:00:00Z"));
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the widget renders"),
            ver,
            closes_edge(&ac_id, &ver_id),
        ];

        let report = ready_report(&records, &task_id);
        assert!(report.ready, "passing non-drifted evidence gates open");
        assert_eq!(report.task.record_id, task_id);
        let row = &report.criteria[0];
        assert!(row.satisfied);
        assert_eq!(row.not_ready_reason, None);
        assert_eq!(row.evidence.len(), 1);
        let item = &row.evidence[0];
        assert_eq!(item.record_id, ver_id);
        assert!(item.pass);
        assert_eq!(item.freshness, VerificationFreshnessVerdict::Current);
        assert_eq!(report.counts.criteria, 1);
        assert_eq!(report.counts.satisfied, 1);
        assert_eq!(report.counts.unsatisfied, 0);
    }

    /// AC5: evidence recording a failing result blocks completion with reason
    /// `failing_evidence`.
    #[test]
    fn failing_evidence_blocks_completion() {
        let task_id = project_stable_id(&["project", "Task", "gate-t3"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t3", "1"]);
        let (ver_id, ver) = ver_run("gate-fail", "failed", Some("2026-01-01T00:00:00Z"));
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the widget renders"),
            ver,
            closes_edge(&ac_id, &ver_id),
        ];

        let report = ready_report(&records, &task_id);
        assert!(!report.ready);
        let row = &report.criteria[0];
        assert!(!row.satisfied);
        assert_eq!(row.not_ready_reason, Some(NotReadyReason::FailingEvidence));
        assert_eq!(
            row.not_ready_reason.map(NotReadyReason::as_str),
            Some("failing_evidence")
        );
        assert!(!row.evidence[0].pass);
        let _ = ver_id;
    }

    /// AC4: passing evidence whose cited code drifted after the run blocks
    /// completion with reason `stale_evidence` (reuses the #111 drift signal).
    #[test]
    fn drifted_evidence_blocks_completion() {
        let task_id = project_stable_id(&["project", "Task", "gate-t4"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t4", "1"]);
        let (ver_id, ver) = ver_run("gate-stale", "passed", Some("2026-01-01T00:00:00Z"));
        let file_id = "codegraph:v1:file-src-widget-rs";
        let drift_id = "semantic:v1:drift-widget";
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the widget renders"),
            ver,
            closes_edge(&ac_id, &ver_id),
            code_file(file_id, "src/widget.rs"),
            cite_edge(&ver_id, file_id),
            drift_record(
                drift_id,
                file_id,
                file_id,
                "2025-06-01T00:00:00Z",
                "2026-06-01T00:00:00Z",
            ),
        ];

        let report = ready_report(&records, &task_id);
        assert!(!report.ready, "drifted evidence must not gate open");
        let row = &report.criteria[0];
        assert!(!row.satisfied);
        assert_eq!(row.not_ready_reason, Some(NotReadyReason::StaleEvidence));
        assert_eq!(
            row.not_ready_reason.map(NotReadyReason::as_str),
            Some("stale_evidence")
        );
        let item = &row.evidence[0];
        assert!(
            item.pass,
            "the recorded result stays passing; only freshness gates"
        );
        assert_eq!(item.freshness, VerificationFreshnessVerdict::Stale);
        assert!(
            item.freshness_lead.is_some(),
            "stale rows carry the lead text"
        );
    }

    /// AC2 + AC7: every evidence item carries agent-citable handles — the
    /// record id, the cited repo-relative path, and the span.
    #[test]
    fn evidence_item_carries_citable_handles() {
        let task_id = project_stable_id(&["project", "Task", "gate-t5"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t5", "1"]);
        let (ver_id, ver) = ver_run("gate-cite", "passed", Some("2026-01-01T00:00:00Z"));
        let file_id = "codegraph:v1:file-src-gadget-rs";
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the gadget renders"),
            ver,
            closes_edge(&ac_id, &ver_id),
            code_file(file_id, "src/gadget.rs"),
            cite_edge(&ver_id, file_id),
        ];

        let report = ready_report(&records, &task_id);
        assert!(report.ready);
        let item = &report.criteria[0].evidence[0];
        assert_eq!(item.record_id, ver_id);
        assert_eq!(item.citations.len(), 1);
        let citation = &item.citations[0];
        assert_eq!(citation.target_record_id.as_deref(), Some(file_id));
        assert_eq!(
            citation.repo_relative_path.as_deref(),
            Some("src/gadget.rs")
        );
        assert_eq!(citation.span, Some(span(1, 100)));
        assert_eq!(citation.verdict, VerificationFreshnessVerdict::Current);
    }

    /// Precedence: a failing linked record dominates a merely stale passing
    /// record — the recorded failure is the stronger signal.
    #[test]
    fn failing_evidence_dominates_stale_passing_record() {
        let task_id = project_stable_id(&["project", "Task", "gate-t6"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t6", "1"]);
        let (stale_id, stale_ver) =
            ver_run("gate-mixed-stale", "passed", Some("2026-01-01T00:00:00Z"));
        let (fail_id, fail_ver) =
            ver_run("gate-mixed-fail", "failed", Some("2026-02-01T00:00:00Z"));
        let file_id = "codegraph:v1:file-src-mixed-rs";
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the mixed thing renders"),
            stale_ver,
            fail_ver,
            closes_edge(&ac_id, &stale_id),
            closes_edge(&ac_id, &fail_id),
            code_file(file_id, "src/mixed.rs"),
            cite_edge(&stale_id, file_id),
            drift_record(
                "semantic:v1:drift-mixed",
                file_id,
                file_id,
                "2025-06-01T00:00:00Z",
                "2026-06-01T00:00:00Z",
            ),
        ];

        let report = ready_report(&records, &task_id);
        assert!(!report.ready);
        let row = &report.criteria[0];
        assert_eq!(row.evidence.len(), 2, "both records are listed");
        assert_eq!(row.not_ready_reason, Some(NotReadyReason::FailingEvidence));
    }

    /// Fail-closed: a passing record whose cited code cannot be anchored
    /// (`unanchored`) is not live evidence — it must not gate open.
    #[test]
    fn unanchored_passing_record_does_not_gate_open() {
        let task_id = project_stable_id(&["project", "Task", "gate-t7"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t7", "1"]);
        let (ver_id, ver) = ver_run("gate-unanchored", "passed", None);
        let file_id = "codegraph:v1:file-src-unanchored-rs";
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the thing renders"),
            ver,
            closes_edge(&ac_id, &ver_id),
            code_file(file_id, "src/unanchored.rs"),
            cite_edge(&ver_id, file_id),
        ];

        let report = ready_report(&records, &task_id);
        assert!(!report.ready, "unanchored evidence is not live evidence");
        let row = &report.criteria[0];
        assert_eq!(row.not_ready_reason, Some(NotReadyReason::StaleEvidence));
        assert_eq!(
            row.evidence[0].freshness,
            VerificationFreshnessVerdict::Unanchored
        );
    }

    /// A tombstoned verification record is not live evidence.
    #[test]
    fn tombstoned_verification_record_is_not_evidence() {
        let task_id = project_stable_id(&["project", "Task", "gate-t8"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t8", "1"]);
        let (ver_id, ver) = ver_run("gate-tomb", "passed", Some("2026-01-01T00:00:00Z"));
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_id, &task_id, 1, "the thing renders"),
            ver,
            tombstone_of(&ver_id),
            closes_edge(&ac_id, &ver_id),
        ];

        let report = ready_report(&records, &task_id);
        assert!(!report.ready);
        let row = &report.criteria[0];
        assert_eq!(row.not_ready_reason, Some(NotReadyReason::MissingEvidence));
        assert!(
            row.evidence.is_empty(),
            "tombstoned record must not be listed"
        );
    }

    /// AC7: deterministic for an unchanged store — byte-identical JSON across runs.
    #[test]
    fn report_is_deterministic_for_unchanged_store() {
        let task_id = project_stable_id(&["project", "Task", "gate-t9"]);
        let ac_b = project_stable_id(&["project", "AcceptanceCriterion", "gate-t9", "b"]);
        let ac_a = project_stable_id(&["project", "AcceptanceCriterion", "gate-t9", "a"]);
        let (ver_id, ver) = ver_run("gate-det", "passed", Some("2026-01-01T00:00:00Z"));
        let records = vec![
            task_node(&task_id, "Gate the thing", "open"),
            ac_node(&ac_b, &task_id, 2, "second"),
            ac_node(&ac_a, &task_id, 1, "first"),
            ver,
            closes_edge(&ac_b, &ver_id),
            closes_edge(&ac_a, &ver_id),
        ];

        let first = ready_report(&records, &task_id);
        let second = ready_report(&records, &task_id);
        let first_json = serde_json::to_string(&first).expect("serialize");
        let second_json = serde_json::to_string(&second).expect("serialize");
        assert_eq!(first_json, second_json, "byte-identical across runs");
        // Criteria are ordered deterministically (by ordinal, then record id).
        assert_eq!(first.criteria[0].text.as_deref(), Some("first"));
        assert_eq!(first.criteria[1].text.as_deref(), Some("second"));
        assert!(first.ready);
    }

    /// A task with no acceptance criteria is vacuously ready.
    #[test]
    fn task_with_no_criteria_is_vacuously_ready() {
        let task_id = project_stable_id(&["project", "Task", "gate-t10"]);
        let records = vec![task_node(&task_id, "Gate the thing", "open")];

        let report = ready_report(&records, &task_id);
        assert!(report.ready, "no criteria means nothing to gate on");
        assert!(report.criteria.is_empty());
        assert_eq!(report.counts.criteria, 0);
    }

    /// The denormalized `verification_link_id` on the AC node closes the
    /// criterion exactly like a `CLOSES_ACCEPTANCE_CRITERION` edge.
    #[test]
    fn verification_link_id_field_closes_criterion() {
        let task_id = project_stable_id(&["project", "Task", "gate-t11"]);
        let ac_id = project_stable_id(&["project", "AcceptanceCriterion", "gate-t11", "1"]);
        let (ver_id, ver) = ver_run("gate-linkid", "passed", Some("2026-01-01T00:00:00Z"));
        let mut ac = ac_node(&ac_id, &task_id, 1, "the thing renders");
        if let GraphRecord::Node {
            verification_link_id: v,
            ..
        } = &mut ac
        {
            *v = Some(ver_id);
        }
        let records = vec![task_node(&task_id, "Gate the thing", "open"), ac, ver];

        let report = ready_report(&records, &task_id);
        assert!(
            report.ready,
            "verification_link_id must close like the edge"
        );
        assert_eq!(report.criteria[0].evidence.len(), 1);
    }
}
