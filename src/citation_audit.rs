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

use crate::evidence_freshness::FreshnessVerdict;
use crate::ir::{GraphRecord, SourceSpan};
use crate::query::{
    self, FailureHandleError, RepositoryIndex, ResolvedFailureTarget, change_impact_context,
    changes_context, failure_history_context, largest_semantic_drifts, memory_audit_context,
    resolve_drift_target, resolve_failure_handle, subsystem_context, symbol_context,
    task_evidence_context,
};

/// Default gate threshold: fraction of code-answer rows that must carry a
/// stable record ID plus a repo-relative file/span handle or a documented
/// absent-span reason (AC4).
pub const DEFAULT_MIN_CODE_CITATION: f64 = 0.95;

/// Tolerance applied when comparing the measured completeness against the gate
/// threshold so that an exact `0.95` fixture is not rejected by float drift.
const GATE_EPSILON: f64 = 1e-9;

/// Default `--limit` shared by `eg query drift`/`semantic`. The audit measures
/// the default public invocation, so it caps those lanes at the same limit.
pub const DEFAULT_QUERY_LIMIT: usize = 10;

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
    /// Node kind of the matched record (`File` hits are path-cited; `Symbol`
    /// hits need a span), preserved so classification matches `eg query semantic`.
    pub kind: String,
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
    /// Optional history-inclusive record set for the `evidence-freshness` lane.
    ///
    /// `eg query evidence-freshness` reads the history-inclusive store view so
    /// superseded non-temporal versions can yield drift/unresolved verdicts. Over
    /// `--graph` the JSONL already carries that history, so this stays `None` and
    /// the lane uses the same records; over `--data-dir` the CLI supplies the
    /// history-inclusive load here so the audit sees the same rows the public
    /// command would.
    pub freshness_records: Option<Vec<GraphRecord>>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            min_code_citation: DEFAULT_MIN_CODE_CITATION,
            semantic: SemanticInput::default(),
            freshness_records: None,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
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

/// Classifies a record's citation status for external use.
#[must_use]
pub fn classify_record_external(record: &GraphRecord) -> RowClassification {
    classify_record(record).row
}

// ---------------------------------------------------------------------------
// Row classification
// ---------------------------------------------------------------------------

/// Outcome of classifying one record: the row plus an optional diagnostic.
struct Classified {
    row: RowClassification,
    diagnostic: Option<(String, Option<String>)>,
}

/// Returns a protected/withheld-payload handle a record references, if any
/// (AC5/AC8).
///
/// Matches both an explicit protected-store handle (`protected:v1:…`) anywhere
/// in the record and the withheld raw-payload handles (`stdout_handle`,
/// `stderr_handle`, `patch_handle`, `body_handle`, …) that the public memory and
/// failure-history audits surface as `protected_payload` diagnostics even when
/// no protected-store handle is present.
fn referenced_protected_handle(record: &GraphRecord) -> Option<String> {
    if let Some(handle) = scan_protected_prefix(record) {
        return Some(handle);
    }
    let GraphRecord::Node {
        stdout_handle,
        stderr_handle,
        result_handle,
        arguments_handle,
        body_handle,
        diff_hunk_handle,
        patch_handle,
        ..
    } = record
    else {
        return None;
    };
    // `protected_payload_diagnostics` byte-filters only stdout/stderr (a zero-byte
    // command stream stays a normal returned row), but emits `protected_payload`
    // for a `body`/`diff_hunk`/`arguments`/`result` handle whenever it is present,
    // regardless of byte count. Mirror that split so a zero-byte task/tool payload
    // is still excluded as protected rather than audited as an ordinary row.
    if let Some(output) = [stdout_handle, stderr_handle]
        .into_iter()
        .flatten()
        .find(|h| h.bytes > 0)
        .or_else(|| {
            [
                body_handle,
                diff_hunk_handle,
                arguments_handle,
                result_handle,
            ]
            .into_iter()
            .flatten()
            .next()
        })
    {
        return Some(output.hash.clone());
    }
    patch_handle
        .as_ref()
        .map(|patch| format!("patch:{}", patch.path))
}

