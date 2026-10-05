//! Token/byte-budgeted context packs (issue #131).
//!
//! [`assemble_context_pack`] fits a `query context` answer into a
//! caller-supplied token or byte budget and returns the pack as a JSON
//! string. It is pure and deterministic: no I/O, no store access, no
//! mutation — it serializes the section rows it is given and sheds the
//! lowest-priority rows until the measured pack fits the budget.
//!
//! Budget accounting reuses the pinned deterministic counting method from
//! issue #84 ([`crate::token_cost::count_tokens`], `word-punct-v1`); this
//! module defines no new counting method. Completeness accounting reuses the
//! `result_complete` + drop-account contract shape: a pack that shed rows
//! reports `result_complete: false` with the dropped count and IDs, and a
//! pack that kept everything reports `result_complete: true` with no drop
//! account — never a silent omission.
//!
//! Fill priority (drops happen in reverse): highest-trust citable facts
//! first. Within one trust class, sections shed in a fixed relevance order
//! with agent observations last; ties break on record ID, then original row
//! position, so the order is total and deterministic. A record is never
//! split: rows are kept or dropped whole, and a budget that cannot hold even
//! one whole record fails with [`PackTooSmall`] instead of returning a
//! half-record.

use anyhow::Result;
use serde::Serialize;

use crate::query::{StoreCoverage, TrustClass};
use crate::token_cost::{TOKEN_COUNT_METHOD, count_tokens};

use super::{ContextDrift, ContextSections, ExcludedDiagnostic};

// ---------------------------------------------------------------------------
// Caller-facing types
// ---------------------------------------------------------------------------

/// Caller-supplied size ceiling for a context pack (issue #131).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackBudget {
    /// Token ceiling counted with the pinned `word-punct-v1` method (#84).
    Tokens(usize),
    /// Byte ceiling counted over the rendered JSON's UTF-8 bytes.
    Bytes(usize),
}

impl PackBudget {
    /// The numeric ceiling.
    const fn limit(self) -> usize {
        match self {
            Self::Tokens(n) | Self::Bytes(n) => n,
        }
    }

    /// Stable wire string for the budget mode.
    const fn mode_str(self) -> &'static str {
        match self {
            Self::Tokens(_) => "tokens",
            Self::Bytes(_) => "bytes",
        }
    }

    /// Measures rendered pack text with this budget's counting method.
    fn measure(self, text: &str) -> usize {
        match self {
            Self::Tokens(_) => count_tokens(text),
            Self::Bytes(_) => text.len(),
        }
    }
}

/// Envelope fields that do not participate in budget shedding: they are
/// copied verbatim from the un-budgeted answer, so a pack that fits reports
/// sections identical to it.
pub(crate) struct PackFixed<'a> {
    /// The queried symbol's display name.
    pub symbol_name: &'a str,
    /// Non-fatal store-freshness code (issue #82), when computed.
    pub freshness: Option<&'static str>,
    /// Records excluded by supersession filtering (not by the budget).
    pub excluded: Vec<ExcludedDiagnostic<'a>>,
    /// Corpus the current-state view read (issue #427).
    pub corpus_mode: &'static str,
    /// How the corpus mode was chosen.
    pub corpus_mode_source: &'static str,
    /// One-line human description of the corpus that was read.
    pub corpus_disclaimer: String,
    /// Store-level trust-domain presence (issue #196).
    pub store_coverage: StoreCoverage,
}

/// Stable machine-readable diagnostic for a budget that cannot hold even one
/// whole record (issue #131, AC5). No half-record is ever returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackTooSmall {
    /// Stable code: always `"budget_too_small"`.
    pub code: &'static str,
    /// The queried symbol's display name.
    pub symbol_name: String,
    /// `"tokens"` or `"bytes"`.
    pub budget_mode: &'static str,
    /// The caller-supplied budget that failed.
    pub budget: usize,
    /// Measured size of the smallest possible accepted pack (fixed envelope
    /// plus the single highest-priority record): the minimum budget that
    /// could succeed.
    pub first_record_cost: usize,
    /// Human-readable explanation, built deterministically from the fields
    /// above.
    pub message: String,
}

impl PackTooSmall {
    /// Builds the diagnostic for a failed pack.
    fn new(symbol_name: &str, budget: PackBudget, first_record_cost: usize) -> Self {
        let mode = budget.mode_str();
        let limit = budget.limit();
        Self {
            code: "budget_too_small",
            symbol_name: symbol_name.to_owned(),
            budget_mode: mode,
            budget: limit,
            first_record_cost,
            message: format!(
                "budget of {limit} {mode} cannot hold even one whole record; \
                 the smallest possible pack costs {first_record_cost} {mode}"
            ),
        }
    }

    /// The `{"ok": false, "error": {...}}` envelope the CLI prints to stdout
    /// before exiting 3.
    #[must_use]
    pub(crate) fn to_envelope(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": false,
            "error": {
                "code": self.code,
                "symbol_name": self.symbol_name,
                "budget_mode": self.budget_mode,
                "budget": self.budget,
                "first_record_cost": self.first_record_cost,
                "message": self.message,
            }
        })
    }
}

impl std::fmt::Display for PackTooSmall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PackTooSmall {}

// ---------------------------------------------------------------------------
// Fill priority
// ---------------------------------------------------------------------------

/// Pack fill-priority rank over the closed trust vocabulary (issue #114).
///
/// The vocabulary itself is deliberately unordered; the pack imposes this
/// documented ranking so budget pressure sheds the least-trustworthy rows
/// first. Deterministic code facts always outrank every agent-authored
/// class, so pressure can never promote a guess into source truth, and a
/// code fact is never dropped while any lower-trust row is still kept.
const fn trust_priority(class: TrustClass) -> u8 {
    match class {
        TrustClass::SourceDerived => 0,
        TrustClass::VerificationEvidence => 1,
        TrustClass::AgentVerified => 2,
        TrustClass::ProjectState => 3,
        TrustClass::Artifact => 4,
        TrustClass::RuntimeObservation => 5,
        TrustClass::AgentUnverified => 6,
        TrustClass::AgentContradicted => 7,
        TrustClass::Other => 8,
    }
}

