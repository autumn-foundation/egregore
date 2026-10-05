//! Ready-to-dispatch task resolution (issue #161).
//!
//! Pure, read-only, deterministic: given project-graph records, partition the
//! open/in-flight `Task`s into READY (every declared `DEPENDS_ON` prerequisite
//! is `closed_completed`) and BLOCKED (naming each unmet dependency with its
//! record ID and source handle).
//!
//! Trust separation: readiness derives ONLY from author-written `status` and
//! author-declared `depends_on` edges. The lane computes no judgment, ranks
//! nothing, and mutates nothing. A READY verdict is an eligibility lead —
//! "no declared prerequisite blocks this task" — never a claim the task is
//! correct, safe, or non-colliding. Pair with footprint-overlap analysis
//! (issue #150) before dispatching parallel work: eligibility AND
//! non-collision are both required to dispatch now.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::liveness::Liveness;
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

/// Task statuses eligible for readiness evaluation (author-written only).
pub const TASK_READY_CANDIDATE_STATUSES: &[&str] = &["open", "in_progress"];

/// Task statuses that remove a task from readiness evaluation entirely.
///
/// `closed_dropped` is terminal but does NOT satisfy a dependency: only
/// `closed_completed` counts as done (issue #161). A dropped task remains an
/// unmet blocker for everything that depends on it.
pub const TASK_READY_TERMINAL_STATUSES: &[&str] = &["closed_completed", "closed_dropped"];

/// Prefix of an `[unresolved_dependency]` importer diagnostic summary.
pub const UNRESOLVED_DEPENDENCY_PREFIX: &str = "[unresolved_dependency] ";

/// Machine-readable payload carried by an `[unresolved_dependency]` importer
/// diagnostic summary, after the prefix. JSON so any UTF-8 local ID parses
/// unambiguously (issue #161).
#[derive(Debug, Deserialize)]
struct UnresolvedDependencyPayload {
    task_local_id: String,
    file: String,
    line: usize,
    unknown_dep: String,
}

/// One task identity row: stable record ID plus its source handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskIdentityRow {
    /// Stable record ID of the task.
    pub record_id: String,
    /// Human-readable title, when the record carries one.
    pub title: Option<String>,
    /// Author-written status string.
    pub status: String,
    /// Source handle: the local-JSONL handle, or the GitHub issue/PR native
    /// ID plus URL resolved through the task's external handle.
    pub source_handle: Option<String>,
}

/// How an unmet dependency's target resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmetResolution {
    /// Target is a live `Task` whose status is not `closed_completed`.
    Resolved,
    /// The declared local ID matched no task known to the importer.
    Unresolved,
    /// The edge target names a record ID with no live `Task` (defensive; the
    /// local importer never emits this shape).
    TargetAbsent,
}

/// One unmet dependency blocking a candidate task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnmetDependency {
    /// Stable record ID of the blocking task (`None` when unresolved).
    pub record_id: Option<String>,
    /// The declared local ID when it matched nothing (`None` when resolved).
    pub declared_local_id: Option<String>,
    /// Title of the blocking task, when known.
    pub title: Option<String>,
    /// The blocking task's author-written status (`None` when unresolved).
    pub status: Option<String>,
    /// Source handle of the blocking task; for unresolved declarations, the
    /// declaring task's own handle (where the bad reference lives).
    pub source_handle: Option<String>,
    /// How the dependency target resolved.
    pub resolution: UnmetResolution,
}

/// A candidate task blocked on at least one unmet dependency or in a cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockedTaskRow {
    /// The blocked task's identity row (flattened into the JSON object).
    #[serde(flatten)]
    pub task: TaskIdentityRow,
    /// What keeps the task from being ready, in deterministic order.
    pub unmet_dependencies: Vec<UnmetDependency>,
    /// Whether the task sits in a dependency cycle.
    pub in_cycle: bool,
}

/// Lane diagnostic codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskReadyDiagnosticCode {
    /// A dependency cycle was diagnosed (exact member set reported).
    DependencyCycle,
    /// A `depends_on` entry named a task unknown to the importer.
    UnresolvedDependency,
}

impl TaskReadyDiagnosticCode {
    /// The stable snake_case code used in JSON output and text rendering.
    #[must_use]
    pub const fn code_as_str(self) -> &'static str {
        match self {
            Self::DependencyCycle => "dependency_cycle",
            Self::UnresolvedDependency => "unresolved_dependency",
        }
    }
}

/// A lane-level diagnostic: dependency cycles and unresolved declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskReadyDiagnostic {
    /// Machine-readable diagnostic code.
    pub code: TaskReadyDiagnosticCode,
    /// Human-readable explanation.
    pub message: String,
    /// Cycle member record IDs in ascending order; empty otherwise.
    pub members: Vec<String>,
    /// The declaring task's record ID (unresolved-dependency only).
    pub task_record_id: Option<String>,
    /// The unknown declared local ID (unresolved-dependency only).
    pub unknown_dep: Option<String>,
}

