//! CLI end-to-end tests for `eg query task-list` (issue #119).
//!
//! Written RED-first per SPEC-PROOF-RED-GREEN-REFACTOR. These drive the real
//! `eg` binary over a hand-built project-graph JSONL fixture mirroring the
//! AC fixture: one `Task` per status value (`open`, `in_progress`, `blocked`,
//! `closed_completed`, `closed_dropped`, `unknown`) from each `source_kind`
//! (`github_issue` via `ExternalLink`, `local_jsonl` via its own JSONL handle),
//! plus a bi-temporal status-transition pair (same `entity_id`, two
//! `transaction_time` rows) proving the older row is excluded, plus canary
//! nodes proving raw bodies / transcripts never leak.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::json;

fn eg() -> Command {
    Command::cargo_bin("eg").expect("eg binary builds")
}

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

fn fixture_lines() -> Vec<String> {
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
    // Bi-temporal transition: same entity, older row (open) must be excluded
    // in favor of the later transaction_time row (in_progress).
    lines.push(task(
        "task:trans-v1",
        "open",
        json!({
            "entity_id": "entity:transition",
            "source_kind": "local_jsonl",
            "source_handle": "tasks%2Facme.jsonl:transition:aaaa",
            "transaction_time": "2026-09-20T00:00:00Z",
        }),
    ));
    lines.push(task(
        "task:trans-v2",
        "in_progress",
        json!({
            "entity_id": "entity:transition",
            "source_kind": "local_jsonl",
            "source_handle": "tasks%2Facme.jsonl:transition:bbbb",
            "transaction_time": "2026-09-29T00:00:00Z",
        }),
    ));
    // Canary nodes: raw body text behind a redacted handle and transcript
    // text on an unrelated node must never appear in lane output.
    lines.push(task(
        "task:canary",
        "open",
        json!({
            "entity_id": "entity:canary",
            "source_kind": "github_issue",
            "source_external_link_id": "link:gh-canary",
            "body_handle": {"inline": "CANARY-119-body-must-never-surface", "hash": "deadbeef", "bytes": 34},
        }),
    ));
    lines.push(node(
        "link:gh-canary",
        "ExternalLink",
        &json!({
            "system": "github",
            "system_native_id": "#4242",
            "url": "https://github.com/acme/repo/issues/4242",
        }),
    ));
    lines.push(node(
        "sess:canary",
        "AgentRun",
        &json!({"text": "CANARY-119-transcript-must-never-surface"}),
    ));
    lines
}

/// A store whose tasks are all `open`: the `closed_completed` filter matches
/// nothing, exercising the explicit zero-matches envelope.
fn open_only_lines() -> Vec<String> {
    vec![task(
        "task:only-open",
        "open",
        json!({
            "entity_id": "entity:only-open",
            "source_kind": "local_jsonl",
            "source_handle": "tasks%2Facme.jsonl:only-open:cccc",
        }),
    )]
}

struct Workspace {
    _temp: tempfile::TempDir,
    graph: PathBuf,
}

fn setup(lines: &[String]) -> Workspace {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, lines.join("\n") + "\n").expect("write fixture");
    Workspace { _temp: temp, graph }
}

fn task_list(ws: &Workspace, extra: &[&str]) -> std::process::Output {
    let mut cmd = eg();
    cmd.arg("query")
        .arg("task-list")
        .arg("--graph")
        .arg(&ws.graph);
    for e in extra {
        cmd.arg(e);
    }
    cmd.output().expect("run eg query task-list")
}

