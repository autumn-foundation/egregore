//! Shared `--max-records` record-budget machinery for the composite evidence
//! answers (issue #211): `eg query context`, `eg query memory`,
//! `eg query failures`, and `eg query change-impact`.
//!
//! A record budget caps the total number of evidence records across every
//! section of an answer. Sections fill **sequentially in envelope order** and
//! each keeps the top-ranked prefix of its already-deterministic ordering —
//! the budget never re-sorts — and every omitted record is accounted for: a
//! section that loses records is rendered as
//! `{"returned": <kept>, "total": <natural>, "records": [...]}` instead of a
//! bare array, so zero records are ever silently dropped. A section that keeps
//! everything renders as the bare array, byte-identical to the un-budgeted
//! answer; a naturally-empty section stays exactly as it was (omitted when the
//! field is `skip_serializing_if`-gated), while a non-empty section budgeted
//! to zero renders `returned: 0, total: N` — distinguishable from absence.

use serde::Serialize;

/// One evidence section of a composite answer under a record budget.
///
/// [`BudgetedSection::Full`] serializes as the bare record array (nothing
/// omitted — byte-identical to the un-budgeted answer);
/// [`BudgetedSection::truncated`] serializes as the
/// `{"returned","total","records"}` object accounting for every omission.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum BudgetedSection<T> {
    /// No records omitted: the bare array.
    Full(Vec<T>),
    /// Records omitted: the kept prefix plus the omission counts.
    Truncated {
        /// Records kept in this section (the top-ranked prefix).
        returned: usize,
        /// Records this section held before the budget was applied.
        total: usize,
        /// The kept records.
        records: Vec<T>,
    },
}

impl<T> BudgetedSection<T> {
    /// Sequential-fill one section against a shared remaining budget: the
    /// section keeps the top-ranked `min(len, remaining)` prefix and the
    /// remainder flows to the next section. `records` must already be in the
    /// command's deterministic order — the budget truncates the prefix and
    /// never re-sorts.
    #[must_use]
    pub(crate) fn apply(mut records: Vec<T>, remaining: &mut usize) -> Self {
        let total = records.len();
        let kept = total.min(*remaining);
        *remaining -= kept;
        if kept == total {
            Self::Full(records)
        } else {
            records.truncate(kept);
            Self::Truncated {
                returned: kept,
                total,
                records,
            }
        }
    }

    /// Records this section contributes to the answer's total.
    pub(crate) const fn returned(&self) -> usize {
        match self {
            Self::Full(records) => records.len(),
            Self::Truncated { returned, .. } => *returned,
        }
    }

    /// Whether the budget omitted any records from this section.
    pub(crate) const fn was_truncated(&self) -> bool {
        matches!(self, Self::Truncated { .. })
    }

    /// For `skip_serializing_if`: a naturally-empty section stays omitted,
    /// exactly as before the budget existed. A truncated-to-zero section is
    /// never empty — it carries `returned: 0, total: N` so the omission is
    /// accounted for.
    pub(crate) const fn is_empty(&self) -> bool {
        matches!(self, Self::Full(records) if records.is_empty())
    }
}

/// A `--max-records` budget shared across one answer's sections.
///
/// `None` (flag omitted) means unbounded: every section renders
/// [`BudgetedSection::Full`] and the answer is byte-identical to the
/// un-budgeted one.
pub(crate) struct RecordBudget {
    remaining: usize,
}

impl RecordBudget {
    /// Build the budget from the parsed `--max-records` flag value.
    #[must_use]
    pub(crate) fn new(max_records: Option<usize>) -> Self {
        Self {
            remaining: max_records.unwrap_or(usize::MAX),
        }
    }

    /// Fill one evidence section against the remaining budget, in call order.
    pub(crate) fn section<T>(&mut self, records: Vec<T>) -> BudgetedSection<T> {
        BudgetedSection::apply(records, &mut self.remaining)
    }
}
