//! Integration tests for task `depends_on` import (issue #161).
//!
//! Written RED-first per SPEC-PROOF-RED-GREEN-REFACTOR. All tests drive the
//! contract: a `task` line may declare `depends_on: [<local_id>, ...]`; the
//! importer resolves each entry to a task in the same file and emits one
//! additive `DEPENDS_ON` edge (Task → Task) per resolved dependency. Unknown
//! targets produce an `[unresolved_dependency]` diagnostic and no edge —
//! never a silently satisfied dependency.

use std::fs;

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind,
    local_project::{ImportOptions, import_local_tasks},
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

fn task_id(records: &[GraphRecord], local_id: &str) -> String {
    records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::Task,
                name: Some(n),
                ..
            } if n == local_id => Some(id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Task node for local_id '{local_id}'"))
}

fn depends_on_edges(records: &[GraphRecord]) -> Vec<(&str, &str)> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Edge {
                label: EdgeLabel::DependsOn,
                source,
                target,
                ..
            } => Some((source.as_str(), target.as_str())),
            _ => None,
        })
        .collect()
}

fn diagnostic_messages(records: &[GraphRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                kind: NodeKind::Diagnostic,
                summary,
                ..
            } => Some(summary.clone()),
            _ => None,
        })
        .collect()
}

// ── AC1: declared dependencies become additive Task→Task edges ────────────────

#[test]
fn depends_on_emits_one_edge_per_resolved_dependency() {
    let records = import_lines(&[
        header(),
        task("t1", "open", &[]),
        task("t2", "open", &["t1"]),
        task("t3", "open", &["t1", "t2"]),
    ]);
    let t1 = task_id(&records, "t1");
    let t2 = task_id(&records, "t2");
    let t3 = task_id(&records, "t3");

    let mut edges = depends_on_edges(&records);
    edges.sort_unstable();
    let mut expected = vec![
        (t2.as_str(), t1.as_str()),
        (t3.as_str(), t1.as_str()),
        (t3.as_str(), t2.as_str()),
    ];
    expected.sort_unstable();
    assert_eq!(
        edges, expected,
        "one DEPENDS_ON edge per resolved dependency"
    );
}

#[test]
fn task_without_depends_on_emits_no_edges() {
    let records = import_lines(&[header(), task("t1", "open", &[]), task("t2", "open", &[])]);
    assert!(
        depends_on_edges(&records).is_empty(),
        "no depends_on means no DEPENDS_ON edges"
    );
}

// ── AC6: unknown targets are diagnostics, never silent satisfaction ───────────

#[test]
fn unknown_dependency_target_emits_diagnostic_and_no_edge() {
    let records = import_lines(&[header(), task("t1", "open", &["ghost"])]);
    assert!(
        depends_on_edges(&records).is_empty(),
        "unknown target must not mint an edge"
    );
    let diags = diagnostic_messages(&records);
    let payload: serde_json::Value = diags
        .iter()
        .find(|m| m.contains("[unresolved_dependency]"))
        .and_then(|m| {
            serde_json::from_str(m.strip_prefix("[unresolved_dependency] ").unwrap_or(m)).ok()
        })
        .expect("an [unresolved_dependency] diagnostic with a JSON payload");
    assert_eq!(payload["task_local_id"], "t1");
    assert_eq!(payload["unknown_dep"], "ghost");
    assert!(
        payload["file"]
            .as_str()
            .is_some_and(|f| f.ends_with("proj.jsonl"))
    );
    assert!(payload["line"].as_u64().is_some(), "line is recorded");
}

#[test]
fn forward_reference_dependency_resolves() {
    // t1 is declared before t2 but depends on it: resolution is file-wide,
    // not order-dependent.
    let records = import_lines(&[
        header(),
        task("t1", "open", &["t2"]),
        task("t2", "open", &[]),
    ]);
    let t1 = task_id(&records, "t1");
    let t2 = task_id(&records, "t2");
    assert_eq!(depends_on_edges(&records), vec![(t1.as_str(), t2.as_str())]);
}

