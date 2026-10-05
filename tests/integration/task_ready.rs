//! Integration tests for the task-ready lane (issue #161).
//!
//! Written RED-first per SPEC-PROOF-RED-GREEN-REFACTOR. All tests drive
//! `aletheia_egregore::query::task_ready_report` against real importer
//! output: the ready set is exactly the open/in-flight tasks whose declared
//! dependencies are ALL `closed_completed`.

use std::collections::{HashMap, HashSet};
use std::fs;

use aletheia_egregore::{
    GraphRecord, NodeKind,
    local_project::{ImportOptions, import_local_tasks},
    query::{
        TaskReadyDiagnosticCode, TaskReadyOutcome, UnmetResolution, task_ready_exit_code,
        task_ready_report,
    },
};

const FIXED_TX_TIME: &str = "2026-09-27T00:00:00Z";

fn header() -> String {
    serde_json::json!({
        "kind": "header",
        "schema_version": 1,
        "project_slug": "proj",
        "created_at": "2026-09-27T00:00:00Z",
    })
    .to_string()
}

fn task(local_id: &str, status: &str, deps: &[&str]) -> String {
    let mut v = serde_json::json!({
        "kind": "task",
        "local_id": local_id,
        "title": format!("Task {local_id}"),
        "status": status,
        "priority": "normal",
        "assignees": [],
        "labels": [],
        "created_at": "2026-09-27T00:00:00Z",
        "updated_at": "2026-09-27T00:00:00Z",
    });
    if !deps.is_empty() {
        v["depends_on"] = serde_json::json!(deps);
    }
    v.to_string()
}

fn task_revision(local_id: &str, status: &str, deps: &[&str], updated_at: &str) -> String {
    let mut v = serde_json::json!({
        "kind": "task",
        "local_id": local_id,
        "title": format!("Task {local_id}"),
        "status": status,
        "priority": "normal",
        "assignees": [],
        "labels": [],
        "created_at": "2026-09-27T00:00:00Z",
        "updated_at": updated_at,
    });
    if !deps.is_empty() {
        v["depends_on"] = serde_json::json!(deps);
    }
    v.to_string()
}

/// The AC2 fixture: chain t1<-t2<-t3, diamond t4 on t2+t3, independent t5,
/// t6 whose only dep (t0) is already `closed_completed`.
fn ac2_fixture() -> Vec<String> {
    vec![
        header(),
        task("t1", "open", &[]),
        task("t2", "open", &["t1"]),
        task("t3", "open", &["t2"]),
        task("t4", "open", &["t2", "t3"]),
        task("t5", "open", &[]),
        task("t0", "closed_completed", &[]),
        task("t6", "open", &["t0"]),
    ]
}

fn import_lines(lines: &[String]) -> Vec<GraphRecord> {
    let temp = tempfile::tempdir().expect("temp dir");
    let tasks_dir = temp.path().join("tasks");
    fs::create_dir_all(&tasks_dir).expect("tasks dir");
    fs::write(tasks_dir.join("proj.jsonl"), lines.join("\n") + "\n").expect("write fixture");
    let opts = ImportOptions {
        transaction_time: Some(FIXED_TX_TIME.to_owned()),
        ..ImportOptions::pass_through()
    };
    let result = import_local_tasks(&tasks_dir, temp.path(), &opts).expect("import succeeds");
    result.graph.records().to_vec()
}

/// Map `local_id` -> stable `record_id` for the live Task nodes.
fn task_ids_by_local(records: &[GraphRecord]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Task,
            name: Some(local_id),
            ..
        } = r
        {
            map.insert(local_id.clone(), id.clone());
        }
    }
    map
}

fn ready_report(records: &[GraphRecord]) -> aletheia_egregore::query::TaskReadyReport {
    match task_ready_report(records) {
        TaskReadyOutcome::Ready(report) => report,
        TaskReadyOutcome::NoProjectData => panic!("expected a report, got NoProjectData"),
    }
}

fn local_ids<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    by_local: &HashMap<String, String>,
) -> HashSet<String> {
    let rev: HashMap<&str, &str> = by_local
        .iter()
        .map(|(k, v)| (v.as_str(), k.as_str()))
        .collect();
    ids.into_iter()
        .map(|id| {
            (*rev
                .get(id)
                .unwrap_or_else(|| panic!("unknown record id {id}")))
            .to_owned()
        })
        .collect()
}

