//! List project tasks filtered by status (issue #119).
//!
//! Pure, read-only, deterministic: given project-graph records, resolve every
//! live project `Task` to its latest-`transaction_time` version per stable
//! entity (`entity_id`, falling back to the record ID when the importer did
//! not stamp one), then filter to the requested statuses and group the rows
//! by status.
//!
//! A task's status is **imported intent** — which work the source system says
//! is in flight — never proof the code changed, the acceptance criteria hold,
//! or any verification passed. Rows name work to inspect, not work proven
//! done.
//!
//! Trust separation: rows carry only importer-produced, redaction-passed
//! fields (`title`, `summary`, handles, IDs, enums). Raw issue/PR bodies,
//! comment text, transcript text, command output, env values, bearer tokens,
//! and protected raw-artifact payloads never enter the row — bodies live
//! behind `body_handle` on the node and are never read here.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use serde::Serialize;

use super::liveness::Liveness;
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};
use crate::local_project::valid_task_statuses;

/// Task statuses selected by the `active` convenience filter: the in-flight
/// set, mirroring the task-ready lane's candidate statuses (issue #161).
pub const TASK_LIST_ACTIVE_STATUSES: &[&str] = &["open", "in_progress", "blocked"];

/// Default `--status` value: the `active` convenience filter.
pub const TASK_LIST_DEFAULT_STATUS_FILTER: &str = "active";

/// Lane-level trust for the single-domain trust table in `docs/cli/query.md`:
/// rows name work the source system says is in flight, never work proven done.
pub const TASK_LIST_TRUST: &str = "status_lead";

/// Trust disclaimer printed with every report.
pub const TASK_LIST_DISCLAIMER: &str = "A task's status is imported intent — which work the source system says is in flight — never proof the code changed, the acceptance criteria hold, or any verification passed. Status rows name work to inspect, not work proven done.";

/// One listed task: the closed, bounded row surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskListRow {
    /// Stable record ID of the winning (latest-`transaction_time`) row.
    pub record_id: String,
    /// Schema version that produced the record.
    pub schema_version: u32,
    /// Current status (author-written; `"unknown"` when the node records none).
    pub status: String,
    /// Importer source kind (`github_issue`, `local_jsonl`, …).
    pub source_kind: Option<String>,
    /// Source-system handle: the GitHub issue/PR number plus URL resolved
    /// through the task's `ExternalLink`, or the task's own local-JSONL
    /// record handle (`<encoded_path>:<local_id>:<blake3>`).
    pub source_handle: Option<String>,
    /// Human-readable title, when the record carries one.
    pub title: Option<String>,
    /// Agent-facing summary.
    pub summary: String,
}

/// Tasks sharing one status, in canonical order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskListGroup {
    /// The status every row in `tasks` carries.
    pub status: String,
    /// Rows in ascending record-ID order.
    pub tasks: Vec<TaskListRow>,
}

/// Aggregate counts for the listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskListCounts {
    /// Live latest-version tasks seen (before filtering).
    pub tasks: usize,
    /// Rows surviving the status filter.
    pub matched: usize,
}

/// The listing: every matching task grouped by status.
///
/// `groups` follow the closed status-vocabulary order and are in ascending
/// record-ID order within a group, so re-running the identical query over an
/// unchanged store serializes byte-identically. A report whose filter matched
/// nothing is valid and carries `zero_matches: true` — the explicit
/// machine-readable zero, never conflated with an error or with
/// [`TaskListOutcome::NoProjectData`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskListReport {
    /// Aggregate counts.
    pub counts: TaskListCounts,
    /// Non-empty status groups, canonical status order.
    pub groups: Vec<TaskListGroup>,
    /// Explicit machine-readable zero-matches marker.
    pub zero_matches: bool,
}

/// Outcome of task-listing resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskListOutcome {
    /// At least one live `Task` record exists; the report itself may be empty.
    Report(TaskListReport),
    /// No live `Task` records at all: no project data to resolve.
    NoProjectData,
}

