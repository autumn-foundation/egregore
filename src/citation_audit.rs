//! Citation-completeness audit over Egregore's public query workflows (issue #65).
//!
//! This module is a **measurement layer** over existing query, evidence-link,
//! redaction, protected-artifact, project, verification, and user-context
//! contracts. It drives every public query workflow over a seeded local record
//! set, classifies each returned row by trust class, and reports — per workflow
//! and overall — whether the row carries the citation handles its trust class
//! requires. It introduces **no** new graph domain, trust model, edge
//! vocabulary, hosted service, or LLM-generated answer (issue #65 AC11).
//!
//! The audit is pure and deterministic: [`run_citation_audit`] never prints,
//! never exits, performs no I/O, and emits canonically-ordered output so the
//! same seeded record set yields byte-identical reports across runs (AC9).
//!
//! Output is redaction-safe (AC8): rows and diagnostics carry only record IDs,
//! handles, hashes, redaction markers, bounded labels, and counts — never raw
//! transcript text, command output, patch hunks, issue bodies, environment
//! values, bearer tokens, or protected raw-artifact payloads.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ir::{GraphRecord, SourceSpan};
use crate::query::{
    self, FailureHandleError, RepositoryIndex, ResolvedFailureTarget, change_impact_context,
    failure_history_context, largest_semantic_drifts, memory_audit_context, resolve_drift_target,
    resolve_failure_handle, subsystem_context, symbol_context, task_evidence_context,
};

/// Default gate threshold: fraction of code-answer rows that must carry a
/// stable record ID plus a repo-relative file/span handle or a documented
/// absent-span reason (AC4).
pub const DEFAULT_MIN_CODE_CITATION: f64 = 0.95;

/// Tolerance applied when comparing the measured completeness against the gate
/// threshold so that an exact `0.95` fixture is not rejected by float drift.
const GATE_EPSILON: f64 = 1e-9;

// ---------------------------------------------------------------------------
// Public report types
// ---------------------------------------------------------------------------

/// How the `semantic` workflow is supplied to the audit.
///
/// The core audit is store-free and feature-free; the CLI handler collects the
/// embedded-store semantic rows (when the `embeddings` feature is built and a
/// `--data-dir` is given) and passes them in here. Over a plain `--graph`
/// fixture there is no embedded vector index, so `semantic` is reported as
/// disabled with a stable reason rather than silently dropped (AC2/AC7).
#[derive(Debug, Clone)]
pub enum SemanticInput {
    /// No embedded vector index available; reported with a stable reason.
    Disabled {
        /// Stable machine-readable reason, e.g. `requires_embedded_store`.
        reason: &'static str,
    },
    /// Embedded-store semantic retrieval leads to classify as code rows.
    Enabled {
        /// One row per semantic retrieval lead.
        rows: Vec<SemanticRow>,
    },
}

impl Default for SemanticInput {
    fn default() -> Self {
        Self::Disabled {
            reason: "requires_embedded_store",
        }
    }
}

/// One semantic retrieval lead collected from an embedded store.
#[derive(Debug, Clone)]
pub struct SemanticRow {
    /// Stable code record ID of the matched node.
    pub record_id: String,
    /// Repo-relative path of the matched node, when present.
    pub repo_relative_path: Option<String>,
    /// Source span of the matched node, when present.
    pub span: Option<SourceSpan>,
}

/// Audit configuration.
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Gate threshold for code-answer citation completeness (AC4).
    pub min_code_citation: f64,
    /// How the `semantic` workflow is supplied.
    pub semantic: SemanticInput,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            min_code_citation: DEFAULT_MIN_CODE_CITATION,
            semantic: SemanticInput::default(),
        }
    }
}

/// Per-workflow and overall citation tallies (AC3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AggregateCounts {
    /// Total rows the workflow returned.
    pub total_rows: usize,
    /// Rows carrying a non-empty stable record ID.
    pub rows_with_record_id: usize,
    /// Rows carrying the primary citable handle their trust class requires.
    pub rows_with_primary_handle: usize,
    /// Rows that legitimately have no span and use a documented absent-handle rule.
    pub rows_using_absent_handle_rule: usize,
    /// Rows missing a required handle (these fail the gate).
    pub rows_missing_required_handle: usize,
    /// Rows excluded because they are unverified or carry a protected payload.
    pub rows_excluded_unverified_or_protected: usize,
}