fn task_list_json(ws: &Workspace, extra: &[&str]) -> serde_json::Value {
    let out = task_list(ws, extra);
    assert_eq!(
        out.status.code(),
        Some(0),
        "exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

#[test]
fn task_list_reports_every_status_from_both_source_kinds() {
    let ws = setup(&fixture_lines());
    let v = task_list_json(
        &ws,
        &[
            "--status",
            "open,in_progress,blocked,closed_completed,closed_dropped,unknown",
        ],
    );

    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["lane"], json!("task-list"));
    assert_eq!(v["trust"], json!("status_lead"));
    assert_eq!(v["zero_matches"], json!(false));
    assert!(
        v["disclaimer"]
            .as_str()
            .unwrap()
            .contains("imported intent"),
        "disclaimer labels statuses as imported intent, not proof"
    );

    let groups = v["groups"].as_array().expect("groups array");
    let statuses: Vec<&str> = groups
        .iter()
        .map(|g| g["status"].as_str().expect("status string"))
        .collect();
    assert_eq!(
        statuses,
        vec![
            "open",
            "in_progress",
            "blocked",
            "closed_completed",
            "closed_dropped",
            "unknown"
        ],
        "groups in closed-vocabulary canonical order"
    );

    for group in groups {
        for row in group["tasks"].as_array().expect("tasks array") {
            for key in [
                "record_id",
                "schema_version",
                "status",
                "source_kind",
                "source_handle",
                "title",
                "summary",
            ] {
                assert!(row.get(key).is_some(), "row carries {key}: {row}");
            }
            assert_eq!(row["status"], group["status"]);
        }
    }

    // GitHub row resolves number + URL via ExternalLink.
    let gh: Vec<&serde_json::Value> = groups
        .iter()
        .flat_map(|g| g["tasks"].as_array().unwrap().iter())
        .filter(|r| r["record_id"] == json!("task:gh-open"))
        .collect();
    assert_eq!(gh.len(), 1);
    assert_eq!(
        gh[0]["source_handle"],
        json!("#100 <https://github.com/acme/repo/issues/100>")
    );
    assert_eq!(gh[0]["source_kind"], json!("github_issue"));

    // Local row keeps its own JSONL handle.
    let local: Vec<&serde_json::Value> = groups
        .iter()
        .flat_map(|g| g["tasks"].as_array().unwrap().iter())
        .filter(|r| r["record_id"] == json!("task:local-blocked"))
        .collect();
    assert_eq!(local.len(), 1);
    assert_eq!(
        local[0]["source_handle"],
        json!("tasks%2Facme.jsonl:local-blocked:deadbeef")
    );
    assert_eq!(local[0]["source_kind"], json!("local_jsonl"));
}

#[test]
fn task_list_default_filter_is_active() {
    let ws = setup(&fixture_lines());
    let defaulted = task_list_json(&ws, &[]);
    let explicit = task_list_json(&ws, &["--status", "active"]);
    assert_eq!(defaulted, explicit, "omitted --status == --status active");
    let statuses: Vec<&str> = defaulted["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, vec!["open", "in_progress", "blocked"]);
    let filter: BTreeSet<&str> = defaulted["status_filter"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(filter, BTreeSet::from(["open", "in_progress", "blocked"]));
}

#[test]
fn task_list_single_status_filter() {
    let ws = setup(&fixture_lines());
    let v = task_list_json(&ws, &["--status", "blocked"]);
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["status"], json!("blocked"));
    assert_eq!(groups[0]["tasks"].as_array().unwrap().len(), 2);
}

#[test]
fn task_list_older_transaction_time_row_is_excluded() {
    let ws = setup(&fixture_lines());
    let v = task_list_json(&ws, &["--status", "in_progress"]);
    let rows = v["groups"].as_array().unwrap()[0]["tasks"]
        .as_array()
        .unwrap();
    let ids: Vec<&str> = rows
        .iter()
        .map(|r| r["record_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"task:trans-v2"),
        "latest transaction_time row surfaces: {ids:?}"
    );
    assert!(
        !ids.contains(&"task:trans-v1"),
        "older transaction_time row excluded: {ids:?}"
    );
    let winner = rows
        .iter()
        .find(|r| r["record_id"] == json!("task:trans-v2"))
        .unwrap();
    assert_eq!(winner["status"], json!("in_progress"));
}

#[test]
fn task_list_malformed_status_fails_with_stable_diagnostic() {
    let ws = setup(&fixture_lines());
    for bad in ["bogus", "open,", "open,open", "active,open"] {
        let out = task_list(&ws, &["--status", bad]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "exit 3 for {bad:?}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("machine-readable JSON");
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["error"]["code"], json!("invalid_status_filter"));
        // The diagnostic names the offending value: the unknown name, the
        // duplicated name, or the literal `active` combined with others.
        let offending = bad.split(',').next().unwrap_or("");
        if !offending.is_empty() && !bad.ends_with(',') {
            assert!(
                v["error"]["message"].as_str().unwrap().contains(offending),
                "message names the offending value"
            );
        }
    }
}

#[test]
fn task_list_zero_matches_is_explicit_not_an_error() {
    let ws = setup(&open_only_lines());
    let out = task_list(&ws, &["--status", "closed_completed"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "zero matches is a valid answer, not an error; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["zero_matches"], json!(true));
    assert_eq!(v["groups"], json!([]));
    assert_eq!(v["counts"]["matched"], json!(0));
    assert_eq!(v["counts"]["tasks"], json!(1), "tasks were seen");
}

#[test]
fn task_list_no_project_data_is_exit_2() {
    let ws = setup(&[node(
        "sym:a",
        "Symbol",
        &json!({"repo_relative_path": "a.rs"}),
    )]);
    let out = task_list(&ws, &["--status", "open"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "no Task records -> no_project_data; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["error"]["code"], json!("no_project_data"));
}

#[test]
fn task_list_never_emits_raw_bodies_or_transcripts() {
    let ws = setup(&fixture_lines());
    let v = task_list_json(&ws, &["--status", "open"]);
    let wire = serde_json::to_string(&v).expect("serialize");
    assert!(
        !wire.contains("CANARY-119-body-must-never-surface"),
        "raw issue body behind body_handle never surfaces"
    );
    assert!(
        !wire.contains("CANARY-119-transcript-must-never-surface"),
        "unrelated transcript text never surfaces"
    );
    assert!(
        !wire.contains("body_handle"),
        "the body handle field itself is never emitted"
    );
}

#[test]
fn task_list_is_deterministic_across_five_runs() {
    let ws = setup(&fixture_lines());
    let first = task_list(&ws, &["--status", "active"]);
    assert_eq!(first.status.code(), Some(0));
    for i in 2..=5 {
        let next = task_list(&ws, &["--status", "active"]);
        assert_eq!(next.stdout, first.stdout, "run {i} byte-identical to run 1");
        assert_eq!(next.status.code(), Some(0));
    }
}

#[test]
fn task_list_text_format_renders_groups() {
    let ws = setup(&fixture_lines());
    let out = task_list(&ws, &["--status", "open", "--format", "text"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert!(text.contains("task-list:"), "text header present");
    assert!(text.contains("[open]"), "status group header present");
    assert!(text.contains("task:gh-open"), "task row present");
}