/// The pack's sections, in envelope order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackSection {
    SourceFacts,
    TopologyEdges,
    Observations,
    Decisions,
    ProjectState,
    Artifacts,
    VerificationEvidence,
    DriftHistory,
    Unresolved,
    Policy,
}

/// Relevance order within one trust class (lower sheds later). Agent
/// observations shed first: a committed decision with rationale outranks a
/// free-form hypothesis, and an unresolved reference — a pointer to content
/// absent from the answer — outranks neither. Unresolved rows carry no trust
/// label of their own and rank as `other`.
const fn section_priority(section: PackSection) -> u8 {
    match section {
        PackSection::SourceFacts => 0,
        PackSection::TopologyEdges => 1,
        PackSection::VerificationEvidence => 2,
        PackSection::Decisions => 3,
        PackSection::ProjectState => 4,
        PackSection::Artifacts => 5,
        PackSection::DriftHistory => 6,
        PackSection::Unresolved => 7,
        PackSection::Policy => 8,
        PackSection::Observations => 9,
    }
}

// ---------------------------------------------------------------------------
// Pack assembly
// ---------------------------------------------------------------------------

/// One sheddable row: its fill-priority key plus where to find it.
struct PackCandidate<'a> {
    /// Total priority order (lower = kept longer): trust rank, section
    /// rank, record ID, section index, row index. Record IDs are unique per
    /// row, so the key is total even when IDs repeat (unresolved rows share
    /// their source record's ID — the row index disambiguates).
    key: (u8, u8, &'a str, usize, usize),
    /// Index into the builder's section list.
    section: usize,
    /// Index into the section's row list.
    row: usize,
    /// Stable identifier reported in the drop account.
    drop_id: &'a str,
}

/// Collects sections and their sheddable rows before packing.
struct PackBuilder<'a> {
    /// Sections in envelope order; every section is always present, even
    /// when empty, so a complete pack's sections match the un-budgeted
    /// answer exactly.
    sections: Vec<(PackSection, Vec<serde_json::Value>)>,
    candidates: Vec<PackCandidate<'a>>,
}

impl<'a> PackBuilder<'a> {
    fn new() -> Self {
        Self {
            sections: Vec::with_capacity(10),
            candidates: Vec::new(),
        }
    }

    /// Adds one section's rows. Each tuple is
    /// `(record_id, trust, drop_account_id, row)`; `drop_account_id` is the
    /// record ID except for unresolved rows, which are keyed by their
    /// source record's ID.
    ///
    /// AC8: the pack projects rows for citability, not raw payloads. The
    /// `text` field (raw transcript/command-output/patch body) is stripped
    /// from observation rows — the summary, provenance, and evidence links
    /// are the citable handles. This projection applies to the budgeted pack
    /// only; the un-budgeted `query context` output is unchanged.
    fn push<T: Serialize>(
        &mut self,
        section: PackSection,
        rows: Vec<(&'a str, TrustClass, &'a str, T)>,
    ) {
        let section_idx = self.sections.len();
        let mut values = Vec::with_capacity(rows.len());
        for (row_idx, (record_id, trust, drop_id, row)) in rows.into_iter().enumerate() {
            // Context rows use derived `Serialize` with string keys: cannot fail.
            let mut value =
                serde_json::to_value(&row).expect("context row serialization is infallible");
            // AC8: strip raw payloads from the pack projection.
            if section == PackSection::Observations
                && let Some(obj) = value.as_object_mut()
            {
                obj.remove("text");
            }
            values.push(value);
            self.candidates.push(PackCandidate {
                key: (
                    trust_priority(trust),
                    section_priority(section),
                    record_id,
                    section_idx,
                    row_idx,
                ),
                section: section_idx,
                row: row_idx,
                drop_id,
            });
        }
        self.sections.push((section, values));
    }
}

/// The rendered pack envelope: the un-budgeted answer's shape plus the
/// `budget` accounting block. Sections are always present as bare arrays —
/// never `skip_serializing_if`-gated — so a complete pack's sections are
/// identical to the un-budgeted answer's.
#[derive(Serialize)]
struct PackResponse<'x> {
    ok: bool,
    symbol_name: &'x str,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<&'static str>,
    source_facts: Vec<serde_json::Value>,
    // Mirrors the un-budgeted answer (`ContextResponse`): an empty
    // topology-edges section is omitted, not rendered as `[]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topology_edges: Vec<serde_json::Value>,
    observations: Vec<serde_json::Value>,
    decisions: Vec<serde_json::Value>,
    project_state: Vec<serde_json::Value>,
    artifacts: Vec<serde_json::Value>,
    verification_evidence: Vec<serde_json::Value>,
    drift_history: Vec<serde_json::Value>,
    unresolved: Vec<serde_json::Value>,
    policy: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<&'x ExcludedDiagnostic<'x>>,
    corpus_mode: &'static str,
    corpus_mode_source: &'static str,
    corpus_disclaimer: &'x str,
    store_coverage: StoreCoverage,
    budget: PackBudgetReport<'x>,
}

/// The `budget` accounting block: the caller's ceiling, the measured size,
/// and the completeness/drop contract.
#[derive(Serialize)]
struct PackBudgetReport<'x> {
    mode: &'static str,
    budget: usize,
    measured: usize,
    result_complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_count_method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drop_account: Option<PackDropAccount<'x>>,
}