impl AggregateCounts {
    const fn add(&mut self, other: &Self) {
        self.total_rows += other.total_rows;
        self.rows_with_record_id += other.rows_with_record_id;
        self.rows_with_primary_handle += other.rows_with_primary_handle;
        self.rows_using_absent_handle_rule += other.rows_using_absent_handle_rule;
        self.rows_missing_required_handle += other.rows_missing_required_handle;
        self.rows_excluded_unverified_or_protected += other.rows_excluded_unverified_or_protected;
    }

    const fn count_row(&mut self, row: &RowClassification) {
        self.total_rows += 1;
        if !row.record_id.is_empty() {
            self.rows_with_record_id += 1;
        }
        match row.status {
            CitationStatus::Cited => self.rows_with_primary_handle += 1,
            CitationStatus::AbsentHandleDocumented => self.rows_using_absent_handle_rule += 1,
            CitationStatus::MissingRequiredHandle => self.rows_missing_required_handle += 1,
            CitationStatus::ExcludedUnverified | CitationStatus::ExcludedProtected => {
                self.rows_excluded_unverified_or_protected += 1;
            }
        }
    }
}

/// Citation status of a single returned row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationStatus {
    /// Carries a stable record ID plus the primary handle its class requires.
    Cited,
    /// Carries a stable record ID and uses a documented absent-handle rule.
    AbsentHandleDocumented,
    /// Missing a required handle — counts against the gate.
    MissingRequiredHandle,
    /// Excluded as an unverified agent-authored claim — reported, not hidden.
    ExcludedUnverified,
    /// Excluded because it references a protected raw payload — reported, not hidden.
    ExcludedProtected,
}

/// Documented reason a code row legitimately carries no span (AC4 "or
/// documented absent-span reason").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AbsentHandleRule {
    /// A module/repository/commit/change/import node is span-less by design.
    NoSpanModuleLevel,
    /// A drift target could not be resolved to a live node with a span.
    NoSpanDriftTargetUnresolved,
}

/// Classification of one returned row.
#[derive(Debug, Clone, Serialize)]
pub struct RowClassification {
    /// Stable record ID of the row.
    pub record_id: String,
    /// Trust class the row was classified under (reuses existing vocabulary).
    pub trust_class: &'static str,
    /// Citation status.
    pub status: CitationStatus,
    /// The primary citable handle, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_handle: Option<String>,
    /// The documented absent-handle rule, when one applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_handle_reason: Option<AbsentHandleRule>,
}

/// A stable diagnostic for a row or workflow condition (AC7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Workflow that produced the diagnostic.
    pub workflow: &'static str,
    /// Record ID of the node carrying the issue, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_record_id: Option<String>,
    /// Original handle (record ID, path, or hash) — never an inferred value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_handle: Option<String>,
    /// Relation that produced the handle, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
}

impl AuditDiagnostic {
    fn sort_key(&self) -> (&str, &'static str, &str, &str, &str) {
        (
            self.code.as_str(),
            self.workflow,
            self.source_record_id.as_deref().unwrap_or(""),
            self.target_handle.as_deref().unwrap_or(""),
            self.relation.as_deref().unwrap_or(""),
        )
    }
}

/// Per-workflow citation report.
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowReport {
    /// Workflow name (e.g. `symbol`, `context`, `policy`).
    pub workflow: &'static str,
    /// Whether the workflow was enabled in the fixture/runtime.
    pub enabled: bool,
    /// Stable reason the workflow was disabled, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<&'static str>,
    /// The workflow's dominant trust class (informational).
    pub trust_class: &'static str,
    /// Per-workflow tallies.
    pub counts: AggregateCounts,
    /// Classified rows, canonically ordered by record ID.
    pub rows: Vec<RowClassification>,
}

/// Pass/fail gate outcome (AC4/AC5).
#[derive(Debug, Clone, Serialize)]
pub struct GateOutcome {
    /// Measured fraction of code-answer rows that are cited or documented-absent.
    pub code_citation_completeness: f64,
    /// Whether the code-answer completeness gate passed (AC4).
    pub code_gate_pass: bool,
    /// Whether every non-code trust-class row carries a required handle (AC5).
    pub non_code_handle_gate_pass: bool,
    /// Count of missing-handle rows that lack a classifying diagnostic — must be 0.
    pub unclassified_missing_rows: usize,
}

/// The full deterministic citation-completeness report.
#[derive(Debug, Clone, Serialize)]
pub struct CitationAuditReport {
    /// Overall gate pass/fail.
    pub ok: bool,
    /// Gate threshold in effect.
    pub min_code_citation: f64,
    /// Per-workflow reports, canonically ordered by workflow name.
    pub workflows: Vec<WorkflowReport>,
    /// Overall tallies across enabled workflows.
    pub overall: AggregateCounts,
    /// Gate outcome.
    pub gate: GateOutcome,
    /// All diagnostics, canonically ordered.
    pub diagnostics: Vec<AuditDiagnostic>,
}