/// Returns an explicit `protected:v1:<hex>` handle found anywhere in the record.
fn scan_protected_prefix(record: &GraphRecord) -> Option<String> {
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
    // The citation contract requires a stable record ID; a path/span alone is not
    // sufficient (a malformed/imported source fact with an empty id must fail).
    if record_id.is_empty() {
        return Classified {
            row: RowClassification {
                record_id: String::new(),
                trust_class: "source_fact",
                status: CitationStatus::MissingRequiredHandle,
                primary_handle: path.map(str::to_owned),
                absent_handle_reason: None,
            },
            diagnostic: Some(("missing_record_id".to_owned(), path.map(str::to_owned))),
        };
    }
    let documented = |reason: AbsentHandleRule| Classified {
        row: RowClassification {
            record_id: record_id.to_owned(),
            trust_class: "source_fact",
            status: CitationStatus::AbsentHandleDocumented,
            primary_handle: path.map(str::to_owned),
            absent_handle_reason: Some(reason),
        },
        diagnostic: None,
    };
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
        // A `File` source fact is cited by its repo-relative path: a scan emits
        // `File` nodes with a path but no span, and the public context / changes /
        // subsystem rows treat the whole-file path as the citation handle.
        (Some(p), None) if kind == "File" => Classified {
            row: RowClassification {
                record_id: record_id.to_owned(),
                trust_class: "source_fact",
                status: CitationStatus::Cited,
                primary_handle: Some(p.to_owned()),
                absent_handle_reason: None,
            },
            diagnostic: None,
        },
        // A drift target that did not resolve to a span, or a span-less code
        // kind (Commit/Module/…), is documented-absent — with or without a path.
        (_, None) if drift_target => documented(AbsentHandleRule::NoSpanDriftTargetUnresolved),
        (_, None) if is_spanless_code_kind(kind) => documented(AbsentHandleRule::NoSpanModuleLevel),
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

/// Returns the first agent-authored provenance handle that points at something
/// **other than** the claim itself (AC6: a claim is never its own evidence).
///
/// Accepts the same provenance the public `eg query memory`/`context` rows expose:
/// a `source_handle`, a source-artifact path/hash, an external evidence-link
/// target, or the `agent_id`/`session_id` session provenance handle.
fn agent_external_handle(record: &GraphRecord) -> Option<String> {
    let GraphRecord::Node {
        id,
        source_handle,
        source_artifact_path,
        source_artifact_hash,
        evidence_links,
        agent_id,
        session_id,
        ..
    } = record
    else {
        return None;
    };
    // Source/artifact handles must point at something other than the claim's own
    // record ID (AC6: a claim is never its own evidence).
    if let Some(handle) = [source_handle, source_artifact_path, source_artifact_hash]
        .into_iter()
        .flatten()
        .find(|h| !h.is_empty() && h.as_str() != id.as_str())
    {
        return Some(handle.clone());
    }
    if let Some(links) = evidence_links
        && let Some(target) = links.iter().find_map(|link| {
            link.target_record_id
                .as_deref()
                .filter(|t| *t != id && !t.is_empty())
        })
    {
        return Some(target.to_owned());
    }
    // Session provenance (`agent_id:session_id`) is a usable citation handle for
    // imported claims that carry no artifact handle.
    match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(agent), Some(session)) if !agent.is_empty() && !session.is_empty() => {
            Some(format!("{agent}:{session}"))
        }
        _ => None,
    }
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
    [
        entity_id,
        parent_task_id,
        source_external_link_id,
        system_native_id,
        url,
    ]
    .into_iter()
    .flatten()
    .find(|h| !h.is_empty())
    .cloned()
}

