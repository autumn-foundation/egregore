//! Rank code targets by repeated agent-failure density (issue #254).
//!
//! The per-handle failure lanes answer "did anyone fail HERE?" — this lane
//! answers the operator's question BEFORE dispatching the next agent:
//! "which code targets burned the most agent attempts, across the whole
//! store?" It aggregates every live agent-authored `Failure` record, resolves
//! each to its code targets (`Symbol`/`File`) through the shared failure-link
//! relations (`FAILED_ON`, `TOUCHED_FILE`, `MENTIONS_SYMBOL`, `OBSERVES`, …,
//! including denormalized evidence links — the same semantics as
//! [`super::failure_history::outbound_code_task_targets`]), and ranks targets
//! by DISTINCT failing-run count, with total failure count as the documented
//! tie-break.
//!
//! Rank semantics, documented once here:
//!
//! * **Rank key** — `(distinct_run_count DESC, failure_count DESC,
//!   target_record_id ASC)`. The final `record_id` tie-break makes the ranking
//!   byte-identical across repeated runs on an unchanged store (issue #199).
//! * **Run key** — the failure's `session_id` provenance when present and
//!   non-empty; otherwise the failure's own record ID (it counts as its own
//!   run rather than collapsing into a phantom shared run).
//! * **Instant** — `executed_at` preferred, else `observed_at` (RFC 3339).
//!   Without temporal selectors, undated failures participate (the
//!   conservative default, mirroring the failure-history lane's
//!   `still_failing`); WITH `--as-of`/`--since`, undated failures are
//!   excluded — the lane cannot prove they were observed in the window.
//! * **Targets** — `Symbol`/`File` records only. A failure that resolves to
//!   task records alone is NOT a code hotspot; it lands in `unresolved` with
//!   reason `task_only`. A failure resolving to nothing live lands in
//!   `unresolved` with reason `no_code_target`. Nothing is silently dropped.
//! * **Liveness** — tombstoned (deleted) `Failure` records are excluded
//!   entirely, matching the current-state read (issue #421); they appear
//!   neither in the ranking nor in `unresolved`.
//!
//! Deliberately out of scope: diagnosing WHY a target fails (the lane ranks
//! and cites; root-causing stays with the agent), new `Failure` capture,
//! cross-store aggregation, and predictive scoring.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};

use super::failure_history::outbound_code_task_targets;
use super::liveness::Liveness;
use super::{is_codegraph_kind, record_node_kind};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

/// Default `--limit` for `eg query failure-hotspots`.
pub const FAILURE_HOTSPOTS_DEFAULT_LIMIT: usize = 20;
/// Maximum `--limit` for `eg query failure-hotspots`.
pub const FAILURE_HOTSPOTS_MAX_LIMIT: usize = 200;

/// Trust class for hotspot rows (issue #114): rows aggregate agent-authored
/// `Failure` claims, so every row is agent-authored evidence, never source
/// truth.
pub const FAILURE_HOTSPOT_TRUST: &str = "agent_authored";

/// Why [`failure_hotspots`] refused to run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FailureHotspotsError {
    /// The store holds zero live `Failure` records (after temporal filtering).
    /// The caller emits the standard `no_match` envelope (exit 2).
    #[error("no Failure records in the store")]
    NoFailures,
}

/// Why a failure landed in the `unresolved` section.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UnresolvedFailureReason {
    /// The failure links to no live code target at all.
    NoCodeTarget,
    /// The failure links to live records, but only task/project records —
    /// no `Symbol`/`File` target.
    TaskOnly,
}

impl UnresolvedFailureReason {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoCodeTarget => "no_code_target",
            Self::TaskOnly => "task_only",
        }
    }
}

/// One contributing failed attempt backing a hotspot row.
#[derive(Debug, Clone)]
pub struct ContributingFailure<'a> {
    /// The `Failure` record ID — citable evidence.
    pub record_id: &'a str,
    /// The aggregation key: `session_id` provenance, or the failure's own
    /// record ID when it carries none.
    pub run: String,
    /// Raw `session_id` provenance (`None` when the record carries none).
    pub session_id: Option<&'a str>,
    /// Raw `observed_at` provenance (`None` when absent).
    pub observed_at: Option<&'a str>,
    /// The failure classification, e.g. `command_failure` (`None` when absent).
    pub failure_kind: Option<&'a str>,
}