// ---------------------------------------------------------------------------
// Trust-class view (reuses existing vocabulary — AC6/AC11)
// ---------------------------------------------------------------------------

/// The citation audit's trust-class view of a record.
///
/// This is `crate::cli::trust_class_for` (the authoritative `NodeKind` → trust
/// class map) plus a single disambiguation: the user-context node kinds, which
/// `trust_class_for` reports as `"other"`, are mapped to the existing
/// serialized domain string `"user_context"` (`Domain::UserContext`). No new
/// trust vocabulary is introduced (AC11).
#[must_use]
pub fn citation_trust_class(record: &GraphRecord) -> &'static str {
    let base = crate::cli::trust_class_for(record);
    if base != "other" {
        return base;
    }
    match record.node_kind_name() {
        Some(
            "PromoteCandidate" | "PromotionPrompt" | "PromotionDecision" | "Preference"
            | "WorkflowRule" | "NamingDecision" | "Constraint",
        ) => "user_context",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// Row classification
// ---------------------------------------------------------------------------

/// Outcome of classifying one record: the row plus an optional diagnostic.
struct Classified {
    row: RowClassification,
    diagnostic: Option<(String, Option<String>)>,
}

/// Returns the protected-artifact handle a record references, if any (AC5/AC8).
fn referenced_protected_handle(record: &GraphRecord) -> Option<String> {
    let json = serde_json::to_string(record).ok()?;
    let prefix = crate::protected::PROTECTED_HANDLE_PREFIX;
    let start = json.find(prefix)?;
    let tail = &json[start + prefix.len()..];
    let hex: String = tail.chars().take_while(char::is_ascii_hexdigit).collect();
    if hex.len() >= 16 {
        Some(format!("{prefix}{hex}"))
    } else {
        None
    }
}

/// Returns true when the record carries any redaction marker or policy version.
fn carries_redaction(record: &GraphRecord) -> bool {
    if let GraphRecord::Node {
        redaction_policy_version: Some(_),
        ..
    } = record
    {
        return true;
    }
    serde_json::to_string(record).is_ok_and(|json| json.contains("<REDACTED:"))
}

fn is_spanless_code_kind(kind: &str) -> bool {
    matches!(
        kind,
        "Module" | "Repository" | "Commit" | "Change" | "Import"
    )
}

/// Classifies a code-answer row given its resolved handle components.
fn classify_code_handle(
    record_id: &str,
    kind: &str,
    path: Option<&str>,
    span: Option<&SourceSpan>,
    drift_target: bool,
) -> Classified {
    match (path, span) {
        (Some(p), Some(s)) => Classified {
            row: RowClassification {
                record_id: record_id.to_owned(),
                trust_class: "source_fact",
                status: CitationStatus::Cited,
                primary_handle: Some(format!("{p}:{}-{}", s.start_line, s.end_line)),
                absent_handle_reason: None,
            },
            diagnostic: None,
        },
        (Some(p), None) if drift_target => Classified {
            row: RowClassification {
                record_id: record_id.to_owned(),
                trust_class: "source_fact",
                status: CitationStatus::AbsentHandleDocumented,
                primary_handle: Some(p.to_owned()),
                absent_handle_reason: Some(AbsentHandleRule::NoSpanDriftTargetUnresolved),
            },
            diagnostic: None,
        },
        (Some(p), None) if is_spanless_code_kind(kind) => Classified {
            row: RowClassification {
                record_id: record_id.to_owned(),
                trust_class: "source_fact",
                status: CitationStatus::AbsentHandleDocumented,
                primary_handle: Some(p.to_owned()),
                absent_handle_reason: Some(AbsentHandleRule::NoSpanModuleLevel),
            },
            diagnostic: None,
        },
        (path, _) if drift_target => Classified {
            row: RowClassification {
                record_id: record_id.to_owned(),
                trust_class: "source_fact",
                status: CitationStatus::AbsentHandleDocumented,
                primary_handle: path.map(str::to_owned),
                absent_handle_reason: Some(AbsentHandleRule::NoSpanDriftTargetUnresolved),
            },
            diagnostic: None,
        },
        _ => Classified {
            row: RowClassification {
                record_id: record_id.to_owned(),
                trust_class: "source_fact",
                status: CitationStatus::MissingRequiredHandle,
                primary_handle: path.map(str::to_owned),
                absent_handle_reason: None,
            },
            diagnostic: Some(("missing_span".to_owned(), path.map(str::to_owned))),
        },
    }
}

/// Returns the first agent-authored evidence handle that points at a record
/// **other than** the claim itself (AC6: a claim is never its own evidence).
fn agent_external_handle(record: &GraphRecord) -> Option<String> {
    let GraphRecord::Node {
        id,
        source_handle,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    if let Some(handle) = source_handle.as_deref().filter(|h| !h.is_empty()) {
        return Some(handle.to_owned());
    }
    let links = evidence_links.as_ref()?;
    links.iter().find_map(|link| {
        let target = link.target_record_id.as_deref()?;
        if target != id && !target.is_empty() {
            Some(target.to_owned())
        } else {
            None
        }
    })
}

/// Returns the first project/source handle a project-state record carries.
fn project_handle(record: &GraphRecord) -> Option<String> {
    let GraphRecord::Node {
        entity_id,
        parent_task_id,
        source_external_link_id,
        system_native_id,
        url,
        ..
    } = record
    else {
        return None;
    };
    entity_id
        .clone()
        .or_else(|| parent_task_id.clone())
        .or_else(|| source_external_link_id.clone())
        .or_else(|| system_native_id.clone())
        .or_else(|| url.clone())
}

/// Returns the first policy-audit handle a user-context record carries.
fn user_context_handle(record: &GraphRecord) -> Option<String> {
    let GraphRecord::Node { user_context, .. } = record else {
        return None;
    };
    if let Some(decision) = &user_context.approval_decision_id {
        return Some(decision.clone());
    }
    if let Some(mat) = &user_context.materialized_record_id {
        return Some(mat.clone());
    }
    if let Some(candidate) = &user_context.candidate_id {
        return Some(candidate.clone());
    }
    user_context
        .supporting_evidence
        .as_ref()
        .and_then(|links| links.iter().find_map(|l| l.target_record_id.clone()))
}

/// Classifies any record by its own trust class, routing code facts to the
/// code-handle rule and non-code classes to their required-handle rule.
fn classify_record(record: &GraphRecord) -> Classified {
    // Excluded checks first: a protected payload reference is reported, never
    // counted toward the citation ratio, and its raw bytes are never emitted.
    if let Some(handle) = referenced_protected_handle(record) {
        return Classified {
            row: RowClassification {
                record_id: record.id().to_owned(),
                trust_class: citation_trust_class(record),
                status: CitationStatus::ExcludedProtected,
                primary_handle: Some(handle.clone()),
                absent_handle_reason: None,
            },
            diagnostic: Some(("protected_payload".to_owned(), Some(handle))),
        };
    }

    let trust = citation_trust_class(record);
    let id = record.id().to_owned();
    match trust {
        "source_fact" => {
            let (kind, path, span) = match record {
                GraphRecord::Node {
                    kind,
                    repo_relative_path,
                    span,
                    ..
                } => (kind.as_str(), repo_relative_path.as_deref(), span.as_ref()),
                _ => ("", None, None),
            };
            classify_code_handle(&id, kind, path, span, false)
        }
        "agent_authored" => cited_or_missing(&id, trust, agent_external_handle(record)),
        "project_state" => cited_or_missing(&id, trust, project_handle(record)),
        "user_context" => cited_or_missing(&id, trust, user_context_handle(record)),
        // Verification, artifact, and provenance ("other") records are
        // inherently citable by their own stable evidence handle.
        _ => cited(&id, trust, id.clone()),
    }
}

/// Returns a cited row when a required handle is present, else a missing row.
fn cited_or_missing(id: &str, trust: &'static str, handle: Option<String>) -> Classified {
    handle.map_or_else(|| missing(id, trust), |handle| cited(id, trust, handle))
}

fn cited(id: &str, trust: &'static str, handle: String) -> Classified {
    Classified {
        row: RowClassification {
            record_id: id.to_owned(),
            trust_class: trust,
            status: CitationStatus::Cited,
            primary_handle: Some(handle),
            absent_handle_reason: None,
        },
        diagnostic: None,
    }
}

fn missing(id: &str, trust: &'static str) -> Classified {
    Classified {
        row: RowClassification {
            record_id: id.to_owned(),
            trust_class: trust,
            status: CitationStatus::MissingRequiredHandle,
            primary_handle: None,
            absent_handle_reason: None,
        },
        diagnostic: Some(("missing_required_handle".to_owned(), None)),
    }
}

// ---------------------------------------------------------------------------
// Workflow driver scaffolding
// ---------------------------------------------------------------------------

/// Accumulates de-duplicated rows and diagnostics for one workflow.
struct WorkflowBuilder {
    workflow: &'static str,
    trust_class: &'static str,
    enabled: bool,
    disabled_reason: Option<&'static str>,
    rows: BTreeMap<String, RowClassification>,
    diagnostics: BTreeSet<DiagnosticEntry>,
}

/// `(code, source_record_id, target_handle, relation)` — a de-dup key for one
/// pending diagnostic before it is rendered into an [`AuditDiagnostic`].
type DiagnosticEntry = (String, Option<String>, Option<String>, Option<String>);

impl WorkflowBuilder {
    const fn new(workflow: &'static str, trust_class: &'static str) -> Self {
        Self {
            workflow,
            trust_class,
            enabled: true,
            disabled_reason: None,
            rows: BTreeMap::new(),
            diagnostics: BTreeSet::new(),
        }
    }

    const fn disabled(
        workflow: &'static str,
        trust_class: &'static str,
        reason: &'static str,
    ) -> Self {
        let mut builder = Self::new(workflow, trust_class);
        builder.enabled = false;
        builder.disabled_reason = Some(reason);
        builder
    }

    /// Classifies a record and records its row + any diagnostic.
    fn push_record(&mut self, record: &GraphRecord) {
        let classified = classify_record(record);
        self.push_classified(classified, None);
    }

    /// Classifies a record but forces the status (used for excluded sections).
    fn push_excluded(&mut self, record: &GraphRecord, status: CitationStatus) {
        let mut classified = classify_record(record);
        classified.row.status = status;
        classified.diagnostic = None;
        self.push_classified(classified, None);
    }

    fn push_classified(&mut self, classified: Classified, relation: Option<String>) {
        let Classified { row, diagnostic } = classified;
        if let Some((code, target)) = diagnostic {
            self.add_diagnostic(code, Some(row.record_id.clone()), target, relation);
        }
        // A record may surface through several sections/anchors; keep the
        // first (canonically lowest) classification to stay deterministic.
        self.rows.entry(row.record_id.clone()).or_insert(row);
    }

    fn add_diagnostic(
        &mut self,
        code: String,
        source: Option<String>,
        target: Option<String>,
        relation: Option<String>,
    ) {
        self.diagnostics.insert((code, source, target, relation));
    }

    fn note_redaction(&mut self, record: &GraphRecord) {
        if carries_redaction(record) {
            self.add_diagnostic(
                "redacted_field".to_owned(),
                Some(record.id().to_owned()),
                None,
                None,
            );
        }
    }

    fn finish(self) -> (WorkflowReport, Vec<AuditDiagnostic>) {
        let rows: Vec<RowClassification> = self.rows.into_values().collect();
        let mut counts = AggregateCounts::default();
        for row in &rows {
            counts.count_row(row);
        }
        let diagnostics: Vec<AuditDiagnostic> = self
            .diagnostics
            .into_iter()
            .map(|(code, source, target, relation)| AuditDiagnostic {
                code,
                workflow: self.workflow,
                source_record_id: source,
                target_handle: target,
                relation,
            })
            .collect();
        (
            WorkflowReport {
                workflow: self.workflow,
                enabled: self.enabled,
                disabled_reason: self.disabled_reason,
                trust_class: self.trust_class,
                counts,
                rows,
            },
            diagnostics,
        )
    }
}

// ---------------------------------------------------------------------------
// Input derivation (deterministic, from the record set)
// ---------------------------------------------------------------------------

fn tombstoned_ids(records: &[GraphRecord]) -> BTreeSet<&str> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect()
}

fn symbol_names(records: &[GraphRecord]) -> BTreeSet<&str> {
    let tombstoned = tombstoned_ids(records);
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind,
                name: Some(name),
                ..
            } if kind.as_str() == "Symbol" && !tombstoned.contains(id.as_str()) => {
                Some(name.as_str())
            }
            _ => None,
        })
        .collect()
}