#[test]
fn revision_dropping_dependency_emits_no_edge() {
    // A later revision line that omits depends_on retracts the edge: only the
    // latest revision's declared dependencies are emitted.
    let rev2 = serde_json::json!({
        "kind": "task",
        "local_id": "t1",
        "title": "Task t1 rev2",
        "status": "open",
        "priority": "normal",
        "assignees": [],
        "labels": [],
        "created_at": "2026-09-27T00:00:00Z",
        "updated_at": "2026-09-27T01:00:00Z",
    });
    assert!(rev2.get("depends_on").is_none());
    let records = import_lines(&[
        header(),
        task("t1", "open", &["t2"]),
        task("t2", "open", &[]),
        rev2.to_string(),
    ]);
    assert!(
        depends_on_edges(&records).is_empty(),
        "dropped dependency must not leave a stale edge"
    );
}

#[test]
fn duplicate_dependencies_emit_single_edge() {
    let records = import_lines(&[
        header(),
        task("t1", "open", &[]),
        task("t2", "open", &["t1", "t1"]),
    ]);
    let t1 = task_id(&records, "t1");
    let t2 = task_id(&records, "t2");
    assert_eq!(
        depends_on_edges(&records),
        vec![(t2.as_str(), t1.as_str())],
        "duplicate entries collapse to one edge"
    );
}

#[test]
fn self_dependency_imports_edge_for_query_time_cycle_detection() {
    // A self-dependency is author data: the importer carries it through as an
    // edge and the query lane reports the cycle (issue #161 AC5).
    let records = import_lines(&[header(), task("t1", "open", &["t1"])]);
    let t1 = task_id(&records, "t1");
    assert_eq!(depends_on_edges(&records), vec![(t1.as_str(), t1.as_str())]);
}

// ── malformed depends_on entries are per-entry diagnostics, never row rejection ─

/// Build a task line with a raw JSON `depends_on` value (for non-string
/// entries the string-typed `task()` helper cannot express).
fn task_raw(local_id: &str, status: &str, deps: &serde_json::Value) -> String {
    serde_json::json!({
        "kind": "task",
        "local_id": local_id,
        "title": format!("Task {local_id}"),
        "status": status,
        "priority": "normal",
        "assignees": [],
        "labels": [],
        "depends_on": deps,
        "created_at": "2026-09-27T00:00:00Z",
        "updated_at": "2026-09-27T00:00:00Z",
    })
    .to_string()
}

#[test]
fn empty_depends_on_entry_emits_invalid_dependency_and_keeps_other_edges() {
    // An empty entry is ignored with an `[invalid_dependency]` diagnostic; the
    // valid sibling entry still resolves to an edge.
    let records = import_lines(&[
        header(),
        task("t1", "open", &["", "t2"]),
        task("t2", "open", &[]),
    ]);
    let t1 = task_id(&records, "t1");
    let t2 = task_id(&records, "t2");
    assert_eq!(depends_on_edges(&records), vec![(t1.as_str(), t2.as_str())]);
    let diags = diagnostic_messages(&records);
    assert!(
        diags.iter().any(|m| m.starts_with("[invalid_dependency]")),
        "expected an [invalid_dependency] diagnostic, got: {diags:?}"
    );
}

#[test]
fn non_string_depends_on_entry_emits_invalid_dependency_and_keeps_task() {
    // A non-string entry (here: a number and null) is ignored with an
    // `[invalid_dependency]` diagnostic; the task itself is still imported and
    // the valid sibling entry still resolves to an edge.
    let records = import_lines(&[
        header(),
        task_raw("t1", "open", &serde_json::json!([42, null, "t2"])),
        task("t2", "open", &[]),
    ]);
    let t1 = task_id(&records, "t1");
    let t2 = task_id(&records, "t2");
    assert_eq!(depends_on_edges(&records), vec![(t1.as_str(), t2.as_str())]);
    let diags = diagnostic_messages(&records);
    let invalid = diags
        .iter()
        .filter(|m| m.starts_with("[invalid_dependency]"))
        .count();
    assert_eq!(
        invalid,
        2,
        "expected one [invalid_dependency] diagnostic per non-string entry, got: {diags:?}"
    );
}
