//! CLI end-to-end tests for `eg query task-ready` (issue #161).
//!
//! Written RED-first per SPEC-PROOF-RED-GREEN-REFACTOR. These drive the real
//! binaries: `eg import-local-tasks` → `eg query task-ready`, asserting the
//! documented workflow, exit-code contract (0 ready / 2 no-data / 3 cycle),
//! byte-identical repeat runs, and the read-only guarantee (AC9).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

const FIXED_TX_TIME: &str = "2026-09-27T00:00:00Z";

fn eg() -> Command {
    Command::cargo_bin("eg").expect("eg binary builds")
}

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

fn ac2_lines() -> Vec<String> {
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

struct Workspace {
    _temp: tempfile::TempDir,
    root: PathBuf,
    tasks_dir: PathBuf,
    graph: PathBuf,
}

fn setup(lines: &[String]) -> Workspace {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().to_path_buf();
    let tasks_dir = root.join("tasks");
    fs::create_dir_all(&tasks_dir).expect("tasks dir");
    fs::write(tasks_dir.join("proj.jsonl"), lines.join("\n") + "\n").expect("write tasks");
    let graph = root.join("project.jsonl");
    Workspace {
        _temp: temp,
        root,
        tasks_dir,
        graph,
    }
}

fn import(ws: &Workspace) {
    eg().arg("import-local-tasks")
        .arg(&ws.tasks_dir)
        .arg("--out")
        .arg(&ws.graph)
        .arg("--repo-root")
        .arg(&ws.root)
        .arg("--transaction-time")
        .arg(FIXED_TX_TIME)
        .assert()
        .success();
}

fn task_ready_json(ws: &Workspace, extra: &[&str]) -> std::process::Output {
    let mut cmd = eg();
    cmd.arg("query")
        .arg("task-ready")
        .arg("--graph")
        .arg(&ws.graph);
    for e in extra {
        cmd.arg(e);
    }
    cmd.output().expect("run eg query task-ready")
}

/// Map `local_id` -> `record_id` by reading the imported graph JSONL.
fn record_ids_by_local(ws: &Workspace) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let jsonl = fs::read_to_string(&ws.graph).expect("read graph");
    for line in jsonl.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid jsonl");
        if v.get("kind").and_then(|k| k.as_str()) == Some("Task") {
            let id = v
                .get("id")
                .and_then(|i| i.as_str())
                .expect("task id")
                .to_owned();
            // The local importer stores the JSONL `local_id` in the node `name`;
            // `entity_id` is the stable record ID, not the local ID.
            let lid = v
                .get("name")
                .and_then(|e| e.as_str())
                .expect("task name (local_id)")
                .to_owned();
            map.insert(lid, id);
        }
    }
    map
}

fn ready_local_ids(stdout: &[u8], by_local: &BTreeMap<String, String>) -> HashSet<String> {
    let v: serde_json::Value = serde_json::from_slice(stdout).expect("stdout is JSON");
    let rev: BTreeMap<&str, &str> = by_local
        .iter()
        .map(|(k, v)| (v.as_str(), k.as_str()))
        .collect();
    v["ready"]
        .as_array()
        .expect("ready array")
        .iter()
        .map(|r| {
            let rid = r["record_id"].as_str().expect("record_id");
            (*rev.get(rid).expect("known task")).to_owned()
        })
        .collect()
}

// ── The documented shortest workflow ─────────────────────────────────────────