fn file_paths(records: &[GraphRecord]) -> BTreeSet<&str> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                kind,
                repo_relative_path: Some(path),
                ..
            } if kind.as_str() == "File" => Some(path.as_str()),
            _ => None,
        })
        .collect()
}

fn subsystem_prefixes(records: &[GraphRecord]) -> BTreeSet<String> {
    file_paths(records)
        .into_iter()
        .filter_map(|path| path.rsplit_once('/').map(|(dir, _)| dir.to_owned()))
        .collect()
}

fn task_ids(records: &[GraphRecord]) -> BTreeSet<String> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind,
                entity_id,
                ..
            } if kind.as_str() == "Task" => Some(entity_id.clone().unwrap_or_else(|| id.clone())),
            _ => None,
        })
        .collect()
}

fn memory_claim_ids(records: &[GraphRecord]) -> BTreeSet<String> {
    let tombstoned = tombstoned_ids(records);
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { id, kind, .. }
                if matches!(kind.as_str(), "Observation" | "Decision" | "Failure")
                    && !tombstoned.contains(id.as_str()) =>
            {
                Some(id.clone())
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Per-workflow drivers
// ---------------------------------------------------------------------------

fn drive_symbol(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("symbol", "source_fact");
    let tombstoned = tombstoned_ids(records);
    for record in records {
        let GraphRecord::Node { id, kind, .. } = record else {
            continue;
        };
        if kind.as_str() == "Symbol" && !tombstoned.contains(id.as_str()) {
            builder.push_record(record);
            builder.note_redaction(record);
        }
    }
    builder
}

fn drive_file(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("file", "source_fact");
    let tombstoned = tombstoned_ids(records);
    let paths = file_paths(records);
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            repo_relative_path: Some(path),
            ..
        } = record
        else {
            continue;
        };
        if kind.as_str() == "Symbol"
            && paths.contains(path.as_str())
            && !tombstoned.contains(id.as_str())
        {
            builder.push_record(record);
        }
    }
    builder
}

