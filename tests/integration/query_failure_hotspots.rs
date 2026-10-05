//! End-to-end tests for `eg query failure-hotspots` (issue #254): rank code
//! targets by repeated agent-failure density across imported agent runs.
//!
//! A store-wide aggregation lane — no caller-supplied handle: every live
//! `Failure` record resolves to its code (Symbol/File) targets through the
//! failure-link relations, targets are ranked by DISTINCT failing-run count
//! (total failure count is the documented tie-break), each row cites its
//! contributing Failure record IDs and run/session handles, and failures
//! that resolve to no code target appear in an explicit `unresolved`
//! section — never silently dropped.
#![allow(missing_docs, clippy::doc_markdown)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    ir::{
        AGENT_MEMORY_SCHEMA_VERSION, Graph, PROJECT_SCHEMA_VERSION, agent_memory_stable_id,
        project_stable_id, stable_id,
    },
};
use assert_cmd::Command;

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

fn link(target: &str, domain: &str, relation: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    graph: PathBuf,
    alpha_id: String,
    beta_id: String,
    other_file_id: String,
    a1_id: String,
    a2_id: String,
    b1_id: String,
    b2_id: String,
    c1_id: String,
    taskonly_id: String,
    orphan_id: String,
}

/// Seeds a graph JSONL fixture with failures from ≥2 agent runs linked to ≥3
/// code targets:
///
/// * `alpha` (Symbol): `fail-a1` (run-1, 2026-02-01) + `fail-a2` (run-2, 2026-03-01)
///   → 2 distinct runs, 2 failures — the densest hotspot.
/// * `beta` (Symbol): `fail-b1` (run-1, 2026-02-15) + `fail-b2` (run-1, 2026-02-16)
///   → 1 distinct run, 2 failures — beats `other.rs` on the total tie-break.
/// * `src/other.rs` (File): `fail-c1` (run-2, 2026-04-01) → 1 run, 1 failure.
/// * `fail-task`: links only to a Task → `unresolved` (`task_only`).
/// * `fail-orphan`: links to nothing → `unresolved` (`no_code_target`).
/// * `fail-dead`: tombstoned — deleted, must not appear anywhere.
#[allow(clippy::too_many_lines)]
fn seed() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("failure_hotspots_seeded.jsonl");
    let mut graph = Graph::new();

    // ── Repository + code targets ────────────────────────────────────────────
    let repo_id = stable_id(&["node", "Repository", "repo-a"]);
    let repo = GraphRecord::node(
        repo_id.clone(),
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repository repo-a".to_owned(),
    );

    let lib_path = "src/lib.rs".to_owned();
    let lib_file_id = stable_id(&["node", "File", &lib_path]);
    let lib_file = GraphRecord::syntax_node(
        lib_file_id.clone(),
        NodeKind::File,
        lib_path.clone(),
        span(1, 100),
        "lib.rs".to_owned(),
        "rust",
        "Source file lib.rs".to_owned(),
    );

    let alpha_id = stable_id(&["node", "Symbol", &lib_path, "alpha"]);
    let alpha = GraphRecord::syntax_node(
        alpha_id.clone(),
        NodeKind::Symbol,
        lib_path.clone(),
        span(10, 20),
        "alpha".to_owned(),
        "rust",
        "Symbol alpha".to_owned(),
    );

    let beta_id = stable_id(&["node", "Symbol", &lib_path, "beta"]);
    let beta = GraphRecord::syntax_node(
        beta_id.clone(),
        NodeKind::Symbol,
        lib_path,
        span(30, 40),
        "beta".to_owned(),
        "rust",
        "Symbol beta".to_owned(),
    );

    let other_path = "src/other.rs".to_owned();
    let other_file_id = stable_id(&["node", "File", &other_path]);
    let other_file = GraphRecord::syntax_node(
        other_file_id.clone(),
        NodeKind::File,
        other_path,
        span(1, 50),
        "other.rs".to_owned(),
        "rust",
        "Source file other.rs".to_owned(),
    );

    let contains_lib = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id.clone(),
        lib_file_id.clone(),
        None,
        "repo contains lib".to_owned(),
    );
    let contains_other = GraphRecord::edge(
        EdgeLabel::Contains,
        repo_id,
        other_file_id.clone(),
        None,
        "repo contains other".to_owned(),
    );
    let defines_alpha = GraphRecord::edge(
        EdgeLabel::Defines,
        lib_file_id.clone(),
        alpha_id.clone(),
        None,
        "file defines alpha".to_owned(),
    );
    let defines_beta = GraphRecord::edge(
        EdgeLabel::Defines,
        lib_file_id,
        beta_id.clone(),
        None,
        "file defines beta".to_owned(),
    );

    // ── Task (task-only failures land in `unresolved`, not the ranking) ─────
    let task_id = project_stable_id(&["task", "task_254"]);
    let mut task = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        None,
        None,
        Some("Quagmire task".to_owned()),
        "Task #254".to_owned(),
    );
    if let GraphRecord::Node { schema_version, .. } = &mut task {
        *schema_version = PROJECT_SCHEMA_VERSION;
    }

    // ── Failures ─────────────────────────────────────────────────────────────
    let failure =
        |id: &str, run: Option<&str>, observed_at: Option<&str>, links: Vec<EvidenceLink>| {
            let mut node = GraphRecord::node(
                id.to_owned(),
                NodeKind::Failure,
                None,
                None,
                None,
                "agent failure".to_owned(),
            );
            if let GraphRecord::Node {
                failure_kind,
                session_id: sid,
                observed_at: obs,
                schema_version,
                evidence_links,
                ..
            } = &mut node
            {
                *failure_kind = Some("command_failure".to_owned());
                *sid = run.map(str::to_owned);
                *obs = observed_at.map(str::to_owned);
                *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
                *evidence_links = Some(links);
            }
            node
        };

    let a1_id = agent_memory_stable_id(&["failure", "a1"]);
    let a2_id = agent_memory_stable_id(&["failure", "a2"]);
    let b1_id = agent_memory_stable_id(&["failure", "b1"]);
    let b2_id = agent_memory_stable_id(&["failure", "b2"]);
    let c1_id = agent_memory_stable_id(&["failure", "c1"]);
    let taskonly_id = agent_memory_stable_id(&["failure", "task-only"]);
    let orphan_id = agent_memory_stable_id(&["failure", "orphan"]);
    let dead_id = agent_memory_stable_id(&["failure", "dead"]);

    // `dead_id` is borrowed again by the inlined `failure(...)` call below, so
    // this clone is load-bearing rather than redundant.
    let dead_tombstone = GraphRecord::Tombstone {
        id: agent_memory_stable_id(&["tombstone", "dead"]),
        schema_version: AGENT_MEMORY_SCHEMA_VERSION,
        deleted_id: dead_id.clone(),
        summary: "retracted".to_owned(),
        producer: None,
    };

    // The failure records are built inline: one `failure(...)` call each, so
    // there is no run of near-identical `fail_*` bindings for `similar_names`
    // to flag.
    for record in vec![
        repo,
        lib_file,
        alpha,
        beta,
        other_file,
        contains_lib,
        contains_other,
        defines_alpha,
        defines_beta,
        task,
        failure(
            &a1_id,
            Some("run-1"),
            Some("2026-02-01T00:00:00Z"),
            vec![link(&alpha_id, "codegraph", "FAILED_ON")],
        ),
        failure(
            &a2_id,
            Some("run-2"),
            Some("2026-03-01T00:00:00Z"),
            vec![link(&alpha_id, "codegraph", "FAILED_ON")],
        ),
        failure(
            &b1_id,
            Some("run-1"),
            Some("2026-02-15T00:00:00Z"),
            vec![link(&beta_id, "codegraph", "FAILED_ON")],
        ),
        failure(
            &b2_id,
            Some("run-1"),
            Some("2026-02-16T00:00:00Z"),
            vec![link(&beta_id, "codegraph", "FAILED_ON")],
        ),
        failure(
            &c1_id,
            Some("run-2"),
            Some("2026-04-01T00:00:00Z"),
            vec![link(&other_file_id, "codegraph", "TOUCHED_FILE")],
        ),
        failure(
            &taskonly_id,
            Some("run-1"),
            Some("2026-05-01T00:00:00Z"),
            vec![link(&task_id, "project", "REFERENCES_TASK")],
        ),
        failure(
            &orphan_id,
            Some("run-3"),
            Some("2026-05-02T00:00:00Z"),
            vec![],
        ),
        failure(
            &dead_id,
            Some("run-1"),
            Some("2026-01-01T00:00:00Z"),
            vec![link(&alpha_id, "codegraph", "FAILED_ON")],
        ),
        dead_tombstone,
    ] {
        graph.push(record);
    }

    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");

    Fixture {
        _temp: temp,
        graph: path,
        alpha_id,
        beta_id,
        other_file_id,
        a1_id,
        a2_id,
        b1_id,
        b2_id,
        c1_id,
        taskonly_id,
        orphan_id,
    }
}