// ── AC2: the ready-set fixture ────────────────────────────────────────────────

#[test]
fn ready_set_is_chain_head_independent_and_closed_dep_only() {
    let records = import_lines(&ac2_fixture());
    let by_local = task_ids_by_local(&records);
    let report = ready_report(&records);

    let ready: HashSet<String> =
        local_ids(report.ready.iter().map(|r| r.record_id.as_str()), &by_local);
    assert_eq!(
        ready,
        HashSet::from(["t1".to_owned(), "t5".to_owned(), "t6".to_owned()]),
        "ready = open tasks with zero deps or all deps closed_completed"
    );

    let blocked: HashMap<String, Vec<String>> = report
        .blocked
        .iter()
        .map(|r| {
            let lid = local_ids(std::iter::once(r.task.record_id.as_str()), &by_local)
                .into_iter()
                .next()
                .unwrap();
            let unmet: Vec<String> = r
                .unmet_dependencies
                .iter()
                .map(|u| {
                    local_ids(
                        std::iter::once(
                            u.record_id.as_deref().expect("resolved dep has record_id"),
                        ),
                        &by_local,
                    )
                    .into_iter()
                    .next()
                    .unwrap()
                })
                .collect();
            (lid, unmet)
        })
        .collect();
    assert_eq!(blocked.len(), 3, "t2, t3, t4 are blocked");
    assert_eq!(blocked["t2"], vec!["t1".to_owned()]);
    assert_eq!(blocked["t3"], vec!["t2".to_owned()]);
    assert_eq!(blocked["t4"], vec!["t2".to_owned(), "t3".to_owned()]);

    assert!(
        report.diagnostics.is_empty(),
        "no diagnostics on a clean DAG"
    );
    assert!(!report.has_cycle);
    assert_eq!(task_ready_exit_code(&TaskReadyOutcome::Ready(report)), 0);
}

// ── AC3: blocked rows name unmet dependency IDs + source handles ──────────────

#[test]
fn blocked_rows_name_unmet_dependency_ids_and_source_handles() {
    let records = import_lines(&ac2_fixture());
    let report = ready_report(&records);

    for row in &report.blocked {
        assert!(
            !row.unmet_dependencies.is_empty(),
            "blocked task {} must name its blockers",
            row.task.record_id
        );
        for u in &row.unmet_dependencies {
            let rid = u
                .record_id
                .as_deref()
                .expect("resolved dep names a record id");
            assert!(
                rid.starts_with("project:v1:"),
                "stable record id, got {rid}"
            );
            let handle = u
                .source_handle
                .as_deref()
                .expect("dep carries a source handle");
            assert!(
                handle.contains(".egregore/tasks/proj.jsonl") || handle.contains("proj.jsonl"),
                "source handle names the local-JSONL path, got {handle}"
            );
            assert!(u.status.is_some(), "dep status is reported");
        }
        assert!(
            row.task
                .source_handle
                .as_deref()
                .is_some_and(|h: &str| h.contains("proj.jsonl")),
            "blocked task itself carries a source handle"
        );
    }
}

// ── AC4: transitive resolution after a status change ───────────────────────────

#[test]
fn completing_t1_makes_exactly_t2_newly_ready() {
    let mut lines = ac2_fixture();
    // Status change: t1 closes. Appended revision line, later updated_at.
    lines.push(task_revision(
        "t1",
        "closed_completed",
        &[],
        "2026-09-27T02:00:00Z",
    ));
    let records = import_lines(&lines);
    let by_local = task_ids_by_local(&records);
    let report = ready_report(&records);

    let ready: HashSet<String> =
        local_ids(report.ready.iter().map(|r| r.record_id.as_str()), &by_local);
    assert_eq!(
        ready,
        HashSet::from(["t2".to_owned(), "t5".to_owned(), "t6".to_owned()]),
        "only t2 becomes newly eligible: t3 still blocked on t2, t4 on t2+t3"
    );
    // The newly-eligible set vs the pre-change ready set {t1,t5,t6} is exactly {t2}.
    assert!(
        !ready.contains("t1"),
        "closed task is not a dispatch candidate"
    );
}