fn drive_drift(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("drift", "source_fact");
    for drift_rec in largest_semantic_drifts(records, usize::MAX) {
        let GraphRecord::Node {
            id,
            semantic_drift: Some(drift),
            repo_relative_path,
            name,
            ..
        } = drift_rec
        else {
            continue;
        };
        let (path, _name, span) = resolve_drift_target(
            records,
            id,
            drift,
            repo_relative_path.as_deref(),
            name.as_deref(),
        );
        let classified = classify_code_handle(id, "SemanticDrift", path, span.as_ref(), true);
        builder.push_classified(classified, None);
    }
    builder
}

fn drive_semantic(config: &AuditConfig) -> WorkflowBuilder {
    match &config.semantic {
        SemanticInput::Disabled { reason } => {
            let mut builder = WorkflowBuilder::disabled("semantic", "source_fact", reason);
            builder.add_diagnostic("unsupported_workflow".to_owned(), None, None, None);
            builder
        }
        SemanticInput::Enabled { rows } => {
            let mut builder = WorkflowBuilder::new("semantic", "source_fact");
            for row in rows {
                let classified = classify_code_handle(
                    &row.record_id,
                    "Symbol",
                    row.repo_relative_path.as_deref(),
                    row.span.as_ref(),
                    false,
                );
                builder.push_classified(classified, None);
            }
            builder
        }
    }
}