/// A graph with no `Failure` records at all: the lane must report the
/// standard no-match envelope (exit 2), not an error.
fn seed_no_failures() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("failure_hotspots_empty.jsonl");
    let mut graph = Graph::new();
    let repo_id = stable_id(&["node", "Repository", "repo-a"]);
    graph.push(GraphRecord::node(
        repo_id,
        NodeKind::Repository,
        None,
        None,
        Some("repo-a".to_owned()),
        "Repository repo-a".to_owned(),
    ));
    let jsonl = graph.to_jsonl().expect("serialize");
    fs::write(&path, jsonl).expect("write");
    (temp, path)
}

fn run_ok(graph: &Path, extra: &[&str]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut args = vec!["query", "failure-hotspots", "--graph"];
    args.push(graph.to_str().unwrap());
    args.extend_from_slice(extra);
    let stdout = egregore()
        .args(&args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    parse_ndjson(&stdout)
}

fn run(graph: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut args = vec!["query", "failure-hotspots", "--graph"];
    args.push(graph.to_str().unwrap());
    args.extend_from_slice(extra);
    egregore().args(&args).assert()
}

fn parse_ndjson(stdout: &[u8]) -> (serde_json::Value, Vec<serde_json::Value>) {
    let out = String::from_utf8(stdout.to_vec()).expect("utf8");
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let header: serde_json::Value =
        serde_json::from_str(lines.next().expect("header line")).expect("header is JSON");
    let rows: Vec<serde_json::Value> = lines
        .map(|l| serde_json::from_str(l).expect("row is JSON"))
        .collect();
    (header, rows)
}

fn row_for<'a>(rows: &'a [serde_json::Value], target: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["target_record_id"].as_str() == Some(target))
        .unwrap_or_else(|| panic!("no row for target {target}: {rows:?}"))
}