/// Returns the first source/provenance handle an artifact-class record carries.
///
/// Protected/withheld artifacts are excluded before this is reached; a surviving
/// artifact must still name its provenance (source path/hash, patch-byte hash, or
/// a file-edit before/after hash) rather than being credited by its own ID.
fn artifact_handle(record: &GraphRecord) -> Option<String> {
    let GraphRecord::Node {
        source_handle,
        source_artifact_path,
        source_artifact_hash,
        patch_bytes_hash,
        before_hash,
        after_hash,
        ..
    } = record
    else {
        return None;
    };
    [
        source_handle,
        source_artifact_path,
        source_artifact_hash,
        patch_bytes_hash,
        before_hash,
        after_hash,
    ]
    .into_iter()
    .flatten()
    .find(|h| !h.is_empty())
    .cloned()
}

/// Returns the first policy-audit handle a user-context record carries.
fn user_context_handle(record: &GraphRecord) -> Option<String> {
    let GraphRecord::Node { user_context, .. } = record else {
        return None;
    };
    [
        &user_context.approval_decision_id,
        &user_context.materialized_record_id,
        &user_context.candidate_id,
    ]
    .into_iter()
    .flatten()
    .find(|h| !h.is_empty())
    .cloned()
    .or_else(|| {
        user_context.supporting_evidence.as_ref().and_then(|links| {
            links
                .iter()
                .find_map(|l| l.target_record_id.clone().filter(|t| !t.is_empty()))
        })
    })
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
        "artifact" => cited_or_missing(&id, trust, artifact_handle(record)),
        // Verification and provenance ("other") records are inherently citable by
        // their own stable evidence handle — but a record with an empty id carries
        // no usable handle and must fail the gate.
        _ if id.is_empty() => missing(&id, trust),
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
    /// Keyed by `(record_id, temporal_discriminator)` so that scan-history
    /// graphs — where multiple temporal versions share one stable `record_id`
    /// (symbol identity excludes commit, ADR-0004) — keep each version as a
    /// distinct row instead of letting a cited version hide an uncited one.
    rows: BTreeMap<(String, String), RowClassification>,
    diagnostics: BTreeSet<DiagnosticEntry>,
    /// Monotonic counter that disambiguates rows with an **empty** record ID so
    /// several malformed empty-ID public rows are each counted (they would
    /// otherwise collapse into one `("", temporal)` map key).
    empty_id_seq: usize,
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
            empty_id_seq: 0,
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
        self.push_classified(classified, temporal_key(record));
    }

    /// Classifies a record but forces the status (used for excluded sections).
    fn push_excluded(&mut self, record: &GraphRecord, status: CitationStatus) {
        let mut classified = classify_record(record);
        classified.row.status = status;
        classified.diagnostic = None;
        self.push_classified(classified, temporal_key(record));
    }

    fn push_classified(&mut self, classified: Classified, temporal_key: String) {
        let Classified { row, diagnostic } = classified;
        if let Some((code, target)) = diagnostic {
            self.add_diagnostic(code, Some(row.record_id.clone()), target, None);
        }
        // A record may surface through several sections/anchors; keep the first
        // (canonically lowest) classification per (record_id, temporal) to stay
        // deterministic while still counting each temporal version once. Rows with
        // an empty record ID cannot be de-duplicated by identity, so give each a
        // distinct synthetic key — several malformed empty-ID public rows must all
        // be counted, not collapsed into one.
        let identity = if row.record_id.is_empty() {
            self.empty_id_seq += 1;
            format!("\0empty:{}", self.empty_id_seq)
        } else {
            row.record_id.clone()
        };
        self.rows.entry((identity, temporal_key)).or_insert(row);
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

/// Returns a temporal discriminator for a record so that distinct history
/// versions sharing one stable `record_id` are not collapsed into one row.
/// Current-tree records (no `temporal`) share an empty key and de-dup normally.
fn temporal_key(record: &GraphRecord) -> String {
    match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        }
        | GraphRecord::Edge {
            temporal: Some(t), ..
        } => t.git_commit.clone(),
        GraphRecord::Node {
            valid_time: Some(v),
            ..
        } => v.clone(),
        _ => String::new(),
    }
}