fn drive_context(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("context", "source_fact");
    for name in symbol_names(records) {
        let ctx = symbol_context(records, name);
        for record in ctx
            .source_facts
            .iter()
            .chain(&ctx.observations)
            .chain(&ctx.project_state)
            .chain(&ctx.artifacts)
            .chain(&ctx.verification_evidence)
        {
            builder.push_record(record);
            builder.note_redaction(record);
        }
        for unresolved in &ctx.unresolved {
            builder.add_diagnostic(
                "unresolved_evidence_link".to_owned(),
                Some(unresolved.source_record_id.clone()),
                Some(unresolved.target_handle.clone()),
                Some(unresolved.relation.clone()),
            );
        }
    }
    builder
}

fn drive_subsystem(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("subsystem", "source_fact");
    for prefix in subsystem_prefixes(records) {
        let Ok(ctx) = subsystem_context(records, &prefix) else {
            continue;
        };
        for record in ctx
            .source_facts
            .iter()
            .chain(&ctx.observations)
            .chain(&ctx.project_state)
            .chain(&ctx.artifacts)
            .chain(&ctx.verification_evidence)
            .chain(&ctx.semantic_drift)
        {
            builder.push_record(record);
            builder.note_redaction(record);
        }
        for unresolved in &ctx.unresolved {
            builder.add_diagnostic(
                "unresolved_evidence_link".to_owned(),
                Some(unresolved.source_record_id.clone()),
                Some(unresolved.target_handle.clone()),
                Some(unresolved.relation.clone()),
            );
        }
    }
    builder
}