/// One ranked code target.
#[derive(Debug, Clone)]
pub struct FailureHotspotRow<'a> {
    /// 1-based rank assigned over the full ranking BEFORE `--limit`
    /// truncation, so a capped list never renumbers.
    pub rank: usize,
    /// The target `Symbol`/`File` record ID — citable handle.
    pub target_record_id: &'a str,
    /// `"symbol"` or `"file"`.
    pub target_kind: &'static str,
    /// Repo-relative path of the target, when the record carries one.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the target, when the record carries one.
    pub span: Option<SourceSpan>,
    /// Number of distinct agent runs that failed against this target.
    pub distinct_run_count: usize,
    /// Total contributing `Failure` records.
    pub failure_count: usize,
    /// The contributing failures, sorted by record ID.
    pub failures: Vec<ContributingFailure<'a>>,
    /// Trust class (issue #114): [`FAILURE_HOTSPOT_TRUST`].
    pub trust: &'static str,
}

/// A live `Failure` that resolved to no code target.
#[derive(Debug, Clone)]
pub struct UnresolvedFailure<'a> {
    /// The `Failure` record ID — citable evidence.
    pub failure_record_id: &'a str,
    /// Raw `session_id` provenance (`None` when the record carries none).
    pub session_id: Option<&'a str>,
    /// Why the failure has no code target.
    pub reason: UnresolvedFailureReason,
}

/// The failure-hotspots aggregation result.
#[derive(Debug, Clone)]
pub struct FailureHotspotsReport<'a> {
    /// Ranked rows AFTER `--limit` truncation, in rank order.
    pub hotspots: Vec<FailureHotspotRow<'a>>,
    /// Total ranked targets before truncation.
    pub total_hotspots: usize,
    /// Rows returned after truncation (`hotspots.len()`).
    pub returned_hotspots: usize,
    /// Completeness signal: whether `hotspots` was truncated by `limit`.
    pub truncated: bool,
    /// Failures with no code target, sorted by record ID. Never empty only
    /// when every failure resolved — failures are never silently dropped.
    pub unresolved: Vec<UnresolvedFailure<'a>>,
    /// Echo of the `--as-of` selector, when given.
    pub as_of: Option<String>,
    /// Echo of the `--since` selector, when given.
    pub since: Option<String>,
}

/// Parses a failure's wall-clock instant (`executed_at` preferred, else
/// `observed_at`). Returns `None` for non-nodes or unparseable/absent
/// timestamps so the caller stays conservative.
fn failure_instant(record: &GraphRecord) -> Option<DateTime<FixedOffset>> {
    let GraphRecord::Node {
        executed_at,
        observed_at,
        ..
    } = record
    else {
        return None;
    };
    let raw = executed_at.as_deref().or(observed_at.as_deref())?;
    DateTime::parse_from_rfc3339(raw).ok()
}

/// The aggregation key for one failure: its `session_id` provenance when
/// present and non-blank, else its own record ID — an unprovenanced failure
/// counts as its own run rather than collapsing into a phantom shared one.
fn run_key(failure_id: &str, session_id: Option<&str>) -> String {
    session_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| failure_id.to_owned(), str::to_owned)
}

/// Per-target accumulation: the distinct run keys and the contributing
/// failure IDs (both `BTreeSet`s, so accumulation order cannot leak into
/// the ranking).
#[derive(Default)]
struct HotspotAccum<'a> {
    runs: BTreeSet<String>,
    failures: BTreeSet<&'a str>,
}