/// Classifies a `SemanticDrift` node by resolving its drift target's file/span
/// the same way `eg query drift` / `eg query subsystem` render it, instead of
/// crediting the drift node by its own ID. Returns the classification plus the
/// drift node's temporal key. `None` for non-drift records.
fn classify_drift_row(
    records: &[GraphRecord],
    drift_rec: &GraphRecord,
) -> Option<(Classified, String)> {
    let GraphRecord::Node {
        id,
        semantic_drift: Some(drift),
        repo_relative_path,
        name,
        ..
    } = drift_rec
    else {
        return None;
    };
    let (path, _name, span) = resolve_drift_target(
        records,
        id,
        drift,
        repo_relative_path.as_deref(),
        name.as_deref(),
    );
    let classified = classify_code_handle(id, "SemanticDrift", path, span.as_ref(), true);
    Some((classified, temporal_key(drift_rec)))
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

/// A node is current-or-historical when it is not tombstoned, or it carries a
/// temporal anchor (a scan-history version that `eg query symbol`/`file` still
/// returns even after the symbol was later deleted).
fn node_visible(id: &str, temporal_present: bool, tombstoned: &BTreeSet<&str>) -> bool {
    temporal_present || !tombstoned.contains(id)
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
                temporal,
                ..
            } if kind.as_str() == "Symbol" && node_visible(id, temporal.is_some(), &tombstoned) => {
                Some(name.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Every `File` path including tombstoned ones. `eg query failures <path>` and
/// `eg query change-impact <path>` resolve through `resolve_failure_handle`,
/// which is history-bearing and keeps temporal file records reachable after a
/// current-state tombstone — so those lanes seed from this, not `file_paths`.
fn all_file_paths(records: &[GraphRecord]) -> BTreeSet<&str> {
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

/// Stable `AgentSession` record IDs. `eg query failures <agent-session-id>`
/// resolves failures authored in that session (incl. via `AUTHORED_BY` edges).
fn agent_session_ids(records: &[GraphRecord]) -> BTreeSet<String> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { id, kind, .. } if kind.as_str() == "AgentSession" => {
                Some(id.clone())
            }
            _ => None,
        })
        .collect()
}

fn file_paths(records: &[GraphRecord]) -> BTreeSet<&str> {
    // `eg query file <path>` returns nothing for a tombstoned file, so the `file`
    // workflow must not drive deleted-file paths (which would otherwise count
    // historical symbols the public command no longer emits).
    let tombstoned = tombstoned_ids(records);
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind,
                repo_relative_path: Some(path),
                ..
            } if kind.as_str() == "File" && !tombstoned.contains(id.as_str()) => {
                Some(path.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Repo-relative paths of every code source fact (`File` and `Symbol` nodes).
/// `eg query subsystem` accepts a prefix over both, so symbol-only slices with no
/// `File` node must still be driven.
fn source_fact_paths(records: &[GraphRecord]) -> BTreeSet<&str> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                kind,
                repo_relative_path: Some(path),
                ..
            } if matches!(kind.as_str(), "File" | "Symbol") => Some(path.as_str()),
            _ => None,
        })
        .collect()
}

fn subsystem_prefixes(records: &[GraphRecord]) -> BTreeSet<String> {
    // `eg query subsystem <prefix>` accepts a bare file path as a prefix
    // (`path_is_under_prefix` treats an exact path match as under the prefix), so a
    // repo-root file like `build.rs`/`main.rs` is a real public subsystem entry
    // point. Use the parent directory when there is one, else the path itself, so
    // those root-level rows are audited instead of dropped.
    source_fact_paths(records)
        .into_iter()
        .map(|path| match path.rsplit_once('/') {
            Some((dir, _)) => dir.to_owned(),
            None => path.to_owned(),
        })
        .collect()
}

