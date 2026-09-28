//! Flag in-flight tasks whose code footprints overlap (issue #150).
//!
//! Pure, read-only, deterministic: given project-graph records, resolve each
//! in-flight `Task`'s code footprint and report every unordered pair of
//! in-flight tasks that share at least one live `Symbol` / `File` handle.
//!
//! A task's footprint is the set of code-graph handles reached through two
//! legs:
//!
//! * **direct** — the task's own live `MENTIONS_SYMBOL` (task → symbol) and
//!   `TOUCHES_FILE` (task → file) edges;
//! * **session** — any live node with a live `REFERENCES_TASK` edge to the
//!   task (an agent-memory session working on it), followed forward through
//!   that node's live `TOUCHED_FILE` / `MENTIONS_SYMBOL` edges.
//!
//! Trust separation: a footprint is a set of deterministic code-graph handles
//! reached through existing project/agent-memory edges. No agent observation
//! is promoted to a code fact, and an overlap row is an **inspection lead**
//! — "these two in-flight tasks name the same code" — never proof of an edit
//! conflict, of correctness, or of verification. Only the latest
//! `transaction_time` version of each node/edge participates (tombstoned or
//! superseded records resolve to nothing).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::liveness::Liveness;
use super::task_ready::TaskIdentityRow;
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};
use crate::local_project::valid_task_statuses;

/// Task statuses compared by the overlap lane (author-written only).
///
/// Closed and terminal statuses never participate: an overlap lead is only
/// actionable for work that is still in flight. This mirrors the documented
/// in-flight set from the task-ready lane (issue #161).
pub const TASK_OVERLAP_IN_FLIGHT_STATUSES: &[&str] = &["open", "in_progress", "blocked"];

/// Default `--status` filter: every in-flight status.
pub const TASK_OVERLAP_DEFAULT_STATUS_FILTER: &str = "open,in_progress,blocked";

/// Lane-level trust for the single-domain trust table in `docs/cli/query.md`:
/// the rows are inspection leads, not facts.
pub const TASK_OVERLAP_TRUST: &str = "inspection_lead";

/// Trust disclaimer printed with every report.
pub const TASK_OVERLAP_DISCLAIMER: &str = "An overlap row is an inspection lead: two in-flight tasks name the same code-graph handle(s). It is never proof of an edit conflict, of correctness, or of verification. Footprints derive only from deterministic code-graph handles and existing project/agent-memory edges; no agent observation is promoted to a code fact.";

/// Footprint leg through which a shared handle resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FootprintLeg {
    /// The task's own `MENTIONS_SYMBOL` / `TOUCHES_FILE` edge.
    Direct,
    /// A referencing session's `TOUCHED_FILE` / `MENTIONS_SYMBOL` edge.
    Session,
}

impl FootprintLeg {
    /// Stable wire string for the leg.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Session => "session",
        }
    }
}

/// One shared code handle causing an overlap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverlapHandleRow {
    /// Stable record ID of the shared `Symbol` / `File` node.
    pub record_id: String,
    /// Code-graph node kind (`"Symbol"` or `"File"`).
    pub kind: String,
    /// Repo-relative path of the shared handle, when the node carries one.
    pub repo_relative_path: Option<String>,
    /// Source span for symbols, rendered `start_line:start_col-end_line:end_col`
    /// (columns omitted when the node records none); `None` for files.
    pub span: Option<String>,
    /// Footprint legs that resolved this handle, ascending (`direct`, `session`).
    pub via: Vec<String>,
}

/// One overlapping in-flight task pair.
///
/// Canonical order: `task_a.record_id < task_b.record_id`, pairs ascending,
/// shared handles ascending by record ID (documented tie-break).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverlapPairRow {
    /// First task of the pair (lesser record ID).
    pub task_a: TaskIdentityRow,
    /// Second task of the pair.
    pub task_b: TaskIdentityRow,
    /// Shared code handles, ascending record ID.
    pub shared_handles: Vec<OverlapHandleRow>,
}

/// Lane diagnostic codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOverlapDiagnosticCode {
    /// An in-flight task resolved to no live `Symbol` / `File` handle.
    NoFootprint,
}

impl TaskOverlapDiagnosticCode {
    /// The stable snake_case code used in JSON output and text rendering.
    #[must_use]
    pub const fn code_as_str(self) -> &'static str {
        match self {
            Self::NoFootprint => "no_footprint",
        }
    }
}

/// A lane-level diagnostic: tasks with no resolvable footprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskOverlapDiagnostic {
    /// Machine-readable diagnostic code.
    pub code: TaskOverlapDiagnosticCode,
    /// Human-readable explanation.
    pub message: String,
    /// The in-flight task with no resolvable footprint.
    pub task_record_id: Option<String>,
}

/// Counts for the overlap report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskOverlapCounts {
    /// Live latest-version tasks seen.
    pub tasks: usize,
    /// Live tasks whose latest status is in the active filter.
    pub in_flight_tasks: usize,
    /// Overlapping in-flight task pairs.
    pub pairs: usize,
    /// In-flight tasks with no resolvable footprint.
    pub no_footprint_tasks: usize,
    /// Lane diagnostics emitted.
    pub diagnostics: usize,
}