/// Counts for the readiness report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskReadyCounts {
    /// Live latest-version tasks seen.
    pub tasks: usize,
    /// Live latest-version `DEPENDS_ON` edges seen.
    pub dependency_edges: usize,
    /// Candidates with every dependency satisfied.
    pub ready: usize,
    /// Candidates held by unmet dependencies or cycles.
    pub blocked: usize,
    /// Tasks whose author-written status is `blocked` (not candidates).
    pub author_blocked: usize,
    /// Lane diagnostics emitted.
    pub diagnostics: usize,
}

/// The readiness report: every candidate task partitioned by eligibility.
///
/// `ready` and `blocked` are in ascending record-ID order; `blocked` rows
/// carry their unmet dependencies in ascending record-ID order (resolved
/// first, then unresolved declarations, then absent targets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskReadyReport {
    /// Aggregate counts.
    pub counts: TaskReadyCounts,
    /// Ready tasks, ascending record ID.
    pub ready: Vec<TaskIdentityRow>,
    /// Blocked candidates, ascending record ID.
    pub blocked: Vec<BlockedTaskRow>,
    /// Author-blocked tasks, ascending record ID.
    pub author_blocked: Vec<TaskIdentityRow>,
    /// Lane diagnostics, cycles first then unresolved declarations.
    pub diagnostics: Vec<TaskReadyDiagnostic>,
    /// Whether any dependency cycle was diagnosed.
    pub has_cycle: bool,
}

/// Outcome of readiness resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskReadyOutcome {
    /// At least one live `Task` record exists; the report itself may be empty.
    Ready(TaskReadyReport),
    /// No live `Task` records at all: no project data to resolve.
    NoProjectData,
}

/// CLI exit-code contract (issue #161).
///
/// `0` = valid report (an empty ready set is still valid), `2` = no
/// Task/project data, `3` = dependency cycle diagnosed. Malformed input /
/// read failures are `1`, decided by the CLI.
#[must_use]
pub const fn task_ready_exit_code(outcome: &TaskReadyOutcome) -> i32 {
    match outcome {
        TaskReadyOutcome::NoProjectData => 2,
        TaskReadyOutcome::Ready(report) if report.has_cycle => 3,
        TaskReadyOutcome::Ready(_) => 0,
    }
}