/// CLI exit-code contract (issue #119).
///
/// `0` = valid report (zero matches is still valid), `2` = no
/// Task/project data. Malformed `--status` filters are `3`, decided by the
/// CLI; malformed input / read failures are `1`, decided by the CLI.
#[must_use]
pub const fn task_list_exit_code(outcome: &TaskListOutcome) -> i32 {
    match outcome {
        TaskListOutcome::NoProjectData => 2,
        TaskListOutcome::Report(_) => 0,
    }
}

/// How a `--status` filter value failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskListStatusFilterError {
    /// An empty entry (leading, trailing, or doubled comma; or empty value).
    EmptyEntry,
    /// Not a member of the project task-status vocabulary (and not the
    /// `active` convenience keyword standing alone).
    UnknownStatus(String),
    /// The same status twice.
    Duplicate(String),
}

impl TaskListStatusFilterError {
    /// The stable snake_case code used in the CLI error envelope.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "invalid_status_filter"
    }
}

impl std::fmt::Display for TaskListStatusFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyEntry => write!(f, "status filter contains an empty entry"),
            Self::UnknownStatus(s) => write!(
                f,
                "unknown task status '{s}': not in the project task-status vocabulary; use 'active' for the open/in_progress/blocked convenience set"
            ),
            Self::Duplicate(s) => write!(f, "duplicate status '{s}' in filter"),
        }
    }
}

impl std::error::Error for TaskListStatusFilterError {}

/// Parse and validate a `--status` filter.
///
/// `None` (and the literal `active`) selects the active convenience set
/// (`open`, `in_progress`, `blocked`). Otherwise every comma-separated entry
/// must be a member of the closed project task-status vocabulary — every
/// vocabulary member is eligible, not just in-flight ones; anything else —
/// unknown names, empty entries, duplicates — is a malformed filter.
///
/// # Errors
///
/// Returns [`TaskListStatusFilterError`] when any entry is unknown, empty, or
/// duplicated.
pub fn parse_task_list_status_filter(
    raw: Option<&str>,
) -> Result<BTreeSet<String>, TaskListStatusFilterError> {
    let raw = raw.unwrap_or(TASK_LIST_DEFAULT_STATUS_FILTER).trim();
    if raw == "active" {
        return Ok(TASK_LIST_ACTIVE_STATUSES
            .iter()
            .map(|s| (*s).to_owned())
            .collect());
    }
    let mut out = BTreeSet::new();
    for part in raw.split(',') {
        let status = part.trim();
        if status.is_empty() {
            return Err(TaskListStatusFilterError::EmptyEntry);
        }
        if !valid_task_statuses().contains(&status) {
            return Err(TaskListStatusFilterError::UnknownStatus(status.to_owned()));
        }
        if !out.insert(status.to_owned()) {
            return Err(TaskListStatusFilterError::Duplicate(status.to_owned()));
        }
    }
    Ok(out)
}

/// Parsed `transaction_time` ordering key: later instants win; unparseable or
/// absent timestamps sort below every real one, and equal keys fall back to
/// read order (later write wins).
fn transaction_time_key(record: &GraphRecord) -> Option<DateTime<chrono::FixedOffset>> {
    let GraphRecord::Node {
        transaction_time: Some(tt),
        ..
    } = record
    else {
        return None;
    };
    DateTime::parse_from_rfc3339(tt).ok()
}