fn drive_task(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("task", "project_state");
    for task_id in task_ids(records) {
        let ctx = task_evidence_context(records, &task_id);
        for record in ctx
            .tasks
            .iter()
            .chain(&ctx.acceptance_criteria)
            .chain(&ctx.source_facts)
            .chain(&ctx.observations)
            .chain(&ctx.artifacts)
            .chain(&ctx.verification_evidence)
            .chain(&ctx.reviews)
            .chain(&ctx.external_links)
        {
            builder.push_record(record);
            builder.note_redaction(record);
        }
        for unresolved in &ctx.unresolved {
            builder.add_diagnostic(
                "unresolved_evidence_link".to_owned(),
                Some(unresolved.source_record_id.clone()),
                Some(unresolved.target_handle.clone()),
                Some(unresolved.relation.clone()),
            );
        }
    }
    builder
}

fn drive_memory(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("memory", "agent_authored");
    for memory_id in memory_claim_ids(records) {
        // Run with `verified_only` so unverified observations land in the
        // `excluded` section, demonstrating reported-not-hidden exclusion (AC6).
        let ctx = memory_audit_context(records, &memory_id, true);
        for record in ctx
            .memory_claim
            .iter()
            .chain(&ctx.agent_sessions)
            .chain(&ctx.agents)
        {
            builder.push_record(record);
            builder.note_redaction(record);
        }
        for item in ctx
            .supporting_evidence
            .iter()
            .chain(&ctx.contradicting_evidence)
            .chain(&ctx.superseding_records)
            .chain(&ctx.related_code_handles)
            .chain(&ctx.related_project_handles)
            .chain(&ctx.verification_evidence)
        {
            builder.push_record(item.record);
            builder.note_redaction(item.record);
        }
        for item in &ctx.excluded {
            builder.push_excluded(item.record, CitationStatus::ExcludedUnverified);
        }
        for diag in &ctx.diagnostics {
            builder.add_diagnostic(
                diag.code.clone(),
                Some(diag.source_record_id.clone()),
                Some(diag.target_handle.clone()),
                Some(diag.relation.clone()),
            );
        }
    }
    builder
}

/// Resolves a code/task anchor, emitting an `ambiguous_code_handle` diagnostic
/// when the handle matches more than one repository (AC7). Returns `None` for a
/// handle that resolves to nothing live or to a malformed/unsupported input.
fn resolve_anchor(
    builder: &mut WorkflowBuilder,
    records: &[GraphRecord],
    handle: &str,
    repo_index: &RepositoryIndex,
) -> Option<ResolvedFailureTarget> {
    match resolve_failure_handle(records, handle, repo_index, None) {
        Ok(target) if !target.is_empty() => Some(target),
        Ok(_) | Err(FailureHandleError::Unsupported { .. }) => None,
        Err(FailureHandleError::Ambiguous { handle, .. }) => {
            builder.add_diagnostic(
                "ambiguous_code_handle".to_owned(),
                None,
                Some(handle),
                None,
            );
            None
        }
    }
}

fn drive_failures(records: &[GraphRecord], repo_index: &RepositoryIndex) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("failures", "verification_evidence");
    let mut handles: BTreeSet<String> = BTreeSet::new();
    handles.extend(symbol_names(records).into_iter().map(str::to_owned));
    handles.extend(file_paths(records).into_iter().map(str::to_owned));
    handles.extend(task_ids(records));
    for handle in handles {
        let Some(target) = resolve_anchor(&mut builder, records, &handle, repo_index) else {
            continue;
        };
        let ctx = failure_history_context(records, &target);
        for attempt in ctx.runtime_failures.iter().chain(&ctx.agent_failures) {
            builder.push_record(attempt.item.record);
            builder.note_redaction(attempt.item.record);
        }
        for item in ctx.superseding_successes.iter().chain(&ctx.patch_artifacts) {
            builder.push_record(item.record);
        }
        for record in ctx.agent_sessions.iter().chain(&ctx.agents) {
            builder.push_record(record);
        }
        for diag in &ctx.diagnostics {
            builder.add_diagnostic(
                diag.code.clone(),
                Some(diag.source_record_id.clone()),
                Some(diag.target_handle.clone()),
                Some(diag.relation.clone()),
            );
        }
    }
    builder
}