/// Resolve readiness over project-graph records.
///
/// Read-only and deterministic: iterating `BTreeMap`s in key order and
/// emitting rows in ascending record-ID order, so repeated runs serialize
/// byte-identically.
pub fn task_ready_report(records: &[GraphRecord]) -> TaskReadyOutcome {
    let liveness = Liveness::new(records);

    // ── Live Task nodes: latest version per stable ID ─────────────────────
    // Mirrors run_criteria_coverage: tombstoned IDs are out, and the last
    // record in read order is the representative version.
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
        return TaskReadyOutcome::NoProjectData;
    }
    let live_tasks: BTreeMap<&str, &GraphRecord> = node_versions
        .iter()
        .filter_map(|(id, versions)| versions.last().map(|last| (*id, *last)))
        .collect();

    // ── Live latest-version DEPENDS_ON edges ──────────────────────────────
    let mut dep_adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for id in live_tasks.keys() {
        dep_adj.insert(id, BTreeSet::new());
    }
    let mut dependency_edges = 0usize;
    for (index, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            label: EdgeLabel::DependsOn,
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
        // Edges from non-live sources are moot; targets that are not live
        // tasks are kept so the blocker is still named (target_absent).
        if live_tasks.contains_key(source.as_str()) {
            dep_adj
                .entry(source.as_str())
                .or_default()
                .insert(target.as_str());
            dependency_edges += 1;
        }
    }
    // Ensure every referenced target is a Tarjan node (inert when absent).
    // Collect first: inserting while iterating `values()` would alias borrows.
    let referenced: Vec<&str> = dep_adj.values().flatten().copied().collect();
    for target in referenced {
        dep_adj.entry(target).or_default();
    }

    // ── Source-handle resolution ──────────────────────────────────────────
    // Live ExternalLink nodes by record ID, and live EXTERNAL_HANDLE edges
    // (task -> link). A GitHub-backed task's display handle resolves through
    // its `source_external_link_id` (falling back to the edge) to the link's
    // system handle (native ID plus URL); local tasks keep their own handle.
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

    // ── Importer unresolved-dependency diagnostics, attributed to tasks ───
    // Keyed by (repo_relative_path, local_id from node `name`) -> task record ID.
    let mut task_by_source: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for (id, node) in &live_tasks {
        if let GraphRecord::Node {
            repo_relative_path: Some(path),
            name: Some(local_id),
            ..
        } = node
        {
            task_by_source.insert((path.as_str(), local_id.as_str()), id);
        }
    }
    let mut unresolved: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut unresolved_diags: Vec<TaskReadyDiagnostic> = Vec::new();
    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Diagnostic,
            summary,
            ..
        } = record
        else {
            continue;
        };
        let Some(payload_json) = summary.strip_prefix(UNRESOLVED_DEPENDENCY_PREFIX) else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<UnresolvedDependencyPayload>(payload_json) else {
            continue;
        };
        let Some(task_id) =
            task_by_source.get(&(payload.file.as_str(), payload.task_local_id.as_str()))
        else {
            continue;
        };
        unresolved
            .entry(*task_id)
            .or_default()
            .insert(payload.unknown_dep.clone());
        unresolved_diags.push(TaskReadyDiagnostic {
            code: TaskReadyDiagnosticCode::UnresolvedDependency,
            message: format!(
                "task '{}' depends on unknown task '{}' ({} line {})",
                payload.task_local_id, payload.unknown_dep, payload.file, payload.line
            ),
            members: Vec::new(),
            task_record_id: Some((*task_id).to_owned()),
            unknown_dep: Some(payload.unknown_dep),
        });
    }
    unresolved_diags.sort_by(|a, b| {
        (a.task_record_id.as_deref(), a.unknown_dep.as_deref())
            .cmp(&(b.task_record_id.as_deref(), b.unknown_dep.as_deref()))
    });

    // ── Cycle detection: exact SCC members via iterative Tarjan ───────────
    // Sorted nodes and sorted adjacency keep the output deterministic.
    let owned_adj: BTreeMap<String, BTreeSet<String>> = dep_adj
        .iter()
        .map(|(k, vs)| {
            (
                (*k).to_owned(),
                vs.iter().map(|s| (*s).to_owned()).collect(),
            )
        })
        .collect();
    let mut cycles: Vec<Vec<String>> = tarjan_scc(&owned_adj)
        .into_iter()
        .filter(|scc| {
            scc.len() > 1
                || scc
                    .first()
                    .is_some_and(|only| owned_adj[only.as_str()].contains(only))
        })
        .collect();
    cycles.sort();
    let cycle_members: BTreeSet<&str> = cycles.iter().flatten().map(String::as_str).collect();
    let mut diagnostics: Vec<TaskReadyDiagnostic> = cycles
        .iter()
        .map(|members| TaskReadyDiagnostic {
            code: TaskReadyDiagnosticCode::DependencyCycle,
            message: format!("dependency cycle among tasks: {}", members.join(", ")),
            members: members.clone(),
            task_record_id: None,
            unknown_dep: None,
        })
        .collect();
    diagnostics.extend(unresolved_diags);
    diagnostics.sort_by(|a, b| {
        (a.code as u8, &a.members, &a.task_record_id, &a.unknown_dep).cmp(&(
            b.code as u8,
            &b.members,
            &b.task_record_id,
            &b.unknown_dep,
        ))
    });

    // ── Classification ────────────────────────────────────────────────────
    let mut ready: Vec<TaskIdentityRow> = Vec::new();
    let mut blocked: Vec<BlockedTaskRow> = Vec::new();
    let mut author_blocked: Vec<TaskIdentityRow> = Vec::new();

    for (id, node) in &live_tasks {
        let GraphRecord::Node { title, status, .. } = node else {
            continue;
        };
        let status = status.as_deref().unwrap_or("unknown");
        let row = TaskIdentityRow {
            record_id: (*id).to_owned(),
            title: title.clone(),
            status: status.to_owned(),
            source_handle: resolve_handle(node),
        };
        if TASK_READY_TERMINAL_STATUSES.contains(&status) {
            continue;
        }
        if status == "blocked" {
            // Author-declared blocked: never a readiness candidate, and never
            // confused with dependency-blocked rows.
            author_blocked.push(row);
            continue;
        }
        if !TASK_READY_CANDIDATE_STATUSES.contains(&status) {
            continue;
        }

        let mut unmet: Vec<UnmetDependency> = Vec::new();
        // Resolved targets, ascending record ID (BTreeSet iteration order).
        for target in dep_adj.get(*id).map(BTreeSet::iter).into_iter().flatten() {
            match live_tasks.get(target) {
                Some(target_node) => {
                    let GraphRecord::Node {
                        title: dep_title,
                        status: dep_status,
                        ..
                    } = target_node
                    else {
                        continue;
                    };
                    let dep_status = dep_status.as_deref().unwrap_or("unknown");
                    // Only closed_completed satisfies a dependency; a dropped
                    // task stays an unmet blocker.
                    if dep_status != "closed_completed" {
                        unmet.push(UnmetDependency {
                            record_id: Some((*target).to_owned()),
                            declared_local_id: None,
                            title: dep_title.clone(),
                            status: Some(dep_status.to_owned()),
                            source_handle: resolve_handle(target_node),
                            resolution: UnmetResolution::Resolved,
                        });
                    }
                }
                None => unmet.push(UnmetDependency {
                    record_id: Some((*target).to_owned()),
                    declared_local_id: None,
                    title: None,
                    status: None,
                    source_handle: None,
                    resolution: UnmetResolution::TargetAbsent,
                }),
            }
        }
        // Unresolved declarations, ascending declared local ID.
        if let Some(unknowns) = unresolved.get(*id) {
            for unknown in unknowns {
                unmet.push(UnmetDependency {
                    record_id: None,
                    declared_local_id: Some(unknown.clone()),
                    title: None,
                    status: None,
                    // The declaring task's handle: that is where the bad
                    // reference lives.
                    source_handle: row.source_handle.clone(),
                    resolution: UnmetResolution::Unresolved,
                });
            }
        }
        // Deterministic order: resolved (record ID) < unresolved (local ID) <
        // target-absent (record ID).
        unmet.sort_by(|a, b| {
            fn key(u: &UnmetDependency) -> (u8, &str) {
                let (rank, name) = match u.resolution {
                    UnmetResolution::Resolved => (0u8, u.record_id.as_deref().unwrap_or_default()),
                    UnmetResolution::Unresolved => {
                        (1u8, u.declared_local_id.as_deref().unwrap_or_default())
                    }
                    UnmetResolution::TargetAbsent => {
                        (2u8, u.record_id.as_deref().unwrap_or_default())
                    }
                };
                (rank, name)
            }
            key(a).cmp(&key(b))
        });

        let in_cycle = cycle_members.contains(*id);
        if in_cycle || !unmet.is_empty() {
            blocked.push(BlockedTaskRow {
                task: row,
                unmet_dependencies: unmet,
                in_cycle,
            });
        } else {
            ready.push(row);
        }
    }

    let report = TaskReadyReport {
        counts: TaskReadyCounts {
            tasks: live_tasks.len(),
            dependency_edges,
            ready: ready.len(),
            blocked: blocked.len(),
            author_blocked: author_blocked.len(),
            diagnostics: diagnostics.len(),
        },
        ready,
        blocked,
        author_blocked,
        diagnostics,
        has_cycle: !cycles.is_empty(),
    };
    TaskReadyOutcome::Ready(report)
}