#[test]
fn full_workflow_import_then_ready_set_json() {
    let ws = setup(&ac2_lines());
    import(&ws);

    let out = task_ready_json(&ws, &["--format", "json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let by_local = record_ids_by_local(&ws);
    let ready = ready_local_ids(&out.stdout, &by_local);
    assert_eq!(
        ready,
        HashSet::from(["t1".to_owned(), "t5".to_owned(), "t6".to_owned()])
    );

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["ok"], true);
    assert_eq!(v["trust"], "eligibility_lead");
    assert_eq!(v["blocked"].as_array().expect("blocked").len(), 3);
    // Every blocked row names its unmet dependency record IDs with source handles.
    for b in v["blocked"].as_array().unwrap() {
        let unmet = b["unmet_dependencies"].as_array().expect("unmet array");
        assert!(!unmet.is_empty(), "blocked row names blockers");
        for u in unmet {
            assert!(
                u["record_id"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("project:v1:"))
            );
            assert!(
                u["source_handle"]
                    .as_str()
                    .is_some_and(|s| s.contains("proj.jsonl"))
            );
        }
    }
    // Stable record IDs + source handles on every ready row too.
    for r in v["ready"].as_array().unwrap() {
        assert!(
            r["record_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("project:v1:"))
        );
        assert!(
            r["source_handle"]
                .as_str()
                .is_some_and(|s| s.contains("proj.jsonl"))
        );
    }
}

#[test]
fn text_format_lists_ready_and_blocked() {
    let ws = setup(&ac2_lines());
    import(&ws);

    eg().arg("query")
        .arg("task-ready")
        .arg("--graph")
        .arg(&ws.graph)
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .stdout(predicate::str::contains("ready"))
        .stdout(predicate::str::contains("blocked"));
}

#[test]
fn transitive_status_change_makes_exactly_t2_ready() {
    let ws = setup(&ac2_lines());
    import(&ws);
    let by_local = record_ids_by_local(&ws);
    let before = ready_local_ids(&task_ready_json(&ws, &[]).stdout, &by_local);
    assert_eq!(
        before,
        HashSet::from(["t1".to_owned(), "t5".to_owned(), "t6".to_owned()])
    );

    // Operator closes t1: append a revision line, re-import, re-query.
    let mut lines = ac2_lines();
    lines.push(
        serde_json::json!({
            "kind": "task", "local_id": "t1", "title": "Task t1",
            "status": "closed_completed", "priority": "normal",
            "assignees": [], "labels": [],
            "created_at": "2026-09-27T00:00:00Z", "updated_at": "2026-09-27T02:00:00Z",
        })
        .to_string(),
    );
    fs::write(ws.tasks_dir.join("proj.jsonl"), lines.join("\n") + "\n").expect("rewrite tasks");
    import(&ws);

    let out = task_ready_json(&ws, &[]);
    assert_eq!(out.status.code(), Some(0));
    let after = ready_local_ids(&out.stdout, &by_local);
    assert_eq!(
        after,
        HashSet::from(["t2".to_owned(), "t5".to_owned(), "t6".to_owned()]),
        "exactly t2 newly eligible; t3 still blocked on t2"
    );
}

// ── Exit-code contract ───────────────────────────────────────────────────────

#[test]
fn no_tasks_exits_2_with_no_project_data() {
    let ws = setup(&[header()]);
    import(&ws);

    let out = task_ready_json(&ws, &[]);
    assert_eq!(out.status.code(), Some(2), "no Task records -> exit 2");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "no_project_data");
}

#[test]
fn dependency_cycle_exits_3_naming_members() {
    let ws = setup(&[
        header(),
        task("ta", "open", &["tb"]),
        task("tb", "open", &["ta"]),
    ]);
    import(&ws);

    let out = task_ready_json(&ws, &[]);
    assert_eq!(out.status.code(), Some(3), "cycle -> distinct exit code 3");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    let diags = v["diagnostics"].as_array().expect("diagnostics");
    let cycle = diags
        .iter()
        .find(|d| d["code"] == "dependency_cycle")
        .expect("cycle diagnostic");
    let members: HashSet<&str> = cycle["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    let by_local = record_ids_by_local(&ws);
    assert_eq!(
        members,
        HashSet::from([by_local["ta"].as_str(), by_local["tb"].as_str()])
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cycle"),
        "stderr carries a one-line cycle summary"
    );
}

#[test]
fn missing_graph_is_exit_1_error() {
    eg().arg("query")
        .arg("task-ready")
        .arg("--graph")
        .arg("/nonexistent/graph.jsonl")
        .assert()
        .code(1);
}

// ── AC9: read-only + deterministic ────────────────────────────────────────────

fn dir_snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, String)> {
    let mut snap = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&dir).expect("read dir").collect();
        entries.sort_by_key(|e| e.as_ref().map(std::fs::DirEntry::path).unwrap_or_default());
        for e in entries {
            let e = e.expect("dir entry");
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let bytes = fs::read(&p).expect("read file");
                let hash = blake3::hash(&bytes).to_hex().to_string();
                snap.insert(
                    p.strip_prefix(root).unwrap().to_path_buf(),
                    (bytes.len() as u64, hash),
                );
            }
        }
    }
    snap
}

#[test]
fn five_runs_byte_identical_and_read_only() {
    let ws = setup(&ac2_lines());
    import(&ws);

    let before = dir_snapshot(&ws.root);
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let out = task_ready_json(&ws, &[]);
        assert_eq!(out.status.code(), Some(0));
        outputs.push(out.stdout);
    }
    for o in &outputs[1..] {
        assert_eq!(&outputs[0], o, "5 consecutive runs are byte-identical");
    }
    let after = dir_snapshot(&ws.root);
    assert_eq!(before, after, "query created/modified/deleted no files");
}

/// The embedded-store answer must equal the JSONL answer: `DEPENDS_ON` edges
/// survive the `AletheiaDB` adapter write/read round-trip (issue #161).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn task_ready_embedded_store_matches_jsonl() {
    let ws = setup(&ac2_lines());
    import(&ws);
    let data_dir = ws.root.join("store");
    eg().arg("ingest")
        .arg(&ws.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let from_jsonl = task_ready_json(&ws, &["--format", "json"]);
    assert_eq!(from_jsonl.status.code(), Some(0));

    let out = eg()
        .arg("query")
        .arg("task-ready")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("run eg query task-ready --data-dir");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let jl: serde_json::Value =
        serde_json::from_slice(&from_jsonl.stdout).expect("JSONL answer is JSON");
    let db: serde_json::Value = serde_json::from_slice(&out.stdout).expect("store answer is JSON");
    assert_eq!(db, jl, "embedded store answer must match the JSONL answer");
}
