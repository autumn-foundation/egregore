//! Per-run agent cost rollup (issue #132).
//!
//! Answers "what did imported agent runs cost?" from `CostUsage` records the
//! `.traj` importer emits (one per `AgentRun` whenever the source `info`
//! block carries any cost, token, or duration field). One row per run carries
//! the cost/token/duration values verbatim from the transcript, plus the
//! record ID, owning run/session handles, and source handle; `totals`
//! aggregates the full filtered set.
//!
//! # Epistemology
//!
//! Everything here is a TRANSCRIPT-DERIVED CLAIM. Cost figures were read
//! verbatim from a trajectory transcript's `info` block — they are never
//! recomputed, never presented as deterministic code facts, and never
//! zero-defaulted: an absent value stays `null` (unknown) in rows and is
//! excluded from sums. The envelope carries this disclaimer verbatim.
//!
//! # Determinism
//!
//! The pure core is transport-agnostic: it consumes an append-ordered record
//! slice (a `--graph` JSONL or an embedded current-state read) and every
//! intermediate collection is a `Vec` sorted by record ID, so output is byte
//! identical across runs and across transports. Run membership is
//! EDGE-DERIVED only — a `CostUsage` node is attributed to the `AgentRun`
//! its `AuthoredBy` edge names, never to a matching ID string.

use serde::{Deserialize, Serialize};

use crate::ir::{
    COST_USAGE_DERIVATION_TRANSCRIPT, CostUsagePayload, EdgeLabel, GraphRecord, NodeKind,
};

/// Default number of cost rows returned when `--limit` is not supplied.
pub const COST_DEFAULT_LIMIT: usize = 100;

/// Largest accepted `--limit`; anything outside `1..=COST_MAX_LIMIT` is
/// rejected with an `invalid_limit` diagnostic before any store is read.
pub const COST_MAX_LIMIT: usize = 1000;

/// Maximum characters of any single free-text cost field that reaches rendered text output.
///
/// The JSON transport keeps raw values; serde escapes control characters.
/// Mirrors the bounding discipline of the other lanes.
pub const COST_FIELD_MAX_CHARS: usize = 128;

/// Marker appended when [`COST_FIELD_MAX_CHARS`] truncates a value, so a cap
/// is visible rather than silent.
const TRUNCATION_MARKER: char = '…';

/// The standing epistemic disclaimer every cost answer carries verbatim, on
/// both the CLI envelope and the daemon result.
pub const COST_DISCLAIMER: &str = "cost, token, and duration figures are transcript-derived claims read verbatim from trajectory info blocks, never recomputed or verified against provider billing; absent values are unknown (null), never zero; rows are agent-memory records, not deterministic code facts";

/// Trust class stamped on every cost row: transcript-derived accounting is
/// not a truth-bearing claim.
pub const COST_TRUST_CLASS: &str = "other";

/// Verification-outcome filter vocabulary for `--verification`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostVerificationOutcome {
    /// The run's recorded `verification_status` is `verified`.
    Verified,
    /// The run's recorded `verification_status` is `failed`.
    Failed,
}

/// Filters for the cost rollup. All filters are ANDed; `None` means unfiltered.
#[derive(Debug, Clone, Default)]
pub struct CostFilters {
    /// Restrict to runs whose task handle starts with this prefix.
    pub task: Option<String>,
    /// Restrict to the session with this ID (exact or leading prefix).
    pub session: Option<String>,
    /// Restrict by recorded verification outcome.
    pub verification: Option<CostVerificationOutcome>,
}

/// One per-run cost row: the `CostUsage` values verbatim from the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRow {
    /// The `CostUsage` record's stable ID.
    pub record_id: String,
    /// The owning `AgentRun` ID, resolved via the `AuthoredBy` edge.
    pub run_id: Option<String>,
    /// The owning `AgentSession` ID (denormalized on the node).
    pub session_id: Option<String>,
    /// The node's denormalized source handle (`path:blake3`).
    pub source_handle: Option<String>,
    /// Provider model name, verbatim from the transcript.
    pub model_name: Option<String>,
    /// BLAKE3 handle of the run's task text (see the schema doc).
    pub task_handle: Option<String>,
    /// Run verification outcome, verbatim from the transcript.
    pub verification_outcome: Option<String>,
    /// Billed cost in USD, verbatim from the transcript.
    pub actual_cost_usd: Option<f64>,
    /// Total cost in USD, verbatim from the transcript.
    pub total_cost_usd: Option<f64>,
    /// Baseline cost in USD, verbatim from the transcript.
    pub baseline_cost_usd: Option<f64>,
    /// Model the baseline cost was computed against.
    pub baseline_cost_model: Option<String>,
    /// Prompt (input) token count.
    pub prompt_tokens: Option<u64>,
    /// Cache-read token count.
    pub cache_read_tokens: Option<u64>,
    /// Completion (output) token count.
    pub completion_tokens: Option<u64>,
    /// Wall-clock run duration in seconds.
    pub duration_secs: Option<f64>,
    /// Derivation label: always `"transcript_derived"`.
    pub derivation: String,
    /// Trust class: always `"other"`.
    pub trust_class: String,
}

