//! Answer completeness signal (issue #121).
//!
//! Every `eg query` JSON answer carries a stable, documented completeness
//! signal so machine consumers can tell a complete answer from one a limit or
//! a single-winner selection silently truncated. The signal is stamped
//! per-row, following the codebase's dominant per-row stamping convention
//! (`corpus_mode`, `freshness`, `extraction_completeness`, `confidence_band`
//! are all per-row).
//!
//! The rule is uniform across lanes:
//!
//! - `result_complete` is `true` exactly when the printed row set IS the full
//!   match set — nothing was hidden by a cap or a selection.
//! - `total_matches` and `applied_limit` are reported if and only if a cap or
//!   selection actually narrowed the set (`rows_printed < candidates`). Their
//!   presence is therefore equivalent to `result_complete == false`; when the
//!   answer is complete they are omitted, and `result_complete` is the
//!   authoritative "more available" signal.
//!
//! Constructors:
//!
//! - [`RowCompleteness::exhaustive`] — the lane lists every match; no cap or
//!   selection applied (`query file`, uncapped `query symbol`).
//! - [`RowCompleteness::capped`] — a top-`limit` cut narrowed
//!   `total_matches` candidates (`query drift`, `query semantic`,
//!   `query similar`).
//! - [`RowCompleteness::single_winner`] — one row was picked from
//!   `candidates` (`query symbol --at`, `query symbol --as-of`).
//!
//! The exit-2 no-match path prints no JSON answer at all, so it cannot
//! contradict the signal: a no-match outcome is definitionally complete with
//! zero total matches.

use serde::Serialize;

/// Per-row completeness stamp for `eg query` JSON answers (issue #121).
///
/// Serialized with `#[serde(flatten)]` into the row structs, so the fields
/// appear at the top level of each JSONL row: `result_complete` always, and
/// `total_matches` / `applied_limit` exactly when the answer was narrowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct RowCompleteness {
    /// `true` when the printed row set is the complete match set — no cap or
    /// selection hid further rows.
    pub(crate) result_complete: bool,
    /// Candidate count before the cap/selection narrowed the set. Present
    /// only when the answer was narrowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_matches: Option<usize>,
    /// The cap (`--limit` value, or `1` for a single-winner selection) that
    /// narrowed the set. Present only when the answer was narrowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) applied_limit: Option<usize>,
}

impl RowCompleteness {
    /// Exhaustive answer: no cap or selection narrowed the match set.
    pub(crate) const fn exhaustive() -> Self {
        Self {
            result_complete: true,
            total_matches: None,
            applied_limit: None,
        }
    }

    /// Top-`limit` answer: `total_matches` candidates were narrowed to
    /// `limit` rows. Collapses to [`RowCompleteness::exhaustive`] when the
    /// pool fits the limit, so a no-op cap never reports truncation.
    pub(crate) const fn capped(total_matches: usize, limit: usize) -> Self {
        if total_matches <= limit {
            Self::exhaustive()
        } else {
            Self {
                result_complete: false,
                total_matches: Some(total_matches),
                applied_limit: Some(limit),
            }
        }
    }

    /// Single-winner selection (issue #121, AC5): one row was picked from
    /// `candidates` (`query symbol --at` / `--as-of`). A lone candidate is
    /// not a narrowed answer.
    pub(crate) const fn single_winner(candidates: usize) -> Self {
        if candidates <= 1 {
            Self::exhaustive()
        } else {
            Self {
                result_complete: false,
                total_matches: Some(candidates),
                applied_limit: Some(1),
            }
        }
    }

    /// Rebuilds the stamp from a daemon verb `page` envelope (issue #121).
    /// Returns `None` when the daemon predates the signal — without the
    /// server-side pre-truncate pool the client cannot stamp anything
    /// truthful, so it stamps nothing rather than guessing.
    pub(crate) fn from_daemon_page(page: &serde_json::Value) -> Option<Self> {
        let total_matches = usize::try_from(page.get("total_matches")?.as_u64()?).ok()?;
        let applied_limit = usize::try_from(page.get("applied_limit")?.as_u64()?).ok()?;
        Some(Self::capped(total_matches, applied_limit))
    }
}