/// Canonical `Task` record IDs. `task_evidence_context` / `resolve_failure_handle`
/// seed by the stable record `id` (not `entity_id`), so emitting the record ID
/// keeps a task that carries an `entity_id`/source handle from auditing as zero
/// rows and silently escaping the gate.
fn task_ids(records: &[GraphRecord]) -> BTreeSet<String> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node { id, kind, .. } if kind.as_str() == "Task" => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// Source/provenance handles `eg query failures` accepts beyond code/task IDs:
/// a failure or verification record's `source_handle`, source-artifact path/hash,
/// or `session_id`.
fn failure_source_handles(records: &[GraphRecord]) -> BTreeSet<String> {
    let mut handles = BTreeSet::new();
    for record in records {
        let GraphRecord::Node {
            kind,
            source_handle,
            source_artifact_path,
            source_artifact_hash,
            session_id,
            ..
        } = record
        else {
            continue;
        };
        if !matches!(
            kind.as_str(),
            "Failure"
                | "Verification"
                | "CommandRun"
                | "CommandEvidence"
                | "TestRun"
                | "CIStatus"
                | "BenchmarkRun"
                | "CoverageReport"
                | "ProofResult"
        ) {
            continue;
        }
        for handle in [
            source_handle,
            source_artifact_path,
            source_artifact_hash,
            session_id,
        ]
        .into_iter()
        .flatten()
        {
            if !handle.is_empty() {
                handles.insert(handle.clone());
            }
        }
    }
    handles
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
        let GraphRecord::Node {
            id, kind, temporal, ..
        } = record
        else {
            continue;
        };
        if kind.as_str() == "Symbol" && node_visible(id, temporal.is_some(), &tombstoned) {
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
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if kind.as_str() == "Symbol"
            && paths.contains(path.as_str())
            && node_visible(id, temporal.is_some(), &tombstoned)
        {
            builder.push_record(record);
        }
    }
    builder
}