/// Aggregate sums over the full filtered set (pre-`--limit`). A sum is `None`
/// when no matching row carries that measurement — an empty or all-unknown
/// set never sums to zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CostTotals {
    /// Number of rows in the filtered set (before `--limit` truncation).
    pub matching_rows: usize,
    /// `true` when `--limit` truncated the returned rows (totals still cover
    /// the full filtered set).
    pub rows_truncated: bool,
    /// Sum of present `actual_cost_usd` values.
    pub actual_cost_usd: Option<f64>,
    /// Sum of present `total_cost_usd` values.
    pub total_cost_usd: Option<f64>,
    /// Sum of present `baseline_cost_usd` values.
    pub baseline_cost_usd: Option<f64>,
    /// Sum of present `prompt_tokens` values.
    pub prompt_tokens: Option<u64>,
    /// Sum of present `cache_read_tokens` values.
    pub cache_read_tokens: Option<u64>,
    /// Sum of present `completion_tokens` values.
    pub completion_tokens: Option<u64>,
    /// Sum of present `duration_secs` values.
    pub duration_secs: Option<f64>,
}

/// A non-fatal diagnostic attached to the rollup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostDiagnostic {
    /// Machine-readable code (`unrecognized_cost_payload`,
    /// `cost_usage_without_run_edge`).
    pub code: String,
    /// The `CostUsage` record this diagnostic is about.
    pub record_id: String,
    /// Human-readable explanation.
    pub message: String,
}

/// The full cost rollup answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRollup {
    /// Per-run rows, sorted by `record_id`, truncated to `limit`.
    pub rows: Vec<CostRow>,
    /// Aggregate sums over the full filtered set.
    pub totals: CostTotals,
    /// Non-fatal diagnostics, sorted by `record_id`.
    pub diagnostics: Vec<CostDiagnostic>,
    /// [`COST_DISCLAIMER`], carried verbatim.
    pub disclaimer: &'static str,
}

/// Bound a free-text field for rendered text output (issue #104 discipline).
#[must_use]
pub fn bounded_cost_text(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().take(COST_FIELD_MAX_CHARS) {
        out.push(if ch.is_control() { '·' } else { ch });
    }
    if value.chars().count() > COST_FIELD_MAX_CHARS {
        out.push(TRUNCATION_MARKER);
    }
    out
}