/// Merges a [`RowCompleteness`] stamp into a raw JSON row object — the daemon
/// `--daemon` transport prints untyped rows, so the stamp is merged
/// key-by-key instead of via `#[serde(flatten)]`. A no-op for non-objects.
pub(crate) fn stamp_json_row(row: &mut serde_json::Value, completeness: RowCompleteness) {
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    obj.insert(
        "result_complete".to_owned(),
        serde_json::Value::from(completeness.result_complete),
    );
    if let Some(total_matches) = completeness.total_matches {
        obj.insert(
            "total_matches".to_owned(),
            serde_json::Value::from(total_matches),
        );
    }
    if let Some(applied_limit) = completeness.applied_limit {
        obj.insert(
            "applied_limit".to_owned(),
            serde_json::Value::from(applied_limit),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_json(completeness: RowCompleteness) -> serde_json::Value {
        // Serialize through a flattened wrapper, mirroring the row structs.
        #[derive(Serialize)]
        struct Row {
            record_id: &'static str,
            #[serde(flatten)]
            completeness: RowCompleteness,
        }
        serde_json::to_value(Row {
            record_id: "r1",
            completeness,
        })
        .expect("serializes")
    }

    #[test]
    fn exhaustive_reports_complete_without_totals() {
        let json = to_json(RowCompleteness::exhaustive());
        assert_eq!(json["result_complete"], true);
        assert!(json.get("total_matches").is_none());
        assert!(json.get("applied_limit").is_none());
        assert_eq!(json["record_id"], "r1", "flatten keeps sibling fields");
    }

    #[test]
    fn capped_reports_truncation_with_totals() {
        let json = to_json(RowCompleteness::capped(7, 3));
        assert_eq!(json["result_complete"], false);
        assert_eq!(json["total_matches"], 7);
        assert_eq!(json["applied_limit"], 3);
    }

    #[test]
    fn capped_collapses_when_pool_fits_limit() {
        for (total, limit) in [(3, 10), (3, 3), (0, 10)] {
            let json = to_json(RowCompleteness::capped(total, limit));
            assert_eq!(json["result_complete"], true, "total={total} limit={limit}");
            assert!(json.get("total_matches").is_none());
            assert!(json.get("applied_limit").is_none());
        }
    }

    #[test]
    fn single_winner_reports_selection() {
        let json = to_json(RowCompleteness::single_winner(4));
        assert_eq!(json["result_complete"], false);
        assert_eq!(json["total_matches"], 4);
        assert_eq!(json["applied_limit"], 1);
    }

    #[test]
    fn single_winner_collapses_for_zero_or_one_candidate() {
        for candidates in [0, 1] {
            let json = to_json(RowCompleteness::single_winner(candidates));
            assert_eq!(json["result_complete"], true, "candidates={candidates}");
            assert!(json.get("total_matches").is_none());
            assert!(json.get("applied_limit").is_none());
        }
    }

    #[test]
    fn from_daemon_page_rebuilds_stamp() {
        let page = serde_json::json!({ "total_matches": 9, "applied_limit": 5 });
        assert_eq!(
            RowCompleteness::from_daemon_page(&page),
            Some(RowCompleteness::capped(9, 5))
        );
    }

    #[test]
    fn from_daemon_page_returns_none_without_server_totals() {
        let page = serde_json::json!({ "cursor": null, "has_more": false, "returned": 2 });
        assert_eq!(RowCompleteness::from_daemon_page(&page), None);
    }

    #[test]
    fn stamp_json_row_merges_keys() {
        let mut row = serde_json::json!({ "record_id": "r1" });
        stamp_json_row(&mut row, RowCompleteness::capped(9, 5));
        assert_eq!(row["result_complete"], false);
        assert_eq!(row["total_matches"], 9);
        assert_eq!(row["applied_limit"], 5);
        assert_eq!(row["record_id"], "r1");
    }

    #[test]
    fn stamp_json_row_omits_totals_when_complete() {
        let mut row = serde_json::json!({ "record_id": "r1" });
        stamp_json_row(&mut row, RowCompleteness::exhaustive());
        assert_eq!(row["result_complete"], true);
        assert!(row.get("total_matches").is_none());
        assert!(row.get("applied_limit").is_none());
    }
}