fn failure_ids(row: &serde_json::Value) -> Vec<&str> {
    row["failures"]
        .as_array()
        .expect("failures is an array")
        .iter()
        .map(|f| f["record_id"].as_str().expect("record_id is a string"))
        .collect()
}

// ---------------------------------------------------------------------------
// Ranking: distinct-run count primary, total failure count tie-break.
// ---------------------------------------------------------------------------

#[test]
fn ranks_targets_by_distinct_run_count_with_total_as_tiebreak() {
    let fx = seed();
    let (header, rows) = run_ok(&fx.graph, &[]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["total_hotspots"], 3);
    assert_eq!(header["returned_hotspots"], 3);
    assert_eq!(header["truncated"], false);
    assert_eq!(rows.len(), 3);

    // alpha: 2 distinct runs, 2 failures — rank 1.
    let alpha = row_for(&rows, &fx.alpha_id);
    assert_eq!(alpha["rank"], 1);
    assert_eq!(alpha["distinct_run_count"], 2);
    assert_eq!(alpha["failure_count"], 2);
    assert_eq!(alpha["target_kind"], "symbol");
    assert_eq!(alpha["repo_relative_path"], "src/lib.rs");
    assert!(alpha["span"]["start_line"].as_u64().is_some());
    let alpha_fails = failure_ids(alpha);
    assert_eq!(alpha_fails.len(), 2);
    assert!(alpha_fails.contains(&fx.a1_id.as_str()));
    assert!(alpha_fails.contains(&fx.a2_id.as_str()));
    // Each contributing failure carries its run/session handle.
    for f in alpha["failures"].as_array().expect("array") {
        assert!(f["run"].as_str().is_some(), "run handle present: {f}");
        assert!(
            f["session_id"].as_str().is_some(),
            "session_id present: {f}"
        );
    }

    // beta: 1 distinct run (run-1 failed twice), 2 failures — rank 2 on the
    // total tie-break, ahead of other.rs.
    let beta = row_for(&rows, &fx.beta_id);
    assert_eq!(beta["rank"], 2);
    assert_eq!(beta["distinct_run_count"], 1);
    assert_eq!(beta["failure_count"], 2);
    let beta_fails = failure_ids(beta);
    assert_eq!(beta_fails.len(), 2);
    assert!(beta_fails.contains(&fx.b1_id.as_str()));
    assert!(beta_fails.contains(&fx.b2_id.as_str()));

    // other.rs: 1 run, 1 failure — rank 3.
    let other = row_for(&rows, &fx.other_file_id);
    assert_eq!(other["rank"], 3);
    assert_eq!(other["target_kind"], "file");
    assert_eq!(other["repo_relative_path"], "src/other.rs");
    assert_eq!(other["distinct_run_count"], 1);
    assert_eq!(other["failure_count"], 1);
    assert_eq!(failure_ids(other), vec![fx.c1_id.as_str()]);

    // Rows are emitted in rank order.
    let ranks: Vec<u64> = rows
        .iter()
        .map(|r| r["rank"].as_u64().expect("rank is a number"))
        .collect();
    assert_eq!(ranks, vec![1, 2, 3]);

    // The tombstoned failure is deleted: it must not inflate alpha's counts
    // and must not surface in `unresolved`.
    assert_eq!(alpha["failure_count"], 2, "tombstoned failure excluded");
}