/// Explicit drop account: which records the budget shed, in shed order
/// (lowest priority first). Present only when `result_complete` is false.
///
/// `dropped_count` counts shed ROWS, not unique record IDs. Each shed
/// candidate contributes one entry to `dropped_record_ids`. For unresolved
/// rows the entry is the `source_record_id`: this may duplicate the ID of a
/// kept source-fact row, because the unresolved entry is a separate row
/// (a dangling reference) from the source fact itself. The duplication is
/// honest — the source fact was kept, the unresolved reference was shed.
#[derive(Serialize)]
struct PackDropAccount<'x> {
    dropped_count: usize,
    dropped_record_ids: Vec<&'x str>,
}

/// Renders one pack for a keep-mask set and returns the text plus its
/// measured size under `budget`'s counting method.
///
/// `measured` is a fixed point: the field reports the size of the text that
/// carries it. Token mode converges in one extra pass (any integer renders
/// as a single `word-punct-v1` token, so filling in the value cannot change
/// the count); byte mode iterates until the digit count stabilizes, which
/// takes at most three passes for any realistic envelope.
fn render_pack(
    sections: &[(PackSection, Vec<serde_json::Value>)],
    masks: &[Vec<bool>],
    dropped: &[&str],
    fixed: &PackFixed<'_>,
    budget: PackBudget,
) -> (String, usize) {
    let mut measured = 0_usize;
    let mut text = render_pack_once(sections, masks, dropped, fixed, budget, measured);
    for _ in 0..8 {
        let next = budget.measure(&text);
        if next == measured {
            break;
        }
        measured = next;
        text = render_pack_once(sections, masks, dropped, fixed, budget, measured);
    }
    // The loop always converges (see above); the bound is a safety net, and
    // the result is deterministic either way.
    let converged = budget.measure(&text);
    (text, converged)
}

/// Single render pass with a fixed `measured` placeholder value.
#[allow(clippy::too_many_arguments)]
fn render_pack_once(
    sections: &[(PackSection, Vec<serde_json::Value>)],
    masks: &[Vec<bool>],
    dropped: &[&str],
    fixed: &PackFixed<'_>,
    budget: PackBudget,
    measured: usize,
) -> String {
    debug_assert_eq!(sections.len(), 10, "all pack sections are always pushed");
    let kept = |idx: usize| -> Vec<serde_json::Value> {
        sections[idx]
            .1
            .iter()
            .zip(masks[idx].iter())
            .filter(|(_, keep)| **keep)
            .map(|(value, _)| value.clone())
            .collect()
    };
    let result_complete = dropped.is_empty();
    let response = PackResponse {
        ok: true,
        symbol_name: fixed.symbol_name,
        freshness: fixed.freshness,
        source_facts: kept(0),
        topology_edges: kept(1),
        observations: kept(2),
        decisions: kept(3),
        project_state: kept(4),
        artifacts: kept(5),
        verification_evidence: kept(6),
        drift_history: kept(7),
        unresolved: kept(8),
        policy: kept(9),
        excluded: fixed.excluded.iter().collect(),
        corpus_mode: fixed.corpus_mode,
        corpus_mode_source: fixed.corpus_mode_source,
        corpus_disclaimer: fixed.corpus_disclaimer.as_str(),
        store_coverage: fixed.store_coverage,
        budget: PackBudgetReport {
            mode: budget.mode_str(),
            budget: budget.limit(),
            measured,
            result_complete,
            token_count_method: match budget {
                PackBudget::Tokens(_) => Some(TOKEN_COUNT_METHOD),
                PackBudget::Bytes(_) => None,
            },
            drop_account: if result_complete {
                None
            } else {
                Some(PackDropAccount {
                    dropped_count: dropped.len(),
                    dropped_record_ids: dropped.to_vec(),
                })
            },
        },
    };
    // `PackResponse` is derived `Serialize` over borrowed `&str`s and owned
    // JSON values: cannot fail.
    serde_json::to_string(&response).expect("pack serialization is infallible")
}

