//! CLI end-to-end tests for `eg query task-overlap` (issue #150).
//!
//! Written RED-first per SPEC-PROOF-RED-GREEN-REFACTOR. These drive the real
//! `eg` binary over a hand-built project-graph JSONL fixture mirroring the
//! AC fixture: T1/T2 overlap on a symbol (direct `MENTIONS_SYMBOL`) and a
//! file (via the `REFERENCES_TASK` → session → `TOUCHED_FILE` path), T3 is
//! disjoint, T4/T5 are terminal, T6 has no resolvable footprint, and T7/T8
//! prove only the latest-`transaction_time` status counts.

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
    extra["title"] = json!(format!("Task {id}"));
    extra["status"] = json!(status);
    extra["transaction_time"] = json!("2026-09-28T00:00:00Z");
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

fn fixture_lines() -> Vec<String> {
    vec![
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
        task("task:t7", "open", json!({})),
        task("task:t7", "closed_completed", json!({})),
        task("task:t8", "closed_dropped", json!({})),
        task("task:t8", "open", json!({})),
        node(
            "link:gh-42",
            "ExternalLink",
            &json!({
                "system": "github",
                "system_native_id": "#42",
                "url": "https://github.com/acme/repo/issues/42",
            }),
        ),
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
        node(
            "mod:m1",
            "Module",
            &json!({"repo_relative_path": "src/wire.rs"}),
        ),
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
        edge("e1", "MENTIONS_SYMBOL", "task:t1", "sym:s1"),
        edge("e2", "MENTIONS_SYMBOL", "task:t2", "sym:s1"),
        edge("e3", "REFERENCES_TASK", "sess:a", "task:t1"),
        edge("e4", "TOUCHED_FILE", "sess:a", "file:f1"),
        edge("e5", "REFERENCES_TASK", "sess:b", "task:t2"),
        edge("e6", "TOUCHED_FILE", "sess:b", "file:f1"),
        edge("e7", "MENTIONS_SYMBOL", "task:t3", "sym:s2"),
        edge("e8", "MENTIONS_SYMBOL", "task:t4", "sym:s1"),
        edge("e9", "MENTIONS_SYMBOL", "task:t5", "sym:s1"),
        edge("e10", "MENTIONS_SYMBOL", "task:t7", "sym:s1"),
        edge("e11", "MENTIONS_SYMBOL", "task:t8", "sym:s1"),
        edge("e12", "MENTIONS_SYMBOL", "task:t6", "mod:m1"),
    ]
}

struct Workspace {
    _temp: tempfile::TempDir,
    root: PathBuf,
    graph: PathBuf,
}

fn setup(lines: &[String]) -> Workspace {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().to_path_buf();
    let graph = root.join("graph.jsonl");
    fs::write(&graph, lines.join("\n") + "\n").expect("write fixture");
    Workspace {
        _temp: temp,
        root,
        graph,
    }
}

/// Snapshot of every file under `dir`: (relative path, bytes).
fn dir_snapshot(dir: &std::path::Path) -> BTreeSet<(String, Vec<u8>)> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("read dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(dir)
                    .expect("under dir")
                    .to_string_lossy()
                    .into_owned();
                out.insert((rel, fs::read(&path).expect("read file")));
            }
        }
    }
    out
}

fn task_overlap_json(ws: &Workspace, extra: &[&str]) -> std::process::Output {
    let mut cmd = eg();
    cmd.arg("query")
        .arg("task-overlap")
        .arg("--graph")
        .arg(&ws.graph);
    for e in extra {
        cmd.arg(e);
    }
    cmd.output().expect("run eg query task-overlap")
}