#[test]
fn unresolved_section_lists_failures_without_code_target() {
    let fx = seed();
    let (header, _rows) = run_ok(&fx.graph, &[]);

    let unresolved = header["unresolved"]
        .as_array()
        .expect("unresolved is an array");
    assert_eq!(unresolved.len(), 2, "task-only + orphan: {unresolved:?}");

    let by_id = |id: &str| {
        unresolved
            .iter()
            .find(|u| u["failure_record_id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("no unresolved entry for {id}: {unresolved:?}"))
    };
    assert_eq!(by_id(&fx.taskonly_id)["reason"], "task_only");
    assert_eq!(by_id(&fx.orphan_id)["reason"], "no_code_target");
    assert_eq!(by_id(&fx.taskonly_id)["session_id"], "run-1");
}

// ---------------------------------------------------------------------------
// Success metric: byte-identical ranking across 5 repeated runs.
// ---------------------------------------------------------------------------

#[test]
fn ranking_is_byte_identical_across_five_runs() {
    let fx = seed();
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let mut args = vec!["query", "failure-hotspots", "--graph"];
        args.push(fx.graph.to_str().unwrap());
        let stdout = egregore()
            .args(&args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        outputs.push(stdout);
    }
    for (i, out) in outputs.iter().enumerate().skip(1) {
        assert_eq!(
            out, &outputs[0],
            "run {i} differs from run 0 — ranking is not deterministic"
        );
    }
}

// ---------------------------------------------------------------------------
// --limit and the truncation signal.
// ---------------------------------------------------------------------------

#[test]
fn limit_caps_rows_and_signals_truncation() {
    let fx = seed();
    let (header, rows) = run_ok(&fx.graph, &["--limit", "1"]);

    assert_eq!(header["ok"], true);
    assert_eq!(header["total_hotspots"], 3);
    assert_eq!(header["returned_hotspots"], 1);
    assert_eq!(header["truncated"], true);
    assert_eq!(rows.len(), 1);
    // The single returned row is still the top hotspot (rank 1, alpha).
    assert_eq!(rows[0]["rank"], 1);
    assert_eq!(rows[0]["target_record_id"], fx.alpha_id.as_str());
}

#[test]
fn invalid_limit_exits_1() {
    let fx = seed();
    for bad in ["0", "1000000"] {
        let assert = run(&fx.graph, &["--limit", bad]).failure().code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
        assert!(
            stderr.contains("invalid_limit"),
            "bad limit {bad} reports invalid_limit on stderr: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Temporal selectors.
// ---------------------------------------------------------------------------

#[test]
fn since_scopes_the_aggregation_to_later_failures() {
    let fx = seed();
    let (header, rows) = run_ok(&fx.graph, &["--since", "2026-03-01T00:00:00Z"]);

    assert_eq!(header["ok"], true);
    // Only fail-a2 (alpha, run-2) and fail-c1 (other.rs, run-2) are in window.
    // beta's two failures predate the window entirely.
    assert_eq!(header["total_hotspots"], 2);
    assert_eq!(rows.len(), 2);
    let alpha = row_for(&rows, &fx.alpha_id);
    assert_eq!(alpha["distinct_run_count"], 1);
    assert_eq!(failure_ids(alpha), vec![fx.a2_id.as_str()]);
    let other = row_for(&rows, &fx.other_file_id);
    assert_eq!(other["distinct_run_count"], 1);
    assert_eq!(failure_ids(other), vec![fx.c1_id.as_str()]);
    // Failures outside the window contribute nothing — and nothing is dropped
    // silently: unresolved still lists the out-of-window-blind failures? No:
    // failures excluded by the temporal window are EXCLUDED from the answer,
    // not unresolved. The window question is "where were agents stuck then".
    assert!(
        rows.iter()
            .all(|r| r["target_record_id"].as_str() != Some(fx.beta_id.as_str())),
        "beta has no in-window failures: {rows:?}"
    );
}

#[test]
fn as_of_scopes_the_aggregation_to_earlier_failures() {
    let fx = seed();
    let (header, rows) = run_ok(&fx.graph, &["--as-of", "2026-02-15T00:00:00Z"]);

    assert_eq!(header["ok"], true);
    // fail-a1 (02-01) and fail-b1 (02-15, boundary-inclusive) are in window.
    assert_eq!(header["total_hotspots"], 2);
    let alpha = row_for(&rows, &fx.alpha_id);
    assert_eq!(alpha["distinct_run_count"], 1);
    assert_eq!(failure_ids(alpha), vec![fx.a1_id.as_str()]);
    let beta = row_for(&rows, &fx.beta_id);
    assert_eq!(beta["distinct_run_count"], 1);
    assert_eq!(failure_ids(beta), vec![fx.b1_id.as_str()]);
}

#[test]
fn malformed_temporal_selector_exits_1() {
    let fx = seed();
    let assert = run(&fx.graph, &["--as-of", "not-a-timestamp"])
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8");
    assert!(
        stderr.contains("malformed_timestamp"),
        "bad --as-of reports malformed_timestamp on stderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Empty store: no_match envelope, exit 2.
// ---------------------------------------------------------------------------

#[test]
fn empty_store_returns_no_match_exit_2() {
    let (_temp, graph) = seed_no_failures();
    let assert = run(&graph, &[]).failure().code(2);
    let (header, rows) = parse_ndjson(&assert.get_output().stdout.clone());
    assert_eq!(header["ok"], false);
    assert_eq!(header["error"]["code"], "no_match");
    assert!(rows.is_empty());
}

// ---------------------------------------------------------------------------
// Output format.
// ---------------------------------------------------------------------------

#[test]
fn text_format_is_human_readable() {
    let fx = seed();
    let stdout = run(&fx.graph, &["--format", "text"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(stdout).expect("utf8");
    assert!(
        text.contains("alpha") || text.contains("src/lib.rs"),
        "text output names the top hotspot: {text}"
    );
    assert!(
        text.contains("unresolved"),
        "text output names the unresolved section: {text}"
    );
}