/// Moves the resolved answer rows into the pack builder, tagging each row
/// with its trust class and stable sort key. Split out of
/// [`assemble_context_pack`] so the fit loop stays readable.
fn push_answer_sections<'a>(
    builder: &mut PackBuilder<'a>,
    sections: ContextSections<'a>,
    drift_history: Vec<ContextDrift<'a>>,
) {
    builder.push(
        PackSection::SourceFacts,
        sections
            .source_facts
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::TopologyEdges,
        sections
            .topology_edges
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::Observations,
        sections
            .observations
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::Decisions,
        sections
            .decisions
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::ProjectState,
        sections
            .project_state
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::Artifacts,
        sections
            .artifacts
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::VerificationEvidence,
        sections
            .verification_evidence
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::DriftHistory,
        drift_history
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
    builder.push(
        PackSection::Unresolved,
        sections
            .unresolved
            .into_iter()
            .map(|row| {
                (
                    row.source_record_id,
                    TrustClass::Other,
                    row.source_record_id,
                    row,
                )
            })
            .collect(),
    );
    builder.push(
        PackSection::Policy,
        sections
            .policy
            .into_iter()
            .map(|row| (row.record_id, row.trust, row.record_id, row))
            .collect(),
    );
}

/// Fits a `query context` answer into a caller-supplied budget.
///
/// Rows shed worst-first in the documented priority order; kept rows stay in
/// their original section order, so a pack that sheds nothing is identical
/// to the un-budgeted answer's sections. Returns the rendered JSON pack, or
/// [`PackTooSmall`] when the budget cannot hold even one whole record —
/// a record is never split across the budget boundary.
pub(crate) fn assemble_context_pack<'a>(
    sections: ContextSections<'a>,
    drift_history: Vec<ContextDrift<'a>>,
    fixed: &PackFixed<'a>,
    budget: PackBudget,
) -> Result<String, PackTooSmall> {
    let mut builder = PackBuilder::<'a>::new();
    push_answer_sections(&mut builder, sections, drift_history);

    // Priority order, best first. The key is total (record IDs are unique
    // per row; the section/row index disambiguates the rest), so the fill
    // order is deterministic.
    builder.candidates.sort_by_key(|candidate| candidate.key);
    let PackBuilder {
        sections,
        candidates,
    } = builder;
    let total = candidates.len();

    // Keep-masks parallel to `sections`, one bool per row.
    let all_kept: Vec<Vec<bool>> = sections
        .iter()
        .map(|(_, rows)| vec![true; rows.len()])
        .collect();

    // Fast path: the whole answer already fits. One render; sections are
    // byte-identical to the un-budgeted answer.
    let (text, measured) = render_pack(&sections, &all_kept, &[], fixed, budget);
    if measured <= budget.limit() {
        return Ok(text);
    }

    // Fail fast when there is nothing to pack: the full render above already
    // measured the fixed envelope.
    if total == 0 {
        return Err(PackTooSmall::new(fixed.symbol_name, budget, measured));
    }

    // Evaluate every nonempty prefix in priority order (best first) and keep
    // the largest one that fits. Pack size is NOT monotonic in the prefix
    // length: shedding a record removes its row bytes but adds its ID to the
    // explicit drop account, so a shorter prefix can measure larger than a
    // longer one. Greedy worst-first shedding can therefore miss a fitting
    // pack; exhaustive prefix evaluation cannot.
    //
    // The drop account lists shed IDs worst-first (reverse priority order),
    // matching the shed sequence a greedy loop would produce.
    let mut best_fit: Option<String> = None;
    for prefix_len in 1..=total {
        let mut masks: Vec<Vec<bool>> = sections
            .iter()
            .map(|(_, rows)| vec![false; rows.len()])
            .collect();
        for candidate in candidates.iter().take(prefix_len) {
            masks[candidate.section][candidate.row] = true;
        }
        let dropped: Vec<&'a str> = candidates[prefix_len..]
            .iter()
            .rev()
            .map(|candidate| candidate.drop_id)
            .collect();
        let (text, measured) = render_pack(&sections, &masks, &dropped, fixed, budget);
        if measured <= budget.limit() {
            best_fit = Some(text);
        }
    }
    best_fit.map_or_else(
        || {
            // No nonempty prefix fits: the budget cannot hold even one whole
            // record. Report the cost of the minimal one-record pack (best
            // record only) so the caller knows the floor.
            let mut minimal_masks: Vec<Vec<bool>> = sections
                .iter()
                .map(|(_, rows)| vec![false; rows.len()])
                .collect();
            let best = &candidates[0];
            minimal_masks[best.section][best.row] = true;
            let minimal_dropped: Vec<&'a str> = candidates[1..]
                .iter()
                .rev()
                .map(|candidate| candidate.drop_id)
                .collect();
            let (_, minimal_measured) =
                render_pack(&sections, &minimal_masks, &minimal_dropped, fixed, budget);
            Err(PackTooSmall::new(
                fixed.symbol_name,
                budget,
                minimal_measured,
            ))
        },
        Ok,
    )
}