fn drive_drift(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("drift", "source_fact");
    // Measure the DEFAULT `eg query drift` output, which returns the top
    // `DEFAULT_QUERY_LIMIT` rows; later rows are not emitted by the default
    // public invocation the gate targets.
    for drift_rec in largest_semantic_drifts(records, DEFAULT_QUERY_LIMIT) {
        if let Some((classified, tk)) = classify_drift_row(records, drift_rec) {
            builder.push_classified(classified, tk);
        }
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
                    &row.kind,
                    row.repo_relative_path.as_deref(),
                    row.span.as_ref(),
                    false,
                );
                builder.push_classified(classified, String::new());
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
            .chain(&ctx.topology_edges)
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
            .chain(&ctx.topology_edges)
            .chain(&ctx.observations)
            .chain(&ctx.project_state)
            .chain(&ctx.artifacts)
            .chain(&ctx.verification_evidence)
        {
            builder.push_record(record);
            builder.note_redaction(record);
        }
        // Drift rows must be classified by their resolved target handle, exactly
        // as `eg query subsystem` renders them — not credited by their own ID.
        for drift_rec in &ctx.semantic_drift {
            if let Some((classified, tk)) = classify_drift_row(records, drift_rec) {
                builder.push_classified(classified, tk);
            }
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
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
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
        // `eg query task` enriches a verified AC with a field-linked
        // `verification_record` (from `verification_link_id`) even when no edge put
        // it in `verification_evidence`; classify those rows too.
        for ac in &ctx.acceptance_criteria {
            if let GraphRecord::Node {
                verification_link_id: Some(ver_id),
                ..
            } = ac
                && let Some(record) = by_id.get(ver_id.as_str())
            {
                builder.push_record(record);
                builder.note_redaction(record);
            }
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
        // Audit the DEFAULT `eg query memory` view (no `--verified-only`): the
        // default emits unverified supporting/contradicting claims as normal
        // rows, so they must be classified (and gated) rather than moved to the
        // excluded section by a `--verified-only` run the gate does not target.
        let ctx = memory_audit_context(records, &memory_id, false);
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

/// Resolves a code/task anchor into the target(s) the audit should drive.
///
/// A handle that matches more than one repository still has a valid `--repo`
/// execution per candidate, so rather than dropping it (and leaving its rows
/// outside the gate) the audit records an `ambiguous_code_handle` diagnostic and
/// drives each ambiguous **candidate record ID** — each of which resolves
/// unambiguously. Returns an empty vec for handles that resolve to nothing live
/// or to malformed/unsupported input.
fn resolve_anchors(
    builder: &mut WorkflowBuilder,
    records: &[GraphRecord],
    handle: &str,
    repo_index: &RepositoryIndex,
) -> Vec<ResolvedFailureTarget> {
    match resolve_failure_handle(records, handle, repo_index, None) {
        Ok(target) if !target.is_empty() => vec![target],
        Ok(_) | Err(FailureHandleError::Unsupported { .. }) => Vec::new(),
        Err(FailureHandleError::Ambiguous { handle, candidates }) => {
            builder.add_diagnostic("ambiguous_code_handle".to_owned(), None, Some(handle), None);
            candidates
                .iter()
                .filter_map(|candidate| {
                    match resolve_failure_handle(records, candidate, repo_index, None) {
                        Ok(target) if !target.is_empty() => Some(target),
                        _ => None,
                    }
                })
                .collect()
        }
    }
}

fn drive_failures(records: &[GraphRecord], repo_index: &RepositoryIndex) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("failures", "verification_evidence");
    let mut handles: BTreeSet<String> = BTreeSet::new();
    handles.extend(symbol_names(records).into_iter().map(str::to_owned));
    // History-bearing: failures linked to a now-deleted file are still reachable
    // by that path, so seed every file path (not the tombstone-filtered set).
    handles.extend(all_file_paths(records).into_iter().map(str::to_owned));
    handles.extend(task_ids(records));
    handles.extend(agent_session_ids(records));
    // `eg query failures` also resolves source/provenance handles (a failure's
    // source handle, source-artifact path/hash, or session ID), so seed those too
    // — otherwise failures reachable only through them go unmeasured.
    handles.extend(failure_source_handles(records));
    for handle in handles {
        for target in resolve_anchors(&mut builder, records, &handle, repo_index) {
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
    }
    builder
}

fn drive_change_impact(records: &[GraphRecord], repo_index: &RepositoryIndex) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("change-impact", "source_fact");
    let mut handles: BTreeSet<String> = BTreeSet::new();
    handles.extend(symbol_names(records).into_iter().map(str::to_owned));
    // History-bearing handle resolution (see `all_file_paths`).
    handles.extend(all_file_paths(records).into_iter().map(str::to_owned));
    for handle in handles {
        for target in resolve_anchors(&mut builder, records, &handle, repo_index) {
            // Audit the default change-impact invocation (depth 1). Wider
            // `--depth N` runs are operator-driven and outside the default gate.
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
    }
    builder
}

fn drive_policy(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("policy", "user_context");
    // `eg query audit <durable_id>` audits any durable policy node, active or
    // revoked, but only when its audit trail resolves: `eg query policy` filters
    // out durables whose `audit_trail` fails and `eg query audit` returns an
    // error envelope (no rows) for them. So classify a durable only when its
    // trail resolves, and count the resolved chain — never a malformed durable no
    // default public workflow would emit.
    for record in records {
        let GraphRecord::Node { kind, .. } = record else {
            continue;
        };
        if !matches!(
            kind.as_str(),
            "Preference" | "WorkflowRule" | "NamingDecision" | "Constraint"
        ) {
            continue;
        }
        if let Ok(chain) = query::audit_trail(records, record) {
            builder.push_record(record);
            builder.note_redaction(record);
            for chained in chain {
                builder.push_record(chained);
            }
        }
    }
    builder
}

/// `eg query candidates` — pending `PromoteCandidate` rows. These are
/// user-context answers that escape the policy lane (which only sees materialized
/// durables), so an uncited pending candidate must still be gated.
fn drive_candidates(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("candidates", "user_context");
    for candidate in query::pending_candidates(records, None) {
        builder.push_record(candidate);
        builder.note_redaction(candidate);
    }
    builder
}

/// `eg query changes` — commit-range context. The range endpoints are taken from
/// the **commit parent topology** (a root with no in-set parent = base, a tip
/// that is no in-set commit's parent = head) rather than SHA or timestamp order,
/// so neither a child SHA sorting before its parent nor a skewed author time is
/// mistaken for a reversed (disabled) range. Without at least two distinct
/// commits the workflow is reported disabled rather than skipped.
/// Derive a `(base, head)` commit pair from the in-set commit topology so the
/// `changes` lane audits a real ancestry range rather than a timestamp guess.
/// `base` is the earliest root with no in-set parent; `head` is the latest tip
/// that is no in-set commit's parent. Returns `None` when fewer than two
/// distinct commits connect.
fn changes_range(records: &[GraphRecord]) -> Option<(&str, &str)> {
    let mut shas: BTreeSet<&str> = BTreeSet::new();
    let mut parents_of: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut is_parent: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        if let GraphRecord::Node {
            kind,
            name: Some(sha),
            temporal,
            ..
        } = record
            && kind.as_str() == "Commit"
        {
            shas.insert(sha.as_str());
            let parents: Vec<&str> = temporal
                .as_ref()
                .map(|t| t.git_parent_commits.iter().map(String::as_str).collect())
                .unwrap_or_default();
            parents_of.insert(sha.as_str(), parents);
        }
    }
    // A commit is some other in-set commit's parent → it has a child in the slice.
    for parents in parents_of.values() {
        for parent in parents {
            if shas.contains(parent) {
                is_parent.insert(parent);
            }
        }
    }
    // In-set ancestors of `start` reachable via parent edges (excluding `start`).
    let ancestors_of = |start: &str| -> BTreeSet<&str> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut stack = vec![start];
        while let Some(current) = stack.pop() {
            if let Some(parents) = parents_of.get(current) {
                for parent in parents {
                    if shas.contains(parent) && seen.insert(*parent) {
                        stack.push(*parent);
                    }
                }
            }
        }
        seen
    };
    // `changes_context` needs `base` reachable from `head` via parent topology, so
    // root/tip extrema from two disconnected chains pair into a `NoPath`. Walk each
    // tip's (latest sha first) ancestry and pair it with a reachable root — a
    // provably connected range a public `eg query changes <base> <head>` accepts.
    for head in shas.iter().rev() {
        if is_parent.contains(head) {
            continue; // not a tip — some in-set commit descends from it
        }
        let ancestors = ancestors_of(head);
        let base = ancestors
            .iter()
            .find(|a| {
                parents_of
                    .get(**a)
                    .is_none_or(|parents| parents.iter().all(|p| !shas.contains(p)))
            })
            .or_else(|| ancestors.iter().next())
            .copied();
        if let Some(base) = base {
            return Some((base, head));
        }
    }
    None
}

fn drive_changes(records: &[GraphRecord]) -> WorkflowBuilder {
    let Some((base, head)) = changes_range(records) else {
        return WorkflowBuilder::disabled("changes", "source_fact", "requires_commit_range");
    };

    let Ok(ctx) = changes_context(records, base, head, None) else {
        return WorkflowBuilder::disabled("changes", "source_fact", "commit_range_unresolved");
    };

    let mut builder = WorkflowBuilder::new("changes", "source_fact");
    for item in &ctx.changed_files {
        builder.push_record(item.record);
        builder.note_redaction(item.record);
    }
    for item in &ctx.changed_symbols {
        builder.push_record(item.record);
        builder.note_redaction(item.record);
    }
    for item in &ctx.commits {
        builder.push_record(item.record);
    }
    // Deletion tombstones are real `eg query changes` rows; cite each by the
    // stable ID of the code fact it deleted so the deletion result is measured.
    for item in &ctx.tombstones {
        builder.push_classified(
            Classified {
                row: RowClassification {
                    record_id: item.record.id().to_owned(),
                    trust_class: "source_fact",
                    status: CitationStatus::Cited,
                    primary_handle: Some(item.deleted_id.to_owned()),
                    absent_handle_reason: None,
                },
                diagnostic: None,
            },
            String::new(),
        );
    }
    for item in &ctx.drift_records {
        if let Some((classified, tk)) = classify_drift_row(records, item.record) {
            builder.push_classified(classified, tk);
        }
    }
    // Cross-domain evidence rows expose only `record_id`; resolve and classify.
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let cross_ids = ctx
        .observations
        .iter()
        .map(|o| o.record_id)
        .chain(ctx.project_state.iter().map(|p| p.record_id))
        .chain(ctx.artifacts.iter().map(|a| a.record_id))
        .chain(ctx.verification_evidence.iter().map(|v| v.record_id));
    for id in cross_ids {
        if let Some(record) = by_id.get(id) {
            builder.push_record(record);
            builder.note_redaction(record);
        }
    }
    // `eg query changes` also serializes unexplained code changes and unresolved
    // evidence links; classify the former (resolving each ID to its record) and
    // surface the latter as diagnostics so neither escapes the gate.
    for unexplained in &ctx.unexplained {
        if let Some(record) = by_id.get(unexplained.record_id) {
            builder.push_record(record);
            builder.note_redaction(record);
        }
    }
    for unresolved in &ctx.unresolved {
        builder.add_diagnostic(
            "unresolved_evidence_link".to_owned(),
            Some(unresolved.source_record_id.clone()),
            Some(unresolved.target_handle.clone()),
            Some(unresolved.relation.clone()),
        );
    }
    builder
}

/// `eg query evidence-freshness` — per-observation freshness verdicts. Each
/// verdict row pairs an agent-authored observation with its cited code handle;
/// both must carry their required citation, and stale/unresolved verdicts emit a
/// diagnostic against the original handle.
fn drive_evidence_freshness(records: &[GraphRecord]) -> WorkflowBuilder {
    let mut builder = WorkflowBuilder::new("evidence-freshness", "agent_authored");
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    for entry in crate::evidence_freshness::evidence_link_freshness(records) {
        if let Some(observation) = by_id.get(entry.observation_id.as_str()) {
            builder.push_record(observation);
            builder.note_redaction(observation);
        }
        let path = entry.cited_handle.repo_relative_path.as_deref();
        let span = entry.cited_handle.span.as_ref();
        // Resolve the cited target's real kind so a file-level citation (path,
        // no span — e.g. a `TOUCHED_FILE` link) is path-cited, not reported as a
        // span-less `Symbol` miss. Default to `File` (path-cited) when unresolved.
        let kind = entry
            .cited_handle
            .target_record_id
            .as_deref()
            .and_then(|t| by_id.get(t))
            .and_then(|r| r.node_kind_name())
            .unwrap_or("File");
        match (entry.cited_handle.target_record_id.as_deref(), path) {
            // A record-ID citation, or a triple-only citation that resolved to a
            // repo-relative path/span, is a real public freshness row: classify it
            // by its code handle rather than dropping it as unresolved.
            (Some(target_id), _) => {
                let classified = classify_code_handle(target_id, kind, path, span, false);
                builder.push_classified(classified, String::new());
            }
            (None, Some(resolved_path)) => {
                let id = span.map_or_else(
                    || resolved_path.to_owned(),
                    |s| format!("{resolved_path}:{}-{}", s.start_line, s.end_line),
                );
                let classified = classify_code_handle(&id, kind, path, span, false);
                builder.push_classified(classified, String::new());
            }
            (None, None) => builder.add_diagnostic(
                "unresolved_evidence_link".to_owned(),
                Some(entry.observation_id.clone()),
                None,
                None,
            ),
        }
        match entry.verdict {
            FreshnessVerdict::Drifted => builder.add_diagnostic(
                "stale_span".to_owned(),
                Some(entry.observation_id.clone()),
                entry.cited_handle.target_record_id.clone(),
                None,
            ),
            FreshnessVerdict::Unresolved => builder.add_diagnostic(
                "unresolved_evidence_link".to_owned(),
                Some(entry.observation_id.clone()),
                entry.cited_handle.target_record_id.clone(),
                None,
            ),
            FreshnessVerdict::Current | FreshnessVerdict::Untemporal => {}
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
    let freshness_records = config.freshness_records.as_deref().unwrap_or(records);

    let builders = vec![
        drive_candidates(records),
        drive_change_impact(records, &repo_index),
        drive_changes(records),
        drive_context(records),
        drive_drift(records),
        drive_evidence_freshness(freshness_records),
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