/// Iterative Tarjan's strongly-connected-components over a sorted adjacency
/// map. Each component's members are sorted ascending; components are NOT
/// sorted (callers sort). Deterministic: nodes and neighbors are visited in
/// sorted order.
fn tarjan_scc(adj: &BTreeMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    let mut index_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut lowlink: BTreeMap<&str, usize> = BTreeMap::new();
    let mut on_stack: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut next_index = 0usize;
    let mut sccs: Vec<Vec<String>> = Vec::new();

    for root in adj.keys() {
        if index_of.contains_key(root.as_str()) {
            continue;
        }
        index_of.insert(root.as_str(), next_index);
        lowlink.insert(root.as_str(), next_index);
        next_index += 1;
        stack.push(root.as_str());
        on_stack.insert(root.as_str());
        // Work stack of (node, remaining-neighbor iterator), pop-based so
        // pushing a child frame never collides with a live frame borrow.
        let mut work: Vec<(&str, std::collections::btree_set::Iter<'_, String>)> =
            vec![(root.as_str(), adj[root.as_str()].iter())];
        while let Some((v, mut neighbors)) = work.pop() {
            let mut descended = false;
            while let Some(w) = neighbors.next() {
                let w = w.as_str();
                if !index_of.contains_key(w) {
                    index_of.insert(w, next_index);
                    lowlink.insert(w, next_index);
                    next_index += 1;
                    stack.push(w);
                    on_stack.insert(w);
                    // Resume v later; descend into w now.
                    work.push((v, neighbors));
                    work.push((w, adj[w].iter()));
                    descended = true;
                    break;
                } else if on_stack.contains(w) {
                    let merged = lowlink[v].min(index_of[w]);
                    lowlink.insert(v, merged);
                }
            }
            if descended {
                continue;
            }
            // v is finished: propagate to its parent and maybe close an SCC.
            if let Some((u, _)) = work.last() {
                let merged = lowlink[*u].min(lowlink[v]);
                lowlink.insert(*u, merged);
            }
            if lowlink[v] == index_of[v] {
                let mut scc = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack.remove(w);
                    scc.push(w.to_owned());
                    if w == v {
                        break;
                    }
                }
                scc.sort();
                sccs.push(scc);
            }
        }
    }
    sccs
}