/// The overlap report: every overlapping in-flight task pair plus diagnostics.
///
/// `pairs` are in canonical ascending order; `diagnostics` are ascending by
/// task record ID. A report with an empty `pairs` list is valid and carries
/// `zero_overlaps: true` — the explicit machine-readable zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskOverlapReport {
    /// Aggregate counts.
    pub counts: TaskOverlapCounts,
    /// Overlapping pairs, canonical ascending order.
    pub pairs: Vec<OverlapPairRow>,
    /// Lane diagnostics, ascending by task record ID.
    pub diagnostics: Vec<TaskOverlapDiagnostic>,
    /// Explicit machine-readable zero-overlaps marker.
    pub zero_overlaps: bool,
}

/// Outcome of overlap resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOverlapOutcome {
    /// At least one live `Task` record exists; the report itself may be empty.
    Report(TaskOverlapReport),
    /// No live `Task` records at all: no project data to resolve.
    NoProjectData,
}

/// CLI exit-code contract (issue #150).
///
/// `0` = valid report (zero overlaps is still valid), `2` = no
/// Task/project data. Malformed `--status` filters are `3`, decided by the
/// CLI; malformed input / read failures are `1`, decided by the CLI.
#[must_use]
pub const fn task_overlap_exit_code(outcome: &TaskOverlapOutcome) -> i32 {
    match outcome {
        TaskOverlapOutcome::NoProjectData => 2,
        TaskOverlapOutcome::Report(_) => 0,
    }
}

/// How a `--status` filter value failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlapStatusFilterError {
    /// An empty entry (leading, trailing, or doubled comma).
    EmptyEntry,
    /// Not a member of the project task-status vocabulary.
    UnknownStatus(String),
    /// A vocabulary member that is never in-flight (`closed_*`, `unknown`).
    NotInFlight(String),
    /// The same status twice.
    Duplicate(String),
}

impl OverlapStatusFilterError {
    /// The stable snake_case code used in the CLI error envelope.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "invalid_status_filter"
    }
}

impl std::fmt::Display for OverlapStatusFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyEntry => write!(f, "status filter contains an empty entry"),
            Self::UnknownStatus(s) => write!(
                f,
                "unknown task status '{s}': not in the project task-status vocabulary"
            ),
            Self::NotInFlight(s) => write!(
                f,
                "task status '{s}' is not an in-flight status: overlap only compares {TASK_OVERLAP_DEFAULT_STATUS_FILTER}"
            ),
            Self::Duplicate(s) => write!(f, "duplicate status '{s}' in filter"),
        }
    }
}

impl std::error::Error for OverlapStatusFilterError {}

/// Parse and validate a `--status` filter.
///
/// `None` selects the default in-flight set. Every entry must be a member of
/// the closed project task-status vocabulary **and** an in-flight status;
/// anything else — unknown names, `closed_*`, `unknown`, empty entries,
/// duplicates — is a malformed filter.
///
/// # Errors
///
/// Returns [`OverlapStatusFilterError`] when any entry is unknown, not
/// in-flight, empty, or duplicated.
pub fn parse_overlap_status_filter(
    raw: Option<&str>,
) -> Result<BTreeSet<String>, OverlapStatusFilterError> {
    let raw = raw.unwrap_or(TASK_OVERLAP_DEFAULT_STATUS_FILTER);
    let mut out = BTreeSet::new();
    for part in raw.split(',') {
        let status = part.trim();
        if status.is_empty() {
            return Err(OverlapStatusFilterError::EmptyEntry);
        }
        if !valid_task_statuses().contains(&status) {
            return Err(OverlapStatusFilterError::UnknownStatus(status.to_owned()));
        }
        if !TASK_OVERLAP_IN_FLIGHT_STATUSES.contains(&status) {
            return Err(OverlapStatusFilterError::NotInFlight(status.to_owned()));
        }
        if !out.insert(status.to_owned()) {
            return Err(OverlapStatusFilterError::Duplicate(status.to_owned()));
        }
    }
    Ok(out)
}

/// Render a symbol span as `start_line:start_col-end_line:end_col`, omitting
/// the columns when the node records none.
fn render_span(span: &SourceSpan) -> String {
    match (span.start_column, span.end_column) {
        (Some(start_col), Some(end_col)) => {
            format!(
                "{}:{start_col}-{}:{end_col}",
                span.start_line, span.end_line
            )
        }
        _ => format!("{}-{}", span.start_line, span.end_line),
    }
}