/// CLI entry point for a budgeted `query context` pack (issue #131).
///
/// Takes the same fully-resolved answer pieces as the un-budgeted
/// [`super::context::query_context_cmd`] — same corpus, trust index,
/// supersession, drift, freshness, and section builders — and renders a
/// size-bounded pack instead of the full answer.
///
/// The pack prints as compact JSON (the budget counts rendered bytes, so the
/// printed text is exactly what was measured). A budget that cannot hold
/// even one whole record prints the stable
/// `{"ok":false,"error":{"code":"budget_too_small",...}}` envelope to stdout
/// and exits `3`; no half-record is ever returned.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_context_pack(
    display_name: &str,
    freshness_code: Option<&'static str>,
    sections: ContextSections<'_>,
    drift_history: Vec<ContextDrift<'_>>,
    excluded: Vec<ExcludedDiagnostic<'_>>,
    corpus_mode: crate::query::CorpusMode,
    corpus_mode_source: crate::query::CorpusModeSource,
    corpus_disclaimer: String,
    store_coverage: StoreCoverage,
    budget: PackBudget,
) -> Result<()> {
    let fixed = PackFixed {
        symbol_name: display_name,
        freshness: freshness_code,
        excluded,
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer,
        store_coverage,
    };
    match assemble_context_pack(sections, drift_history, &fixed, budget) {
        Ok(pack) => {
            // `print!`, not `println!`: the budget measured exactly these
            // bytes, so stdout must not gain a trailing newline.
            print!("{pack}");
            Ok(())
        }
        Err(too_small) => {
            print!("{}", serde_json::to_string(&too_small.to_envelope())?);
            std::process::exit(3);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{
        ContextSections, ExcludedDiagnostic, context_decision, context_linked_item,
        context_observation, context_source_fact,
    };
    use crate::ir::{GraphRecord, NodeKind};
    use crate::query::{self, TrustClass, TrustIndex};
    use crate::token_cost::count_tokens;

    use super::{PackBudget, PackFixed, PackTooSmall, assemble_context_pack};

    // ── fixtures ────────────────────────────────────────────────────────────

    fn symbol_record(id: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn observation_record(id: &str, agent: &str, session: &str, note: &str) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            note.to_owned(),
        );
        if let GraphRecord::Node {
            agent_id,
            session_id,
            observed_at,
            confidence,
            ..
        } = &mut rec
        {
            *agent_id = Some(agent.to_owned());
            *session_id = Some(session.to_owned());
            *observed_at = Some("2026-09-29T12:00:00Z".to_owned());
            *confidence = Some("0.7".to_owned());
        }
        rec
    }

    fn decision_record(id: &str, agent: &str) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            NodeKind::Decision,
            None,
            None,
            None,
            "chose the simple parser".to_owned(),
        );
        if let GraphRecord::Node {
            decision_text,
            rationale_summary,
            agent_id,
            observed_at,
            confidence,
            ..
        } = &mut rec
        {
            *decision_text = Some("use the simple parser".to_owned());
            *rationale_summary = Some("fewer moving parts".to_owned());
            *agent_id = Some(agent.to_owned());
            *observed_at = Some("2026-09-29T11:00:00Z".to_owned());
            *confidence = Some("0.9".to_owned());
        }
        rec
    }

    fn task_record(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Task,
            None,
            None,
            Some("ship it".to_owned()),
            "task ship it".to_owned(),
        )
    }

    /// Two source facts, one decision, one task, two (wordy) observations.
    fn fixture_records() -> Vec<GraphRecord> {
        vec![
            symbol_record("codegraph:v1:sym:alpha", "alpha"),
            symbol_record("codegraph:v1:sym:beta", "beta"),
            decision_record("agent:v1:dec:1", "vesper"),
            task_record("project:v1:task:9"),
            observation_record(
                "agent:v1:obs:1",
                "vesper",
                "sess-1",
                &"the alpha symbol looks suspicious ".repeat(12),
            ),
            observation_record(
                "agent:v1:obs:2",
                "vesper",
                "sess-2",
                &"beta might be dead code ".repeat(12),
            ),
        ]
    }

    fn build_sections<'a>(
        records: &'a [GraphRecord],
        trust: &'a TrustIndex,
    ) -> ContextSections<'a> {
        let mut sections = ContextSections {
            source_facts: Vec::new(),
            topology_edges: Vec::new(),
            observations: Vec::new(),
            decisions: Vec::new(),
            project_state: Vec::new(),
            artifacts: Vec::new(),
            verification_evidence: Vec::new(),
            unresolved: Vec::new(),
            policy: Vec::new(),
        };
        for record in records {
            match record {
                GraphRecord::Node { kind, .. } => match kind {
                    NodeKind::Symbol => {
                        if let Some(row) = context_source_fact(record, trust) {
                            sections.source_facts.push(row);
                        }
                    }
                    NodeKind::Observation => {
                        if let Some(row) = context_observation(record, trust) {
                            sections.observations.push(row);
                        }
                    }
                    NodeKind::Decision => {
                        if let Some(row) = context_decision(record, records, trust) {
                            sections.decisions.push(row);
                        }
                    }
                    NodeKind::Task => {
                        if let Some(row) = context_linked_item(record, trust) {
                            sections.project_state.push(row);
                        }
                    }
                    _ => {}
                },
                GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => {}
            }
        }
        sections
    }

    fn fixed<'a>() -> PackFixed<'a> {
        PackFixed {
            symbol_name: "alpha",
            freshness: None,
            excluded: Vec::<ExcludedDiagnostic>::new(),
            corpus_mode: "single_snapshot",
            corpus_mode_source: "default",
            corpus_disclaimer: String::new(),
            store_coverage: query::StoreCoverage::default(),
        }
    }

    fn pack_value(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("pack must be valid JSON")
    }

    fn record_ids(value: &serde_json::Value, section: &str) -> Vec<String> {
        value[section]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|row| row.get("record_id")?.as_str().map(str::to_owned))
            .collect()
    }

    // ── AC4: generous budget → complete pack, sections identical ──────────────

    #[test]
    fn generous_budget_returns_complete_pack_with_unbudgeted_sections() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let sections = build_sections(&records, &trust);

        // Snapshot every section before the pack consumes the rows. Each
        // section serializes to `serde_json::Value` first so the expected
        // table is homogeneous.
        //
        // AC8 projection: the pack strips the raw `text` field from
        // observations. The expected observations reflect this projection —
        // the pack is identical to the un-budgeted answer's sections except
        // for the AC8 redaction.
        let mut expected_observations = serde_json::to_value(&sections.observations).unwrap();
        if let Some(arr) = expected_observations.as_array_mut() {
            for row in arr {
                if let Some(obj) = row.as_object_mut() {
                    obj.remove("text");
                }
            }
        }
        let expected: Vec<(&str, serde_json::Value)> = vec![
            (
                "source_facts",
                serde_json::to_value(&sections.source_facts).unwrap(),
            ),
            (
                "topology_edges",
                serde_json::to_value(&sections.topology_edges).unwrap(),
            ),
            ("observations", expected_observations),
            (
                "decisions",
                serde_json::to_value(&sections.decisions).unwrap(),
            ),
            (
                "project_state",
                serde_json::to_value(&sections.project_state).unwrap(),
            ),
            (
                "artifacts",
                serde_json::to_value(&sections.artifacts).unwrap(),
            ),
            (
                "verification_evidence",
                serde_json::to_value(&sections.verification_evidence).unwrap(),
            ),
            (
                "unresolved",
                serde_json::to_value(&sections.unresolved).unwrap(),
            ),
            ("policy", serde_json::to_value(&sections.policy).unwrap()),
        ];

        let text = assemble_context_pack(
            sections,
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .expect("generous budget must succeed");
        let pack = pack_value(&text);

        assert_eq!(pack["ok"], true);
        assert_eq!(pack["symbol_name"], "alpha");
        let budget = &pack["budget"];
        assert_eq!(budget["mode"], "tokens");
        assert_eq!(budget["budget"], 1_000_000);
        assert_eq!(budget["result_complete"], true);
        assert!(
            budget.get("drop_account").is_none(),
            "a complete pack must carry no drop account"
        );
        // AC4: every section identical to the un-budgeted answer's — including
        // its quirks: the un-budgeted answer omits an empty topology-edges
        // section (`skip_serializing_if`), so the pack does too.
        for (name, expected_rows) in &expected {
            let actual = &pack[name];
            if *name == "topology_edges" && expected_rows.as_array().is_some_and(Vec::is_empty) {
                assert!(
                    actual.is_null(),
                    "empty topology_edges must be omitted like the un-budgeted answer"
                );
            } else {
                assert_eq!(
                    actual, expected_rows,
                    "section {name} must match the un-budgeted answer"
                );
            }
        }
    }

    // ── AC2/AC3: tight budget → drops, drop account, measured ≤ budget ───────

    #[test]
    fn tight_budget_drops_lowest_trust_first_with_explicit_drop_account() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let sections = build_sections(&records, &trust);

        let full = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        let full_tokens = count_tokens(&full);

        let text = assemble_context_pack(
            sections,
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(full_tokens - 1),
        )
        .expect("near-full budget must still succeed");
        let pack = pack_value(&text);
        let budget = &pack["budget"];

        assert_eq!(budget["result_complete"], false);
        let drop_account = budget
            .get("drop_account")
            .expect("an incomplete pack must carry a drop account");
        let dropped_count = drop_account["dropped_count"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap();
        assert!(dropped_count >= 1, "something must have been dropped");
        let dropped_ids: Vec<String> = drop_account["dropped_record_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(dropped_ids.len(), dropped_count);

        // AC6: under pressure the unverified observations shed first — every
        // dropped record is an observation, every code fact is retained.
        for id in &dropped_ids {
            assert!(
                id.starts_with("agent:v1:obs:"),
                "only lowest-trust observations may drop first, got {id}"
            );
        }
        let kept_facts = record_ids(&pack, "source_facts");
        assert_eq!(
            kept_facts,
            vec!["codegraph:v1:sym:alpha", "codegraph:v1:sym:beta"],
            "code facts must survive budget pressure"
        );

        // AC2: the measured pack never exceeds the budget.
        let measured = budget["measured"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap();
        assert_eq!(measured, count_tokens(&text));
        assert!(
            measured < full_tokens,
            "measured {measured} exceeds budget {full_tokens}"
        );
    }

    // ── AC5: no half-records; too-small budget → stable diagnostic ───────────

    #[test]
    fn prefix_evaluation_finds_fitting_pack_when_shorter_prefix_is_larger() {
        // Regression test for non-monotonic pack size: shedding a record
        // removes its row bytes but adds its ID to the drop account. A record
        // with a very long ID and small row content makes the one-record
        // prefix LARGER than the two-record prefix. A fail-fast on the
        // one-record pack would report budget_too_small; exhaustive prefix
        // evaluation finds the fitting two-record pack.
        let long_id = format!("codegraph:v1:sym:{}", "x".repeat(500));
        let records = vec![
            symbol_record("codegraph:v1:sym:alpha", "alpha"),
            symbol_record(&long_id, "beta"),
            observation_record("agent:v1:obs:1", "vesper", "sess-1", "short note"),
        ];
        let trust = TrustIndex::build(&records);

        let full = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        let full_tokens = count_tokens(&full);

        // Find the threshold where the full 3-record pack stops fitting.
        // Scan down; the last budget with 3 records kept is B3.
        let mut b3: Option<usize> = None;
        for budget in (1..=full_tokens).rev() {
            let text = assemble_context_pack(
                build_sections(&records, &trust),
                Vec::new(),
                &fixed(),
                PackBudget::Tokens(budget),
            )
            .expect("the full budget must succeed");
            let pack = pack_value(&text);
            let kept =
                record_ids(&pack, "source_facts").len() + record_ids(&pack, "observations").len();
            if kept == 3 {
                b3 = Some(budget);
                break;
            }
        }
        let b3 = b3.expect("the full pack must fit at its own token count");

        // One token below B3, the 3-record pack no longer fits. The assembler
        // must return the 2-record prefix — not budget_too_small. The old
        // fail-fast would check the 1-record prefix (which drops the 500-char
        // ID into the drop account, making it LARGER than the 2-record pack)
        // and incorrectly fail.
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(b3 - 1),
        )
        .expect("the 2-record prefix must fit when the 3-record pack does not");
        let pack = pack_value(&text);
        let kept_facts = record_ids(&pack, "source_facts");
        let kept_obs = record_ids(&pack, "observations");
        assert_eq!(
            kept_facts.len() + kept_obs.len(),
            2,
            "must keep exactly 2 records at budget {}",
            b3 - 1
        );
        assert_eq!(kept_facts.len(), 2, "both source facts must be kept");
        assert!(kept_obs.is_empty(), "the observation must be shed");

        // The drop account must list the shed observation (worst-first).
        let dropped = pack["budget"]["drop_account"]["dropped_record_ids"]
            .as_array()
            .expect("incomplete pack must carry a drop account");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].as_str().unwrap(), "agent:v1:obs:1");
    }

    #[test]
    fn budget_too_small_for_one_record_fails_with_stable_diagnostic() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);

        let err1 = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1),
        )
        .expect_err("a 1-token budget cannot hold any record");
        let err2 = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1),
        )
        .expect_err("the diagnostic must be deterministic");

        assert_eq!(err1, err2, "the diagnostic must be stable across runs");
        assert_eq!(err1.code, "budget_too_small");
        assert_eq!(err1.symbol_name, "alpha");
        assert_eq!(err1.budget_mode, "tokens");
        assert_eq!(err1.budget, 1);
        assert!(err1.first_record_cost > 1);
        assert!(!err1.message.is_empty());
    }

    #[test]
    fn records_are_never_split_across_the_budget_boundary() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);

        // Sweep budgets from tiny to full: every accepted pack must contain
        // only whole rows — each retained row's record_id resolves, and the
        // retained set is always a prefix of the priority order. Budgets
        // below the one-record minimum fail per AC5 instead of returning a
        // half-record.
        let full = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        let full_tokens = count_tokens(&full);
        let mut step = full_tokens / 8;
        if step == 0 {
            step = 1;
        }
        let mut budget = step;
        while budget < full_tokens {
            let text = match assemble_context_pack(
                build_sections(&records, &trust),
                Vec::new(),
                &fixed(),
                PackBudget::Tokens(budget),
            ) {
                Ok(text) => text,
                Err(too_small) => {
                    assert_eq!(too_small.code, "budget_too_small");
                    budget += step;
                    continue;
                }
            };
            let pack = pack_value(&text);
            // Every retained row is a complete JSON object with its stable ID.
            for section in ["source_facts", "observations", "decisions", "project_state"] {
                for row in pack[section].as_array().cloned().unwrap_or_default() {
                    assert!(
                        row.get("record_id").and_then(|v| v.as_str()).is_some(),
                        "retained rows must be whole records with stable IDs"
                    );
                }
            }
            let measured = pack["budget"]["measured"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap();
            assert!(
                measured <= budget,
                "measured {measured} exceeds budget {budget}"
            );
            budget += step;
        }
    }

    #[test]
    fn empty_sections_with_sufficient_budget_returns_complete_pack() {
        // A matched symbol with no projected rows: the pack is complete
        // (nothing was shed) with empty sections. The budget is irrelevant
        // beyond covering the fixed envelope.
        let records: Vec<GraphRecord> = Vec::new();
        let trust = TrustIndex::build(&records);
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .expect("empty sections must pack successfully with a generous budget");
        let pack = pack_value(&text);
        assert_eq!(pack["budget"]["result_complete"], true);
        assert!(pack["budget"].get("drop_account").is_none());
        for section in [
            "source_facts",
            "topology_edges",
            "observations",
            "decisions",
            "project_state",
            "artifacts",
            "verification_evidence",
            "drift_history",
            "unresolved",
            "policy",
        ] {
            let rows = pack[section].as_array().cloned().unwrap_or_default();
            assert!(rows.is_empty(), "section {section} must be empty");
        }
    }

    #[test]
    fn empty_sections_with_tiny_budget_fails_stable() {
        // Even with no rows, a budget that cannot cover the fixed envelope
        // fails with the stable budget_too_small diagnostic. The
        // first_record_cost reports the envelope cost (the minimum budget
        // for any pack).
        let records: Vec<GraphRecord> = Vec::new();
        let trust = TrustIndex::build(&records);
        let err = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1),
        )
        .expect_err("a 1-token budget cannot cover the envelope");
        assert_eq!(err.code, "budget_too_small");
        assert!(err.first_record_cost > 1);
    }

    // ── AC6/AC7: trust separation + citable handles ──────────────────────────

    #[test]
    fn observations_carry_provenance_and_never_enter_source_facts() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        let pack = pack_value(&text);

        let observations = pack["observations"].as_array().unwrap();
        assert_eq!(observations.len(), 2);
        for obs in observations {
            assert_eq!(obs["agent_id"], "vesper");
            assert!(obs["session_id"].as_str().is_some());
            assert_eq!(obs["observed_at"], "2026-09-29T12:00:00Z");
            assert_eq!(obs["confidence"], "0.7");
            assert!(
                obs["provenance_handle"].as_str().is_some() || obs["agent_id"].as_str().is_some(),
                "every observation must carry provenance"
            );
            // Unverified: the fixture cites no passing verification record.
            assert_eq!(obs["trust"], TrustClass::AgentUnverified.as_str());
        }

        // Agent observations never appear in the source-facts section.
        let fact_ids = record_ids(&pack, "source_facts");
        for obs in observations {
            let id = obs["record_id"].as_str().unwrap();
            assert!(
                !fact_ids.contains(&id.to_owned()),
                "observation {id} leaked into source_facts"
            );
        }
        // Every retained row carries a stable record ID (AC7), audited across
        // ALL sections — not just the four above. Each section's rows also
        // carry their citable handle:
        // - source_facts: repo_relative_path and/or span (path/span handle)
        // - topology_edges: record_id (edge ID)
        // - observations: provenance_handle or agent_id+session_id
        // - decisions: record_id
        // - project_state/artifacts/verification_evidence: record_id
        // - drift_history: record_id
        // - unresolved: source_record_id (the dangling reference's source)
        // - policy: record_id
        for section in [
            "source_facts",
            "topology_edges",
            "observations",
            "decisions",
            "project_state",
            "artifacts",
            "verification_evidence",
            "drift_history",
            "unresolved",
            "policy",
        ] {
            for row in pack[section].as_array().cloned().unwrap_or_default() {
                // Stable ID: record_id for all sections except unresolved,
                // which uses source_record_id.
                let id = if section == "unresolved" {
                    row["source_record_id"].as_str()
                } else {
                    row["record_id"].as_str()
                };
                assert!(
                    !id.unwrap_or("").is_empty(),
                    "every retained row in {section} needs a stable ID"
                );

                // Citable handle per section.
                match section {
                    "source_facts" => {
                        // Path/span handle: at least one of repo_relative_path
                        // or span must be present for code facts.
                        let has_path = row["repo_relative_path"].as_str().is_some();
                        let has_span = row["span"].is_object();
                        assert!(
                            has_path || has_span,
                            "source fact {} must carry a path/span handle",
                            id.unwrap()
                        );
                    }
                    "observations" => {
                        // Provenance handle: provenance_handle, or agent_id
                        // with session_id.
                        let has_provenance = row["provenance_handle"].as_str().is_some();
                        let has_agent = row["agent_id"].as_str().is_some()
                            && row["session_id"].as_str().is_some();
                        assert!(
                            has_provenance || has_agent,
                            "observation {} must carry provenance",
                            id.unwrap()
                        );
                    }
                    "unresolved" => {
                        // The dangling reference identifies its source and target.
                        assert!(
                            row["target_handle"].as_str().is_some(),
                            "unresolved {} must carry target_handle",
                            id.unwrap()
                        );
                    }
                    _ => {
                        // Other sections: record_id is the citable handle.
                    }
                }
            }
        }
    }

    // ── AC8: no raw payloads ────────────────────────────────────────────────

    #[test]
    fn pack_contains_no_raw_transcript_or_command_output() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        // The fixture summaries are short synthetic strings; the pack must
        // not smuggle raw bodies. Allowed: IDs, handles, spans, scores,
        // confidence, redaction markers.
        for forbidden in ["transcript", "stdout", "stderr", "patch_hunk", "env:"] {
            assert!(
                !text.contains(forbidden),
                "pack must not contain raw payload {forbidden:?}"
            );
        }
    }

    #[test]
    fn pack_excludes_hostile_raw_payloads_from_record_text() {
        // Crafted records with transcript/command-output/patch/issue-body/
        // environment/token-shaped data in the raw `text` field. The pack's
        // projection must exclude the raw payload — only the summary,
        // handles, and metadata are citable, never the raw body.
        let mut rec = observation_record(
            "agent:v1:obs:hostile",
            "vesper",
            "sess-99",
            "observed suspicious activity",
        );
        if let GraphRecord::Node { text, .. } = &mut rec {
            *text = Some(
                "transcript: user said 'hello'\nstdout: command output here\n\
                 stderr: error details\npatch_hunk: @@ -1,2 +1,2 @@\n\
                 env: SECRET_TOKEN=abc123\n\
                 issue body: the bug is in the parser\n\
                 protected payload: [REDACTED]"
                    .to_owned(),
            );
        }
        let records = vec![rec];
        let trust = TrustIndex::build(&records);
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        // The raw payload strings must NOT appear in the pack.
        for forbidden in [
            "transcript: user said",
            "stdout: command output",
            "stderr: error details",
            "patch_hunk:",
            "SECRET_TOKEN=abc123",
            "issue body: the bug",
            "protected payload:",
        ] {
            assert!(
                !text.contains(forbidden),
                "pack must not contain raw payload {forbidden:?}"
            );
        }
        // But the summary and citable handles MUST be present.
        assert!(text.contains("observed suspicious activity"));
        assert!(text.contains("agent:v1:obs:hostile"));
    }

    // ── AC9: determinism + byte-identity across runs ────────────────────────

    #[test]
    fn pack_is_byte_identical_across_five_runs() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let full = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        let full_tokens = count_tokens(&full);
        let budget = PackBudget::Tokens(full_tokens - 1);

        let first = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            budget,
        )
        .unwrap();
        for _ in 1..5 {
            let again = assemble_context_pack(
                build_sections(&records, &trust),
                Vec::new(),
                &fixed(),
                budget,
            )
            .unwrap();
            assert_eq!(again, first, "pack must be byte-identical across runs");
        }
    }

    // ── byte budgets ────────────────────────────────────────────────────────

    #[test]
    fn byte_budget_is_measured_as_utf8_length() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Bytes(10_000_000),
        )
        .unwrap();
        let pack = pack_value(&text);
        let budget = &pack["budget"];
        assert_eq!(budget["mode"], "bytes");
        assert_eq!(budget["budget"], 10_000_000);
        assert_eq!(budget["result_complete"], true);
        assert!(budget.get("token_count_method").is_none());
        let measured = budget["measured"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap();
        assert_eq!(measured, text.len(), "byte mode measures UTF-8 length");
    }

    #[test]
    fn tight_byte_budget_drops_and_reports_honestly() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let full = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Bytes(10_000_000),
        )
        .unwrap();
        let full_bytes = full.len();

        // NOTE: the budget limit is serialized inside the pack, so dropping
        // the limit from 8 digits (`10_000_000`) to fewer digits *saves*
        // bytes. Subtract well past any digit-count effect so the budget is
        // genuinely tight and must shed at least one row.
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Bytes(full_bytes - 50),
        )
        .unwrap();
        let pack = pack_value(&text);
        assert_eq!(pack["budget"]["result_complete"], false);
        assert!(
            pack["budget"]["drop_account"]["dropped_count"]
                .as_u64()
                .unwrap()
                >= 1
        );
        let measured = pack["budget"]["measured"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap();
        assert_eq!(measured, text.len());
        assert!(measured <= full_bytes - 50);
    }

    // ── pinned token method ─────────────────────────────────────────────────

    #[test]
    fn token_budget_uses_the_pinned_word_punct_method() {
        let records = fixture_records();
        let trust = TrustIndex::build(&records);
        let text = assemble_context_pack(
            build_sections(&records, &trust),
            Vec::new(),
            &fixed(),
            PackBudget::Tokens(1_000_000),
        )
        .unwrap();
        let pack = pack_value(&text);
        assert_eq!(pack["budget"]["token_count_method"], "word-punct-v1");
        let measured = pack["budget"]["measured"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap();
        assert_eq!(
            measured,
            count_tokens(&text),
            "measured tokens must use the pinned #84 method"
        );
    }

    // ── PackTooSmall is an honest value type ─────────────────────────────────

    #[test]
    fn pack_too_small_type_shape() {
        let err = PackTooSmall {
            code: "budget_too_small",
            symbol_name: "alpha".to_owned(),
            budget_mode: "tokens",
            budget: 1,
            first_record_cost: 42,
            message: "x".to_owned(),
        };
        assert_eq!(err.code, "budget_too_small");
    }

    // ── CLI error envelope ──────────────────────────────────────────────────

    #[test]
    fn too_small_envelope_is_stable_and_machine_readable() {
        let err = PackTooSmall {
            code: "budget_too_small",
            symbol_name: "alpha".to_owned(),
            budget_mode: "tokens",
            budget: 1,
            first_record_cost: 42,
            message: "budget of 1 tokens cannot hold even one whole record; \
                      the smallest possible pack costs 42 tokens"
                .to_owned(),
        };
        let envelope = err.to_envelope();
        assert_eq!(envelope["ok"], false);
        let error = &envelope["error"];
        assert_eq!(error["code"], "budget_too_small");
        assert_eq!(error["symbol_name"], "alpha");
        assert_eq!(error["budget_mode"], "tokens");
        assert_eq!(error["budget"], 1);
        assert_eq!(error["first_record_cost"], 42);
        assert!(error["message"].as_str().is_some());
        // Deterministic: re-rendering gives byte-identical JSON.
        let again = serde_json::to_string(&err.to_envelope()).unwrap();
        assert_eq!(serde_json::to_string(&envelope).unwrap(), again);
    }
}