/// Resolve the task listing over project-graph records.
///
/// Read-only and deterministic: iterating `BTreeMap`s in key order, grouping
/// in closed-vocabulary status order, and emitting rows in ascending
/// record-ID order, so repeated runs serialize byte-identically.
///
/// # Panics
///
/// Panics only on a violated internal invariant: every entity group holds at
/// least one row by construction (groups are created by pushing a row).
pub fn task_list_report(records: &[GraphRecord], filter: &BTreeSet<String>) -> TaskListOutcome {
    let liveness = Liveness::new(records);

    // ── Live Task nodes: latest write per record ID, grouped by entity ────
    // Mirrors task_ready's liveness gate (tombstoned IDs are out; the last
    // write of a record ID is its representative), then groups rows by the
    // stable project-domain entity (`entity_id`, falling back to the record
    // ID when the importer did not stamp one) so a status transition written
    // as a new row resolves bi-temporally: the latest `transaction_time`
    // version wins, ties break by later read order.
    let mut versions: BTreeMap<&str, Vec<(usize, &GraphRecord)>> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Task,
            entity_id,
            ..
        } = record
            && !liveness.deleted(id.as_str())
            && liveness.is_latest_node_version(id.as_str(), index)
        {
            let key = entity_id.as_deref().unwrap_or(id.as_str());
            versions.entry(key).or_default().push((index, record));
        }
    }
    if versions.is_empty() {
        return TaskListOutcome::NoProjectData;
    }

    // ── Source-handle resolution ──────────────────────────────────────────
    // Mirrors the task-ready lane: live ExternalLink nodes by record ID, and
    // live EXTERNAL_HANDLE edges (task -> link). A GitHub-backed task's
    // display handle resolves through its `source_external_link_id`
    // (falling back to the edge) to the link's system handle (native ID plus
    // URL); local tasks keep their own JSONL handle.
    let mut links: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::ExternalLink,
            ..
        } = record
            && !liveness.deleted(id.as_str())
            && liveness.is_latest_node_version(id.as_str(), index)
        {
            links.insert(id.as_str(), record);
        }
    }
    let mut handle_target: BTreeMap<&str, &str> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if let GraphRecord::Edge {
            id,
            label: EdgeLabel::ExternalHandle,
            source,
            target,
            ..
        } = record
            && !liveness.deleted(id.as_str())
            && liveness.is_latest_edge_version(id.as_str(), index)
        {
            handle_target.insert(source.as_str(), target.as_str());
        }
    }
    let resolve_handle = |node: &GraphRecord| -> Option<String> {
        let GraphRecord::Node {
            id,
            source_handle,
            source_external_link_id,
            ..
        } = node
        else {
            return None;
        };
        let link_id = source_external_link_id
            .as_deref()
            .or_else(|| handle_target.get(id.as_str()).copied());
        if let Some(link_id) = link_id
            && let Some(link) = links.get(link_id)
            && let GraphRecord::Node {
                system_native_id,
                url,
                ..
            } = *link
        {
            match (system_native_id, url) {
                (Some(native), Some(u)) => return Some(format!("{native} <{u}>")),
                (Some(native), None) => return Some(native.clone()),
                (None, Some(u)) => return Some(u.clone()),
                (None, None) => {}
            }
        }
        source_handle.clone()
    };

    // ── Bi-temporal resolution, filtering, grouping ────────────────────────
    let mut status_order: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, status) in valid_task_statuses().iter().enumerate() {
        status_order.insert(status, i);
    }
    let mut grouped: BTreeMap<&str, Vec<TaskListRow>> = BTreeMap::new();
    let mut tasks = 0usize;
    for rows in versions.values() {
        tasks += 1;
        let winner = rows
            .iter()
            .max_by(|(ia, ra), (ib, rb)| {
                transaction_time_key(ra)
                    .cmp(&transaction_time_key(rb))
                    .then_with(|| ia.cmp(ib))
            })
            .expect("an entity group always holds at least one row");
        let node = winner.1;
        let GraphRecord::Node {
            id,
            schema_version,
            status,
            source_kind,
            title,
            summary,
            ..
        } = node
        else {
            continue;
        };
        let status = status.as_deref().unwrap_or("unknown");
        if !filter.contains(status) {
            continue;
        }
        grouped.entry(status).or_default().push(TaskListRow {
            record_id: id.clone(),
            schema_version: *schema_version,
            status: status.to_owned(),
            source_kind: source_kind.clone(),
            source_handle: resolve_handle(node),
            title: title.clone(),
            summary: summary.clone(),
        });
    }

    let mut matched = 0usize;
    let mut groups: Vec<TaskListGroup> = Vec::new();
    let mut ordered: Vec<(&str, Vec<TaskListRow>)> = grouped.into_iter().collect();
    ordered.sort_by_key(|(status, _)| status_order.get(status).copied().unwrap_or(usize::MAX));
    for (status, mut rows) in ordered {
        rows.sort_by(|a, b| a.record_id.cmp(&b.record_id));
        matched += rows.len();
        groups.push(TaskListGroup {
            status: status.to_owned(),
            tasks: rows,
        });
    }

    TaskListOutcome::Report(TaskListReport {
        counts: TaskListCounts { tasks, matched },
        groups,
        zero_matches: matched == 0,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::{
        TASK_LIST_ACTIVE_STATUSES, TaskListOutcome, TaskListStatusFilterError,
        parse_task_list_status_filter, task_list_exit_code, task_list_report,
    };
    use crate::ir::GraphRecord;

    fn node(id: &str, kind: &str, extra: &serde_json::Value) -> String {
        let mut v = json!({
            "record_type": "node",
            "id": id,
            "kind": kind,
            "schema_version": 1,
            "summary": format!("{kind} {id}"),
        });
        for (k, val) in extra.as_object().expect("extra is an object") {
            v[k] = val.clone();
        }
        v.to_string()
    }

    fn task(id: &str, status: &str, extra: serde_json::Value) -> String {
        let mut extra = extra;
        extra["title"] = json!(format!("Title of {id}"));
        extra["status"] = json!(status);
        if extra.get("transaction_time").is_none() {
            extra["transaction_time"] = json!("2026-09-28T00:00:00Z");
        }
        node(id, "Task", &extra)
    }

    fn parse(lines: Vec<String>) -> Vec<GraphRecord> {
        lines
            .into_iter()
            .map(|l| serde_json::from_str::<GraphRecord>(&l).expect("valid line"))
            .collect()
    }

    /// Six statuses x two source kinds, one task each, with the required
    /// source-system handles: GitHub rows resolve through an `ExternalLink`,
    /// local rows carry their own JSONL handle.
    fn every_status_fixture() -> Vec<GraphRecord> {
        let statuses = [
            "open",
            "in_progress",
            "blocked",
            "closed_completed",
            "closed_dropped",
            "unknown",
        ];
        let mut lines = Vec::new();
        for (i, status) in statuses.iter().enumerate() {
            lines.push(task(
                &format!("task:gh-{status}"),
                status,
                json!({
                    "entity_id": format!("entity:gh-{status}"),
                    "source_kind": "github_issue",
                    "source_external_link_id": format!("link:gh-{i}"),
                }),
            ));
            lines.push(node(
                &format!("link:gh-{i}"),
                "ExternalLink",
                &json!({
                    "system": "github",
                    "system_native_id": format!("#{}", 100 + i),
                    "url": format!("https://github.com/acme/repo/issues/{}", 100 + i),
                }),
            ));
            lines.push(task(
                &format!("task:local-{status}"),
                status,
                json!({
                    "entity_id": format!("entity:local-{status}"),
                    "source_kind": "local_jsonl",
                    "source_handle": format!("tasks%2Facme.jsonl:local-{status}:deadbeef"),
                    "repo_relative_path": "tasks/acme.jsonl",
                    "name": format!("local-{status}"),
                }),
            ));
        }
        parse(lines)
    }

    fn active_filter() -> BTreeSet<String> {
        TASK_LIST_ACTIVE_STATUSES
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    #[test]
    fn filter_defaults_to_active_and_parses_explicit_sets() {
        assert_eq!(
            parse_task_list_status_filter(None).expect("default parses"),
            active_filter(),
            "omitted --status selects the active convenience set"
        );
        assert_eq!(
            parse_task_list_status_filter(Some("active")).expect("active parses"),
            active_filter(),
            "`active` is the documented convenience spelling"
        );
        assert_eq!(
            parse_task_list_status_filter(Some(" open , blocked ")).expect("trims"),
            BTreeSet::from(["open".to_owned(), "blocked".to_owned()]),
            "entries are trimmed"
        );
        assert_eq!(
            parse_task_list_status_filter(Some("closed_completed,unknown"))
                .expect("terminal parses"),
            BTreeSet::from(["closed_completed".to_owned(), "unknown".to_owned()]),
            "every vocabulary member is eligible, not just in-flight ones"
        );
    }

    #[test]
    fn filter_rejects_malformed_with_stable_code() {
        assert_eq!(
            parse_task_list_status_filter(Some("open,bogus")),
            Err(TaskListStatusFilterError::UnknownStatus("bogus".to_owned()))
        );
        assert_eq!(
            parse_task_list_status_filter(Some("open,")),
            Err(TaskListStatusFilterError::EmptyEntry)
        );
        assert_eq!(
            parse_task_list_status_filter(Some("open,open")),
            Err(TaskListStatusFilterError::Duplicate("open".to_owned()))
        );
        // `active` is a convenience keyword, not a vocabulary member: it must
        // stand alone.
        assert_eq!(
            parse_task_list_status_filter(Some("active,open")),
            Err(TaskListStatusFilterError::UnknownStatus(
                "active".to_owned()
            ))
        );
        assert_eq!(
            parse_task_list_status_filter(Some("Open")),
            Err(TaskListStatusFilterError::UnknownStatus("Open".to_owned()))
        );
        for bad in ["bogus", "open,", "open,open", "active,open", ""] {
            let err = parse_task_list_status_filter(Some(bad)).expect_err("malformed");
            assert_eq!(
                err.code(),
                "invalid_status_filter",
                "every malformation carries the stable diagnostic code"
            );
        }
    }

    #[test]
    fn report_lists_every_status_from_both_source_kinds() {
        let records = every_status_fixture();
        let all: BTreeSet<String> = [
            "open",
            "in_progress",
            "blocked",
            "closed_completed",
            "closed_dropped",
            "unknown",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let TaskListOutcome::Report(report) = task_list_report(&records, &all) else {
            panic!("expected a report");
        };
        assert!(!report.zero_matches);
        assert_eq!(report.counts.tasks, 12);
        assert_eq!(report.counts.matched, 12);
        assert_eq!(report.groups.len(), 6, "one group per status");
        for group in &report.groups {
            assert_eq!(group.tasks.len(), 2, "github + local row per status");
            for row in &group.tasks {
                assert_eq!(row.status, group.status);
                assert!(!row.record_id.is_empty(), "record ID carried");
                assert_eq!(row.schema_version, 1, "schema version carried");
                assert!(
                    row.source_kind.as_deref() == Some("github_issue")
                        || row.source_kind.as_deref() == Some("local_jsonl")
                );
                assert!(
                    row.title
                        .as_deref()
                        .is_some_and(|t| t.contains(&row.record_id)),
                    "title carried"
                );
                assert!(!row.summary.is_empty(), "summary carried");
            }
        }
    }

    #[test]
    fn github_handle_resolves_number_and_url_local_keeps_jsonl_handle() {
        let records = every_status_fixture();
        let open: BTreeSet<String> = BTreeSet::from(["open".to_owned()]);
        let TaskListOutcome::Report(report) = task_list_report(&records, &open) else {
            panic!("expected a report");
        };
        assert_eq!(report.groups.len(), 1);
        let rows = &report.groups[0].tasks;
        assert_eq!(rows.len(), 2);
        let gh = rows
            .iter()
            .find(|r| r.record_id == "task:gh-open")
            .expect("github row");
        assert_eq!(
            gh.source_handle.as_deref(),
            Some("#100 <https://github.com/acme/repo/issues/100>"),
            "GitHub handle is the issue/PR number plus URL via ExternalLink"
        );
        let local = rows
            .iter()
            .find(|r| r.record_id == "task:local-open")
            .expect("local row");
        assert_eq!(
            local.source_handle.as_deref(),
            Some("tasks%2Facme.jsonl:local-open:deadbeef"),
            "local handle is the task's own JSONL record handle"
        );
    }

    #[test]
    fn latest_transaction_time_row_wins_per_entity() {
        let lines = vec![
            // Same entity, status moved open -> in_progress; only the later
            // transaction_time row may surface.
            task(
                "task:e1-v1",
                "open",
                json!({
                    "entity_id": "entity:e1",
                    "transaction_time": "2026-09-20T00:00:00Z",
                }),
            ),
            task(
                "task:e1-v2",
                "in_progress",
                json!({
                    "entity_id": "entity:e1",
                    "transaction_time": "2026-09-28T00:00:00Z",
                }),
            ),
            // Same record ID written twice (append-only rewrite style): the
            // later read-order row wins when transaction_time ties.
            task(
                "task:e2",
                "blocked",
                json!({
                    "entity_id": "entity:e2",
                    "transaction_time": "2026-09-28T00:00:00Z",
                }),
            ),
            task(
                "task:e2",
                "closed_completed",
                json!({
                    "entity_id": "entity:e2",
                    "transaction_time": "2026-09-28T00:00:00Z",
                }),
            ),
            task("task:steady", "open", json!({"entity_id": "entity:steady"})),
        ];
        let records = parse(lines);
        let all: BTreeSet<String> = [
            "open",
            "in_progress",
            "blocked",
            "closed_completed",
            "closed_dropped",
            "unknown",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let TaskListOutcome::Report(report) = task_list_report(&records, &all) else {
            panic!("expected a report");
        };
        assert_eq!(report.counts.tasks, 3, "three entities, not five rows");
        let ids: Vec<&str> = report
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter().map(|r| r.record_id.as_str()))
            .collect();
        assert!(
            ids.contains(&"task:e1-v2") && !ids.contains(&"task:e1-v1"),
            "older transaction_time row excluded: {ids:?}"
        );
        assert!(
            ids.contains(&"task:e2") && !ids.iter().any(|i| i.contains("v1")),
            "same-ID rewrite resolves to the later row"
        );
        let e1 = report
            .groups
            .iter()
            .flat_map(|g| &g.tasks)
            .find(|r| r.record_id == "task:e1-v2")
            .expect("e1 winner");
        assert_eq!(e1.status, "in_progress");
        let e2 = report
            .groups
            .iter()
            .flat_map(|g| &g.tasks)
            .find(|r| r.record_id == "task:e2")
            .expect("e2 winner");
        assert_eq!(e2.status, "closed_completed");
    }

    #[test]
    fn tombstoned_tasks_are_excluded() {
        let lines = vec![
            task("task:live", "open", json!({"entity_id": "entity:live"})),
            task("task:gone", "open", json!({"entity_id": "entity:gone"})),
            json!({
                "record_type": "tombstone",
                "id": "tombstone:task:gone",
                "deleted_id": "task:gone",
                "schema_version": 1,
                "summary": "retract task:gone",
            })
            .to_string(),
        ];
        let records = parse(lines);
        let TaskListOutcome::Report(report) = task_list_report(&records, &active_filter()) else {
            panic!("expected a report");
        };
        assert_eq!(report.counts.tasks, 1);
        assert!(
            report
                .groups
                .iter()
                .flat_map(|g| &g.tasks)
                .all(|r| r.record_id == "task:live")
        );
    }

    #[test]
    fn zero_matches_is_explicit_and_distinct_from_no_project_data() {
        // Tasks exist, but the filter matches none of them.
        let records = every_status_fixture();
        let dropped: BTreeSet<String> = BTreeSet::from(["closed_dropped".to_owned()]);
        // Sanity: the fixture really has no closed_dropped gap before we
        // remove the rows — rebuild without the dropped rows.
        let open_only: Vec<GraphRecord> = records
            .into_iter()
            .filter(|r| !matches!(r, GraphRecord::Node { status: Some(s), .. } if s == "closed_dropped"))
            .collect();
        let TaskListOutcome::Report(report) = task_list_report(&open_only, &dropped) else {
            panic!("expected a report");
        };
        assert!(report.zero_matches, "explicit machine-readable zero");
        assert!(report.groups.is_empty());
        assert_eq!(report.counts.matched, 0);
        assert!(report.counts.tasks > 0, "tasks were seen");
        assert_eq!(task_list_exit_code(&TaskListOutcome::Report(report)), 0);

        // No Task records at all: a different outcome and exit code.
        let no_tasks = parse(vec![node(
            "sym:a",
            "Symbol",
            &json!({"repo_relative_path": "a.rs"}),
        )]);
        let outcome = task_list_report(&no_tasks, &dropped);
        assert_eq!(outcome, TaskListOutcome::NoProjectData);
        assert_eq!(task_list_exit_code(&outcome), 2);
    }

    #[test]
    fn groups_follow_vocabulary_order_and_rows_sort_by_record_id() {
        let records = every_status_fixture();
        let all: BTreeSet<String> = [
            "unknown",
            "closed_dropped",
            "closed_completed",
            "blocked",
            "in_progress",
            "open",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let TaskListOutcome::Report(report) = task_list_report(&records, &all) else {
            panic!("expected a report");
        };
        let order: Vec<&str> = report.groups.iter().map(|g| g.status.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "open",
                "in_progress",
                "blocked",
                "closed_completed",
                "closed_dropped",
                "unknown"
            ],
            "groups in closed-vocabulary canonical order regardless of filter order"
        );
        for group in &report.groups {
            let ids: Vec<&str> = group.tasks.iter().map(|r| r.record_id.as_str()).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            assert_eq!(ids, sorted, "rows ascending by record ID");
        }
    }

    #[test]
    fn active_filter_selects_only_open_in_progress_blocked() {
        let records = every_status_fixture();
        let TaskListOutcome::Report(report) = task_list_report(&records, &active_filter()) else {
            panic!("expected a report");
        };
        assert!(!report.zero_matches);
        let order: Vec<&str> = report.groups.iter().map(|g| g.status.as_str()).collect();
        assert_eq!(order, vec!["open", "in_progress", "blocked"]);
        assert_eq!(report.counts.matched, 6);
    }

    #[test]
    fn report_rows_carry_no_bodies_or_raw_payloads() {
        let canary = "CANARY-119-body-must-never-surface";
        let lines = vec![
            task(
                "task:gh-open",
                "open",
                json!({
                    "entity_id": "entity:gh-open",
                    "source_kind": "github_issue",
                    "source_external_link_id": "link:gh-0",
                    // A body_handle would carry the raw body behind a
                    // redacted handle (inline content included); the lane
                    // must never read it.
                    "body_handle": {"inline": canary, "hash": "deadbeef", "bytes": 34},
                }),
            ),
            node(
                "link:gh-0",
                "ExternalLink",
                &json!({
                    "system": "github",
                    "system_native_id": "#100",
                    "url": "https://github.com/acme/repo/issues/100",
                }),
            ),
            // An unrelated node carrying transcript text: the lane reads
            // only Task/ExternalLink nodes, so this can never leak.
            node("sess:a", "AgentRun", &json!({"text": canary})),
        ];
        let records = parse(lines);
        let TaskListOutcome::Report(report) = task_list_report(&records, &active_filter()) else {
            panic!("expected a report");
        };
        let wire = serde_json::to_string(&report).expect("serializes");
        assert!(
            !wire.contains(canary),
            "no raw body / transcript text in the report"
        );
        assert!(
            !wire.contains("body_handle"),
            "the redacted body handle field is never emitted"
        );
        // Row keys are exactly the documented, bounded surface.
        let row_value = serde_json::to_value(&report.groups[0].tasks[0]).expect("row value");
        let keys: BTreeSet<&str> = row_value
            .as_object()
            .expect("row is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "record_id",
                "schema_version",
                "status",
                "source_handle",
                "source_kind",
                "summary",
                "title"
            ]),
            "row surface is closed: {keys:?}"
        );
    }
}