/// Compute the per-run cost rollup over `records`.
///
/// Rows come only from `CostUsage` nodes whose `text` parses as the canonical
/// [`CostUsagePayload`] with the transcript derivation label. `CostUsage`
/// nodes from other importers (e.g. the per-turn Codex/Claude-Code shapes)
/// are excluded from rows and reported in `diagnostics` — never silently
/// dropped and never misread as run-level cost.
#[must_use]
pub fn cost_rollup(records: &[GraphRecord], filters: &CostFilters, limit: usize) -> CostRollup {
    let mut rows: Vec<CostRow> = Vec::new();
    let mut diagnostics: Vec<CostDiagnostic> = Vec::new();

    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            text,
            session_id,
            source_handle,
            ..
        } = record
        else {
            continue;
        };
        if *kind != NodeKind::CostUsage {
            continue;
        }
        let payload: CostUsagePayload = match text
            .as_deref()
            .and_then(|t| serde_json::from_str::<CostUsagePayload>(t).ok())
        {
            Some(p) if p.derivation == COST_USAGE_DERIVATION_TRANSCRIPT => p,
            _ => {
                diagnostics.push(CostDiagnostic {
                    code: "unrecognized_cost_payload".to_owned(),
                    record_id: id.clone(),
                    message: "CostUsage text is not the canonical transcript-derived \
                              payload; excluded from the rollup rather than misread"
                        .to_owned(),
                });
                continue;
            }
        };

        if let Some(task_filter) = &filters.task
            && !payload
                .task_handle
                .as_deref()
                .is_some_and(|h| h.starts_with(task_filter))
        {
            continue;
        }
        if let Some(session_filter) = &filters.session
            && !session_id
                .as_deref()
                .is_some_and(|s| s.starts_with(session_filter))
        {
            continue;
        }
        if let Some(wanted) = filters.verification {
            let outcome = payload.verification_outcome.as_deref().unwrap_or("");
            let matches = match wanted {
                CostVerificationOutcome::Verified => outcome.eq_ignore_ascii_case("verified"),
                CostVerificationOutcome::Failed => outcome.eq_ignore_ascii_case("failed"),
            };
            if !matches {
                continue;
            }
        }

        let run_id = authored_by_run(records, id);
        if run_id.is_none() {
            diagnostics.push(CostDiagnostic {
                code: "cost_usage_without_run_edge".to_owned(),
                record_id: id.clone(),
                message: "CostUsage has no AuthoredBy edge to an AgentRun; \
                          run_id is unknown"
                    .to_owned(),
            });
        }

        rows.push(CostRow {
            record_id: id.clone(),
            run_id,
            session_id: session_id.clone(),
            source_handle: source_handle.clone(),
            model_name: payload.model_name,
            task_handle: payload.task_handle,
            verification_outcome: payload.verification_outcome,
            actual_cost_usd: payload.actual_cost_usd,
            total_cost_usd: payload.total_cost_usd,
            baseline_cost_usd: payload.baseline_cost_usd,
            baseline_cost_model: payload.baseline_cost_model,
            prompt_tokens: payload.prompt_tokens,
            cache_read_tokens: payload.cache_read_tokens,
            completion_tokens: payload.completion_tokens,
            duration_secs: payload.duration_secs,
            derivation: payload.derivation,
            trust_class: COST_TRUST_CLASS.to_owned(),
        });
    }

    rows.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    diagnostics.sort_by(|a, b| a.record_id.cmp(&b.record_id));

    let matching_rows = rows.len();
    let rows_truncated = rows.len() > limit;
    let totals = sum_rows(&rows);
    let totals = CostTotals {
        matching_rows,
        rows_truncated,
        ..totals
    };
    rows.truncate(limit);

    CostRollup {
        rows,
        totals,
        diagnostics,
        disclaimer: COST_DISCLAIMER,
    }
}

/// Resolve the `AgentRun` a `CostUsage` node belongs to via its `AuthoredBy`
/// edge (edge-derived membership, never a matching ID string).
fn authored_by_run(records: &[GraphRecord], cost_id: &str) -> Option<String> {
    records.iter().find_map(|r| {
        let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = r
        else {
            return None;
        };
        (*label == EdgeLabel::AuthoredBy && source == cost_id).then(|| target.clone())
    })
}

/// Sum the present values of every matching row. A measurement no row carries
/// stays `None` (unknown) — an empty or all-unknown set never sums to zero.
fn sum_rows(rows: &[CostRow]) -> CostTotals {
    let mut totals = CostTotals::default();
    for row in rows {
        totals.actual_cost_usd = add_opt_f64(totals.actual_cost_usd, row.actual_cost_usd);
        totals.total_cost_usd = add_opt_f64(totals.total_cost_usd, row.total_cost_usd);
        totals.baseline_cost_usd = add_opt_f64(totals.baseline_cost_usd, row.baseline_cost_usd);
        totals.prompt_tokens = add_opt_u64(totals.prompt_tokens, row.prompt_tokens);
        totals.cache_read_tokens = add_opt_u64(totals.cache_read_tokens, row.cache_read_tokens);
        totals.completion_tokens = add_opt_u64(totals.completion_tokens, row.completion_tokens);
        totals.duration_secs = add_opt_f64(totals.duration_secs, row.duration_secs);
    }
    totals
}

fn add_opt_f64(acc: Option<f64>, v: Option<f64>) -> Option<f64> {
    match (acc, v) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

const fn add_opt_u64(acc: Option<u64>, v: Option<u64>) -> Option<u64> {
    match (acc, v) {
        // Overflow degrades the total to unknown rather than silently
        // saturating: a wrong number is worse than an honest null.
        (Some(a), Some(b)) => match a.checked_add(b) {
            Some(sum) => Some(sum),
            None => None,
        },
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}