// ── AC5: dependency cycles ────────────────────────────────────────────────────

#[test]
fn dependency_cycle_is_diagnostic_with_distinct_exit_code() {
    let records = import_lines(&[
        header(),
        task("ta", "open", &["tb"]),
        task("tb", "open", &["ta"]),
        task("solo", "open", &[]),
    ]);
    let by_local = task_ids_by_local(&records);
    let outcome = task_ready_report(&records);
    let TaskReadyOutcome::Ready(report) = &outcome else {
        panic!("expected a report");
    };

    assert!(report.has_cycle);
    assert_eq!(
        task_ready_exit_code(&outcome),
        3,
        "cycles get a distinct exit code"
    );

    let cycle_diags: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == TaskReadyDiagnosticCode::DependencyCycle)
        .collect();
    assert_eq!(cycle_diags.len(), 1, "one diagnostic per cycle");
    let members: HashSet<String> =
        local_ids(cycle_diags[0].members.iter().map(String::as_str), &by_local);
    assert_eq!(members, HashSet::from(["ta".to_owned(), "tb".to_owned()]));

    let ready: HashSet<String> =
        local_ids(report.ready.iter().map(|r| r.record_id.as_str()), &by_local);
    assert_eq!(
        ready,
        HashSet::from(["solo".to_owned()]),
        "cycle members are never ready"
    );
    assert!(
        report.blocked.iter().any(|r| {
            local_ids(std::iter::once(r.task.record_id.as_str()), &by_local).contains("ta")
                && r.in_cycle
        }),
        "cycle members are reported blocked with in_cycle set"
    );
}

#[test]
fn github_backed_task_resolves_source_handle_through_external_link() {
    // Hand-built GitHub-style records: the Task carries source_external_link_id,
    // the ExternalLink carries the system handle. No importer involved.
    let task_id = "project:v1:gh-task".to_owned();
    let link_id = "project:v1:gh-link".to_owned();
    let mut task = GraphRecord::node(
        task_id,
        NodeKind::Task,
        None,
        None,
        Some("Fix the thing".to_owned()),
        "github_issue #42".to_owned(),
    );
    if let GraphRecord::Node {
        status,
        source_external_link_id,
        ..
    } = &mut task
    {
        *status = Some("open".to_owned());
        *source_external_link_id = Some(link_id.clone());
    }
    let mut link = GraphRecord::node(
        link_id,
        NodeKind::ExternalLink,
        None,
        None,
        None,
        "github issue:42".to_owned(),
    );
    if let GraphRecord::Node {
        system_native_id,
        url,
        ..
    } = &mut link
    {
        *system_native_id = Some("issue:42".to_owned());
        *url = Some("https://github.com/example/repo/issues/42".to_owned());
    }
    let report = ready_report(&[task, link]);
    assert_eq!(report.ready.len(), 1);
    assert_eq!(
        report.ready[0].source_handle.as_deref(),
        Some("issue:42 <https://github.com/example/repo/issues/42>"),
        "GitHub handle resolves to native ID plus URL"
    );
}

#[test]
fn self_dependency_is_reported_as_cycle() {
    let records = import_lines(&[header(), task("t1", "open", &["t1"])]);
    let outcome = task_ready_report(&records);
    let TaskReadyOutcome::Ready(report) = &outcome else {
        panic!("expected a report");
    };
    assert!(report.has_cycle);
    assert!(
        report.ready.is_empty(),
        "self-dependent task is never ready"
    );
    assert_eq!(task_ready_exit_code(&outcome), 3);
}

// ── AC6: dropped and unknown blockers ─────────────────────────────────────────

#[test]
fn closed_dropped_blocker_keeps_task_blocked() {
    // Documented rule: a closed_dropped dependency does NOT satisfy the
    // dependent — the prerequisite was cancelled, not completed.
    let records = import_lines(&[
        header(),
        task("tx", "closed_dropped", &[]),
        task("td", "open", &["tx"]),
    ]);
    let by_local = task_ids_by_local(&records);
    let report = ready_report(&records);

    assert!(report.ready.is_empty());
    assert_eq!(report.blocked.len(), 1);
    let row = &report.blocked[0];
    assert_eq!(
        local_ids(std::iter::once(row.task.record_id.as_str()), &by_local),
        HashSet::from(["td".to_owned()])
    );
    assert_eq!(row.unmet_dependencies.len(), 1);
    let unmet = &row.unmet_dependencies[0];
    assert_eq!(unmet.status.as_deref(), Some("closed_dropped"));
    assert_eq!(unmet.resolution, UnmetResolution::Resolved);
    assert_eq!(task_ready_exit_code(&TaskReadyOutcome::Ready(report)), 0);
}