fn drive_change_impact(records: &[GraphRecord], repo_index: &RepositoryIndex) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("change-impact", "source_fact");
    let mut handles: BTreeSet<String> = BTreeSet::new();
    handles.extend(symbol_names(records).into_iter().map(str::to_owned));
    handles.extend(file_paths(records).into_iter().map(str::to_owned));
    for handle in handles {
        let Some(target) = resolve_anchor(&mut builder, records, &handle, repo_index) else {
            continue;
        };
        let ctx = change_impact_context(records, &target, 1, repo_index, None);
        for lead in ctx
            .direct_callers
            .iter()
            .chain(&ctx.direct_callees)
            .chain(&ctx.referencing_files)
            .chain(&ctx.implementation_symbols)
            .chain(&ctx.containing_context)
        {
            builder.push_record(lead.record);
        }
        for diag in &ctx.diagnostics {
            builder.add_diagnostic(
                diag.code.clone(),
                Some(diag.source_record_id.clone()),
                Some(diag.target_handle.clone()),
                Some(diag.relation.clone()),
            );
        }
    }
    builder
}

fn drive_policy(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("policy", "user_context");
    for durable in query::active_policy(records, None) {
        builder.push_record(durable);
        builder.note_redaction(durable);
        if let Ok(chain) = query::audit_trail(records, durable) {
            for record in chain {
                builder.push_record(record);
            }
        }
    }
    builder
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Non-code trust classes gated by AC5 (every such row must carry a handle).
const NON_CODE_GATED: &[&str] = &[
    "agent_authored",
    "project_state",
    "artifact",
    "verification_evidence",
    "user_context",
];

/// Runs the citation-completeness audit over `records` and returns a
/// deterministic, redaction-safe report.
#[must_use]
pub fn run_citation_audit(records: &[GraphRecord], config: &AuditConfig) -> CitationAuditReport {
    let repo_index = RepositoryIndex::build(records);

    let builders = vec![
        drive_change_impact(records, &repo_index),
        drive_context(records),
        drive_drift(records),
        drive_failures(records, &repo_index),
        drive_file(records),
        drive_memory(records),
        drive_policy(records),
        drive_semantic(config),
        drive_subsystem(records),
        drive_symbol(records),
        drive_task(records),
    ];

    let mut workflows = Vec::new();
    let mut diagnostics = Vec::new();
    for builder in builders {
        let (report, mut diags) = builder.finish();
        diagnostics.append(&mut diags);
        workflows.push(report);
    }
    workflows.sort_by_key(|w| w.workflow);
    diagnostics.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    diagnostics.dedup();

    // Overall tallies across every (enabled or disabled) workflow.
    let mut overall = AggregateCounts::default();
    for workflow in &workflows {
        overall.add(&workflow.counts);
    }

    // Gate A (AC4): code-answer rows must be ≥ threshold cited-or-documented.
    let mut code_total = 0usize;
    let mut code_satisfied = 0usize;
    // Gate B (AC5): no gated non-code row may be missing its handle.
    let mut non_code_gate_pass = true;
    // Success metric: every missing-handle row must carry a classifying diagnostic.
    let diag_sources: BTreeSet<&str> = diagnostics
        .iter()
        .filter_map(|d| d.source_record_id.as_deref())
        .collect();
    let mut unclassified_missing_rows = 0usize;

    for workflow in &workflows {
        for row in &workflow.rows {
            if row.trust_class == "source_fact" {
                code_total += 1;
                if matches!(
                    row.status,
                    CitationStatus::Cited | CitationStatus::AbsentHandleDocumented
                ) {
                    code_satisfied += 1;
                }
            }
            if row.status == CitationStatus::MissingRequiredHandle {
                if NON_CODE_GATED.contains(&row.trust_class) {
                    non_code_gate_pass = false;
                }
                if !diag_sources.contains(row.record_id.as_str()) {
                    unclassified_missing_rows += 1;
                }
            }
        }
    }

    let code_citation_completeness = if code_total == 0 {
        1.0
    } else {
        // Counts are small (row tallies); precision loss is not a concern.
        #[allow(clippy::cast_precision_loss)]
        {
            code_satisfied as f64 / code_total as f64
        }
    };
    let code_gate_pass = code_citation_completeness + GATE_EPSILON >= config.min_code_citation;
    let ok = code_gate_pass && non_code_gate_pass && unclassified_missing_rows == 0;

    CitationAuditReport {
        ok,
        min_code_citation: config.min_code_citation,
        workflows,
        overall,
        gate: GateOutcome {
            code_citation_completeness,
            code_gate_pass,
            non_code_handle_gate_pass: non_code_gate_pass,
            unclassified_missing_rows,
        },
        diagnostics,
    }
}

#[cfg(test)]
mod tests;