/// Resolve overlap over project-graph records.
///
/// Read-only and deterministic: only the latest `transaction_time` version of
/// each node/edge participates, every traversal is over sorted maps, and rows
/// are emitted in canonical ascending order, so repeated runs serialize
/// byte-identically.
pub fn task_overlap_report(
    records: &[GraphRecord],
    in_flight: &BTreeSet<String>,
) -> TaskOverlapOutcome {
    // One footprint entry per live Symbol/File handle, keyed by record ID,
    // carrying every leg that resolved it.
    struct FootprintEntry<'a> {
        record: &'a GraphRecord,
        via: BTreeSet<FootprintLeg>,
    }

    /// Insert `target` into `footprint` when it is a live Symbol/File node.
    fn insert_handle<'a>(
        footprint: &mut BTreeMap<&'a str, FootprintEntry<'a>>,
        live_nodes: &BTreeMap<&'a str, &'a GraphRecord>,
        target: &'a str,
        leg: FootprintLeg,
    ) {
        let Some(target_record) = live_nodes.get(target) else {
            return;
        };
        let GraphRecord::Node { kind, .. } = target_record else {
            return;
        };
        if !matches!(kind, NodeKind::Symbol | NodeKind::File) {
            return;
        }
        footprint
            .entry(target)
            .or_insert(FootprintEntry {
                record: target_record,
                via: BTreeSet::new(),
            })
            .via
            .insert(leg);
    }

    let liveness = Liveness::new(records);

    // ── Live Task nodes: latest version per stable ID ─────────────────────
    // The representative version is the one with the greatest
    // `transaction_time` (RFC3339 strings compare lexicographically); a later
    // read order breaks ties. Tombstoned IDs are out entirely.
    let mut node_versions: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Task,
            ..
        } = record
            && !liveness.deleted(id.as_str())
        {
            node_versions.entry(id.as_str()).or_default().push(record);
        }
    }
    if node_versions.is_empty() {
        return TaskOverlapOutcome::NoProjectData;
    }
    let live_tasks: BTreeMap<&str, &GraphRecord> = node_versions
        .iter()
        .filter_map(|(id, versions)| {
            fn tx(record: &GraphRecord) -> &str {
                match record {
                    GraphRecord::Node {
                        transaction_time, ..
                    } => transaction_time.as_deref().unwrap_or(""),
                    _ => "",
                }
            }
            // `max_by` returns the last maximum on ties, so read order breaks
            // transaction_time ties.
            versions
                .iter()
                .max_by(|a, b| tx(a).cmp(tx(b)))
                .map(|record| (*id, *record))
        })
        .collect();

    // ── Live latest-version nodes (code handles, sessions, links) ─────────
    let mut live_nodes: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if let GraphRecord::Node { id, .. } = record
            && !liveness.deleted(id.as_str())
            && liveness.is_latest_node_version(id.as_str(), index)
        {
            live_nodes.insert(id.as_str(), record);
        }
    }

    // ── Live latest-version edges, forward and reverse ────────────────────
    let mut out_edges: BTreeMap<&str, Vec<(EdgeLabel, &str)>> = BTreeMap::new();
    let mut in_edges: BTreeMap<&str, Vec<(&str, EdgeLabel)>> = BTreeMap::new();
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
        out_edges
            .entry(source.as_str())
            .or_default()
            .push((*label, target.as_str()));
        in_edges
            .entry(target.as_str())
            .or_default()
            .push((source.as_str(), *label));
    }

    // ── Source-handle resolution (mirrors task_ready_report) ──────────────
    // Live ExternalLink nodes by record ID, and live EXTERNAL_HANDLE edges
    // (task -> link). A GitHub-backed task's display handle resolves through
    // its `source_external_link_id` (falling back to the edge) to the link's
    // system handle (native ID plus URL); local tasks keep their own handle.
    let mut links: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for (id, node) in &live_nodes {
        if let GraphRecord::Node {
            kind: NodeKind::ExternalLink,
            ..
        } = node
        {
            links.insert(id, node);
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

    // ── Footprint resolution ──────────────────────────────────────────────
    let mut footprints: BTreeMap<&str, BTreeMap<&str, FootprintEntry<'_>>> = BTreeMap::new();
    let mut in_flight_tasks = 0usize;
    for (id, node) in &live_tasks {
        let GraphRecord::Node { status, .. } = node else {
            continue;
        };
        let status = status.as_deref().unwrap_or("unknown");
        if !in_flight.contains(status) {
            continue;
        }
        in_flight_tasks += 1;

        let mut footprint: BTreeMap<&str, FootprintEntry<'_>> = BTreeMap::new();
        // Direct leg: the task's own MENTIONS_SYMBOL / TOUCHES_FILE edges.
        if let Some(edges) = out_edges.get(*id) {
            for (label, target) in edges {
                if matches!(label, EdgeLabel::MentionsSymbol | EdgeLabel::TouchesFile) {
                    insert_handle(&mut footprint, &live_nodes, target, FootprintLeg::Direct);
                }
            }
        }
        // Session leg: live nodes with a live REFERENCES_TASK edge to the
        // task, followed forward through TOUCHED_FILE / MENTIONS_SYMBOL.
        if let Some(referrers) = in_edges.get(*id) {
            for (referrer, label) in referrers {
                if *label != EdgeLabel::ReferencesTask || !live_nodes.contains_key(referrer) {
                    continue;
                }
                if let Some(edges) = out_edges.get(*referrer) {
                    for (touch_label, target) in edges {
                        if matches!(
                            touch_label,
                            EdgeLabel::TouchedFile | EdgeLabel::MentionsSymbol
                        ) {
                            insert_handle(
                                &mut footprint,
                                &live_nodes,
                                target,
                                FootprintLeg::Session,
                            );
                        }
                    }
                }
            }
        }
        footprints.insert(id, footprint);
    }

    // ── Diagnostics: in-flight tasks with no resolvable footprint ─────────
    // A task lands here when it has no touch edges at all, when its edges
    // are tombstoned/superseded, or when every target is absent, tombstoned,
    // or not a Symbol/File. It is reported, never silently ignored and never
    // fabricated into a pair.
    let mut diagnostics: Vec<TaskOverlapDiagnostic> = Vec::new();
    for (id, footprint) in &footprints {
        if footprint.is_empty() {
            diagnostics.push(TaskOverlapDiagnostic {
                code: TaskOverlapDiagnosticCode::NoFootprint,
                message: format!(
                    "task '{id}' is in-flight but has no resolvable footprint: no live MENTIONS_SYMBOL / TOUCHES_FILE edge and no live REFERENCES_TASK session path resolves to a live Symbol/File handle"
                ),
                task_record_id: Some((*id).to_owned()),
            });
        }
    }
    diagnostics.sort_by(|a, b| a.task_record_id.cmp(&b.task_record_id));

    // ── Pairs: unordered in-flight pairs sharing ≥1 handle ────────────────
    // Canonical order: ascending (task_a, task_b) by record ID; shared
    // handles ascending by record ID. BTreeMap iteration is already sorted,
    // so the nested loop below emits pairs in canonical order directly.
    let mut pairs: Vec<OverlapPairRow> = Vec::new();
    let footprint_tasks: Vec<(&str, &BTreeMap<&str, FootprintEntry<'_>>)> = footprints
        .iter()
        .filter(|(_, fp)| !fp.is_empty())
        .map(|(id, fp)| (*id, fp))
        .collect();
    for (i, (a_id, a_fp)) in footprint_tasks.iter().enumerate() {
        for (b_id, b_fp) in &footprint_tasks[i + 1..] {
            let mut shared: Vec<OverlapHandleRow> = Vec::new();
            for (handle_id, a_entry) in *a_fp {
                let Some(b_entry) = b_fp.get(handle_id) else {
                    continue;
                };
                let GraphRecord::Node {
                    kind,
                    repo_relative_path,
                    span,
                    ..
                } = a_entry.record
                else {
                    continue;
                };
                let via: BTreeSet<FootprintLeg> =
                    a_entry.via.union(&b_entry.via).copied().collect();
                shared.push(OverlapHandleRow {
                    record_id: (*handle_id).to_owned(),
                    kind: kind.as_str().to_owned(),
                    repo_relative_path: repo_relative_path.clone(),
                    span: if *kind == NodeKind::Symbol {
                        span.as_ref().map(render_span)
                    } else {
                        None
                    },
                    via: via
                        .into_iter()
                        .map(FootprintLeg::as_str)
                        .map(str::to_owned)
                        .collect(),
                });
            }
            if shared.is_empty() {
                continue;
            }
            let identity = |id: &str| -> TaskIdentityRow {
                let GraphRecord::Node { title, status, .. } = live_tasks[id] else {
                    // Unreachable: live_tasks only holds nodes.
                    return TaskIdentityRow {
                        record_id: id.to_owned(),
                        title: None,
                        status: "unknown".to_owned(),
                        source_handle: None,
                    };
                };
                TaskIdentityRow {
                    record_id: id.to_owned(),
                    title: title.clone(),
                    status: status.clone().unwrap_or_else(|| "unknown".to_owned()),
                    source_handle: resolve_handle(live_tasks[id]),
                }
            };
            pairs.push(OverlapPairRow {
                task_a: identity(a_id),
                task_b: identity(b_id),
                shared_handles: shared,
            });
        }
    }

    let no_footprint_tasks = diagnostics.len();
    let zero_overlaps = pairs.is_empty();
    let report = TaskOverlapReport {
        counts: TaskOverlapCounts {
            tasks: live_tasks.len(),
            in_flight_tasks,
            pairs: pairs.len(),
            no_footprint_tasks,
            diagnostics: diagnostics.len(),
        },
        pairs,
        diagnostics,
        zero_overlaps,
    };
    TaskOverlapOutcome::Report(report)
}

#[cfg(test)]
mod tests {
    //! RED-first tests for issue #150 (SPEC-PROOF-RED-GREEN-REFACTOR).
    //!
    //! The fixture mirrors the acceptance criteria: T1/T2 share a symbol via
    //! direct `MENTIONS_SYMBOL` edges and a file via the session-mediated
    //! path; T3 is disjoint; T4/T5 are terminal; T6 has no resolvable
    //! footprint; T7 proves only the latest-`transaction_time` status counts
    //! (open then closed); T8 proves it the other way (closed then open).
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::{
        OverlapStatusFilterError, TaskOverlapDiagnosticCode, TaskOverlapOutcome,
        parse_overlap_status_filter, task_overlap_exit_code, task_overlap_report,
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
        extra["title"] = json!(format!("Task {id}"));
        extra["status"] = json!(status);
        // Callers may pin an explicit transaction_time; the default keeps
        // single-version tasks comparable.
        if extra.get("transaction_time").is_none() {
            extra["transaction_time"] = json!("2026-09-28T00:00:00Z");
        }
        node(id, "Task", &extra)
    }

    fn edge(id: &str, label: &str, source: &str, target: &str) -> String {
        json!({
            "record_type": "edge",
            "id": id,
            "schema_version": 1,
            "label": label,
            "source": source,
            "target": target,
            "summary": format!("{label} {source} -> {target}"),
        })
        .to_string()
    }

    fn tombstone(id: &str) -> String {
        json!({
            "record_type": "tombstone",
            "id": format!("tomb:{id}"),
            "deleted_id": id,
            "schema_version": 1,
            "summary": format!("tombstone {id}"),
        })
        .to_string()
    }

    /// The AC fixture: T1/T2 overlap on a symbol (direct) and a file (via
    /// sessions); T3 disjoint; T4/T5 terminal; T6 no resolvable footprint;
    /// T7 open→closed (excluded); T8 closed→open (included).
    fn ac_fixture() -> Vec<GraphRecord> {
        let lines = [
            // ── Tasks ──
            task(
                "task:t1",
                "open",
                json!({"source_external_link_id": "link:gh-42"}),
            ),
            task(
                "task:t2",
                "in_progress",
                json!({"source_handle": "tasks%2Facme.jsonl:t2 <file://tasks/acme.jsonl>"}),
            ),
            task("task:t3", "open", json!({})),
            task("task:t4", "closed_completed", json!({})),
            task("task:t5", "closed_dropped", json!({})),
            task("task:t6", "open", json!({})),
            // T7: latest transaction_time status is closed_completed.
            task(
                "task:t7",
                "open",
                json!({"transaction_time": "2026-09-28T00:00:00Z"}),
            ),
            task(
                "task:t7",
                "closed_completed",
                json!({"transaction_time": "2026-09-28T00:00:01Z"}),
            ),
            // T8: latest transaction_time status is open.
            task(
                "task:t8",
                "closed_dropped",
                json!({"transaction_time": "2026-09-28T00:00:00Z"}),
            ),
            task(
                "task:t8",
                "open",
                json!({"transaction_time": "2026-09-28T00:00:01Z"}),
            ),
            // ── External link for T1 (GitHub issue handle) ──
            node(
                "link:gh-42",
                "ExternalLink",
                &json!({
                    "system": "github",
                    "system_native_id": "#42",
                    "url": "https://github.com/acme/repo/issues/42",
                }),
            ),
            // ── Code handles ──
            node(
                "sym:s1",
                "Symbol",
                &json!({
                    "repo_relative_path": "src/wire.rs",
                    "name": "parse_frame",
                    "span": {
                        "start_byte": 100, "end_byte": 200,
                        "start_line": 10, "end_line": 14,
                        "start_column": 4, "end_column": 1,
                    },
                }),
            ),
            node(
                "file:f1",
                "File",
                &json!({"repo_relative_path": "src/wire.rs"}),
            ),
            node(
                "sym:s2",
                "Symbol",
                &json!({
                    "repo_relative_path": "src/other.rs",
                    "name": "helper",
                    "span": {
                        "start_byte": 10, "end_byte": 50,
                        "start_line": 3, "end_line": 5,
                    },
                }),
            ),
            // Not a Symbol/File: never a footprint handle.
            node(
                "mod:m1",
                "Module",
                &json!({"repo_relative_path": "src/wire.rs"}),
            ),
            // ── Sessions (agent-memory nodes; raw payloads must never leak) ──
            node(
                "sess:a",
                "AgentRun",
                &json!({"text": "CANARY-session-a-transcript"}),
            ),
            node(
                "sess:b",
                "AgentRun",
                &json!({"arguments_summary": "CANARY-session-b-cmd-output"}),
            ),
            // ── Footprint edges ──
            edge("e1", "MENTIONS_SYMBOL", "task:t1", "sym:s1"),
            edge("e2", "MENTIONS_SYMBOL", "task:t2", "sym:s1"),
            edge("e3", "REFERENCES_TASK", "sess:a", "task:t1"),
            edge("e4", "TOUCHED_FILE", "sess:a", "file:f1"),
            edge("e5", "REFERENCES_TASK", "sess:b", "task:t2"),
            edge("e6", "TOUCHED_FILE", "sess:b", "file:f1"),
            // T3: disjoint footprint.
            edge("e7", "MENTIONS_SYMBOL", "task:t3", "sym:s2"),
            // T4/T5: terminal, must never appear in pairs.
            edge("e8", "MENTIONS_SYMBOL", "task:t4", "sym:s1"),
            edge("e9", "MENTIONS_SYMBOL", "task:t5", "sym:s1"),
            // T7's footprint is moot: latest status is closed_completed.
            edge("e10", "MENTIONS_SYMBOL", "task:t7", "sym:s1"),
            // T8's latest status is open: it pairs with T1/T2.
            edge("e11", "MENTIONS_SYMBOL", "task:t8", "sym:s1"),
            // T6 points at a Module: not a Symbol/File, so no resolvable
            // footprint despite having an edge.
            edge("e12", "MENTIONS_SYMBOL", "task:t6", "mod:m1"),
        ];
        lines
            .iter()
            .map(|l| serde_json::from_str::<GraphRecord>(l).expect("valid fixture line"))
            .collect()
    }

    fn default_filter() -> BTreeSet<String> {
        parse_overlap_status_filter(None).expect("default filter parses")
    }

    fn report(records: &[GraphRecord]) -> super::TaskOverlapReport {
        match task_overlap_report(records, &default_filter()) {
            TaskOverlapOutcome::Report(r) => r,
            TaskOverlapOutcome::NoProjectData => panic!("expected a report, got NoProjectData"),
        }
    }

    #[test]
    fn overlap_pairs_found_via_direct_and_session_legs() {
        let r = report(&ac_fixture());
        let pair_ids: Vec<(&str, &str)> = r
            .pairs
            .iter()
            .map(|p| (p.task_a.record_id.as_str(), p.task_b.record_id.as_str()))
            .collect();
        assert_eq!(
            pair_ids,
            vec![
                ("task:t1", "task:t2"),
                ("task:t1", "task:t8"),
                ("task:t2", "task:t8"),
            ],
            "canonical ascending pair order"
        );

        let t1t2 = &r.pairs[0];
        let handles: Vec<(&str, &str, Vec<&str>)> = t1t2
            .shared_handles
            .iter()
            .map(|h| {
                (
                    h.record_id.as_str(),
                    h.kind.as_str(),
                    h.via.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            handles,
            vec![
                ("file:f1", "File", vec!["session"]),
                ("sym:s1", "Symbol", vec!["direct"]),
            ],
            "shared handles ascending by record ID with their footprint legs"
        );

        let s1 = &t1t2.shared_handles[1];
        assert_eq!(s1.repo_relative_path.as_deref(), Some("src/wire.rs"));
        assert_eq!(s1.span.as_deref(), Some("10:4-14:1"));

        let f1 = &t1t2.shared_handles[0];
        assert_eq!(f1.repo_relative_path.as_deref(), Some("src/wire.rs"));
        assert_eq!(f1.span, None, "files carry no span");

        // Source handles: T1 via GitHub ExternalLink, T2 via local handle.
        assert_eq!(
            t1t2.task_a.source_handle.as_deref(),
            Some("#42 <https://github.com/acme/repo/issues/42>")
        );
        assert_eq!(
            t1t2.task_b.source_handle.as_deref(),
            Some("tasks%2Facme.jsonl:t2 <file://tasks/acme.jsonl>")
        );
        assert!(!r.zero_overlaps);
    }

    #[test]
    fn touches_file_is_a_direct_footprint_leg() {
        // A task that declares TOUCHES_FILE on a file overlaps a task whose
        // session touched the same file.
        let lines = [
            task("task:a", "open", json!({})),
            task("task:b", "open", json!({})),
            node("sess:x", "AgentRun", &json!({})),
            node("file:f", "File", &json!({"repo_relative_path": "f.rs"})),
            edge("e1", "TOUCHES_FILE", "task:a", "file:f"),
            edge("e2", "REFERENCES_TASK", "sess:x", "task:b"),
            edge("e3", "TOUCHED_FILE", "sess:x", "file:f"),
        ];
        let records: Vec<GraphRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid line"))
            .collect();
        let r = report(&records);
        assert_eq!(r.pairs.len(), 1);
        let via: Vec<&str> = r.pairs[0].shared_handles[0]
            .via
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(via, vec!["direct", "session"]);
    }

    #[test]
    fn closed_and_terminal_tasks_never_appear() {
        let r = report(&ac_fixture());
        for p in &r.pairs {
            for t in [&p.task_a, &p.task_b] {
                assert!(
                    !["closed_completed", "closed_dropped"].contains(&t.status.as_str()),
                    "terminal task {} in pair",
                    t.record_id
                );
                assert!(
                    !["task:t4", "task:t5", "task:t7"].contains(&t.record_id.as_str()),
                    "excluded task {} in pair",
                    t.record_id
                );
            }
        }
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.task_record_id.as_deref() == Some("task:t4")),
            "terminal tasks get no footprint diagnostics either"
        );
    }

    #[test]
    fn disjoint_task_is_in_no_pair() {
        let r = report(&ac_fixture());
        assert!(
            r.pairs
                .iter()
                .all(|p| p.task_a.record_id != "task:t3" && p.task_b.record_id != "task:t3"),
            "disjoint T3 must appear in no pair"
        );
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.task_record_id.as_deref() == Some("task:t3")),
            "T3 has a footprint, so no diagnostic"
        );
    }

    #[test]
    fn task_with_no_resolvable_footprint_gets_diagnostic() {
        let r = report(&ac_fixture());
        assert!(
            r.pairs
                .iter()
                .all(|p| p.task_a.record_id != "task:t6" && p.task_b.record_id != "task:t6"),
            "T6 must appear in no pair"
        );
        let diag = r
            .diagnostics
            .iter()
            .find(|d| d.task_record_id.as_deref() == Some("task:t6"))
            .expect("T6 gets a no_footprint diagnostic");
        assert_eq!(diag.code, TaskOverlapDiagnosticCode::NoFootprint);
        assert_eq!(diag.code.code_as_str(), "no_footprint");
        assert!(
            diag.message.contains("no resolvable footprint"),
            "diagnostic is documented, never silent: {}",
            diag.message
        );
        assert_eq!(r.counts.no_footprint_tasks, 1);
    }

    #[test]
    fn latest_transaction_time_status_wins() {
        let r = report(&ac_fixture());
        // T7's latest status is closed_completed: excluded entirely.
        assert!(
            r.pairs
                .iter()
                .all(|p| p.task_a.record_id != "task:t7" && p.task_b.record_id != "task:t7")
        );
        // T8's latest status is open: in-flight, paired on sym:s1.
        let t8_count = r
            .pairs
            .iter()
            .filter(|p| p.task_a.record_id == "task:t8" || p.task_b.record_id == "task:t8")
            .count();
        assert_eq!(t8_count, 2, "T8 pairs with T1 and T2");
        assert_eq!(r.counts.in_flight_tasks, 5);
    }

    #[test]
    fn transaction_time_outranks_read_order() {
        // task:t9 lists `open` LAST in read order, but its `closed_completed`
        // version carries the later transaction_time: it must be excluded,
        // proving version selection compares transaction_time rather than
        // trusting read order.
        let lines = [
            task(
                "task:t9",
                "closed_completed",
                json!({"transaction_time": "2026-09-28T00:00:01Z"}),
            ),
            task(
                "task:t9",
                "open",
                json!({"transaction_time": "2026-09-28T00:00:00Z"}),
            ),
            task("task:t10", "open", json!({})),
            node("sym:s", "Symbol", &json!({"repo_relative_path": "a.rs"})),
            edge("e1", "MENTIONS_SYMBOL", "task:t9", "sym:s"),
            edge("e2", "MENTIONS_SYMBOL", "task:t10", "sym:s"),
        ];
        let records: Vec<GraphRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid line"))
            .collect();
        let r = report(&records);
        assert!(r.pairs.is_empty(), "t9 is closed: no pair is emitted");
        assert!(r.zero_overlaps);
        assert_eq!(r.counts.in_flight_tasks, 1, "only t10 is in-flight");
        assert!(
            r.diagnostics.is_empty(),
            "t10 has a footprint and t9 is not in-flight"
        );
    }

    #[test]
    fn zero_overlaps_is_explicit_and_distinct_from_no_project_data() {
        // In-flight tasks, all with disjoint footprints: valid report, zero pairs.
        let lines = [
            task("task:a", "open", json!({})),
            task("task:b", "blocked", json!({})),
            node("sym:a", "Symbol", &json!({"repo_relative_path": "a.rs"})),
            node("sym:b", "Symbol", &json!({"repo_relative_path": "b.rs"})),
            edge("e1", "MENTIONS_SYMBOL", "task:a", "sym:a"),
            edge("e2", "MENTIONS_SYMBOL", "task:b", "sym:b"),
        ];
        let records: Vec<GraphRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid line"))
            .collect();
        let outcome = task_overlap_report(&records, &default_filter());
        let TaskOverlapOutcome::Report(r) = &outcome else {
            panic!("expected a report");
        };
        assert!(r.pairs.is_empty());
        assert!(r.zero_overlaps, "zero overlaps is an explicit marker");
        assert_eq!(task_overlap_exit_code(&outcome), 0);

        // No Task records at all: a different outcome and exit code.
        let empty: Vec<GraphRecord> = vec![node(
            "sym:a",
            "Symbol",
            &json!({"repo_relative_path": "a.rs"}),
        )]
        .into_iter()
        .map(|l| serde_json::from_str::<GraphRecord>(&l).expect("valid line"))
        .collect();
        let outcome = task_overlap_report(&empty, &default_filter());
        assert_eq!(outcome, TaskOverlapOutcome::NoProjectData);
        assert_eq!(task_overlap_exit_code(&outcome), 2);
    }

    #[test]
    fn status_filter_parse_defaults_and_rejects_malformed() {
        assert_eq!(
            default_filter(),
            BTreeSet::from([
                "open".to_owned(),
                "in_progress".to_owned(),
                "blocked".to_owned()
            ])
        );
        // Unknown vocabulary member.
        assert_eq!(
            parse_overlap_status_filter(Some("open,bogus")),
            Err(OverlapStatusFilterError::UnknownStatus("bogus".to_owned()))
        );
        // Valid vocabulary but never in-flight.
        assert_eq!(
            parse_overlap_status_filter(Some("open,closed_completed")),
            Err(OverlapStatusFilterError::NotInFlight(
                "closed_completed".to_owned()
            ))
        );
        assert_eq!(
            parse_overlap_status_filter(Some("superseded")),
            Err(OverlapStatusFilterError::UnknownStatus(
                "superseded".to_owned()
            ))
        );
        // Empty entry and duplicates.
        assert_eq!(
            parse_overlap_status_filter(Some("open,")),
            Err(OverlapStatusFilterError::EmptyEntry)
        );
        assert_eq!(
            parse_overlap_status_filter(Some("open,open")),
            Err(OverlapStatusFilterError::Duplicate("open".to_owned()))
        );
        // A single in-flight status narrows the set.
        assert_eq!(
            parse_overlap_status_filter(Some("blocked")),
            Ok(BTreeSet::from(["blocked".to_owned()]))
        );
        // The diagnostic code is stable for every malformation.
        for bad in ["bogus", "open,", "open,open", "closed_dropped"] {
            let err = parse_overlap_status_filter(Some(bad)).expect_err("malformed");
            assert_eq!(err.code(), "invalid_status_filter");
        }
    }

    #[test]
    fn status_filter_restricts_the_in_flight_set() {
        let records = ac_fixture();
        let open_only = parse_overlap_status_filter(Some("open")).expect("valid");
        let TaskOverlapOutcome::Report(r) = task_overlap_report(&records, &open_only) else {
            panic!("expected a report");
        };
        let pair_ids: Vec<(&str, &str)> = r
            .pairs
            .iter()
            .map(|p| (p.task_a.record_id.as_str(), p.task_b.record_id.as_str()))
            .collect();
        // T2 is in_progress: excluded, so only (T1,T8) remains.
        assert_eq!(pair_ids, vec![("task:t1", "task:t8")]);
        // T2 is out of scope: no footprint diagnostic for it either.
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.task_record_id.as_deref() == Some("task:t2"))
        );
    }

    #[test]
    fn tombstoned_touch_edge_is_not_a_footprint() {
        let lines = [
            task("task:a", "open", json!({})),
            node("sym:a", "Symbol", &json!({"repo_relative_path": "a.rs"})),
            edge("e1", "MENTIONS_SYMBOL", "task:a", "sym:a"),
            tombstone("e1"),
        ];
        let records: Vec<GraphRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid line"))
            .collect();
        let r = report(&records);
        assert!(r.pairs.is_empty());
        assert!(r.zero_overlaps);
        assert_eq!(r.counts.no_footprint_tasks, 1);
        assert_eq!(
            r.diagnostics[0].code,
            TaskOverlapDiagnosticCode::NoFootprint
        );
    }

    #[test]
    fn tombstoned_session_leg_is_not_a_footprint() {
        let lines = [
            task("task:a", "open", json!({})),
            node("sess:a", "AgentRun", &json!({})),
            node("file:f", "File", &json!({"repo_relative_path": "f.rs"})),
            edge("e1", "REFERENCES_TASK", "sess:a", "task:a"),
            edge("e2", "TOUCHED_FILE", "sess:a", "file:f"),
            tombstone("sess:a"),
        ];
        let records: Vec<GraphRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid line"))
            .collect();
        let r = report(&records);
        assert!(r.zero_overlaps);
        assert_eq!(r.counts.no_footprint_tasks, 1);
    }

    #[test]
    fn tombstoned_handle_target_is_not_resolvable() {
        let lines = [
            task("task:a", "open", json!({})),
            node("sym:a", "Symbol", &json!({"repo_relative_path": "a.rs"})),
            edge("e1", "MENTIONS_SYMBOL", "task:a", "sym:a"),
            tombstone("sym:a"),
        ];
        let records: Vec<GraphRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid line"))
            .collect();
        let r = report(&records);
        assert!(r.zero_overlaps);
        assert_eq!(r.counts.no_footprint_tasks, 1);
    }

    #[test]
    fn five_runs_serialize_byte_identical() {
        let records = ac_fixture();
        let first = serde_json::to_string(&report(&records)).expect("serializes");
        for _ in 0..4 {
            let again = serde_json::to_string(&report(&records)).expect("serializes");
            assert_eq!(first, again, "runs must be byte-identical");
        }
    }

    #[test]
    fn output_never_carries_raw_artifact_payloads() {
        let r = report(&ac_fixture());
        let json = serde_json::to_string(&r).expect("serializes");
        for canary in ["CANARY-session-a-transcript", "CANARY-session-b-cmd-output"] {
            assert!(
                !json.contains(canary),
                "raw agent payload leaked into output"
            );
        }
    }

    #[test]
    fn pair_and_handle_ordering_is_canonical_regardless_of_input_order() {
        let mut records = ac_fixture();
        // Stable sort by descending ID: reorders across IDs but preserves
        // each task's same-ID version order (latest-transaction_time status
        // semantics must not change).
        records.sort_by(|a, b| b.id().cmp(a.id()));
        let r = report(&records);
        let pair_ids: Vec<(&str, &str)> = r
            .pairs
            .iter()
            .map(|p| (p.task_a.record_id.as_str(), p.task_b.record_id.as_str()))
            .collect();
        assert_eq!(
            pair_ids,
            vec![
                ("task:t1", "task:t2"),
                ("task:t1", "task:t8"),
                ("task:t2", "task:t8"),
            ]
        );
        let diag_ids: Vec<&str> = r
            .diagnostics
            .iter()
            .filter_map(|d| d.task_record_id.as_deref())
            .collect();
        assert_eq!(diag_ids, vec!["task:t6"]);
    }
}