#[test]
fn task_overlap_reports_the_ac_pairs() {
    let ws = setup(&fixture_lines());
    let out = task_overlap_json(&ws, &["--format", "json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");

    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["lane"], json!("task-overlap"));
    assert_eq!(v["trust"], json!("inspection_lead"));
    assert_eq!(v["zero_overlaps"], json!(false));
    assert!(
        v["disclaimer"]
            .as_str()
            .unwrap()
            .contains("inspection lead"),
        "disclaimer labels the answer an inspection lead"
    );

    let pair_ids: Vec<(String, String)> = v["pairs"]
        .as_array()
        .expect("pairs array")
        .iter()
        .map(|p| {
            (
                p["task_a"]["record_id"].as_str().unwrap().to_owned(),
                p["task_b"]["record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        pair_ids,
        vec![
            ("task:t1".to_owned(), "task:t2".to_owned()),
            ("task:t1".to_owned(), "task:t8".to_owned()),
            ("task:t2".to_owned(), "task:t8".to_owned()),
        ]
    );

    // Shared handles for (T1,T2): file:f1 via the session leg, sym:s1 direct.
    let handles = &v["pairs"][0]["shared_handles"];
    let handle_ids: Vec<&str> = handles
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["record_id"].as_str().unwrap())
        .collect();
    assert_eq!(handle_ids, vec!["file:f1", "sym:s1"]);
    assert_eq!(handles[0]["kind"], json!("File"));
    assert_eq!(handles[0]["via"], json!(["session"]));
    assert_eq!(handles[0]["repo_relative_path"], json!("src/wire.rs"));
    assert_eq!(handles[1]["kind"], json!("Symbol"));
    assert_eq!(handles[1]["via"], json!(["direct"]));
    assert_eq!(handles[1]["span"], json!("10:4-14:1"));

    // Source handles: GitHub issue via ExternalLink, local handle otherwise.
    assert_eq!(
        v["pairs"][0]["task_a"]["source_handle"],
        json!("#42 <https://github.com/acme/repo/issues/42>")
    );
    assert_eq!(
        v["pairs"][0]["task_b"]["source_handle"],
        json!("tasks%2Facme.jsonl:t2 <file://tasks/acme.jsonl>")
    );

    // T6: documented no-resolvable-footprint diagnostic, never a pair.
    let diags = v["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], json!("no_footprint"));
    assert_eq!(diags[0]["task_record_id"], json!("task:t6"));
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("no resolvable footprint")
    );

    assert_eq!(v["counts"]["pairs"], json!(3));
    assert_eq!(v["counts"]["in_flight_tasks"], json!(5));
    assert_eq!(v["counts"]["no_footprint_tasks"], json!(1));

    // Raw agent payloads never leak into the output.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("CANARY"));
}

#[test]
fn task_overlap_five_runs_byte_identical_and_read_only() {
    let ws = setup(&fixture_lines());
    let before = dir_snapshot(&ws.root);
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let out = task_overlap_json(&ws, &["--format", "json"]);
        assert_eq!(out.status.code(), Some(0));
        outputs.push(out.stdout);
    }
    for o in &outputs[1..] {
        assert_eq!(&outputs[0], o, "5 consecutive runs are byte-identical");
    }
    let after = dir_snapshot(&ws.root);
    assert_eq!(before, after, "query created/modified/deleted no files");
}

#[test]
fn task_overlap_malformed_status_filter_exits_3_with_stable_diagnostic() {
    let ws = setup(&fixture_lines());
    for bad in [
        "bogus",
        "open,",
        "open,open",
        "closed_completed",
        "superseded",
    ] {
        let out = task_overlap_json(&ws, &["--status", bad]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "status={bad}: stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["error"]["code"], json!("invalid_status_filter"));
        assert!(
            !v["error"]["message"].as_str().unwrap().is_empty(),
            "diagnostic carries a message"
        );
    }
}

#[test]
fn task_overlap_no_project_data_exits_2() {
    let ws = setup(&[node(
        "sym:a",
        "Symbol",
        &json!({"repo_relative_path": "a.rs"}),
    )]);
    let out = task_overlap_json(&ws, &["--format", "json"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["error"]["code"], json!("no_project_data"));
}

#[test]
fn task_overlap_zero_overlaps_is_valid_and_explicit() {
    let ws = setup(&[
        task("task:a", "open", json!({})),
        task("task:b", "blocked", json!({})),
        node("sym:a", "Symbol", &json!({"repo_relative_path": "a.rs"})),
        node("sym:b", "Symbol", &json!({"repo_relative_path": "b.rs"})),
        edge("e1", "MENTIONS_SYMBOL", "task:a", "sym:a"),
        edge("e2", "MENTIONS_SYMBOL", "task:b", "sym:b"),
    ]);
    let out = task_overlap_json(&ws, &["--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["pairs"], json!([]));
    assert_eq!(v["zero_overlaps"], json!(true));
    assert_eq!(v["counts"]["pairs"], json!(0));
}

#[test]
fn task_overlap_status_filter_narrows_the_report() {
    let ws = setup(&fixture_lines());
    let out = task_overlap_json(&ws, &["--status", "open"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let pair_ids: Vec<(String, String)> = v["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["task_a"]["record_id"].as_str().unwrap().to_owned(),
                p["task_b"]["record_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(pair_ids, vec![("task:t1".to_owned(), "task:t8".to_owned())]);
}

#[test]
fn task_overlap_text_format_is_human_readable() {
    let ws = setup(&fixture_lines());
    let out = task_overlap_json(&ws, &["--format", "text"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("task-overlap"), "names the lane");
    assert!(stdout.contains("task:t1"), "names the tasks");
    assert!(
        stdout.contains("inspection lead"),
        "carries the trust disclaimer"
    );
    assert!(!stdout.contains("CANARY"), "no raw payloads in text either");
}

/// The embedded-store answer must equal the JSONL answer: footprint edges
/// survive the `AletheiaDB` adapter write/read round-trip (issue #150).
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn task_overlap_embedded_store_matches_jsonl() {
    let ws = setup(&fixture_lines());
    let data_dir = ws.root.join("store");
    eg().arg("ingest")
        .arg(&ws.graph)
        .args(["--adapter", "embedded", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();

    let from_jsonl = task_overlap_json(&ws, &["--format", "json"]);
    assert_eq!(from_jsonl.status.code(), Some(0));

    let out = eg()
        .arg("query")
        .arg("task-overlap")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("run eg query task-overlap --data-dir");
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
