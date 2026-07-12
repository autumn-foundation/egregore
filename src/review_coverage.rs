//! `eg audit review-coverage` — a standing, citable review-coverage gate (issue #339).
//!
//! For every pull request merged in a half-open valid-time window `[from, to)`
//! (keyed on `merged_at`), this lane classifies the PR into the CLOSED
//! verdict-class set `{covered, approval_stale_head, self_approved_only,
//! uncovered}` and gates on the `covered / merged_prs` ratio.
//!
//! The per-PR classification is NOT computed here: it is delegated to the SHARED
//! [`crate::evidence_pack::derive_review_coverage`] so #338's evidence-pack
//! `review_coverage` section / `merged_pr_without_approving_review` gap and this
//! lane can never fork the logic (AC7). This module only shapes that derivation
//! into a deterministic, redaction-safe report plus the gate outcome.
//!
//! Substrate: GitHub PR/review facts from the #46 importer, promoted first-class
//! by #333 (`merged_at` / `head_sha` / `merge_commit_sha` / `system_native_id`),
//! anchored to reviewed commits by #334 (`review_commit_sha`). Identity nodes
//! (#335) are not yet available, so the non-author check compares recorded author
//! logins and degrades to an `identity_unavailable` sub-label when a login is
//! missing — never fabricating a self-approval and never inventing an identity
//! node kind.
//!
//! Pure and deterministic: no I/O, no network, no wall clock; byte-identical
//! across runs on an unchanged store.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::evidence_pack::{ReviewCoverageOptions, ReviewVerdict, Window, derive_review_coverage};
use crate::ir::GraphRecord;

/// The verbatim report disclaimer (AC). States the measurement's exact scope.
pub const REVIEW_COVERAGE_DISCLAIMER: &str = "measures recorded review execution captured in the graph; \
NOT GitHub branch-protection configuration, NOT review quality, and NOT whether unrecorded reviews \
happened elsewhere; absence of a recorded approving review means no imported evidence of one, not that \
no review occurred; never an auditor opinion";

/// One classified merged-in-window PR, serialized for the report (AC).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCoverageRowJson {
    /// The PR Task record ID (always cited).
    pub pr_task_id: String,
    /// The source-system-native PR handle (`system_native_id`), when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_native_id: Option<String>,
    /// The PR's merge commit SHA (`merge_commit_sha`), when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
    /// The resolved merge time (`merged_at`) used to window the PR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    /// The closed verdict class wire name.
    pub verdict: String,
    /// Sorted, closed-set sub-labels (`identity_unavailable`, `approval_unanchored`).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub sub_labels: Vec<String>,
    /// The deciding approving review record ID (covered rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approving_review_id: Option<String>,
    /// The deciding approving review's `review_commit_sha` (covered rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_commit_sha: Option<String>,
    /// The deciding approver login (covered rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approver_login: Option<String>,
    /// The deciding approver `ExternalIdentity` record ID — a reserved slot that
    /// is always present and serializes as an explicit `null` until issue #335
    /// lands identity nodes, so filling it later is not a breaking shape change.
    pub approver_identity_id: Option<String>,
}

/// A stable, redaction-safe report diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCoverageDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Record IDs the diagnostic derives from, sorted.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub record_ids: Vec<String>,
    /// Human-readable, redaction-safe detail.
    pub detail: String,
}

/// The strictness options echoed into the report (AC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportOptions {
    /// Whether an approving review must come from a non-author identity.
    pub require_non_author: bool,
    /// Whether the approval must be anchored at the PR's final head commit.
    pub require_final_head: bool,
}

/// The full deterministic `eg audit review-coverage` report (AC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCoverageReport {
    /// Whether the coverage gate passed (`covered / merged_prs >= min_coverage`).
    pub ok: bool,
    /// The measured valid-time window.
    pub window: Window,
    /// The strictness options in effect.
    pub options: ReportOptions,
    /// The `--min-coverage` threshold in effect.
    pub min_coverage: f64,
    /// Distinct merged-in-window PRs.
    pub merged_pr_count: usize,
    /// Merged PRs classified `covered`.
    pub covered_count: usize,
    /// Coverage fraction (`covered / merged`, vacuously 1.0 when none merged).
    pub coverage: f64,
    /// Per-verdict-class counts, every class present (0 when empty).
    pub verdict_counts: BTreeMap<String, usize>,
    /// One classified row per merged-in-window PR, sorted by PR Task record ID.
    pub rows: Vec<ReviewCoverageRowJson>,
    /// Stable, redaction-safe diagnostics.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<ReviewCoverageDiagnostic>,
    /// The verbatim always-present disclaimer.
    pub disclaimer: String,
}