#[test]
fn unknown_dependency_target_keeps_task_blocked_with_unresolved_marker() {
    let records = import_lines(&[header(), task("tu", "open", &["ghost"])]);
    let report = ready_report(&records);

    assert!(report.ready.is_empty());
    assert_eq!(report.blocked.len(), 1);
    let unmet = &report.blocked[0].unmet_dependencies;
    assert_eq!(unmet.len(), 1);
    assert_eq!(unmet[0].resolution, UnmetResolution::Unresolved);
    assert_eq!(unmet[0].declared_local_id.as_deref(), Some("ghost"));
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == TaskReadyDiagnosticCode::UnresolvedDependency
                && d.unknown_dep.as_deref() == Some("ghost")),
        "unresolved target surfaces as a lane diagnostic"
    );
}

// ── AC7: zero-ready vs no-data vs error ───────────────────────────────────────

#[test]
fn no_tasks_is_no_project_data_with_exit_2() {
    let records = import_lines(&[header()]);
    let outcome = task_ready_report(&records);
    assert!(
        matches!(outcome, TaskReadyOutcome::NoProjectData),
        "a store with no Task records is NoProjectData, not an empty ready set"
    );
    assert_eq!(task_ready_exit_code(&outcome), 2);
}

#[test]
fn zero_ready_with_tasks_is_valid_empty_with_exit_0() {
    // Every task blocked: valid empty ready set, still exit 0.
    // t1 is author-blocked; t2 is open but depends on t1 (not completed).
    let records = import_lines(&[
        header(),
        task("t1", "blocked", &[]),
        task("t2", "open", &["t1"]),
    ]);
    let outcome = task_ready_report(&records);
    let TaskReadyOutcome::Ready(report) = &outcome else {
        panic!("expected a report");
    };
    assert!(report.ready.is_empty(), "nothing eligible");
    assert_eq!(
        task_ready_exit_code(&outcome),
        0,
        "valid empty is exit 0, not 2"
    );
    assert_eq!(
        report.author_blocked.len(),
        1,
        "author-blocked task reported separately"
    );
}

// ── AC9: determinism ─────────────────────────────────────────────────────────

#[test]
fn five_runs_serialize_byte_identical() {
    let records = import_lines(&ac2_fixture());
    let first = match task_ready_report(&records) {
        TaskReadyOutcome::Ready(r) => serde_json::to_string(&r).expect("serialize"),
        TaskReadyOutcome::NoProjectData => panic!("expected report"),
    };
    for _ in 0..4 {
        let next = match task_ready_report(&records) {
            TaskReadyOutcome::Ready(r) => serde_json::to_string(&r).expect("serialize"),
            TaskReadyOutcome::NoProjectData => panic!("expected report"),
        };
        assert_eq!(first, next, "report must be byte-identical across runs");
    }
}

// ── Trust separation (AC8): status + deps only ────────────────────────────────

#[test]
fn priority_labels_assignees_do_not_affect_eligibility() {
    // Eligibility derives solely from author-written status + declared
    // dependencies: two tasks differing only in priority/labels/assignees
    // classify identically.
    let mk = |lid: &str, priority: &str| {
        serde_json::json!({
            "kind": "task",
            "local_id": lid,
            "title": format!("Task {lid}"),
            "status": "open",
            "priority": priority,
            "assignees": ["alice"],
            "labels": ["urgent-shiny"],
            "created_at": "2026-09-27T00:00:00Z",
            "updated_at": "2026-09-27T00:00:00Z",
        })
        .to_string()
    };
    let records = import_lines(&[header(), mk("a", "urgent"), mk("b", "low")]);
    let report = ready_report(&records);
    assert_eq!(
        report.ready.len(),
        2,
        "both open, no deps: both ready regardless of priority"
    );
}