/// Ranks every code target in the store by repeated agent-failure density.
///
/// `limit` truncates the ranking AFTER ordering; the report states the
/// truncation explicitly. `as_of` / `since` bound the aggregation to failures
/// observed within `since..=as_of` (either end may be absent for an
/// open-ended window).
///
/// # Errors
///
/// Returns [`FailureHotspotsError::NoFailures`] when the store holds zero
/// live `Failure` records (after temporal filtering) — the caller reports
/// the standard `no_match` envelope (exit 2).
#[allow(clippy::too_many_lines)]
pub fn failure_hotspots<'a>(
    records: &'a [GraphRecord],
    limit: usize,
    as_of: Option<DateTime<FixedOffset>>,
    since: Option<DateTime<FixedOffset>>,
) -> Result<FailureHotspotsReport<'a>, FailureHotspotsError> {
    let liveness = Liveness::new(records);
    let deleted = |id: &str| liveness.deleted(id);
    // Last occurrence wins for duplicate IDs, mirroring `failure_history` and
    // `Liveness`: the store's current-state view is last-write-wins.
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let present = |id: &str| -> Option<&'a GraphRecord> {
        if deleted(id) {
            None
        } else {
            by_id.get(id).copied()
        }
    };

    // Outgoing edges keyed by source, skipping retracted edges (mirrors
    // `failure_history_context`): a tombstoned link must not attribute a
    // failure to a target on a current-state read.
    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = record
        {
            if deleted(id.as_str()) {
                continue;
            }
            edges_from
                .entry(source.as_str())
                .or_default()
                .push((label, target.as_str()));
        }
    }

    let temporal = as_of.is_some() || since.is_some();
    let in_window = |instant: Option<DateTime<FixedOffset>>| -> bool {
        if !temporal {
            return true;
        }
        let Some(instant) = instant else {
            // The lane cannot prove an undated failure was observed in the
            // window — excluded, never guessed into it.
            return false;
        };
        as_of.is_none_or(|bound| instant <= bound) && since.is_none_or(|bound| instant >= bound)
    };

    let mut accum: BTreeMap<&str, HotspotAccum<'a>> = BTreeMap::new();
    let mut unresolved: Vec<UnresolvedFailure<'a>> = Vec::new();
    let mut saw_failure = false;

    for record in by_id.values() {
        let node: &'a GraphRecord = record;
        let GraphRecord::Node {
            kind: NodeKind::Failure,
            session_id,
            ..
        } = node
        else {
            continue;
        };
        let failure_id = node.id();
        if deleted(failure_id) {
            continue;
        }
        let instant = failure_instant(node);
        if !in_window(instant) {
            continue;
        }
        saw_failure = true;

        let targets = outbound_code_task_targets(node, &edges_from, &present);
        let mut code_targets: Vec<&str> = targets
            .iter()
            .filter(|id| {
                present(id).is_some_and(
                    |t| matches!(record_node_kind(t), Some(kind) if is_codegraph_kind(kind)),
                )
            })
            .copied()
            .collect();
        code_targets.sort_unstable();

        if code_targets.is_empty() {
            let reason = if targets.is_empty() {
                UnresolvedFailureReason::NoCodeTarget
            } else {
                UnresolvedFailureReason::TaskOnly
            };
            unresolved.push(UnresolvedFailure {
                failure_record_id: failure_id,
                session_id: session_id.as_deref(),
                reason,
            });
            continue;
        }

        let run = run_key(failure_id, session_id.as_deref());
        for target in code_targets {
            let slot = accum.entry(target).or_default();
            slot.runs.insert(run.clone());
            slot.failures.insert(failure_id);
        }
    }

    if !saw_failure {
        return Err(FailureHotspotsError::NoFailures);
    }

    // Rank: distinct runs DESC, total failures DESC, record ID ASC — the last
    // key makes ties byte-stable (issue #199).
    let mut ranked: Vec<(&str, HotspotAccum<'a>)> = accum.into_iter().collect();
    ranked.sort_by(|(a_id, a), (b_id, b)| {
        b.runs
            .len()
            .cmp(&a.runs.len())
            .then_with(|| b.failures.len().cmp(&a.failures.len()))
            .then_with(|| a_id.cmp(b_id))
    });

    let total_hotspots = ranked.len();
    let mut rows: Vec<FailureHotspotRow<'a>> = Vec::with_capacity(total_hotspots);
    for (position, (target_id, slot)) in ranked.into_iter().enumerate() {
        let HotspotAccum {
            runs,
            failures: failure_ids,
        } = slot;
        let mut failures: Vec<ContributingFailure<'a>> = Vec::with_capacity(failure_ids.len());
        for failure_id in failure_ids {
            let Some(node) = present(failure_id) else {
                continue;
            };
            let GraphRecord::Node {
                session_id,
                observed_at,
                failure_kind,
                ..
            } = node
            else {
                continue;
            };
            failures.push(ContributingFailure {
                record_id: failure_id,
                run: run_key(failure_id, session_id.as_deref()),
                session_id: session_id.as_deref(),
                observed_at: observed_at.as_deref(),
                failure_kind: failure_kind.as_deref(),
            });
        }
        failures.sort_by(|a, b| a.record_id.cmp(b.record_id));

        let (target_kind, repo_relative_path, span) = match present(target_id) {
            Some(GraphRecord::Node {
                kind,
                repo_relative_path,
                span,
                ..
            }) => (
                if matches!(kind, NodeKind::File) {
                    "file"
                } else {
                    "symbol"
                },
                repo_relative_path.as_deref(),
                *span,
            ),
            _ => ("symbol", None, None),
        };
        rows.push(FailureHotspotRow {
            rank: position + 1,
            target_record_id: target_id,
            target_kind,
            repo_relative_path,
            span,
            distinct_run_count: runs.len(),
            failure_count: failures.len(),
            failures,
            trust: FAILURE_HOTSPOT_TRUST,
        });
    }

    let truncated = total_hotspots > limit;
    rows.truncate(limit);
    unresolved.sort_by(|a, b| a.failure_record_id.cmp(b.failure_record_id));

    Ok(FailureHotspotsReport {
        returned_hotspots: rows.len(),
        hotspots: rows,
        total_hotspots,
        truncated,
        unresolved,
        as_of: as_of.map(|instant| instant.to_rfc3339()),
        since: since.map(|instant| instant.to_rfc3339()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::stable_id;

    const fn span(lines: (usize, usize)) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: lines.0,
            end_line: lines.1,
            start_column: None,
            end_column: None,
        }
    }

    fn symbol(id: &str, name: &str, path: &str) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::Symbol,
            path.to_owned(),
            span((1, 2)),
            name.to_owned(),
            "rust",
            format!("Symbol {name}"),
        )
    }

    fn file(id: &str, path: &str) -> GraphRecord {
        GraphRecord::syntax_node(
            id.to_owned(),
            NodeKind::File,
            path.to_owned(),
            span((1, 50)),
            path.to_owned(),
            "rust",
            format!("File {path}"),
        )
    }

    fn failure(id: &str, run: Option<&str>, observed_at: Option<&str>) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Failure,
            None,
            None,
            None,
            "agent failure".to_owned(),
        );
        if let GraphRecord::Node {
            session_id,
            observed_at: obs,
            failure_kind,
            ..
        } = &mut node
        {
            *session_id = run.map(str::to_owned);
            *obs = observed_at.map(str::to_owned);
            *failure_kind = Some("command_failure".to_owned());
        }
        node
    }

    fn failed_on(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::FailedOn,
            source.to_owned(),
            target.to_owned(),
            None,
            "failed on".to_owned(),
        )
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("tomb_{deleted_id}"),
            schema_version: 1,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    /// Two targets: `dense` (2 runs x 1 failure) and `loud` (1 run x 3
    /// failures). Distinct-run count is the primary key, so `dense` ranks
    /// first even though it has fewer total failures.
    fn ranking_fixture() -> Vec<GraphRecord> {
        let dense = stable_id(&["target", "dense"]);
        let loud = stable_id(&["target", "loud"]);
        let mut records = vec![
            symbol(&dense, "dense", "src/a.rs"),
            symbol(&loud, "loud", "src/b.rs"),
            failure("fail-d1", Some("run-1"), Some("2026-01-01T00:00:00Z")),
            failure("fail-d2", Some("run-2"), Some("2026-01-02T00:00:00Z")),
            failure("fail-l1", Some("run-1"), Some("2026-01-01T00:00:00Z")),
            failure("fail-l2", Some("run-1"), Some("2026-01-02T00:00:00Z")),
            failure("fail-l3", Some("run-1"), Some("2026-01-03T00:00:00Z")),
            failed_on("fail-d1", &dense),
            failed_on("fail-d2", &dense),
            failed_on("fail-l1", &loud),
            failed_on("fail-l2", &loud),
            failed_on("fail-l3", &loud),
        ];
        records.push(failure(
            "fail-orphan",
            Some("run-9"),
            Some("2026-01-01T00:00:00Z"),
        ));
        records
    }

    #[test]
    fn distinct_run_count_is_the_primary_rank_key() {
        let records = ranking_fixture();
        let report = failure_hotspots(&records, 20, None, None).expect("runs");
        assert_eq!(report.total_hotspots, 2);
        assert_eq!(report.hotspots[0].distinct_run_count, 2);
        assert_eq!(report.hotspots[0].failure_count, 2);
        assert_eq!(report.hotspots[0].rank, 1);
        let dense = stable_id(&["target", "dense"]);
        assert_eq!(report.hotspots[0].target_record_id, dense.as_str());
        assert_eq!(report.hotspots[1].distinct_run_count, 1);
        assert_eq!(report.hotspots[1].failure_count, 3);
        assert_eq!(report.hotspots[1].rank, 2);
        // The orphan failure is unresolved, never silently dropped.
        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(report.unresolved[0].failure_record_id, "fail-orphan");
        assert_eq!(
            report.unresolved[0].reason,
            UnresolvedFailureReason::NoCodeTarget
        );
    }

    #[test]
    fn full_tie_breaks_by_record_id_for_byte_stability() {
        let a = stable_id(&["target", "a"]);
        let b = stable_id(&["target", "b"]);
        let records = vec![
            symbol(&a, "a", "src/a.rs"),
            symbol(&b, "b", "src/b.rs"),
            failure("f1", Some("run-1"), Some("2026-01-01T00:00:00Z")),
            failure("f2", Some("run-1"), Some("2026-01-02T00:00:00Z")),
            failed_on("f1", &b),
            failed_on("f2", &a),
        ];
        let report = failure_hotspots(&records, 20, None, None).expect("runs");
        // Both targets: 1 run, 1 failure — the smaller record ID ranks first,
        // deterministically.
        let (first, second) = if a < b { (&a, &b) } else { (&b, &a) };
        assert_eq!(report.hotspots[0].target_record_id, first.as_str());
        assert_eq!(report.hotspots[1].target_record_id, second.as_str());
    }

    #[test]
    fn sessionless_failures_count_as_their_own_runs() {
        let target = stable_id(&["target", "t"]);
        let records = vec![
            symbol(&target, "t", "src/t.rs"),
            failure("f1", None, Some("2026-01-01T00:00:00Z")),
            failure("f2", None, Some("2026-01-02T00:00:00Z")),
            failed_on("f1", &target),
            failed_on("f2", &target),
        ];
        let report = failure_hotspots(&records, 20, None, None).expect("runs");
        let row = &report.hotspots[0];
        assert_eq!(
            row.distinct_run_count, 2,
            "each unprovenanced failure is its own run"
        );
        assert_eq!(row.failure_count, 2);
        assert_eq!(row.failures[0].run, "f1");
        assert_eq!(row.failures[0].session_id, None);
    }

    #[test]
    fn tombstoned_failures_are_excluded_entirely() {
        let target = stable_id(&["target", "t"]);
        let records = vec![
            symbol(&target, "t", "src/t.rs"),
            failure("f1", Some("run-1"), Some("2026-01-01T00:00:00Z")),
            failed_on("f1", &target),
            tombstone("f1"),
        ];
        let err = failure_hotspots(&records, 20, None, None).expect_err("no live failures");
        assert_eq!(err, FailureHotspotsError::NoFailures);
    }

    #[test]
    fn temporal_window_filters_failures_and_excludes_undated() {
        let target = stable_id(&["target", "t"]);
        let records = vec![
            symbol(&target, "t", "src/t.rs"),
            failure("old", Some("run-1"), Some("2026-01-01T00:00:00Z")),
            failure("new", Some("run-2"), Some("2026-06-01T00:00:00Z")),
            failure("undated", Some("run-3"), None),
            failed_on("old", &target),
            failed_on("new", &target),
            failed_on("undated", &target),
        ];
        let since = DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z").expect("parse");
        let report = failure_hotspots(&records, 20, None, Some(since)).expect("runs");
        let row = &report.hotspots[0];
        assert_eq!(row.distinct_run_count, 1);
        assert_eq!(row.failures.len(), 1);
        assert_eq!(row.failures[0].record_id, "new");
        assert!(
            report.unresolved.is_empty(),
            "filtered-out failures are excluded, not unresolved"
        );
    }

    #[test]
    fn task_only_failures_are_unresolved_not_ranked() {
        let target = stable_id(&["target", "t"]);
        let task_id = "project:v1:0000000000000000000000000000000000000000000000000000000000000000";
        let task = GraphRecord::node(
            task_id.to_owned(),
            NodeKind::Task,
            None,
            None,
            Some("t".to_owned()),
            "Task".to_owned(),
        );
        let records = vec![
            symbol(&target, "t", "src/t.rs"),
            task,
            failure("f1", Some("run-1"), Some("2026-01-01T00:00:00Z")),
            failed_on("f1", task_id),
        ];
        let report = failure_hotspots(&records, 20, None, None).expect("runs");
        assert!(report.hotspots.is_empty());
        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(
            report.unresolved[0].reason,
            UnresolvedFailureReason::TaskOnly
        );
    }

    #[test]
    fn limit_truncates_after_ranking_and_signals() {
        let records = ranking_fixture();
        let report = failure_hotspots(&records, 1, None, None).expect("runs");
        assert_eq!(report.total_hotspots, 2);
        assert_eq!(report.returned_hotspots, 1);
        assert!(report.truncated);
        assert_eq!(report.hotspots.len(), 1);
        // The surviving row keeps its pre-truncation rank.
        assert_eq!(report.hotspots[0].rank, 1);
        let dense = stable_id(&["target", "dense"]);
        assert_eq!(report.hotspots[0].target_record_id, dense.as_str());
    }

    #[test]
    fn empty_store_is_no_failures() {
        let records = vec![file(&stable_id(&["f"]), "src/x.rs")];
        let err = failure_hotspots(&records, 20, None, None).expect_err("no failures");
        assert_eq!(err, FailureHotspotsError::NoFailures);
    }

    #[test]
    fn contributing_failures_carry_run_handles() {
        let records = ranking_fixture();
        let report = failure_hotspots(&records, 20, None, None).expect("runs");
        let dense = stable_id(&["target", "dense"]);
        let row = report
            .hotspots
            .iter()
            .find(|r| r.target_record_id == dense.as_str())
            .expect("dense row");
        let runs: BTreeSet<&str> = row.failures.iter().map(|f| f.run.as_str()).collect();
        assert_eq!(runs, BTreeSet::from(["run-1", "run-2"]));
        for f in &row.failures {
            assert_eq!(f.session_id, Some(f.run.as_str()));
            assert!(f.observed_at.is_some());
            assert_eq!(f.failure_kind, Some("command_failure"));
        }
    }

    #[test]
    fn duplicate_failure_id_resolves_to_the_last_write() {
        // The store's current-state view is last-write-wins (mirroring
        // `failure_history` and `Liveness`): a rewritten `Failure` record
        // must contribute its newest session, not its first.
        let target = stable_id(&["target", "dup"]);
        let records = vec![
            symbol(&target, "dup", "src/dup.rs"),
            failure("f1", Some("run-old"), Some("2026-01-01T00:00:00Z")),
            failed_on("f1", &target),
            failure("f1", Some("run-new"), Some("2026-02-01T00:00:00Z")),
        ];
        let report = failure_hotspots(&records, 20, None, None).expect("report");
        assert_eq!(report.hotspots.len(), 1);
        let row = &report.hotspots[0];
        assert_eq!(row.failures.len(), 1);
        assert_eq!(row.failures[0].run.as_str(), "run-new");
        assert_eq!(row.failures[0].session_id, Some("run-new"));
    }
}