/// Runs the review-coverage gate over a record set (issue #339).
///
/// Pure and deterministic: delegates classification to the shared
/// [`derive_review_coverage`], then applies the `min_coverage` gate. An empty
/// window (zero merged PRs) is an explicit vacuous pass (`empty_window`
/// diagnostic, `ok: true`).
#[must_use]
pub fn run_review_coverage(
    records: &[GraphRecord],
    window: &Window,
    options: ReviewCoverageOptions,
    min_coverage: f64,
) -> ReviewCoverageReport {
    let derivation = derive_review_coverage(records, window, options);

    let merged_pr_count = derivation.merged_pr_ids.len();
    let covered_count = derivation.covered_pr_ids.len();
    #[allow(clippy::cast_precision_loss)]
    let coverage = if merged_pr_count == 0 {
        1.0
    } else {
        covered_count as f64 / merged_pr_count as f64
    };

    // Every verdict class is always present in the tally (0 when unseen), so the
    // closed set is visible in the report regardless of the store.
    let mut verdict_counts: BTreeMap<String, usize> = ReviewVerdict::ALL
        .iter()
        .map(|v| (v.as_wire().to_owned(), 0usize))
        .collect();
    let rows: Vec<ReviewCoverageRowJson> = derivation
        .rows
        .iter()
        .map(|r| {
            *verdict_counts
                .entry(r.verdict.as_wire().to_owned())
                .or_insert(0) += 1;
            ReviewCoverageRowJson {
                pr_task_id: r.pr_task_id.clone(),
                system_native_id: r.system_native_id.clone(),
                merge_commit_sha: r.merge_commit_sha.clone(),
                merged_at: r.merged_at.clone(),
                verdict: r.verdict.as_wire().to_owned(),
                sub_labels: r.sub_labels.clone(),
                approving_review_id: r.approving_review_id.clone(),
                review_commit_sha: r.review_commit_sha.clone(),
                approver_login: r.approver_login.clone(),
                approver_identity_id: r.approver_identity_id.clone(),
            }
        })
        .collect();

    let mut diagnostics: Vec<ReviewCoverageDiagnostic> = Vec::new();

    // Empty window: an explicit vacuous pass, never an error.
    if merged_pr_count == 0 {
        diagnostics.push(ReviewCoverageDiagnostic {
            code: "empty_window".to_owned(),
            record_ids: Vec::new(),
            detail: "no pull requests merged in the window; coverage is vacuously satisfied"
                .to_owned(),
        });
    }

    // PRs that look merged but had no window-resolvable merge time are excluded
    // under a counted diagnostic (never windowed on update time).
    if !derivation.excluded_unresolvable_merge_time.is_empty() {
        diagnostics.push(ReviewCoverageDiagnostic {
            code: "excluded_unresolvable_merge_time".to_owned(),
            record_ids: derivation.excluded_unresolvable_merge_time.clone(),
            detail: "pull request(s) carry a merge_commit_sha but no window-resolvable merged_at; \
                     excluded from the merged set"
                .to_owned(),
        });
    }

    // #335 identity-signal limitation note: with identity nodes unavailable the
    // non-author check compares recorded author logins.
    if options.require_non_author {
        diagnostics.push(ReviewCoverageDiagnostic {
            code: "identity_signal_login_based".to_owned(),
            record_ids: Vec::new(),
            detail: "non-author check compares recorded author logins; ExternalIdentity nodes \
                     (issue #335) are unavailable, so a missing login degrades to the \
                     identity_unavailable sub-label rather than a fabricated self-approval"
                .to_owned(),
        });
    }

    let ok = coverage >= min_coverage;
    if !ok {
        // Name the ratio and the failing (non-covered) PR record IDs.
        let mut failing: Vec<String> = derivation
            .merged_pr_ids
            .iter()
            .filter(|id| !derivation.covered_pr_ids.contains(*id))
            .cloned()
            .collect();
        failing.sort();
        diagnostics.push(ReviewCoverageDiagnostic {
            code: "below_review_coverage_threshold".to_owned(),
            record_ids: failing,
            detail: format!(
                "review coverage {coverage:.4} is below the required minimum {min_coverage:.4}"
            ),
        });
    }

    diagnostics.sort_by(|a, b| {
        (a.code.as_str(), a.record_ids.first().map(String::as_str))
            .cmp(&(b.code.as_str(), b.record_ids.first().map(String::as_str)))
    });

    ReviewCoverageReport {
        ok,
        window: window.clone(),
        options: ReportOptions {
            require_non_author: options.require_non_author,
            require_final_head: options.require_final_head,
        },
        min_coverage,
        merged_pr_count,
        covered_count,
        coverage,
        verdict_counts,
        rows,
        diagnostics,
        disclaimer: REVIEW_COVERAGE_DISCLAIMER.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_pack::fixture;
    use crate::ir::GraphRecord;

    const FROM: &str = "2026-03-01T00:00:00Z";
    const TO: &str = "2026-04-01T00:00:00Z";

    fn win() -> Window {
        Window {
            from: FROM.to_owned(),
            to: TO.to_owned(),
        }
    }

    fn with_author(mut r: GraphRecord, login: &str) -> GraphRecord {
        if let GraphRecord::Node { author, .. } = &mut r {
            *author = Some(login.to_owned());
        }
        r
    }

    fn with_review_commit(mut r: GraphRecord, sha: &str) -> GraphRecord {
        if let GraphRecord::Node {
            review_commit_sha, ..
        } = &mut r
        {
            *review_commit_sha = Some(sha.to_owned());
        }
        r
    }

    fn with_system_native_id(mut r: GraphRecord, s: &str) -> GraphRecord {
        if let GraphRecord::Node {
            system_native_id, ..
        } = &mut r
        {
            *system_native_id = Some(s.to_owned());
        }
        r
    }

    fn without_head_sha(mut r: GraphRecord) -> GraphRecord {
        if let GraphRecord::Node { head_sha, .. } = &mut r {
            *head_sha = None;
        }
        r
    }

    fn head_sha_of(pr: &GraphRecord) -> String {
        match pr {
            GraphRecord::Node {
                head_sha: Some(h), ..
            } => h.clone(),
            _ => panic!("pr has no head_sha"),
        }
    }

    /// Builds a fixture with all four planted verdict cases plus the two
    /// degraded-covered sub-label cases (>=8 merged PRs).
    #[allow(clippy::too_many_lines)]
    fn planted_records() -> Vec<GraphRecord> {
        let mut records: Vec<GraphRecord> = Vec::new();

        // (a) covered: non-author approval at final head.
        let pr01 = with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr01", "2026-03-05T09:00:00Z", "c01"),
                "1",
            ),
            "author-1",
        );
        let r01 = with_review_commit(
            with_author(
                fixture::review("project:v1:r01", "2026-03-05T08:00:00Z", "approved"),
                "rev-1",
            ),
            &head_sha_of(&pr01),
        );
        records.push(fixture::references_task(
            "project:v1:r01",
            "project:v1:pr01",
        ));
        records.push(pr01);
        records.push(r01);

        // (a') covered: a second one for a non-degenerate ratio.
        let pr02 = with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr02", "2026-03-06T09:00:00Z", "c02"),
                "2",
            ),
            "author-2",
        );
        let r02 = with_review_commit(
            with_author(
                fixture::review("project:v1:r02", "2026-03-06T08:00:00Z", "approved"),
                "rev-2",
            ),
            &head_sha_of(&pr02),
        );
        records.push(fixture::references_task(
            "project:v1:r02",
            "project:v1:pr02",
        ));
        records.push(pr02);
        records.push(r02);

        // (b) approval_stale_head: non-author approval anchored to a non-head commit.
        let pr03 = with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr03", "2026-03-07T09:00:00Z", "c03"),
                "3",
            ),
            "author-3",
        );
        let r03 = with_review_commit(
            with_author(
                fixture::review("project:v1:r03", "2026-03-07T08:00:00Z", "approved"),
                "rev-3",
            ),
            "some-other-sha",
        );
        records.push(fixture::references_task(
            "project:v1:r03",
            "project:v1:pr03",
        ));
        records.push(pr03);
        records.push(r03);

        // (c) self_approved_only: only approval is by the PR author.
        let pr04 = with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr04", "2026-03-08T09:00:00Z", "c04"),
                "4",
            ),
            "author-4",
        );
        let r04 = with_review_commit(
            with_author(
                fixture::review("project:v1:r04", "2026-03-08T08:00:00Z", "approved"),
                "author-4",
            ),
            &head_sha_of(&pr04),
        );
        records.push(fixture::references_task(
            "project:v1:r04",
            "project:v1:pr04",
        ));
        records.push(pr04);
        records.push(r04);

        // (d) uncovered: merged with zero reviews.
        records.push(with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr05", "2026-03-09T09:00:00Z", "c05"),
                "5",
            ),
            "author-5",
        ));
        records.push(with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr06", "2026-03-10T09:00:00Z", "c06"),
                "6",
            ),
            "author-6",
        ));

        // covered with approval_unanchored sub-label (no review_commit_sha).
        let pr07 = with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr07", "2026-03-11T09:00:00Z", "c07"),
                "7",
            ),
            "author-7",
        );
        let r07 = with_author(
            fixture::review("project:v1:r07", "2026-03-11T08:00:00Z", "approved"),
            "rev-7",
        );
        records.push(fixture::references_task(
            "project:v1:r07",
            "project:v1:pr07",
        ));
        records.push(pr07);
        records.push(r07);

        // covered with identity_unavailable sub-label (review has no author login).
        let pr08 = with_author(
            with_system_native_id(
                fixture::pr("project:v1:pr08", "2026-03-12T09:00:00Z", "c08"),
                "8",
            ),
            "author-8",
        );
        let r08 = with_review_commit(
            fixture::review("project:v1:r08", "2026-03-12T08:00:00Z", "approved"),
            &head_sha_of(&pr08),
        );
        records.push(fixture::references_task(
            "project:v1:r08",
            "project:v1:pr08",
        ));
        records.push(pr08);
        records.push(r08);

        records
    }

    fn row<'a>(report: &'a ReviewCoverageReport, pr: &str) -> &'a ReviewCoverageRowJson {
        report
            .rows
            .iter()
            .find(|r| r.pr_task_id == pr)
            .unwrap_or_else(|| panic!("row for {pr} present"))
    }

    #[test]
    fn classifies_all_four_cases_into_the_closed_set() {
        let records = planted_records();
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        assert_eq!(report.merged_pr_count, 8);
        assert_eq!(row(&report, "project:v1:pr01").verdict, "covered");
        assert_eq!(row(&report, "project:v1:pr02").verdict, "covered");
        assert_eq!(
            row(&report, "project:v1:pr03").verdict,
            "approval_stale_head"
        );
        assert_eq!(
            row(&report, "project:v1:pr04").verdict,
            "self_approved_only"
        );
        assert_eq!(row(&report, "project:v1:pr05").verdict, "uncovered");
        assert_eq!(row(&report, "project:v1:pr06").verdict, "uncovered");
        assert_eq!(row(&report, "project:v1:pr07").verdict, "covered");
        assert_eq!(row(&report, "project:v1:pr08").verdict, "covered");
        // Every row is in the closed set; no misclassifications.
        let closed = [
            "covered",
            "approval_stale_head",
            "self_approved_only",
            "uncovered",
        ];
        for r in &report.rows {
            assert!(
                closed.contains(&r.verdict.as_str()),
                "verdict {}",
                r.verdict
            );
        }
        assert_eq!(report.verdict_counts["covered"], 4);
        assert_eq!(report.verdict_counts["approval_stale_head"], 1);
        assert_eq!(report.verdict_counts["self_approved_only"], 1);
        assert_eq!(report.verdict_counts["uncovered"], 2);
    }

    #[test]
    fn covered_rows_carry_the_full_citation_set() {
        let records = planted_records();
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        for pr in [
            "project:v1:pr01",
            "project:v1:pr02",
            "project:v1:pr07",
            "project:v1:pr08",
        ] {
            let r = row(&report, pr);
            assert!(r.system_native_id.is_some(), "{pr} system_native_id");
            assert!(r.merge_commit_sha.is_some(), "{pr} merge_commit_sha");
            assert!(r.approving_review_id.is_some(), "{pr} approving_review_id");
            assert!(
                r.approver_login.is_some()
                    || r.sub_labels.contains(&"identity_unavailable".to_owned())
            );
            // #335 identity nodes unavailable.
            assert!(r.approver_identity_id.is_none());
        }
        // pr01/pr02 anchored: review_commit_sha present.
        assert!(row(&report, "project:v1:pr01").review_commit_sha.is_some());
        // pr07 unanchored sub-label.
        assert!(
            row(&report, "project:v1:pr07")
                .sub_labels
                .contains(&"approval_unanchored".to_owned())
        );
        // pr08 identity_unavailable sub-label.
        assert!(
            row(&report, "project:v1:pr08")
                .sub_labels
                .contains(&"identity_unavailable".to_owned())
        );
    }

    #[test]
    fn every_row_carries_pr_identity_citations() {
        let records = planted_records();
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        for r in &report.rows {
            assert!(!r.pr_task_id.is_empty());
            assert!(r.system_native_id.is_some());
            assert!(r.merge_commit_sha.is_some());
        }
    }

    #[test]
    fn gate_fails_at_default_threshold_on_planted_gaps() {
        let records = planted_records();
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        assert!(!report.ok);
        assert_eq!(report.covered_count, 4);
        assert!((report.coverage - 0.5).abs() < 1e-9);
        let diag = report
            .diagnostics
            .iter()
            .find(|d| d.code == "below_review_coverage_threshold")
            .expect("below-threshold diagnostic present");
        // Names the failing (non-covered) PR record IDs.
        for pr in [
            "project:v1:pr03",
            "project:v1:pr04",
            "project:v1:pr05",
            "project:v1:pr06",
        ] {
            assert!(diag.record_ids.contains(&pr.to_owned()), "failing {pr}");
        }
    }

    #[test]
    fn gate_passes_on_fully_covered_store() {
        // Two PRs, each approved by a non-author review at the final head.
        let mut records: Vec<GraphRecord> = Vec::new();
        for (n, day) in [("pr01", "05"), ("pr02", "06")] {
            let pr = with_author(
                with_system_native_id(
                    fixture::pr(
                        &format!("project:v1:{n}"),
                        &format!("2026-03-{day}T09:00:00Z"),
                        n,
                    ),
                    n,
                ),
                &format!("author-{n}"),
            );
            let rv_id = format!("project:v1:rv-{n}");
            let rv = with_review_commit(
                with_author(
                    fixture::review(&rv_id, &format!("2026-03-{day}T08:00:00Z"), "approved"),
                    &format!("rev-{n}"),
                ),
                &head_sha_of(&pr),
            );
            records.push(fixture::references_task(&rv_id, &format!("project:v1:{n}")));
            records.push(pr);
            records.push(rv);
        }
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        assert!(report.ok, "diagnostics={:?}", report.diagnostics);
        assert_eq!(report.covered_count, 2);
        assert_eq!(report.merged_pr_count, 2);
    }

    #[test]
    fn empty_window_is_a_vacuous_pass() {
        let records = planted_records();
        let past = Window {
            from: "2025-01-01T00:00:00Z".to_owned(),
            to: "2025-02-01T00:00:00Z".to_owned(),
        };
        let report = run_review_coverage(&records, &past, ReviewCoverageOptions::default(), 1.0);
        assert!(report.ok);
        assert_eq!(report.merged_pr_count, 0);
        assert!(report.diagnostics.iter().any(|d| d.code == "empty_window"));
    }

    #[test]
    fn output_is_byte_identical_across_runs() {
        let records = planted_records();
        let a = serde_json::to_string(&run_review_coverage(
            &records,
            &win(),
            ReviewCoverageOptions::default(),
            1.0,
        ))
        .unwrap();
        for _ in 0..5 {
            let b = serde_json::to_string(&run_review_coverage(
                &records,
                &win(),
                ReviewCoverageOptions::default(),
                1.0,
            ))
            .unwrap();
            assert_eq!(a, b, "review-coverage output must be byte-identical");
        }
    }

    #[test]
    fn reserved_approver_identity_id_serializes_as_explicit_null() {
        // Pre-#335: every `approver_identity_id` is `None`, but the documented
        // contract reserves the slot as an always-present `null` field so adding
        // identity nodes later fills the slot rather than being a breaking shape
        // change. The key must appear even when the value is `None`.
        let records = planted_records();
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        assert!(!report.rows.is_empty(), "fixture must produce rows");
        for r in &report.rows {
            assert!(r.approver_identity_id.is_none(), "pre-#335 rows carry None");
            let json = serde_json::to_string(r).unwrap();
            assert!(
                json.contains("\"approver_identity_id\":null"),
                "reserved key must serialize as explicit null: {json}"
            );
        }
    }

    #[test]
    fn require_final_head_off_treats_stale_as_covered() {
        let records = planted_records();
        let options = ReviewCoverageOptions {
            require_non_author: true,
            require_final_head: false,
        };
        let report = run_review_coverage(&records, &win(), options, 1.0);
        // pr03 (stale) is now covered because the final-head knob is off.
        assert_eq!(row(&report, "project:v1:pr03").verdict, "covered");
    }

    #[test]
    fn anchored_approval_with_missing_head_sha_is_not_covered_under_require_final_head() {
        // A PR merged in-window with an ANCHORED approving review (has a
        // `review_commit_sha`) from a non-author, but the PR record carries NO
        // `head_sha` (e.g. a pre-#333 or partial import). Under
        // `--require-final-head` (default on) the final head cannot be confirmed,
        // so the approval must NOT be classified `covered` — it degrades to
        // `approval_stale_head` + `head_sha_unavailable`, never a silent pass
        // (Codex P2 false-`covered` bug).
        let pr = without_head_sha(with_author(
            with_system_native_id(
                fixture::pr("project:v1:prX", "2026-03-15T09:00:00Z", "cX"),
                "9",
            ),
            "author-X",
        ));
        let review = with_review_commit(
            with_author(
                fixture::review("project:v1:rX", "2026-03-15T08:00:00Z", "approved"),
                "rev-X",
            ),
            "any-anchor-sha",
        );
        let records = vec![
            fixture::references_task("project:v1:rX", "project:v1:prX"),
            pr,
            review,
        ];

        // require_final_head ON (default): unverifiable final head is not covered.
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        let r = row(&report, "project:v1:prX");
        assert_eq!(
            r.verdict, "approval_stale_head",
            "an unverifiable final head must not be classified covered"
        );
        assert!(
            r.sub_labels.iter().any(|s| s == "head_sha_unavailable"),
            "the missing-head_sha degradation must be reported, got {:?}",
            r.sub_labels
        );
        assert_eq!(report.covered_count, 0, "must not count toward covered");

        // require_final_head OFF (lenient/pack path): head is not checked, so the
        // same PR stays covered — #338 pack numbers and the AC7 divergence
        // fixture are unaffected.
        let lenient = ReviewCoverageOptions {
            require_non_author: true,
            require_final_head: false,
        };
        let report_off = run_review_coverage(&records, &win(), lenient, 1.0);
        assert_eq!(
            row(&report_off, "project:v1:prX").verdict,
            "covered",
            "with --require-final-head off the missing head_sha is not checked"
        );
    }

    #[test]
    fn self_approval_never_fabricated_when_identity_unavailable() {
        // A self-approval requires BOTH logins present and equal. With the review
        // login absent it must degrade to covered+identity_unavailable, never
        // self_approved_only.
        let records = planted_records();
        let report = run_review_coverage(&records, &win(), ReviewCoverageOptions::default(), 1.0);
        assert_eq!(row(&report, "project:v1:pr08").verdict, "covered");
        assert!(
            row(&report, "project:v1:pr08")
                .sub_labels
                .contains(&"identity_unavailable".to_owned())
        );
    }
}
